//! `glob` 네이티브 툴 (DESIGN.md §4.4).
//!
//! `.git` 무시. 결과 개수 상한 + 패턴 좁히기 힌트.

use crate::tools::util::str_arg;
use crate::tools::{ToolHandler, ToolRegistry};

/// 결과 개수 상한.
const MAX_RESULTS: usize = 200;

/// glob 도구 스키마.
pub fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "매칭할 glob 패턴 (예: **/*.rs)"
            }
        },
        "required": ["pattern"]
    })
}

/// glob 툴을 레지스트리에 등록한다.
pub fn register(reg: &mut ToolRegistry) {
    let handler: ToolHandler = std::sync::Arc::new(|args| {
        Box::pin(async move {
            let pattern = match str_arg(&args, "pattern") {
                Some(p) if !p.trim().is_empty() => p,
                _ => return Err("glob: pattern 인자가 필요합니다".to_string()),
            };

            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            let mut matches: Vec<String> = Vec::new();

            // 프로젝트 루트 기준 재귀 매칭. .git 등 숨김 디렉터리 제외.
            let full_pattern = cwd.join(&pattern);
            let globber = glob::glob(&full_pattern.to_string_lossy())
                .map_err(|e| format!("glob: 패턴 파싱 실패: {e}"))?;

            for entry in globber {
                let entry = entry.map_err(|e| format!("glob: 매칭 오류: {e}"))?;
                // .git 디렉터리 제외 (그 외 숨김 경로는 허용).
                let mut skip = false;
                for c in entry.components() {
                    let s = c.as_os_str().to_string_lossy().to_string();
                    if s == ".git" {
                        skip = true;
                        break;
                    }
                }
                if skip {
                    continue;
                }
                let rel = entry
                    .strip_prefix(&cwd)
                    .unwrap_or(&entry)
                    .to_string_lossy()
                    .to_string();
                matches.push(rel);
                if matches.len() >= MAX_RESULTS {
                    break;
                }
            }

            matches.sort();

            if matches.is_empty() {
                return Ok("(매칭되는 파일이 없습니다)".to_string());
            }

            let mut out = matches.join("\n");
            if matches.len() >= MAX_RESULTS {
                out.push_str(&format!(
                    "\n...[결과 상한 {MAX_RESULTS}개 도달] 힌트: 패턴을 더 구체적으로 좁히세요 (예: src/**/*.rs)"
                ));
            }
            Ok(out)
        })
    });
    reg.register(
        "glob",
        "파일 패턴 검색 (.git 무시, 결과 상한 + 좁히기 힌트)",
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
            reg.dispatch("glob", args).await
        })
    }

    #[test]
    fn glob_finds_rust_files() {
        let out = dispatch(serde_json::json!({"pattern": "src/**/*.rs"})).unwrap();
        assert!(out.contains("src/main.rs"));
        assert!(out.contains("src/tools/mod.rs"));
    }

    #[test]
    fn no_match_returns_message() {
        let out = dispatch(serde_json::json!({"pattern": "**/*.zzz_nope"})).unwrap();
        assert!(out.contains("매칭되는 파일이 없습니다"));
    }

    #[test]
    fn missing_pattern_is_error() {
        let res = dispatch(serde_json::json!({}));
        assert!(res.is_err());
    }

    #[test]
    fn ignores_git_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/config"), "x").unwrap();
        std::fs::write(dir.path().join("real.txt"), "x").unwrap();
        // cwd 변경 없이 상대 패턴은 프로젝트 루트 기준이므로, 절대 경로로 검증.
        let mut reg = ToolRegistry::new(false);
        register(&mut reg);
        let res = tokio::runtime::Runtime::new().unwrap().block_on(async move {
            reg.dispatch("glob", serde_json::json!({"pattern": dir.path().join("**/*").to_string_lossy()})).await
        });
        let out = res.unwrap();
        println!("GLOB GIT OUTPUT:\n{out}");
        assert!(!out.contains(".git"));
        assert!(out.contains("real.txt"));
    }
}