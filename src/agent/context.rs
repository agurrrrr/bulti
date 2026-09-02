//! 컨텍스트 관리 (DESIGN.md §4.5).
//!
//! - 토큰 추정 `estimate_tokens` (§4.5.1): ASCII 룬 4:1, 비ASCII(한글·CJK) 1:1, 이미지 base64 `len/4`.
//! - trim 폴백 `trim_messages` (§4.5.2): 시스템 프롬프트·최초 사용자 프롬프트 보존, 오래된 턴부터 제거.

use crate::llm::Message;

/// 토큰 추정 (DESIGN.md §4.5.1).
///
/// 룬 단위 계산. ASCII 룬은 4글자당 1토큰, 비ASCII 룬(한글·CJK)은 글자당 1토큰.
/// 이미지 base64는 `len/4`.
///
/// 바이트 수를 4로 나누는 순진한 휴리스틱은 한글에서 실제 토큰의 절반 이하로
/// 추정되어 트리밍이 늦어지고, llama.cpp의 조용한 context shift로 이어져 퇴행
/// 출력을 만든다 (shepherd #5978/#5981 사고).
pub fn estimate_tokens(text: &str) -> u64 {
    let mut tokens: u64 = 0;
    for c in text.chars() {
        if c == '\n' {
            continue;
        }
        if c.is_ascii() {
            tokens += 1;
        } else {
            // 비ASCII 룬(한글·CJK 등): 글자당 1토큰
            tokens += 4;
        }
    }
    // ASCII 룬 4:1 압축
    tokens / 4
}

/// 이미지 base64 토큰 추정 (DESIGN.md §4.5.1): `len/4`.
pub fn estimate_image_tokens(base64_len: u64) -> u64 {
    base64_len / 4
}

/// 메시지 목록의 총 토큰 추정치.
pub fn estimate_messages_tokens(messages: &[Message]) -> u64 {
    let mut total: u64 = 0;
    for m in messages {
        if let Some(content) = &m.content {
            total += estimate_tokens(content);
        }
        // 이미지 content (OpenAI vision format) 처리
        if let Some(content) = &m.content {
            if content.contains("data:image/") && content.contains("base64,") {
                // base64 본문 길이 추정
                if let Some(idx) = content.find("base64,") {
                    let body_len = content.len() - idx - "base64,".len();
                    total += estimate_image_tokens(body_len as u64);
                }
            }
        }
        if let Some(args) = &m.tool_calls {
            for tc in args {
                if let Some(func) = &tc.function {
                    if let Some(name) = &func.name {
                        total += estimate_tokens(name);
                    }
                }
            }
        }
        if let Some(name) = &m.name {
            total += estimate_tokens(name);
        }
    }
    total
}

/// trim 폴백 (DESIGN.md §4.5.2).
///
/// 핸드오프가 불가능할 때만 사용한다. 시스템 프롬프트와 최초 사용자 프롬프트는
/// 보존하고, 가장 오래된 턴(assistant + 딸린 tool 결과)부터 통째로 제거한다.
///
/// `keep_turns`는 보존할 최신 턴 수. 반환값은 (trimmed_messages, 제거된 턴 수).
pub fn trim_messages(messages: Vec<Message>, keep_turns: usize) -> (Vec<Message>, usize) {
    // 시스템 프롬프트(role == "system")와 최초 사용자 프롬프트는 보존 대상.
    let mut head: Vec<Message> = Vec::new();

    // 1) 시스템 프롬프트 보존
    let mut i = 0;
    while i < messages.len() && messages[i].role == "system" {
        head.push(messages[i].clone());
        i += 1;
    }

    // 2) 최초 사용자 프롬프트 보존
    let mut first_user_idx: Option<usize> = None;
    for (idx, m) in messages.iter().enumerate().skip(i) {
        if m.role == "user" {
            first_user_idx = Some(idx);
            break;
        }
    }

    // 최초 사용자 턴만 보존 (그 뒤의 assistant/tool은 제거 대상).
    let mut preserve_end = i;
    if let Some(fidx) = first_user_idx {
        preserve_end = fidx + 1;
    }

    for m in messages.iter().take(preserve_end).skip(i) {
        head.push(m.clone());
    }

    // 3) 나머지(가장 오래된 턴부터)에서 keep_turns 만큼 최신 턴을 보존.
    let rest = &messages[preserve_end..];
    let removed = rest.len().saturating_sub(keep_turns);
    let tail: Vec<Message> = rest[removed.min(rest.len())..].to_vec();

    let mut result = head;
    result.extend(tail);
    (result, removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> Message {
        Message {
            role: role.to_string(),
            content: Some(content.to_string()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    // ── 토큰 추정 (§4.5.1) ──

    #[test]
    fn estimate_ascii_is_len_div_4() {
        let s = "hello world this is a test"; // 25 chars
        assert_eq!(estimate_tokens(s), 25 / 4);
    }

    #[test]
    fn estimate_korean_counts_each_char() {
        // 한글은 글자당 4토큰, ASCII 룬 4:1 압축 후 나눔.
        // "안녕하세요" = 5 비ASCII 룬 → 5*4 = 20, 20/4 = 5토큰
        let s = "안녕하세요";
        assert_eq!(estimate_tokens(s), 5);
    }

    #[test]
    fn estimate_korean_heavier_than_naive_byte_div4() {
        // 한글 10글자: 룬 기반 추정이 바이트/4 추정보다 크다 (shepherd #5978).
        let korean = "한글한글한글한글한글"; // 10 비ASCII 룬
        let est = estimate_tokens(korean);
        let naive = korean.len() as u64 / 4; // UTF-8 30바이트/4 = 7
        assert!(est > naive);
    }

    #[test]
    fn estimate_mixed_text() {
        let s = "hello 안녕 world";
        // hello(5 ASCII) + 안녕(2 비ASCII) + world(5 ASCII) = 12 ASCII + 2 비ASCII
        // = 12/4 + 2*4/4 = 3 + 2 = 5
        assert_eq!(estimate_tokens(s), 5);
    }

    #[test]
    fn estimate_image_base64() {
        // base64 100자 → 25토큰
        assert_eq!(estimate_image_tokens(100), 25);
    }

    // ── trim (§4.5.2) ──

    #[test]
    fn trim_preserves_system_and_first_user() {
        let messages = vec![
            msg("system", "sys"),
            msg("user", "first"),
            msg("assistant", "a1"),
            msg("tool", "t1"),
            msg("assistant", "a2"),
            msg("tool", "t2"),
        ];
        let (out, removed) = trim_messages(messages, 2);
        // 시스템과 최초 사용자 턴 + 딸린 tool 결과 보존
        assert_eq!(out[0].role, "system");
        assert_eq!(out[1].content.as_deref(), Some("first"));
        // 가장 오래된 턴부터 제거
        assert_eq!(removed, 2);
        // 마지막 2턴 보존
        assert_eq!(out[out.len() - 1].content.as_deref(), Some("t2"));
    }

    #[test]
    fn trim_removes_oldest_turns() {
        let mut messages = vec![msg("system", "sys"), msg("user", "first")];
        for i in 0..10 {
            messages.push(msg("assistant", &format!("a{i}")));
            messages.push(msg("tool", &format!("t{i}")));
        }
        let (out, removed) = trim_messages(messages, 4);
        // 시스템 + 최초 사용자 + 최신 4턴
        assert_eq!(removed, 16);
        assert_eq!(out[out.len() - 1].content.as_deref(), Some("t9"));
        assert!(!out.iter().any(|m| m.content.as_deref() == Some("a0")));
    }

    #[test]
    fn trim_no_removal_when_within_keep() {
        let messages = vec![
            msg("system", "sys"),
            msg("user", "first"),
            msg("assistant", "a1"),
            msg("tool", "t1"),
        ];
        let (out, removed) = trim_messages(messages, 10);
        assert_eq!(removed, 0);
        assert_eq!(out.len(), 4);
    }
}