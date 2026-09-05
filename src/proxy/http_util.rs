//! Shared HTTP helpers for the Codex proxy path.

use crate::config::Provider;
use crate::exchange::ExchangeLog;
use crate::proxy::providers::transform_codex_chat::chat_error_to_response_error;
use crate::proxy::response_processor::{
    strip_entity_headers_for_rebuilt_body, strip_hop_by_hop_response_headers,
};
use axum::{
    body::Body,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::Response,
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::convert::Infallible;

#[derive(Clone, Copy)]
pub enum AuthStyle {
    Bearer,
    Anthropic,
}

pub fn join_url(base: &str, path: &str) -> String {
    let (base_without_query, base_query) = base.split_once('?').unwrap_or((base, ""));
    let base = base_without_query.trim_end_matches('/');
    let (path_without_query, endpoint_query) = path.split_once('?').unwrap_or((path, ""));
    let path = if path_without_query.starts_with('/') {
        path_without_query.to_string()
    } else {
        format!("/{path_without_query}")
    };
    let origin_only = reqwest::Url::parse(base)
        .ok()
        .is_some_and(|url| matches!(url.path(), "" | "/"));
    let mut url = if base.ends_with(&path) {
        base.to_string()
    } else if base.ends_with("/v1") && path.starts_with("/v1/") {
        format!("{}{}", base.trim_end_matches("/v1"), path)
    } else if origin_only && path != "/v1" && !path.starts_with("/v1/") {
        format!("{base}/v1{path}")
    } else {
        format!("{base}{path}")
    };
    while url.contains("/v1/v1") {
        url = url.replace("/v1/v1", "/v1");
    }
    if !base_query.is_empty() {
        url.push('?');
        url.push_str(base_query);
    }
    if !endpoint_query.is_empty() {
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str(endpoint_query);
    }
    url
}

pub fn resolve_endpoint_url(base: &str, endpoint: &str, is_full_url: bool) -> String {
    if !is_full_url {
        return join_url(base, endpoint);
    }
    let (base_without_query, base_query) = base.split_once('?').unwrap_or((base, ""));
    let (_, endpoint_query) = endpoint.split_once('?').unwrap_or((endpoint, ""));
    let mut url = base_without_query.trim_end_matches('/').to_string();
    if !base_query.is_empty() {
        url.push('?');
        url.push_str(base_query);
    }
    if !endpoint_query.is_empty() {
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str(endpoint_query);
    }
    url
}

pub async fn send_json(
    client: &reqwest::Client,
    provider: &Provider,
    url: &str,
    client_headers: &HeaderMap,
    body: &Value,
    auth: AuthStyle,
    anthropic_version: Option<&str>,
) -> Result<reqwest::Response, String> {
    let mut req = client.post(url).json(body);
    match auth {
        AuthStyle::Bearer => {
            req = req.header(
                header::AUTHORIZATION,
                format!("Bearer {}", provider.api_key),
            );
        }
        AuthStyle::Anthropic => {
            req = req.header("x-api-key", provider.api_key.as_str()).header(
                "anthropic-version",
                anthropic_version.unwrap_or("2023-06-01"),
            );
        }
    }
    let ua = client_headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .unwrap_or("codex_cli_rs/0.149.1");
    req = req.header(header::USER_AGENT, ua);
    for name in ["x-codex-turn-state", "x-request-id", "openai-beta"] {
        if let Some(value) = client_headers.get(name) {
            if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
                req = req.header(name, value);
            }
        }
    }
    req.send().await.map_err(|e| e.to_string())
}

pub fn response_headers_for_body(headers: &HeaderMap, content_type: &str) -> HeaderMap {
    let mut headers = headers.clone();
    strip_hop_by_hop_response_headers(&mut headers);
    strip_entity_headers_for_rebuilt_body(&mut headers);
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type).unwrap_or(HeaderValue::from_static("application/json")),
    );
    headers
}

pub fn response_with_headers<B: Into<Body>>(
    status: StatusCode,
    headers: HeaderMap,
    body: B,
) -> Response {
    let mut builder = Response::builder().status(status);
    for (name, value) in headers {
        if let Some(name) = name {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(body.into())
        .unwrap_or_else(|_| json_error(StatusCode::INTERNAL_SERVER_ERROR, "build response"))
}

pub fn json_response(status: StatusCode, value: Value) -> Response {
    let bytes = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
    response_with_headers(
        status,
        response_headers_for_body(&HeaderMap::new(), "application/json"),
        bytes,
    )
}

pub fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    let body = json!({ "error": { "message": message.into(), "type": "proxy_error" } });
    json_response(status, body)
}

pub fn sse_response<S, E>(
    status: StatusCode,
    upstream_headers: &HeaderMap,
    stream: S,
    exchange: &mut ExchangeLog,
) -> Response
where
    S: futures_util::Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    exchange.mark_streaming();
    let stream = stream.map(|item| match item {
        Ok(bytes) => Ok::<Bytes, Infallible>(bytes),
        Err(err) => {
            log::error!("sse stream error: {err}");
            Ok(Bytes::from(format!("event: error\ndata: {err}\n\n")))
        }
    });
    let mut headers = response_headers_for_body(upstream_headers, "text/event-stream");
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response_with_headers(status, headers, Body::from_stream(stream))
}

pub async fn relay_error_body(
    upstream: reqwest::Response,
    exchange: &mut ExchangeLog,
    convert_to_responses: bool,
) -> Response {
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let headers = upstream.headers().clone();
    let bytes = upstream.bytes().await.unwrap_or_default().to_vec();
    exchange.write_raw("upstream_error.json", &bytes);
    let body = if convert_to_responses {
        converted_error_bytes(&bytes)
    } else {
        bytes
    };
    response_with_headers(
        status,
        response_headers_for_body(&headers, "application/json"),
        body,
    )
}

pub fn converted_error_body(body: &Value) -> Value {
    chat_error_to_response_error(Some(body))
}

pub fn converted_error_bytes(bytes: &[u8]) -> Vec<u8> {
    let value = serde_json::from_slice::<Value>(bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(bytes).into_owned()));
    serde_json::to_vec(&converted_error_body(&value)).unwrap_or_else(|_| {
        br#"{"error":{"message":"upstream error","type":"upstream_error"}}"#.to_vec()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn join_url_matches_cc_switch_codex_url_rules() {
        assert_eq!(
            join_url("https://api.example.com", "/responses"),
            "https://api.example.com/v1/responses"
        );
        assert_eq!(
            join_url("https://api.example.com/", "responses"),
            "https://api.example.com/v1/responses"
        );
        assert_eq!(
            join_url("https://api.example.com/v1", "/responses"),
            "https://api.example.com/v1/responses"
        );
        assert_eq!(
            join_url("https://api.example.com/v1", "/v1/responses"),
            "https://api.example.com/v1/responses"
        );
        assert_eq!(
            join_url("https://api.example.com/openai", "/responses"),
            "https://api.example.com/openai/responses"
        );
        assert_eq!(
            join_url("https://api.example.com", "/v1/messages"),
            "https://api.example.com/v1/messages"
        );
        assert_eq!(
            join_url("https://api.example.com", "/v1/v1/responses"),
            "https://api.example.com/v1/responses"
        );
    }

    #[test]
    fn join_url_preserves_query_and_accepts_full_endpoint_bases() {
        assert_eq!(
            join_url("https://api.example.com/v1/chat/completions", "/chat/completions"),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            join_url(
                "https://api.example.com/v1/chat/completions",
                "/chat/completions?model=fast"
            ),
            "https://api.example.com/v1/chat/completions?model=fast"
        );
        assert_eq!(
            join_url("https://api.example.com/v1", "/responses?stream=true"),
            "https://api.example.com/v1/responses?stream=true"
        );
        assert_eq!(
            join_url("https://api.example.com/v1/messages?beta=1", "/v1/messages"),
            "https://api.example.com/v1/messages?beta=1"
        );
    }

    #[test]
    fn explicit_full_endpoint_keeps_base_path_and_adds_query() {
        assert_eq!(
            resolve_endpoint_url(
                "https://relay.example/custom/generate",
                "/chat/completions?stream=true",
                true,
            ),
            "https://relay.example/custom/generate?stream=true"
        );
    }

    #[test]
    fn rebuilt_response_headers_preserve_trace_and_drop_transport_fields() {
        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", HeaderValue::from_static("req-1"));
        headers.insert("connection", HeaderValue::from_static("x-debug"));
        headers.insert("x-debug", HeaderValue::from_static("drop"));
        headers.insert("content-length", HeaderValue::from_static("99"));
        headers.insert("content-encoding", HeaderValue::from_static("gzip"));
        headers.insert("content-type", HeaderValue::from_static("text/plain"));

        let rebuilt = response_headers_for_body(&headers, "application/json");
        assert_eq!(rebuilt.get("x-request-id").unwrap(), "req-1");
        assert!(rebuilt.get("connection").is_none());
        assert!(rebuilt.get("x-debug").is_none());
        assert!(rebuilt.get("content-length").is_none());
        assert!(rebuilt.get("content-encoding").is_none());
        assert_eq!(rebuilt.get("content-type").unwrap(), "application/json");
    }

    #[test]
    fn chat_error_is_wrapped_as_responses_error() {
        let body = json!({
            "error": {"message": "bad tool", "type": "invalid_request_error", "code": "tool"}
        });
        let converted = converted_error_body(&body);
        assert_eq!(converted["error"]["message"], "bad tool");
        assert_eq!(converted["error"]["type"], "invalid_request_error");
    }

    #[test]
    fn plain_text_chat_error_is_wrapped_as_responses_error() {
        let converted: Value =
            serde_json::from_slice(&converted_error_bytes(b"Unauthorized")).unwrap();
        assert_eq!(converted["error"]["message"], "Unauthorized");
        assert_eq!(converted["error"]["type"], "upstream_error");
    }
}
