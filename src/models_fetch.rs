//! Fetch upstream model ids via OpenAI-compatible `GET /v1/models`.

use crate::{ApiFormat, ConfigError, StoredProvider};
use serde_json::Value;

/// List model ids from provider. Tries `/v1/models` (and bare `/models` if needed).
pub async fn fetch_model_ids(provider: &StoredProvider) -> Result<Vec<String>, ConfigError> {
    let client = reqwest::Client::builder()
        .user_agent("local-llm-proxy/llpx")
        .build()
        .map_err(|e| ConfigError::new(e.to_string()))?;

    let base = provider.base_url.trim_end_matches('/');
    let candidates = model_list_urls(base);

    let mut last_err = ConfigError::new("no models endpoint tried");
    for url in candidates {
        match try_list(&client, &url, provider).await {
            Ok(ids) if !ids.is_empty() => return Ok(ids),
            Ok(_) => last_err = ConfigError::new(format!("{url}: empty model list")),
            Err(err) => last_err = err,
        }
    }
    Err(last_err)
}

fn model_list_urls(base: &str) -> Vec<String> {
    let mut urls = Vec::new();
    // Anthropic gateways sometimes still expose OpenAI-compatible /v1/models.
    if base.ends_with("/v1") {
        urls.push(format!("{base}/models"));
    } else if base.ends_with("/v1/models") {
        urls.push(base.to_string());
    } else {
        urls.push(format!("{base}/v1/models"));
        urls.push(format!("{base}/models"));
    }
    urls
}

async fn try_list(
    client: &reqwest::Client,
    url: &str,
    provider: &StoredProvider,
) -> Result<Vec<String>, ConfigError> {
    let mut req = client.get(url);
    match provider.api_format {
        ApiFormat::Anthropic => {
            req = req
                .header("x-api-key", provider.api_key.as_str())
                .header("anthropic-version", "2023-06-01")
                .header(
                    reqwest::header::AUTHORIZATION,
                    format!("Bearer {}", provider.api_key),
                );
        }
        _ => {
            req = req.header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", provider.api_key),
            );
        }
    }
    let resp = req
        .send()
        .await
        .map_err(|e| ConfigError::new(format!("GET {url}: {e}")))?;
    let status = resp.status();
    let body = resp
        .bytes()
        .await
        .map_err(|e| ConfigError::new(format!("read {url}: {e}")))?;
    if !status.is_success() {
        let snippet = String::from_utf8_lossy(&body);
        let snippet = snippet.chars().take(200).collect::<String>();
        return Err(ConfigError::new(format!("GET {url} → {status}: {snippet}")));
    }
    parse_model_ids(&body)
}

fn parse_model_ids(body: &[u8]) -> Result<Vec<String>, ConfigError> {
    let value: Value =
        serde_json::from_slice(body).map_err(|e| ConfigError::new(format!("models json: {e}")))?;

    if let Some(arr) = value.get("data").and_then(|v| v.as_array()) {
        let mut ids = Vec::new();
        for item in arr {
            if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                ids.push(id.to_string());
            }
        }
        if !ids.is_empty() {
            ids.sort();
            ids.dedup();
            return Ok(ids);
        }
    }

    if let Some(arr) = value.get("models").and_then(|v| v.as_array()) {
        let mut ids = Vec::new();
        for item in arr {
            if let Some(s) = item.as_str() {
                ids.push(s.to_string());
            } else if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                ids.push(id.to_string());
            }
        }
        ids.sort();
        ids.dedup();
        return Ok(ids);
    }

    Err(ConfigError::new(
        "unsupported /models response shape (expected OpenAI data[] or models[])",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_shape() {
        let body = br#"{"object":"list","data":[{"id":"b"},{"id":"a"}]}"#;
        let ids = parse_model_ids(body).unwrap();
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn parses_models_array_shape() {
        let body = br#"{"models":["z","y"]}"#;
        let ids = parse_model_ids(body).unwrap();
        assert_eq!(ids, vec!["y".to_string(), "z".to_string()]);
    }

    #[test]
    fn builds_models_urls_for_v1_base() {
        assert_eq!(
            model_list_urls("https://example.com/v1"),
            vec!["https://example.com/v1/models"]
        );
    }

    #[test]
    fn keeps_explicit_models_endpoint() {
        assert_eq!(
            model_list_urls("https://example.com/v1/models"),
            vec!["https://example.com/v1/models"]
        );
    }

    #[test]
    fn tries_v1_then_bare_models_for_generic_base() {
        assert_eq!(
            model_list_urls("https://example.com/api"),
            vec![
                "https://example.com/api/v1/models",
                "https://example.com/api/models"
            ]
        );
    }
}
