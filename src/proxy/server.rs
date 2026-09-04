//! Proxy server state and router (cc-switch-aligned surface for Codex).

use crate::config::Provider;
use crate::proxy::handlers::{handle_responses, handle_responses_compact};
use crate::proxy::http_util::{json_error, json_response};
use crate::store::AgentProxyStore;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::Response,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct ProxyState {
    pub client: reqwest::Client,
    pub runtime: Arc<RwLock<RuntimeProviders>>,
    pub exchange_log_dir: PathBuf,
}

pub struct RuntimeProviders {
    pub store_path: PathBuf,
    pub active_name: String,
    pub providers: Vec<Provider>,
}

impl RuntimeProviders {
    pub fn active(&self) -> &Provider {
        self.providers
            .iter()
            .find(|p| p.name == self.active_name)
            .expect("active provider missing")
    }

    pub fn list_public(&self) -> Value {
        json!({
            "active": self.active_name,
            "providers": self.providers.iter().map(|p| json!({
                "name": p.name,
                "api_format": p.api_format.as_str(),
                "base_url": p.base_url,
                "model_mappings": p.model_mappings,
            })).collect::<Vec<_>>(),
        })
    }

    pub fn set_active(&mut self, name: &str) -> Result<&Provider, String> {
        let mut store = AgentProxyStore::load(&self.store_path).map_err(|e| e.to_string())?;
        store.set_active(name).map_err(|e| e.to_string())?;
        let (active_name, providers) = store.clone().into_providers().map_err(|e| e.to_string())?;
        store.save(&self.store_path).map_err(|e| e.to_string())?;
        self.active_name = active_name;
        self.providers = providers;
        Ok(self.active())
    }
}

impl ProxyState {
    pub async fn active_provider(&self) -> Provider {
        self.runtime.read().await.active().clone()
    }
}

pub fn build_router(state: ProxyState) -> Router {
    Router::new()
        .route("/models", get(models))
        .route("/v1/models", get(models))
        .route("/v1/responses", post(handle_responses))
        .route("/v1/v1/responses", post(handle_responses))
        .route("/codex/v1/responses", post(handle_responses))
        .route("/v1/responses/compact", post(handle_responses_compact))
        .route("/responses", post(handle_responses))
        .route("/responses/compact", post(handle_responses_compact))
        .route("/compact", post(handle_responses_compact))
        .route("/v1/v1/responses/compact", post(handle_responses_compact))
        .route("/codex/v1/responses/compact", post(handle_responses_compact))
        .route("/health", get(health))
        .route("/v1/admin/providers", get(admin_providers))
        .route("/v1/admin/active", post(admin_set_active))
        .with_state(state)
}

async fn health(State(state): State<ProxyState>) -> Json<Value> {
    let rt = state.runtime.read().await;
    let p = rt.active();
    Json(json!({
        "ok": true,
        "active_provider": p.name,
        "api_format": p.api_format.as_str(),
        "upstream": p.base_url,
    }))
}

async fn admin_providers(State(state): State<ProxyState>) -> Json<Value> {
    Json(state.runtime.read().await.list_public())
}

#[derive(Deserialize)]
struct SetActiveBody {
    name: String,
}

async fn admin_set_active(
    State(state): State<ProxyState>,
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
                }),
            )
        }
        Err(err) => json_error(StatusCode::NOT_FOUND, err),
    }
}

async fn models(State(state): State<ProxyState>) -> Json<Value> {
    let p = state.active_provider().await;
    let ids = public_model_ids(&p);
    Json(json!({
        "object": "list",
        "data": ids.into_iter().map(|id| json!({
            "id": id,
            "object": "model",
            "owned_by": "agent-proxy"
        })).collect::<Vec<_>>()
    }))
}

fn public_model_ids(provider: &Provider) -> Vec<String> {
    let mut ids = provider.model_mappings.keys().cloned().collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ApiFormat;
    use std::collections::HashMap;

    #[test]
    fn models_expose_mapping_keys_only() {
        let provider = Provider {
            name: "test".into(),
            base_url: "https://example.com/v1".into(),
            api_key: "key".into(),
            api_format: ApiFormat::OpenaiResponses,
            max_output_tokens: None,
            model_mappings: HashMap::from([
                ("zeta".into(), "upstream-z".into()),
                ("alpha".into(), "upstream-a".into()),
            ]),
        };
        assert_eq!(public_model_ids(&provider), ["alpha", "zeta"]);

        let mut empty = provider;
        empty.model_mappings.clear();
        assert!(public_model_ids(&empty).is_empty());
    }

    #[test]
    fn hot_switch_reloads_provider_config_from_store() {
        use crate::config::ApiFormat;
        use crate::store::StoredProvider;
        use std::{collections::BTreeMap, fs};

        let dir = std::env::temp_dir().join(format!(
            "agent-proxy-runtime-{}",
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
            model_mappings: BTreeMap::from([(model.into(), model.into())]),
            max_output_tokens: None,
        };
        let original = AgentProxyStore {
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

        let updated = AgentProxyStore {
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
}
