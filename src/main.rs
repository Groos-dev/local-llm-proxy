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
    AppConfig, ExchangeLog, ModelRoute, ProviderRegistry, SseModelRestorer,
    build_model_fallback_request, compact_response_from_model_response,
    normalize_request_for_upstream, normalize_response_for_client, rewrite_request_model,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap, convert::Infallible, env, fs, net::SocketAddr, path::PathBuf, sync::Arc,
};

const DEFAULT_CONFIG_PATH: &str = "config.toml";
const MODELS_ETAG: &str = "local-llm-proxy-v1";

#[derive(Clone)]
struct AppState {
    client: reqwest::Client,
    registry: Arc<ProviderRegistry>,
    exchange_log_dir: PathBuf,
}

#[tokio::main]
async fn main() {
    let config_path = env::var_os("CONFIG_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(DEFAULT_CONFIG_PATH));
    let config = AppConfig::load(&config_path)
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
        .unwrap_or_else(|| PathBuf::from(".run/exchanges"));
    let api_keys = config
        .providers
        .iter()
        .map(|provider| {
            env::var(&provider.api_key_env)
                .map(|key| (provider.name.clone(), key))
                .map_err(|err| {
                    format!(
                        "provider '{}' requires {}: {err}",
                        provider.name, provider.api_key_env
                    )
                })
        })
        .collect::<Result<HashMap<_, _>, _>>()
        .unwrap_or_else(|err| panic!("invalid provider credentials: {err}"));
    let registry = ProviderRegistry::new(config, api_keys)
        .unwrap_or_else(|err| panic!("invalid provider configuration: {err}"));
    let _ = fs::create_dir_all(&exchange_log_dir);
    let state = AppState {
        client: reqwest::Client::new(),
        registry: Arc::new(registry),
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

async fn models(State(state): State<AppState>) -> Response {
    json_response(
        StatusCode::OK,
        state.registry.public_models_list(),
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
    let Some(model) = codex_request.get("model").and_then(Value::as_str) else {
        return error_response(StatusCode::BAD_REQUEST, "unsupported model");
    };
    let Some(route) = state.registry.route_for_public_model(model) else {
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
    let Some(route) = state.registry.route_for_public_model(model) else {
        return error_response(StatusCode::BAD_REQUEST, "unsupported model");
    };
    let mut payload = codex_request;
    rewrite_request_model(&route, &mut payload);
    normalize_request_for_upstream(&route, &mut payload);

    if !route.supports_compact {
        return compact_unavailable_response(
            &state,
            &headers,
            &route,
            &payload,
            "provider compact is disabled",
            None,
        )
        .await;
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
            return compact_unavailable_response(
                &state,
                &headers,
                &route,
                &payload,
                "upstream compact request failed",
                None,
            )
            .await;
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
        return compact_unavailable_response(
            &state,
            &headers,
            &route,
            &payload,
            "upstream compact not found",
            Some((status, bytes, content_type, response_headers)),
        )
        .await;
    }
    if !status.is_success() {
        return compact_unavailable_response(
            &state,
            &headers,
            &route,
            &payload,
            "upstream compact failed",
            Some((status, bytes, content_type, response_headers)),
        )
        .await;
    }

    let body = match serde_json::from_slice::<Value>(&bytes) {
        Ok(mut body) => {
            normalize_response_for_client(&route, &mut body);
            Bytes::from(serde_json::to_vec(&body).unwrap())
        }
        Err(_) => {
            return compact_unavailable_response(
                &state,
                &headers,
                &route,
                &payload,
                "upstream compact returned invalid JSON",
                Some((
                    StatusCode::BAD_GATEWAY,
                    bytes,
                    content_type,
                    response_headers,
                )),
            )
            .await;
        }
    };
    raw_response(
        status,
        body,
        &content_type,
        Some(route.public_model.as_str()),
        &response_headers,
    )
}

async fn compact_unavailable_response(
    state: &AppState,
    request_headers: &HeaderMap,
    route: &ModelRoute,
    payload: &Value,
    message: &str,
    upstream: Option<(StatusCode, Bytes, String, HeaderMap)>,
) -> Response {
    if let Some(response) = model_compact_fallback(state, request_headers, route, payload).await {
        eprintln!("using model compact fallback for {}", route.public_model);
        return response;
    }
    if let Some((status, body, content_type, headers)) = upstream {
        return raw_response(status, body, &content_type, None, &headers);
    }
    error_response(StatusCode::BAD_GATEWAY, message)
}

async fn model_compact_fallback(
    state: &AppState,
    request_headers: &HeaderMap,
    route: &ModelRoute,
    payload: &Value,
) -> Option<Response> {
    let mut fallback_payload = build_model_fallback_request(route, payload);
    normalize_request_for_upstream(route, &mut fallback_payload);
    let mut request = state
        .client
        .post(format!("{}/responses", route.upstream_base_url))
        .bearer_auth(&route.api_key)
        .header(header::CONTENT_TYPE, "application/json")
        .json(&fallback_payload);
    request = forward_request_headers(request, request_headers);

    let upstream = match request.send().await {
        Ok(response) => response,
        Err(err) => {
            eprintln!("model compact fallback request failed: {err}");
            return None;
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
    let bytes = upstream.bytes().await.ok()?;
    if !status.is_success() {
        eprintln!(
            "model compact fallback returned status={status} body={}",
            String::from_utf8_lossy(&bytes)
        );
        return None;
    }

    let mut model_response = if content_type.starts_with("text/event-stream") {
        collect_streamed_model_response(route, &bytes)?
    } else {
        serde_json::from_slice::<Value>(&bytes).ok()?
    };
    normalize_response_for_client(route, &mut model_response);
    let compact_response = compact_response_from_model_response(route, model_response)?;
    Some(raw_response(
        StatusCode::OK,
        Bytes::from(serde_json::to_vec(&compact_response).ok()?),
        "application/json",
        Some(route.public_model.as_str()),
        &response_headers,
    ))
}

fn collect_streamed_model_response(route: &ModelRoute, bytes: &[u8]) -> Option<Value> {
    let mut restorer = SseModelRestorer::default();
    let mut events = restorer.push(bytes, route);
    if let Some(event) = restorer.finish(route) {
        events.push(event);
    }

    let mut output_items = Vec::new();
    let mut completed_output = None;
    for event in events {
        let text = std::str::from_utf8(&event).ok()?;
        for line in text.lines() {
            let Some(data) = line.strip_prefix("data:").map(str::trim) else {
                continue;
            };
            if data == "[DONE]" {
                continue;
            }
            let body = serde_json::from_str::<Value>(data).ok()?;
            match body.get("type").and_then(Value::as_str) {
                Some("response.output_item.done") => {
                    if let Some(item) = body.get("item") {
                        output_items.push(item.clone());
                    }
                }
                Some("response.completed") => {
                    completed_output = body
                        .pointer("/response/output")
                        .and_then(Value::as_array)
                        .cloned();
                }
                _ => {}
            }
        }
    }
    let output = completed_output.or_else(|| (!output_items.is_empty()).then_some(output_items))?;
    Some(json!({"output": output}))
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
    let output = stream! {
        let mut restorer = SseModelRestorer::default();
        let mut raw = Vec::new();
        futures_util::pin_mut!(source);
        while let Some(chunk) = source.next().await {
            match chunk {
                Ok(chunk) => {
                    raw.extend_from_slice(&chunk);
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
    if let Some(model) = public_model
        && let Ok(value) = HeaderValue::from_str(model)
    {
        response
            .headers_mut()
            .insert(HeaderName::from_static("openai-model"), value);
    }
    // TODO 这些header 一定会有吗，回写有什么作用, 影响codex工作吗
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

#[cfg(test)]
mod tests {
    use super::*;
    use local_llm_proxy::ChannelKind;

    #[test]
    fn collects_streamed_model_output_for_compact_response() {
        let route = ModelRoute {
            origin_model: "upstream-model".to_string(),
            public_model: "public-model".to_string(),
            provider_name: "provider".to_string(),
            upstream_base_url: "https://example.com/v1".to_string(),
            api_key: "secret".to_string(),
            channel: ChannelKind::DeepSeek,
            supports_compact: false,
        };
        let stream = concat!(
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"summary\"}]}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"upstream-model\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"summary\"}]}]}}\n\n",
            "data: [DONE]\n\n"
        );

        let response = collect_streamed_model_response(&route, stream.as_bytes()).unwrap();
        let compact = compact_response_from_model_response(&route, response).unwrap();

        assert_eq!(compact["model"], "public-model");
        assert_eq!(compact["output"][0]["role"], "assistant");
        assert_eq!(compact["output"][0]["content"][0]["text"], "summary");
    }
}
