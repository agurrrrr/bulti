//! `mcp_tools` 모델 도구 (DESIGN.md §4.9, 2단계 레이지 로딩).
//!
//! 시스템 프롬프트에는 MCP 서버 인덱스(이름+설명)만 주입하고,
//! 모델이 `mcp_tools(server)`로 호출할 때 해당 서버의 툴 목록을 조회한다.
//! 이후 요청에 해당 서버 툴 스키마가 옵트인 주입된다(정의+디스패처 함께 활성화).

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::config::McpConfig;
use crate::mcp::McpManager;
use crate::tools::util::str_arg;
use crate::tools::{ToolHandler, ToolRegistry};

/// mcp_tools 도구 스키마.
pub fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "server": {
                "type": "string",
                "description": "MCP 서버 이름 (시스템 프롬프트의 서버 인덱스에서 확인)"
            }
        },
        "required": ["server"]
    })
}

/// mcp_tools 도구를 레지스트리에 등록한다.
///
/// `servers`/`manager` 를 등록 시점에 캡처해 디스패처에서 사용한다.
/// 호출 성공 시 해당 서버 툴 스키마를 레지스트리에 옵트인 주입한다.
pub fn register(
    reg: &Arc<ToolRegistry>,
    servers: BTreeMap<String, McpConfig>,
    manager: Arc<McpManager>,
) {
    let reg = reg.clone();
    let reg_for_handler = reg.clone();
    let handler: ToolHandler = Arc::new(move |args| {
        let servers = servers.clone();
        let manager = manager.clone();
        let reg = reg_for_handler.clone();
        Box::pin(async move {
            let server = match str_arg(&args, "server") {
                Some(s) if !s.trim().is_empty() => s,
                _ => return Err("server 은 MCP 서버 이름 문자열이어야 합니다".to_string()),
            };
            let config = match servers.get(&server) {
                Some(c) => c.clone(),
                None => return Err(format!("MCP 서버 '{server}'이 설정에 없습니다")),
            };
            let client = manager.client(&server, &config);
            let list = match client.list_tools(&server).await {
                Ok(list) => list,
                Err(e) => return Err(e.to_string()),
            };

            // 옵트인 주입: 해당 서버 툴 스키마를 레지스트리에 등록 (정의+디스패처 함께).
            match client.tool_schemas(&server).await {
                Ok(schemas) => {
                    for schema in schemas {
                        let server_name = server.clone();
                        let tool_name = schema.name.clone();
                        let client = client.clone();
                        let handler: ToolHandler = Arc::new(move |args| {
                            let client = client.clone();
                            let server = server_name.clone();
                            let tool = tool_name.clone();
                            Box::pin(async move {
                                client
                                    .call_tool(&server, &tool, args)
                                    .await
                                    .map_err(|e| e.to_string())
                            })
                        });
                        reg.register(
                            &format!("{server}__{tool}", server = server, tool = schema.name),
                            &format!("[MCP:{server}] {} — {}", schema.name, schema.description),
                            schema.input_schema,
                            handler,
                        );
                    }
                }
                Err(e) => {
                    // 옵트인 주입 실패 시에도 목록은 반환한다 (부분 성공).
                    return Ok(format!("{list}\n(주의: 툴 스키마 옵트인 주입 실패: {e})"));
                }
            }

            Ok(list)
        })
    });
    reg.register(
        "mcp_tools",
        "MCP 서버의 툴 목록을 조회합니다. 시스템 프롬프트의 MCP 서버 인덱스에서 해당 서버의 툴이 필요하다고 판단될 때 호출하세요.",
        schema(),
        handler,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// mcp_tools 도구가 레지스트리에 등록되는지 확인한다 (2단계 로딩 진입점).
    #[test]
    fn register_adds_mcp_tools_tool() {
        let reg = Arc::new(crate::tools::ToolRegistry::new(false));
        let servers = BTreeMap::new();
        let manager = Arc::new(McpManager::new());
        register(&reg, servers, manager);
        assert!(reg.names().contains(&"mcp_tools".to_string()));
    }

    /// 스키마는 server 필드를 필수로 요구한다.
    #[test]
    fn schema_requires_server() {
        let s = schema();
        let required = s["required"].as_array().unwrap();
        let names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"server"));
    }
}