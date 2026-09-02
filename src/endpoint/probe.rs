//! 컨텍스트 길이 프로브 체인 (DESIGN.md §4.1.2).
//!
//! 우선순위:
//! 1. 수동 `context_tokens > 0` (mod.rs 의 probe_context 에서 처리)
//! 2. `GET {base}/models` 확장 필드 (`max_model_len`/`meta.n_ctx`/`max_context_length`)
//! 3. `GET {root}/props` (llama.cpp, `/v1` 이면 상위 경로 조정)
//! 4. `GET {host}/api/show?model=<id>` (Ollama)
//! 5. 폴백 32768 (mod.rs 의 probe_context 에서 처리)

use serde_json::Value;

use crate::config::EndpointConfig;

/// 프로브 결과 상태.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    Ok,
    HttpError(u16, String),
}

/// 프로브 체인 2~4 단계를 순서대로 시도한다.
/// 성공 시 `(context_tokens, 근거 소스)` 를 반환한다.
pub async fn probe_chain(
    ep: &EndpointConfig,
) -> Result<(u64, String), Box<dyn std::error::Error + Send + Sync>> {
    // 2. GET {base}/models 확장 필드 탐색.
    if let Some((tokens, source)) = probe_models(ep).await? {
        return Ok((tokens, source));
    }

    // 3. GET {root}/props (llama.cpp).
    if let Some((tokens, source)) = probe_props(ep).await? {
        return Ok((tokens, source));
    }

    // 4. GET {host}/api/show?model=<id> (Ollama).
    if let Some((tokens, source)) = probe_ollama(ep).await? {
        return Ok((tokens, source));
    }

    Err("모든 프로브 소스가 컨텍스트 길이를 반환하지 않았습니다".into())
}

/// `GET {base}/models` 에서 확장 필드를 순서대로 탐색한다.
async fn probe_models(
    ep: &EndpointConfig,
) -> Result<Option<(u64, String)>, Box<dyn std::error::Error + Send + Sync>> {
    let client = reqwest::Client::new();
    let url = format!("{}/models", ep.url.trim_end_matches('/'));
    let mut req = client.get(&url);
    if let Some(key) = &ep.api_key {
        req = req.bearer_auth(key);
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        return Ok(None);
    }
    let json: Value = resp.json().await?;

    // `data` 배열에서 대상 모델을 찾아 확장 필드를 탐색한다.
    let data = json.get("data").and_then(|d| d.as_array());

    // 대상 모델 항목 목록: id 가 ep.model 과 일치하는 항목들, 없으면 전체.
    let matches: Vec<&Value> = data
        .map(|arr| {
            let by_id: Vec<&Value> = arr
                .iter()
                .filter(|item| {
                    item.get("id")
                        .and_then(|v| v.as_str())
                        .map(|id| id == ep.model)
                        .unwrap_or(false)
                })
                .collect();
            if by_id.is_empty() {
                arr.iter().collect()
            } else {
                by_id
            }
        })
        .unwrap_or_default();

    for item in matches {
        // vLLM: data[].max_model_len
        if let Some(v) = item.get("max_model_len").and_then(|x| x.as_u64()) {
            return Ok(Some((v, format!("{url} data[].max_model_len"))));
        }
        // llama.cpp: data[].meta.n_ctx
        if let Some(v) = item
            .get("meta")
            .and_then(|m| m.get("n_ctx"))
            .and_then(|x| x.as_u64())
        {
            return Ok(Some((v, format!("{url} data[].meta.n_ctx"))));
        }
        // LM Studio: data[].max_context_length
        if let Some(v) = item.get("max_context_length").and_then(|x| x.as_u64()) {
            return Ok(Some((v, format!("{url} data[].max_context_length"))));
        }
    }

    Ok(None)
}

/// `GET {root}/props` (llama.cpp 전용). URL 이 `/v1` 으로 끝나면 상위 경로로 조정한다.
async fn probe_props(
    ep: &EndpointConfig,
) -> Result<Option<(u64, String)>, Box<dyn std::error::Error + Send + Sync>> {
    let client = reqwest::Client::new();
    let base = ep.url.trim_end_matches('/');

    // /v1 이면 상위 경로로 조정 (예: http://host/v1 -> http://host)
    let root = if base.ends_with("/v1") {
        base.trim_end_matches("/v1")
    } else {
        base
    };

    let url = format!("{root}/props");
    let mut req = client.get(&url);
    if let Some(key) = &ep.api_key {
        req = req.bearer_auth(key);
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        return Ok(None);
    }
    let json: Value = resp.json().await?;

    // default_generation_settings.n_ctx
    if let Some(v) = json
        .get("default_generation_settings")
        .and_then(|s| s.get("n_ctx"))
        .and_then(|x| x.as_u64())
    {
        return Ok(Some((
            v,
            format!("{url} default_generation_settings.n_ctx"),
        )));
    }
    // 일부 빌드는 최상위에 n_ctx 가 오기도 한다.
    if let Some(v) = json.get("n_ctx").and_then(|x| x.as_u64()) {
        return Ok(Some((v, format!("{url} n_ctx"))));
    }

    Ok(None)
}

/// `GET {host}/api/show?model=<id>` (Ollama 전용). `model_info` 의 `*.context_length`.
async fn probe_ollama(
    ep: &EndpointConfig,
) -> Result<Option<(u64, String)>, Box<dyn std::error::Error + Send + Sync>> {
    let client = reqwest::Client::new();
    // host 추출: base URL 에서 scheme://host[:port] 부분.
    let base = ep.url.trim_end_matches('/');
    let host = extract_host(base).unwrap_or_else(|| base.to_string());

    let url = format!("{host}/api/show?model={}", ep.model);
    let mut req = client.get(&url);
    if let Some(key) = &ep.api_key {
        req = req.bearer_auth(key);
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        return Ok(None);
    }
    let json: Value = resp.json().await?;

    // model_info 의 값들 중 context_length 필드.
    let model_info = json.get("model_info").and_then(|m| m.as_object());
    if let Some(info) = model_info {
        for (key, val) in info {
            if key.contains("context_length") {
                if let Some(v) = val.as_u64() {
                    return Ok(Some((v, format!("{url} model_info.{key}"))));
                }
            }
        }
    }

    Ok(None)
}

/// URL 에서 `scheme://host[:port]` 부분을 추출한다.
fn extract_host(url: &str) -> Option<String> {
    let after_scheme = url.split("://").nth(1)?;
    let host = after_scheme.split('/').next()?;
    Some(format!("{}://{}", url.split("://").next()?, host))
}

/// 400 에러 메시지에서 컨텍스트 길이 숫자를 파싱한다 (런타임 교정용).
#[allow(dead_code)]
pub fn parse_context_from_error(body: &str) -> Option<u64> {
    // JSON body 파싱 시도.
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        // message 필드에서 숫자 찾기
        if let Some(msg) = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .or_else(|| v.get("message").and_then(|m| m.as_str()))
        {
            if let Some(n) = extract_context_number(msg) {
                return Some(n);
            }
        }
    }
    // 평문 body 에서도 시도.
    extract_context_number(body)
}

/// 문자열에서 컨텍스트 길이 숫자를 추출한다.
/// `max` / `context` / `n_ctx` 근처의 숫자를 찾는다.
#[allow(dead_code)]
fn extract_context_number(text: &str) -> Option<u64> {
    // "context length", "max_context", "n_ctx", "context_length" 근처 숫자 패턴.
    for pattern in [
        "maximum context length is",
        "max context length is",
        "max_context_length",
        "context_length",
        "maximum context length",
        "max_model_len",
        "n_ctx",
    ] {
        if let Some(idx) = text.find(pattern) {
            let tail = &text[idx + pattern.len()..];
            if let Some(n) = first_number(tail) {
                return Some(n);
            }
        }
    }
    None
}

/// 문자열 시작 근처에서 첫 숫자(연속된 자릿수)를 찾는다.
#[allow(dead_code)]
fn first_number(s: &str) -> Option<u64> {
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            let mut num = String::new();
            num.push(c);
            for d in chars.by_ref() {
                if d.is_ascii_digit() {
                    num.push(d);
                } else {
                    break;
                }
            }
            return num.parse().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_number_from_message() {
        assert_eq!(
            extract_context_number("This model's maximum context length is 65536 tokens"),
            Some(65536)
        );
        assert_eq!(
            extract_context_number("max_model_len: 131072"),
            Some(131072)
        );
        assert_eq!(extract_context_number("n_ctx = 60160"), Some(60160));
        assert_eq!(extract_context_number("context_length: 32768"), Some(32768));
    }

    #[test]
    fn extract_number_no_match() {
        assert_eq!(extract_context_number("invalid api key"), None);
        assert_eq!(extract_context_number(""), None);
    }

    #[test]
    fn parse_error_json() {
        let body = r#"{"error":{"message":"maximum context length is 65536 tokens"}}"#;
        assert_eq!(parse_context_from_error(body), Some(65536));
    }

    #[test]
    fn parse_error_plain() {
        assert_eq!(
            parse_context_from_error("Error: max context length is 128000"),
            Some(128000)
        );
    }

    #[test]
    fn extract_host_parses() {
        assert_eq!(
            extract_host("http://127.0.0.1:11434/v1").unwrap(),
            "http://127.0.0.1:11434"
        );
        assert_eq!(
            extract_host("https://api.example.com/v1").unwrap(),
            "https://api.example.com"
        );
    }
}
