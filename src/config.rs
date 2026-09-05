use crate::provider::CodexChatReasoningConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashSet,
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
    /// Provider selected on start; its credentials are used for upstream calls,
    /// and its identity drives Codex live base_url/auth takeover.
    pub active_provider: String,
    pub providers: Vec<ProviderConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiFormat {
    OpenaiResponses,
    OpenaiChat,
    Anthropic,
}

impl ApiFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenaiResponses => "openai_responses",
            Self::OpenaiChat => "openai_chat",
            Self::Anthropic => "anthropic",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "openai_responses" | "responses" | "openai-responses" => Some(Self::OpenaiResponses),
            "openai_chat"
            | "chat"
            | "chat_completions"
            | "chat-completions"
            | "openai-chat"
            | "openai_chat_completions" => Some(Self::OpenaiChat),
            "anthropic" | "anthropic_messages" | "anthropic-messages" | "claude" | "messages" => {
                Some(Self::Anthropic)
            }
            _ => None,
        }
    }
}

impl Default for ApiFormat {
    fn default() -> Self {
        Self::OpenaiResponses
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderConfig {
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub is_full_url: bool,
    pub api_key: String,
    #[serde(default)]
    pub api_format: ApiFormat,
    /// Optional identity mapping seed when migrating from TOML.
    #[serde(default)]
    pub upstream_model: Option<String>,
    /// Optional output ceiling injected as Responses `max_output_tokens` before
    /// Anthropic conversion (mirrors cc-switch provider meta).
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    /// Explicit Responses→Chat reasoning wire shape (cc-switch `meta.codex_chat_reasoning`).
    #[serde(default)]
    pub codex_chat_reasoning: Option<CodexChatReasoningConfig>,
    /// Optional model catalog (cc-switch `settings_config.modelCatalog`) for Zen effort levels.
    #[serde(default)]
    pub model_catalog: Option<Value>,
}

#[derive(Clone, Debug)]
pub struct Provider {
    pub name: String,
    pub base_url: String,
    pub is_full_url: bool,
    pub api_key: String,
    pub api_format: ApiFormat,
    pub max_output_tokens: Option<u64>,
    /// Default upstream model (cc-switch `settings_config.model`) for reasoning infer fallback.
    pub upstream_model: Option<String>,
    /// Explicit Responses→Chat reasoning wire shape (cc-switch `meta.codex_chat_reasoning`).
    pub codex_chat_reasoning: Option<CodexChatReasoningConfig>,
    /// Optional model catalog for Zen per-model `reasoningLevels`.
    pub model_catalog: Option<Value>,
    /// Codex/client model id → upstream model id.
    pub model_mappings: std::collections::HashMap<String, String>,
}

impl Provider {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.name.trim().is_empty() {
            return Err(ConfigError::new("provider name must not be empty"));
        }
        if self.api_key.trim().is_empty() {
            return Err(ConfigError::new(format!(
                "provider '{}' api_key must not be empty",
                self.name
            )));
        }
        let url = reqwest::Url::parse(&self.base_url).map_err(|err| {
            ConfigError::new(format!(
                "provider '{}' base_url is invalid: {err}",
                self.name
            ))
        })?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(ConfigError::new(format!(
                "provider '{}' base_url must be an http or https URL",
                self.name
            )));
        }
        Ok(())
    }

    /// Resolve the upstream model for an inbound Codex/client model id.
    /// Unmapped ids pass through; there is no provider-level default.
    pub fn resolve_upstream_model(&self, request_model: Option<&str>) -> Option<String> {
        let req = request_model?;
        Some(
            self.model_mappings
                .get(req)
                .cloned()
                .unwrap_or_else(|| req.to_string()),
        )
    }
}

#[derive(Debug)]
pub struct ConfigError(String);

impl ConfigError {
    pub fn new(message: impl Into<String>) -> Self {
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
        let mut names = HashSet::new();
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
}

impl From<ProviderConfig> for Provider {
    fn from(cfg: ProviderConfig) -> Self {
        let mut model_mappings = std::collections::HashMap::new();
        if let Some(m) = cfg.upstream_model.as_ref() {
            model_mappings.insert(m.clone(), m.clone());
        }
        Self {
            name: cfg.name,
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            is_full_url: cfg.is_full_url,
            api_key: cfg.api_key,
            api_format: cfg.api_format,
            max_output_tokens: cfg.max_output_tokens,
            upstream_model: cfg.upstream_model.clone(),
            codex_chat_reasoning: cfg.codex_chat_reasoning,
            model_catalog: cfg.model_catalog,
            model_mappings,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_api_format_aliases() {
        assert!(matches!(
            ApiFormat::parse("anthropic_messages"),
            Some(ApiFormat::Anthropic)
        ));
        assert!(matches!(
            ApiFormat::parse("openai_chat"),
            Some(ApiFormat::OpenaiChat)
        ));
        assert!(matches!(
            ApiFormat::parse("responses"),
            Some(ApiFormat::OpenaiResponses)
        ));
    }

    #[test]
    fn loads_active_provider() {
        let cfg = AppConfig::from_toml(
            r#"
active_provider = "a"
[[providers]]
name = "a"
base_url = "https://example.com/v1"
api_key = "k"
api_format = "anthropic"
upstream_model = "claude-sonnet"
"#,
        )
        .unwrap();
        let (active, providers) = cfg.into_providers().unwrap();
        assert_eq!(active, "a");
        assert_eq!(providers[0].name, "a");
        assert!(matches!(providers[0].api_format, ApiFormat::Anthropic));
        assert_eq!(
            providers[0]
                .model_mappings
                .get("claude-sonnet")
                .map(String::as_str),
            Some("claude-sonnet")
        );
        assert!(!providers[0].is_full_url);
    }

    #[test]
    fn parses_explicit_full_url_provider_flag() {
        let cfg = AppConfig::from_toml(
            r#"
active_provider = "a"
[[providers]]
name = "a"
base_url = "https://relay.example/generate"
api_key = "k"
is_full_url = true
"#,
        )
        .unwrap();
        let (_, providers) = cfg.into_providers().unwrap();
        assert!(providers[0].is_full_url);
    }

    #[test]
    fn resolve_maps_or_passes_through_without_default() {
        let provider = Provider {
            name: "a".into(),
            base_url: "https://example.com/v1".into(),
            is_full_url: false,
            api_key: "k".into(),
            api_format: ApiFormat::OpenaiResponses,
            max_output_tokens: None,
            upstream_model: None,
            codex_chat_reasoning: None,
            model_catalog: None,
            model_mappings: std::collections::HashMap::from([("client".into(), "upstream".into())]),
        };
        assert_eq!(
            provider.resolve_upstream_model(Some("client")).as_deref(),
            Some("upstream")
        );
        assert_eq!(
            provider.resolve_upstream_model(Some("other")).as_deref(),
            Some("other")
        );
        assert_eq!(provider.resolve_upstream_model(None), None);
    }

    #[test]
    fn rejects_invalid_provider_connection() {
        let cases = [
            ("base_url = \"not-a-url\"\napi_key = \"key\"", "base_url"),
            (
                "base_url = \"file:///tmp/provider\"\napi_key = \"key\"",
                "http or https",
            ),
            (
                "base_url = \"https://a.example/v1\"\napi_key = \"\"",
                "api_key",
            ),
        ];
        for (provider, message) in cases {
            let input =
                format!("active_provider = \"a\"\n[[providers]]\nname = \"a\"\n{provider}\n");
            let error = AppConfig::from_toml(&input)
                .unwrap()
                .into_providers()
                .unwrap_err();
            assert!(error.to_string().contains(message), "{error}");
        }
    }

    #[test]
    fn rejects_duplicate_provider_names() {
        let input = r#"
active_provider = "a"
[[providers]]
name = "a"
base_url = "https://a.example/v1"
api_key = "key-a"
[[providers]]
name = "a"
base_url = "https://b.example/v1"
api_key = "key-b"
"#;
        let error = AppConfig::from_toml(input)
            .unwrap()
            .into_providers()
            .unwrap_err();
        assert!(error.to_string().contains("duplicate provider"));
    }
}
