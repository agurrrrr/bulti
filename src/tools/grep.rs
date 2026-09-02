//! `grep` 네이티브 툴 (DESIGN.md §4.4).
//!
//! 자체 구현(walkdir+regex). 결과 상한 + 힌트.

use regex::Regex;
use walkdir::WalkDir;

use crate::tools::util::str_arg;
use crate::tools::{ToolHandler, ToolRegistry};

/// 결과 개수 상한.
const MAX_RESULTS: usize = 200;

/// grep 도구 스키마.
pub fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "검색할 정규식 패턴"
            },
            "glob": {
                "type": "string",
                "description": "파일 필터 glob (선택, 예: **/*.rs)"
            },
            "path": {
                "type": "string",
                "description": "검색 시작 경로 (선택, 기본 프로젝트 루트)"
            }
        },
        "required": ["pattern"]
    })
}

/// grep 툴을 레지스트리에 등록한다.
pub fn register(reg: &mut ToolRegistry) {
    let handler: ToolHandler = std::sync::Arc::new(|args| {
        Box::pin(async move {
            let pattern = match str_arg(&args, "pattern") {
                Some(p) if !p.trim().is_empty() => p,
                _ => return Err("grep: pattern 인자가 필요합니다".to_string()),
            };
            let glob_filter = str_arg(&args, "glob");
            let path_arg = str_arg(&args, "path");

            let re = Regex::new(&pattern)
                .map_err(|e| format!("grep: 정규식 파싱 실패: {e}"))?;

            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            let start = match &path_arg {
                Some(p) => std::path::Path::new(p).to_path_buf(),
                None => cwd.clone(),
            };
            if !start.exists() {
                return Err(format!("grep: 경로를 찾을 수 없습니다: {path_arg:?}"));
            }

            // 자체 구현(walkdir+regex). .git 등 숨김 디렉터리 제외.
            let mut results: Vec<String> = Vec::new();

            let walker = WalkDir::new(&start)
                .into_iter()
                .filter_entry(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    !(name.starts_with('.') && e.depth() > 0)
                });

            for entry in walker {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if entry.file_type().is_dir() {
                    continue;
                }
                // glob 필터 적용. 패턴을 검색 시작 경로 기준으로 매칭.
                if let Some(g) = &glob_filter {
                    // 절대경로면 그대로, 상대경로면 start 기준으로 결합.
                    let pattern = if g.starts_with('/') {
                        g.clone()
                    } else {
                        start.join(g).to_string_lossy().to_string()
                    };
                    let matched = glob::glob(&pattern)
                        .map(|mut gb| gb.any(|p| p.map(|p| p == entry.path()).unwrap_or(false)))
                        .unwrap_or(false);
                    if !matched {
                        continue;
                    }
                }
                let content = match std::fs::read_to_string(entry.path()) {
                    Ok(c) => c,
                    Err(_) => continue, // 바이너리 등 읽기 불가 파일은 건너뜀.
                };
                for (i, line) in content.lines().enumerate() {
                    if re.is_match(line) {
                        let rel = entry.path().strip_prefix(&cwd).unwrap_or(entry.path());
                        results.push(format!("{}:{}:{}", rel.to_string_lossy(), i + 1, line));
                        if results.len() >= MAX_RESULTS {
                            break;
                        }
                    }
                }
                if results.len() >= MAX_RESULTS {
                    break;
                }
            }

            if results.is_empty() {
                return Ok("(일치하는 결과가 없습니다)".to_string());
            }

            let mut out = results.join("\n");
            if results.len() >= MAX_RESULTS {
                out.push_str(&format!(
                    "\n...[결과 상한 {MAX_RESULTS}개 도달] 힌트: 패턴·glob을 더 좁히세요"
                ));
            }
            Ok(out)
        })
    });
    reg.register(
        "grep",
        "정규식 검색 (자체 구현 walkdir+regex, .git 무시, 결과 상한 + 힌트)",
        schema(),
        handler,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatch(args: serde_json::Value) -> Result<String, String> {
        let mut reg = ToolRegistry::new(false);
        register(&mut reg);
        tokio::runtime::Runtime::new().unwrap().block_on(async move {
            reg.dispatch("grep", args).await
        })
    }

    #[test]
    fn finds_line_with_file_and_number() {
        // 프로젝트 내 특정 패턴 검색.
        let out = dispatch(serde_json::json!({"pattern": "fn main", "glob": "src/main.rs"})).unwrap();
        assert!(out.contains("src/main.rs"));
        assert!(out.contains("fn main"));
    }

    #[test]
    fn no_match_returns_message() {
        // 프로젝트 루트 대신 임시 디렉터리 기반으로 검색해 자기 자신(테스트 코드) 매칭을 피한다.
        let dir = tempfile::tempdir().unwrap();
        // 임시 디렉터리에 아무 파일도 없으므로 항상 매칭 없음.
        let out = dispatch(serde_json::json!({"pattern": "zzz_nonexistent_zzz", "path": dir.path().to_string_lossy()})).unwrap();
        assert!(out.contains("일치하는 결과가 없습니다"));
    }

    #[test]
    fn invalid_regex_is_error() {
        let res = dispatch(serde_json::json!({"pattern": "["}));
        assert!(res.is_err());
    }

    #[test]
    fn missing_pattern_is_error() {
        let res = dispatch(serde_json::json!({}));
        assert!(res.is_err());
    }

    #[test]
    fn glob_filter_restricts_results() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn foo() {}\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "fn foo() {}\n").unwrap();
        let mut reg = ToolRegistry::new(false);
        register(&mut reg);
        let res = tokio::runtime::Runtime::new().unwrap().block_on(async move {
            reg.dispatch(
                "grep",
                serde_json::json!({
                    "pattern": "fn foo",
                    "path": dir.path().to_string_lossy(),
                    "glob": "*.rs"
                }),
            )
            .await
        });
        let out = res.unwrap_or_else(|e| e);
        println!("GREP FILTER OUTPUT:\n{out}");
        assert!(out.contains("a.rs"));
        assert!(!out.contains("b.txt"));
    }

    #[test]
    fn ignores_hidden_dirs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/config"), "secret\n").unwrap();
        std::fs::write(dir.path().join("real.txt"), "secret\n").unwrap();
        let mut reg = ToolRegistry::new(false);
        register(&mut reg);
        let res = tokio::runtime::Runtime::new().unwrap().block_on(async move {
            reg.dispatch(
                "grep",
                serde_json::json!({"pattern": "secret", "path": dir.path().to_string_lossy()}),
            )
            .await
        });
        let out = res.unwrap();
        assert!(!out.contains(".git"));
        assert!(out.contains("real.txt"));
    }
}