//! `bulti history` 서브커맨드 구현 (list/show/last) (DESIGN.md §4.7.2).

use super::{HistoryArgs, HistoryCommand};
use crate::history;

/// history 서브커맨드를 실행한다.
pub fn run(args: HistoryArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let conn = history::open()?;
    match args.command {
        HistoryCommand::List(a) => {
            let rows = history::list_runs(
                &conn,
                a.n,
                a.status.as_deref(),
                a.chain.as_deref(),
            )?;
            if rows.is_empty() {
                println!("기록된 작업이 없습니다.");
                return Ok(0);
            }
            for r in &rows {
                let model = r.model.as_deref().unwrap_or("-");
                let files = history::parse_files_touched(r.files_touched.as_deref()).len();
                let seg = if r.segment_index == 0 {
                    String::new()
                } else {
                    format!(" [seg {}]", r.segment_index)
                };
                println!(
                    "#{:<4} {:<11} {}{} | {} | {} | {} 파일 | {}ms",
                    r.id,
                    r.status,
                    &r.started_at[..19],
                    seg,
                    r.endpoint,
                    model,
                    files,
                    r.duration_ms.map(|d| d.to_string()).unwrap_or("-".into()),
                );
            }
            Ok(0)
        }
        HistoryCommand::Show { id } => {
            let n: i64 = id
                .trim()
                .parse()
                .map_err(|_| format!("id 는 숫자여야 합니다: {id}"))?;
            match history::get_run(&conn, n)? {
                Some(r) => {
                    println!("작업 #{}\n", r.id);
                    println!("  상태:        {}", r.status);
                    println!("  시작:        {}", r.started_at);
                    println!(
                        "  종료:        {}",
                        r.finished_at.as_deref().unwrap_or("-")
                    );
                    println!("  cwd:         {}", r.cwd);
                    println!("  endpoint:    {}", r.endpoint);
                    println!("  model:       {}", r.model.as_deref().unwrap_or("-"));
                    println!("  chain_id:    {}", r.chain_id);
                    println!("  segment:     {}", r.segment_index);
                    println!("  depth:       {}", r.handoff_depth);
                    println!(
                        "  parent_run:  {}",
                        r.parent_run_id.map(|p| p.to_string()).unwrap_or("-".into())
                    );
                    println!(
                        "  tokens:      {}/{}",
                        r.input_tokens.map(|t| t.to_string()).unwrap_or("-".into()),
                        r.output_tokens.map(|t| t.to_string()).unwrap_or("-".into())
                    );
                    let files = history::parse_files_touched(r.files_touched.as_deref());
                    println!("  files:       {}", files.join(", "));
                    println!(
                        "  duration:    {}ms",
                        r.duration_ms.map(|d| d.to_string()).unwrap_or("-".into())
                    );
                    println!("\n  프롬프트:\n{}", r.prompt);
                    if let Some(res) = &r.result {
                        println!("\n  결과:\n{res}");
                    }
                    Ok(0)
                }
                None => {
                    println!("작업 #{n} 을 찾을 수 없습니다.");
                    Ok(1)
                }
            }
        }
        HistoryCommand::Last(a) => {
            let chain = if a.chain {
                history::last_run(&conn, None)?.map(|r| r.chain_id.clone())
            } else {
                None
            };
            // 위에서 None 이면 체인 미지정 그대로.
            let rows = match chain {
                Some(c) => history::list_runs(&conn, Some(1), None, Some(&c))?,
                None => history::list_runs(&conn, Some(1), None, None)?,
            };
            match rows.first() {
                Some(r) => {
                    println!("마지막 작업: #{}\n", r.id);
                    println!("  상태:      {}", r.status);
                    println!("  시작:      {}", r.started_at);
                    println!("  chain_id:  {}", r.chain_id);
                    println!("  endpoint:  {}", r.endpoint);
                    println!("  model:     {}", r.model.as_deref().unwrap_or("-"));
                    if let Some(res) = &r.result {
                        println!("\n  결과:\n{}", &res[..res.len().min(500)]);
                    }
                    Ok(0)
                }
                None => {
                    println!("기록된 작업이 없습니다.");
                    Ok(1)
                }
            }
        }
    }
}