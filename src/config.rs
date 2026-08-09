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
    pub providers: Vec<ProviderConfig>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub base_url: String,
    pub api_key_env: String,
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
        let mut public_models = HashSet::new();
        for provider in &self.providers {
            if provider.name.trim().is_empty() {
                return Err(ConfigError::new("provider name must not be empty"));
            }
            if !provider_names.insert(&provider.name) {
                return Err(ConfigError::new(format!(
                    "duplicate provider name '{}'",
                    provider.name
                )));
            }
            if provider.api_key_env.trim().is_empty() {
                return Err(ConfigError::new(format!(
                    "provider '{}' api_key_env must not be empty",
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
            for model in &provider.models {
                if model.public_model.trim().is_empty() || model.upstream_model.trim().is_empty() {
                    return Err(ConfigError::new(format!(
                        "provider '{}' model names must not be empty",
                        provider.name
                    )));
                }
                if !public_models.insert(&model.public_model) {
                    return Err(ConfigError::new(format!(
                        "duplicate public model '{}'",
                        model.public_model
                    )));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ProviderRegistry {
    routes: HashMap<String, ModelRoute>,
    model_order: Vec<String>,
}

impl ProviderRegistry {
    pub fn new(config: AppConfig, api_keys: HashMap<String, String>) -> Result<Self, ConfigError> {
        config.validate()?;
        let mut routes = HashMap::new();
        let mut model_order = Vec::new();

        for provider in config.providers {
            let api_key = api_keys.get(&provider.name).ok_or_else(|| {
                ConfigError::new(format!(
                    "missing API key for provider '{}' in environment variable '{}'",
                    provider.name, provider.api_key_env
                ))
            })?;
            if api_key.is_empty() {
                return Err(ConfigError::new(format!(
                    "API key for provider '{}' must not be empty",
                    provider.name
                )));
            }
            let base_url = provider.base_url.trim_end_matches('/').to_string();
            for model in provider.models {
                let route = ModelRoute {
                    origin_model: model.upstream_model,
                    public_model: model.public_model.clone(),
                    provider_name: provider.name.clone(),
                    upstream_base_url: base_url.clone(),
                    api_key: api_key.clone(),
                    channel: model.response_adapter,
                    supports_compact: provider.supports_compact,
                };
                if routes.insert(model.public_model.clone(), route).is_some() {
                    return Err(ConfigError::new(format!(
                        "duplicate public model '{}'",
                        model.public_model
                    )));
                }
                model_order.push(model.public_model);
            }
        }

        Ok(Self {
            routes,
            model_order,
        })
    }

    pub fn route_for_public_model(&self, public_model: &str) -> Option<ModelRoute> {
        self.routes.get(public_model).cloned()
    }

    pub fn public_models_list(&self) -> Value {
        let data = self
            .model_order
            .iter()
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

[[providers]]
name = "ada"
base_url = "http://ada.example/v1"
api_key_env = "ADA_API_KEY"
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
name = "glm"
base_url = "https://glm.example/v1"
api_key_env = "GLM_API_KEY"
supports_compact = true

[[providers.models]]
public_model = "gpt-other"
upstream_model = "glm-other"
response_adapter = "standard"
"#;

    fn api_keys() -> HashMap<String, String> {
        HashMap::from([
            ("ada".to_string(), "ada-secret".to_string()),
            ("glm".to_string(), "glm-secret".to_string()),
        ])
    }

    #[test]
    fn loads_multiple_providers_and_resolves_route_details() {
        let config = AppConfig::from_toml(CONFIG).unwrap();
        let registry = ProviderRegistry::new(config, api_keys()).unwrap();

        let route = registry.route_for_public_model("gpt-other").unwrap();
        assert_eq!(route.provider_name, "glm");
        assert_eq!(route.origin_model, "glm-other");
        assert_eq!(route.upstream_base_url, "https://glm.example/v1");
        assert_eq!(route.api_key, "glm-secret");
        assert!(route.supports_compact);
        assert_eq!(route.channel, ChannelKind::Standard);

        let deepseek_route = registry.route_for_public_model("gpt-luna").unwrap();
        assert_eq!(deepseek_route.channel, ChannelKind::DeepSeek);
        assert!(!deepseek_route.supports_compact);

        let same_provider_standard = registry.route_for_public_model("gpt-sol").unwrap();
        assert_eq!(same_provider_standard.provider_name, "ada");
        assert_eq!(same_provider_standard.channel, ChannelKind::Standard);
        assert!(!same_provider_standard.supports_compact);

        let body = registry.public_models_list();
        let ids = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["gpt-luna", "gpt-terra", "gpt-sol", "gpt-other"]);
    }

    #[test]
    fn rejects_duplicate_public_models_across_providers() {
        let config = AppConfig::from_toml(&CONFIG.replace("gpt-sol", "gpt-luna")).unwrap();
        let error = ProviderRegistry::new(config, api_keys()).unwrap_err();
        assert!(error.to_string().contains("duplicate public model"));
    }

    #[test]
    fn rejects_provider_without_models() {
        let config = AppConfig::from_toml(
            r#"
            [[providers]]
            name = "empty"
            base_url = "https://example.com/v1"
            api_key_env = "EMPTY_API_KEY"
            supports_compact = false
            "#,
        )
        .unwrap();

        let error = ProviderRegistry::new(config, HashMap::new()).unwrap_err();
        assert!(error.to_string().contains("must define at least one model"));
    }

    #[test]
    fn rejects_invalid_provider_url() {
        let config =
            AppConfig::from_toml(&CONFIG.replace("https://glm.example/v1", "not-a-url")).unwrap();
        let error = ProviderRegistry::new(config, api_keys()).unwrap_err();
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
