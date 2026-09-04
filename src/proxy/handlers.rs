//! HTTP handlers for Codex Responses (cc-switch-aligned entry).

use crate::proxy::forwarder::RequestForwarder;
use crate::proxy::handler_context::RequestContext;
use crate::proxy::http_util::json_error;
use crate::proxy::providers::transform_codex_chat::build_codex_tool_context_from_request;
use crate::proxy::providers::transform_codex_responses_namespace;
use crate::proxy::server::ProxyState;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Response,
};
use bytes::Bytes;
use serde_json::{Value, json};

/// Handle `/v1/responses` (and aliases).
pub async fn handle_responses(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_responses_inner(state, headers, body, false).await
}

/// Handle `/v1/responses/compact` (and aliases).
pub async fn handle_responses_compact(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_responses_inner(state, headers, body, true).await
}

async fn handle_responses_inner(
    state: ProxyState,
    headers: HeaderMap,
    body: Bytes,
    compact: bool,
) -> Response {
    let provider = state.active_provider().await;
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
    let result = forwarder
        .forward_with_retry(&mut ctx, request_json, tool_context, namespace_restore_map)
        .await;
    result.response
}
