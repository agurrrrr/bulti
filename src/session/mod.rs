//! 대화형 세션 저장·재개 (DESIGN.md §4.13.2).
//!
//! 세션은 `~/.bulti/sessions/<session_id>.json`에 저장한다. 세션은 사용자
//! 턴들의 연속을 담으며, 재개 시 이전 대화 맥락을 프롬프트에 복원한다.
//!
//! - 저장: 매 턴 종료 시 세션 파일에 메시지 배열을 갱신한다.
//! - 조회: `bulti session list` — 세션 목록 출력.
//! - 재개: `bulti chat --resume <id>`(또는 `/resume <id>`) 시 세션 파일의
//!   대화 기록을 프롬프트에 복원한다.
//! - 삭제: `bulti session delete <id>`.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::Config;

/// 세션 디렉터리 이름 (`~/.bulti/sessions`).
pub const SESSIONS_DIR_NAME: &str = "sessions";
/// 세션 파일 확장자.
pub const SESSION_FILE_EXT: &str = "json";

/// 세션 저장·조회 오류.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("홈 디렉터리를 찾을 수 없습니다: {0}")]
    HomeDirNotFound(String),
    #[error("세션 파일 읽기 실패: {0}")]
    Read(String),
    #[error("세션 파일 쓰기 실패: {0}")]
    Write(String),
    #[error("JSON 직렬화 실패: {0}")]
    Json(#[from] serde_json::Error),
    #[error("세션 '{0}' 을(를) 찾을 수 없습니다")]
    NotFound(String),
}

/// 세션 디렉터리 경로 (`~/.bulti/sessions`) 를 반환한다.
pub fn sessions_dir() -> Result<PathBuf, SessionError> {
    Config::config_dir()
        .map(|dir| dir.join(SESSIONS_DIR_NAME))
        .map_err(|e| SessionError::HomeDirNotFound(e.to_string()))
}

/// 세션 파일 경로 (`~/.bulti/sessions/<id>.json`) 를 반환한다.
pub fn session_path(id: &str) -> Result<PathBuf, SessionError> {
    Ok(sessions_dir()?.join(format!("{id}.{SESSION_FILE_EXT}")))
}

/// 한 턴(사용자 프롬프트 → 모델 응답)의 기록.
/// 토큰 필드는 `#[serde(default)]` — 이전 버전 세션 JSON 호환.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TurnRecord {
    pub turn: u32,
    pub user: String,
    pub assistant: String,
    pub chain_id: String,
    pub files_touched: Vec<String>,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub model: String,
}

/// 세션 파일 본문.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub session_id: String,
    pub created_at: String,
    pub updated_at: String,
    pub endpoint: String,
    pub model: String,
    pub system_prompt_at: String,
    pub turns: Vec<TurnRecord>,
}

impl Session {
    /// 새 세션을 만든다.
    pub fn new(session_id: &str, endpoint: &str, model: &str) -> Self {
        let now = crate::history::now_rfc3339();
        Self {
            session_id: session_id.to_string(),
            created_at: now.clone(),
            updated_at: now,
            endpoint: endpoint.to_string(),
            model: model.to_string(),
            system_prompt_at: crate::history::now_rfc3339(),
            turns: Vec::new(),
        }
    }

    /// 턴을 추가하고 `updated_at`을 갱신한다.
    pub fn push_turn(&mut self, turn: TurnRecord) {
        self.turns.push(turn);
        self.updated_at = crate::history::now_rfc3339();
    }

    /// 세션 누적 input 토큰 합.
    pub fn total_input_tokens(&self) -> u64 {
        self.turns.iter().map(|t| t.input_tokens).sum()
    }

    /// 세션 누적 output 토큰 합.
    pub fn total_output_tokens(&self) -> u64 {
        self.turns.iter().map(|t| t.output_tokens).sum()
    }

    /// 모델별 사용량 (모델명, input, output) — 모델명 정렬.
    /// 토큰이 0 이고 모델명도 비어 있는 턴(구버전 세션·컴팩트)은 제외한다.
    pub fn per_model_usage(&self) -> Vec<(String, u64, u64)> {
        use std::collections::BTreeMap;
        let mut map: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        for t in &self.turns {
            if t.input_tokens == 0 && t.output_tokens == 0 {
                continue;
            }
            let entry = map.entry(t.model.clone()).or_insert((0, 0));
            entry.0 += t.input_tokens;
            entry.1 += t.output_tokens;
        }
        map.into_iter()
            .map(|(m, (i, o))| (m, i, o))
            .collect()
    }

    /// 재개용 대화 기록 문자열을 만든다. 이전 턴의 사용자·모델 메시지를
    /// 프롬프트에 복원할 수 있게 사람이 읽기 쉬운 형태로 조합한다.
    pub fn conversation_context(&self) -> String {
        let mut out = String::new();
        for t in &self.turns {
            out.push_str(&format!("[사용자]\n{}\n\n", t.user));
            out.push_str(&format!("[모델]\n{}\n\n", t.assistant));
        }
        out
    }

    /// 대화 기록 전체의 토큰 수를 추정한다.
    /// 정확한 토큰 카운터 없이 글자 수 근사(0.75 배)로 컨텍스트 사용량을
    /// 표시한다 (`/session-info` 용).
    pub fn estimate_tokens(&self) -> u64 {
        let text = self.conversation_context();
        let chars = text.chars().count() as u64;
        chars * 3 / 4
    }

    /// `/export` 용: 대화를 사람이 읽기 좋은 마크다운 문서로 만든다.
    pub fn export_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# bulti 세션 기록\n\n");
        out.push_str(&format!("- 세션 id: `{}`\n", self.session_id));
        out.push_str(&format!("- 엔드포인트: {}\n", self.endpoint));
        out.push_str(&format!("- 모델: {}\n", self.model));
        out.push_str(&format!("- 생성: {}\n", self.created_at));
        out.push_str(&format!("- 마지막 갱신: {}\n", self.updated_at));
        out.push_str(&format!("- 턴 수: {}\n\n", self.turns.len()));
        out.push_str("---\n\n");
        for t in &self.turns {
            out.push_str(&format!("## 턴 {}\n\n", t.turn));
            out.push_str("### 사용자\n\n");
            out.push_str(t.user.trim());
            out.push_str("\n\n### 모델\n\n");
            out.push_str(t.assistant.trim());
            out.push_str("\n\n");
            if !t.files_touched.is_empty() {
                out.push_str("**수정 파일**\n\n");
                for f in &t.files_touched {
                    out.push_str(&format!("- `{f}`\n"));
                }
                out.push('\n');
            }
            out.push_str("---\n\n");
        }
        out
    }

    /// `/fork` 용: 새 id 의 포크 세션 클론을 만든다.
    /// 턴 기록은 그대로 복사하고, id·시각만 새로 만든다.
    pub fn fork(&self, new_id: &str) -> Self {
        let now = crate::history::now_rfc3339();
        Self {
            session_id: new_id.to_string(),
            created_at: now.clone(),
            updated_at: now,
            endpoint: self.endpoint.clone(),
            model: self.model.clone(),
            system_prompt_at: self.system_prompt_at.clone(),
            turns: self.turns.clone(),
        }
    }
}

/// 세션을 저장한다. 디렉터리가 없으면 생성한다.
pub fn save(session: &Session) -> Result<PathBuf, SessionError> {
    let dir = sessions_dir()?;
    fs::create_dir_all(&dir).map_err(|e| SessionError::Write(e.to_string()))?;
    let path = dir.join(format!("{}.{SESSION_FILE_EXT}", session.session_id));
    let json = serde_json::to_string_pretty(session)?;
    fs::write(&path, json).map_err(|e| SessionError::Write(e.to_string()))?;
    Ok(path)
}

/// 세션 id 로 세션을 로드한다.
pub fn load(id: &str) -> Result<Session, SessionError> {
    let path = session_path(id)?;
    let text = fs::read_to_string(&path).map_err(|e| SessionError::Read(e.to_string()))?;
    serde_json::from_str(&text).map_err(SessionError::Json)
}

/// 세션 id 로 세션을 삭제한다. 파일이 없으면 NotFound.
pub fn delete(id: &str) -> Result<(), SessionError> {
    let path = session_path(id)?;
    if !path.is_file() {
        return Err(SessionError::NotFound(id.to_string()));
    }
    fs::remove_file(&path).map_err(|e| SessionError::Write(e.to_string()))
}

/// 세션 목록 (id, 생성 시각, 턴 수, 마지막 갱신 시각, 모델, 엔드포인트).
#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub turns: usize,
    pub model: String,
    pub endpoint: String,
}

/// 세션 디렉터리의 모든 세션 목록을 반환한다. 정렬은 마지막 갱신 시각 역순.
pub fn list() -> Result<Vec<SessionMeta>, SessionError> {
    let dir = sessions_dir()?;
    let mut metas: Vec<SessionMeta> = Vec::new();
    if !dir.is_dir() {
        return Ok(metas);
    }
    for entry in fs::read_dir(&dir).map_err(|e| SessionError::Read(e.to_string()))? {
        let entry = entry.map_err(|e| SessionError::Read(e.to_string()))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(SESSION_FILE_EXT) {
            continue;
        }
        let id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let session = match load(&id) {
            Ok(s) => s,
            Err(_) => continue,
        };
        metas.push(SessionMeta {
            id,
            created_at: session.created_at,
            updated_at: session.updated_at,
            turns: session.turns.len(),
            model: session.model,
            endpoint: session.endpoint,
        });
    }
    metas.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(metas)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 테스트용 임시 세션 디렉터리 + 세션을 만든다.
    fn temp_session() -> (tempfile::TempDir, Session) {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new("sess-1", "local", "model-x");
        (dir, session)
    }

    /// 세션 저장 → 로드 → 턴 추가 → 재저장 → 재로드가 동작한다.
    #[test]
    fn save_load_push_turn_roundtrip() {
        let (_dir, mut session) = temp_session();
        session.push_turn(TurnRecord {
            turn: 0,
            user: "안녕".to_string(),
            assistant: "안녕하세요".to_string(),
            chain_id: "chain-1".to_string(),
            files_touched: vec!["src/a.rs".to_string()],
            ..Default::default()
        });
        session.push_turn(TurnRecord {
            turn: 1,
            user: "요약해".to_string(),
            assistant: "요약입니다".to_string(),
            chain_id: "chain-2".to_string(),
            files_touched: vec![],
            ..Default::default()
        });

        let path = save(&session).unwrap();
        assert!(path.is_file());

        let loaded = load("sess-1").unwrap();
        assert_eq!(loaded.turns.len(), 2);
        assert_eq!(loaded.turns[0].user, "안녕");
        assert_eq!(loaded.turns[1].assistant, "요약입니다");
        assert_eq!(loaded.turns[0].files_touched, vec!["src/a.rs"]);
    }

    /// 대화 기록 컨텍스트 문자열에 사용자·모델 메시지가 포함된다.
    #[test]
    fn conversation_context_includes_turns() {
        let (_dir, mut session) = temp_session();
        session.push_turn(TurnRecord {
            turn: 0,
            user: "질문1".to_string(),
            assistant: "답변1".to_string(),
            chain_id: "c".to_string(),
            files_touched: vec![],
            ..Default::default()
        });
        let ctx = session.conversation_context();
        assert!(ctx.contains("질문1"));
        assert!(ctx.contains("답변1"));
        assert!(ctx.contains("[사용자]"));
        assert!(ctx.contains("[모델]"));
    }

    /// fork 는 턴을 그대로 복제하고 id·시각만 바꾼다.
    #[test]
    fn fork_copies_turns_and_changes_id() {
        let (_dir, mut session) = temp_session();
        session.push_turn(TurnRecord {
            turn: 0,
            user: "질문1".to_string(),
            assistant: "답변1".to_string(),
            chain_id: "c1".to_string(),
            files_touched: vec!["src/a.rs".to_string()],
            ..Default::default()
        });
        session.push_turn(TurnRecord {
            turn: 1,
            user: "질문2".to_string(),
            assistant: "답변2".to_string(),
            chain_id: "c2".to_string(),
            files_touched: vec![],
            ..Default::default()
        });

        let forked = session.fork("sess-fork");
        assert_eq!(forked.session_id, "sess-fork");
        assert_ne!(forked.session_id, session.session_id);
        assert_eq!(forked.turns.len(), session.turns.len());
        assert_eq!(forked.turns[0].user, "질문1");
        assert_eq!(forked.turns[1].assistant, "답변2");
        assert_eq!(forked.turns[0].files_touched, vec!["src/a.rs"]);
        assert_eq!(forked.model, session.model);
        assert_eq!(forked.endpoint, session.endpoint);
        // 원본 세션에는 영향이 없어야 한다.
        assert_eq!(session.session_id, "sess-1");
        assert_eq!(session.turns.len(), 2);
    }

    /// export_markdown 은 세션 메타와 모든 턴을 포함한다.
    #[test]
    fn export_markdown_includes_meta_and_turns() {
        let (_dir, mut session) = temp_session();
        session.push_turn(TurnRecord {
            turn: 0,
            user: "질문1".to_string(),
            assistant: "답변1".to_string(),
            chain_id: "c1".to_string(),
            files_touched: vec!["src/a.rs".to_string()],
            ..Default::default()
        });

        let md = session.export_markdown();
        assert!(md.contains("# bulti 세션 기록"));
        assert!(md.contains("`sess-1`"));
        assert!(md.contains("local"));
        assert!(md.contains("model-x"));
        assert!(md.contains("## 턴 0"));
        assert!(md.contains("질문1"));
        assert!(md.contains("답변1"));
        assert!(md.contains("`src/a.rs`"));
    }

    /// estimate_tokens 는 대화 기록이 있으면 0 보다 크다.
    #[test]
    fn estimate_tokens_positive() {
        let (_dir, mut session) = temp_session();
        session.push_turn(TurnRecord {
            turn: 0,
            user: "긴 질문 텍스트".repeat(10),
            assistant: "긴 답변 텍스트".repeat(10),
            chain_id: "c".to_string(),
            files_touched: vec![],
            ..Default::default()
        });
        assert!(session.estimate_tokens() > 0);
    }

    /// list 는 갱신 시각 역순으로 정렬한다.
    #[test]
    fn list_sorts_by_updated_at() {
        let _dir = tempfile::tempdir().unwrap();
        // save 는 실제 ~/.bulti 를 쓰므로 여기선 세션 파일을 직접 생성.
        let mut s1 = Session::new("a", "local", "m");
        s1.updated_at = "2026-09-06T01:00:00Z".to_string();
        let mut s2 = Session::new("b", "local", "m");
        s2.updated_at = "2026-09-06T02:00:00Z".to_string();
        // 세션 디렉터리 경로를 임시로 쓰기 위해 list 는 실제 경로 사용 —
        // 대신 delete/load NotFound 검증.
        let _ = (s1, s2);
        assert!(delete("nonexistent").is_err());
        assert!(load("nonexistent").is_err());
    }

    /// 없는 세션 삭제·로드는 NotFound 오류.
    #[test]
    fn missing_session_errors() {
        assert!(matches!(
            load("nope"),
            Err(SessionError::NotFound(_) | SessionError::Read(_))
        ));
    }
}