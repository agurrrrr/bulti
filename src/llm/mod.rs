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
    /// reasoning effort (thinking 모델). None 이면 요청에서 생략.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
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

/// TUI 표시용 도구 호출 이벤트 (채널 전용 — SSE 파싱 대상이 아니므로
/// serde `skip` 처리).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ToolEvent {
    pub name: String,
    /// 인자 JSON 에서 핵심 값을 요약한 한 줄.
    pub args_summary: String,
    /// `false` 이면 "호출 중" 표시, `true` 면 완료(성공) 표시.
    pub ok: bool,
    /// 실패 시 에러 메시지.
    pub error: Option<String>,
}

/// SSE delta (stream chunk 의 `.choices[0].delta`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Delta {
    pub content: Option<String>,
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallDelta>,
    /// agent 루프가 `delta_tx` 로 보내는 도구 호출 이벤트 (TUI 표시용).
    /// SSE 응답에서는 항상 `None`.
    #[serde(default)]
    pub tool_call_event: Option<ToolEvent>,
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
    #[serde(default)]
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

/// 스트림 유휴(idle) 타임아웃 기본값.
///
/// reqwest 의 `read_timeout` 은 청크를 받을 때마다 리셋되므로 "총 소요 시간"이
/// 아니라 "마지막 데이터 이후 경과 시간"에 적용된다. 로컬 모델이 긴 응답을
/// 생성하거나 큰 프롬프트를 prefill 하는 동안에도 스트림이 끊기지 않는다.
///
/// 기존에는 요청별 총 타임아웃(120초)을 걸어 두어, 큰 작업에서 120초를 넘겨
/// 생성 중이던 스트림이 중간에 잘리고 세그먼트가 failed 로 끝났다
/// (`세그먼트 실패 — 이전 대화 맥락은 유지됩니다`).
pub const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// SSE 스트리밍 클라이언트 (DESIGN.md §4.2).
#[derive(Clone)]
pub struct LlmClient {
    client: reqwest::Client,
}

impl LlmClient {
    /// 새 클라이언트를 만든다.
    pub fn new() -> Self {
        Self::with_idle_timeout(DEFAULT_STREAM_IDLE_TIMEOUT)
    }

    /// 유휴 타임아웃을 지정해 클라이언트를 만든다.
    ///
    /// 테스트와 느린 로컬 모델 대응을 위해 열어 둔다. 총 타임아웃(`timeout`)은
    /// 걸지 않는다 — 스트리밍 생성이 길어져도 살아 있게 하려면 유휴 타임아웃만
    /// 적용해야 한다.
    pub fn with_idle_timeout(idle: Duration) -> Self {
        let client = reqwest::Client::builder()
            .read_timeout(idle)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client }
    }

    /// 스트리밍 채팅 완료를 실행한다.
    ///
    /// `delta_tx` 가 주어지면 도착하는 각 SSE 델타를 채널로 전송한다 (TUI 점진적
    /// 렌더링용). `None` 이면 기존처럼 최종 `ChatResponse` 만 누적·반환한다.
    pub async fn chat(
        &self,
        opts: &ChatOptions,
        request: &ChatRequest,
        delta_tx: Option<tokio::sync::mpsc::UnboundedSender<Delta>>,
    ) -> Result<ChatResponse, LlmError> {
        let url = format!(
            "{}/chat/completions",
            opts.endpoint.url.trim_end_matches('/')
        );

        // 총 타임아웃은 걸지 않는다. 스트리밍 생성이 길어져도 유휴할 때만
        // 클라이언트의 read_timeout 이 동작하도록 둔다 (DEFAULT_STREAM_IDLE_TIMEOUT).
        let mut req = self.client.post(&url).json(request);
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

            // TUI 점진적 렌더링용: 파싱된 델타를 채널로 전송한다.
            if let (Some(tx), Some(choices)) = (&delta_tx, &chunk.choices) {
                if let Some(choice) = choices.first() {
                    let _ = tx.send(choice.delta.clone());
                }
            }

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
            reasoning_effort: None,
            input_price_per_mtok: None,
            output_price_per_mtok: None,
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
            reasoning_effort: None,
        };

        let resp = client.chat(&opts, &req, None).await.unwrap();
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
            reasoning_effort: None,
        };

        let resp = client.chat(&opts, &req, None).await.unwrap();
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
            reasoning_effort: None,
        };

        let resp = client.chat(&opts, &req, None).await.unwrap();
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
            reasoning_effort: None,
        };

        let err = client.chat(&opts, &req, None).await.unwrap_err();
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
            reasoning_effort: None,
        };

        let err = client.chat(&opts, &req, None).await.unwrap_err();
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
            reasoning_effort: None,
        };

        let resp = client.chat(&opts, &req, None).await.unwrap();
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
            reasoning_effort: None,
        };

        let err = client.chat(&opts, &req, None).await.unwrap_err();
        match err {
            LlmError::Empty => {}
            other => panic!("예상치 못한 오류: {other:?}"),
        }
    }

    // ── 스트리밍 유휴 타임아웃 (세그먼트 실패 회귀) ──

    /// 원시 TCP 서버로 SSE 를 청크 사이 지연을 두고 흘려보낸다.
    ///
    /// 청크 사이 간격(`gap`)은 임의로 조절할 수 있어, "총 소요 시간"이 아니라
    /// "유휴 시간"이 타임아웃 기준인지 검증할 수 있다.
    async fn spawn_slow_sse_server(chunks: Vec<String>, gap: Duration) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // 요청 헤더까지만 읽고 본문은 무시한다.
            let mut buf = Vec::new();
            let mut tmp = [0u8; 512];
            loop {
                match sock.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                }
            }
            let header =
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
            let _ = sock.write_all(header.as_bytes()).await;
            let _ = sock.flush().await;
            for c in chunks {
                // 각 청크를 보내기 전에 지연을 둔다 — 첫 청크 지연으로 stall 을
                // 재현할 수 있고, 중간 지연으로 긴 활성 스트림을 재현할 수 있다.
                tokio::time::sleep(gap).await;
                let _ = sock.write_all(c.as_bytes()).await;
                let _ = sock.flush().await;
            }
            let _ = sock.write_all(b"data: [DONE]\n\n").await;
            let _ = sock.flush().await;
        });
        format!("http://{addr}")
    }

    fn sse_content_chunk(text: &str) -> String {
        format!(
            "data: {}\n\n",
            json!({"choices": [{"delta": {"content": text}, "finish_reason": null}]})
        )
    }

    fn simple_request() -> ChatRequest {
        ChatRequest {
            model: "m".to_string(),
            messages: vec![],
            tools: vec![],
            stream: true,
            max_tokens: 1024,
            temperature: None,
            frequency_penalty: 0.3,
            presence_penalty: 0.3,
            reasoning_effort: None,
        }
    }

    /// 총 스트리밍 시간이 유휴 타임아웃보다 길어도, 청크가 계속 오면 성공해야 한다.
    ///
    /// 기존 총 타임아웃(120초) 방식은 긴 생성 중에도 스트림을 끊어 세그먼트를
    /// failed 로 만들었다. 이 테스트는 그 회귀를 막는다.
    #[tokio::test]
    async fn long_active_stream_survives_idle_timeout() {
        let chunks: Vec<String> = (0..6).map(|_| sse_content_chunk("가")).collect();
        // gap 100ms × 6 = 약 600ms (유휴 타임아웃 300ms 보다 김)
        let url = spawn_slow_sse_server(chunks, Duration::from_millis(100)).await;

        let client = LlmClient::with_idle_timeout(Duration::from_millis(300));
        let opts = ChatOptions {
            endpoint: test_endpoint(&url),
            temperature: None,
        };

        let resp = client.chat(&opts, &simple_request(), None).await.unwrap();
        assert_eq!(resp.content.as_deref(), Some("가가가가가가"));
    }

    /// 청크가 유휴 타임아웃보다 오래 오지 않으면 중단되어야 한다.
    #[tokio::test]
    async fn stalled_stream_hits_idle_timeout() {
        // 서버가 헤더만 보낸 뒤 첫 청크를 유휴 타임아웃(50ms)보다 늦게 보낸다.
        let url =
            spawn_slow_sse_server(vec![sse_content_chunk("늦음")], Duration::from_millis(200))
                .await;

        let client = LlmClient::with_idle_timeout(Duration::from_millis(50));
        let opts = ChatOptions {
            endpoint: test_endpoint(&url),
            temperature: None,
        };

        let err = tokio::time::timeout(
            Duration::from_secs(5),
            client.chat(&opts, &simple_request(), None),
        )
        .await
        .expect("테스트가 스스로 타임아웃되면 안 된다")
        .unwrap_err();
        assert!(
            matches!(err, LlmError::Network(_) | LlmError::Sse(_)),
            "예상치 못한 오류: {err:?}"
        );
    }
}
