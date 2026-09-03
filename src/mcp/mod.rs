//! 레이지 MCP 클라이언트 (rmcp) (DESIGN.md §4.9).
//!
//! 2단계 레이지 로딩:
//! - 시스템 프롬프트에는 서버 인덱스(이름+설명)만 주입한다.
//! - `mcp_tools(server)` → 해당 서버 툴 목록(이름·설명·파라미터 요약) 반환.
//!   이후 요청에 해당 서버 툴 스키마가 옵트인 주입된다(정의+디스패처 함께 활성화).
//! - `mcp_call(server, tool, args)` → 툴 호출. 스키마 불일치 오류에 파라미터 스키마 재안내.
//!
//! 클라이언트는 rmcp stdio transport(`TokioChildProcess`)를 사용하며,
//! 서버 프로세스는 **첫 MCP 도구 호출 시에만** spawn 된다(지연 시작).
//! 결과 파싱은 `content`(text)와 `structuredContent` 모두 고려하며,
//! text 가 비면 `structuredContent` 원본 JSON 으로 폴백한다 (shepherd #6350 교훈).
//! 타임아웃(60초)·서버 장애는 도구 결과 오류로 반환하고 run 을 죽이지 않는다.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use rmcp::model::{CallToolRequestParams, ContentBlock};
use rmcp::service::{serve_client, RoleClient, RunningService};
use rmcp::transport::child_process::TokioChildProcess;
use tokio::process::Command;

use crate::config::McpConfig;

/// MCP 도구 호출 타임아웃 (초).
pub const MCP_TIMEOUT_SECS: u64 = 60;

/// MCP 관련 오류. 모두 도구 결과 오류 문자열로 변환된다.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("MCP 서버 '{name}' 프로세스를 시작하지 못했습니다: {source}")]
    Spawn {
        name: String,
        #[source]
        source: std::io::Error,
    },
    #[error("MCP 서버 '{name}' 초기화에 실패했습니다: {source}")]
    Initialize {
        name: String,
        #[source]
        source: Box<rmcp::service::ClientInitializeError>,
    },
    #[error("MCP 서버 '{name}' 도구 목록 조회 실패: {source}")]
    ListTools {
        name: String,
        #[source]
        source: rmcp::service::ServiceError,
    },
    #[error("MCP 서버 '{name}' 툴 '{tool}' 호출 실패: {source}")]
    Call {
        name: String,
        tool: String,
        #[source]
        source: rmcp::service::ServiceError,
    },
    #[error("MCP 도구 호출이 {MCP_TIMEOUT_SECS}초를 초과해 타임아웃되었습니다")]
    Timeout,
    #[error("MCP 서버 '{name}'에 툴 '{tool}'이 없습니다")]
    ToolNotFound { name: String, tool: String },
}

/// 하나의 MCP 서버 연결. 서버 프로세스는 첫 호출 시에만 spawn 된다.
pub struct McpClient {
    /// 서버 설정 (command, args, env, description).
    config: McpConfig,
    /// 활성 rmcp 연결. `None` 이면 아직 spawn 하지 않음 (지연 시작).
    /// Arc 로 감싸 lock 을 짧게 유지해 Send future 를 보장한다.
    running: Mutex<Option<Arc<RunningService<RoleClient, ()>>>>,
}

impl McpClient {
    /// 서버 설정으로 클라이언트를 만든다. 이 시점에는 프로세스를 띄우지 않는다.
    pub fn new(config: McpConfig) -> Self {
        Self {
            config,
            running: Mutex::new(None),
        }
    }

    /// 서버 프로세스를 (첫 호출 시에만) spawn 하고 연결을 초기화한다.
    async fn ensure_connected(&self, name: &str) -> Result<(), McpError> {
        // 이미 연결됨 → lock 을 짧게 유지하고 즉시 반환.
        {
            let guard = self.running.lock().unwrap();
            if guard.is_some() {
                return Ok(());
            }
        }

        // tokio::process::Command 조립 → CommandWrap → TokioChildProcess (stdio transport).
        // await 동안 lock 을 유지하지 않도록 spawn+초기화는 lock 없이 수행한다.
        let mut cmd = Command::new(&self.config.command);
        cmd.args(&self.config.args);
        cmd.envs(&self.config.env);
        let transport = TokioChildProcess::new(cmd).map_err(|source| McpError::Spawn {
            name: name.to_string(),
            source,
        })?;

        let running = serve_client((), transport)
            .await
            .map_err(|source| McpError::Initialize {
                name: name.to_string(),
                source: Box::new(source),
            })?;

        // 저장 (lock 짧게).
        let mut guard = self.running.lock().unwrap();
        *guard = Some(Arc::new(running));
        Ok(())
    }

    /// 연결된 `RunningService` 를 Arc 로 가져온다. lock 은 즉시 해제되어 future 가 Send 를 유지한다.
    async fn connected(&self, name: &str) -> Result<Arc<RunningService<RoleClient, ()>>, McpError> {
        self.ensure_connected(name).await?;
        let guard = self.running.lock().unwrap();
        guard
            .as_ref()
            .cloned()
            .ok_or_else(|| McpError::ToolNotFound {
                name: name.to_string(),
                tool: "<연결>".to_string(),
            })
    }

    /// 서버 툴 목록을 조회한다. (이름·설명·파라미터 요약)
    pub async fn list_tools(&self, name: &str) -> Result<String, McpError> {
        self.ensure_connected(name).await?;
        let running = self.connected(name).await?;
        let result = tokio::time::timeout(
            tokio::time::Duration::from_secs(MCP_TIMEOUT_SECS),
            running.list_tools(None),
        )
        .await
        .map_err(|_| McpError::Timeout)?
        .map_err(|source| McpError::ListTools {
            name: name.to_string(),
            source,
        })?;

        let mut out = String::new();
        for tool in &result.tools {
            let desc = tool
                .description
                .as_deref()
                .unwrap_or("(설명 없음)")
                .to_string();
            let params = summarize_schema(&tool.input_schema);
            out.push_str(&format!(
                "- {} — {}{}\n",
                tool.name,
                desc,
                if params.is_empty() {
                    String::new()
                } else {
                    format!(" (파라미터: {params})")
                }
            ));
        }
        Ok(out)
    }

    /// 툴을 호출한다. 결과는 `content`(text) 우선, 비면 `structuredContent` 원본 JSON 폴백.
    pub async fn call_tool(
        &self,
        name: &str,
        tool: &str,
        args: serde_json::Value,
    ) -> Result<String, McpError> {
        self.ensure_connected(name).await?;
        let running = self.connected(name).await?;

        let params = CallToolRequestParams::new(tool.to_string())
            .with_arguments(args.as_object().cloned().unwrap_or_default());
        let result = tokio::time::timeout(
            tokio::time::Duration::from_secs(MCP_TIMEOUT_SECS),
            running.call_tool(params),
        )
        .await
        .map_err(|_| McpError::Timeout)?
        .map_err(|source| McpError::Call {
            name: name.to_string(),
            tool: tool.to_string(),
            source,
        })?;

        // is_error 가 true 면 오류로 반환한다.
        if result.is_error == Some(true) {
            let text = extract_text(&result.content);
            let message = if text.is_empty() {
                format!("MCP 툴 '{tool}' 실행 오류")
            } else {
                text
            };
            return Err(McpError::Call {
                name: name.to_string(),
                tool: tool.to_string(),
                source: rmcp::service::ServiceError::McpError(rmcp::model::ErrorData::new(
                    rmcp::model::ErrorCode(-32602),
                    message,
                    None,
                )),
            });
        }

        // content(text) 우선, 비면 structuredContent 원본 JSON 폴백 (shepherd #6350).
        Ok(parse_tool_result(&result.content, result.structured_content.as_ref()))
    }

    /// 서버 툴 이름 목록을 반환한다 (스키마 옵트인 주입용).
    pub async fn tool_schemas(&self, name: &str) -> Result<Vec<ToolSchema>, McpError> {
        self.ensure_connected(name).await?;
        let running = self.connected(name).await?;
        let result = tokio::time::timeout(
            tokio::time::Duration::from_secs(MCP_TIMEOUT_SECS),
            running.list_tools(None),
        )
        .await
        .map_err(|_| McpError::Timeout)?
        .map_err(|source| McpError::ListTools {
            name: name.to_string(),
            source,
        })?;

        Ok(result
            .tools
            .into_iter()
            .map(|t| ToolSchema {
                name: t.name.to_string(),
                description: t.description.as_deref().unwrap_or("").to_string(),
                input_schema: serde_json::to_value(&t.input_schema).unwrap_or(serde_json::json!({})),
            })
            .collect())
    }
}

/// 옵트인 주입용 서버 툴 스키마 요약.
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// MCP 서버 맵을 캐시해 지연 spawn 을 관리하는 매니저.
pub struct McpManager {
    /// 서버 이름 → 지연 spawn 클라이언트.
    clients: Mutex<BTreeMap<String, Arc<McpClient>>>,
}

impl McpManager {
    /// 빈 매니저를 만든다.
    pub fn new() -> Self {
        Self {
            clients: Mutex::new(BTreeMap::new()),
        }
    }

    /// 서버 이름으로 클라이언트를 조회한다. 없으면 새로 만든다 (이 시점엔 spawn 안 함).
    pub fn client(&self, name: &str, config: &McpConfig) -> Arc<McpClient> {
        let mut guard = self.clients.lock().unwrap();
        guard
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(McpClient::new(config.clone())))
            .clone()
    }
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

/// `content` 블록 목록에서 텍스트를 합친다.
fn extract_text(content: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in content {
        if let ContentBlock::Text(text) = block {
            out.push_str(&text.text);
        }
    }
    out
}

/// `call_tool` 결과를 문자열로 정규화한다.
///
/// 우선 `content`(text)를 합친다. 비어 있으면 `structured_content` 원본 JSON 을
/// pretty-print 해 폴백으로 사용한다. 둘 다 없으면 "(결과 없음)"을 반환한다.
/// (shepherd #6350: structuredContent 만 있는 서버도 결과가 유실되지 않도록)
fn parse_tool_result(content: &[ContentBlock], structured: Option<&serde_json::Value>) -> String {
    let text = extract_text(content);
    if !text.is_empty() {
        return text;
    }
    if let Some(sc) = structured {
        return serde_json::to_string_pretty(sc).unwrap_or_else(|_| sc.to_string());
    }
    "(결과 없음)".to_string()
}

/// 입력 스키마에서 파라미터 요약을 만든다. `required: null` → `[]` 정규화 포함.
fn summarize_schema(schema: &serde_json::Map<String, serde_json::Value>) -> String {
    let obj = serde_json::Value::Object(schema.clone());
    let normalized = crate::tools::normalize_schema(obj);
    let Some(props) = normalized.get("properties").and_then(|p| p.as_object()) else {
        return String::new();
    };
    let mut names: Vec<String> = props.keys().cloned().collect();
    names.sort();
    names.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `required: null` → `[]` 정규화가 파라미터 요약에 반영되는지 확인한다.
    #[test]
    fn summarize_schema_ignores_required_null() {
        let mut map = serde_json::Map::new();
        map.insert("type".into(), json!("object"));
        map.insert("properties".into(), json!({"a": {"type": "string"}, "b": {"type": "number"}}));
        map.insert("required".into(), serde_json::Value::Null);
        let s = summarize_schema(&map);
        assert_eq!(s, "a, b");
    }

    /// 파라미터가 없으면 빈 문자열을 반환한다.
    #[test]
    fn summarize_schema_empty_when_no_properties() {
        let mut map = serde_json::Map::new();
        map.insert("type".into(), json!("object"));
        let s = summarize_schema(&map);
        assert_eq!(s, "");
    }

    /// text 블록만 합친다 (이미지·리소스 블록은 무시).
    #[test]
    fn extract_text_joins_text_blocks() {
        let content = vec![
            ContentBlock::text("hello"),
            ContentBlock::text(" world"),
        ];
        assert_eq!(extract_text(&content), "hello world");
    }

    /// content(text)가 있으면 그것을 우선 반환한다.
    #[test]
    fn parse_tool_result_prefers_text_content() {
        let content = vec![ContentBlock::text("text result")];
        let structured = json!({"key": "value"});
        assert_eq!(parse_tool_result(&content, Some(&structured)), "text result");
    }

    /// content(text)가 비어 있으면 structuredContent 원본 JSON 을 폴백으로 반환한다 (shepherd #6350).
    #[test]
    fn parse_tool_result_falls_back_to_structured_content() {
        let structured = json!({"sum": 42, "items": [1, 2, 3]});
        let out = parse_tool_result(&[], Some(&structured));
        // pretty-printed JSON 에 필드가 포함되어야 한다.
        assert!(out.contains("\"sum\": 42"), "출력: {out}");
        assert!(out.contains("\"items\": ["));
    }

    /// content 와 structuredContent 가 모두 없으면 "(결과 없음)"을 반환한다.
    #[test]
    fn parse_tool_result_empty_returns_placeholder() {
        assert_eq!(parse_tool_result(&[], None), "(결과 없음)");
    }

    /// content(text)가 있으면 structuredContent 를 무시한다 (우선순위 확인).
    #[test]
    fn parse_tool_result_ignores_structured_when_text_present() {
        let content = vec![ContentBlock::text("hello")];
        let structured = json!({"ignored": true});
        assert_eq!(parse_tool_result(&content, Some(&structured)), "hello");
    }

    /// McpClient::new 는 프로세스를 spawn 하지 않는다 (지연 로딩).
    /// running 이 None 으로 유지되어 첫 호출(ensure_connected) 전에는 연결이 없다.
    #[test]
    fn mcp_client_new_does_not_spawn() {
        let config = McpConfig {
            command: "echo".to_string(),
            args: vec![],
            env: BTreeMap::new(),
            description: None,
        };
        let client = McpClient::new(config);
        let guard = client.running.lock().unwrap();
        assert!(guard.is_none(), "new() 시점에는 프로세스를 띄우지 않아야 한다");
    }

    /// McpManager::client 는 클라이언트를 만들 뿐 spawn 하지 않는다.
    /// 두 번 호출해도 같은 인스턴스를 반환한다 (캐시).
    #[test]
    fn mcp_manager_client_returns_cached_instance() {
        let manager = McpManager::new();
        let config = McpConfig {
            command: "echo".to_string(),
            args: vec![],
            env: BTreeMap::new(),
            description: None,
        };
        let a = manager.client("srv", &config);
        let b = manager.client("srv", &config);
        assert!(Arc::ptr_eq(&a, &b), "같은 서버는 같은 클라이언트 인스턴스를 반환해야 한다");
        // 여전히 spawn 되지 않았음을 확인.
        let guard = a.running.lock().unwrap();
        assert!(guard.is_none(), "client() 는 spawn 을 수행하지 않아야 한다");
    }
}