//! HTTP handlers for Codex Responses (cc-switch-aligned entry).

use crate::proxy::forwarder::RequestForwarder;
use crate::proxy::handler_context::RequestContext;
use crate::proxy::content_encoding::{
    decompress_body_with_limit, get_content_encoding, is_supported_content_encoding,
    MAX_REQUEST_BODY_BYTES,
};
use crate::proxy::http_util::json_error;
use crate::proxy::providers::transform_codex_chat::build_codex_tool_context_from_request;
use crate::proxy::providers::transform_codex_responses_namespace;
use crate::proxy::server::ProxyState;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};
use bytes::Bytes;
use serde_json::{Value, json};

/// Handle `/v1/responses` (and aliases).
pub async fn handle_responses(
    State(state): State<ProxyState>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_responses_inner(state, uri, headers, body, false, "/responses").await
}

/// Handle `/v1/responses/compact` (and aliases).
pub async fn handle_responses_compact(
    State(state): State<ProxyState>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_responses_inner(state, uri, headers, body, true, "/responses/compact").await
}

/// Handle Codex's standalone Alpha Search endpoint.
pub async fn handle_alpha_search(
    State(state): State<ProxyState>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_responses_inner(state, uri, headers, body, false, "/alpha/search").await
}

async fn handle_responses_inner(
    state: ProxyState,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
    compact: bool,
    endpoint: &str,
) -> Response {
    let provider = state.active_provider().await;
    let mut headers = headers;
    let body = match decode_request_body(&mut headers, body) {
        Ok(body) => body,
        Err(err) => return json_error(StatusCode::BAD_REQUEST, err),
    };
    let mut request_json: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            return json_error(StatusCode::BAD_REQUEST, format!("invalid json: {err}"));
        }
    };

    if let Some(upstream_model) =
        provider.resolve_upstream_model(request_json.get("model").and_then(|v| v.as_str()))
    {
        request_json["model"] = json!(upstream_model);
    }

    let is_stream = request_json
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let tool_context = build_codex_tool_context_from_request(&request_json);
    let namespace_restore_map =
        transform_codex_responses_namespace::namespace_restore_map(&request_json);

    let mut ctx = RequestContext::new(
        provider,
        headers,
        &state.exchange_log_dir,
        compact,
        is_stream,
    );
    ctx.exchange.write("codex_request.json", &request_json);

    let forwarder = RequestForwarder::new(&state);
    let endpoint = endpoint_with_query(&uri, endpoint);
    let result = forwarder
        .forward_with_retry(
            &mut ctx,
            &endpoint,
            request_json,
            tool_context,
            namespace_restore_map,
        )
        .await;
    result.response
}

fn endpoint_with_query(uri: &Uri, endpoint: &str) -> String {
    match uri.query() {
        Some(query) if !query.is_empty() => format!("{endpoint}?{query}"),
        _ => endpoint.to_string(),
    }
}

fn decode_request_body(headers: &mut HeaderMap, body: Bytes) -> Result<Bytes, String> {
    let Some(content_encoding) = get_content_encoding(headers) else {
        return Ok(body);
    };
    if !is_supported_content_encoding(&content_encoding) {
        return Err(format!("unsupported content-encoding: {content_encoding}"));
    }
    let decoded = decompress_body_with_limit(
        &content_encoding,
        &body,
        MAX_REQUEST_BODY_BYTES,
    )
    .map_err(|err| err.to_string())?
    .ok_or_else(|| format!("unsupported content-encoding: {content_encoding}"))?;
    headers.remove("content-encoding");
    headers.remove("content-length");
    headers.remove("transfer-encoding");
    Ok(Bytes::from(decoded))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn endpoint_preserves_query_string() {
        let uri: Uri = "/v1/alpha/search?query=latest%20model&limit=5".parse().unwrap();
        assert_eq!(
            endpoint_with_query(&uri, "/alpha/search"),
            "/alpha/search?query=latest%20model&limit=5"
        );
    }

    #[test]
    fn decode_request_body_rewrites_headers_after_gzip() {
        let payload = br#"{"model":"gpt-5.4"}"#;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(payload).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", "gzip".parse().unwrap());
        headers.insert("content-length", "99".parse().unwrap());
        headers.insert("transfer-encoding", "chunked".parse().unwrap());

        assert_eq!(
            decode_request_body(&mut headers, Bytes::from(compressed))
                .unwrap()
                .as_ref(),
            payload
        );
        assert!(headers.get("content-encoding").is_none());
        assert!(headers.get("content-length").is_none());
        assert!(headers.get("transfer-encoding").is_none());
    }

    #[test]
    fn decode_request_body_rejects_unknown_encoding() {
        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", "compress".parse().unwrap());
        let error = decode_request_body(&mut headers, Bytes::from_static(b"body")).unwrap_err();
        assert!(error.contains("unsupported content-encoding"));
    }
}
