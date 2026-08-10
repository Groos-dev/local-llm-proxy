use crate::channel::ChannelKind;
use crate::model::ModelRoute;
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt::{self, Display, Formatter},
    fs,
    path::Path,
};

#[derive(Clone, Debug, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub bind_addr: Option<String>,
    #[serde(default)]
    pub exchange_log_dir: Option<String>,
    pub default_provider: String,
    pub providers: Vec<ProviderConfig>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub supports_compact: bool,
    #[serde(default)]
    pub models: Vec<ModelConfig>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ModelConfig {
    pub public_model: String,
    pub upstream_model: String,
    pub response_adapter: ChannelKind,
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

    fn validate(&self) -> Result<(), ConfigError> {
        if self.providers.is_empty() {
            return Err(ConfigError::new(
                "configuration must define at least one provider",
            ));
        }

        let mut provider_names = HashSet::new();
        for provider in &self.providers {
            if provider.name.trim().is_empty() {
                return Err(ConfigError::new("provider name must not be empty"));
            }
            if !provider_names.insert(provider.name.as_str()) {
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
                    "provider '{}' must define at least one model",
                    provider.name
                )));
            }
            let mut public_models = HashSet::new();
            for model in &provider.models {
                if model.public_model.trim().is_empty() || model.upstream_model.trim().is_empty() {
                    return Err(ConfigError::new(format!(
                        "provider '{}' model names must not be empty",
                        provider.name
                    )));
                }
                if !public_models.insert(model.public_model.as_str()) {
                    return Err(ConfigError::new(format!(
                        "duplicate public model '{}' in provider '{}'",
                        model.public_model, provider.name
                    )));
                }
            }
        }

        if self.default_provider.trim().is_empty() {
            return Err(ConfigError::new("default_provider must not be empty"));
        }
        if !provider_names.contains(self.default_provider.as_str()) {
            return Err(ConfigError::new(format!(
                "default_provider '{}' does not match any provider name",
                self.default_provider
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ProviderRegistry {
    routes: HashMap<String, HashMap<String, ModelRoute>>,
    model_order: HashMap<String, Vec<String>>,
    provider_names: Vec<String>,
    default_provider: String,
}

impl ProviderRegistry {
    pub fn new(config: AppConfig) -> Result<Self, ConfigError> {
        config.validate()?;
        let default_provider = config.default_provider;
        let mut routes = HashMap::new();
        let mut model_order = HashMap::new();
        let mut provider_names = Vec::new();

        for provider in config.providers {
            let base_url = provider.base_url.trim_end_matches('/').to_string();
            let mut provider_routes = HashMap::new();
            let mut provider_models = Vec::new();
            for model in provider.models {
                let public_model = model.public_model.clone();
                let route = ModelRoute {
                    origin_model: model.upstream_model,
                    public_model: public_model.clone(),
                    provider_name: provider.name.clone(),
                    upstream_base_url: base_url.clone(),
                    api_key: provider.api_key.clone(),
                    channel: model.response_adapter,
                    supports_compact: provider.supports_compact,
                };
                provider_routes.insert(public_model.clone(), route);
                provider_models.push(public_model);
            }
            provider_names.push(provider.name.clone());
            routes.insert(provider.name.clone(), provider_routes);
            model_order.insert(provider.name, provider_models);
        }

        Ok(Self {
            routes,
            model_order,
            provider_names,
            default_provider,
        })
    }

    pub fn default_provider(&self) -> &str {
        &self.default_provider
    }

    pub fn provider_names(&self) -> &[String] {
        &self.provider_names
    }

    pub fn has_provider(&self, provider: &str) -> bool {
        self.routes.contains_key(provider)
    }

    pub fn route_for_public_model(&self, provider: &str, public_model: &str) -> Option<ModelRoute> {
        self.routes
            .get(provider)
            .and_then(|routes| routes.get(public_model))
            .cloned()
    }

    pub fn public_models_list(&self, provider: &str) -> Value {
        let data = self
            .model_order
            .get(provider)
            .into_iter()
            .flatten()
            .map(|public_model| {
                serde_json::json!({
                    "id": public_model,
                    "object": "model",
                    "created": 1_700_000_000i64,
                    "owned_by": "local-llm-proxy",
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "object": "list",
            "data": data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
bind_addr = "127.0.0.1:8787"
exchange_log_dir = ".run/exchanges"
default_provider = "ada"

[[providers]]
name = "ada"
base_url = "http://ada.example/v1"
api_key = "ada-secret"
supports_compact = false

[[providers.models]]
public_model = "gpt-luna"
upstream_model = "DeepSeek-V4-Flash"
response_adapter = "deepseek"

[[providers.models]]
public_model = "gpt-terra"
upstream_model = "DeepSeek-V4-Pro"
response_adapter = "deepseek"

[[providers.models]]
public_model = "gpt-sol"
upstream_model = "glm-5.2"
response_adapter = "standard"

[[providers]]
name = "mmkg"
base_url = "https://mmkg.example/v1"
api_key = "mmkg-secret"
supports_compact = true

[[providers.models]]
public_model = "gpt-luna"
upstream_model = "gpt-luna"
response_adapter = "standard"

[[providers.models]]
public_model = "gpt-sol"
upstream_model = "gpt-sol"
response_adapter = "standard"

[[providers.models]]
public_model = "gpt-5.6"
upstream_model = "gpt-5.6"
response_adapter = "standard"
"#;

    #[test]
    fn loads_multiple_providers_and_resolves_route_details() {
        let config = AppConfig::from_toml(CONFIG).unwrap();
        let registry = ProviderRegistry::new(config).unwrap();

        assert_eq!(registry.default_provider(), "ada");
        assert_eq!(
            registry.provider_names(),
            &["ada".to_string(), "mmkg".to_string()]
        );

        let route = registry.route_for_public_model("mmkg", "gpt-5.6").unwrap();
        assert_eq!(route.provider_name, "mmkg");
        assert_eq!(route.origin_model, "gpt-5.6");
        assert_eq!(route.upstream_base_url, "https://mmkg.example/v1");
        assert_eq!(route.api_key, "mmkg-secret");
        assert!(route.supports_compact);
        assert_eq!(route.channel, ChannelKind::Standard);

        let deepseek_route = registry.route_for_public_model("ada", "gpt-luna").unwrap();
        assert_eq!(deepseek_route.channel, ChannelKind::DeepSeek);
        assert!(!deepseek_route.supports_compact);

        let same_provider_standard = registry.route_for_public_model("ada", "gpt-sol").unwrap();
        assert_eq!(same_provider_standard.provider_name, "ada");
        assert_eq!(same_provider_standard.channel, ChannelKind::Standard);
        assert!(!same_provider_standard.supports_compact);
    }

    #[test]
    fn resolves_overlapping_model_by_active_provider() {
        let registry = ProviderRegistry::new(AppConfig::from_toml(CONFIG).unwrap()).unwrap();

        let ada = registry.route_for_public_model("ada", "gpt-luna").unwrap();
        assert_eq!(ada.origin_model, "DeepSeek-V4-Flash");
        assert_eq!(ada.channel, ChannelKind::DeepSeek);

        let mmkg = registry.route_for_public_model("mmkg", "gpt-luna").unwrap();
        assert_eq!(mmkg.origin_model, "gpt-luna");
        assert_eq!(mmkg.channel, ChannelKind::Standard);

        assert!(registry.route_for_public_model("ada", "gpt-5.6").is_none());
    }

    #[test]
    fn public_models_list_is_scoped_to_provider() {
        let registry = ProviderRegistry::new(AppConfig::from_toml(CONFIG).unwrap()).unwrap();

        let ada_list = registry.public_models_list("ada");
        let ada_ids = ada_list["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ada_ids, vec!["gpt-luna", "gpt-terra", "gpt-sol"]);

        let mmkg_list = registry.public_models_list("mmkg");
        let mmkg_ids = mmkg_list["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(mmkg_ids, vec!["gpt-luna", "gpt-sol", "gpt-5.6"]);
    }

    #[test]
    fn rejects_unknown_default_provider() {
        let config = AppConfig::from_toml(&CONFIG.replace(
            "default_provider = \"ada\"",
            "default_provider = \"missing\"",
        ))
        .unwrap();
        let error = ProviderRegistry::new(config).unwrap_err();
        assert!(error.to_string().contains("default_provider"));
    }

    #[test]
    fn rejects_duplicate_public_model_within_same_provider() {
        let config = AppConfig::from_toml(&CONFIG.replace(
            "public_model = \"gpt-terra\"",
            "public_model = \"gpt-luna\"",
        ))
        .unwrap();
        let error = ProviderRegistry::new(config).unwrap_err();
        assert!(error.to_string().contains("duplicate public model"));
    }

    #[test]
    fn rejects_provider_without_models() {
        let config = AppConfig::from_toml(
            r#"
            default_provider = "empty"
            [[providers]]
            name = "empty"
            base_url = "https://example.com/v1"
            api_key = "empty-secret"
            supports_compact = false
            "#,
        )
        .unwrap();

        let error = ProviderRegistry::new(config).unwrap_err();
        assert!(error.to_string().contains("must define at least one model"));
    }

    #[test]
    fn rejects_invalid_provider_url() {
        let config =
            AppConfig::from_toml(&CONFIG.replace("https://mmkg.example/v1", "not-a-url")).unwrap();
        let error = ProviderRegistry::new(config).unwrap_err();
        assert!(error.to_string().contains("base_url"));
    }

    #[test]
    fn rejects_unknown_response_adapter_during_parse() {
        let error = AppConfig::from_toml(&CONFIG.replace(
            "response_adapter = \"standard\"",
            "response_adapter = \"unknown\"",
        ))
        .unwrap_err();
        assert!(error.to_string().contains("response_adapter"));
    }
}
