//! 엔드포인트 등록·프로브·컨텍스트 길이 확정 (DESIGN.md §4.1).
//!
//! 단계 1: `add|list|use|remove|set|test|probe` 서브커맨드,
//! API 키 마스킹·센티널, 컨텍스트 길이 프로브 체인 구현.

pub mod probe;

use thiserror::Error;

use crate::config::{Config, EndpointConfig};

/// 엔드포인트 관련 오류.
#[derive(Debug, Error)]
pub enum EndpointError {
    #[error("엔드포인트를 찾을 수 없습니다: {0}")]
    NotFound(String),
    #[error("필드 수정 실패: {0}")]
    InvalidField(String),
    #[error("설정 저장 실패: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("HTTP 오류: {0}")]
    Http(#[from] reqwest::Error),
}

/// API 키 마스킹 (DESIGN.md §4.1.1). 화면 출력용.
pub fn mask_api_key(key: &str) -> String {
    if key.is_empty() {
        return String::new();
    }
    // 앞 4자 + 나머지 마스킹. 너무 짧으면 전부 마스킹.
    let visible = key.chars().take(4).collect::<String>();
    if visible.chars().count() >= key.chars().count() {
        return "****".to_string();
    }
    format!("{visible}…{}", "*".repeat(8))
}

/// 키 센티널 상수. `set` 에서 "변경 없음" 을 나타낸다.
pub const KEY_SENTINEL: &str = "********";

/// 키가 센티널 또는 마스킹 문자열 그대로인지 판별한다.
/// 비어 있거나 센티널이면 "변경 없음" 으로 처리한다.
pub fn is_key_sentinel(value: &str) -> bool {
    value.is_empty() || value == KEY_SENTINEL || value == "****"
}

/// 엔드포인트 등록용 사양 (add).
#[derive(Debug, Clone)]
pub struct EndpointAddSpec {
    pub name: String,
    pub url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub context_tokens: u64,
    pub vision: bool,
    pub thinking: bool,
}

/// 엔드포인트 등록 (add).
pub fn add_endpoint(cfg: &mut Config, spec: &EndpointAddSpec) -> Result<(), EndpointError> {
    let ep = EndpointConfig {
        url: spec.url.clone(),
        api_key: spec.api_key.clone(),
        model: spec.model.clone(),
        context_tokens: spec.context_tokens,
        vision: spec.vision,
        thinking: spec.thinking,
        max_iterations: 200,
    };
    cfg.endpoints.insert(spec.name.clone(), ep);
    // 첫 등록이면 자동 활성화.
    if cfg.active_endpoint.is_none() {
        cfg.active_endpoint = Some(spec.name.clone());
    }
    Ok(())
}

/// 엔드포인트 제거 (remove).
pub fn remove_endpoint(cfg: &mut Config, name: &str) -> Result<(), EndpointError> {
    if cfg.endpoints.remove(name).is_none() {
        return Err(EndpointError::NotFound(name.to_string()));
    }
    // 활성이던 엔드포인트가 제거되면 active_endpoint 해제.
    if cfg.active_endpoint.as_deref() == Some(name) {
        cfg.active_endpoint = None;
    }
    Ok(())
}

/// 엔드포인트 활성화 (use).
pub fn use_endpoint(cfg: &mut Config, name: &str) -> Result<(), EndpointError> {
    if !cfg.endpoints.contains_key(name) {
        return Err(EndpointError::NotFound(name.to_string()));
    }
    cfg.active_endpoint = Some(name.to_string());
    Ok(())
}

/// 엔드포인트 필드 수정 (set).
///
/// 보안 규칙 (§4.1.1): 키 필드가 비었거나 마스킹 문자열 그대로면
/// "변경 없음" 센티널로 처리해 실제 키를 덮어쓰지 않는다.
pub fn set_endpoint_field(
    cfg: &mut Config,
    name: &str,
    field: &str,
    value: &str,
) -> Result<(), EndpointError> {
    let ep = cfg
        .endpoints
        .get_mut(name)
        .ok_or_else(|| EndpointError::NotFound(name.to_string()))?;

    match field {
        "url" => ep.url = value.to_string(),
        "model" => ep.model = value.to_string(),
        "context_tokens" => {
            ep.context_tokens = value.parse().map_err(|_| {
                EndpointError::InvalidField("context_tokens 는 숫자여야 합니다".into())
            })?;
        }
        "vision" => {
            ep.vision = value.parse().unwrap_or(false);
        }
        "thinking" => {
            ep.thinking = value.parse().unwrap_or(false);
        }
        "max_iterations" => {
            ep.max_iterations = value.parse().map_err(|_| {
                EndpointError::InvalidField("max_iterations 는 숫자여야 합니다".into())
            })?;
        }
        "api_key" | "key" => {
            // 센티널이면 기존 키 유지 (변경 없음).
            if !is_key_sentinel(value) {
                ep.api_key = Some(value.to_string());
            }
        }
        _ => {
            return Err(EndpointError::InvalidField(format!(
                "지원하지 않는 필드입니다: {field}"
            )));
        }
    }
    Ok(())
}

/// 엔드포인트 목록용 출력 행.
pub struct EndpointRow {
    pub name: String,
    pub url: String,
    pub api_key_masked: String,
    pub model: String,
    pub context_tokens: u64,
    pub active: bool,
}

/// 목록 출력용 행을 만든다. API 키는 항상 마스킹된다.
pub fn list_endpoints(cfg: &Config) -> Vec<EndpointRow> {
    let mut rows: Vec<EndpointRow> = cfg
        .endpoints
        .iter()
        .map(|(name, ep)| EndpointRow {
            name: name.clone(),
            url: ep.url.clone(),
            api_key_masked: match &ep.api_key {
                Some(k) => mask_api_key(k),
                None => String::new(),
            },
            model: ep.model.clone(),
            context_tokens: ep.context_tokens,
            active: cfg.active_endpoint.as_deref() == Some(name.as_str()),
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

/// 엔드포인트 연결·인증 확인 (test). `/models` 를 호출한다.
pub async fn test_endpoint(ep: &EndpointConfig) -> Result<probe::ProbeOutcome, EndpointError> {
    let client = reqwest::Client::new();
    let mut req = client.get(format!("{}/models", ep.url.trim_end_matches('/')));
    if let Some(key) = &ep.api_key {
        req = req.bearer_auth(key);
    }
    let resp = req.send().await?;
    if resp.status().is_success() {
        Ok(probe::ProbeOutcome::Ok)
    } else {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        Ok(probe::ProbeOutcome::HttpError(status, body))
    }
}

/// 프로브 결과와 근거 요약.
pub struct ProbeReport {
    pub context_tokens: u64,
    pub source: String,
}

/// 컨텍스트 길이 프로브 체인 (§4.1.2)을 실행하고 결과를 반환한다.
pub async fn probe_context(ep: &EndpointConfig) -> Result<ProbeReport, EndpointError> {
    // 1. 수동 설정값 최우선.
    if ep.context_tokens > 0 {
        return Ok(ProbeReport {
            context_tokens: ep.context_tokens,
            source: "manual context_tokens".to_string(),
        });
    }

    // 2~4. HTTP 프로브 체인.
    match probe::probe_chain(ep).await {
        Ok((tokens, source)) => Ok(ProbeReport {
            context_tokens: tokens,
            source,
        }),
        Err(e) => {
            // 5. 폴백 기본값 32768 + stderr 경고.
            eprintln!("⚠️  컨텍스트 길이 프로브 실패, 기본값 32768 사용: {e}");
            Ok(ProbeReport {
                context_tokens: 32768,
                source: "fallback 32768".to_string(),
            })
        }
    }
}

/// 런타임 교정: 400 에러 메시지에서 숫자를 파싱해 엔드포인트 설정값을 자동 보정한다.
///
/// `err` 에 `max_context_length` / `n_ctx` / `context_length` 계열 숫자가 있으면
/// 그 값으로 `context_tokens` 을 갱신하고 경고를 남긴다.
/// (llm 모듈 구현 단계에서 호출 예정 — 현재는 테스트로 검증.)
#[allow(dead_code)]
pub fn apply_runtime_correction(cfg: &mut Config, name: &str, err_body: &str) -> Option<u64> {
    let parsed = probe::parse_context_from_error(err_body)?;
    let ep = cfg.endpoints.get_mut(name)?;
    if ep.context_tokens != parsed {
        ep.context_tokens = parsed;
        tracing::warn!(
            "런타임 교정: 엔드포인트 '{name}' 컨텍스트 길이를 {} 으로 보정했습니다",
            parsed
        );
    }
    Some(parsed)
}

/// 활성 엔드포인트 이름을 반환한다 (없으면 None).
#[allow(dead_code)]
pub fn active_endpoint_name(cfg: &Config) -> Option<&str> {
    cfg.active_endpoint.as_deref()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_cfg() -> Config {
        Config::new()
    }

    /// 테스트용 엔드포인트 등록 헬퍼. 기본값: 키 없음, 모델 "m", 컨텍스트 0(자동), vision/thinking false.
    fn add_test(cfg: &mut Config, name: &str) {
        add_endpoint(
            cfg,
            &EndpointAddSpec {
                name: name.to_string(),
                url: "http://x/v1".to_string(),
                api_key: None,
                model: "m".to_string(),
                context_tokens: 0,
                vision: false,
                thinking: false,
            },
        )
        .unwrap();
    }

    #[test]
    fn mask_api_key_hides_middle() {
        let masked = mask_api_key("sk-abcdef123456");
        assert!(masked.starts_with("sk-a"));
        assert!(masked.contains('*'));
        assert!(!masked.contains("abcdef"));
    }

    #[test]
    fn mask_api_key_empty() {
        assert_eq!(mask_api_key(""), "");
    }

    #[test]
    fn key_sentinel_detection() {
        assert!(is_key_sentinel(""));
        assert!(is_key_sentinel(KEY_SENTINEL));
        assert!(is_key_sentinel("****"));
        assert!(!is_key_sentinel("sk-real-key"));
    }

    #[test]
    fn add_endpoint_auto_activates_first() {
        let mut cfg = empty_cfg();
        add_endpoint(
            &mut cfg,
            &EndpointAddSpec {
                name: "main".to_string(),
                url: "http://127.0.0.1:8084/v1".to_string(),
                api_key: Some("sk-test".to_string()),
                model: "qwen3".to_string(),
                context_tokens: 0,
                vision: true,
                thinking: true,
            },
        )
        .unwrap();
        assert_eq!(cfg.active_endpoint.as_deref(), Some("main"));
        assert_eq!(cfg.endpoints["main"].url, "http://127.0.0.1:8084/v1");
        assert_eq!(cfg.endpoints["main"].api_key.as_deref(), Some("sk-test"));
    }

    #[test]
    fn set_api_key_sentinel_keeps_original() {
        let mut cfg = empty_cfg();
        add_endpoint(
            &mut cfg,
            &EndpointAddSpec {
                name: "main".to_string(),
                url: "http://x/v1".to_string(),
                api_key: Some("sk-original".to_string()),
                model: "m".to_string(),
                context_tokens: 0,
                vision: false,
                thinking: false,
            },
        )
        .unwrap();
        // 센티널이면 기존 키 유지.
        set_endpoint_field(&mut cfg, "main", "api_key", KEY_SENTINEL).unwrap();
        assert_eq!(
            cfg.endpoints["main"].api_key.as_deref(),
            Some("sk-original")
        );
        // 빈 값도 유지.
        set_endpoint_field(&mut cfg, "main", "api_key", "").unwrap();
        assert_eq!(
            cfg.endpoints["main"].api_key.as_deref(),
            Some("sk-original")
        );
        // 실제 키면 교체.
        set_endpoint_field(&mut cfg, "main", "api_key", "sk-new").unwrap();
        assert_eq!(cfg.endpoints["main"].api_key.as_deref(), Some("sk-new"));
    }

    #[test]
    fn set_other_fields() {
        let mut cfg = empty_cfg();
        add_test(&mut cfg, "main");
        set_endpoint_field(&mut cfg, "main", "url", "http://y/v1").unwrap();
        set_endpoint_field(&mut cfg, "main", "model", "m2").unwrap();
        set_endpoint_field(&mut cfg, "main", "context_tokens", "16000").unwrap();
        assert_eq!(cfg.endpoints["main"].url, "http://y/v1");
        assert_eq!(cfg.endpoints["main"].model, "m2");
        assert_eq!(cfg.endpoints["main"].context_tokens, 16000);
    }

    #[test]
    fn set_invalid_field_errors() {
        let mut cfg = empty_cfg();
        add_test(&mut cfg, "main");
        assert!(set_endpoint_field(&mut cfg, "main", "nope", "v").is_err());
        assert!(set_endpoint_field(&mut cfg, "missing", "url", "v").is_err());
    }

    #[test]
    fn remove_endpoint_clears_active() {
        let mut cfg = empty_cfg();
        add_test(&mut cfg, "main");
        remove_endpoint(&mut cfg, "main").unwrap();
        assert!(cfg.endpoints.is_empty());
        assert!(cfg.active_endpoint.is_none());
    }

    #[test]
    fn use_endpoint_requires_existing() {
        let mut cfg = empty_cfg();
        assert!(use_endpoint(&mut cfg, "nope").is_err());
        add_endpoint(
            &mut cfg,
            &EndpointAddSpec {
                name: "a".to_string(),
                url: "http://x/v1".to_string(),
                api_key: None,
                model: "m".to_string(),
                context_tokens: 0,
                vision: false,
                thinking: false,
            },
        )
        .unwrap();
        use_endpoint(&mut cfg, "a").unwrap();
        assert_eq!(cfg.active_endpoint.as_deref(), Some("a"));
    }

    #[test]
    fn list_endpoints_masks_key() {
        let mut cfg = empty_cfg();
        add_endpoint(
            &mut cfg,
            &EndpointAddSpec {
                name: "main".to_string(),
                url: "http://x/v1".to_string(),
                api_key: Some("sk-super-secret".to_string()),
                model: "m".to_string(),
                context_tokens: 0,
                vision: false,
                thinking: false,
            },
        )
        .unwrap();
        let rows = list_endpoints(&cfg);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].api_key_masked.contains('*'));
        assert!(!rows[0].api_key_masked.contains("super-secret"));
        assert!(rows[0].active);
    }

    #[test]
    fn runtime_correction_parses_number() {
        let mut cfg = empty_cfg();
        add_endpoint(
            &mut cfg,
            &EndpointAddSpec {
                name: "main".to_string(),
                url: "http://x/v1".to_string(),
                api_key: None,
                model: "m".to_string(),
                context_tokens: 0,
                vision: false,
                thinking: false,
            },
        )
        .unwrap();
        let body = r#"{"error":{"message":"This model's maximum context length is 65536 tokens"}}"#;
        let parsed = apply_runtime_correction(&mut cfg, "main", body);
        assert_eq!(parsed, Some(65536));
        assert_eq!(cfg.endpoints["main"].context_tokens, 65536);
    }

    #[test]
    fn runtime_correction_no_number_returns_none() {
        let mut cfg = empty_cfg();
        add_test(&mut cfg, "main");
        assert!(apply_runtime_correction(&mut cfg, "main", "some other error").is_none());
    }
}
