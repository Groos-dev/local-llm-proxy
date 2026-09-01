//! Local Codex Responses proxy with cc-switch-compatible protocol bridges.
//!
//! Codex CLI always talks Responses to this process. The active provider's
//! `api_format` selects:
//! - `openai_responses` → passthrough to upstream `/responses`
//! - `openai_chat` → Chat Completions bridge
//! - `anthropic` → Anthropic Messages bridge

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::Response,
    routing::{get, post},
};
use bytes::Bytes;
use futures_util::StreamExt;
use local_llm_proxy::{
    ApiFormat, ExchangeLog, Provider, default_store_path, load_runtime,
    proxy::providers::{
        streaming_codex_anthropic::create_responses_sse_stream_from_anthropic_with_context,
        streaming_codex_chat::create_responses_sse_stream_from_chat_with_context,
        transform_codex_anthropic::{
            anthropic_response_to_responses_with_context, responses_request_to_anthropic,
        },
        transform_codex_chat::{
            CodexToolContext, build_codex_tool_context_from_request,
            chat_completion_to_response_with_context, chat_error_to_response_error,
            responses_to_chat_completions_with_reasoning,
        },
    },
    store::LlpxStore,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{convert::Infallible, env, fs, net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::sync::RwLock;

const DEFAULT_CONFIG_PATH: &str = "config.toml";
const DEFAULT_ANTHROPIC_MAX_TOKENS: u64 = 8192;

#[derive(Clone)]
struct AppState {
    client: reqwest::Client,
    runtime: Arc<RwLock<RuntimeProviders>>,
    exchange_log_dir: PathBuf,
}

struct RuntimeProviders {
    /// JSON store path (preferred) used when hot-switching active provider.
    store_path: PathBuf,
    active_name: String,
    providers: Vec<Provider>,
}

impl RuntimeProviders {
    fn active(&self) -> &Provider {
        self.providers
            .iter()
            .find(|p| p.name == self.active_name)
            .expect("active provider missing")
    }

    fn list_public(&self) -> Value {
        json!({
            "active": self.active_name,
            "providers": self.providers.iter().map(|p| json!({
                "name": p.name,
                "api_format": p.api_format.as_str(),
                "base_url": p.base_url,
                "upstream_model": p.upstream_model,
                "model_mappings": p.model_mappings,
            })).collect::<Vec<_>>(),
        })
    }

    fn set_active(&mut self, name: &str) -> Result<&Provider, String> {
        let mut store = LlpxStore::load(&self.store_path).map_err(|e| e.to_string())?;
        store.set_active(name).map_err(|e| e.to_string())?;
        let (active_name, providers) = store.clone().into_providers().map_err(|e| e.to_string())?;
        store.save(&self.store_path).map_err(|e| e.to_string())?;
        self.active_name = active_name;
        self.providers = providers;
        Ok(self.active())
    }
}

impl AppState {
    async fn active_provider(&self) -> Provider {
        self.runtime.read().await.active().clone()
    }
}

#[tokio::main]
async fn main() {
    let _ = env_logger::try_init();

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let toml_path = env::var_os("CONFIG_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join(DEFAULT_CONFIG_PATH));
    let store_path = env::var_os("LLPX_STORE")
        .map(PathBuf::from)
        .unwrap_or_else(default_store_path);
    let (store, store_path) = load_runtime(&store_path, Some(&toml_path))
        .unwrap_or_else(|err| panic!("failed to load config/store: {err}"));
    let bind_addr = env::var("BIND_ADDR")
        .ok()
        .or(store.bind_addr.clone())
        .unwrap_or_else(|| "127.0.0.1:8787".to_string())
        .parse::<SocketAddr>()
        .expect("BIND_ADDR must be a socket address");
    let exchange_log_dir = env::var("EXCHANGE_LOG_DIR")
        .ok()
        .or(store.exchange_log_dir.clone())
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join(".run/exchanges"));
    let _ = fs::create_dir_all(&exchange_log_dir);

    let (active_name, providers) = store
        .into_providers()
        .unwrap_or_else(|err| panic!("invalid provider configuration: {err}"));
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .unwrap_or_else(|err| panic!("failed to bind {bind_addr}: {err}"));

    let runtime = RuntimeProviders {
        store_path: store_path.clone(),
        active_name: active_name.clone(),
        providers,
    };
    let active_snapshot = (
        runtime.active().name.clone(),
        runtime.active().api_format.as_str(),
        runtime.active().base_url.clone(),
    );
    let state = AppState {
        client: reqwest::Client::new(),
        runtime: Arc::new(RwLock::new(runtime)),
        exchange_log_dir,
    };

    eprintln!(
        "local-llm-proxy listening on {bind_addr} provider={} api_format={} → {}",
        active_snapshot.0, active_snapshot.1, active_snapshot.2
    );

    let app = Router::new()
        .route("/models", get(models))
        .route("/v1/models", get(models))
        .route("/v1/responses", post(responses))
        .route("/v1/v1/responses", post(responses))
        .route("/codex/v1/responses", post(responses))
        .route("/v1/responses/compact", post(compact))
        .route("/responses", post(responses))
        .route("/responses/compact", post(compact))
        .route("/compact", post(compact))
        .route("/v1/v1/responses/compact", post(compact))
        .route("/codex/v1/responses/compact", post(compact))
        .route("/health", get(health))
        .route("/v1/admin/providers", get(admin_providers))
        .route("/v1/admin/active", post(admin_set_active))
        .with_state(state);

    axum::serve(listener, app).await.unwrap();
}

async fn health(State(state): State<AppState>) -> Json<Value> {
    let rt = state.runtime.read().await;
    let p = rt.active();
    Json(json!({
        "ok": true,
        "active_provider": p.name,
        "api_format": p.api_format.as_str(),
        "upstream": p.base_url,
        "upstream_model": p.upstream_model,
    }))
}

async fn admin_providers(State(state): State<AppState>) -> Json<Value> {
    Json(state.runtime.read().await.list_public())
}

#[derive(Deserialize)]
struct SetActiveBody {
    name: String,
}

async fn admin_set_active(
    State(state): State<AppState>,
    Json(body): Json<SetActiveBody>,
) -> Response {
    let mut rt = state.runtime.write().await;
    match rt.set_active(&body.name) {
        Ok(p) => {
            eprintln!(
                "hot-switched active provider → {} ({}) {}",
                p.name,
                p.api_format.as_str(),
                p.base_url
            );
            json_response(
                StatusCode::OK,
                json!({
                    "ok": true,
                    "active": p.name,
                    "api_format": p.api_format.as_str(),
                    "upstream": p.base_url,
                    "upstream_model": p.upstream_model,
                }),
            )
        }
        Err(err) => json_error(StatusCode::NOT_FOUND, err),
    }
}

async fn models(State(state): State<AppState>) -> Json<Value> {
    let p = state.active_provider().await;
    let ids = public_model_ids(&p);
    Json(json!({
        "object": "list",
        "data": ids.into_iter().map(|id| json!({
            "id": id,
            "object": "model",
            "owned_by": "local-llm-proxy"
        })).collect::<Vec<_>>()
    }))
}

fn public_model_ids(provider: &Provider) -> Vec<String> {
    let mut ids = provider.model_mappings.keys().cloned().collect::<Vec<_>>();
    if ids.is_empty()
        && let Some(model) = provider.upstream_model.as_ref()
    {
        ids.push(model.clone());
    }
    ids.sort();
    ids.dedup();
    ids
}

async fn responses(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    forward_responses(&state, headers, body, false).await
}

async fn compact(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    // Align with cc-switch: Compact uses the same provider routing as /responses.
    // openai_responses → passthrough /responses/compact; chat/anthropic → same bridges
    // (endpoint rewritten upstream). Do not 404 solely because api_format is a bridge.
    forward_responses(&state, headers, body, true).await
}

async fn forward_responses(
    state: &AppState,
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

    let mut exchange = ExchangeLog::create(&state.exchange_log_dir);
    exchange.write("codex_request.json", &request_json);

    match provider.api_format {
        ApiFormat::OpenaiResponses => {
            passthrough(
                state,
                &provider,
                headers,
                request_json,
                compact,
                &mut exchange,
            )
            .await
        }
        ApiFormat::OpenaiChat => {
            // Compact on a Chat upstream is bridged like Responses (cc-switch
            // rewrites /responses/compact → /chat/completions).
            chat_bridge(
                state,
                &provider,
                headers,
                request_json,
                is_stream,
                tool_context,
                &mut exchange,
            )
            .await
        }
        ApiFormat::Anthropic => {
            // Compact on Anthropic upstream uses the Messages bridge (cc-switch
            // rewrites /responses/compact → /v1/messages).
            anthropic_bridge(
                state,
                &provider,
                headers,
                request_json,
                is_stream,
                tool_context,
                &mut exchange,
            )
            .await
        }
    }
}

async fn passthrough(
    state: &AppState,
    provider: &Provider,
    headers: HeaderMap,
    body: Value,
    compact: bool,
    exchange: &mut ExchangeLog,
) -> Response {
    let path = if compact {
        "/responses/compact"
    } else {
        "/responses"
    };
    let url = join_url(&provider.base_url, path);
    exchange.write("upstream_request.json", &body);
    let upstream = match send_json(
        state,
        provider,
        &url,
        &headers,
        &body,
        AuthStyle::Bearer,
        None,
    )
    .await
    {
        Ok(r) => r,
        Err(err) => return json_error(StatusCode::BAD_GATEWAY, err),
    };
    relay_upstream(upstream, exchange).await
}

async fn chat_bridge(
    state: &AppState,
    provider: &Provider,
    headers: HeaderMap,
    body: Value,
    is_stream: bool,
    tool_context: CodexToolContext,
    exchange: &mut ExchangeLog,
) -> Response {
    let mut chat_body = match responses_to_chat_completions_with_reasoning(body, None) {
        Ok(v) => v,
        Err(err) => return json_error(StatusCode::BAD_REQUEST, err.to_string()),
    };
    if let Some(upstream_model) =
        provider.resolve_upstream_model(chat_body.get("model").and_then(|v| v.as_str()))
    {
        chat_body["model"] = json!(upstream_model);
    }
    exchange.write("upstream_request.json", &chat_body);
    let url = join_url(&provider.base_url, "/chat/completions");
    let upstream = match send_json(
        state,
        provider,
        &url,
        &headers,
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
        return relay_error_body(upstream, exchange, true).await;
    }

    if is_stream || content_type_is_sse(upstream.headers()) {
        let headers = upstream.headers().clone();
        let stream = upstream.bytes_stream();
        let converted = create_responses_sse_stream_from_chat_with_context(stream, tool_context);
        return sse_response(status, &headers, converted, exchange);
    }

    let bytes = match upstream.bytes().await {
        Ok(b) => b,
        Err(err) => return json_error(StatusCode::BAD_GATEWAY, err.to_string()),
    };
    exchange.write_raw("upstream_response.json", &bytes);
    let chat_json: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(err) => return json_error(StatusCode::BAD_GATEWAY, format!("upstream json: {err}")),
    };
    match chat_completion_to_response_with_context(chat_json, &tool_context) {
        Ok(converted) => {
            exchange.write("codex_response.json", &converted);
            json_response(status, converted)
        }
        Err(err) => json_error(StatusCode::BAD_GATEWAY, err.to_string()),
    }
}

async fn anthropic_bridge(
    state: &AppState,
    provider: &Provider,
    headers: HeaderMap,
    mut body: Value,
    is_stream: bool,
    tool_context: CodexToolContext,
    exchange: &mut ExchangeLog,
) -> Response {
    if let Some(max_out) = provider.max_output_tokens.filter(|v| *v > 0) {
        body["max_output_tokens"] = json!(max_out);
    }
    let anthropic_body = match responses_request_to_anthropic(body, DEFAULT_ANTHROPIC_MAX_TOKENS) {
        Ok(v) => v,
        Err(err) => return json_error(StatusCode::BAD_REQUEST, err.to_string()),
    };
    exchange.write("upstream_request.json", &anthropic_body);
    let url = join_url(&provider.base_url, "/v1/messages");
    // If base_url already ends with /v1/messages, join_url may double — normalize.
    let url = if provider
        .base_url
        .trim_end_matches('/')
        .ends_with("/v1/messages")
    {
        provider.base_url.trim_end_matches('/').to_string()
    } else if provider.base_url.trim_end_matches('/').ends_with("/v1") {
        format!("{}/messages", provider.base_url.trim_end_matches('/'))
    } else {
        url
    };

    let upstream = match send_json(
        state,
        provider,
        &url,
        &headers,
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
        return relay_error_body(upstream, exchange, true).await;
    }

    if is_stream || content_type_is_sse(upstream.headers()) {
        let headers = upstream.headers().clone();
        let stream = upstream.bytes_stream();
        let converted =
            create_responses_sse_stream_from_anthropic_with_context(stream, tool_context);
        return sse_response(status, &headers, converted, exchange);
    }

    let bytes = match upstream.bytes().await {
        Ok(b) => b,
        Err(err) => return json_error(StatusCode::BAD_GATEWAY, err.to_string()),
    };
    exchange.write_raw("upstream_response.json", &bytes);
    let anthropic_json: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(err) => return json_error(StatusCode::BAD_GATEWAY, format!("upstream json: {err}")),
    };
    match anthropic_response_to_responses_with_context(anthropic_json, &tool_context) {
        Ok(converted) => {
            exchange.write("codex_response.json", &converted);
            json_response(status, converted)
        }
        Err(err) => json_error(StatusCode::BAD_GATEWAY, err.to_string()),
    }
}

#[derive(Clone, Copy)]
enum AuthStyle {
    Bearer,
    Anthropic,
}

async fn send_json(
    state: &AppState,
    provider: &Provider,
    url: &str,
    client_headers: &HeaderMap,
    body: &Value,
    auth: AuthStyle,
    anthropic_version: Option<&str>,
) -> Result<reqwest::Response, String> {
    let mut req = state.client.post(url).json(body);
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
    // Some gateways (Cloudflare) reject bare library UAs; prefer Codex client UA.
    let ua = client_headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .unwrap_or("codex_cli_rs/0.149.1");
    req = req.header(header::USER_AGENT, ua);
    // Forward a small allow-list of Codex headers.
    for name in ["x-codex-turn-state", "x-request-id", "openai-beta"] {
        if let Some(value) = client_headers.get(name) {
            if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
                req = req.header(name, value);
            }
        }
    }
    req.send().await.map_err(|e| e.to_string())
}

async fn relay_upstream(upstream: reqwest::Response, exchange: &mut ExchangeLog) -> Response {
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let headers = upstream.headers().clone();
    if content_type_is_sse(upstream.headers()) {
        let headers = upstream.headers().clone();
        let stream = upstream.bytes_stream().map(|chunk| {
            chunk
                .map_err(|err| std::io::Error::other(err.to_string()))
                .map(Bytes::from)
        });
        return sse_response(status, &headers, stream, exchange);
    }
    let bytes = match upstream.bytes().await {
        Ok(b) => b,
        Err(err) => return json_error(StatusCode::BAD_GATEWAY, err.to_string()),
    };
    exchange.write_raw("upstream_response.json", &bytes);
    response_with_headers(status, response_headers_for_passthrough(&headers), bytes)
}

async fn relay_error_body(
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

fn sse_response<S, E>(
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

fn json_response(status: StatusCode, value: Value) -> Response {
    let bytes = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
    response_with_headers(
        status,
        response_headers_for_body(&HeaderMap::new(), "application/json"),
        bytes,
    )
}

fn response_with_headers<B: Into<Body>>(
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

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    let body = json!({ "error": { "message": message.into(), "type": "proxy_error" } });
    json_response(status, body)
}

fn content_type_is_sse(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"))
}

const HOP_BY_HOP_HEADERS: [&str; 10] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

fn response_headers_for_passthrough(headers: &HeaderMap) -> HeaderMap {
    let mut headers = headers.clone();
    remove_hop_by_hop_headers(&mut headers);
    headers
}

fn response_headers_for_body(headers: &HeaderMap, content_type: &str) -> HeaderMap {
    let mut headers = response_headers_for_passthrough(headers);
    headers.remove(header::CONTENT_ENCODING);
    headers.remove(header::CONTENT_LENGTH);
    headers.remove(header::TRANSFER_ENCODING);
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type).unwrap_or(HeaderValue::from_static("application/json")),
    );
    headers
}

fn remove_hop_by_hop_headers(headers: &mut HeaderMap) {
    let connection_names = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect::<Vec<_>>();
    for name in HOP_BY_HOP_HEADERS {
        headers.remove(name);
    }
    for name in connection_names {
        headers.remove(name);
    }
}

fn converted_error_body(body: &Value) -> Value {
    chat_error_to_response_error(Some(body))
}

fn converted_error_bytes(bytes: &[u8]) -> Vec<u8> {
    let value = serde_json::from_slice::<Value>(bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(bytes).into_owned()));
    serde_json::to_vec(&converted_error_body(&value)).unwrap_or_else(|_| {
        br#"{"error":{"message":"upstream error","type":"upstream_error"}}"#.to_vec()
    })
}

fn join_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    // Avoid /v1/v1 when base already ends with /v1 and path starts with /v1
    if base.ends_with("/v1") && path.starts_with("/v1/") {
        return format!("{}{}", base.trim_end_matches("/v1"), path);
    }
    // Match cc-switch: an origin-only base URL uses the provider's /v1 API prefix.
    let origin_only = reqwest::Url::parse(base)
        .ok()
        .is_some_and(|url| matches!(url.path(), "" | "/"));
    if origin_only && path != "/v1" && !path.starts_with("/v1/") {
        return format!("{base}/v1{path}");
    }
    format!("{base}{path}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use std::collections::HashMap;

    #[test]
    fn models_expose_mapping_keys_and_fallback_model() {
        let provider = Provider {
            name: "test".into(),
            base_url: "https://example.com/v1".into(),
            api_key: "key".into(),
            api_format: ApiFormat::OpenaiResponses,
            upstream_model: Some("fallback".into()),
            max_output_tokens: None,
            model_mappings: HashMap::from([
                ("zeta".into(), "upstream-z".into()),
                ("alpha".into(), "upstream-a".into()),
            ]),
        };
        assert_eq!(public_model_ids(&provider), ["alpha", "zeta"]);

        let mut fallback = provider;
        fallback.model_mappings.clear();
        assert_eq!(public_model_ids(&fallback), ["fallback"]);
    }

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
    }

    #[test]
    fn hot_switch_reloads_provider_config_from_store() {
        use local_llm_proxy::StoredProvider;
        use std::{collections::BTreeMap, fs};

        let dir = std::env::temp_dir().join(format!(
            "llpx-runtime-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store.json");
        let provider = |name: &str, model: &str| StoredProvider {
            name: name.into(),
            base_url: "https://example.com/v1".into(),
            api_key: "key".into(),
            api_format: ApiFormat::OpenaiResponses,
            default_upstream_model: Some(model.into()),
            model_mappings: BTreeMap::from([(model.into(), model.into())]),
            max_output_tokens: None,
        };
        let original = LlpxStore {
            version: 1,
            bind_addr: None,
            exchange_log_dir: None,
            codex_active: true,
            active_provider: "a".into(),
            providers: vec![provider("a", "model-a")],
        };
        original.save(&path).unwrap();
        let (active_name, providers) = original.into_providers().unwrap();
        let mut runtime = RuntimeProviders {
            store_path: path.clone(),
            active_name,
            providers,
        };

        let updated = LlpxStore {
            version: 1,
            bind_addr: None,
            exchange_log_dir: None,
            codex_active: true,
            active_provider: "a".into(),
            providers: vec![provider("a", "model-a"), provider("b", "model-b")],
        };
        updated.save(&path).unwrap();

        let active = runtime.set_active("b").unwrap();
        assert_eq!(active.name, "b");
        assert_eq!(
            active.resolve_upstream_model(Some("model-b")).as_deref(),
            Some("model-b")
        );
        let _ = fs::remove_dir_all(&dir);
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
