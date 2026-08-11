use axum::http::HeaderMap;
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static EXCHANGE_SEQ: AtomicU64 = AtomicU64::new(0);

const SSE_PARTIAL_NAME: &str = "ada_response.sse.partial";
const SSE_FINAL_NAME: &str = "ada_response.sse";

pub struct ExchangeLog {
    dir: PathBuf,
    dir_name: String,
    request_id: String,
    public_model: String,
    started_at_ms: u128,
    finished: AtomicBool,
    /// Captured when SSE streaming starts so Drop can still record request-id.
    sse_headers: std::sync::Mutex<Option<HeaderMap>>,
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
            finished: AtomicBool::new(false),
            sse_headers: std::sync::Mutex::new(None),
        }
    }

    /// Remember upstream response headers for incomplete Drop finalization.
    pub fn note_sse_headers(&self, headers: &HeaderMap) {
        if let Ok(mut slot) = self.sse_headers.lock() {
            *slot = Some(headers.clone());
        }
    }

    /// Append raw upstream SSE bytes as they arrive (survives client disconnect).
    pub fn append_sse_chunk(&self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        let path = self.dir.join(SSE_PARTIAL_NAME);
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = file.write_all(chunk);
        }
    }

    pub fn finish_text(
        &self,
        status: u16,
        content_type: &str,
        ada_response: &[u8],
        response_headers: &HeaderMap,
    ) {
        if self
            .finished
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        self.write_finished(status, content_type, ada_response, response_headers, false);
    }

    fn write_finished(
        &self,
        status: u16,
        content_type: &str,
        ada_response: &[u8],
        response_headers: &HeaderMap,
        incomplete: bool,
    ) {
        let path = if content_type.starts_with("text/event-stream") {
            self.dir.join(SSE_FINAL_NAME)
        } else if content_type.contains("json") {
            self.dir.join("ada_response.json")
        } else {
            self.dir.join("ada_response.bin")
        };
        let _ = fs::write(&path, ada_response);
        let _ = fs::remove_file(self.dir.join(SSE_PARTIAL_NAME));
        let ada_request_id = header_str(response_headers, "request-id")
            .or_else(|| header_str(response_headers, "x-request-id"));
        let has_dsml = String::from_utf8_lossy(ada_response).contains("DSML");
        let mut meta = json!({
            "request_id": self.request_id,
            "exchange_dir": self.dir_name,
            "public_model": self.public_model,
            "ada_request_id": ada_request_id,
            "content_type": content_type,
            "ada_response_bytes": ada_response.len(),
            "started_at_ms": self.started_at_ms,
            "finished_at_ms": unix_ms(),
            "has_dsml": has_dsml,
        });
        if let Some(obj) = meta.as_object_mut() {
            if incomplete {
                obj.insert("incomplete".to_string(), Value::Bool(true));
            } else {
                obj.insert("status".to_string(), json!(status));
            }
        }
        write_json(&self.dir.join("meta.json"), &meta);
        if incomplete {
            eprintln!(
                "exchange incomplete dir={} request_id={} bytes={}",
                self.dir_name,
                self.request_id,
                ada_response.len()
            );
        } else {
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

    fn finalize_incomplete_from_partial(&self) {
        if self
            .finished
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let partial = self.dir.join(SSE_PARTIAL_NAME);
        let bytes = fs::read(&partial).unwrap_or_default();
        if bytes.is_empty() {
            // No SSE bytes yet; leave begin-only meta as-is but mark incomplete.
            let mut meta = json!({
                "request_id": self.request_id,
                "exchange_dir": self.dir_name,
                "public_model": self.public_model,
                "started_at_ms": self.started_at_ms,
                "finished_at_ms": unix_ms(),
                "incomplete": true,
                "ada_response_bytes": 0,
            });
            if let Ok(guard) = self.sse_headers.lock()
                && let Some(headers) = guard.as_ref()
                && let Some(obj) = meta.as_object_mut()
            {
                obj.insert(
                    "content_type".to_string(),
                    Value::String("text/event-stream".to_string()),
                );
                if let Some(id) = header_str(headers, "request-id")
                    .or_else(|| header_str(headers, "x-request-id"))
                {
                    obj.insert("ada_request_id".to_string(), Value::String(id));
                }
            }
            write_json(&self.dir.join("meta.json"), &meta);
            eprintln!(
                "exchange incomplete dir={} request_id={} bytes=0",
                self.dir_name, self.request_id
            );
            return;
        }
        let headers = self
            .sse_headers
            .lock()
            .ok()
            .and_then(|mut g| g.take())
            .unwrap_or_default();
        self.write_finished(200, "text/event-stream", &bytes, &headers, true);
    }
}

impl Drop for ExchangeLog {
    fn drop(&mut self) {
        if !self.finished.load(Ordering::SeqCst) {
            self.finalize_incomplete_from_partial();
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn temp_root() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "llp-exchange-{}-{}",
            unix_ms(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::create_dir_all(&root);
        root
    }

    fn begin_log(root: &Path) -> ExchangeLog {
        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", HeaderValue::from_static("req-test"));
        ExchangeLog::begin(
            root,
            &headers,
            &json!({"model": "gpt"}),
            &json!({"model": "upstream"}),
            "gpt",
        )
    }

    #[test]
    fn finish_text_writes_sse_and_clears_partial() {
        let root = temp_root();
        let log = begin_log(&root);
        let dir = log.dir.clone();
        log.append_sse_chunk(b"event: x\ndata: 1\n\n");
        assert!(dir.join(SSE_PARTIAL_NAME).exists());
        let mut headers = HeaderMap::new();
        headers.insert("request-id", HeaderValue::from_static("ada-1"));
        log.finish_text(
            200,
            "text/event-stream",
            b"event: done\ndata: {}\n\n",
            &headers,
        );
        assert!(dir.join(SSE_FINAL_NAME).exists());
        assert!(!dir.join(SSE_PARTIAL_NAME).exists());
        let meta: Value =
            serde_json::from_slice(&fs::read(dir.join("meta.json")).unwrap()).unwrap();
        assert_eq!(meta["status"], 200);
        assert!(meta.get("incomplete").is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn drop_promotes_partial_when_unfinished() {
        let root = temp_root();
        let dir = {
            let log = begin_log(&root);
            let dir = log.dir.clone();
            let mut headers = HeaderMap::new();
            headers.insert("request-id", HeaderValue::from_static("ada-drop"));
            log.note_sse_headers(&headers);
            log.append_sse_chunk(
                b"event: response.output_item.done\ndata: {\"type\":\"message\"}\n\n",
            );
            dir
            // Drop here
        };
        assert!(dir.join(SSE_FINAL_NAME).exists());
        assert!(!dir.join(SSE_PARTIAL_NAME).exists());
        let body = fs::read_to_string(dir.join(SSE_FINAL_NAME)).unwrap();
        assert!(body.contains("response.output_item.done"));
        let meta: Value =
            serde_json::from_slice(&fs::read(dir.join("meta.json")).unwrap()).unwrap();
        assert_eq!(meta["incomplete"], true);
        assert!(meta.get("status").is_none());
        assert_eq!(meta["ada_request_id"], "ada-drop");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn drop_after_finish_is_noop() {
        let root = temp_root();
        let dir = {
            let log = begin_log(&root);
            let dir = log.dir.clone();
            log.finish_text(200, "application/json", b"{\"ok\":true}", &HeaderMap::new());
            dir
        };
        let meta: Value =
            serde_json::from_slice(&fs::read(dir.join("meta.json")).unwrap()).unwrap();
        assert!(meta.get("incomplete").is_none());
        assert_eq!(meta["status"], 200);
        assert!(dir.join("ada_response.json").exists());
        let _ = fs::remove_dir_all(&root);
    }
}
