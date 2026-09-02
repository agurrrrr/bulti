//! `bulti config` 서브커맨드 구현 (get/set/list).

use std::collections::BTreeMap;

use super::{ConfigArgs, ConfigCommand};
use crate::config::Config;

/// 설정 서브커맨드를 실행한다.
pub fn run(args: ConfigArgs, cfg: &mut Config) -> Result<i32, Box<dyn std::error::Error>> {
    match args.command {
        ConfigCommand::Get { key } => {
            let value = get_value(cfg, &key);
            match value {
                Some(v) => {
                    println!("{} = {}", key, v);
                    Ok(0)
                }
                None => {
                    tracing::warn!("설정 키를 찾을 수 없습니다: {}", key);
                    Ok(1)
                }
            }
        }
        ConfigCommand::Set { key, value } => {
            set_value(cfg, &key, &value);
            cfg.save()?;
            println!("{} = {}", key, value);
            Ok(0)
        }
        ConfigCommand::List => {
            list(cfg);
            Ok(0)
        }
    }
}

/// 설정 값을 문자열로 조회한다.
fn get_value(cfg: &Config, key: &str) -> Option<String> {
    match key {
        "version" => Some(cfg.version.to_string()),
        "active_endpoint" => cfg.active_endpoint.clone(),
        "handoff_threshold_pct" => Some(cfg.context.handoff_threshold_pct.to_string()),
        "max_handoff_depth" => Some(cfg.context.max_handoff_depth.to_string()),
        "handoff_warn_depth" => Some(cfg.context.handoff_warn_depth.to_string()),
        "update.repo" => cfg.update.as_ref().map(|u| u.repo.clone()),
        "update.mode" => cfg.update.as_ref().map(|u| u.mode.to_string()),
        _ => {
            // endpoints.* / mcp.* 접근
            if let Some(rest) = key.strip_prefix("endpoints.") {
                let (name, field) = rest.split_once('.').unwrap_or((rest, ""));
                let ep = cfg.endpoints.get(name)?;
                return endpoint_field(ep, field);
            }
            if let Some(rest) = key.strip_prefix("mcp.") {
                let (name, field) = rest.split_once('.').unwrap_or((rest, ""));
                let m = cfg.mcp.get(name)?;
                return mcp_field(m, field);
            }
            None
        }
    }
}

/// 엔드포인트 필드를 문자열로 조회한다.
fn endpoint_field(ep: &crate::config::EndpointConfig, field: &str) -> Option<String> {
    match field {
        "url" => Some(ep.url.clone()),
        "api_key" => ep.api_key.clone(),
        "model" => Some(ep.model.clone()),
        "context_tokens" => Some(ep.context_tokens.to_string()),
        "vision" => Some(ep.vision.to_string()),
        "thinking" => Some(ep.thinking.to_string()),
        "max_iterations" => Some(ep.max_iterations.to_string()),
        _ => None,
    }
}

/// MCP 필드를 문자열로 조회한다.
fn mcp_field(m: &crate::config::McpConfig, field: &str) -> Option<String> {
    match field {
        "command" => Some(m.command.clone()),
        "args" => Some(m.args.join(" ")),
        "description" => m.description.clone(),
        _ => None,
    }
}

/// 설정 값을 수정한다. 미지원 키는 무시한다.
fn set_value(cfg: &mut Config, key: &str, value: &str) {
    match key {
        "active_endpoint" => cfg.active_endpoint = Some(value.to_string()),
        "handoff_threshold_pct" => {
            if let Ok(v) = value.parse::<u8>() {
                cfg.context.handoff_threshold_pct = v;
            }
        }
        "max_handoff_depth" => {
            if let Ok(v) = value.parse::<u32>() {
                cfg.context.max_handoff_depth = v;
            }
        }
        "handoff_warn_depth" => {
            if let Ok(v) = value.parse::<u32>() {
                cfg.context.handoff_warn_depth = v;
            }
        }
        "update.repo" => {
            let u = cfg
                .update
                .get_or_insert_with(|| crate::config::UpdateConfig {
                    repo: String::new(),
                    mode: crate::config::UpdateMode::default(),
                });
            u.repo = value.to_string();
        }
        "update.mode" => {
            let mode = match value {
                "check" => crate::config::UpdateMode::Check,
                "download" => crate::config::UpdateMode::Download,
                "off" => crate::config::UpdateMode::Off,
                _ => return,
            };
            let u = cfg
                .update
                .get_or_insert_with(|| crate::config::UpdateConfig {
                    repo: String::new(),
                    mode: crate::config::UpdateMode::default(),
                });
            u.mode = mode;
        }
        _ => {
            tracing::warn!("설정 키 수정 미지원: {}", key);
        }
    }
}

/// 설정 전체를 평면 키 목록으로 출력한다.
fn list(cfg: &Config) {
    let mut keys: BTreeMap<String, String> = BTreeMap::new();
    keys.insert("version".to_string(), cfg.version.to_string());
    if let Some(ae) = &cfg.active_endpoint {
        keys.insert("active_endpoint".to_string(), ae.clone());
    }
    keys.insert(
        "handoff_threshold_pct".to_string(),
        cfg.context.handoff_threshold_pct.to_string(),
    );
    keys.insert(
        "max_handoff_depth".to_string(),
        cfg.context.max_handoff_depth.to_string(),
    );
    keys.insert(
        "handoff_warn_depth".to_string(),
        cfg.context.handoff_warn_depth.to_string(),
    );
    for (name, ep) in &cfg.endpoints {
        keys.insert(format!("endpoints.{name}.url"), ep.url.clone());
        keys.insert(format!("endpoints.{name}.model"), ep.model.clone());
        keys.insert(
            format!("endpoints.{name}.context_tokens"),
            ep.context_tokens.to_string(),
        );
        keys.insert(format!("endpoints.{name}.vision"), ep.vision.to_string());
        keys.insert(
            format!("endpoints.{name}.thinking"),
            ep.thinking.to_string(),
        );
        keys.insert(
            format!("endpoints.{name}.max_iterations"),
            ep.max_iterations.to_string(),
        );
    }
    for (name, m) in &cfg.mcp {
        keys.insert(format!("mcp.{name}.command"), m.command.clone());
        keys.insert(format!("mcp.{name}.args"), m.args.join(" "));
    }
    if let Some(u) = &cfg.update {
        keys.insert("update.repo".to_string(), u.repo.clone());
        keys.insert("update.mode".to_string(), u.mode.to_string());
    }
    for (k, v) in keys {
        println!("{} = {}", k, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        crate::config::tests::sample_config()
    }

    #[test]
    fn get_active_endpoint() {
        let cfg = cfg();
        assert_eq!(get_value(&cfg, "active_endpoint").as_deref(), Some("main"));
    }

    #[test]
    fn get_endpoint_field() {
        let cfg = cfg();
        assert_eq!(
            get_value(&cfg, "endpoints.main.model").as_deref(),
            Some("qwen3.8-27b-q2")
        );
        assert_eq!(
            get_value(&cfg, "endpoints.main.api_key").as_deref(),
            Some("sk-test")
        );
    }

    #[test]
    fn get_missing_returns_none() {
        let cfg = cfg();
        assert!(get_value(&cfg, "nope").is_none());
    }

    #[test]
    fn set_and_save_roundtrip() {
        let mut cfg = Config::new();
        set_value(&mut cfg, "active_endpoint", "main");
        assert_eq!(cfg.active_endpoint.as_deref(), Some("main"));
        set_value(&mut cfg, "handoff_threshold_pct", "80");
        assert_eq!(cfg.context.handoff_threshold_pct, 80);
    }
}
