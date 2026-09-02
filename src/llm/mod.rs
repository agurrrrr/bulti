//! OpenAI 호환 클라이언트 (SSE 스트리밍, 툴콜 누적) (DESIGN.md §4.2).
//!
//! 단계 2: `POST /chat/completions` (`stream: true`) 요청 조립,
//! SSE `data:` 라인 파싱, index 기반 툴콜 누적, reasoning_content 분리,
//! finish_reason 처리, usage 수집, 오류 매핑을 구현한다.
//!
//! 이 모듈의 공개 API 는 agent 루프(단계 4)에서 사용 예정이므로 dead code 를 허용한다.

#![allow(dead_code)]

use std::time::Duration;

use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::EndpointConfig;

/// LLM 요청 오류 분류.
#[derive(Debug, Error)]
pub enum LlmError {
    /// HTTP 4xx/5xx → failed.
    #[error("API error: {status}: {body}")]
    Api { status: u16, body: String },
    /// 타임아웃·연결 거절 → failed.
    #[error("네트워크 오류: {0}")]
    Network(String),
    /// SSE 스트림 파싱 오류.
    #[error("SSE 파싱 오류: {0}")]
    Sse(String),
    /// 응답 JSON 파싱 오류.
    #[error("응답 파싱 오류: {0}")]
    Json(String),
    /// 응답이 비어 있음 (no content).
    #[error("빈 응답")]
    Empty,
    /// 스트림이 예상치 못하게 종료됨.
    #[error("스트림이 끝나기 전에 종료됨")]
    Truncated,
}

/// 도구 정의 (OpenAI tools 배열 항목).
#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    pub r#type: String,
    pub function: ToolFunction,
}

/// 도구 함수 정의.
#[derive(Debug, Clone, Serialize)]
pub struct ToolFunction {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// 대화 메시지 (OpenAI messages 배열 항목).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub struct Message {
    pub role: String,
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// 요청 본문.
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    pub stream: bool,
    pub max_tokens: u64,
    pub temperature: Option<f64>,
    pub frequency_penalty: f64,
    pub presence_penalty: f64,
}

/// SSE delta 내부의 툴콜 조각.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallDelta {
    pub index: Option<u64>,
    pub id: Option<String>,
    pub r#type: Option<String>,
    pub function: Option<ToolCallFunctionDelta>,
}

/// 툴콜 함수 조각 (name 은 첫 청크에만 오고, arguments 는 조각으로 쪼개져 온다).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunctionDelta {
    pub name: Option<String>,
    pub arguments: Option<String>,
}

/// SSE delta (stream chunk 의 `.choices[0].delta`).
#[derive(Debug, Clone, Deserialize)]
pub struct Delta {
    pub content: Option<String>,
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallDelta>,
}

/// SSE chunk 최상위 객체.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatChunk {
    pub choices: Option<Vec<ChatChoice>>,
    pub usage: Option<Usage>,
}

/// choices[0] 항목.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatChoice {
    pub delta: Delta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// usage 수집.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

/// 툴콜 누적 결과 (index 기반).
#[derive(Debug, Clone, Default)]
pub struct AccumulatedToolCall {
    pub index: u64,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments: String,
}

/// 최종 툴콜 (인자 JSON 파싱 완료).
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: Option<String>,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// 스트리밍 응답 결과.
#[derive(Debug)]
pub struct ChatResponse {
    pub content: Option<String>,
    pub reasoning_content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: String,
    pub usage: Usage,
    pub incomplete: bool,
}

/// 요청 옵션.
#[derive(Debug, Clone)]
pub struct ChatOptions {
    pub endpoint: EndpointConfig,
    pub temperature: Option<f64>,
}

/// 최대 툴콜 arguments 누적 길이 (가드).
const MAX_TOOLCALL_ARGS: usize = 64 * 1024;

/// SSE 스트리밍 클라이언트 (DESIGN.md §4.2).
pub struct LlmClient {
    client: reqwest::Client,
    timeout: Duration,
}

impl LlmClient {
    /// 새 클라이언트를 만든다.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            timeout: Duration::from_secs(120),
        }
    }

    /// 스트리밍 채팅 완료를 실행한다.
    pub async fn chat(
        &self,
        opts: &ChatOptions,
        request: &ChatRequest,
    ) -> Result<ChatResponse, LlmError> {
        let url = format!(
            "{}/chat/completions",
            opts.endpoint.url.trim_end_matches('/')
        );

        let mut req = self.client.post(&url).timeout(self.timeout).json(request);
        if let Some(key) = &opts.endpoint.api_key {
            req = req.bearer_auth(key);
        }

        let resp = req.send().await.map_err(|e| {
            // 타임아웃·연결 거절 → Network 오류.
            LlmError::Network(e.to_string())
        })?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::Api {
                status: status.as_u16(),
                body,
            });
        }

        let bytes = resp.bytes_stream().eventsource();
        tokio::pin!(bytes);

        let mut content: Option<String> = None;
        let mut reasoning: Option<String> = None;
        // index → 누적 툴콜.
        let mut tool_calls: std::collections::BTreeMap<u64, AccumulatedToolCall> =
            std::collections::BTreeMap::new();
        let mut usage = Usage::default();
        let mut finish_reason: Option<String> = None;
        let mut saw_any_delta = false;

        while let Some(event) = bytes.next().await {
            let event = event.map_err(|e| LlmError::Sse(e.to_string()))?;
            if event.data.trim().is_empty() {
                continue;
            }
            // `[DONE]` 마커는 무시.
            if event.data.trim() == "[DONE]" {
                break;
            }

            let chunk: ChatChunk =
                serde_json::from_str(&event.data).map_err(|e| LlmError::Json(e.to_string()))?;

            if let Some(u) = chunk.usage {
                usage = u;
            }

            if let Some(choices) = chunk.choices {
                if let Some(choice) = choices.first() {
                    saw_any_delta = true;
                    let d = &choice.delta;

                    if let Some(c) = &d.content {
                        content.get_or_insert_with(String::new).push_str(c);
                    }
                    if let Some(r) = &d.reasoning_content {
                        // reasoning_content 는 별도 버퍼로 누적 (Live·히스토리 전용).
                        reasoning.get_or_insert_with(String::new).push_str(r);
                    }

                    for tc in &d.tool_calls {
                        let idx = tc.index.unwrap_or(0);
                        let entry = tool_calls
                            .entry(idx)
                            .or_insert_with(|| AccumulatedToolCall {
                                index: idx,
                                id: None,
                                name: None,
                                arguments: String::new(),
                            });
                        // id·name 은 첫 청크에만 오므로 첫 값만 채운다.
                        if entry.id.is_none() {
                            entry.id = tc.id.clone();
                        }
                        if entry.name.is_none() {
                            if let Some(f) = &tc.function {
                                if let Some(n) = &f.name {
                                    entry.name = Some(n.clone());
                                }
                            }
                        }
                        // arguments 는 조각으로 이어 붙인다.
                        if let Some(f) = &tc.function {
                            if let Some(args) = &f.arguments {
                                if entry.arguments.len() < MAX_TOOLCALL_ARGS {
                                    entry.arguments.push_str(args);
                                }
                            }
                        }
                    }

                    if let Some(fr) = &choice.finish_reason {
                        if !fr.is_empty() {
                            finish_reason = Some(fr.clone());
                        }
                    }
                }
            }
        }

        if !saw_any_delta {
            return Err(LlmError::Empty);
        }

        let finish_reason = finish_reason.unwrap_or_else(|| "stop".to_string());

        // finish_reason 처리 (§4.2): stop/tool_calls 정상, length+내용 비면 incomplete.
        let incomplete = match finish_reason.as_str() {
            "stop" | "tool_calls" => false,
            "length" => {
                let has_content = content.as_deref().is_some_and(|c| !c.trim().is_empty());
                let has_tools = !tool_calls.is_empty();
                // 내용이 있으면 정상 (조각적 완료), 비면 incomplete.
                !(has_content || has_tools)
            }
            // 그 외 (예: "content_filter") 는 오류로 취급하되 incomplete 로.
            _ => true,
        };

        // 누적 툴콜을 최종 툴콜로 변환 (arguments JSON 파싱).
        let mut final_tools = Vec::new();
        for (_, acc) in tool_calls {
            let name = acc.name.clone().unwrap_or_default();
            let parsed = if acc.arguments.trim().is_empty() {
                serde_json::Value::Object(serde_json::Map::new())
            } else {
                serde_json::from_str(&acc.arguments)
                    .unwrap_or(serde_json::Value::String(acc.arguments))
            };
            final_tools.push(ToolCall {
                id: acc.id,
                name,
                arguments: parsed,
            });
        }

        Ok(ChatResponse {
            content,
            reasoning_content: reasoning,
            tool_calls: final_tools,
            finish_reason,
            usage,
            incomplete,
        })
    }
}

impl Default for LlmClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_endpoint(url: &str) -> EndpointConfig {
        EndpointConfig {
            url: url.to_string(),
            api_key: None,
            model: "m".to_string(),
            context_tokens: 4096,
            vision: false,
            thinking: true,
            max_iterations: 200,
        }
    }

    /// SSE 응답 본문을 만든다 (각 라인이 `data:` 프리픽스).
    fn sse_body(chunks: &[serde_json::Value]) -> String {
        let mut out = String::new();
        for c in chunks {
            out.push_str("data: ");
            out.push_str(&c.to_string());
            out.push_str("\n\n");
        }
        out.push_str("data: [DONE]\n\n");
        out.push_str("data: [DONE]\n\n");
        out
    }

    #[tokio::test]
    async fn streams_content_and_usage() {
        let server = MockServer::start().await;
        let body = sse_body(&[
            json!({
                "choices": [{"delta": {"content": "안녕"}, "finish_reason": null}],
                "usage": null
            }),
            json!({
                "choices": [{"delta": {"content": "하세요"}, "finish_reason": null}],
                "usage": null
            }),
            json!({
                "choices": [{"delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
            }),
        ]);
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .mount(&server)
            .await;

        let client = LlmClient::new();
        let opts = ChatOptions {
            endpoint: test_endpoint(&server.uri()),
            temperature: None,
        };
        let req = ChatRequest {
            model: "m".to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: Some("hi".to_string()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            }],
            tools: vec![],
            stream: true,
            max_tokens: 1024,
            temperature: None,
            frequency_penalty: 0.3,
            presence_penalty: 0.3,
        };

        let resp = client.chat(&opts, &req).await.unwrap();
        assert_eq!(resp.content.as_deref(), Some("안녕하세요"));
        assert_eq!(resp.finish_reason, "stop");
        assert!(!resp.incomplete);
        assert_eq!(resp.usage.prompt_tokens, Some(10));
        assert_eq!(resp.usage.completion_tokens, Some(5));
        assert_eq!(resp.usage.total_tokens, Some(15));
    }

    #[tokio::test]
    async fn accumulates_tool_calls_by_index() {
        let server = MockServer::start().await;
        // 툴콜이 여러 청크로 쪼개져 오고, index 0 과 index 1 이 동시에 누적된다.
        let body = sse_body(&[
            json!({
                "choices": [{"delta": {
                    "tool_calls": [
                        {"index": 0, "id": "call_1", "type": "function",
                         "function": {"name": "read_file", "arguments": ""}}
                    ]
                }, "finish_reason": null}],
                "usage": null
            }),
            json!({
                "choices": [{"delta": {
                    "tool_calls": [
                        {"index": 0, "function": {"arguments": "{\"path\":"}},
                        {"index": 1, "id": "call_2", "type": "function",
                         "function": {"name": "bash", "arguments": ""}}
                    ]
                }, "finish_reason": null}],
                "usage": null
            }),
            json!({
                "choices": [{"delta": {
                    "tool_calls": [
                        {"index": 0, "function": {"arguments": "\"src/main.rs\"}"}},
                        {"index": 1, "function": {"arguments": "{\"command\":\"ls\"}"}}
                    ]
                }, "finish_reason": "tool_calls"}],
                "usage": null
            }),
        ]);
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
            .mount(&server)
            .await;

        let client = LlmClient::new();
        let opts = ChatOptions {
            endpoint: test_endpoint(&server.uri()),
            temperature: None,
        };
        let req = ChatRequest {
            model: "m".to_string(),
            messages: vec![],
            tools: vec![],
            stream: true,
            max_tokens: 1024,
            temperature: None,
            frequency_penalty: 0.3,
            presence_penalty: 0.3,
        };

        let resp = client.chat(&opts, &req).await.unwrap();
        assert_eq!(resp.finish_reason, "tool_calls");
        assert!(!resp.incomplete);
        assert_eq!(resp.tool_calls.len(), 2);

        let t0 = &resp.tool_calls[0];
        assert_eq!(t0.id.as_deref(), Some("call_1"));
        assert_eq!(t0.name, "read_file");
        assert_eq!(t0.arguments, json!({"path": "src/main.rs"}));

        let t1 = &resp.tool_calls[1];
        assert_eq!(t1.id.as_deref(), Some("call_2"));
        assert_eq!(t1.name, "bash");
        assert_eq!(t1.arguments, json!({"command": "ls"}));
    }

    #[tokio::test]
    async fn separates_reasoning_content() {
        let server = MockServer::start().await;
        let body = sse_body(&[
            json!({
                "choices": [{"delta": {"reasoning_content": "생각"}, "finish_reason": null}],
                "usage": null
            }),
            json!({
                "choices": [{"delta": {"reasoning_content": " 더 하기", "content": "답"}, "finish_reason": null}],
                "usage": null
            }),
            json!({
                "choices": [{"delta": {}, "finish_reason": "stop"}],
                "usage": null
            }),
        ]);
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
            .mount(&server)
            .await;

        let client = LlmClient::new();
        let opts = ChatOptions {
            endpoint: test_endpoint(&server.uri()),
            temperature: None,
        };
        let req = ChatRequest {
            model: "m".to_string(),
            messages: vec![],
            tools: vec![],
            stream: true,
            max_tokens: 1024,
            temperature: None,
            frequency_penalty: 0.3,
            presence_penalty: 0.3,
        };

        let resp = client.chat(&opts, &req).await.unwrap();
        // reasoning_content 는 content 와 분리되어 누적된다.
        assert_eq!(resp.content.as_deref(), Some("답"));
        assert_eq!(resp.reasoning_content.as_deref(), Some("생각 더 하기"));
    }

    #[tokio::test]
    async fn maps_http_error_to_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_raw("{\"error\":\"bad request\"}", "application/json"),
            )
            .mount(&server)
            .await;

        let client = LlmClient::new();
        let opts = ChatOptions {
            endpoint: test_endpoint(&server.uri()),
            temperature: None,
        };
        let req = ChatRequest {
            model: "m".to_string(),
            messages: vec![],
            tools: vec![],
            stream: true,
            max_tokens: 1024,
            temperature: None,
            frequency_penalty: 0.3,
            presence_penalty: 0.3,
        };

        let err = client.chat(&opts, &req).await.unwrap_err();
        match err {
            LlmError::Api { status, body } => {
                assert_eq!(status, 400);
                assert!(body.contains("bad request"));
            }
            other => panic!("예상치 못한 오류: {other:?}"),
        }
    }

    #[tokio::test]
    async fn maps_connection_refused_to_network() {
        let client = LlmClient::new();
        let opts = ChatOptions {
            endpoint: test_endpoint("http://127.0.0.1:1"),
            temperature: None,
        };
        let req = ChatRequest {
            model: "m".to_string(),
            messages: vec![],
            tools: vec![],
            stream: true,
            max_tokens: 1024,
            temperature: None,
            frequency_penalty: 0.3,
            presence_penalty: 0.3,
        };

        let err = client.chat(&opts, &req).await.unwrap_err();
        match err {
            LlmError::Network(_) => {}
            other => panic!("예상치 못한 오류: {other:?}"),
        }
    }

    #[tokio::test]
    async fn marks_length_without_content_incomplete() {
        let server = MockServer::start().await;
        let body = sse_body(&[json!({
            "choices": [{"delta": {}, "finish_reason": "length"}],
            "usage": null
        })]);
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
            .mount(&server)
            .await;

        let client = LlmClient::new();
        let opts = ChatOptions {
            endpoint: test_endpoint(&server.uri()),
            temperature: None,
        };
        let req = ChatRequest {
            model: "m".to_string(),
            messages: vec![],
            tools: vec![],
            stream: true,
            max_tokens: 1024,
            temperature: None,
            frequency_penalty: 0.3,
            presence_penalty: 0.3,
        };

        let resp = client.chat(&opts, &req).await.unwrap();
        assert_eq!(resp.finish_reason, "length");
        assert!(resp.incomplete);
        assert!(resp.content.is_none());
        assert!(resp.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn maps_empty_stream_to_empty_error() {
        let server = MockServer::start().await;
        let body = "data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
            .mount(&server)
            .await;

        let client = LlmClient::new();
        let opts = ChatOptions {
            endpoint: test_endpoint(&server.uri()),
            temperature: None,
        };
        let req = ChatRequest {
            model: "m".to_string(),
            messages: vec![],
            tools: vec![],
            stream: true,
            max_tokens: 1024,
            temperature: None,
            frequency_penalty: 0.3,
            presence_penalty: 0.3,
        };

        let err = client.chat(&opts, &req).await.unwrap_err();
        match err {
            LlmError::Empty => {}
            other => panic!("예상치 못한 오류: {other:?}"),
        }
    }
}
