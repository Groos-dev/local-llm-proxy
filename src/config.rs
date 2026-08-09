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
