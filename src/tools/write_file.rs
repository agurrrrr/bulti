//! `write_file` 네이티브 툴 (DESIGN.md §4.4).
//!
//! 부모 디렉터리 자동 생성. 빈 content도 명시적 생성으로 취급.

use crate::tools::util::str_arg;
use crate::tools::{ToolHandler, ToolRegistry};

/// write_file 도구 스키마.
pub fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "쓸 파일 경로 (프로젝트 루트 기준 상대경로)"
            },
            "content": {
                "type": "string",
                "description": "파일 내용 (빈 문자열도 명시적 생성으로 취급)"
            }
        },
        "required": ["path", "content"]
    })
}

/// write_file 툴을 레지스트리에 등록한다.
pub fn register(reg: &mut ToolRegistry) {
    let handler: ToolHandler = std::sync::Arc::new(|args| {
        Box::pin(async move {
            let path_str = match str_arg(&args, "path") {
                Some(p) if !p.trim().is_empty() => p,
                _ => return Err("write_file: path 인자가 필요합니다".to_string()),
            };
            let content = str_arg(&args, "content").unwrap_or_default();

            let path = std::path::Path::new(&path_str);
            // 부모 디렉터리 자동 생성.
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("write_file: 디렉터리 생성 실패: {e}"))?;
                }
            }
            std::fs::write(path, &content)
                .map_err(|e| format!("write_file: 쓰기 실패: {e}"))?;
            Ok(format!("파일을 작성했습니다: {path_str} ({}자)", content.chars().count()))
        })
    });
    reg.register(
        "write_file",
        "파일 작성 (부모 디렉터리 자동 생성, 빈 content도 생성 처리)",
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
            reg.dispatch("write_file", args).await
        })
    }

    #[test]
    fn writes_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("out.txt");
        let res = dispatch(serde_json::json!({"path": p.to_string_lossy(), "content": "hello"}));
        assert!(res.is_ok());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hello");
    }

    #[test]
    fn creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a/b/c/deep.txt");
        let res = dispatch(serde_json::json!({"path": p.to_string_lossy(), "content": "x"}));
        assert!(res.is_ok());
        assert!(p.exists());
    }

    #[test]
    fn empty_content_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("empty.txt");
        let res = dispatch(serde_json::json!({"path": p.to_string_lossy(), "content": ""}));
        assert!(res.is_ok());
        assert!(p.exists());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "");
    }

    #[test]
    fn missing_path_is_error() {
        let res = dispatch(serde_json::json!({"content": "x"}));
        assert!(res.is_err());
    }
}