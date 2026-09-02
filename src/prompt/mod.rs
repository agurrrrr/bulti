//! 시스템 프롬프트 계층 조립 (DESIGN.md §4.10).
//!
//! 기본 조립 (병합):
//!   [빌트인 베이스] + [~/.bulti/prompts/default.md] + [<프로젝트>/.bulti/system.md] + [인덱스 섹션]
//!
//! 완전 교체: `--system-file <path>` / `--system "<text>"` → 빌트인·글로벌·프로젝트 무시,
//! 인덱스 섹션은 유지.
//!
//! 템플릿 변수 (모든 계층에서 치환): `{{cwd}}`, `{{os}}`, `{{endpoint}}`, `{{model}}`,
//! `{{context_tokens}}`.

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::config::{Config, EndpointConfig};

/// 빌트인 베이스 (include_str! 로 포함, 저장소에서 버전 관리).
const BASE_MD: &str = include_str!("base.md");

/// 인덱스 섹션 헤더.
const INDEX_HEADER: &str = "## 레이지 로딩 인덱스 (스킬·MCP·history)";

/// 프로젝트 시스템 프롬프트 파일 이름.
pub const PROJECT_PROMPT_FILENAME: &str = ".bulti/system.md";

/// 글로벌 기본 프롬프트 파일 이름.
pub const GLOBAL_PROMPT_FILENAME: &str = "prompts/default.md";

/// 프롬프트 조립 오류.
#[derive(Debug, Error)]
pub enum PromptError {
    #[error("시스템 프롬프트 파일을 읽는 데 실패했습니다: {0}")]
    Io(#[from] std::io::Error),
    #[error("홈 디렉터리를 찾을 수 없습니다: {0}")]
    HomeDirNotFound(String),
    #[error("템플릿 변수 치환에 실패했습니다: {0}")]
    Template(String),
}

/// 프롬프트 조립에 필요한 런타임 컨텍스트.
#[derive(Debug, Clone)]
pub struct PromptContext {
    /// 현재 작업 디렉터리 ({{cwd}}).
    pub cwd: PathBuf,
    /// 활성 엔드포인트 이름 ({{endpoint}}).
    pub endpoint: String,
    /// 활성 엔드포인트 설정 ({{model}}, {{context_tokens}}).
    pub endpoint_config: Option<EndpointConfig>,
    /// 글로벌 디렉터리 (기본 `~/.bulti`).
    pub global_dir: PathBuf,
    /// 프로젝트 루트 (`.bulti/system.md` 탐색 기준).
    pub project_root: PathBuf,
    /// 스킬 인덱스 (이름 + 설명 1행/스킬).
    pub skills: Vec<SkillIndex>,
    /// MCP 서버 인덱스 (서버 이름 + 설명).
    pub mcp_servers: Vec<McpIndex>,
}

/// 스킬 인덱스 항목.
#[derive(Debug, Clone)]
pub struct SkillIndex {
    pub name: String,
    pub description: String,
}

/// MCP 서버 인덱스 항목.
#[derive(Debug, Clone)]
pub struct McpIndex {
    pub name: String,
    pub description: String,
}

/// 템플릿 변수 값 모음.
#[derive(Debug, Clone)]
pub struct TemplateValues {
    pub cwd: String,
    pub os: String,
    pub endpoint: String,
    pub model: String,
    pub context_tokens: String,
}

/// 템플릿 변수 치환 (모든 계층에서 동일하게 적용).
pub fn render_template(text: &str, values: &TemplateValues) -> String {
    text.replace("{{cwd}}", &values.cwd)
        .replace("{{os}}", &values.os)
        .replace("{{endpoint}}", &values.endpoint)
        .replace("{{model}}", &values.model)
        .replace("{{context_tokens}}", &values.context_tokens)
}

/// 인덱스 섹션을 조립한다. 스킬·MCP·history 안내를 포함한다.
pub fn build_index(ctx: &PromptContext) -> String {
    let mut out = String::new();
    out.push_str(INDEX_HEADER);
    out.push('\n');

    // 스킬 인덱스.
    if ctx.skills.is_empty() {
        out.push_str("- 스킬: 없음\n");
    } else {
        out.push_str("- 스킬 (필요할 때 `skill_load(name)` 로 로드):\n");
        for s in &ctx.skills {
            out.push_str(&format!("  - {} — {}\n", s.name, s.description));
        }
    }

    // MCP 인덱스.
    if ctx.mcp_servers.is_empty() {
        out.push_str("- MCP 서버: 없음\n");
    } else {
        out.push_str("- MCP 서버 (필요할 때 `mcp_tools(server)` 로 로드):\n");
        for m in &ctx.mcp_servers {
            out.push_str(&format!("  - {} — {}\n", m.name, m.description));
        }
    }

    // history 안내.
    out.push_str(
        "- history (이전 작업 맥락 회수): `history_list(query?, limit?)`, `history_read(run_id)`\n",
    );

    out
}

/// 글로벌 기본 프롬프트 경로 (`<global_dir>/prompts/default.md`).
pub fn global_prompt_path(global_dir: &Path) -> PathBuf {
    global_dir.join(GLOBAL_PROMPT_FILENAME)
}

/// 프로젝트 시스템 프롬프트 경로 (`<project_root>/.bulti/system.md`).
pub fn project_prompt_path(project_root: &Path) -> PathBuf {
    project_root.join(PROJECT_PROMPT_FILENAME)
}

/// 파일을 읽되, 없으면 빈 문자열을 반환한다.
fn read_optional(path: &Path) -> Result<String, PromptError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(PromptError::Io(e)),
    }
}

/// 최종 시스템 프롬프트를 조립한다.
///
/// `overrides`:
/// - `Some(Override::Inline(text))` → `--system "<text>"`
/// - `Some(Override::File(path))` → `--system-file <path>`
/// - `None` → 계층 병합 (빌트인 + 글로벌 + 프로젝트 + 인덱스)
pub fn assemble(ctx: &PromptContext, overrides: Option<Override>) -> Result<String, PromptError> {
    // 템플릿 변수 값.
    let values = template_values(ctx);

    // 인덱스 섹션은 항상 유지.
    let index = build_index(ctx);

    let body = match overrides {
        // 완전 교체: 빌트인·글로벌·프로젝트 무시, 인덱스만 유지.
        Some(Override::Inline(text)) => text.to_string(),
        Some(Override::File(path)) => fs::read_to_string(path)?,
        // 계층 병합.
        None => {
            let global = read_optional(&global_prompt_path(&ctx.global_dir))?;
            let project = read_optional(&project_prompt_path(&ctx.project_root))?;
            let mut parts = Vec::new();
            parts.push(BASE_MD.to_string());
            if !global.trim().is_empty() {
                parts.push(global);
            }
            if !project.trim().is_empty() {
                parts.push(project);
            }
            parts.join("\n\n")
        }
    };

    // 변수 치환 (모든 계층, 교체 본문에도 적용).
    let rendered = render_template(&body, &values);

    // 인덱스 섹션을 본문 뒤에 붙인다.
    Ok(format!("{rendered}\n\n{index}"))
}

/// 템플릿 변수 값을 계산한다.
pub fn template_values(ctx: &PromptContext) -> TemplateValues {
    let os = std::env::consts::OS.to_string();
    let model = ctx
        .endpoint_config
        .as_ref()
        .map(|e| e.model.clone())
        .unwrap_or_default();
    let context_tokens = ctx
        .endpoint_config
        .as_ref()
        .map(|e| e.context_tokens.to_string())
        .unwrap_or_default();
    TemplateValues {
        cwd: ctx.cwd.display().to_string(),
        os,
        endpoint: ctx.endpoint.clone(),
        model,
        context_tokens,
    }
}

/// 완전 교체 오버라이드 종류.
#[derive(Debug, Clone)]
pub enum Override {
    /// `--system "<text>"` 인라인.
    Inline(String),
    /// `--system-file <path>` 파일.
    File(PathBuf),
}

/// `bulti prompt show` 구현: 최종 조립 결과를 그대로 출력한다.
pub fn show(ctx: &PromptContext, overrides: Option<Override>) -> Result<String, PromptError> {
    assemble(ctx, overrides)
}

/// `bulti prompt edit` 구현: 글로벌 파일을 $EDITOR(기본 vi)로 연다.
/// 편집 후 저장·닫기가 완료되면 아무것도 반환하지 않는다.
pub fn edit(ctx: &PromptContext) -> Result<(), PromptError> {
    let path = prepare_global_file(&ctx.global_dir)?;
    run_editor(&path)
}

/// 글로벌 프롬프트 파일을 준비한다 (디렉터리 생성 + 빈 파일 생성). 경로를 반환한다.
pub fn prepare_global_file(global_dir: &Path) -> Result<PathBuf, PromptError> {
    let path = global_prompt_path(global_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // 파일이 없으면 빈 파일 생성.
    if !path.exists() {
        fs::write(&path, "")?;
    }
    Ok(path)
}

/// $EDITOR(기본 vi)로 파일을 연다.
pub fn run_editor(path: &Path) -> Result<(), PromptError> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let status = std::process::Command::new(&editor).arg(path).status();
    match status {
        Ok(st) if st.success() => Ok(()),
        Ok(st) => Err(PromptError::Template(format!(
            "편집기가 오류 상태로 종료되었습니다 (exit {}): {}",
            st.code().unwrap_or(-1),
            editor
        ))),
        Err(e) => Err(PromptError::Template(format!(
            "편집기 실행 실패 ({editor}): {e}"
        ))),
    }
}

/// `PromptContext` 를 구성한다. 활성 엔드포인트 정보를 설정에서 가져온다.
pub fn context_from_config(
    cfg: &Config,
    cwd: PathBuf,
    project_root: PathBuf,
    skills: Vec<SkillIndex>,
    mcp_servers: Vec<McpIndex>,
) -> Result<PromptContext, PromptError> {
    let global_dir = Config::config_dir().map_err(|e| PromptError::HomeDirNotFound(e.to_string()))?;
    let (endpoint, endpoint_config) = match cfg.active_endpoint_config() {
        Some((name, ep)) => (name.to_string(), Some(ep.clone())),
        None => ("(없음)".to_string(), None),
    };
    Ok(PromptContext {
        cwd,
        endpoint,
        endpoint_config,
        global_dir,
        project_root,
        skills,
        mcp_servers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx(root: &Path, global: &Path) -> PromptContext {
        PromptContext {
            cwd: root.to_path_buf(),
            endpoint: "main".to_string(),
            endpoint_config: Some(EndpointConfig {
                url: "http://127.0.0.1:8084/v1".to_string(),
                api_key: None,
                model: "qwen3".to_string(),
                context_tokens: 16384,
                vision: false,
                thinking: false,
                max_iterations: 200,
            }),
            global_dir: global.to_path_buf(),
            project_root: root.to_path_buf(),
            skills: vec![SkillIndex {
                name: "korean-report".to_string(),
                description: "한국어 보고 스타일".to_string(),
            }],
            mcp_servers: vec![McpIndex {
                name: "files".to_string(),
                description: "파일시스템 접근".to_string(),
            }],
        }
    }

    #[test]
    fn template_variable_substitution() {
        let values = TemplateValues {
            cwd: "/tmp/proj".to_string(),
            os: "linux".to_string(),
            endpoint: "main".to_string(),
            model: "qwen3".to_string(),
            context_tokens: "16384".to_string(),
        };
        let text = "cwd={{cwd}} os={{os}} ep={{endpoint}} m={{model}} c={{context_tokens}}";
        let out = render_template(text, &values);
        assert_eq!(out, "cwd=/tmp/proj os=linux ep=main m=qwen3 c=16384");
    }

    #[test]
    fn template_unknown_variable_stays() {
        let values = TemplateValues {
            cwd: "c".to_string(),
            os: "o".to_string(),
            endpoint: "e".to_string(),
            model: "m".to_string(),
            context_tokens: "0".to_string(),
        };
        let out = render_template("{{unknown}}", &values);
        assert_eq!(out, "{{unknown}}");
    }

    #[test]
    fn index_contains_skills_mcp_history() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let ctx = test_ctx(&root, dir.path());
        let index = build_index(&ctx);
        assert!(index.contains("korean-report"));
        assert!(index.contains("files"));
        assert!(index.contains("history_list"));
        assert!(index.contains("skill_load"));
        assert!(index.contains("mcp_tools"));
    }

    #[test]
    fn index_empty_lists() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let mut ctx = test_ctx(&root, dir.path());
        ctx.skills.clear();
        ctx.mcp_servers.clear();
        let index = build_index(&ctx);
        assert!(index.contains("스킬: 없음"));
        assert!(index.contains("MCP 서버: 없음"));
        assert!(index.contains("history_list"));
    }

    #[test]
    fn merge_layers_builtin_global_project_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();

        // 글로벌
        let global_dir = dir.path().join("global");
        std::fs::create_dir_all(&global_dir).unwrap();
        let global_prompt = global_prompt_path(&global_dir);
        if let Some(parent) = global_prompt.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&global_prompt, "글로벌 지시 {{cwd}}").unwrap();

        // 프로젝트
        std::fs::create_dir_all(root.join(".bulti")).unwrap();
        std::fs::write(project_prompt_path(&root), "프로젝트 지시").unwrap();

        let ctx = test_ctx(&root, &global_dir);
        let out = assemble(&ctx, None).unwrap();

        // 계층 병합 순서 검증.
        assert!(out.contains("불티(Bulti) — 로컬 LLM 코딩 에이전트")); // 빌트인
        assert!(out.contains("글로벌 지시")); // 글로벌
        assert!(out.contains("프로젝트 지시")); // 프로젝트
        assert!(out.contains(INDEX_HEADER)); // 인덱스

        // 변수 치환.
        assert!(out.contains(&format!("글로벌 지시 {}", root.display())));

        // 순서: 빌트인 → 글로벌 → 프로젝트 → 인덱스.
        let base_pos = out.find("불티(Bulti)").unwrap();
        let global_pos = out.find("글로벌 지시").unwrap();
        let proj_pos = out.find("프로젝트 지시").unwrap();
        let idx_pos = out.find(INDEX_HEADER).unwrap();
        assert!(base_pos < global_pos);
        assert!(global_pos < proj_pos);
        assert!(proj_pos < idx_pos);
    }

    #[test]
    fn merge_skips_missing_layers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let global_dir = dir.path().join("global");
        std::fs::create_dir_all(&global_dir).unwrap();
        // 글로벌·프로젝트 파일을 만들지 않음 → 빌트인 + 인덱스만.
        let ctx = test_ctx(&root, &global_dir);
        let out = assemble(&ctx, None).unwrap();
        assert!(out.contains("불티(Bulti)"));
        assert!(!out.contains("글로벌 지시"));
        assert!(!out.contains("프로젝트 지시"));
        assert!(out.contains(INDEX_HEADER));
    }

    #[test]
    fn inline_override_replaces_all_keeps_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let global_dir = dir.path().join("global");
        std::fs::create_dir_all(&global_dir).unwrap();
        let global_prompt = global_prompt_path(&global_dir);
        if let Some(parent) = global_prompt.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&global_prompt, "글로벌 지시").unwrap();
        std::fs::create_dir_all(root.join(".bulti")).unwrap();
        std::fs::write(project_prompt_path(&root), "프로젝트 지시").unwrap();

        let ctx = test_ctx(&root, &global_dir);
        let out = assemble(&ctx, Some(Override::Inline("완전 교체 {{cwd}}".to_string()))).unwrap();

        // 빌트인·글로벌·프로젝트 무시.
        assert!(!out.contains("불티(Bulti)"));
        assert!(!out.contains("글로벌 지시"));
        assert!(!out.contains("프로젝트 지시"));
        // 교체 본문 + 변수 치환 + 인덱스 유지.
        assert!(out.contains(&format!("완전 교체 {}", root.display())));
        assert!(out.contains(INDEX_HEADER));
    }

    #[test]
    fn file_override_replaces_all_keeps_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let global_dir = dir.path().join("global");
        std::fs::create_dir_all(&global_dir).unwrap();
        let ctx = test_ctx(&root, &global_dir);

        let file = dir.path().join("custom.md");
        std::fs::write(&file, "파일 교체 {{os}}").unwrap();
        let out = assemble(&ctx, Some(Override::File(file.clone()))).unwrap();

        assert!(!out.contains("불티(Bulti)"));
        assert!(out.contains(&format!("파일 교체 {}", std::env::consts::OS)));
        assert!(out.contains(INDEX_HEADER));
    }

    #[test]
    fn file_override_missing_errors() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let global_dir = dir.path().join("global");
        std::fs::create_dir_all(&global_dir).unwrap();
        let ctx = test_ctx(&root, &global_dir);
        let missing = dir.path().join("nope.md");
        let res = assemble(&ctx, Some(Override::File(missing)));
        assert!(res.is_err());
    }

    #[test]
    fn template_values_from_context() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let ctx = test_ctx(&root, dir.path());
        let v = template_values(&ctx);
        assert_eq!(v.cwd, root.display().to_string());
        assert_eq!(v.endpoint, "main");
        assert_eq!(v.model, "qwen3");
        assert_eq!(v.context_tokens, "16384");
        assert!(!v.os.is_empty());
    }

    #[test]
    fn context_from_config_no_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::new();
        let ctx = context_from_config(&cfg, dir.path().to_path_buf(), dir.path().to_path_buf(), vec![], vec![])
            .unwrap();
        assert_eq!(ctx.endpoint, "(없음)");
        assert!(ctx.endpoint_config.is_none());
    }

    #[test]
    fn edit_creates_global_file() {
        let dir = tempfile::tempdir().unwrap();
        let global_dir = dir.path().join("global");
        std::fs::create_dir_all(&global_dir).unwrap();
        // prepare_global_file 로 파일 생성·준비만 검증.
        let path = global_prompt_path(&global_dir);
        assert!(!path.exists());
        let prepared = prepare_global_file(&global_dir).unwrap();
        assert_eq!(prepared, path);
        assert!(path.exists());
    }
}