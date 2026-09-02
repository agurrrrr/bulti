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
            println!("엔드포인트 '{}' 등록 완료", a.name);
            Ok(0)
        }
        EndpointCommand::List => {
            let rows = endpoint::list_endpoints(cfg);
            if rows.is_empty() {
                println!("등록된 엔드포인트가 없습니다.");
                return Ok(0);
            }
            for r in &rows {
                let active = if r.active { " (활성)" } else { "" };
                let key = if r.api_key_masked.is_empty() {
                    "-".to_string()
                } else {
                    r.api_key_masked.clone()
                };
                let ctx = if r.context_tokens > 0 {
                    r.context_tokens.to_string()
                } else {
                    "자동(프로브)".to_string()
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
            println!("활성 엔드포인트를 '{}' 로 전환했습니다", name);
            Ok(0)
        }
        EndpointCommand::Remove { name } => {
            endpoint::remove_endpoint(cfg, &name)?;
            cfg.save()?;
            println!("엔드포인트 '{}' 제거 완료", name);
            Ok(0)
        }
        EndpointCommand::Set(s) => {
            let (field, value) = s
                .field
                .split_once('=')
                .ok_or("set 은 `key=value` 형태여야 합니다")?;
            endpoint::set_endpoint_field(cfg, &s.name, field, value)?;
            cfg.save()?;
            // 키는 마스킹해서 출력.
            if field == "api_key" || field == "key" {
                println!("엔드포인트 '{}' api_key 변경 없음", s.name);
            } else {
                println!("엔드포인트 '{}' {field} = {value}", s.name);
            }
            Ok(0)
        }
        EndpointCommand::Test { name } => {
            let ep = cfg
                .endpoints
                .get(&name)
                .ok_or_else(|| format!("엔드포인트를 찾을 수 없습니다: {name}"))?;
            let rt = tokio::runtime::Runtime::new()?;
            match rt.block_on(endpoint::test_endpoint(ep))? {
                endpoint::probe::ProbeOutcome::Ok => {
                    println!("엔드포인트 '{}' 연결·인증 성공", name);
                    Ok(0)
                }
                endpoint::probe::ProbeOutcome::HttpError(status, body) => {
                    eprintln!("⚠️  엔드포인트 '{}' 오류 {status}: {}", name, body);
                    Ok(1)
                }
            }
        }
        EndpointCommand::Probe { name } => {
            let ep = cfg
                .endpoints
                .get(&name)
                .ok_or_else(|| format!("엔드포인트를 찾을 수 없습니다: {name}"))?;
            let rt = tokio::runtime::Runtime::new()?;
            let report = rt.block_on(endpoint::probe_context(ep))?;
            println!(
                "엔드포인트 '{}' 컨텍스트 길이: {} (근거: {})",
                name, report.context_tokens, report.source
            );
            Ok(0)
        }
    }
}
