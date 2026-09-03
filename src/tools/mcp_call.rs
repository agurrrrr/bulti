//! `mcp_call` 모델 도구 (DESIGN.md §4.9, 2단계 레이지 로딩).
//!
//! 옵트인 주입된 서버 툴(`{server}__{tool}`) 대신 범용 호출을 제공한다.
//! `mcp_call(server, tool, args)` → 해당 서버 툴을 호출한다.
//! 스키마 불일치 오류 시 파라미터 스키마를 재안내해 모델이 다시 시도하도록 돕는다.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::config::McpConfig;
use crate::mcp::McpManager;
use crate::tools::util::str_arg;
use crate::tools::{ToolHandler, ToolRegistry};

/// mcp_call 도구 스키마.
pub fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "server": {
                "type": "string",
                "description": "MCP 서버 이름 (시스템 프롬프트의 서버 인덱스에서 확인)"
            },
            "tool": {
                "type": "string",
                "description": "호출할 서버 툴 이름"
            },
            "args": {
                "type": "object",
                "description": "툴에 전달할 인자 JSON 객체"
            }
        },
        "required": ["server", "tool", "args"]
    })
}

/// mcp_call 도구를 레지스트리에 등록한다.
///
/// `servers`/`manager` 를 등록 시점에 캡처해 디스패처에서 사용한다.
/// `mcp_tools`와 달리 항상 활성 상태로 등록된다.
pub fn register(
    reg: &Arc<ToolRegistry>,
    servers: BTreeMap<String, McpConfig>,
    manager: Arc<McpManager>,
) {
    let handler: ToolHandler = Arc::new(move |args| {
        let servers = servers.clone();
        let manager = manager.clone();
        Box::pin(async move {
            let server = match str_arg(&args, "server") {
                Some(s) if !s.trim().is_empty() => s,
                _ => return Err("server 은 MCP 서버 이름 문자열이어야 합니다".to_string()),
            };
            let tool = match str_arg(&args, "tool") {
                Some(t) if !t.trim().is_empty() => t,
                _ => return Err("tool 은 호출할 서버 툴 이름 문자열이어야 합니다".to_string()),
            };
            let tool_args = args.get("args").cloned().unwrap_or(serde_json::json!({}));
            if !tool_args.is_object() {
                return Err("args 는 JSON 객체여야 합니다".to_string());
            }

            let config = match servers.get(&server) {
                Some(c) => c.clone(),
                None => return Err(format!("MCP 서버 '{server}'이 설정에 없습니다")),
            };
            let client = manager.client(&server, &config);

            match client.call_tool(&server, &tool, tool_args).await {
                Ok(out) => Ok(out),
                Err(e) => {
                    // 스키마 불일치 오류 시 파라미터 스키마를 재안내한다.
                    let err = e.to_string();
                    if looks_like_schema_mismatch(&err) {
                        if let Ok(schemas) = client.tool_schemas(&server).await {
                            if let Some(schema) = schemas.iter().find(|s| s.name == tool) {
                                let params = schema
                                    .input_schema
                                    .get("properties")
                                    .cloned()
                                    .unwrap_or(serde_json::json!({}));
                                return Err(format!(
                                    "{err}\n{tool} 파라미터 스키마: {}",
                                    serde_json::to_string_pretty(&params)
                                        .unwrap_or_else(|_| params.to_string())
                                ));
                            }
                        }
                    }
                    Err(err)
                }
            }
        })
    });
    reg.register(
        "mcp_call",
        "MCP 서버의 툴을 직접 호출합니다. server, tool, args(JSON 객체)를 지정하세요. 옵트인 주입된 {server}__{tool} 툴이 없거나, 범용 호출이 필요할 때 사용합니다.",
        schema(),
        handler,
    );
}

/// 오류 문자열이 스키마 불일치(인자 검증 실패)로 보이는지 판단한다.
fn looks_like_schema_mismatch(err: &str) -> bool {
    const HINTS: &[&str] = &[
        "invalid",
        "invalid params",
        "invalid argument",
        "validation failed",
        "missing required",
        "unexpected",
        "type mismatch",
        "인자",
        "파라미터",
        "스키마",
    ];
    let lower = err.to_lowercase();
    HINTS.iter().any(|h| lower.contains(h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_required_fields() {
        let s = schema();
        let required = s["required"].as_array().unwrap();
        let names: Vec<&str> = required
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(names.contains(&"server"));
        assert!(names.contains(&"tool"));
        assert!(names.contains(&"args"));
    }

    #[test]
    fn detects_schema_mismatch_hints() {
        assert!(looks_like_schema_mismatch("invalid params: missing required field"));
        assert!(looks_like_schema_mismatch("validation failed: unexpected value"));
        assert!(!looks_like_schema_mismatch("tool execution error"));
    }

    #[test]
    fn register_adds_mcp_call_tool() {
        let reg = Arc::new(crate::tools::ToolRegistry::new(false));
        let servers = BTreeMap::new();
        let manager = Arc::new(McpManager::new());
        register(&reg, servers, manager);
        assert!(reg.names().contains(&"mcp_call".to_string()));
    }
}