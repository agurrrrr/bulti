//! `bulti session` 서브커맨드 구현 (DESIGN.md §4.13.2 세션 저장·재개).
//!
//! - `bulti session list` — 세션 목록 (id, 생성 시각, 턴 수, 마지막 갱신 시각).
//! - `bulti session delete <id>` — 세션 삭제.

use super::{SessionArgs, SessionCommand};
use crate::session;

/// `bulti session` 진입점.
pub fn run(args: SessionArgs) -> Result<i32, Box<dyn std::error::Error>> {
    match args.command {
        SessionCommand::List => list(),
        SessionCommand::Delete { id } => delete(&id),
    }
}

/// 세션 목록 출력.
fn list() -> Result<i32, Box<dyn std::error::Error>> {
    let metas = session::list().map_err(|e| e.to_string())?;
    if metas.is_empty() {
        println!("{}", crate::i18n::tr("No sessions."));
        return Ok(0);
    }
    println!(
        "{:<40}  {:<24}  {:<6}  {:<24}",
        "ID",
        crate::i18n::tr("Created"),
        crate::i18n::tr("Turns"),
        crate::i18n::tr("Updated")
    );
    for m in &metas {
        println!(
            "{:<40}  {:<24}  {:<6}  {:<24}",
            m.id, m.created_at, m.turns, m.updated_at
        );
    }
    Ok(0)
}

/// 세션 삭제.
fn delete(id: &str) -> Result<i32, Box<dyn std::error::Error>> {
    session::delete(id).map_err(|e| e.to_string())?;
    println!("{}", crate::i18n::tr_fmt("Deleted session '{id}'.", &[id]));
    Ok(0)
}
