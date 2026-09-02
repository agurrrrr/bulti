//! `skill_load` 모델 도구 (DESIGN.md §4.8).
//!
//! 시스템 프롬프트에는 스킬 인덱스(이름+설명 1행/스킬)만 주입하고,
//! 모델이 `skill_load(name)`으로 호출할 때 본문 전체를 로드한다.
//! 본문은 절대 자동 주입하지 않는다.

use std::path::PathBuf;

use crate::skills;
use crate::tools::util::str_arg;
use crate::tools::{ToolHandler, ToolRegistry};

/// skill_load 도구 스키마.
pub fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "로드할 스킬 이름 (프로젝트 → 글로벌 → 번들 순으로 찾음)"
            }
        },
        "required": ["name"]
    })
}

/// skill_load 도구를 레지스트리에 등록한다.
///
/// `cwd`/`global_dir` 을 등록 시점에 캡처해 디스패처에서 사용한다.
pub fn register(reg: &mut ToolRegistry, cwd: PathBuf, global_dir: PathBuf) {
    let handler: ToolHandler = std::sync::Arc::new(move |args| {
        let cwd = cwd.clone();
        let global_dir = global_dir.clone();
        Box::pin(async move {
            let name = match str_arg(&args, "name") {
                Some(n) if !n.trim().is_empty() => n,
                _ => return Err("name 은 스킬 이름 문자열이어야 합니다".to_string()),
            };
            match skills::load(&name, &cwd, &global_dir) {
                Ok(body) => Ok(body),
                Err(e) => Err(e.to_string()),
            }
        })
    });
    reg.register(
        "skill_load",
        "스킬 본문 전체를 로드합니다. 시스템 프롬프트의 스킬 인덱스에서 본문이 필요하다고 판단될 때 호출하세요.",
        schema(),
        handler,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "bulti_skill_tool_test_{}_{}",
            std::process::id(),
            label
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn loads_bundled_skill() {
        let root = temp_dir("bundled");
        let mut reg = crate::tools::ToolRegistry::new(false);
        register(&mut reg, root.clone(), root.clone());
        let res = reg
            .dispatch(
                "skill_load",
                serde_json::json!({"name": "korean-report"}),
            )
            .await;
        let out = res.unwrap();
        assert!(out.contains("한국어 보고"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn missing_skill_returns_error() {
        let root = temp_dir("missing");
        let mut reg = crate::tools::ToolRegistry::new(false);
        register(&mut reg, root.clone(), root.clone());
        let res = reg
            .dispatch("skill_load", serde_json::json!({"name": "nope"}))
            .await;
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("찾을 수 없습니다"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn missing_name_returns_error() {
        let root = temp_dir("missing_name");
        let mut reg = crate::tools::ToolRegistry::new(false);
        register(&mut reg, root.clone(), root.clone());
        let res = reg.dispatch("skill_load", serde_json::json!({})).await;
        assert!(res.is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }
}