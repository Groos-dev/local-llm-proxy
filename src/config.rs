use crate::channel::ChannelKind;
use crate::model::ModelRoute;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt::{self, Display, Formatter},
    fs,
    path::Path,
};

/// Public model names exposed externally. Only these three names are accepted by /v1/models and request routing.
pub const PUBLIC_MODELS: [&str; 3] = ["gpt-5.6-luna", "gpt-5.6-terra", "gpt-5.6-sol"];

#[derive(Clone, Debug, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub bind_addr: Option<String>,
    #[serde(default)]
    pub exchange_log_dir: Option<String>,
    pub default_provider: String,
    pub providers: Vec<ProviderConfig>,
}

/// Static provider definition: connection info, supported upstream models and their adapters, and whether compact is supported.
/// Dynamic model-to-provider routing lives elsewhere.
#[derive(Clone, Debug, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub supports_compact: bool,
    #[serde(default)]
    pub models: Vec<ProviderModelConfig>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProviderModelConfig {
    pub upstream_model: String,
    pub response_adapter: ChannelKind,
}

#[derive(Clone, Debug)]
pub struct Provider {
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub supports_compact: bool,
    pub models: HashMap<String, ChannelKind>,
    pub model_order: Vec<String>,
}

impl Provider {
    pub fn adapter_for(&self, upstream_model: &str) -> Option<ChannelKind> {
        self.models.get(upstream_model).copied()
    }

    pub fn supports_model(&self, upstream_model: &str) -> bool {
        self.models.contains_key(upstream_model)
    }
}

/// A single dynamic route: public model -> provider + upstream model.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelRouteConfig {
    pub provider: String,
    pub upstream_model: String,
}

/// Runtime-adjustable route table persisted to JSON, fully decoupled from the static provider catalog.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RouteTable {
    pub routes: HashMap<String, ModelRouteConfig>,
}

#[derive(Debug)]
pub struct ConfigError(String);

impl ConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl Display for ConfigError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for ConfigError {}

impl AppConfig {
    pub fn from_toml(input: &str) -> Result<Self, ConfigError> {
        toml::from_str(input).map_err(|err| ConfigError::new(format!("invalid TOML: {err}")))
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let input = fs::read_to_string(path)
            .map_err(|err| ConfigError::new(format!("read {}: {err}", path.display())))?;
        Self::from_toml(&input)
    }
}

impl RouteTable {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let input = fs::read_to_string(path)
            .map_err(|err| ConfigError::new(format!("read {}: {err}", path.display())))?;
        serde_json::from_str(&input)
            .map_err(|err| ConfigError::new(format!("invalid route table: {err}")))
    }

    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| ConfigError::new(format!("create {}: {err}", parent.display())))?;
        }
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|err| ConfigError::new(format!("serialize route table: {err}")))?;
        fs::write(path, bytes)
            .map_err(|err| ConfigError::new(format!("write {}: {err}", path.display())))
    }

    pub fn get(&self, public_model: &str) -> Option<&ModelRouteConfig> {
        self.routes.get(public_model)
    }

    pub fn set(&mut self, public_model: String, route: ModelRouteConfig) {
        self.routes.insert(public_model, route);
    }

    pub fn remove(&mut self, public_model: &str) -> bool {
        self.routes.remove(public_model).is_some()
    }

    /// Startup default mapping: fill in same-name passthrough routes for the fixed public models.
    /// Only when a public model has no explicit route and the default provider declares a same-name upstream model,
    /// create a public_model -> default_provider/public_model self route. Existing routes are untouched.
    pub fn ensure_default_self_routes(&mut self, catalog: &ProviderCatalog) -> usize {
        let default_provider = catalog.default_provider();
        let mut added = 0;
        for public_model in PUBLIC_MODELS {
            if self.routes.contains_key(public_model) {
                continue;
            }
            if !catalog.supports_model(default_provider, public_model) {
                continue;
            }
            self.routes.insert(
                public_model.to_string(),
                ModelRouteConfig {
                    provider: default_provider.to_string(),
                    upstream_model: public_model.to_string(),
                },
            );
            added += 1;
        }
        added
    }
}

#[derive(Clone, Debug)]
pub struct ProviderCatalog {
    providers: HashMap<String, Provider>,
    provider_order: Vec<String>,
    default_provider: String,
}

impl ProviderCatalog {
    pub fn new(config: AppConfig) -> Result<Self, ConfigError> {
        if config.providers.is_empty() {
            return Err(ConfigError::new(
                "configuration must define at least one provider",
            ));
        }

        let mut providers = HashMap::new();
        let mut provider_order = Vec::new();
        let mut seen = HashSet::new();
        for provider in config.providers {
            if provider.name.trim().is_empty() {
                return Err(ConfigError::new("provider name must not be empty"));
            }
            if !seen.insert(provider.name.clone()) {
                return Err(ConfigError::new(format!(
                    "duplicate provider name '{}'",
                    provider.name
                )));
            }
            if provider.api_key.trim().is_empty() {
                return Err(ConfigError::new(format!(
                    "provider '{}' api_key must not be empty",
                    provider.name
                )));
            }
            let url = reqwest::Url::parse(&provider.base_url).map_err(|err| {
                ConfigError::new(format!(
                    "provider '{}' base_url is invalid: {err}",
                    provider.name
                ))
            })?;
            if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                return Err(ConfigError::new(format!(
                    "provider '{}' base_url must be an http or https URL",
                    provider.name
                )));
            }
            if provider.models.is_empty() {
                return Err(ConfigError::new(format!(
                    "provider '{}' must declare at least one model",
                    provider.name
                )));
            }

            let mut model_map = HashMap::new();
            let mut model_order = Vec::new();
            for model in provider.models {
                let name = model.upstream_model.trim();
                if name.is_empty() {
                    return Err(ConfigError::new(format!(
                        "provider '{}' upstream_model must not be empty",
                        provider.name
                    )));
                }
                if model_map
                    .insert(name.to_string(), model.response_adapter)
                    .is_some()
                {
                    return Err(ConfigError::new(format!(
                        "provider '{}' declares duplicate upstream model '{}'",
                        provider.name, name
                    )));
                }
                model_order.push(name.to_string());
            }

            provider_order.push(provider.name.clone());
            providers.insert(
                provider.name.clone(),
                Provider {
                    name: provider.name,
                    base_url: provider.base_url.trim_end_matches('/').to_string(),
                    api_key: provider.api_key,
                    supports_compact: provider.supports_compact,
                    models: model_map,
                    model_order,
                },
            );
        }

        if config.default_provider.trim().is_empty() {
            return Err(ConfigError::new("default_provider must not be empty"));
        }
        if !providers.contains_key(config.default_provider.as_str()) {
            return Err(ConfigError::new(format!(
                "default_provider '{}' does not match any provider name",
                config.default_provider
            )));
        }

        Ok(Self {
            providers,
            provider_order,
            default_provider: config.default_provider,
        })
    }

    pub fn provider_names(&self) -> &[String] {
        &self.provider_order
    }

    pub fn has_provider(&self, name: &str) -> bool {
        self.providers.contains_key(name)
    }

    pub fn default_provider(&self) -> &str {
        &self.default_provider
    }

    pub fn get(&self, name: &str) -> Option<&Provider> {
        self.providers.get(name)
    }

    pub fn supports_model(&self, provider: &str, upstream_model: &str) -> bool {
        self.get(provider)
            .map(|p| p.supports_model(upstream_model))
            .unwrap_or(false)
    }
}

/// Resolve a dynamic route against the static provider catalog into a forwardable ModelRoute.
pub fn resolve_route(
    catalog: &ProviderCatalog,
    table: &RouteTable,
    public_model: &str,
) -> Option<ModelRoute> {
    let config = table.get(public_model)?;
    let provider = catalog.get(&config.provider)?;
    let channel = provider.adapter_for(&config.upstream_model)?;
    Some(ModelRoute {
        origin_model: config.upstream_model.clone(),
        public_model: public_model.to_string(),
        provider_name: provider.name.clone(),
        upstream_base_url: provider.base_url.clone(),
        api_key: provider.api_key.clone(),
        channel,
        supports_compact: provider.supports_compact,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
bind_addr = "127.0.0.1:8787"
exchange_log_dir = ".run/exchanges"
default_provider = "mmkg"

[[providers]]
name = "ada"
base_url = "http://ada.example/v1"
api_key = "ada-secret"
supports_compact = false

[[providers.models]]
upstream_model = "DeepSeek-V4-Flash"
response_adapter = "deepseek"

[[providers.models]]
upstream_model = "DeepSeek-V4-Pro"
response_adapter = "deepseek"

[[providers]]
name = "mmkg"
base_url = "https://mmkg.example/v1"
api_key = "mmkg-secret"
supports_compact = true

[[providers.models]]
upstream_model = "gpt-5.6-luna"
response_adapter = "standard"

[[providers.models]]
upstream_model = "gpt-5.6-terra"
response_adapter = "standard"

[[providers.models]]
upstream_model = "gpt-5.6-sol"
response_adapter = "standard"

[[providers.models]]
upstream_model = "gpt-5.6"
response_adapter = "standard"
"#;

    #[test]
    fn loads_providers_with_per_model_adapters() {
        let config = AppConfig::from_toml(CONFIG).unwrap();
        let catalog = ProviderCatalog::new(config).unwrap();

        assert_eq!(
            catalog.provider_names(),
            &["ada".to_string(), "mmkg".to_string()]
        );
        let ada = catalog.get("ada").unwrap();
        assert_eq!(
            ada.adapter_for("DeepSeek-V4-Flash"),
            Some(ChannelKind::DeepSeek)
        );
        assert!(ada.supports_model("DeepSeek-V4-Pro"));
        assert!(!ada.supports_compact);

        let mmkg = catalog.get("mmkg").unwrap();
        assert_eq!(
            mmkg.adapter_for("gpt-5.6-luna"),
            Some(ChannelKind::Standard)
        );
        assert_eq!(mmkg.adapter_for("gpt-5.6"), Some(ChannelKind::Standard));
        assert!(mmkg.supports_compact);
    }

    #[test]
    fn route_table_round_trips_through_json() {
        let mut table = RouteTable::default();
        table.set(
            "gpt-5.6-luna".to_string(),
            ModelRouteConfig {
                provider: "ada".to_string(),
                upstream_model: "DeepSeek-V4-Flash".to_string(),
            },
        );

        let json = serde_json::to_string(&table).unwrap();
        let decoded: RouteTable = serde_json::from_str(&json).unwrap();
        assert_eq!(
            decoded.get("gpt-5.6-luna").unwrap().upstream_model,
            "DeepSeek-V4-Flash"
        );
    }

    #[test]
    fn ensure_default_self_routes_fills_only_missing_supported_models() {
        let catalog = ProviderCatalog::new(AppConfig::from_toml(CONFIG).unwrap()).unwrap();
        let mut table = RouteTable::default();
        table.set(
            "gpt-5.6-luna".to_string(),
            ModelRouteConfig {
                provider: "ada".to_string(),
                upstream_model: "DeepSeek-V4-Flash".to_string(),
            },
        );

        let added = table.ensure_default_self_routes(&catalog);

        // luna already has an explicit route, so it is not overwritten; terra/sol are same-name models of default provider mmkg and get filled in.
        assert_eq!(added, 2);
        assert_eq!(table.get("gpt-5.6-luna").unwrap().provider, "ada");
        assert_eq!(
            table.get("gpt-5.6-terra").unwrap().upstream_model,
            "gpt-5.6-terra"
        );
        assert_eq!(table.get("gpt-5.6-terra").unwrap().provider, "mmkg");
        assert_eq!(table.get("gpt-5.6-sol").unwrap().provider, "mmkg");
    }

    #[test]
    fn resolves_route_by_combining_dynamic_table_and_static_catalog() {
        let catalog = ProviderCatalog::new(AppConfig::from_toml(CONFIG).unwrap()).unwrap();
        let mut table = RouteTable::default();
        table.set(
            "gpt-5.6-luna".to_string(),
            ModelRouteConfig {
                provider: "ada".to_string(),
                upstream_model: "DeepSeek-V4-Flash".to_string(),
            },
        );

        let route = resolve_route(&catalog, &table, "gpt-5.6-luna").unwrap();
        assert_eq!(route.public_model, "gpt-5.6-luna");
        assert_eq!(route.origin_model, "DeepSeek-V4-Flash");
        assert_eq!(route.provider_name, "ada");
        assert_eq!(route.upstream_base_url, "http://ada.example/v1");
        assert_eq!(route.channel, ChannelKind::DeepSeek);
        assert!(!route.supports_compact);
    }

    #[test]
    fn resolve_route_is_none_for_missing_route_or_provider_or_model() {
        let catalog = ProviderCatalog::new(AppConfig::from_toml(CONFIG).unwrap()).unwrap();
        let table = RouteTable::default();
        assert!(resolve_route(&catalog, &table, "gpt-5.6-luna").is_none());

        let mut table = RouteTable::default();
        table.set(
            "gpt-5.6-luna".to_string(),
            ModelRouteConfig {
                provider: "missing".to_string(),
                upstream_model: "x".to_string(),
            },
        );
        assert!(resolve_route(&catalog, &table, "gpt-5.6-luna").is_none());

        let mut table = RouteTable::default();
        table.set(
            "gpt-5.6-luna".to_string(),
            ModelRouteConfig {
                provider: "ada".to_string(),
                upstream_model: "not-declared".to_string(),
            },
        );
        assert!(resolve_route(&catalog, &table, "gpt-5.6-luna").is_none());
    }

    #[test]
    fn rejects_duplicate_provider_name() {
        let input = CONFIG.replace("name = \"mmkg\"", "name = \"ada\"");
        let error = ProviderCatalog::new(AppConfig::from_toml(&input).unwrap()).unwrap_err();
        assert!(error.to_string().contains("duplicate provider"));
    }

    #[test]
    fn rejects_duplicate_upstream_model_within_provider() {
        let input = CONFIG.replace(
            "upstream_model = \"DeepSeek-V4-Pro\"",
            "upstream_model = \"DeepSeek-V4-Flash\"",
        );
        let error = ProviderCatalog::new(AppConfig::from_toml(&input).unwrap()).unwrap_err();
        assert!(error.to_string().contains("duplicate upstream model"));
    }

    #[test]
    fn rejects_provider_without_models() {
        let input = r#"
default_provider = "empty"
[[providers]]
name = "empty"
base_url = "http://empty.example/v1"
api_key = "empty-secret"
supports_compact = false
"#;
        let error = ProviderCatalog::new(AppConfig::from_toml(input).unwrap()).unwrap_err();
        assert!(error.to_string().contains("at least one model"));
    }

    #[test]
    fn rejects_invalid_provider_url() {
        let input = CONFIG.replace("https://mmkg.example/v1", "not-a-url");
        let error = ProviderCatalog::new(AppConfig::from_toml(&input).unwrap()).unwrap_err();
        assert!(error.to_string().contains("base_url"));
    }
}
