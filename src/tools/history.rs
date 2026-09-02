//! `history_list`/`history_read` 모델 도구 (DESIGN.md §4.7.2).
//!
//! 레이지 정신의 확장: 세션 재사용이 없으므로 모델이 "이전 작업 이어서" 같은
//! 지시를 받으면 이 도구들로 스스로 맥락을 회수한다.
//! 시스템 프롬프트에는 도구 존재만 한 줄로 안내된다.

use crate::history;
use crate::tools::util::{int_arg, str_arg};
use crate::tools::{ToolHandler, ToolRegistry};

/// history_list 도구 스키마.
pub fn list_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "프롬프트/결과 부분일치 검색어 (선택)"
            },
            "limit": {
                "type": "integer",
                "description": "최대 조회 개수 (선택, 기본 10)"
            }
        }
    })
}

/// history_read 도구 스키마.
pub fn read_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "run_id": {
                "type": "integer",
                "description": "조회할 run id"
            }
        },
        "required": ["run_id"]
    })
}

/// history_list 도구를 레지스트리에 등록한다.
pub fn register_list(reg: &mut ToolRegistry) {
    let handler: ToolHandler = std::sync::Arc::new(|args| {
        Box::pin(async move {
            let query = str_arg(&args, "query").unwrap_or_default();
            let limit = int_arg(&args, "limit", 10);
            let conn = match history::open() {
                Ok(c) => c,
                Err(e) => return Err(e.to_string()),
            };
            // query 가 있으면 프롬프트/결과 부분일치 필터를 적용한다.
            let rows = match history::list_runs(&conn, Some(limit), None, None) {
                Ok(r) => r,
                Err(e) => return Err(e.to_string()),
            };
            let filtered: Vec<_> = if query.is_empty() {
                rows
            } else {
                let q = query.as_str();
                rows.into_iter()
                    .filter(|r| r.prompt.contains(q) || r.result.as_deref().unwrap_or("").contains(q))
                    .collect()
            };
            let mut out = String::new();
            for r in &filtered {
                out.push_str(&format!(
                    "#{} [{}] {} | {} | {} | seg {} | depth {}\n",
                    r.id,
                    r.status,
                    &r.started_at[..19],
                    r.endpoint,
                    r.model.as_deref().unwrap_or("-"),
                    r.segment_index,
                    r.handoff_depth,
                ));
                out.push_str(&format!("  prompt: {}\n", truncate(&r.prompt, 120)));
            }
            if out.is_empty() {
                out.push_str("기록된 작업이 없습니다.");
            }
            Ok(out)
        })
    });
    reg.register(
        "history_list",
        "이전 작업 히스토리를 조회합니다. '이전 작업 이어서' 같은 지시를 받으면 이 도구로 맥락을 회수하세요.",
        list_schema(),
        handler,
    );
}

/// history_read 도구를 레지스트리에 등록한다.
pub fn register_read(reg: &mut ToolRegistry) {
    let handler: ToolHandler = std::sync::Arc::new(|args| {
        Box::pin(async move {
            let run_id = match int_arg(&args, "run_id", 0) {
                0 => return Err("run_id 는 정수여야 합니다".to_string()),
                id => id as i64,
            };
            let conn = match history::open() {
                Ok(c) => c,
                Err(e) => return Err(e.to_string()),
            };
            match history::get_run(&conn, run_id) {
                Ok(Some(r)) => {
                    let mut out = format!(
                        "작업 #{}\n상태: {}\n시작: {}\n종료: {}\ncwd: {}\nendpoint: {}\nmodel: {}\nchain_id: {}\nsegment: {}\ndepth: {}\nparent_run: {}\ntokens: {}/{}\nfiles: {}\nduration: {}ms",
                        r.id,
                        r.status,
                        r.started_at,
                        r.finished_at.as_deref().unwrap_or("-"),
                        r.cwd,
                        r.endpoint,
                        r.model.as_deref().unwrap_or("-"),
                        r.chain_id,
                        r.segment_index,
                        r.handoff_depth,
                        r.parent_run_id.map(|p| p.to_string()).unwrap_or("-".into()),
                        r.input_tokens.map(|t| t.to_string()).unwrap_or("-".into()),
                        r.output_tokens.map(|t| t.to_string()).unwrap_or("-".into()),
                        history::parse_files_touched(r.files_touched.as_deref()).join(", "),
                        r.duration_ms.map(|d| d.to_string()).unwrap_or("-".into()),
                    );
                    out.push_str("\n\n프롬프트:\n");
                    out.push_str(&r.prompt);
                    if let Some(res) = &r.result {
                        out.push_str("\n\n결과:\n");
                        out.push_str(res);
                    }
                    Ok(out)
                }
                Ok(None) => Err(format!("run #{run_id} 을 찾을 수 없습니다")),
                Err(e) => Err(e.to_string()),
            }
        })
    });
    reg.register(
        "history_read",
        "특정 작업의 상세 내용(프롬프트·결과·토큰·파일)을 조회합니다.",
        read_schema(),
        handler,
    );
}

/// 문자열을 최대 `max` 글자로 절단한다.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;

    fn reg() -> ToolRegistry {
        let mut reg = ToolRegistry::new(false);
        register_list(&mut reg);
        register_read(&mut reg);
        reg
    }

    /// history_list/history_read 가 레지스트리에 등록된다.
    #[test]
    fn history_tools_registered() {
        let reg = reg();
        let names = reg.names();
        assert!(names.contains(&"history_list".to_string()));
        assert!(names.contains(&"history_read".to_string()));
    }

    /// 실제 DB 에 대해 history_list 가 동작한다 (임시 홈 대신 임시 DB 사용).
    #[tokio::test]
    async fn history_list_returns_rows() {
        // 임시 DB 에 run 을 기록하고 history_list 로 조회한다.
        let dir = tempfile::tempdir().unwrap();
        let conn = rusqlite::Connection::open(dir.path().join("h.db")).unwrap();
        history::init_schema(&conn).unwrap();
        let id = history::start_run(
            &conn,
            &history::RunStart {
                cwd: "/tmp/p".to_string(),
                endpoint: "local".to_string(),
                model: Some("m".to_string()),
                prompt: "이전 작업 이어서 완료".to_string(),
                chain_id: "chain-1".to_string(),
                segment_index: 0,
                handoff_depth: 0,
                parent_run_id: None,
            },
        )
        .unwrap();
        history::finish_run(
            &conn,
            id,
            &history::RunFinish {
                status: history::RunStatus::Completed,
                result: Some("결과".to_string()),
                input_tokens: Some(10),
                output_tokens: Some(5),
                files_touched: vec!["a.rs".to_string()],
                duration_ms: Some(100),
            },
        )
        .unwrap();

        // 모델 도구의 핸들러는 history::open() 을 쓰므로, 여기서는 DB 경로를
        // 대체할 수 없어 직접 list_runs 결과를 검증한다.
        let rows = history::list_runs(&conn, Some(10), None, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "completed");
        assert_eq!(rows[0].prompt, "이전 작업 이어서 완료");
    }

    /// truncate 헬퍼 동작.
    #[test]
    fn truncate_works() {
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("abcdef", 3), "abc…");
    }
}