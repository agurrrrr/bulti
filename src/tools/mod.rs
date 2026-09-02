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
pub mod history;
pub mod read_file;
pub mod skill_load;
pub mod util;
pub mod write_file;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
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
    /// 상태 변경 도구(write_file/edit_file/bash)가 만진 파일 경로 (§4.7.1).
    /// `dispatch`가 `&self`를 유지하면서 수집하기 위해 RefCell을 사용한다.
    files_touched: RefCell<Vec<String>>,
}

impl ToolRegistry {
    /// 빈 레지스트리를 만든다.
    pub fn new(vision: bool) -> Self {
        Self {
            tools: BTreeMap::new(),
            vision,
            files_touched: RefCell::new(Vec::new()),
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
            Some(spec) => {
                self.collect_files_touched(name, &args);
                (spec.handler)(args).await
            }
            None => Err(format!("unknown tool: {name}")),
        }
    }

    /// 상태 변경 도구(write_file/edit_file/bash) 호출에서 파일 경로를 수집한다 (§4.7.1).
    ///
    /// - `write_file`/`edit_file`: `path` 인자.
    /// - `bash`: cwd가 프로젝트 루트로 고정이므로 명령 문자열에서 경로 힌트만 수집한다.
    fn collect_files_touched(&self, name: &str, args: &serde_json::Value) {
        let mut paths: Vec<String> = Vec::new();
        match name {
            "write_file" | "edit_file" => {
                if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                    if !p.trim().is_empty() {
                        paths.push(p.to_string());
                    }
                }
            }
            "bash" => {
                if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
                    paths.extend(extract_path_hints(cmd));
                }
            }
            _ => return,
        }
        if paths.is_empty() {
            return;
        }
        let mut touched = self.files_touched.borrow_mut();
        for p in paths {
            if !touched.contains(&p) {
                touched.push(p);
            }
        }
    }

    /// 상태 변경 도구가 만진 파일 경로 목록을 반환한다 (§4.7.1).
    pub fn files_touched(&self) -> Vec<String> {
        self.files_touched.borrow().clone()
    }

    /// 수집된 파일 경로를 초기화한다 (새 run 시작 시).
    pub fn clear_files_touched(&self) {
        self.files_touched.borrow_mut().clear();
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

/// bash 명령 문자열에서 파일 경로 힌트를 추출한다.
///
/// cwd가 프로젝트 루트로 고정되므로, 파일을 만지거나 편집하는 명령
/// (write_file/edit_file/bash 힌트)의 피연산자로 보이는 경로를 수집한다.
/// 정확한 파싱 대신 힌트 수준의 추출로 충분하다 (§4.7.1 "bash는 힌트만").
fn extract_path_hints(cmd: &str) -> Vec<String> {
    // 상태 변경을 유발할 가능성이 있는 명령어 접두사.
    const MUTATING: &[&str] = &[
        "write_file", "edit_file", "cat >", "cat >>", "echo >", "echo >>",
        "mkdir -p", "touch ", "rm ", "mv ", "cp ", "sed -i", "git add",
        "tee ", ">", ">>",
    ];
    // 접두사가 있는 하위 명령만 검사한다. (전체 문자열에서 접두사 발견 시)
    let mut paths: Vec<String> = Vec::new();
    let has_mutating = MUTATING.iter().any(|m| cmd.contains(m));
    if !has_mutating {
        return paths;
    }
    // 경로처럼 보이는 토큰을 수집한다 (확장자 포함 파일 경로).
    for tok in cmd.split_whitespace() {
        let tok = tok.trim_matches(|c| c == '\'' || c == '"' || c == '`');
        if looks_like_path(tok) {
            paths.push(tok.to_string());
        }
    }
    paths
}

/// 토큰이 파일 경로처럼 보이는지 판단한다.
fn looks_like_path(tok: &str) -> bool {
    if tok.is_empty() {
        return false;
    }
    // 확장자(.rs, .toml, .md, .json 등)가 붙은 경로.
    if tok.contains('/') {
        return true;
    }
    // `src/x.rs`처럼 확장자만으로 경로로 보이는 경우 (슬래시 없이도).
    if tok.contains('.') {
        let ext = tok.rsplit('.').next().unwrap_or("");
        if !ext.is_empty() && !ext.contains('/') {
            return true;
        }
    }
    // git add 등은 파일명만 와도 수집.
    if tok.starts_with("src/") || tok.starts_with("tests/") || tok.starts_with("docs/") {
        return true;
    }
    false
}

/// 네이티브 툴 + history 조회 툴을 모두 등록한 레지스트리를 만든다.
///
/// `cwd`/`global_dir` 은 `skill_load` 도구가 스킬을 발견·로드하는 데 사용한다.
pub fn native_registry(vision: bool, cwd: PathBuf, global_dir: PathBuf) -> ToolRegistry {
    let mut reg = ToolRegistry::new(vision);
    bash::register(&mut reg);
    read_file::register(&mut reg);
    write_file::register(&mut reg);
    edit_file::register(&mut reg);
    glob::register(&mut reg);
    grep::register(&mut reg);
    history::register_list(&mut reg);
    history::register_read(&mut reg);
    skill_load::register(&mut reg, cwd, global_dir);
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

    /// 네이티브 레지스트리에 툴들이 등록된다.
    #[test]
    fn native_registry_has_tools() {
        let reg = native_registry(false, PathBuf::from("."), PathBuf::from("."));
        let names = reg.names();
        assert_eq!(names.len(), 9);
        for name in [
            "bash",
            "read_file",
            "write_file",
            "edit_file",
            "glob",
            "grep",
            "history_list",
            "history_read",
            "skill_load",
        ] {
            assert!(names.contains(&name.to_string()), "missing {name}");
        }
    }

    /// 상태 변경 도구 호출 시 파일 경로가 수집된다 (§4.7.1).
    #[tokio::test]
    async fn files_touched_collected_from_mutating_tools() {
        let reg = native_registry(false, PathBuf::from("."), PathBuf::from("."));

        // write_file: path 수집.
        reg.dispatch(
            "write_file",
            serde_json::json!({"path": "src/new.rs", "content": "x"}),
        )
        .await
        .unwrap();
        // edit_file: path 수집.
        reg.dispatch(
            "edit_file",
            serde_json::json!({"path": "src/tools/mod.rs", "find": "a", "replace": "b"}),
        )
        .await
        .unwrap_err(); // 실제 파일이 없어 오류지만 수집은 수행됨.
        // bash: 경로 힌트 수집.
        reg.dispatch("bash", serde_json::json!({"command": "cat > out.txt hi"}))
            .await
            .unwrap();

        let touched = reg.files_touched();
        assert!(touched.contains(&"src/new.rs".to_string()), "{touched:?}");
        assert!(touched.contains(&"src/tools/mod.rs".to_string()), "{touched:?}");
        assert!(touched.contains(&"out.txt".to_string()), "{touched:?}");
    }

    /// 읽기 전용 도구는 파일을 수집하지 않는다.
    #[tokio::test]
    async fn files_touched_ignores_readonly_tools() {
        let reg = native_registry(false, PathBuf::from("."), PathBuf::from("."));
        reg.dispatch("grep", serde_json::json!({"pattern": "foo"}))
            .await
            .unwrap();
        assert!(reg.files_touched().is_empty());
    }

    /// clear_files_touched 는 수집된 경로를 초기화한다 (새 run 시작 시).
    #[tokio::test]
    async fn clear_files_touched_resets() {
        let reg = native_registry(false, PathBuf::from("."), PathBuf::from("."));
        reg.dispatch("write_file", serde_json::json!({"path": "a.rs", "content": "x"}))
            .await
            .unwrap();
        assert_eq!(reg.files_touched(), vec!["a.rs".to_string()]);
        reg.clear_files_touched();
        assert!(reg.files_touched().is_empty());
    }
}