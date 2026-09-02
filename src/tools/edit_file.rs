//! `edit_file` 네이티브 툴 (DESIGN.md §4.4).
//!
//! 정확 문자열 치환. 유니코드 혼동 문자 경고. 다중 발견 시 오류(replace_all 아니면).

use crate::tools::util::{bool_arg, str_arg};
use crate::tools::{ToolHandler, ToolRegistry};

/// edit_file 도구 스키마.
pub fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "편집할 파일 경로"
            },
            "find": {
                "type": "string",
                "description": "정확히 치환할 문자열"
            },
            "replace": {
                "type": "string",
                "description": "교체할 문자열"
            },
            "replace_all": {
                "type": "boolean",
                "description": "모든 발견 지점 교체 (기본 false)"
            }
        },
        "required": ["path", "find", "replace"]
    })
}

/// 유니코드 혼동 문자 경고 (DESIGN.md §4.4).
/// 유니코드 정규화로 동일하게 보이는 대체 문자열이 있으면 경고한다.
fn unicode_warning(find: &str) -> Option<String> {
    // NFKC 정규화 후 다른 원본과 비교. 편의상 대표 혼동 문자들을 감지한다.
    let suspicious = [
        '‘', '’', '“', '”', '–', '—', '…', '·', '\u{00A0}', '\u{2028}', '\u{2029}',
    ];
    let has = find.chars().any(|c| suspicious.contains(&c));
    if has {
        Some(
            "경고: find 문자열에 유니코드 혼동 문자(스마트 따옴표·대시·줄바꿈 등)가 있습니다. 정확히 일치하지 않으면 유니코드 정규화 버전을 확인하세요.".to_string(),
        )
    } else {
        None
    }
}

/// edit_file 툴을 레지스트리에 등록한다.
pub fn register(reg: &mut ToolRegistry) {
    let handler: ToolHandler = std::sync::Arc::new(|args| {
        Box::pin(async move {
            let path_str = match str_arg(&args, "path") {
                Some(p) if !p.trim().is_empty() => p,
                _ => return Err("edit_file: path 인자가 필요합니다".to_string()),
            };
            let find = match str_arg(&args, "find") {
                Some(f) if !f.is_empty() => f,
                _ => return Err("edit_file: find 인자가 필요합니다".to_string()),
            };
            let replace = str_arg(&args, "replace").unwrap_or_default();
            let replace_all = bool_arg(&args, "replace_all", false);

            let path = std::path::Path::new(&path_str);
            if !path.exists() {
                return Err(format!("edit_file: 파일을 찾을 수 없습니다: {path_str}"));
            }
            let content = std::fs::read_to_string(path)
                .map_err(|e| format!("edit_file: 읽기 실패: {e}"))?;

            // 유니코드 혼동 문자 경고.
            let warning = unicode_warning(&find);

            let count = count_occurrences(&content, &find);

            // 다중 발견 시 오류 (replace_all 아니면).
            if count > 1 && !replace_all {
                let msg = format!(
                    "edit_file: '{find}'이(가) {count}곳에서 발견되었습니다. replace_all=true를 쓰거나 find를 더 구체적으로 지정하세요."
                );
                return Err(msg);
            }
            if count == 0 {
                let msg = format!(
                    "edit_file: '{find}'을(를) 찾을 수 없습니다.{}{}",
                    if warning.is_some() { "\n" } else { "" },
                    warning.unwrap_or_default()
                );
                return Err(msg);
            }

            let new_content = if replace_all {
                content.replace(&find, &replace)
            } else {
                content.replacen(&find, &replace, 1)
            };

            std::fs::write(path, &new_content)
                .map_err(|e| format!("edit_file: 쓰기 실패: {e}"))?;

            let mut msg = format!(
                "편집 완료: {path_str} — {count}곳 치환 (replace_all={replace_all})"
            );
            if let Some(w) = warning {
                msg.push('\n');
                msg.push_str(&w);
            }
            Ok(msg)
        })
    });
    reg.register(
        "edit_file",
        "정확 문자열 치환 편집 (다중 발견 시 오류, replace_all로 전체 치환, 유니코드 경고)",
        schema(),
        handler,
    );
}

/// 중복 문자열 개수 (겹침 없이).
fn count_occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    haystack.match_indices(needle).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatch(args: serde_json::Value) -> Result<String, String> {
        let mut reg = ToolRegistry::new(false);
        register(&mut reg);
        tokio::runtime::Runtime::new().unwrap().block_on(async move {
            reg.dispatch("edit_file", args).await
        })
    }

    #[test]
    fn exact_replace_works() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("e.txt");
        std::fs::write(&p, "hello world").unwrap();
        let res = dispatch(serde_json::json!({"path": p.to_string_lossy(), "find": "hello", "replace": "hi"}));
        assert!(res.is_ok());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hi world");
    }

    #[test]
    fn multiple_match_errors_without_replace_all() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("m.txt");
        std::fs::write(&p, "a b a c").unwrap();
        let res = dispatch(serde_json::json!({"path": p.to_string_lossy(), "find": "a", "replace": "x"}));
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("replace_all"));
    }

    #[test]
    fn replace_all_replaces_everywhere() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("r.txt");
        std::fs::write(&p, "a b a c").unwrap();
        let res = dispatch(serde_json::json!({"path": p.to_string_lossy(), "find": "a", "replace": "x", "replace_all": true}));
        assert!(res.is_ok());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "x b x c");
    }

    #[test]
    fn not_found_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("n.txt");
        std::fs::write(&p, "content").unwrap();
        let res = dispatch(serde_json::json!({"path": p.to_string_lossy(), "find": "zzz", "replace": "x"}));
        assert!(res.is_err());
    }

    #[test]
    fn unicode_warning_detects_smart_quotes() {
        assert!(unicode_warning("‘hello’").is_some());
        assert!(unicode_warning("plain").is_none());
    }

    #[test]
    fn missing_file_is_error() {
        let res = dispatch(serde_json::json!({"path": "/nonexistent", "find": "a", "replace": "b"}));
        assert!(res.is_err());
    }
}