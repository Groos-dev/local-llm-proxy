//! Local Codex Responses proxy with cc-switch-compatible protocol bridges.
//!
//! Call chain (Codex):
//! `server` → `handlers::handle_responses` → `RequestContext` →
//! `forwarder::forward_with_retry` → `process_response` / chat|anthropic bridges.

use agent_proxy::{
    default_store_path, load_runtime,
    proxy::providers::codex_chat_history::CodexChatHistoryStore,
    proxy::server::{ProxyState, RuntimeProviders, build_router},
};
use std::{env, fs, net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::sync::RwLock;

const DEFAULT_CONFIG_PATH: &str = "config.toml";

#[tokio::main]
async fn main() {
    let _ = env_logger::try_init();

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let toml_path = env::var_os("CONFIG_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join(DEFAULT_CONFIG_PATH));
    let store_path = env::var_os("AGENT_PROXY_STORE")
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
    let state = ProxyState {
        client: reqwest::Client::new(),
        runtime: Arc::new(RwLock::new(runtime)),
        exchange_log_dir,
        codex_chat_history: Arc::new(CodexChatHistoryStore::default()),
    };

    eprintln!(
        "AgentProxy listening on {bind_addr} provider={} api_format={} → {}",
        active_snapshot.0, active_snapshot.1, active_snapshot.2
    );

    let app = build_router(state);
    axum::serve(listener, app).await.unwrap();
}
