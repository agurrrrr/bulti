//! `bulti endpoint` 서브커맨드 구현 (add/list/use/remove/set/test/probe).

use super::{EndpointArgs, EndpointCommand};
use crate::config::Config;
use crate::endpoint;

/// 엔드포인트 서브커맨드를 실행한다.
pub fn run(args: EndpointArgs, cfg: &mut Config) -> Result<i32, Box<dyn std::error::Error>> {
    match args.command {
        EndpointCommand::Add(a) => {
            endpoint::add_endpoint(
                cfg,
                &endpoint::EndpointAddSpec {
                    name: a.name.clone(),
                    url: a.url.clone(),
                    api_key: a.api_key.clone(),
                    model: a.model.clone(),
                    context_tokens: a.context_tokens.unwrap_or(0),
                    vision: a.vision,
                    thinking: a.thinking,
                },
            )?;
            cfg.save()?;
            println!(
                "{}",
                crate::i18n::tr_fmt("Registered endpoint '{name}'", &[&a.name])
            );
            Ok(0)
        }
        EndpointCommand::List => {
            let rows = endpoint::list_endpoints(cfg);
            if rows.is_empty() {
                println!("{}", crate::i18n::tr("No endpoints registered."));
                return Ok(0);
            }
            for r in &rows {
                let active = if r.active {
                    crate::i18n::tr(" (active)")
                } else {
                    ""
                };
                let key = if r.api_key_masked.is_empty() {
                    "-".to_string()
                } else {
                    r.api_key_masked.clone()
                };
                let ctx = if r.context_tokens > 0 {
                    r.context_tokens.to_string()
                } else {
                    crate::i18n::tr("auto (probe)").to_string()
                };
                println!(
                    "{}{}\n  url: {}\n  api_key: {}\n  model: {}\n  context_tokens: {}\n",
                    r.name, active, r.url, key, r.model, ctx
                );
            }
            Ok(0)
        }
        EndpointCommand::Use { name } => {
            endpoint::use_endpoint(cfg, &name)?;
            cfg.save()?;
            println!(
                "{}",
                crate::i18n::tr_fmt("Activated endpoint '{name}'", &[&name])
            );
            Ok(0)
        }
        EndpointCommand::Remove { name } => {
            endpoint::remove_endpoint(cfg, &name)?;
            cfg.save()?;
            println!(
                "{}",
                crate::i18n::tr_fmt("Removed endpoint '{name}'", &[&name])
            );
            Ok(0)
        }
        EndpointCommand::Set(s) => {
            let (field, value) = s
                .field
                .split_once('=')
                .ok_or_else(|| crate::i18n::tr("set must be in `key=value` form").to_string())?;
            endpoint::set_endpoint_field(cfg, &s.name, field, value)?;
            cfg.save()?;
            // 키는 마스킹해서 출력.
            if field == "api_key" || field == "key" {
                println!(
                    "{}",
                    crate::i18n::tr_fmt("Endpoint '{name}' api_key unchanged", &[&s.name])
                );
            } else {
                println!(
                    "{}",
                    crate::i18n::tr_fmt(
                        "Endpoint '{name}' {field} = {value}",
                        &[&s.name, field, value]
                    )
                );
            }
            Ok(0)
        }
        EndpointCommand::Test { name } => {
            let ep = cfg
                .endpoints
                .get(&name)
                .ok_or_else(|| crate::i18n::tr_fmt("Endpoint not found: {name}", &[&name]))?;
            let rt = tokio::runtime::Runtime::new()?;
            match rt.block_on(endpoint::test_endpoint(ep))? {
                endpoint::probe::ProbeOutcome::Ok => {
                    println!(
                        "{}",
                        crate::i18n::tr_fmt(
                            "Endpoint '{name}' connection/auth succeeded",
                            &[&name]
                        )
                    );
                    Ok(0)
                }
                endpoint::probe::ProbeOutcome::HttpError(status, body) => {
                    eprintln!(
                        "{}",
                        crate::i18n::tr_fmt(
                            "⚠️  Endpoint '{name}' error {status}: {body}",
                            &[&name, &status.to_string(), &body]
                        )
                    );
                    Ok(1)
                }
            }
        }
        EndpointCommand::Probe { name } => {
            let ep = cfg
                .endpoints
                .get(&name)
                .ok_or_else(|| crate::i18n::tr_fmt("Endpoint not found: {name}", &[&name]))?;
            let rt = tokio::runtime::Runtime::new()?;
            let report = rt.block_on(endpoint::probe_context(ep))?;
            println!(
                "{}",
                crate::i18n::tr_fmt(
                    "Endpoint '{name}' context length: {tokens} (source: {source})",
                    &[&name, &report.context_tokens.to_string(), &report.source]
                )
            );
            Ok(0)
        }
    }
}
