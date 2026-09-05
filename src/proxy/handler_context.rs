//! Request context for the Codex Responses proxy path (cc-switch-aligned).

use crate::config::Provider;
use crate::exchange::ExchangeLog;
use axum::http::HeaderMap;
use std::time::Instant;

/// Streaming timeout config (seconds). `0` disables that timeout.
#[derive(Debug, Clone, Copy)]
pub struct StreamingTimeoutConfig {
    pub first_byte_timeout: u64,
    pub idle_timeout: u64,
}

impl Default for StreamingTimeoutConfig {
    fn default() -> Self {
        Self {
            first_byte_timeout: 60,
            idle_timeout: 120,
        }
    }
}

impl StreamingTimeoutConfig {
    /// Defaults match cc-switch (60 / 120). Override with env vars; `0` disables.
    pub fn from_env() -> Self {
        Self {
            first_byte_timeout: env_u64("AGENT_PROXY_STREAM_FIRST_BYTE_TIMEOUT", 60),
            idle_timeout: env_u64("AGENT_PROXY_STREAM_IDLE_TIMEOUT", 120),
        }
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Per-request context spanning the Codex `/responses` lifecycle.
pub struct RequestContext {
    pub start_time: Instant,
    pub provider: Provider,
    pub client_headers: HeaderMap,
    pub tag: &'static str,
    pub streaming_timeout: StreamingTimeoutConfig,
    pub exchange: ExchangeLog,
    pub compact: bool,
    pub is_stream: bool,
}

impl RequestContext {
    pub fn new(
        provider: Provider,
        client_headers: HeaderMap,
        exchange_log_dir: &std::path::Path,
        compact: bool,
        is_stream: bool,
    ) -> Self {
        Self {
            start_time: Instant::now(),
            provider,
            client_headers,
            tag: "Codex",
            streaming_timeout: StreamingTimeoutConfig::from_env(),
            exchange: ExchangeLog::create(exchange_log_dir),
            compact,
            is_stream,
        }
    }
}
