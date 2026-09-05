//! 请求转发器

use crate::proxy::cache_injector::{self, anthropic_cache_injection_enabled};
use crate::proxy::handler_context::RequestContext;
use crate::proxy::http_util::{
    AuthStyle, json_error, json_response, relay_error_body, resolve_endpoint_url,
    send_json, sse_response,
};
use crate::proxy::providers::codex_chat_history::record_responses_sse_stream;
use crate::proxy::providers::{
    client_provided_codex_session_id, inject_codex_chat_prompt_cache_key,
    provider_needs_responses_namespace_flatten, resolve_codex_chat_reasoning_config,
    should_convert_codex_responses_to_anthropic, should_convert_codex_responses_to_chat,
    streaming_codex_anthropic::create_responses_sse_stream_from_anthropic_with_context,
    streaming_codex_chat::create_responses_sse_stream_from_chat_with_context,
    transform_codex_anthropic::{
        anthropic_response_to_responses_with_context, responses_request_to_anthropic,
    },
    transform_codex_chat::{
        CodexToolContext, chat_completion_to_response_with_context,
        responses_to_chat_completions_with_reasoning,
    },
    transform_codex_responses_namespace::{self, NamespacedName},
    transform_codex_chat_moonshot_schema,
    transform_codex_responses_xai_sanitize,
};
use crate::proxy::response_processor::{is_sse_response, process_response};
use crate::proxy::server::ProxyState;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde_json::{Value, json};
use std::collections::HashMap;

const DEFAULT_ANTHROPIC_MAX_TOKENS: u64 = 8192;

pub struct RequestForwarder {
    state: ProxyState,
}

pub struct ForwardResult {
    pub response: Response,
}

impl RequestForwarder {
    pub fn new(state: &ProxyState) -> Self {
        Self {
            state: state.clone(),
        }
    }

    /// Name retained for cc-switch parity; single active provider (no failover loop).
    pub(crate) async fn forward_with_retry(
        &self,
        ctx: &mut RequestContext,
        endpoint: &str,
        body: Value,
        tool_context: CodexToolContext,
        namespace_restore_map: HashMap<String, NamespacedName>,
    ) -> ForwardResult {
        let response = self
            .forward(ctx, endpoint, body, tool_context, namespace_restore_map)
            .await;
        ForwardResult { response }
    }

    async fn forward(
        &self,
        ctx: &mut RequestContext,
        endpoint: &str,
        body: Value,
        tool_context: CodexToolContext,
        namespace_restore_map: HashMap<String, NamespacedName>,
    ) -> Response {
        let provider = &ctx.provider;
        if should_convert_codex_responses_to_anthropic(provider, endpoint) {
            return self.forward_anthropic(ctx, body, tool_context).await;
        }
        if should_convert_codex_responses_to_chat(provider, endpoint) {
            return self.forward_chat(ctx, body, tool_context).await;
        }
        self.forward_responses_passthrough(ctx, endpoint, body, namespace_restore_map)
            .await
    }

    async fn forward_responses_passthrough(
        &self,
        ctx: &mut RequestContext,
        endpoint: &str,
        mut body: Value,
        namespace_restore_map: HashMap<String, NamespacedName>,
    ) -> Response {
        let provider = ctx.provider.clone();

        if provider_needs_responses_namespace_flatten(&provider) {
            if let Ok(true) =
                transform_codex_responses_namespace::flatten_request_namespaces(&mut body)
            {
                log::debug!(
                    "[Codex] Flattened namespace tools for native Responses upstream (provider={})",
                    provider.name
                );
            }
            let upstream_model =
                provider.resolve_upstream_model(body.get("model").and_then(|v| v.as_str()));
            transform_codex_responses_xai_sanitize::apply_xai_native_responses_request_compat(
                &mut body,
                &provider.name,
                upstream_model.as_deref(),
                &json!({}),
            );
        }

        let url = resolve_endpoint_url(&provider.base_url, endpoint, provider.is_full_url);
        ctx.exchange.write("upstream_request.json", &body);
        let upstream = match send_json(
            &self.state.client,
            &provider,
            &url,
            &ctx.client_headers,
            &body,
            AuthStyle::Bearer,
            None,
        )
        .await
        {
            Ok(r) => r,
            Err(err) => return json_error(StatusCode::BAD_GATEWAY, err),
        };

        if provider_needs_responses_namespace_flatten(&provider) {
            return handle_xai_native_response(upstream, ctx, namespace_restore_map).await;
        }

        process_response(
            upstream,
            ctx.tag,
            ctx.streaming_timeout,
            &mut ctx.exchange,
        )
        .await
    }

    async fn forward_chat(
        &self,
        ctx: &mut RequestContext,
        mut body: Value,
        tool_context: CodexToolContext,
    ) -> Response {
        let provider = ctx.provider.clone();
        let explicit_prompt_cache_key = body
            .get("prompt_cache_key")
            .and_then(|value| value.as_str())
            .map(ToString::to_string);
        let client_session = client_provided_codex_session_id(&ctx.client_headers, &body);

        let restored = self
            .state
            .codex_chat_history
            .enrich_request(&mut body)
            .await;
        if restored > 0 {
            log::debug!(
                "[Codex] Restored or enriched {restored} cached function call item(s) for Chat upstream"
            );
        }

        let reasoning_config = resolve_codex_chat_reasoning_config(&provider, &body);
        let mut chat_body =
            match responses_to_chat_completions_with_reasoning(body, reasoning_config.as_ref()) {
                Ok(v) => v,
                Err(err) => return json_error(StatusCode::BAD_REQUEST, err.to_string()),
            };
        if let Some(upstream_model) =
            provider.resolve_upstream_model(chat_body.get("model").and_then(|v| v.as_str()))
        {
            chat_body["model"] = json!(upstream_model);
        }
        if transform_codex_chat_moonshot_schema::upstream_requires_ref_sibling_all_of(
            &provider.base_url,
        ) {
            transform_codex_chat_moonshot_schema::wrap_ref_siblings_in_chat_tools(&mut chat_body);
        }
        inject_codex_chat_prompt_cache_key(
            &provider,
            &mut chat_body,
            explicit_prompt_cache_key.as_deref(),
            client_session.as_deref(),
        );

        ctx.exchange.write("upstream_request.json", &chat_body);
        let url = resolve_endpoint_url(
            &provider.base_url,
            "/chat/completions",
            provider.is_full_url,
        );
        let upstream = match send_json(
            &self.state.client,
            &provider,
            &url,
            &ctx.client_headers,
            &chat_body,
            AuthStyle::Bearer,
            None,
        )
        .await
        {
            Ok(r) => r,
            Err(err) => return json_error(StatusCode::BAD_GATEWAY, err),
        };

        let status =
            StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        if !status.is_success() {
            return relay_error_body(upstream, &mut ctx.exchange, true).await;
        }

        if ctx.is_stream || is_sse_response(upstream.headers()) {
            let headers = upstream.headers().clone();
            let stream = upstream.bytes_stream();
            let converted =
                create_responses_sse_stream_from_chat_with_context(stream, tool_context);
            let converted =
                record_responses_sse_stream(converted, self.state.codex_chat_history.clone());
            return sse_response(status, &headers, converted, &mut ctx.exchange);
        }

        let bytes = match upstream.bytes().await {
            Ok(b) => b,
            Err(err) => return json_error(StatusCode::BAD_GATEWAY, err.to_string()),
        };
        ctx.exchange.write_raw("upstream_response.json", &bytes);
        let chat_json: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(err) => {
                return json_error(StatusCode::BAD_GATEWAY, format!("upstream json: {err}"));
            }
        };
        match chat_completion_to_response_with_context(chat_json, &tool_context) {
            Ok(converted) => {
                self.state
                    .codex_chat_history
                    .record_response(&converted)
                    .await;
                ctx.exchange.write("codex_response.json", &converted);
                json_response(status, converted)
            }
            Err(err) => json_error(StatusCode::BAD_GATEWAY, err.to_string()),
        }
    }

    async fn forward_anthropic(
        &self,
        ctx: &mut RequestContext,
        mut body: Value,
        tool_context: CodexToolContext,
    ) -> Response {
        let provider = ctx.provider.clone();
        if let Some(max_out) = provider.max_output_tokens.filter(|v| *v > 0) {
            body["max_output_tokens"] = json!(max_out);
        }
        let mut anthropic_body =
            match responses_request_to_anthropic(body, DEFAULT_ANTHROPIC_MAX_TOKENS) {
                Ok(v) => v,
                Err(err) => return json_error(StatusCode::BAD_REQUEST, err.to_string()),
            };
        cache_injector::inject(&mut anthropic_body, anthropic_cache_injection_enabled());

        ctx.exchange
            .write("upstream_request.json", &anthropic_body);
        let url = resolve_endpoint_url(&provider.base_url, "/v1/messages", provider.is_full_url);

        let upstream = match send_json(
            &self.state.client,
            &provider,
            &url,
            &ctx.client_headers,
            &anthropic_body,
            AuthStyle::Anthropic,
            Some("2023-06-01"),
        )
        .await
        {
            Ok(r) => r,
            Err(err) => return json_error(StatusCode::BAD_GATEWAY, err),
        };

        let status =
            StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        if !status.is_success() {
            return relay_error_body(upstream, &mut ctx.exchange, true).await;
        }

        if ctx.is_stream || is_sse_response(upstream.headers()) {
            let headers = upstream.headers().clone();
            let stream = upstream.bytes_stream();
            let converted =
                create_responses_sse_stream_from_anthropic_with_context(stream, tool_context);
            return sse_response(status, &headers, converted, &mut ctx.exchange);
        }

        let bytes = match upstream.bytes().await {
            Ok(b) => b,
            Err(err) => return json_error(StatusCode::BAD_GATEWAY, err.to_string()),
        };
        ctx.exchange.write_raw("upstream_response.json", &bytes);
        let anthropic_json: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(err) => {
                return json_error(StatusCode::BAD_GATEWAY, format!("upstream json: {err}"));
            }
        };
        match anthropic_response_to_responses_with_context(anthropic_json, &tool_context) {
            Ok(converted) => {
                ctx.exchange.write("codex_response.json", &converted);
                json_response(status, converted)
            }
            Err(err) => json_error(StatusCode::BAD_GATEWAY, err.to_string()),
        }
    }
}

async fn handle_xai_native_response(
    upstream: reqwest::Response,
    ctx: &mut RequestContext,
    namespace_restore_map: HashMap<String, NamespacedName>,
) -> Response {
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if is_sse_response(upstream.headers()) {
        let headers = upstream.headers().clone();
        let stream = upstream.bytes_stream();
        let converted =
            transform_codex_responses_xai_sanitize::create_xai_native_responses_sse_stream(
                stream,
                namespace_restore_map,
            );
        return sse_response(status, &headers, converted, &mut ctx.exchange);
    }

    let bytes = match upstream.bytes().await {
        Ok(b) => b,
        Err(err) => return json_error(StatusCode::BAD_GATEWAY, err.to_string()),
    };
    ctx.exchange.write_raw("upstream_response.json", &bytes);
    let Ok(mut body) = serde_json::from_slice::<Value>(&bytes) else {
        return crate::proxy::http_util::response_with_headers(
            status,
            crate::proxy::http_util::response_headers_for_body(
                &HeaderMap::new(),
                "application/json",
            ),
            bytes,
        );
    };
    let _ = transform_codex_responses_namespace::restore_response_namespaces(
        &mut body,
        &namespace_restore_map,
    );
    let _ = transform_codex_responses_xai_sanitize::normalize_xai_function_call_integer_arguments(
        &mut body,
    );
    ctx.exchange.write("codex_response.json", &body);
    json_response(status, body)
}
