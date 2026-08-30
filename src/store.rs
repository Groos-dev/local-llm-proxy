//! Persistent JSON store for providers, active selection, and model mappings.

use crate::config::{ApiFormat, AppConfig, ConfigError, Provider};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

pub const STORE_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LlpxStore {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub bind_addr: Option<String>,
    #[serde(default)]
    pub exchange_log_dir: Option<String>,
    pub active_provider: String,
    pub providers: Vec<StoredProvider>,
}

fn default_version() -> u32 {
    STORE_VERSION
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredProvider {
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    #[serde(default)]
    pub api_format: ApiFormat,
    /// Fallback upstream model when the request model has no mapping entry.
    #[serde(default)]
    pub default_upstream_model: Option<String>,
    /// client/Codex model id → upstream model id (default identity).
    #[serde(default)]
    pub model_mappings: BTreeMap<String, String>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
}

impl LlpxStore {
    pub fn empty(active: impl Into<String>) -> Self {
        Self {
            version: STORE_VERSION,
            bind_addr: Some("127.0.0.1:8787".into()),
            exchange_log_dir: Some(".run/exchanges".into()),
            active_provider: active.into(),
            providers: Vec::new(),
        }
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path)
            .map_err(|e| ConfigError::new(format!("read {}: {e}", path.display())))?;
        serde_json::from_str(&text)
            .map_err(|e| ConfigError::new(format!("parse {}: {e}", path.display())))
    }

    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| ConfigError::new(format!("create {}: {e}", parent.display())))?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| ConfigError::new(format!("serialize store: {e}")))?;
        fs::write(path, text + "\n")
            .map_err(|e| ConfigError::new(format!("write {}: {e}", path.display())))?;
        Ok(())
    }

    pub fn from_app_config(cfg: &AppConfig) -> Self {
        Self {
            version: STORE_VERSION,
            bind_addr: cfg.bind_addr.clone(),
            exchange_log_dir: cfg.exchange_log_dir.clone(),
            active_provider: cfg.active_provider.clone(),
            providers: cfg
                .providers
                .iter()
                .map(|p| StoredProvider {
                    name: p.name.clone(),
                    base_url: p.base_url.clone(),
                    api_key: p.api_key.clone(),
                    api_format: p.api_format.clone(),
                    default_upstream_model: p.upstream_model.clone(),
                    model_mappings: identity_mapping(p.upstream_model.as_deref()),
                    max_output_tokens: p.max_output_tokens,
                })
                .collect(),
        }
    }

    pub fn into_providers(self) -> Result<(String, Vec<Provider>), ConfigError> {
        if self.providers.is_empty() {
            return Err(ConfigError::new("providers list is empty"));
        }
        let active = self.active_provider.clone();
        if !self.providers.iter().any(|p| p.name == active) {
            return Err(ConfigError::new(format!(
                "active_provider '{active}' not found in providers"
            )));
        }
        let mut names = std::collections::HashSet::new();
        let providers = self
            .providers
            .into_iter()
            .map(Provider::from)
            .map(|provider| {
                provider.validate()?;
                if !names.insert(provider.name.clone()) {
                    return Err(ConfigError::new(format!(
                        "duplicate provider name '{}'",
                        provider.name
                    )));
                }
                Ok(provider)
            })
            .collect::<Result<Vec<_>, ConfigError>>()?;
        Ok((active, providers))
    }

    pub fn upsert_provider(&mut self, provider: StoredProvider) {
        if let Some(slot) = self.providers.iter_mut().find(|p| p.name == provider.name) {
            *slot = provider;
        } else {
            if self.providers.is_empty() {
                self.active_provider = provider.name.clone();
            }
            self.providers.push(provider);
        }
    }

    pub fn set_active(&mut self, name: &str) -> Result<(), ConfigError> {
        if !self.providers.iter().any(|p| p.name == name) {
            return Err(ConfigError::new(format!("provider '{name}' not found")));
        }
        self.active_provider = name.to_string();
        Ok(())
    }

    pub fn rename_provider(
        &mut self,
        old_name: &str,
        provider: StoredProvider,
    ) -> Result<(), ConfigError> {
        if old_name != provider.name && self.get(&provider.name).is_some() {
            return Err(ConfigError::new(format!(
                "provider '{}' already exists",
                provider.name
            )));
        }
        let slot = self
            .providers
            .iter_mut()
            .find(|item| item.name == old_name)
            .ok_or_else(|| ConfigError::new(format!("provider '{old_name}' not found")))?;
        *slot = provider;
        if self.active_provider == old_name {
            self.active_provider = slot.name.clone();
        }
        Ok(())
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut StoredProvider> {
        self.providers.iter_mut().find(|p| p.name == name)
    }

    pub fn get(&self, name: &str) -> Option<&StoredProvider> {
        self.providers.iter().find(|p| p.name == name)
    }
}

fn identity_mapping(default_model: Option<&str>) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Some(m) = default_model {
        map.insert(m.to_string(), m.to_string());
    }
    map
}

impl From<StoredProvider> for Provider {
    fn from(p: StoredProvider) -> Self {
        Self {
            name: p.name,
            base_url: p.base_url.trim_end_matches('/').to_string(),
            api_key: p.api_key,
            api_format: p.api_format,
            upstream_model: p.default_upstream_model,
            max_output_tokens: p.max_output_tokens,
            model_mappings: p.model_mappings.into_iter().collect(),
        }
    }
}

impl StoredProvider {
    /// Seed identity mappings from a list of upstream model ids.
    pub fn apply_identity_mappings_from_models(&mut self, model_ids: &[String]) {
        for id in model_ids {
            self.model_mappings
                .entry(id.clone())
                .or_insert_with(|| id.clone());
        }
        if self.default_upstream_model.is_none() {
            self.default_upstream_model = model_ids.first().cloned();
        }
    }
}

/// Resolve store path: `LLPX_STORE` → `~/.llpx/store.json`.
pub fn default_store_path() -> PathBuf {
    if let Some(p) = env::var_os("LLPX_STORE") {
        return PathBuf::from(p);
    }
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".llpx")
        .join("store.json")
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Load runtime config: prefer JSON store, else migrate/load TOML.
pub fn load_runtime(
    store_path: &Path,
    toml_path: Option<&Path>,
) -> Result<(LlpxStore, PathBuf), ConfigError> {
    if store_path.exists() {
        return Ok((LlpxStore::load(store_path)?, store_path.to_path_buf()));
    }
    if let Some(toml) = toml_path.filter(|p| p.exists()) {
        let cfg = AppConfig::load(toml)?;
        let store = LlpxStore::from_app_config(&cfg);
        store.save(store_path)?;
        return Ok((store, store_path.to_path_buf()));
    }
    Err(ConfigError::new(format!(
        "no store at {} and no config.toml to migrate",
        store_path.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!("llpx-store-{n}"));
        fs::create_dir_all(&dir).unwrap();
        dir.join("store.json")
    }

    #[test]
    fn round_trip_store_json() {
        let path = tmp();
        let mut store = LlpxStore::empty("a");
        store.upsert_provider(StoredProvider {
            name: "a".into(),
            base_url: "https://example.com/v1".into(),
            api_key: "k".into(),
            api_format: ApiFormat::OpenaiResponses,
            default_upstream_model: Some("m1".into()),
            model_mappings: BTreeMap::from([("m1".into(), "m1".into())]),
            max_output_tokens: None,
        });
        store.save(&path).unwrap();
        let loaded = LlpxStore::load(&path).unwrap();
        assert_eq!(loaded.active_provider, "a");
        assert_eq!(loaded.providers[0].model_mappings["m1"], "m1");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn identity_seed_preserves_existing_mappings() {
        let mut p = StoredProvider {
            name: "a".into(),
            base_url: "https://x".into(),
            api_key: "k".into(),
            api_format: ApiFormat::OpenaiChat,
            default_upstream_model: None,
            model_mappings: BTreeMap::from([("client".into(), "upstream".into())]),
            max_output_tokens: None,
        };
        p.apply_identity_mappings_from_models(&["a".into(), "b".into()]);
        assert_eq!(p.model_mappings["client"], "upstream");
        assert_eq!(p.model_mappings["a"], "a");
        assert_eq!(p.model_mappings["b"], "b");
        assert_eq!(p.default_upstream_model.as_deref(), Some("a"));
    }

    #[test]
    fn rename_provider_rejects_existing_name() {
        let mut store = LlpxStore::empty("a");
        let provider = |name: &str| StoredProvider {
            name: name.into(),
            base_url: "https://example.com/v1".into(),
            api_key: "key".into(),
            api_format: ApiFormat::OpenaiResponses,
            default_upstream_model: Some("model".into()),
            model_mappings: BTreeMap::from([(String::from("model"), String::from("model"))]),
            max_output_tokens: None,
        };
        store.upsert_provider(provider("a"));
        store.upsert_provider(provider("b"));
        assert!(store.rename_provider("a", provider("b")).is_err());
    }
}
