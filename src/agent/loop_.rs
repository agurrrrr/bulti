//! 세그먼트 실행 루프 (DESIGN.md §4.3, §4.6).
//!
//! 한 세그먼트(체인 한 단위)의 실행을 담당한다:
//! - 시스템+유저 프롬프트로 메시지 초기화
//! - 도구 정의를 포함한 채팅 요청 반복
//! - 가드(§5) 적용으로 퇴행·거짓 완료·stuck 방어
//! - 도구 호출로 결과를 메시지에 반영
//! - 도구 호출이 없으면 핸드오프 시도 (§4.6)
//!
//! 세그먼트 상태: `completed` / `failed` / `incomplete` / `interrupted`.
//! 결과에는 content / usage / files_touched / depth 와 핸드오프 판정을 포함한다.

use crate::agent::guards::{
    check_build_gate, check_empty_loop, check_fffd_degenerate, check_future_intention,
    check_pause_summary, check_stream_repetition, check_stuck_signature, update_after_tool_call,
    GuardContext, GuardOutcome,
};
use crate::agent::handoff::{
    build_handoff_messages, build_handoff_prompt, handoff_max_tokens, is_handoff_summary_acceptable,
    parse_handoff_response, should_attempt_handoff, HandoffDecision, HandoffDepthGuard,
    HandoffResponse,
};
use crate::agent::context::estimate_messages_tokens;
use crate::config::EndpointConfig;
use crate::llm::{ChatOptions, ChatRequest, LlmClient, Message};
use crate::tools::ToolRegistry;

/// 세그먼트 종료 상태.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentStatus {
    /// 체인 전체 완료 (핸드오프 NEXT_TASK 없음).
    Completed,
    /// LLM 오류·네트워크 오류 등으로 실패.
    Failed,
    /// 가드·max_iterations·핸드오프 게이트 실패 등으로 미완료.
    Incomplete,
    /// SIGINT 등으로 중단.
    Interrupted,
}

impl SegmentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Incomplete => "incomplete",
            Self::Interrupted => "interrupted",
        }
    }
}

/// 세그먼트 실행 결과.
#[derive(Debug, Clone)]
pub struct SegmentResult {
    pub status: SegmentStatus,
    pub content: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub files_touched: Vec<String>,
    pub depth: u32,
    /// 핸드오프 판정 (도구 호출 없이 시도했을 때만 Some).
    pub handoff: Option<HandoffDecision>,
    /// 핸드오프 응답 (다음 세그먼트 프롬프트 재조립용).
    pub handoff_response: Option<HandoffResponse>,
}

/// 세그먼트 실행 파라미터 묶음.
#[derive(Debug, Clone)]
pub struct SegmentParams {
    pub endpoint: EndpointConfig,
    pub temperature: Option<f64>,
    pub system_prompt: String,
    pub user_prompt: String,
    pub max_iterations: u32,
    pub context_tokens: u64,
    pub handoff_threshold_pct: u8,
    pub max_handoff_depth: u32,
    pub handoff_warn_depth: u32,
}

/// 한 세그먼트를 실행한다.
///
/// 도구 호출 루프 → 가드 → (도구 없으면) 핸드오프 시도 순서로 진행한다.
pub async fn run_segment(
    client: &LlmClient,
    registry: &ToolRegistry,
    params: &SegmentParams,
    depth: u32,
) -> SegmentResult {
    let mut messages = vec![
        Message {
            role: "system".to_string(),
            content: Some(params.system_prompt.clone()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        },
        Message {
            role: "user".to_string(),
            content: Some(params.user_prompt.clone()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        },
    ];

    let opts = ChatOptions {
        endpoint: params.endpoint.clone(),
        temperature: params.temperature,
    };
    let tools = registry.definitions();

    let mut guard = GuardContext::default();
    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;
    let mut final_content = String::new();
    let mut depth_guard = HandoffDepthGuard { depth };

    for _ in 0..params.max_iterations {
        let req = ChatRequest {
            model: opts.endpoint.model.clone(),
            messages: messages.clone(),
            tools: tools.clone(),
            stream: true,
            max_tokens: 4096,
            temperature: opts.temperature,
            frequency_penalty: 0.0,
            presence_penalty: 0.0,
        };

        let resp = match client.chat(&opts, &req).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("LLM 오류: {e}");
                return SegmentResult {
                    status: SegmentStatus::Failed,
                    content: final_content,
                    input_tokens,
                    output_tokens,
                    files_touched: registry.files_touched(),
                    depth,
                    handoff: None,
                    handoff_response: None,
                };
            }
        };

        // usage 집계.
        if let Some(u) = resp.usage.prompt_tokens {
            input_tokens = u;
        }
        if let Some(u) = resp.usage.completion_tokens {
            output_tokens = u;
        }
        if let Some(c) = &resp.content {
            final_content.push_str(c);
        }

        // 가드 적용.
        if resp.content.as_deref().is_none_or(|c| c.trim().is_empty()) {
            guard.empty_turns += 1;
        } else {
            guard.empty_turns = 0;
        }
        let combined = resp.content.clone().unwrap_or_default();
        let outcomes = [
            check_empty_loop(&guard),
            check_stream_repetition(&combined),
            check_stuck_signature(&guard),
            check_fffd_degenerate(&combined),
            check_future_intention(&guard, resp.tool_calls.len(), &combined),
            check_build_gate(&guard, &combined),
            check_pause_summary(&guard, &combined),
        ];
        for outcome in outcomes {
            if let GuardOutcome::Trigger(reason) = outcome {
                tracing::warn!("가드 발동: {reason}");
                return SegmentResult {
                    status: SegmentStatus::Incomplete,
                    content: final_content,
                    input_tokens,
                    output_tokens,
                    files_touched: registry.files_touched(),
                    depth,
                    handoff: None,
                    handoff_response: None,
                };
            }
        }

        // 도구 호출 처리.
        if !resp.tool_calls.is_empty() {
            for tc in &resp.tool_calls {
                let sig = crate::agent::guards::tool_signature(&tc.name, &tc.arguments);
                let is_state_change = is_state_change_tool(&tc.name);
                update_after_tool_call(&mut guard, sig, is_state_change);

                let result = registry.dispatch(&tc.name, tc.arguments.clone()).await;
                let result_text = match result {
                    Ok(t) => t,
                    Err(e) => format!("오류: {e}"),
                };

                // 도구 결과 메시지 추가.
                messages.push(Message {
                    role: "assistant".to_string(),
                    content: None,
                    tool_calls: Some(vec![tool_call_delta(tc)]),
                    tool_call_id: None,
                    name: None,
                });
                messages.push(Message {
                    role: "tool".to_string(),
                    content: Some(result_text),
                    tool_calls: None,
                    tool_call_id: tc.id.clone(),
                    name: Some(tc.name.clone()),
                });
            }
            continue;
        }

        // 도구 호출 없음 → 핸드오프 시도 (§4.6).
        let should_try = should_attempt_handoff(
            &messages,
            params.context_tokens,
            params.handoff_threshold_pct,
        ) || resp.finish_reason == "stop";
        if !should_try {
            // 아직 맥락이 충분하지 않으면 계속 진행.
            continue;
        }

        let (handoff_decision, handoff_response) = attempt_handoff(
            client,
            &opts,
            &params.system_prompt,
            &params.user_prompt,
            &messages,
            &mut depth_guard,
            params.context_tokens,
        )
        .await;

        return match handoff_decision {
            HandoffDecision::Handoff => SegmentResult {
                status: SegmentStatus::Completed,
                content: final_content,
                input_tokens,
                output_tokens,
                files_touched: registry.files_touched(),
                depth: depth_guard.depth,
                handoff: Some(HandoffDecision::Handoff),
                handoff_response,
            },
            HandoffDecision::Complete => SegmentResult {
                status: SegmentStatus::Completed,
                content: final_content,
                input_tokens,
                output_tokens,
                files_touched: registry.files_touched(),
                depth: depth_guard.depth,
                handoff: Some(HandoffDecision::Complete),
                handoff_response,
            },
            HandoffDecision::Fallback => {
                // 게이트 실패 → trim 폴백으로 현재 세그먼트 계속.
                tracing::warn!("핸드오프 게이트 실패 — trim 폴백");
                let (trimmed, _) = crate::agent::context::trim_messages(messages, 4);
                messages = trimmed;
                continue;
            }
        };
    }

    // max_iterations 초과 → incomplete.
    SegmentResult {
        status: SegmentStatus::Incomplete,
        content: final_content,
        input_tokens,
        output_tokens,
        files_touched: registry.files_touched(),
        depth,
        handoff: None,
        handoff_response: None,
    }
}

/// 핸드오프 요청을 시도하고 판정 + 파싱 응답을 반환한다.
async fn attempt_handoff(
    client: &LlmClient,
    opts: &ChatOptions,
    system_prompt: &str,
    user_prompt: &str,
    messages: &[Message],
    depth_guard: &mut HandoffDepthGuard,
    context_tokens: u64,
) -> (HandoffDecision, Option<HandoffResponse>) {
    // depth 가드: runaway 면 핸드오프 금지 → Fallback.
    if depth_guard.runaway() {
        return (HandoffDecision::Fallback, None);
    }

    let _handoff_prompt = build_handoff_prompt();
    let handoff_msgs = build_handoff_messages(system_prompt, user_prompt);
    let req = ChatRequest {
        model: opts.endpoint.model.clone(),
        messages: handoff_msgs,
        tools: vec![],
        stream: true,
        max_tokens: handoff_max_tokens(context_tokens),
        temperature: opts.temperature,
        frequency_penalty: 0.0,
        presence_penalty: 0.0,
    };

    // (표현용) 대화 전체 토큰 추정을 조회해 경고 판단.
    let _ = estimate_messages_tokens(messages);

    let resp = match client.chat(opts, &req).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("핸드오프 요청 오류: {e}");
            return (HandoffDecision::Fallback, None);
        }
    };

    let content = resp.content.unwrap_or_default();
    let parsed = parse_handoff_response(&content);
    if !is_handoff_summary_acceptable(&parsed.summary) {
        return (HandoffDecision::Fallback, None);
    }

    depth_guard.increment();
    if parsed.next_task.trim().is_empty() {
        (HandoffDecision::Complete, Some(parsed))
    } else {
        (HandoffDecision::Handoff, Some(parsed))
    }
}

/// 도구 이름이 상태 변경(파일·bash)인지 판단한다.
fn is_state_change_tool(name: &str) -> bool {
    matches!(
        name,
        "write_file" | "edit_file" | "bash" | "mcp_call" | "skill_load"
    )
}

/// ToolCall 을 Message.tool_calls 용 ToolCallDelta 로 변환한다.
fn tool_call_delta(tc: &crate::llm::ToolCall) -> crate::llm::ToolCallDelta {
    crate::llm::ToolCallDelta {
        index: Some(0),
        id: tc.id.clone(),
        r#type: Some("function".to_string()),
        function: Some(crate::llm::ToolCallFunctionDelta {
            name: Some(tc.name.clone()),
            arguments: Some(tc.arguments.to_string()),
        }),
    }
}

/// 세그먼트 상태 목록으로 체인 전체 상태를 판정한다.
pub fn chain_status(segment_statuses: &[SegmentStatus]) -> &'static str {
    let strs: Vec<&str> = segment_statuses
        .iter()
        .map(SegmentStatus::as_str)
        .collect();
    crate::agent::handoff::chain_status(&strs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn params(url: &str, max_iterations: u32) -> SegmentParams {
        SegmentParams {
            endpoint: EndpointConfig {
                url: url.to_string(),
                api_key: None,
                model: "test-model".to_string(),
                context_tokens: 0,
                vision: false,
                thinking: false,
                max_iterations,
            },
            temperature: None,
            system_prompt: "시스템".to_string(),
            user_prompt: "유저".to_string(),
            max_iterations,
            context_tokens: 0,
            handoff_threshold_pct: 75,
            max_handoff_depth: 12,
            handoff_warn_depth: 8,
        }
    }

    fn sse_chunk(content: &str) -> String {
        format!(
            "data: {}\n\n",
            serde_json::json!({
                "choices": [{"delta": {"content": content}}]
            })
        )
    }

    #[tokio::test]
    async fn segment_completes_when_stop_without_next_task() {
        let server = MockServer::start().await;
        // endpoint url 은 server.uri() 그대로이므로 chat 은 /chat/completions 로 요청한다.
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(
                    sse_chunk(
                        "1. 원 요청/의도\n\
열람·변경 파일: src/cli/run_cmd.rs 와 src/agent/loop_.rs 를 수정\n\
한 일: bulti run 명령을 구현하고 핸드오프 체인을 연결\n\
남은 작업: 없음, 모든 작업이 완료됨\n\
하지 말 것: 이미 완료된 작업을 되돌리지 말 것, wiremock 응답은 set_body_raw 와 &str 을 사용\n\
다음 한 걸음: 체인을 완료하고 최종 보고를 작성\n\
===NEXT_TASK===\n",
                    )
                    .as_str(),
                    "text/event-stream",
                ),
            )
            .expect(2)
            .mount(&server)
            .await;

        let client = LlmClient::new();
        let registry = ToolRegistry::new(false);
        let params = params(server.uri().as_str(), 100);
        let result = run_segment(&client, &registry, &params, 0).await;

        assert_eq!(result.status, SegmentStatus::Completed);
    }

    #[tokio::test]
    async fn segment_failed_on_network_error() {
        let client = LlmClient::new();
        let registry = ToolRegistry::new(false);
        let params = params("http://127.0.0.1:1/v1", 100);
        let result = run_segment(&client, &registry, &params, 0).await;
        assert_eq!(result.status, SegmentStatus::Failed);
    }

    #[tokio::test]
    async fn segment_incomplete_on_max_iterations() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(sse_chunk("").as_str(), "text/event-stream"),
            )
            .expect(2)
            .mount(&server)
            .await;

        let client = LlmClient::new();
        let registry = ToolRegistry::new(false);
        let params = params(server.uri().as_str(), 1);
        let result = run_segment(&client, &registry, &params, 0).await;

        // max_iterations=1 로 첫 chat 후 도구 없음·핸드오프 시도 → 게이트 실패로 fallback → 루프 종료 → incomplete
        assert_eq!(result.status, SegmentStatus::Incomplete);
    }

    #[test]
    fn chain_status_aggregates() {
        let completed = [
            SegmentStatus::Completed,
            SegmentStatus::Completed,
            SegmentStatus::Completed,
        ];
        assert_eq!(chain_status(&completed), "completed");

        let mixed = [
            SegmentStatus::Completed,
            SegmentStatus::Incomplete,
            SegmentStatus::Completed,
        ];
        assert_eq!(chain_status(&mixed), "incomplete");
    }

    #[test]
    fn segment_status_as_str() {
        assert_eq!(SegmentStatus::Completed.as_str(), "completed");
        assert_eq!(SegmentStatus::Failed.as_str(), "failed");
        assert_eq!(SegmentStatus::Incomplete.as_str(), "incomplete");
        assert_eq!(SegmentStatus::Interrupted.as_str(), "interrupted");
    }
}