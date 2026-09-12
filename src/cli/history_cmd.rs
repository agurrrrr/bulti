//! `bulti history` 서브커맨드 구현 (list/show/last) (DESIGN.md §4.7.2).

use super::{HistoryArgs, HistoryCommand};
use crate::history;

/// history 서브커맨드를 실행한다.
pub fn run(args: HistoryArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let conn = history::open()?;
    match args.command {
        HistoryCommand::List(a) => {
            let rows = history::list_runs(&conn, a.n, a.status.as_deref(), a.chain.as_deref())?;
            if rows.is_empty() {
                println!("{}", crate::i18n::tr("No recorded tasks."));
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
                    "#{:<4} {:<11} {}{} | {} | {} | {} {} | {}ms",
                    r.id,
                    r.status,
                    &r.started_at[..19],
                    seg,
                    r.endpoint,
                    model,
                    files,
                    crate::i18n::tr("files"),
                    r.duration_ms.map(|d| d.to_string()).unwrap_or("-".into()),
                );
            }
            Ok(0)
        }
        HistoryCommand::Show { id } => {
            let n: i64 = id
                .trim()
                .parse()
                .map_err(|_| crate::i18n::tr_fmt("id must be a number: {id}", &[id.trim()]))?;
            match history::get_run(&conn, n)? {
                Some(r) => {
                    println!(
                        "{}",
                        crate::i18n::tr_fmt("Task #{id}\n", &[&r.id.to_string()])
                    );
                    println!("{}", crate::i18n::tr_fmt("  status:      {}", &[&r.status]));
                    println!(
                        "{}",
                        crate::i18n::tr_fmt("  started:     {}", &[&r.started_at])
                    );
                    println!(
                        "{}",
                        crate::i18n::tr_fmt(
                            "  finished:    {}",
                            &[r.finished_at.as_deref().unwrap_or("-")]
                        )
                    );
                    println!("{}", crate::i18n::tr_fmt("  cwd:         {}", &[&r.cwd]));
                    println!(
                        "{}",
                        crate::i18n::tr_fmt("  endpoint:    {}", &[&r.endpoint])
                    );
                    println!(
                        "{}",
                        crate::i18n::tr_fmt(
                            "  model:       {}",
                            &[r.model.as_deref().unwrap_or("-")]
                        )
                    );
                    println!(
                        "{}",
                        crate::i18n::tr_fmt("  chain_id:    {}", &[&r.chain_id])
                    );
                    println!(
                        "{}",
                        crate::i18n::tr_fmt("  segment:     {}", &[&r.segment_index.to_string()])
                    );
                    println!(
                        "{}",
                        crate::i18n::tr_fmt("  depth:       {}", &[&r.handoff_depth.to_string()])
                    );
                    println!(
                        "{}",
                        crate::i18n::tr_fmt(
                            "  parent_run:  {}",
                            &[&r.parent_run_id
                                .map(|p| p.to_string())
                                .unwrap_or_else(|| "-".into())]
                        )
                    );
                    println!(
                        "{}",
                        crate::i18n::tr_fmt(
                            "  tokens:      {}/{}",
                            &[
                                &r.input_tokens
                                    .map(|t| t.to_string())
                                    .unwrap_or_else(|| "-".into()),
                                &r.output_tokens
                                    .map(|t| t.to_string())
                                    .unwrap_or_else(|| "-".into()),
                            ]
                        )
                    );
                    let files = history::parse_files_touched(r.files_touched.as_deref());
                    println!(
                        "{}",
                        crate::i18n::tr_fmt("  files:       {}", &[&files.join(", ")])
                    );
                    println!(
                        "{}",
                        crate::i18n::tr_fmt(
                            "  duration:    {}ms",
                            &[&r.duration_ms
                                .map(|d| d.to_string())
                                .unwrap_or_else(|| "-".into())]
                        )
                    );
                    println!("{}", crate::i18n::tr_fmt("\n  prompt:\n{}", &[&r.prompt]));
                    if let Some(res) = &r.result {
                        println!("{}", crate::i18n::tr_fmt("\n  result:\n{res}", &[res]));
                    }
                    Ok(0)
                }
                None => {
                    println!(
                        "{}",
                        crate::i18n::tr_fmt("Task #{id} not found.", &[&n.to_string()])
                    );
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
                    println!(
                        "{}",
                        crate::i18n::tr_fmt("Last task: #{id}\n", &[&r.id.to_string()])
                    );
                    println!("{}", crate::i18n::tr_fmt("  status:      {}", &[&r.status]));
                    println!(
                        "{}",
                        crate::i18n::tr_fmt("  started:     {}", &[&r.started_at])
                    );
                    println!(
                        "{}",
                        crate::i18n::tr_fmt("  chain_id:    {}", &[&r.chain_id])
                    );
                    println!(
                        "{}",
                        crate::i18n::tr_fmt("  endpoint:    {}", &[&r.endpoint])
                    );
                    println!(
                        "{}",
                        crate::i18n::tr_fmt(
                            "  model:       {}",
                            &[r.model.as_deref().unwrap_or("-")]
                        )
                    );
                    if let Some(res) = &r.result {
                        println!(
                            "{}",
                            crate::i18n::tr_fmt(
                                "\n  result:\n{res}",
                                &[&res[..res.len().min(500)]]
                            )
                        );
                    }
                    Ok(0)
                }
                None => {
                    println!("{}", crate::i18n::tr("No recorded tasks."));
                    Ok(1)
                }
            }
        }
    }
}
