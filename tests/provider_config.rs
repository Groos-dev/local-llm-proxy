use local_llm_proxy::{AppConfig, ChannelKind, ProviderRegistry};
use std::collections::HashMap;

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
