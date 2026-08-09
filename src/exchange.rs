use axum::http::HeaderMap;
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static EXCHANGE_SEQ: AtomicU64 = AtomicU64::new(0);

pub struct ExchangeLog {
    dir: PathBuf,
    dir_name: String,
    request_id: String,
    public_model: String,
    started_at_ms: u128,
}

impl ExchangeLog {
    pub fn begin(
        root: &Path,
        headers: &HeaderMap,
        codex_request: &Value,
        upstream_request: &Value,
        public_model: &str,
    ) -> Self {
        let request_id = client_request_id(headers);
        let started_at_ms = unix_ms();
        let seq = EXCHANGE_SEQ.fetch_add(1, Ordering::Relaxed);
        // Codex reuses session id as x-request-id; uniquify so turns do not overwrite.
        let dir_name = format!("{request_id}_{started_at_ms}_{seq}");
        let dir = root.join(&dir_name);
        let _ = fs::create_dir_all(&dir);
        write_json(&dir.join("codex_request.json"), codex_request);
        write_json(&dir.join("upstream_request.json"), upstream_request);
        write_json(
            &dir.join("meta.json"),
            &json!({
                "request_id": request_id,
                "exchange_dir": dir_name,
                "public_model": public_model,
                "started_at_ms": started_at_ms,
            }),
        );
        eprintln!("exchange begin dir={dir_name} request_id={request_id} model={public_model}");
        Self {
            dir,
            dir_name,
            request_id,
            public_model: public_model.to_string(),
            started_at_ms,
        }
    }

    pub fn finish_text(
        &self,
        status: u16,
        content_type: &str,
        ada_response: &[u8],
        response_headers: &HeaderMap,
    ) {
        let path = if content_type.starts_with("text/event-stream") {
            self.dir.join("ada_response.sse")
        } else if content_type.contains("json") {
            self.dir.join("ada_response.json")
        } else {
            self.dir.join("ada_response.bin")
        };
        let _ = fs::write(&path, ada_response);
        let ada_request_id = header_str(response_headers, "request-id")
            .or_else(|| header_str(response_headers, "x-request-id"));
        let has_dsml = String::from_utf8_lossy(ada_response).contains("DSML");
        write_json(
            &self.dir.join("meta.json"),
            &json!({
                "request_id": self.request_id,
                "exchange_dir": self.dir_name,
                "public_model": self.public_model,
                "ada_request_id": ada_request_id,
                "status": status,
                "content_type": content_type,
                "ada_response_bytes": ada_response.len(),
                "started_at_ms": self.started_at_ms,
                "finished_at_ms": unix_ms(),
                "has_dsml": has_dsml,
            }),
        );
        eprintln!(
            "exchange done dir={} request_id={} status={} bytes={} dsml={}",
            self.dir_name,
            self.request_id,
            status,
            ada_response.len(),
            has_dsml
        );
    }
}

fn client_request_id(headers: &HeaderMap) -> String {
    for name in ["x-request-id", "request-id", "x-client-request-id"] {
        if let Some(value) = header_str(headers, name) {
            let sanitized = sanitize_request_id(&value);
            if !sanitized.is_empty() {
                return sanitized;
            }
        }
    }
    format!("local-{}", unix_ms())
}

fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn sanitize_request_id(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '_'
            }
        })
        .take(160)
        .collect()
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn write_json(path: &Path, value: &Value) {
    if let Ok(bytes) = serde_json::to_vec_pretty(value) {
        let _ = fs::write(path, bytes);
    }
}
