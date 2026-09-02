//! 네이티브 툴 + ToolRegistry (정의·실행 통합) (DESIGN.md §4.4).
//!
//! 단계 3: ToolRegistry 계약(정의·디스패처 통합, `required: null` → `[]` 정규화)과
//! 네이티브 툴 6종(`bash`, `read_file`, `write_file`, `edit_file`, `glob`, `grep`)을 구현한다.
//!
//! **ToolRegistry 계약 (shepherd unknown-tool 사고 재발 방지):** 모델에게 보이는
//! "툴 정의"와 실제 실행하는 "디스패처"가 반드시 같은 레지스트리에서 나온다.
//! 정의만 전달하고 실행기를 잊으면 모든 호출이 `unknown tool`로 죽는다.

pub mod bash;
pub mod edit_file;
pub mod glob;
pub mod grep;
pub mod read_file;
pub mod util;
pub mod write_file;

use std::collections::BTreeMap;
use std::sync::Arc;

use futures_util::future::BoxFuture;

use crate::llm::{ToolDef, ToolFunction};

/// 툴 디스패처. 인자 JSON을 받아 결과 텍스트(또는 오류)를 반환한다.
/// 오류도 결과 문자열로 반환해 run을 죽이지 않는다 (DESIGN.md §4.9).
pub type ToolHandler =
    Arc<dyn Fn(serde_json::Value) -> BoxFuture<'static, Result<String, String>> + Send + Sync>;

/// 레지스트리에 등록된 툴 하나. 정의와 디스패처를 함께 묶는다.
pub struct ToolSpec {
    /// 모델에게 보이는 정의 (스키마 직렬화용).
    pub def: ToolDef,
    /// 실제 실행 디스패처.
    pub handler: ToolHandler,
}

/// ToolRegistry. 정의·디스패처 통합의 단일 출처.
pub struct ToolRegistry {
    tools: BTreeMap<String, ToolSpec>,
    /// 비전 엔드포인트 여부 (read_file 이미지 처리에 사용).
    vision: bool,
}

impl ToolRegistry {
    /// 빈 레지스트리를 만든다.
    pub fn new(vision: bool) -> Self {
        Self {
            tools: BTreeMap::new(),
            vision,
        }
    }

    /// 툴을 등록한다. 스키마는 직렬화 시 `required: null` → `[]`로 정규화된다.
    pub fn register(
        &mut self,
        name: &str,
        description: &str,
        parameters: serde_json::Value,
        handler: ToolHandler,
    ) {
        let parameters = normalize_schema(parameters);
        let def = ToolDef {
            r#type: "function".to_string(),
            function: ToolFunction {
                name: name.to_string(),
                description: description.to_string(),
                parameters,
            },
        };
        self.tools.insert(name.to_string(), ToolSpec { def, handler });
    }

    /// 모델에게 노출할 툴 정의 목록. 요청 본문의 `tools` 배열에 쓰인다.
    pub fn definitions(&self) -> Vec<ToolDef> {
        self.tools.values().map(|spec| spec.def.clone()).collect()
    }

    /// 툴을 실행한다. 미등록 툴은 `unknown tool` 오류를 반환한다.
    pub async fn dispatch(&self, name: &str, args: serde_json::Value) -> Result<String, String> {
        match self.tools.get(name) {
            Some(spec) => (spec.handler)(args).await,
            None => Err(format!("unknown tool: {name}")),
        }
    }

    /// 등록된 툴 이름 목록.
    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// 비전 엔드포인트 여부.
    pub fn vision(&self) -> bool {
        self.vision
    }
}

/// 스키마 직렬화 시 `required: null` → `[]`로 정규화한다.
///
/// llama.cpp 툴 템플릿 파서가 `required: null`을 400으로 거부하는 문제
/// (shepherd #5814)를 방지한다.
pub fn normalize_schema(schema: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = schema.as_object() {
        if let Some(required) = obj.get("required") {
            let normalized = if required.is_null() {
                serde_json::json!([])
            } else {
                required.clone()
            };
            let mut obj = obj.clone();
            obj.insert("required".to_string(), normalized);
            return serde_json::Value::Object(obj);
        }
    }
    schema
}

/// 네이티브 툴 6종을 모두 등록한 레지스트리를 만든다.
pub fn native_registry(vision: bool) -> ToolRegistry {
    let mut reg = ToolRegistry::new(vision);
    bash::register(&mut reg);
    read_file::register(&mut reg);
    write_file::register(&mut reg);
    edit_file::register(&mut reg);
    glob::register(&mut reg);
    grep::register(&mut reg);
    reg
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: serde_json::Value) -> serde_json::Value {
        v
    }

    /// 등록한 툴의 정의와 디스패처가 같은 레지스트리에서 나오는지 확인한다.
    #[tokio::test]
    async fn registry_definitions_and_dispatch_from_same_source() {
        let mut reg = ToolRegistry::new(false);
        reg.register(
            "echo",
            "입력 그대로 반환",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string"}
                },
                "required": ["text"],
            }),
            Arc::new(|a| {
                Box::pin(async move {
                    Ok(a["text"].as_str().unwrap_or("").to_string())
                })
            }),
        );

        // 정의에 등록된 툴은 디스패처에서도 실행 가능해야 한다.
        let defs = reg.definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].function.name, "echo");

        let res = reg
            .dispatch("echo", args(serde_json::json!({"text": "hello"})))
            .await
            .unwrap();
        assert_eq!(res, "hello");

        // 미등록 툴은 unknown tool 오류.
        let err = reg.dispatch("nope", args(serde_json::json!({}))).await;
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("unknown tool"));
    }

    /// `required: null` → `[]` 정규화 테스트.
    #[test]
    fn normalize_schema_required_null_to_empty() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"x": {"type": "string"}},
            "required": null,
        });
        let normalized = normalize_schema(schema);
        let required = normalized["required"].as_array().unwrap();
        assert!(required.is_empty(), "required: null 은 [] 로 정규화되어야 함");
    }

    /// 이미 `required` 배열이면 그대로 유지된다.
    #[test]
    fn normalize_schema_keeps_existing_required() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"x": {"type": "string"}},
            "required": ["x"],
        });
        let normalized = normalize_schema(schema);
        let required = normalized["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
    }

    /// `required` 키가 아예 없으면 변경하지 않는다.
    #[test]
    fn normalize_schema_no_required_unchanged() {
        let schema = serde_json::json!({"type": "object", "properties": {}});
        let normalized = normalize_schema(schema);
        assert_eq!(normalized["type"], "object");
        assert!(normalized.get("required").is_none());
    }

    /// 네이티브 레지스트리에 6종 툴이 등록된다.
    #[test]
    fn native_registry_has_6_tools() {
        let reg = native_registry(false);
        let names = reg.names();
        assert_eq!(names.len(), 6);
        for name in ["bash", "read_file", "write_file", "edit_file", "glob", "grep"] {
            assert!(names.contains(&name.to_string()), "missing {name}");
        }
    }
}