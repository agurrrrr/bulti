//! 자동 컨텍스트 핸드오프 (DESIGN.md §4.6, 핵심).
//!
//! - 트리거 판정 `should_attempt_handoff` (§4.6.1): 추정 토큰 ≥ ctx × threshold_pct
//! - 9섹션 지시문 `build_handoff_prompt` (§4.6.2): 구조화 요약 + `===NEXT_TASK===`
//! - 파서 `parse_handoff_response` (§4.6.2): 요약·NEXT_TASK·섹션 추출
//! - 품질 게이트 `is_handoff_summary_acceptable` (§4.6.3)
//! - depth 가드 `HandoffDepthGuard` (§4.6.1): warn(8) / max(12) 런어웨이
//! - 체인 종료 판정 `chain_status` (§4.6.4)

use serde_json::json;

use crate::agent::context::estimate_messages_tokens;
use crate::llm::{ChatOptions, ChatRequest, LlmClient, Message};

/// 핸드오프 트리거 기본 임계값 (%). config 기본값과 동일하게 75.
pub const DEFAULT_HANDOFF_THRESHOLD_PCT: u8 = 75;
/// depth 경고 시작.
pub const DEFAULT_HANDOFF_WARN_DEPTH: u32 = 8;
/// depth 상한 (런어웨이 가드).
pub const DEFAULT_MAX_HANDOFF_DEPTH: u32 = 12;

/// 9섹션 키워드 (DESIGN.md §4.6.2).
pub const HANDOFF_SECTIONS: [&str; 9] = [
    "원 요청/의도",
    "핵심 기술/개념",
    "열람·변경 파일",
    "한 일",
    "실패·수정",
    "현재 진행",
    "남은 작업",
    "하지 말 것",
    "다음 한 걸음",
];

/// 품질 게이트 필수 키워드 (DESIGN.md §4.6.3): 5개 이상 필요.
pub const REQUIRED_KEYWORDS: [&str; 5] = ["원 요청", "열람", "한 일", "남은 작업", "하지 말"];

/// `===NEXT_TASK===` 마커.
pub const NEXT_TASK_MARKER: &str = "===NEXT_TASK===";

/// 품질 게이트 최소 길이 (룬 기준).
pub const MIN_SUMMARY_LEN: usize = 200;
/// degenerate 검사: 동일 라인 반복 상한.
pub const MAX_DUPLICATE_LINE: u32 = 8;
/// degenerate 검사: U+FFFD 비율 상한 (최소 20 룬).
pub const MAX_FFFD_RATIO: f64 = 0.2;

/// 핸드오프 판정 결과.
#[derive(Debug, Clone, PartialEq)]
pub enum HandoffDecision {
    /// 게이트 통과 + NEXT_TASK 있음 → 새 세그먼트 시작.
    Handoff,
    /// 게이트 통과 + NEXT_TASK 없음 → 체인 전체 완료.
    Complete,
    /// 게이트 실패 → trim 폴백으로 현재 세그먼트 계속.
    Fallback,
}

/// 핸드오프 응답 파싱 결과.
#[derive(Debug, Clone, Default)]
pub struct HandoffResponse {
    /// 9섹션 요약 전문.
    pub summary: String,
    /// `===NEXT_TASK===` 아래 과제 (비어 있으면 체인 완료).
    pub next_task: String,
    /// 감지된 섹션 번호 목록.
    pub sections: Vec<usize>,
}

/// depth 가드 (§4.6.1).
#[derive(Debug, Clone, Default)]
pub struct HandoffDepthGuard {
    pub depth: u32,
}

impl HandoffDepthGuard {
    pub fn new() -> Self {
        Self { depth: 0 }
    }

    /// depth 증가 (세그먼트당 +1). run 시작 시 0.
    pub fn increment(&mut self) {
        self.depth += 1;
    }

    /// warn(8) 이상이면 true — stderr 경고용.
    pub fn should_warn(&self) -> bool {
        self.depth >= DEFAULT_HANDOFF_WARN_DEPTH
    }

    /// max(12) 이상이면 true — 런어웨이 가드: 핸드오프 금지.
    pub fn runaway(&self) -> bool {
        self.depth >= DEFAULT_MAX_HANDOFF_DEPTH
    }
}

/// 트리거 판정 (§4.6.1).
///
/// `estimate(messages) ≥ context_tokens × threshold_pct/100` 이면 핸드오프 시도.
pub fn should_attempt_handoff(messages: &[Message], context_tokens: u64, threshold_pct: u8) -> bool {
    if context_tokens == 0 {
        return false;
    }
    let threshold = context_tokens as u128 * threshold_pct as u128 / 100;
    let estimated = estimate_messages_tokens(messages) as u128;
    estimated >= threshold
}

/// 핸드오프 지시문 프롬프트 조립 (§4.6.2).
///
/// 도구 없이 마지막 요청 — 9섹션 구조화 요약 + `===NEXT_TASK===` 지시.
/// 후속 세그먼트는 이전 대화를 볼 수 없으므로 파일 경로·결정사항·주의점 모두 포함을 명시한다.
pub fn build_handoff_prompt() -> String {
    let sections: Vec<String> = HANDOFF_SECTIONS
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{}. {}", i + 1, s))
        .collect();

    format!(
        r#"컨텍스트 한계에 도달했습니다. 도구를 사용하지 말고, 지금까지의 작업을 아래 9개 섹션으로 정리한 구조화 요약을 작성하라.

반드시 파일 경로·결정사항·주의점·현재 상태를 모두 포함하라. 후속 세그먼트는 이전 대화를 볼 수 없다.

{}

그 아래 `{marker}` 마커에 "후속 세그먼트가 바로 이어서 실행할 수 있는 완결형 작업 프롬프트"를 작성하라. 남은 작업이 없으면 `{marker}` 아래를 비워 두고 체인 완료를 선언하라.
"#,
        sections.join("\n"),
        marker = NEXT_TASK_MARKER
    )
}

/// 핸드오프 응답 파싱 (§4.6.2).
///
/// `===NEXT_TASK===` 마커 기준으로 요약과 과제를 분리하고, 9섹션 번호를 감지한다.
pub fn parse_handoff_response(content: &str) -> HandoffResponse {
    let mut resp = HandoffResponse::default();

    let marker_idx = content.rfind(NEXT_TASK_MARKER);
    match marker_idx {
        Some(idx) => {
            resp.summary = content[..idx].trim().to_string();
            resp.next_task = content[idx + NEXT_TASK_MARKER.len()..].trim().to_string();
        }
        None => {
            resp.summary = content.trim().to_string();
        }
    }

    // 섹션 번호 감지: "1. 원 요청/의도" 형태의 라인을 찾는다.
    for (i, section) in HANDOFF_SECTIONS.iter().enumerate() {
        let num = i + 1;
        let pattern = format!("{num}. {section}");
        if content.contains(&pattern) {
            resp.sections.push(i);
        }
    }

    resp
}

/// 품질 게이트 (§4.6.3).
///
/// `is_handoff_summary_acceptable(summary)`:
/// - 최소 길이 200자 이상 (룬 기준)
/// - 필수 섹션 키워드 5개 이상 존재 (`원 요청`, `열람`, `한 일`, `남은 작업`, `하지 말`)
/// - degenerate 검사: 동일 라인 반복, U+FFFD 다수
pub fn is_handoff_summary_acceptable(summary: &str) -> bool {
    // 1) 최소 길이 (룬 기준)
    let len: usize = summary.chars().count();
    if len < MIN_SUMMARY_LEN {
        return false;
    }

    // 2) 필수 섹션 키워드 5개 이상
    let mut keyword_hits = 0;
    for kw in REQUIRED_KEYWORDS {
        if summary.contains(kw) {
            keyword_hits += 1;
        }
    }
    if keyword_hits < 5 {
        return false;
    }

    // 3) degenerate 검사
    if degenerate_line_repeat(summary) {
        return false;
    }
    if degenerate_fffd(summary) {
        return false;
    }

    true
}

/// degenerate: 동일 라인이 8회 이상 반복되면 true.
fn degenerate_line_repeat(text: &str) -> bool {
    let mut counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        *counts.entry(trimmed.to_string()).or_insert(0) += 1;
        if counts[trimmed] >= MAX_DUPLICATE_LINE {
            return true;
        }
    }
    false
}

/// degenerate: U+FFFD 비율 ≥ 0.2 (최소 20 룬)면 true.
fn degenerate_fffd(text: &str) -> bool {
    let total: usize = text.chars().count();
    if total < 20 {
        return false;
    }
    let fffd: usize = text.chars().filter(|&c| c == '\u{FFFD}').count();
    fffd as f64 / total as f64 >= MAX_FFFD_RATIO
}

/// 체인 종료 판정 (§4.6.4).
///
/// run의 최종 상태는 마지막 세그먼트가 아니라 체인 전체로 판정한다.
/// 어느 세그먼트든 failed/incomplete로 끝나면 run은 그 상태를 물려받는다.
///
/// `segment_statuses`: 각 세그먼트의 상태 (completed | failed | incomplete).
/// 반환값: 체인 전체 상태.
pub fn chain_status(segment_statuses: &[&str]) -> &'static str {
    for s in segment_statuses {
        if *s == "failed" || *s == "incomplete" {
            return "incomplete";
        }
    }
    "completed"
}

/// 핸드오프 요약을 새 세그먼트 프롬프트로 조립 (§4.6.1).
///
/// 새 세그먼트의 프롬프트 = 핸드오프 요약 전문 + `===NEXT_TASK===` 아래의 과제.
pub fn build_new_segment_prompt(handoff: &HandoffResponse) -> String {
    format!(
        "{summary}\n\n{marker}\n{next_task}",
        summary = handoff.summary,
        marker = NEXT_TASK_MARKER,
        next_task = handoff.next_task
    )
}

/// 핸드오프 요청 메시지 조립 (도구 없이, 마지막 요청).
pub fn build_handoff_messages(system_prompt: &str, user_prompt: &str) -> Vec<Message> {
    let system = Message {
        role: "system".to_string(),
        content: Some(system_prompt.to_string()),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    };
    let user = Message {
        role: "user".to_string(),
        content: Some(user_prompt.to_string()),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    };
    vec![system, user]
}

/// 핸드오프 요청용 max_tokens (§4.6.1): `context_tokens / 4`.
pub fn handoff_max_tokens(context_tokens: u64) -> u64 {
    (context_tokens / 4).max(1)
}

/// JSON 보고서용 핸드오프 정보 직렬화.
pub fn handoff_report_json(decision: &HandoffDecision) -> serde_json::Value {
    json!({ "decision": format!("{decision:?}") })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn msg(role: &str, content: &str) -> Message {
        Message {
            role: role.to_string(),
            content: Some(content.to_string()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    /// 9섹션 모두 포함한 샘플 요약 (게이트 통과용).
    fn good_summary() -> String {
        format!(
            r#"1. 원 요청/의도: 자동 컨텍스트 핸드오프 구현.
2. 핵심 기술/개념: 토큰 추정, 9섹션 요약.
3. 열람·변경 파일: src/agent/handoff.rs.
4. 한 일: 트리거·파서·품질 게이트 구현.
5. 실패·수정: clippy 오류 수정.
6. 현재 진행: 단위 테스트 작성 중.
7. 남은 작업: e2e 테스트.
8. 하지 말 것: 세션 재사용 금지.
9. 다음 한 걸음: wiremock e2e 테스트.
"#
        )
    }

    // ── 트리거 (§4.6.1) ──

    #[test]
    fn trigger_when_estimate_reaches_threshold() {
        // ctx=100, threshold=75 → 75토큰 이상이면 트리거
        let messages = vec![
            msg("system", "s"),
            msg("user", &"x".repeat(300)), // 300 ASCII → 75토큰
        ];
        assert!(should_attempt_handoff(&messages, 100, 75));
    }

    #[test]
    fn no_trigger_below_threshold() {
        let messages = vec![
            msg("system", "s"),
            msg("user", &"x".repeat(200)), // 50토큰 < 75
        ];
        assert!(!should_attempt_handoff(&messages, 100, 75));
    }

    #[test]
    fn no_trigger_when_ctx_zero() {
        let messages = vec![msg("user", "hi")];
        assert!(!should_attempt_handoff(&messages, 0, 75));
    }

    #[test]
    fn threshold_boundary_exact() {
        // 정확히 75토큰이면 트리거 (≥)
        let messages = vec![msg("user", &"x".repeat(300))];
        assert!(should_attempt_handoff(&messages, 100, 75));
    }

    // ── 파서 (§4.6.2) ──

    #[test]
    fn parses_summary_and_next_task() {
        let content = format!(
            "9섹션 요약 내용\n\n{marker}\n다음 과제: 파일 수정",
            marker = NEXT_TASK_MARKER
        );
        let parsed = parse_handoff_response(&content);
        assert_eq!(parsed.summary, "9섹션 요약 내용");
        assert_eq!(parsed.next_task, "다음 과제: 파일 수정");
    }

    #[test]
    fn parses_no_next_task_as_complete() {
        let content = "9섹션 요약 내용\n\n작업 완료".to_string();
        let parsed = parse_handoff_response(&content);
        assert_eq!(parsed.summary, "9섹션 요약 내용\n\n작업 완료");
        assert!(parsed.next_task.is_empty());
    }

    #[test]
    fn detects_sections() {
        let parsed = parse_handoff_response(&good_summary());
        assert_eq!(parsed.sections.len(), 9);
    }

    #[test]
    fn detects_no_sections() {
        let parsed = parse_handoff_response("아무 내용 없음");
        assert!(parsed.sections.is_empty());
    }

    // ── 품질 게이트 (§4.6.3) ──

    #[test]
    fn gate_accepts_good_summary() {
        assert!(is_handoff_summary_acceptable(&good_summary()));
    }

    #[test]
    fn gate_rejects_short_summary() {
        let short = "원 요청 열람 한 일 남은 작업 하지 말 짧음";
        assert!(!is_handoff_summary_acceptable(short));
    }

    #[test]
    fn gate_rejects_missing_keywords() {
        // 200자 이상이지만 필수 키워드 5개 미만
        let mut s = "x".repeat(300);
        s.push_str("원 요청 열람 한 일"); // 4개만
        assert!(!is_handoff_summary_acceptable(&s));
    }

    #[test]
    fn gate_rejects_degenerate_line_repeat() {
        // 필수 키워드 포함 + 200자 이상이지만 동일 라인 8회 반복
        let mut s = good_summary();
        for _ in 0..8 {
            s.push_str("반복 라인\n");
        }
        assert!(!is_handoff_summary_acceptable(&s));
    }

    #[test]
    fn gate_rejects_degenerate_fffd() {
        let mut s = good_summary();
        // 20 룬 이상 + U+FFFD 비율 0.2 이상 (충분히 많이 추가)
        s.push_str(&"\u{FFFD}".repeat(300));
        assert!(!is_handoff_summary_acceptable(&s));
    }

    #[test]
    fn gate_accepts_fffd_below_ratio() {
        let mut s = good_summary();
        s.push_str("\u{FFFD}\u{FFFD}"); // 소수만 → 비율 낮음
        assert!(is_handoff_summary_acceptable(&s));
    }

    // ── depth 가드 (§4.6.1) ──

    #[test]
    fn depth_warn_at_8() {
        let g = HandoffDepthGuard { depth: 8 };
        assert!(g.should_warn());
        assert!(!g.runaway());
    }

    #[test]
    fn depth_runaway_at_12() {
        let g = HandoffDepthGuard { depth: 12 };
        assert!(g.should_warn());
        assert!(g.runaway());
    }

    #[test]
    fn depth_below_warn() {
        let g = HandoffDepthGuard { depth: 0 };
        assert!(!g.should_warn());
        assert!(!g.runaway());
    }

    #[test]
    fn depth_increment() {
        let mut g = HandoffDepthGuard::new();
        g.increment();
        assert_eq!(g.depth, 1);
    }

    // ── 체인 종료 판정 (§4.6.4) ──

    #[test]
    fn chain_all_completed() {
        assert_eq!(chain_status(&["completed", "completed"]), "completed");
    }

    #[test]
    fn chain_inherits_failure() {
        assert_eq!(chain_status(&["completed", "failed"]), "incomplete");
        assert_eq!(chain_status(&["incomplete", "completed"]), "incomplete");
    }

    // ── 기타 ──

    #[test]
    fn handoff_max_tokens_is_ctx_div_4() {
        assert_eq!(handoff_max_tokens(4096), 1024);
        assert_eq!(handoff_max_tokens(1), 1);
    }

    #[test]
    fn new_segment_prompt_contains_marker_and_task() {
        let h = HandoffResponse {
            summary: "요약".to_string(),
            next_task: "과제".to_string(),
            sections: vec![],
        };
        let p = build_new_segment_prompt(&h);
        assert!(p.contains(NEXT_TASK_MARKER));
        assert!(p.contains("과제"));
    }

    #[test]
    fn handoff_prompt_has_all_9_sections() {
        let p = build_handoff_prompt();
        for (i, s) in HANDOFF_SECTIONS.iter().enumerate() {
            assert!(p.contains(&format!("{}. {}", i + 1, s)));
        }
        assert!(p.contains(NEXT_TASK_MARKER));
    }

    // ── wiremock e2e (§4.6.1 흐름) ──

    fn test_endpoint(url: &str) -> crate::config::EndpointConfig {
        crate::config::EndpointConfig {
            url: url.to_string(),
            api_key: None,
            model: "m".to_string(),
            context_tokens: 4096,
            vision: false,
            thinking: true,
            max_iterations: 200,
        }
    }

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

    /// 핸드오프 e2e: 트리거 → handoff 요청 → 게이트 통과 → NEXT_TASK 파싱.
    #[tokio::test]
    async fn e2e_handoff_trigger_and_next_task() {
        let server = MockServer::start().await;

        // 1) 일반 세그먼트 요청 (도구 없이 완료 응답)
        let normal_body = sse_body(&[
            json!({
                "choices": [{"delta": {"content": "작업 진행"}, "finish_reason": null}],
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
                    .set_body_raw(normal_body, "text/event-stream"),
            )
            .mount(&server)
            .await;

        let client = LlmClient::new();
        let opts = ChatOptions {
            endpoint: test_endpoint(&server.uri()),
            temperature: None,
        };

        // ── 트리거 판정 ──
        // ctx=4096, threshold=75% → 3072토큰 이상이면 트리거. ASCII 4:1 압축이므로
        // 12288자 이상 필요. 13000 ASCII → 3250토큰(≥3072) → 트리거.
        let big_user = "x".repeat(13000);
        let messages = vec![
            msg("system", "sys"),
            msg("user", &big_user),
        ];
        assert!(should_attempt_handoff(&messages, 4096, 75));

        // ── handoff 요청 (도구 없이) ──
        let handoff_prompt = build_handoff_prompt();
        let handoff_messages = build_handoff_messages("시스템", &handoff_prompt);

        let req = ChatRequest {
            model: "m".to_string(),
            messages: handoff_messages,
            tools: vec![],
            stream: true,
            max_tokens: handoff_max_tokens(4096), // 1024
            temperature: None,
            frequency_penalty: 0.0,
            presence_penalty: 0.0,
        };
        let resp = client.chat(&opts, &req).await.unwrap();
        let content = resp.content.unwrap();

        // ── 파싱 ──
        let parsed = parse_handoff_response(&content);
        assert_eq!(parsed.summary, "작업 진행");

        // ── 게이트 ──
        // 응답이 짧으므로 게이트 실패 → Fallback (trim 폴백) 확인
        assert!(!is_handoff_summary_acceptable(&parsed.summary));
    }

    /// wiremock e2e: 게이트 통과 요약 + NEXT_TASK 있음 → 새 세그먼트 프롬프트 조립.
    #[tokio::test]
    async fn e2e_handoff_gate_pass_and_new_segment() {
        let server = MockServer::start().await;

        // 게이트 통과용 9섹션 응답
        let summary = format!(
            r#"1. 원 요청/의도: 자동 컨텍스트 핸드오프 구현.
2. 핵심 기술/개념: 토큰 추정, 9섹션 요약.
3. 열람·변경 파일: src/agent/handoff.rs.
4. 한 일: 트리거·파서·품질 게이트 구현.
5. 실패·수정: clippy 오류 수정.
6. 현재 진행: 단위 테스트 작성 중.
7. 남은 작업: e2e 테스트.
8. 하지 말 것: 세션 재사용 금지.
9. 다음 한 걸음: wiremock e2e 테스트.
{marker}
다음 과제: e2e 테스트 작성 완료 후 커밋
"#,
            marker = NEXT_TASK_MARKER
        );
        let body = sse_body(&[
            json!({
                "choices": [{"delta": {"content": &summary}, "finish_reason": null}],
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
        let handoff_prompt = build_handoff_prompt();
        let handoff_messages = build_handoff_messages("시스템", &handoff_prompt);
        let req = ChatRequest {
            model: "m".to_string(),
            messages: handoff_messages,
            tools: vec![],
            stream: true,
            max_tokens: handoff_max_tokens(4096),
            temperature: None,
            frequency_penalty: 0.0,
            presence_penalty: 0.0,
        };
        let resp = client.chat(&opts, &req).await.unwrap();
        let content = resp.content.unwrap();

        let parsed = parse_handoff_response(&content);
        // 게이트 통과
        assert!(is_handoff_summary_acceptable(&parsed.summary));
        // NEXT_TASK 있음 → 새 세그먼트
        assert!(!parsed.next_task.is_empty());
        let new_prompt = build_new_segment_prompt(&parsed);
        assert!(new_prompt.contains(NEXT_TASK_MARKER));
        assert!(new_prompt.contains("다음 과제"));
    }

    /// wiremock e2e: NEXT_TASK 없음 → 체인 완료 판정.
    #[tokio::test]
    async fn e2e_handoff_no_next_task_chain_complete() {
        let server = MockServer::start().await;

        let summary = format!(
            r#"1. 원 요청/의도: 자동 컨텍스트 핸드오프 구현.
2. 핵심 기술/개념: 토큰 추정, 9섹션 요약.
3. 열람·변경 파일: src/agent/handoff.rs.
4. 한 일: 트리거·파서·품질 게이트 구현.
5. 실패·수정: clippy 오류 수정.
6. 현재 진행: 단위 테스트 작성 중.
7. 남은 작업: 없음.
8. 하지 말 것: 세션 재사용 금지.
9. 다음 한 걸음: 없음.
작업 완료
"#
        );
        let body = sse_body(&[
            json!({
                "choices": [{"delta": {"content": &summary}, "finish_reason": null}],
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
        let handoff_prompt = build_handoff_prompt();
        let handoff_messages = build_handoff_messages("시스템", &handoff_prompt);
        let req = ChatRequest {
            model: "m".to_string(),
            messages: handoff_messages,
            tools: vec![],
            stream: true,
            max_tokens: handoff_max_tokens(4096),
            temperature: None,
            frequency_penalty: 0.0,
            presence_penalty: 0.0,
        };
        let resp = client.chat(&opts, &req).await.unwrap();
        let content = resp.content.unwrap();

        let parsed = parse_handoff_response(&content);
        // 게이트 통과 + NEXT_TASK 없음 → 체인 완료
        assert!(is_handoff_summary_acceptable(&parsed.summary));
        assert!(parsed.next_task.is_empty());
        assert_eq!(chain_status(&["completed", "completed"]), "completed");
    }
}