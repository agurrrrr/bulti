//! 네이티브 툴 공통 유틸리티 (인자 파싱, 결과 절단).

use serde_json::Value;

/// 인자에서 문자열 필드를 추출한다. 없거나 타입이 다르면 None.
pub fn str_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

/// 인자에서 정수 필드를 추출한다. 없으면 기본값.
pub fn int_arg(args: &Value, key: &str, default: u64) -> u64 {
    args.get(key)
        .and_then(|v| v.as_u64())
        .unwrap_or(default)
}

/// 인자에서 불리언 필드를 추출한다. 없으면 기본값.
pub fn bool_arg(args: &Value, key: &str, default: bool) -> bool {
    args.get(key)
        .and_then(|v| v.as_bool())
        .unwrap_or(default)
}

/// 툴 결과 공통 절단 (DESIGN.md §4.5.2).
///
/// 히스토리 저장 직전 8,000자 상한. 잘릴 때 도구별 행동 가능 힌트를 붙여
/// 다른 tool-call signature를 유도한다 (shepherd #6309 데드락 방지).
pub fn truncate_tool_result(s: &str, tool: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_chars).collect();
    let hint = match tool {
        "bash" => "파일로 redirect 후 read_file 페이징 또는 head/grep으로 좁히기",
        "grep" => "패턴·glob 좁히기",
        "read_file" => "offset·limit으로 페이징 좁히기",
        "glob" => "패턴을 더 구체적으로 좁히기",
        _ => "결과를 좁혀서 재시도",
    };
    let n = s.chars().count() - max_chars;
    format!("{truncated}\n...truncated {n} chars — 힌트: {hint}")
}

/// 인자 요약 (Live 출력용). 긴 값을 잘라 표시한다.
pub fn summarize_args(args: &Value, max_len: usize) -> String {
    match args {
        Value::Object(map) => {
            let mut parts = Vec::new();
            for (k, v) in map {
                let val = match v {
                    Value::Null => "null".to_string(),
                    Value::Bool(b) => b.to_string(),
                    Value::Number(n) => n.to_string(),
                    Value::String(s) => {
                        if s.chars().count() > max_len {
                            let head: String = s.chars().take(max_len).collect();
                            format!("{head}…")
                        } else {
                            s.clone()
                        }
                    }
                    other => other.to_string(),
                };
                parts.push(format!("{k}={val}"));
            }
            parts.join(" ")
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short() {
        let s = "short";
        assert_eq!(truncate_tool_result(s, "bash", 100), s);
    }

    #[test]
    fn truncate_appends_hint() {
        let s = "x".repeat(100);
        let out = truncate_tool_result(&s, "bash", 50);
        assert!(out.contains("...truncated"));
        assert!(out.contains("파일로 redirect"));
    }

    #[test]
    fn truncate_hint_differs_by_tool() {
        let s = "x".repeat(100);
        let grep_out = truncate_tool_result(&s, "grep", 50);
        assert!(grep_out.contains("패턴·glob 좁히기"));
    }

    #[test]
    fn summarize_args_shortens_long_strings() {
        let args = serde_json::json!({"command": "a".repeat(200)});
        let out = summarize_args(&args, 10);
        assert!(out.contains("command=a"));
        assert!(out.contains("…"));
    }
}