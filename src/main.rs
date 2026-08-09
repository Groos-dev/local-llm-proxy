use async_stream::stream;
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::Response,
    routing::{get, post},
};
use bytes::Bytes;
use futures_util::StreamExt;
use local_llm_proxy::{
    ExchangeLog, ModelRoute, SseModelRestorer, compact_fallback, normalize_request_for_upstream,
    normalize_response_for_client, public_models_list, rewrite_request_model,
};
use serde_json::{Value, json};
use std::{convert::Infallible, env, fs, net::SocketAddr, path::PathBuf};

const DEFAULT_ADA_BASE_URL: &str = "http://ada-cli-golang.ctripcorp.com/coding-plan/openai/v1";
const MODELS_ETAG: &str = "local-llm-proxy-v1";

#[derive(Clone)]
struct AppState {
    client: reqwest::Client,
    api_key: String,
    upstream_base_url: String,
    upstream_responses_url: String,
    exchange_log_dir: PathBuf,
}

#[tokio::main]
async fn main() {
    let api_key = env::var("ADA_API_KEY").expect("ADA_API_KEY must be set");
    let base_url = env::var("ADA_BASE_URL").unwrap_or_else(|_| DEFAULT_ADA_BASE_URL.to_string());
    let bind_addr = env::var("BIND_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8787".to_string())
        .parse::<SocketAddr>()
        .expect("BIND_ADDR must be a socket address");
    let exchange_log_dir = env::var("EXCHANGE_LOG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(".run/exchanges"));
    let _ = fs::create_dir_all(&exchange_log_dir);
    let state = AppState {
        client: reqwest::Client::new(),
        api_key,
        upstream_base_url: base_url.trim_end_matches('/').to_string(),
        upstream_responses_url: format!("{}/responses", base_url.trim_end_matches('/')),
        exchange_log_dir,
    };
    let app = Router::new()
        .route("/v1/models", get(models))
        .route("/v1/responses", post(responses))
        .route("/v1/responses/compact", post(compact))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind_addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn models() -> Response {
    json_response(
        StatusCode::OK,
        public_models_list(),
        Some("application/json"),
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
    let mut payload = codex_request.clone();
    let Some(route) = rewrite_request_model(&mut payload) else {
        return error_response(StatusCode::BAD_REQUEST, "unsupported model");
    };
    normalize_request_for_upstream(route, &mut payload);
    let exchange = ExchangeLog::begin(
        &state.exchange_log_dir,
        &headers,
        &codex_request,
        &payload,
        route.public_model,
    );

    let mut upstream = state
        .client
        .post(&state.upstream_responses_url)
        .bearer_auth(&state.api_key)
        .header(header::CONTENT_TYPE, "application/json")
        .json(&payload);
    upstream = forward_request_headers(upstream, &headers);

    let upstream = match upstream.send().await {
        Ok(response) => response,
        Err(err) => {
            eprintln!("upstream responses request failed: {err}");
            exchange.finish_text(502, "text/plain", err.to_string().as_bytes(), &HeaderMap::new());
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
            normalize_response_for_client(route, &mut body);
            Bytes::from(serde_json::to_vec(&body).unwrap())
        }
        Err(_) => bytes,
    };
    raw_response(
        status,
        body,
        &content_type,
        Some(route.public_model),
        &response_headers,
    )
}

async fn compact(State(state): State<AppState>, request: Request) -> Response {
    let headers = request.headers().clone();
    let body = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(body) => body,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid request body"),
    };
    let mut payload: Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "request body must be JSON"),
    };
    let Some(route) = rewrite_request_model(&mut payload) else {
        return error_response(StatusCode::BAD_REQUEST, "unsupported model");
    };
    normalize_request_for_upstream(route, &mut payload);

    let mut upstream = state
        .client
        .post(format!("{}/responses/compact", state.upstream_base_url))
        .bearer_auth(&state.api_key)
        .header(header::CONTENT_TYPE, "application/json")
        .json(&payload);
    upstream = forward_request_headers(upstream, &headers);

    let upstream = match upstream.send().await {
        Ok(response) => response,
        Err(err) => {
            eprintln!("upstream compact request failed: {err}");
            return compact_unavailable_response(route, &payload, "upstream compact request failed");
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

    if status == StatusCode::NOT_FOUND {
        eprintln!("upstream compact returned 404");
        return compact_unavailable_response(route, &payload, "upstream compact not found");
    }
    if !status.is_success() {
        return raw_response(status, bytes, &content_type, None, &response_headers);
    }

    let body = match serde_json::from_slice::<Value>(&bytes) {
        Ok(mut body) => {
            normalize_response_for_client(route, &mut body);
            Bytes::from(serde_json::to_vec(&body).unwrap())
        }
        Err(_) => bytes,
    };
    raw_response(
        status,
        body,
        &content_type,
        Some(route.public_model),
        &response_headers,
    )
}

fn compact_unavailable_response(route: ModelRoute, payload: &Value, message: &str) -> Response {
    if let Some(body) = compact_fallback(route, payload) {
        eprintln!("using channel local compact for {}", route.public_model);
        return raw_response(
            StatusCode::OK,
            Bytes::from(serde_json::to_vec(&body).unwrap()),
            "application/json",
            Some(route.public_model),
            &HeaderMap::new(),
        );
    }
    error_response(StatusCode::BAD_GATEWAY, message)
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
    let output = stream! {
        let mut restorer = SseModelRestorer::default();
        let mut raw = Vec::new();
        futures_util::pin_mut!(source);
        while let Some(chunk) = source.next().await {
            match chunk {
                Ok(chunk) => {
                    raw.extend_from_slice(&chunk);
                    for event in restorer.push(&chunk, route) {
                        yield Ok::<Bytes, Infallible>(Bytes::from(event));
                    }
                }
                Err(_) => break,
            }
        }
        if let Some(event) = restorer.finish(route) {
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
        Some(route.public_model),
        headers,
    )
}

fn error_response(status: StatusCode, message: &str) -> Response {
    json_response(
        status,
        json!({ "error": { "message": message, "type": "invalid_request_error" } }),
        Some("application/json"),
        false,
    )
}

fn json_response(
    status: StatusCode,
    body: Value,
    content_type: Option<&str>,
    models_etag: bool,
) -> Response {
    raw_response(
        status,
        Bytes::from(serde_json::to_vec(&body).unwrap()),
        content_type.unwrap_or("application/json"),
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
    if let Some(model) = public_model {
        if let Ok(value) = HeaderValue::from_str(model) {
            response
                .headers_mut()
                .insert(HeaderName::from_static("openai-model"), value);
        }
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
