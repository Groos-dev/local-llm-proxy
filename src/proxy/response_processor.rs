//! Responses passthrough response path (aligned with cc-switch `response_processor`).
//!
//! Usage / DB logging is intentionally omitted; stream timeouts and hop-by-hop
//! header stripping match the cc-switch skeleton.

use crate::exchange::ExchangeLog;
use crate::proxy::sse::{append_utf8_safe, strip_sse_field, take_sse_block};
use axum::{
    body::Body,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::Response,
};
use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::Stream;
use std::time::Duration;

pub use crate::proxy::handler_context::StreamingTimeoutConfig;

const HOP_BY_HOP_RESPONSE_HEADERS: &[&str] = &[
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

/// Remove hop-by-hop response headers and any names listed in `Connection`.
pub fn strip_hop_by_hop_response_headers(headers: &mut HeaderMap) {
    let connection_listed_headers: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .filter_map(|name| HeaderName::from_bytes(name.as_bytes()).ok())
        .collect();

    for name in HOP_BY_HOP_RESPONSE_HEADERS {
        headers.remove(*name);
    }

    for name in connection_listed_headers {
        headers.remove(name);
    }
}

/// Drop entity headers that become wrong after rebuilding the body.
pub fn strip_entity_headers_for_rebuilt_body(headers: &mut HeaderMap) {
    headers.remove(header::CONTENT_ENCODING);
    headers.remove(header::CONTENT_LENGTH);
    headers.remove(header::TRANSFER_ENCODING);
}

pub fn is_sse_response(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("text/event-stream"))
}

/// Create a passthrough byte stream with first-byte / idle timeouts.
///
/// Structure matches cc-switch `create_logged_passthrough_stream`; usage collector
/// is always absent (no DB logging).
pub fn create_logged_passthrough_stream(
    stream: impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
    tag: &'static str,
    timeout_config: StreamingTimeoutConfig,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    async_stream::stream! {
        let mut buffer = String::new();
        let mut utf8_remainder: Vec<u8> = Vec::new();
        // usage_collector intentionally omitted — inspect only when debug logging.
        let inspect_sse_events = log::log_enabled!(log::Level::Debug);
        let mut is_first_chunk = true;

        let first_byte_timeout = if timeout_config.first_byte_timeout > 0 {
            Some(Duration::from_secs(timeout_config.first_byte_timeout))
        } else {
            None
        };
        let idle_timeout = if timeout_config.idle_timeout > 0 {
            Some(Duration::from_secs(timeout_config.idle_timeout))
        } else {
            None
        };

        tokio::pin!(stream);

        loop {
            let timeout_duration = if is_first_chunk {
                first_byte_timeout
            } else {
                idle_timeout
            };

            let chunk_result = match timeout_duration {
                Some(duration) => {
                    match tokio::time::timeout(duration, stream.next()).await {
                        Ok(Some(chunk)) => Some(chunk),
                        Ok(None) => None,
                        Err(_) => {
                            let timeout_type = if is_first_chunk { "首字节" } else { "静默期" };
                            log::error!(
                                "[{tag}] 流式响应{}超时 ({}秒)",
                                timeout_type,
                                duration.as_secs()
                            );
                            yield Err(std::io::Error::other(format!(
                                "流式响应{timeout_type}超时"
                            )));
                            break;
                        }
                    }
                }
                None => stream.next().await,
            };

            match chunk_result {
                Some(Ok(bytes)) => {
                    if is_first_chunk {
                        log::debug!("[{tag}] 已接收上游流式首包: bytes={}", bytes.len());
                    }
                    is_first_chunk = false;
                    if inspect_sse_events {
                        append_utf8_safe(&mut buffer, &mut utf8_remainder, &bytes);
                        while let Some(event_text) = take_sse_block(&mut buffer) {
                            if event_text.trim().is_empty() {
                                continue;
                            }
                            for line in event_text.lines() {
                                if let Some(data) = strip_sse_field(line, "data") {
                                    if data.trim() == "[DONE]" {
                                        log::debug!("[{tag}] <<< SSE: [DONE]");
                                    } else {
                                        log::trace!(
                                            "[{tag}] <<< SSE data: bytes={} (content omitted)",
                                            data.len()
                                        );
                                    }
                                }
                            }
                        }
                    }

                    yield Ok(bytes);
                }
                Some(Err(e)) => {
                    log::error!("[{tag}] 流错误: {e}");
                    yield Err(std::io::Error::other(e.to_string()));
                    break;
                }
                None => break,
            }
        }
    }
}

/// Process an upstream Responses (or other) reply as a byte-level passthrough.
/// Alias aligned with cc-switch `process_response`.
pub async fn process_response(
    upstream: reqwest::Response,
    tag: &'static str,
    timeout_config: StreamingTimeoutConfig,
    exchange: &mut ExchangeLog,
) -> Response {
    process_upstream_response(upstream, tag, timeout_config, exchange).await
}

pub async fn process_upstream_response(
    upstream: reqwest::Response,
    tag: &'static str,
    timeout_config: StreamingTimeoutConfig,
    exchange: &mut ExchangeLog,
) -> Response {
    if is_sse_response(upstream.headers()) {
        handle_streaming(upstream, tag, timeout_config, exchange)
    } else {
        handle_non_streaming(upstream, tag, exchange).await
    }
}

fn handle_streaming(
    upstream: reqwest::Response,
    tag: &'static str,
    timeout_config: StreamingTimeoutConfig,
    exchange: &mut ExchangeLog,
) -> Response {
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    log::debug!(
        "[{tag}] 已接收上游流式响应: status={}",
        status.as_u16()
    );

    let mut response_headers = upstream.headers().clone();
    strip_hop_by_hop_response_headers(&mut response_headers);

    exchange.mark_streaming();

    let stream = upstream.bytes_stream().map(|chunk| {
        chunk
            .map_err(|err| std::io::Error::other(err.to_string()))
            .map(Bytes::from)
    });
    let logged_stream = create_logged_passthrough_stream(stream, tag, timeout_config);

    let mut builder = Response::builder().status(status);
    for (key, value) in &response_headers {
        builder = builder.header(key, value);
    }

    match builder.body(Body::from_stream(logged_stream)) {
        Ok(resp) => resp,
        Err(e) => {
            log::error!("[{tag}] 构建流式响应失败: {e}");
            json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to build streaming response: {e}"),
            )
        }
    }
}

async fn handle_non_streaming(
    upstream: reqwest::Response,
    tag: &'static str,
    exchange: &mut ExchangeLog,
) -> Response {
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response_headers = upstream.headers().clone();
    strip_hop_by_hop_response_headers(&mut response_headers);

    let body_bytes = match upstream.bytes().await {
        Ok(b) => b,
        Err(e) => {
            log::error!("[{tag}] 读取上游响应体失败: {e}");
            return json_error_response(StatusCode::BAD_GATEWAY, e.to_string());
        }
    };

    log::debug!(
        "[{tag}] 上游响应体已接收: bytes={} (content omitted)",
        body_bytes.len()
    );
    exchange.write_raw("upstream_response.json", &body_bytes);

    let mut builder = Response::builder().status(status);
    for (key, value) in response_headers.iter() {
        builder = builder.header(key, value);
    }

    match builder.body(Body::from(body_bytes)) {
        Ok(resp) => resp,
        Err(e) => {
            log::error!("[{tag}] 构建响应失败: {e}");
            json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to build response: {e}"),
            )
        }
    }
}

fn json_error_response(status: StatusCode, message: impl Into<String>) -> Response {
    let body = serde_json::json!({
        "error": { "message": message.into(), "type": "proxy_error" }
    });
    let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let mut builder = Response::builder().status(status);
    for (name, value) in headers {
        if let Some(name) = name {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(Body::from(bytes))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_hop_by_hop_removes_standard_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
        headers.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        headers.insert("transfer-encoding", HeaderValue::from_static("chunked"));
        headers.insert("x-request-id", HeaderValue::from_static("abc"));

        strip_hop_by_hop_response_headers(&mut headers);

        assert!(headers.get(header::CONTENT_TYPE).is_some());
        assert!(headers.get("x-request-id").is_some());
        assert!(headers.get(header::CONNECTION).is_none());
        assert!(headers.get("keep-alive").is_none());
        assert!(headers.get("transfer-encoding").is_none());
    }

    #[test]
    fn strip_hop_by_hop_removes_connection_listed_extensions() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONNECTION,
            HeaderValue::from_static("keep-alive, x-foo-close"),
        );
        headers.insert("x-foo-close", HeaderValue::from_static("1"));
        headers.insert("x-keep", HeaderValue::from_static("yes"));

        strip_hop_by_hop_response_headers(&mut headers);

        assert!(headers.get("x-foo-close").is_none());
        assert!(headers.get("x-keep").is_some());
        assert!(headers.get(header::CONNECTION).is_none());
    }

    #[test]
    fn streaming_timeout_zero_disables() {
        let cfg = StreamingTimeoutConfig {
            first_byte_timeout: 0,
            idle_timeout: 0,
        };
        assert_eq!(cfg.first_byte_timeout, 0);
        assert_eq!(cfg.idle_timeout, 0);
    }

    #[test]
    fn is_sse_detects_event_stream() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream; charset=utf-8"),
        );
        assert!(is_sse_response(&headers));

        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        assert!(!is_sse_response(&headers));
    }
}
