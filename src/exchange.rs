use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static EXCHANGE_SEQ: AtomicU64 = AtomicU64::new(0);

pub struct ExchangeLog {
    dir: PathBuf,
}

impl ExchangeLog {
    pub fn create(root: &Path) -> Self {
        let started_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let seq = EXCHANGE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir_name = format!("ex_{started_at_ms}_{seq}");
        let dir = root.join(&dir_name);
        let _ = fs::create_dir_all(&dir);
        eprintln!("exchange begin dir={dir_name}");
        Self { dir }
    }

    pub fn write(&mut self, name: &str, value: &Value) {
        if let Ok(bytes) = serde_json::to_vec_pretty(value) {
            let _ = fs::write(self.dir.join(name), bytes);
        }
    }

    pub fn write_raw(&mut self, name: &str, bytes: &[u8]) {
        let _ = fs::write(self.dir.join(name), bytes);
    }

    pub fn mark_streaming(&mut self) {
        let _ = fs::write(self.dir.join("streaming"), b"1");
    }
}
