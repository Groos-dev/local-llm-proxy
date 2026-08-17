use async_stream::stream;
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::Response,
    routing::{delete, get, post, put},
};
use bytes::Bytes;
use futures_util::StreamExt;
use local_llm_proxy::{
    ExchangeLog, ModelRoute, ModelRouteConfig, PUBLIC_MODELS, ProviderCatalog, RouteTable,
    SseModelRestorer, normalize_request_for_upstream, normalize_response_for_client, resolve_route,
    rewrite_request_model,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    convert::Infallible,
    env, fs,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, RwLock},
};

const DEFAULT_CONFIG_PATH: &str = "config.toml";
const DEFAULT_ROUTES_PATH: &str = ".run/routes.json";
const MODELS_ETAG: &str = "local-llm-proxy-v1";

#[derive(Clone)]
struct AppState {
    client: reqwest::Client,
    catalog: Arc<ProviderCatalog>,
    routes: Arc<RwLock<RouteTable>>,
    routes_path: PathBuf,
    exchange_log_dir: PathBuf,
}

#[tokio::main]
async fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let config_path = env::var_os("CONFIG_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join(DEFAULT_CONFIG_PATH));
    let config = local_llm_proxy::AppConfig::load(&config_path)
        .unwrap_or_else(|err| panic!("failed to load {}: {err}", config_path.display()));
    let bind_addr = env::var("BIND_ADDR")
        .ok()
        .or(config.bind_addr.clone())
        .unwrap_or_else(|| "127.0.0.1:8787".to_string())
        .parse::<SocketAddr>()
        .expect("BIND_ADDR must be a socket address");
    let exchange_log_dir = env::var("EXCHANGE_LOG_DIR")
        .ok()
        .or(config.exchange_log_dir.clone())
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join(".run/exchanges"));
    let routes_path = env::var_os("ROUTES_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join(DEFAULT_ROUTES_PATH));

    let catalog = ProviderCatalog::new(config)
        .unwrap_or_else(|err| panic!("invalid provider configuration: {err}"));
    let mut routes = RouteTable::load(&routes_path)
        .unwrap_or_else(|err| panic!("failed to load {}: {err}", routes_path.display()));
    let added = routes.ensure_default_self_routes(&catalog);
    if added > 0 {
        routes
            .save(&routes_path)
            .unwrap_or_else(|err| panic!("failed to save {}: {err}", routes_path.display()));
        eprintln!(
            "seeded {added} default self route(s) from {}",
            catalog.default_provider()
        );
    }
    let _ = fs::create_dir_all(&exchange_log_dir);

    let state = AppState {
        client: reqwest::Client::new(),
        catalog: Arc::new(catalog),
        routes: Arc::new(RwLock::new(routes)),
        routes_path,
        exchange_log_dir,
    };

    let app = Router::new()
        .route("/v1/models", get(models))
        .route("/v1/responses", post(responses))
        .route("/v1/responses/compact", post(compact))
        .route("/compact", post(compact))
        .route("/v1/admin/providers", get(providers))
        .route("/v1/admin/routes", get(get_routes))
        .route("/v1/admin/routes/{model}", put(put_route))
        .route("/v1/admin/routes/{model}", delete(delete_route))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind_addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

fn route_for_model(state: &AppState, model: &str) -> Option<ModelRoute> {
    let routes = state.routes.read().expect("routes lock");
    resolve_route(&state.catalog, &routes, model)
}

async fn providers(State(state): State<AppState>) -> Response {
    let data: Vec<Value> = state
        .catalog
        .provider_names()
        .iter()
        .filter_map(|name| state.catalog.get(name))
        .map(|provider| {
            json!({
                "name": provider.name,
                "base_url": provider.base_url,
                "supports_compact": provider.supports_compact,
                "models": provider.model_order.iter().map(|upstream| {
                    json!({
                        "upstream_model": upstream,
                        "response_adapter": provider.adapter_for(upstream),
                    })
                }).collect::<Vec<_>>(),
            })
        })
        .collect();
    json_response(StatusCode::OK, json!({ "providers": data }), true)
}

async fn get_routes(State(state): State<AppState>) -> Response {
    let routes = state.routes.read().expect("routes lock");
    let mut data = serde_json::Map::new();
    for model in PUBLIC_MODELS {
        if let Some(route) = routes.get(model) {
            data.insert(model.to_string(), json!(route));
        }
    }
    json_response(StatusCode::OK, json!({ "routes": data }), true)
}

#[derive(Debug, Deserialize)]
struct SetRouteRequest {
    provider: String,
    upstream_model: String,
}

async fn put_route(
    State(state): State<AppState>,
    Path(model): Path<String>,
    Json(body): Json<SetRouteRequest>,
) -> Response {
    if !PUBLIC_MODELS.contains(&model.as_str()) {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!("unsupported public model '{model}'"),
        );
    }
    if body.upstream_model.trim().is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "upstream_model must not be empty");
    }
    if !state
        .catalog
        .supports_model(&body.provider, &body.upstream_model)
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            "provider does not support this upstream_model",
        );
    }

    let mut routes = state.routes.write().expect("routes lock");
    routes.set(
        model,
        ModelRouteConfig {
            provider: body.provider,
            upstream_model: body.upstream_model,
        },
    );
    if let Err(err) = routes.save(&state.routes_path) {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
    }
    routes_response(StatusCode::OK, &routes)
}

async fn delete_route(State(state): State<AppState>, Path(model): Path<String>) -> Response {
    let mut routes = state.routes.write().expect("routes lock");
    if !routes.remove(&model) {
        return error_response(StatusCode::NOT_FOUND, "route not found");
    }
    if let Err(err) = routes.save(&state.routes_path) {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
    }
    routes_response(StatusCode::OK, &routes)
}

fn routes_response(status: StatusCode, routes: &RouteTable) -> Response {
    let mut data = serde_json::Map::new();
    for model in PUBLIC_MODELS {
        if let Some(route) = routes.get(model) {
            data.insert(model.to_string(), json!(route));
        }
    }
    json_response(status, json!({ "routes": data }), true)
}

async fn models(State(state): State<AppState>) -> Response {
    let routes = state.routes.read().expect("routes lock");
    let data: Vec<Value> = PUBLIC_MODELS
        .iter()
        .filter(|model| routes.get(model).is_some())
        .map(|model| {
            json!({
                "id": model,
                "object": "model",
                "created": 1_700_000_000i64,
                "owned_by": "local-llm-proxy",
            })
        })
        .collect();
    json_response(
        StatusCode::OK,
        json!({ "object": "list", "data": data }),
        true,
    )
}

async fn responses(State(state): State<AppState>, request: Request) -> Response {
    let headers = request.headers().clone();
    let body = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(body) => body,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid request body"),
    };
    let codex_request: Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "request body must be JSON"),
    };
    let Some(model) = codex_request.get("model").and_then(Value::as_str) else {
        return error_response(StatusCode::BAD_REQUEST, "unsupported model");
    };
    let Some(route) = route_for_model(&state, model) else {
        return error_response(StatusCode::BAD_REQUEST, "unsupported model");
    };
    let mut payload = codex_request.clone();
    rewrite_request_model(&route, &mut payload);
    normalize_request_for_upstream(&route, &mut payload);
    let exchange = ExchangeLog::begin(
        &state.exchange_log_dir,
        &headers,
        &codex_request,
        &payload,
        &route.public_model,
        &route.provider_name,
        &route.origin_model,
    );

    let mut upstream = state
        .client
        .post(format!("{}/responses", route.upstream_base_url))
        .bearer_auth(&route.api_key)
        .header(header::CONTENT_TYPE, "application/json")
        .json(&payload);
    upstream = forward_request_headers(upstream, &headers);

    let upstream = match upstream.send().await {
        Ok(response) => response,
        Err(err) => {
            eprintln!("upstream responses request failed: {err}");
            exchange.finish_text(
                502,
                "text/plain",
                err.to_string().as_bytes(),
                &HeaderMap::new(),
            );
            return error_response(StatusCode::BAD_GATEWAY, "upstream request failed");
        }
    };
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let response_headers = upstream.headers().clone();
    let content_type = response_headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/json")
        .to_string();

    if !status.is_success() {
        let bytes = upstream.bytes().await.unwrap_or_default();
        eprintln!(
            "upstream responses status={status} body={}",
            String::from_utf8_lossy(&bytes)
        );
        exchange.finish_text(status.as_u16(), &content_type, &bytes, &response_headers);
        return raw_response(status, bytes, &content_type, None, &response_headers);
    }
    if content_type.starts_with("text/event-stream") {
        return stream_response(upstream, route, &response_headers, Some(exchange));
    }

    let bytes = upstream.bytes().await.unwrap_or_default();
    exchange.finish_text(status.as_u16(), &content_type, &bytes, &response_headers);
    let body = match serde_json::from_slice::<Value>(&bytes) {
        Ok(mut body) => {
            normalize_response_for_client(&route, &mut body);
            Bytes::from(serde_json::to_vec(&body).unwrap())
        }
        Err(_) => bytes,
    };
    raw_response(
        status,
        body,
        &content_type,
        Some(route.public_model.as_str()),
        &response_headers,
    )
}

async fn compact(State(state): State<AppState>, request: Request) -> Response {
    let headers = request.headers().clone();
    let body = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(body) => body,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid request body"),
    };
    let codex_request: Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "request body must be JSON"),
    };
    let Some(model) = codex_request.get("model").and_then(Value::as_str) else {
        return error_response(StatusCode::BAD_REQUEST, "unsupported model");
    };
    let Some(route) = route_for_model(&state, model) else {
        return error_response(StatusCode::BAD_REQUEST, "unsupported model");
    };
    let mut payload = codex_request;
    rewrite_request_model(&route, &mut payload);
    normalize_request_for_upstream(&route, &mut payload);

    if !route.supports_compact {
        return error_response(StatusCode::NOT_FOUND, "provider does not support compact");
    }

    let mut upstream = state
        .client
        .post(format!("{}/responses/compact", route.upstream_base_url))
        .bearer_auth(&route.api_key)
        .header(header::CONTENT_TYPE, "application/json")
        .json(&payload);
    upstream = forward_request_headers(upstream, &headers);

    let upstream = match upstream.send().await {
        Ok(response) => response,
        Err(err) => {
            eprintln!("upstream compact request failed: {err}");
            return error_response(StatusCode::BAD_GATEWAY, "upstream compact request failed");
        }
    };
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let response_headers = upstream.headers().clone();
    let content_type = response_headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let bytes = upstream.bytes().await.unwrap_or_default();

    if !status.is_success() {
        eprintln!(
            "upstream compact status={status} body={}",
            String::from_utf8_lossy(&bytes)
        );
        return raw_response(status, bytes, &content_type, None, &response_headers);
    }

    let body = match serde_json::from_slice::<Value>(&bytes) {
        Ok(mut body) => {
            normalize_response_for_client(&route, &mut body);
            Bytes::from(serde_json::to_vec(&body).unwrap())
        }
        Err(_) => bytes,
    };
    raw_response(
        status,
        body,
        &content_type,
        Some(route.public_model.as_str()),
        &response_headers,
    )
}

fn forward_request_headers(
    mut request: reqwest::RequestBuilder,
    headers: &HeaderMap,
) -> reqwest::RequestBuilder {
    for (name, value) in headers {
        if matches!(
            name,
            &header::AUTHORIZATION
                | &header::CONTENT_LENGTH
                | &header::CONTENT_TYPE
                | &header::HOST
                | &header::TRANSFER_ENCODING
        ) {
            continue;
        }
        request = request.header(name, value);
    }
    request
}

fn stream_response(
    upstream: reqwest::Response,
    route: ModelRoute,
    headers: &HeaderMap,
    exchange: Option<ExchangeLog>,
) -> Response {
    let source = upstream.bytes_stream();
    let response_headers = headers.clone();
    let public_model = route.public_model.clone();
    if let Some(exchange) = exchange.as_ref() {
        exchange.note_sse_headers(&response_headers);
    }
    let output = stream! {
        let mut restorer = SseModelRestorer::default();
        let mut raw = Vec::new();
        futures_util::pin_mut!(source);
        while let Some(chunk) = source.next().await {
            match chunk {
                Ok(chunk) => {
                    raw.extend_from_slice(&chunk);
                    if let Some(exchange) = exchange.as_ref() {
                        exchange.append_sse_chunk(&chunk);
                    }
                    for event in restorer.push(&chunk, &route) {
                        yield Ok::<Bytes, Infallible>(Bytes::from(event));
                    }
                }
                Err(_) => break,
            }
        }
        if let Some(event) = restorer.finish(&route) {
            yield Ok(Bytes::from(event));
        }
        if let Some(exchange) = exchange {
            exchange.finish_text(200, "text/event-stream", &raw, &response_headers);
        }
    };
    raw_response(
        StatusCode::OK,
        Body::from_stream(output),
        "text/event-stream",
        Some(public_model.as_str()),
        headers,
    )
}

fn error_response(status: StatusCode, message: &str) -> Response {
    json_response(
        status,
        json!({ "error": { "message": message, "type": "invalid_request_error" } }),
        false,
    )
}

fn json_response(status: StatusCode, body: Value, models_etag: bool) -> Response {
    raw_response(
        status,
        Bytes::from(serde_json::to_vec(&body).unwrap()),
        "application/json",
        models_etag.then_some(""),
        &HeaderMap::new(),
    )
}

fn raw_response<B: Into<Body>>(
    status: StatusCode,
    body: B,
    content_type: &str,
    public_model: Option<&str>,
    upstream_headers: &HeaderMap,
) -> Response {
    let mut response = Response::new(body.into());
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type).unwrap(),
    );
    if public_model.is_some() || upstream_headers.contains_key("x-models-etag") {
        response.headers_mut().insert(
            HeaderName::from_static("x-models-etag"),
            HeaderValue::from_static(MODELS_ETAG),
        );
    }
    if let Some(model) = public_model
        && let Ok(value) = HeaderValue::from_str(model)
    {
        response
            .headers_mut()
            .insert(HeaderName::from_static("openai-model"), value);
    }
    for name in [
        "request-id",
        "x-request-id",
        "x-trace-id",
        "x-codex-turn-state",
        "x-reasoning-included",
    ] {
        let name = HeaderName::from_static(name);
        if let Some(value) = upstream_headers.get(&name) {
            response.headers_mut().insert(name, value.clone());
        }
    }
    response
}
