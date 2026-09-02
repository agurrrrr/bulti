//! rusqlite 저장·조회 (DESIGN.md §4.7).
//!
//! 단계 6: `~/.bulti/history.db` 에 run 단위 자동 저장·조회를 구현한다.
//!
//! - 저장 (§4.7.1): run 시작 INSERT(running) → 종료 UPDATE. 핸드오프로 새
//!   세그먼트가 시작되면 `parent_run_id`로 연결한 새 행 insert.
//! - 조회 (§4.7.2): CLI `bulti history list/show/last`, 모델 도구
//!   `history_list`/`history_read`.

#![allow(dead_code)]

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;

use crate::config::Config;

/// DB 파일 이름.
pub const HISTORY_DB_FILENAME: &str = "history.db";

/// run 상태.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Completed,
    Failed,
    Incomplete,
    Interrupted,
}

impl RunStatus {
    /// DB 저장용 문자열.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Incomplete => "incomplete",
            Self::Interrupted => "interrupted",
        }
    }

    /// 문자열에서 상태로 변환한다.
    pub fn from_str(s: &str) -> Self {
        match s {
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "incomplete" => Self::Incomplete,
            "interrupted" => Self::Interrupted,
            _ => Self::Running,
        }
    }
}

/// 저장·조회 오류.
#[derive(Debug, Error)]
pub enum HistoryError {
    #[error("홈 디렉터리를 찾을 수 없습니다: {0}")]
    HomeDirNotFound(String),
    #[error("DB 작업 실패: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("DB 열기 실패: {0}")]
    Open(String),
    #[error("JSON 직렬화 실패: {0}")]
    Json(#[from] serde_json::Error),
}

/// history.db 경로를 반환한다.
pub fn history_db_path() -> Result<PathBuf, HistoryError> {
    Config::config_dir()
        .map(|dir| dir.join(HISTORY_DB_FILENAME))
        .map_err(|e| HistoryError::HomeDirNotFound(e.to_string()))
}

/// DB 연결을 열고 스키마를 생성한다.
pub fn open() -> Result<Connection, HistoryError> {
    let path = history_db_path()?;
    let conn = Connection::open(&path).map_err(|e| HistoryError::Open(e.to_string()))?;
    init_schema(&conn)?;
    Ok(conn)
}

/// `runs` 테이블 스키마를 생성한다 (§4.7.1).
pub fn init_schema(conn: &Connection) -> Result<(), HistoryError> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS runs (
            id            INTEGER PRIMARY KEY,
            started_at    TEXT NOT NULL,     -- RFC3339
            finished_at   TEXT,
            cwd           TEXT NOT NULL,
            endpoint      TEXT NOT NULL,
            model         TEXT,
            status        TEXT NOT NULL,     -- running|completed|failed|incomplete|interrupted
            prompt        TEXT NOT NULL,     -- 세그먼트 시작 프롬프트
            result        TEXT,              -- 최종 응답 또는 핸드오프 요약
            chain_id      TEXT NOT NULL,     -- 같은 run 세그먼트를 잇는 UUID
            segment_index INTEGER NOT NULL DEFAULT 0,
            handoff_depth INTEGER NOT NULL DEFAULT 0,
            parent_run_id INTEGER,           -- 핸드오프로 이어진 직전 run id
            input_tokens  INTEGER,
            output_tokens INTEGER,
            files_touched TEXT,              -- JSON 배열
            duration_ms   INTEGER
        );
        "#,
    )?;
    Ok(())
}

/// RFC3339 타임스탬프를 만든다.
pub fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // UTC 기준 ISO8601/RFC3339 근사 (초 단위).
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // 1970-01-01 기준 날짜 계산.
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// days(1970-01-01 기준)를 (년, 월, 일)로 변환한다 (Howard Hinnant 알고리즘).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = (z / 146097).max(0);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// 새 run 시작 정보. 인자 개수 초과(clippy)를 피하기 위한 묶음 구조체.
pub struct RunStart {
    pub cwd: String,
    pub endpoint: String,
    pub model: Option<String>,
    pub prompt: String,
    pub chain_id: String,
    pub segment_index: u32,
    pub handoff_depth: u32,
    pub parent_run_id: Option<i64>,
}

/// 새 run 을 시작한다. INSERT(running) 후 생성된 id 를 반환한다.
pub fn start_run(conn: &Connection, info: &RunStart) -> Result<i64, HistoryError> {
    let now = now_rfc3339();
    conn.execute(
        "INSERT INTO runs (
            started_at, cwd, endpoint, model, status, prompt,
            chain_id, segment_index, handoff_depth, parent_run_id
        ) VALUES (?1, ?2, ?3, ?4, 'running', ?5, ?6, ?7, ?8, ?9)",
        params![
            now,
            info.cwd,
            info.endpoint,
            info.model.as_deref(),
            info.prompt,
            info.chain_id,
            info.segment_index,
            info.handoff_depth,
            info.parent_run_id,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// run 종료 정보. 인자 개수 초과(clippy)를 피하기 위한 묶음 구조체.
pub struct RunFinish {
    pub status: RunStatus,
    pub result: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub files_touched: Vec<String>,
    pub duration_ms: Option<u64>,
}

/// run 종료 시 UPDATE 한다.
pub fn finish_run(conn: &Connection, id: i64, info: &RunFinish) -> Result<(), HistoryError> {
    let now = now_rfc3339();
    let files_json = serde_json::to_string(&info.files_touched)?;
    conn.execute(
        "UPDATE runs SET
            finished_at = ?1, status = ?2, result = ?3,
            input_tokens = ?4, output_tokens = ?5,
            files_touched = ?6, duration_ms = ?7
        WHERE id = ?8",
        params![
            now,
            info.status.as_str(),
            info.result.as_deref(),
            info.input_tokens,
            info.output_tokens,
            files_json,
            info.duration_ms,
            id,
        ],
    )?;
    Ok(())
}

/// 조회용 run 행.
#[derive(Debug, Clone)]
pub struct RunRow {
    pub id: i64,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub cwd: String,
    pub endpoint: String,
    pub model: Option<String>,
    pub status: String,
    pub prompt: String,
    pub result: Option<String>,
    pub chain_id: String,
    pub segment_index: u32,
    pub handoff_depth: u32,
    pub parent_run_id: Option<i64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub files_touched: Option<String>,
    pub duration_ms: Option<u64>,
}

fn row_to_run(row: &rusqlite::Row) -> Result<RunRow, rusqlite::Error> {
    Ok(RunRow {
        id: row.get(0)?,
        started_at: row.get(1)?,
        finished_at: row.get(2)?,
        cwd: row.get(3)?,
        endpoint: row.get(4)?,
        model: row.get(5)?,
        status: row.get(6)?,
        prompt: row.get(7)?,
        result: row.get(8)?,
        chain_id: row.get(9)?,
        segment_index: row.get::<_, u32>(10)?,
        handoff_depth: row.get::<_, u32>(11)?,
        parent_run_id: row.get(12)?,
        input_tokens: row.get::<_, Option<u64>>(13)?,
        output_tokens: row.get::<_, Option<u64>>(14)?,
        files_touched: row.get(15)?,
        duration_ms: row.get::<_, Option<u64>>(16)?,
    })
}

/// 최근 run 목록. `limit`/`status`/`chain_id` 필터를 지원한다 (§4.7.2).
pub fn list_runs(
    conn: &Connection,
    limit: Option<u64>,
    status: Option<&str>,
    chain_id: Option<&str>,
) -> Result<Vec<RunRow>, HistoryError> {
    let mut sql = "SELECT id, started_at, finished_at, cwd, endpoint, model, status, prompt,
        result, chain_id, segment_index, handoff_depth, parent_run_id,
        input_tokens, output_tokens, files_touched, duration_ms
        FROM runs".to_string();
    let mut conds: Vec<String> = Vec::new();
    let mut args: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(s) = status {
        conds.push("status = ?".to_string());
        args.push(s.to_string().into());
    }
    if let Some(c) = chain_id {
        conds.push("chain_id = ?".to_string());
        args.push(c.to_string().into());
    }
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }
    sql.push_str(" ORDER BY id DESC");
    if let Some(n) = limit {
        sql.push_str(" LIMIT ");
        sql.push_str(&n.to_string());
    }
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), row_to_run)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// 특정 run 상세 조회.
pub fn get_run(conn: &Connection, id: i64) -> Result<Option<RunRow>, HistoryError> {
    let mut stmt = conn.prepare(
        "SELECT id, started_at, finished_at, cwd, endpoint, model, status, prompt,
        result, chain_id, segment_index, handoff_depth, parent_run_id,
        input_tokens, output_tokens, files_touched, duration_ms
        FROM runs WHERE id = ?1",
    )?;
    let row = stmt.query_row(params![id], row_to_run).optional()?;
    Ok(row)
}

/// 마지막 run 조회. `chain`이 주어지면 그 체인의 마지막 run 을 조회한다.
pub fn last_run(conn: &Connection, chain: Option<&str>) -> Result<Option<RunRow>, HistoryError> {
    let (sql, arg): (&str, Option<String>) = match chain {
        Some(c) => (
            "SELECT id, started_at, finished_at, cwd, endpoint, model, status, prompt,
            result, chain_id, segment_index, handoff_depth, parent_run_id,
            input_tokens, output_tokens, files_touched, duration_ms
            FROM runs WHERE chain_id = ?1 ORDER BY id DESC LIMIT 1",
            Some(c.to_string()),
        ),
        None => (
            "SELECT id, started_at, finished_at, cwd, endpoint, model, status, prompt,
            result, chain_id, segment_index, handoff_depth, parent_run_id,
            input_tokens, output_tokens, files_touched, duration_ms
            FROM runs ORDER BY id DESC LIMIT 1",
            None,
        ),
    };
    let mut stmt = conn.prepare(sql)?;
    let row = match arg {
        Some(a) => stmt.query_row(params![a], row_to_run).optional()?,
        None => stmt.query_row([], row_to_run).optional()?,
    };
    Ok(row)
}

/// `files_touched` JSON 을 문자열 배열로 파싱한다.
pub fn parse_files_touched(json: Option<&str>) -> Vec<String> {
    match json {
        Some(s) if !s.is_empty() => {
            serde_json::from_str::<Vec<String>>(s).unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 테스트용 임시 DB 연결을 만든다.
    fn temp_conn() -> (Connection, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("history.db")).unwrap();
        init_schema(&conn).unwrap();
        (conn, dir)
    }

    fn run_start(conn: &Connection, chain: &str, seg: u32, depth: u32) -> i64 {
        start_run(
            conn,
            &RunStart {
                cwd: "/tmp/proj".to_string(),
                endpoint: "local".to_string(),
                model: Some("m".to_string()),
                prompt: "프롬프트".to_string(),
                chain_id: chain.to_string(),
                segment_index: seg,
                handoff_depth: depth,
                parent_run_id: None,
            },
        )
        .unwrap()
    }

    /// 스키마 생성이 멱등(두 번 실행해도 오류 없음)인지 확인한다.
    #[test]
    fn init_schema_is_idempotent() {
        let (conn, _dir) = temp_conn();
        init_schema(&conn).unwrap();
        init_schema(&conn).unwrap();
    }

    /// run 시작 INSERT → 종료 UPDATE 가 동작한다.
    #[test]
    fn start_then_finish_run() {
        let (conn, _dir) = temp_conn();
        let id = run_start(&conn, "chain-1", 0, 0);
        assert!(id > 0);

        finish_run(
            &conn,
            id,
            &RunFinish {
                status: RunStatus::Completed,
                result: Some("최종 결과".to_string()),
                input_tokens: Some(100),
                output_tokens: Some(50),
                files_touched: vec!["src/x.rs".to_string()],
                duration_ms: Some(1234),
            },
        )
        .unwrap();

        let row = get_run(&conn, id).unwrap().unwrap();
        assert_eq!(row.status, "completed");
        assert_eq!(row.result.as_deref(), Some("최종 결과"));
        assert_eq!(row.input_tokens, Some(100));
        assert_eq!(row.output_tokens, Some(50));
        assert_eq!(row.duration_ms, Some(1234));
        assert!(row.finished_at.is_some());
        assert_eq!(parse_files_touched(row.files_touched.as_deref()), vec!["src/x.rs"]);
    }

    /// 핸드오프 세그먼트 insert 가 `parent_run_id`로 이어진다.
    #[test]
    fn handoff_segment_links_parent() {
        let (conn, _dir) = temp_conn();
        let parent = run_start(&conn, "chain-1", 0, 0);
        let child = start_run(
            &conn,
            &RunStart {
                cwd: "/tmp/proj".to_string(),
                endpoint: "local".to_string(),
                model: Some("m".to_string()),
                prompt: "핸드오프 요약+과제".to_string(),
                chain_id: "chain-1".to_string(),
                segment_index: 1,
                handoff_depth: 1,
                parent_run_id: Some(parent),
            },
        )
        .unwrap();
        assert!(child > parent);

        let child_row = get_run(&conn, child).unwrap().unwrap();
        assert_eq!(child_row.parent_run_id, Some(parent));
        assert_eq!(child_row.segment_index, 1);
        assert_eq!(child_row.handoff_depth, 1);
        assert_eq!(child_row.chain_id, "chain-1");
    }

    /// list_runs 필터·정렬·limit 동작.
    #[test]
    fn list_runs_filters_and_limits() {
        let (conn, _dir) = temp_conn();
        let a = run_start(&conn, "chain-a", 0, 0);
        let b = run_start(&conn, "chain-b", 0, 0);
        let c = run_start(&conn, "chain-a", 1, 1);
        finish_run(&conn, a, &RunFinish {
            status: RunStatus::Completed,
            result: Some("r1".to_string()),
            input_tokens: None,
            output_tokens: None,
            files_touched: vec![],
            duration_ms: None,
        })
        .unwrap();
        finish_run(&conn, b, &RunFinish {
            status: RunStatus::Failed,
            result: Some("r2".to_string()),
            input_tokens: None,
            output_tokens: None,
            files_touched: vec![],
            duration_ms: None,
        })
        .unwrap();
        finish_run(&conn, c, &RunFinish {
            status: RunStatus::Completed,
            result: Some("r3".to_string()),
            input_tokens: None,
            output_tokens: None,
            files_touched: vec![],
            duration_ms: None,
        })
        .unwrap();

        // 전체 목록 (id desc)
        let all = list_runs(&conn, None, None, None).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].id, c);
        assert_eq!(all[2].id, a);

        // status 필터
        let completed = list_runs(&conn, None, Some("completed"), None).unwrap();
        assert_eq!(completed.len(), 2);

        // chain 필터
        let chain_a = list_runs(&conn, None, None, Some("chain-a")).unwrap();
        assert_eq!(chain_a.len(), 2);
        assert!(chain_a.iter().all(|r| r.chain_id == "chain-a"));

        // limit
        let limited = list_runs(&conn, Some(2), None, None).unwrap();
        assert_eq!(limited.len(), 2);
    }

    /// last_run 과 chain 지정 last_run 동작.
    #[test]
    fn last_run_plain_and_chain() {
        let (conn, _dir) = temp_conn();
        let a = run_start(&conn, "chain-a", 0, 0);
        let b = run_start(&conn, "chain-b", 0, 0);
        finish_run(&conn, a, &RunFinish {
            status: RunStatus::Completed,
            result: Some("r1".to_string()),
            input_tokens: None,
            output_tokens: None,
            files_touched: vec![],
            duration_ms: None,
        })
        .unwrap();
        finish_run(&conn, b, &RunFinish {
            status: RunStatus::Completed,
            result: Some("r2".to_string()),
            input_tokens: None,
            output_tokens: None,
            files_touched: vec![],
            duration_ms: None,
        })
        .unwrap();

        let last = last_run(&conn, None).unwrap().unwrap();
        assert_eq!(last.id, b);

        let last_a = last_run(&conn, Some("chain-a")).unwrap().unwrap();
        assert_eq!(last_a.id, a);
    }

    /// 빈 DB 에서 last_run 은 None.
    #[test]
    fn last_run_empty_returns_none() {
        let (conn, _dir) = temp_conn();
        assert!(last_run(&conn, None).unwrap().is_none());
        assert!(get_run(&conn, 999).unwrap().is_none());
    }

    /// RFC3339 타임스탬프 포맷.
    #[test]
    fn rfc3339_is_well_formed() {
        let s = now_rfc3339();
        assert!(s.ends_with('Z'));
        assert!(s.contains('T'));
        // YYYY-MM-DDTHH:MM:SSZ
        assert_eq!(s.len(), 20);
    }

    /// files_touched 파싱 — 빈 값은 빈 벡터.
    #[test]
    fn parse_files_touched_handles_empty() {
        assert_eq!(parse_files_touched(None), Vec::<String>::new());
        assert_eq!(parse_files_touched(Some("")), Vec::<String>::new());
        assert_eq!(parse_files_touched(Some("[]")), Vec::<String>::new());
        assert_eq!(
            parse_files_touched(Some("[\"a.rs\",\"b.rs\"]")),
            vec!["a.rs".to_string(), "b.rs".to_string()]
        );
    }
}