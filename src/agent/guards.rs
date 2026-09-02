//! 가드 체계 (DESIGN.md §5).
//!
//! 로컬 모델의 실패 양상에 대한 방어. 모든 가드는 양성(잡아야 할 것)·음성
//! (잡으면 안 되는 것) 케이스를 테이블 테스트로 박제한다 (shepherd #6294 교훈).

/// 가드 판정 결과.
#[derive(Debug, Clone, PartialEq)]
pub enum GuardOutcome {
    /// 정상 (가드 발동 안 함).
    Pass,
    /// 가드 발동 — 세그먼트 종료 사유.
    Trigger(String),
}

/// 가드 컨텍스트 (축적 상태).
#[derive(Debug, Clone, Default)]
pub struct GuardContext {
    /// 연속 빈 응답 턴 수.
    pub empty_turns: u32,
    /// 직전 tool-call 시그니처들 (최근 4개).
    pub recent_signatures: Vec<String>,
    /// future-intention nudge 횟수.
    pub future_nudges: u32,
    /// pause-summary nudge 횟수.
    pub pause_nudges: u32,
    /// 상태 변경 도구 호출 여부 (future-intention 리셋용).
    pub state_change_called: bool,
    /// 코드 수정 도구 호출 여부 (build gate용).
    pub code_modified: bool,
    /// bash 도구 호출 여부 (build gate용).
    pub bash_called: bool,
}

/// 빈 응답 루프 가드 (DESIGN.md §5, #5978).
///
/// content 빈 턴이 연속 6턴이면 incomplete. reasoning-only 턴이어도 카운터 리셋 없음.
pub fn check_empty_loop(ctx: &GuardContext) -> GuardOutcome {
    if ctx.empty_turns >= 6 {
        GuardOutcome::Trigger("incomplete: empty response loop".to_string())
    } else {
        GuardOutcome::Pass
    }
}

/// 스트림 반복 감지 가드 (DESIGN.md §5, #6008).
///
/// 마지막 ~4KB에서 동일 라인 8회 / 짧은 문구 8회 반복 시 즉시 중단.
pub fn check_stream_repetition(text: &str) -> GuardOutcome {
    let lines: Vec<&str> = text.lines().collect();
    let mut counts: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // 짧은 문구(≤ 20자)만 반복 감지 대상
        if trimmed.chars().count() <= 20 {
            *counts.entry(trimmed).or_insert(0) += 1;
        }
    }
    if counts.values().any(|&c| c >= 8) {
        GuardOutcome::Trigger("incomplete: stream repetition".to_string())
    } else {
        GuardOutcome::Pass
    }
}

/// tool-call 시그니처 (도구+인자 요약).
pub fn tool_signature(name: &str, args: &serde_json::Value) -> String {
    let args_str = args.to_string();
    let truncated: String = args_str.chars().take(80).collect();
    format!("{name}:{truncated}")
}

/// stuck tool signature 가드 (DESIGN.md §5, #6309).
///
/// 동일 (도구+인자) 시그니처 4턴 연속이면 incomplete "no progress".
/// read_file 진행도는 시그니처에 반영해 정상 페이징은 통과.
pub fn check_stuck_signature(ctx: &GuardContext) -> GuardOutcome {
    if ctx.recent_signatures.len() >= 4 {
        let last = &ctx.recent_signatures[ctx.recent_signatures.len() - 1];
        if ctx.recent_signatures.iter().all(|s| s == last) {
            return GuardOutcome::Trigger("incomplete: no progress".to_string());
        }
    }
    GuardOutcome::Pass
}

/// U+FFFD degenerate 가드 (DESIGN.md §5, #6145).
///
/// content의 U+FFFD 비율 ≥ 0.2 (최소 20 룬)면 즉시 incomplete "silent context overflow".
pub fn check_fffd_degenerate(content: &str) -> GuardOutcome {
    let total: u64 = content.chars().count() as u64;
    if total < 20 {
        return GuardOutcome::Pass;
    }
    let fffd: u64 = content.chars().filter(|&c| c == '\u{FFFD}').count() as u64;
    if fffd as f64 / total as f64 >= 0.2 {
        GuardOutcome::Trigger("incomplete: silent context overflow".to_string())
    } else {
        GuardOutcome::Pass
    }
}

/// future-intention nudge 가드 (DESIGN.md §5, #6290/#6294).
///
/// 도구 호출 0 + "~하겠습니다/let me ~" 문장 종결이면 완료 대신 nudge, 상한 2회.
/// 상태 변경 도구 호출 시 리셋.
pub fn check_future_intention(ctx: &GuardContext, tool_calls: usize, text: &str) -> GuardOutcome {
    if tool_calls == 0 && is_future_intention(text) {
        // 상태 변경 도구 호출 시 리셋
        let nudges = if ctx.state_change_called { 0 } else { ctx.future_nudges };
        if nudges >= 2 {
            return GuardOutcome::Trigger("incomplete: future intention nudge limit".to_string());
        }
        return GuardOutcome::Trigger("nudge: future intention".to_string());
    }
    GuardOutcome::Pass
}

fn is_future_intention(text: &str) -> bool {
    let t = text.trim().to_lowercase();
    // "~하겠습니다" 패턴
    if t.contains("하겠습니다") || t.contains("하겠어요") || t.contains("할게요") {
        return true;
    }
    // "let me ~" / "I will ~" 패턴
    if t.contains("let me") || t.contains("i will") || t.contains("i'll") {
        return true;
    }
    // 미래 의도 문장 종결 (마침표/줄바꿈으로 끝나고 행동 예고)
    if (t.ends_with('.') || t.ends_with('\n')) && (t.contains("will") || t.contains("going to")) {
        return true;
    }
    false
}

/// build gate 가드 (DESIGN.md §5, #6294).
///
/// 코드 수정했고 최종 메시지가 빌드 언급 + bash 미호출이면 incomplete
/// "build verification never run".
pub fn check_build_gate(ctx: &GuardContext, final_text: &str) -> GuardOutcome {
    if ctx.code_modified && !ctx.bash_called && mentions_build(final_text) {
        GuardOutcome::Trigger("incomplete: build verification never run".to_string())
    } else {
        GuardOutcome::Pass
    }
}

fn mentions_build(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("build") || t.contains("cargo") || t.contains("컴파일") || t.contains("빌드")
}

/// pause-summary 가드 (DESIGN.md §5, #6690).
///
/// "중단 시점/다음 세션/to be continued" 패턴이면 nudge 2회 → 핸드오프 라우팅.
/// 절대 조용히 완료 처리하지 않음.
pub fn check_pause_summary(ctx: &GuardContext, text: &str) -> GuardOutcome {
    if is_pause_summary(text) {
        if ctx.pause_nudges >= 2 {
            return GuardOutcome::Trigger("handoff: pause-summary".to_string());
        }
        return GuardOutcome::Trigger("nudge: pause-summary".to_string());
    }
    GuardOutcome::Pass
}

fn is_pause_summary(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("to be continued")
        || t.contains("다음 세션")
        || t.contains("중단 시점")
        || t.contains("이어서 하겠")
        || t.contains("continue later")
        || t.contains("다음에 이어서")
}

/// 가드 컨텍스트를 툴 콜 결과로 갱신한다.
pub fn update_after_tool_call(ctx: &mut GuardContext, sig: String, is_state_change: bool) {
    ctx.recent_signatures.push(sig);
    if ctx.recent_signatures.len() > 4 {
        ctx.recent_signatures.remove(0);
    }
    if is_state_change {
        ctx.state_change_called = true;
        ctx.future_nudges = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 빈 응답 루프 (#5978) ──

    #[test]
    fn empty_loop_positive_at_6_turns() {
        let mut ctx = GuardContext::default();
        ctx.empty_turns = 6;
        let r = check_empty_loop(&ctx);
        assert!(matches!(r, GuardOutcome::Trigger(_)));
    }

    #[test]
    fn empty_loop_negative_below_6_turns() {
        let mut ctx = GuardContext::default();
        ctx.empty_turns = 5;
        assert_eq!(check_empty_loop(&ctx), GuardOutcome::Pass);
    }

    // ── 스트림 반복 (#6008) ──

    #[test]
    fn stream_repetition_positive_8_same_short_line() {
        let text = "ok\nok\nok\nok\nok\nok\nok\nok\n";
        assert!(matches!(check_stream_repetition(text), GuardOutcome::Trigger(_)));
    }

    #[test]
    fn stream_repetition_negative_7_same_line() {
        let text = "ok\nok\nok\nok\nok\nok\nok\n";
        assert_eq!(check_stream_repetition(text), GuardOutcome::Pass);
    }

    #[test]
    fn stream_repetition_negative_long_lines() {
        // 20자 넘는 라인은 반복 감지 대상 아님 (짧은 문구만)
        let text = "this is a long line that should not trigger\nthis is a long line that should not trigger\nthis is a long line that should not trigger\nthis is a long line that should not trigger\nthis is a long line that should not trigger\nthis is a long line that should not trigger\nthis is a long line that should not trigger\nthis is a long line that should not trigger\n";
        assert_eq!(check_stream_repetition(text), GuardOutcome::Pass);
    }

    // ── stuck tool signature (#6309) ──

    #[test]
    fn stuck_signature_positive_4_same() {
        let mut ctx = GuardContext::default();
        for _ in 0..4 {
            ctx.recent_signatures.push("bash:{command:ls}".to_string());
        }
        assert!(matches!(check_stuck_signature(&ctx), GuardOutcome::Trigger(_)));
    }

    #[test]
    fn stuck_signature_negative_progress() {
        // read_file 페이징: offset이 달라 시그니처가 다르므로 정상 통과
        let mut ctx = GuardContext::default();
        ctx.recent_signatures.push("read_file:{path:a,offset:0}".to_string());
        ctx.recent_signatures.push("read_file:{path:a,offset:200}".to_string());
        ctx.recent_signatures.push("read_file:{path:a,offset:400}".to_string());
        ctx.recent_signatures.push("read_file:{path:a,offset:600}".to_string());
        assert_eq!(check_stuck_signature(&ctx), GuardOutcome::Pass);
    }

    #[test]
    fn stuck_signature_negative_3_same() {
        let mut ctx = GuardContext::default();
        for _ in 0..3 {
            ctx.recent_signatures.push("bash:{command:ls}".to_string());
        }
        assert_eq!(check_stuck_signature(&ctx), GuardOutcome::Pass);
    }

    // ── U+FFFD degenerate (#6145) ──

    #[test]
    fn fffd_positive_high_ratio() {
        // 20 룬 이상, FFFD 비율 ≥ 0.2
        let content = "\u{FFFD}".repeat(20); // 20개 전부 FFFD → 비율 1.0
        assert!(matches!(check_fffd_degenerate(&content), GuardOutcome::Trigger(_)));
    }

    #[test]
    fn fffd_negative_low_ratio() {
        let mut content = "normal text ".repeat(10); // 120 룬
        content.push_str("\u{FFFD}\u{FFFD}\u{FFFD}"); // 3개 → 비율 < 0.2
        assert_eq!(check_fffd_degenerate(&content), GuardOutcome::Pass);
    }

    #[test]
    fn fffd_negative_below_min_20() {
        // 최소 20 룬 미만은 검사 안 함
        let content = "\u{FFFD}\u{FFFD}"; // 2 룬
        assert_eq!(check_fffd_degenerate(&content), GuardOutcome::Pass);
    }

    // ── future-intention nudge (#6290/#6294) ──

    #[test]
    fn future_intention_positive_korean() {
        let mut ctx = GuardContext::default();
        let r = check_future_intention(&ctx, 0, "코드를 수정하겠습니다.");
        assert!(matches!(r, GuardOutcome::Trigger(_)));
    }

    #[test]
    fn future_intention_positive_english() {
        let mut ctx = GuardContext::default();
        let r = check_future_intention(&ctx, 0, "Let me fix the bug.");
        assert!(matches!(r, GuardOutcome::Trigger(_)));
    }

    #[test]
    fn future_intention_negative_with_tool_call() {
        let mut ctx = GuardContext::default();
        // 도구 호출이 있으면 nudge 아님
        assert_eq!(check_future_intention(&ctx, 1, "코드를 수정하겠습니다."), GuardOutcome::Pass);
    }

    #[test]
    fn future_intention_negative_no_intent() {
        let mut ctx = GuardContext::default();
        assert_eq!(check_future_intention(&ctx, 0, "완료되었습니다."), GuardOutcome::Pass);
    }

    #[test]
    fn future_intention_positive_limit_2() {
        let mut ctx = GuardContext::default();
        ctx.future_nudges = 2;
        let r = check_future_intention(&ctx, 0, "수정하겠습니다.");
        assert!(matches!(r, GuardOutcome::Trigger(_)));
    }

    // ── build gate (#6294) ──

    #[test]
    fn build_gate_positive() {
        let mut ctx = GuardContext::default();
        ctx.code_modified = true;
        ctx.bash_called = false;
        let r = check_build_gate(&ctx, "빌드가 통과했습니다.");
        assert!(matches!(r, GuardOutcome::Trigger(_)));
    }

    #[test]
    fn build_gate_negative_bash_called() {
        let mut ctx = GuardContext::default();
        ctx.code_modified = true;
        ctx.bash_called = true; // bash 호출했으므로 build gate 통과
        assert_eq!(check_build_gate(&ctx, "빌드가 통과했습니다."), GuardOutcome::Pass);
    }

    #[test]
    fn build_gate_negative_no_code_modified() {
        let mut ctx = GuardContext::default();
        ctx.code_modified = false;
        assert_eq!(check_build_gate(&ctx, "빌드가 통과했습니다."), GuardOutcome::Pass);
    }

    // ── pause-summary (#6690) ──

    #[test]
    fn pause_summary_positive() {
        let mut ctx = GuardContext::default();
        let r = check_pause_summary(&ctx, "이 작업은 다음 세션에서 이어서 하겠습니다.");
        assert!(matches!(r, GuardOutcome::Trigger(_)));
    }

    #[test]
    fn pause_summary_positive_english() {
        let mut ctx = GuardContext::default();
        let r = check_pause_summary(&ctx, "To be continued...");
        assert!(matches!(r, GuardOutcome::Trigger(_)));
    }

    #[test]
    fn pause_summary_negative_normal() {
        let mut ctx = GuardContext::default();
        assert_eq!(check_pause_summary(&ctx, "모든 작업이 완료되었습니다."), GuardOutcome::Pass);
    }

    #[test]
    fn pause_summary_positive_handoff_after_2() {
        let mut ctx = GuardContext::default();
        ctx.pause_nudges = 2;
        let r = check_pause_summary(&ctx, "중단 시점에서 이어서 하겠습니다.");
        assert!(matches!(r, GuardOutcome::Trigger(_)));
        // Trigger 사유가 handoff인지 확인
        if let GuardOutcome::Trigger(reason) = r {
            assert!(reason.contains("handoff"));
        }
    }

    // ── update_after_tool_call ──

    #[test]
    fn update_after_tool_call_tracks_signatures() {
        let mut ctx = GuardContext::default();
        update_after_tool_call(&mut ctx, "bash:ls".to_string(), false);
        update_after_tool_call(&mut ctx, "bash:ls".to_string(), false);
        update_after_tool_call(&mut ctx, "bash:ls".to_string(), false);
        update_after_tool_call(&mut ctx, "bash:ls".to_string(), false);
        assert_eq!(ctx.recent_signatures.len(), 4);
        assert!(matches!(check_stuck_signature(&ctx), GuardOutcome::Trigger(_)));
    }

    #[test]
    fn update_after_tool_call_resets_future_on_state_change() {
        let mut ctx = GuardContext::default();
        ctx.future_nudges = 2;
        update_after_tool_call(&mut ctx, "write_file:x".to_string(), true);
        assert_eq!(ctx.future_nudges, 0);
        assert!(ctx.state_change_called);
    }

    #[test]
    fn tool_signature_truncates_long_args() {
        let args = serde_json::json!({"command": "a".repeat(200)});
        let sig = tool_signature("bash", &args);
        assert!(sig.starts_with("bash:"));
        assert!(sig.chars().count() <= 85);
    }
}