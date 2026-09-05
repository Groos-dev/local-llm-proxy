//! Codex provider routing helpers (aligned with cc-switch `providers/codex.rs`).

use crate::config::{ApiFormat, Provider};
use crate::provider::CodexChatReasoningConfig;
use serde_json::Value;

/// Whether a Codex `/responses` request must be bridged to Chat Completions.
pub fn should_convert_codex_responses_to_chat(provider: &Provider, endpoint: &str) -> bool {
    responses_family_endpoint(endpoint) && matches!(provider.api_format, ApiFormat::OpenaiChat)
}

/// Whether a Codex `/responses` request must be bridged to Anthropic Messages.
pub fn should_convert_codex_responses_to_anthropic(provider: &Provider, endpoint: &str) -> bool {
    responses_family_endpoint(endpoint) && matches!(provider.api_format, ApiFormat::Anthropic)
}

fn responses_family_endpoint(endpoint: &str) -> bool {
    let path = endpoint
        .split_once('?')
        .map_or(endpoint, |(path, _query)| path);
    matches!(
        path,
        "/responses" | "/v1/responses" | "/responses/compact" | "/v1/responses/compact"
    )
}

/// Native Responses passthrough to gateways that reject Codex `namespace` tools
/// (notably `api.x.ai`).
pub fn provider_needs_responses_namespace_flatten(provider: &Provider) -> bool {
    if !matches!(provider.api_format, ApiFormat::OpenaiResponses) {
        return false;
    }
    let base = provider.base_url.to_ascii_lowercase();
    base.contains("api.x.ai") || base.contains("x.ai/")
}

/// Infer Chat Completions reasoning wire shape from provider name / base_url / model.
///
/// Aligned with cc-switch: explicit `codex_chat_reasoning` wins; otherwise infer.
/// Zen attaches per-model `effort_levels` from `model_catalog`.
pub fn resolve_codex_chat_reasoning_config(
    provider: &Provider,
    body: &Value,
) -> Option<CodexChatReasoningConfig> {
    let mut config = if let Some(config) = provider.codex_chat_reasoning.clone() {
        normalize_codex_chat_reasoning_config(config)
    } else {
        infer_codex_chat_reasoning_config(provider, body)?
    };

    if config.effort_value_mode.as_deref() == Some("zen") {
        config.effort_levels = zen_catalog_effort_levels(provider, body);
    }

    Some(config)
}

/// Default upstream model (cc-switch `settings_config.model`).
pub fn codex_provider_upstream_model(provider: &Provider) -> Option<String> {
    provider
        .upstream_model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(ToString::to_string)
}

fn zen_catalog_effort_levels(provider: &Provider, body: &Value) -> Option<Vec<String>> {
    let model = body.get("model")?.as_str()?.trim();
    if model.is_empty() {
        return None;
    }
    let entries = provider.model_catalog.as_ref()?.get("models")?.as_array()?;
    let entry = entries.iter().find(|entry| {
        entry
            .get("model")
            .and_then(|value| value.as_str())
            .is_some_and(|name| name.eq_ignore_ascii_case(model))
    })?;
    let levels_value = entry
        .get("reasoningLevels")
        .or_else(|| entry.get("reasoning_levels"))?;
    let levels: Vec<String> = levels_value
        .as_array()?
        .iter()
        .filter_map(|level| level.as_str().map(str::to_string))
        .collect();
    (!levels.is_empty()).then_some(levels)
}

fn normalize_codex_chat_reasoning_config(
    mut config: CodexChatReasoningConfig,
) -> CodexChatReasoningConfig {
    if config.supports_effort.unwrap_or(false) && config.supports_thinking.is_none() {
        config.supports_thinking = Some(true);
    }
    config
}

fn infer_codex_chat_reasoning_config(
    provider: &Provider,
    body: &Value,
) -> Option<CodexChatReasoningConfig> {
    let model = body
        .get("model")
        .and_then(|value| value.as_str())
        .map(ToString::to_string)
        .or_else(|| codex_provider_upstream_model(provider))
        .unwrap_or_default()
        .to_ascii_lowercase();
    let base_url = provider.base_url.to_ascii_lowercase();
    let name = provider.name.to_ascii_lowercase();

    if let Some(config) = infer_aggregator_platform_config(&name, &base_url) {
        return Some(config);
    }

    let haystack = format!("{name} {base_url} {model}");

    if haystack.contains("deepseek") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(true),
            supports_effort: Some(true),
            thinking_param: Some("thinking".to_string()),
            effort_param: Some("reasoning_effort".to_string()),
            effort_value_mode: Some("deepseek".to_string()),
            output_format: Some("reasoning_content".to_string()),
            effort_levels: None,
        });
    }

    if haystack.contains("stepfun") || haystack.contains("step-3.5-flash-2603") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(true),
            supports_effort: Some(model.contains("2603") || model.contains("step-3.7-flash")),
            thinking_param: Some("none".to_string()),
            effort_param: Some("reasoning_effort".to_string()),
            effort_value_mode: Some(
                if model.contains("2603") {
                    "low_high"
                } else {
                    "passthrough"
                }
                .to_string(),
            ),
            output_format: Some("reasoning".to_string()),
            effort_levels: None,
        });
    }

    if haystack.contains("kimi") || haystack.contains("moonshot") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(true),
            supports_effort: Some(false),
            thinking_param: Some("thinking".to_string()),
            effort_param: Some("none".to_string()),
            effort_value_mode: None,
            output_format: Some("reasoning_content".to_string()),
            effort_levels: None,
        });
    }

    if haystack.contains("glm") || haystack.contains("zhipu") || haystack.contains("z.ai") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(true),
            supports_effort: Some(false),
            thinking_param: Some("thinking".to_string()),
            effort_param: Some("none".to_string()),
            effort_value_mode: None,
            output_format: Some("reasoning_content".to_string()),
            effort_levels: None,
        });
    }

    if haystack.contains("qwen") || haystack.contains("dashscope") || haystack.contains("bailian") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(true),
            supports_effort: Some(false),
            thinking_param: Some("enable_thinking".to_string()),
            effort_param: Some("none".to_string()),
            effort_value_mode: None,
            output_format: Some("reasoning_content".to_string()),
            effort_levels: None,
        });
    }

    if haystack.contains("minimax") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(true),
            supports_effort: Some(false),
            thinking_param: Some("reasoning_split".to_string()),
            effort_param: Some("none".to_string()),
            effort_value_mode: None,
            output_format: Some("reasoning_details".to_string()),
            effort_levels: None,
        });
    }

    if haystack.contains("mimo") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(true),
            supports_effort: Some(false),
            thinking_param: Some("thinking".to_string()),
            effort_param: Some("none".to_string()),
            effort_value_mode: None,
            output_format: Some("reasoning_content".to_string()),
            effort_levels: None,
        });
    }

    None
}

fn infer_aggregator_platform_config(
    name: &str,
    base_url: &str,
) -> Option<CodexChatReasoningConfig> {
    let platform = format!("{name} {base_url}");

    if platform.contains("openrouter") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(false),
            supports_effort: Some(true),
            thinking_param: Some("none".to_string()),
            effort_param: Some("reasoning.effort".to_string()),
            effort_value_mode: Some("openrouter".to_string()),
            output_format: Some("auto".to_string()),
            effort_levels: None,
        });
    }

    if platform.contains("siliconflow") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(true),
            supports_effort: Some(false),
            thinking_param: Some("enable_thinking".to_string()),
            effort_param: Some("none".to_string()),
            effort_value_mode: None,
            output_format: Some("reasoning_content".to_string()),
            effort_levels: None,
        });
    }

    if platform.contains("modelscope") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(true),
            supports_effort: Some(false),
            thinking_param: Some("enable_thinking".to_string()),
            effort_param: Some("none".to_string()),
            effort_value_mode: None,
            output_format: Some("reasoning_content".to_string()),
            effort_levels: None,
        });
    }

    if platform.contains("opencode.ai") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(true),
            supports_effort: Some(true),
            thinking_param: Some("none".to_string()),
            effort_param: Some("reasoning_effort".to_string()),
            effort_value_mode: Some("zen".to_string()),
            output_format: Some("reasoning_content".to_string()),
            effort_levels: None,
        });
    }

    None
}

/// Whether Chat Completions upstream accepts `prompt_cache_key`.
/// Unknown gateways default to false (many 400 on unknown fields).
pub fn should_send_codex_chat_prompt_cache_key(provider: &Provider) -> bool {
    match std::env::var("AGENT_PROXY_PROMPT_CACHE")
        .unwrap_or_else(|_| "auto".into())
        .to_ascii_lowercase()
        .as_str()
    {
        "enabled" | "1" | "true" | "on" => return true,
        "disabled" | "0" | "false" | "off" => return false,
        _ => {}
    }

    let Ok(url) = reqwest::Url::parse(&provider.base_url) else {
        return false;
    };
    match url.host_str() {
        Some("api.openai.com") => true,
        Some("api.kimi.com") => {
            let path = url.path().trim_end_matches('/');
            path == "/coding" || path.starts_with("/coding/")
        }
        _ => false,
    }
}

/// Inject a stable `prompt_cache_key` after Responses → Chat conversion.
/// Explicit client key wins; otherwise only a real client-provided session ID.
pub fn inject_codex_chat_prompt_cache_key(
    provider: &Provider,
    chat_body: &mut Value,
    explicit_key: Option<&str>,
    client_session_id: Option<&str>,
) -> bool {
    if !should_send_codex_chat_prompt_cache_key(provider) {
        return false;
    }

    let key = explicit_key
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .or_else(|| {
            client_session_id
                .map(str::trim)
                .filter(|session_id| !session_id.is_empty())
        });
    let Some(key) = key else {
        return false;
    };

    chat_body["prompt_cache_key"] = Value::String(key.to_string());
    true
}

/// Client-provided Codex session id suitable for `prompt_cache_key` (never synthetic).
pub fn client_provided_codex_session_id(
    headers: &axum::http::HeaderMap,
    body: &Value,
) -> Option<String> {
    for header_name in ["session_id", "x-session-id"] {
        if let Some(value) = headers.get(header_name).and_then(|v| v.to_str().ok()) {
            let session_id = value.trim();
            if session_id.len() > 20 {
                return Some(format!("codex_{session_id}"));
            }
        }
    }
    body.get("metadata")
        .and_then(|m| m.get("session_id"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("codex_{s}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::CodexChatReasoningConfig;
    use axum::http::{HeaderMap, HeaderValue};
    use serde_json::json;
    use std::collections::HashMap;

    fn provider(format: ApiFormat, base_url: &str) -> Provider {
        Provider {
            name: "t".into(),
            base_url: base_url.into(),
            is_full_url: false,
            api_key: "k".into(),
            api_format: format,
            max_output_tokens: None,
            upstream_model: None,
            codex_chat_reasoning: None,
            model_catalog: None,
            model_mappings: HashMap::new(),
        }
    }

    fn named_provider(name: &str, format: ApiFormat, base_url: &str) -> Provider {
        let mut p = provider(format, base_url);
        p.name = name.into();
        p
    }

    #[test]
    fn convert_gates_follow_api_format() {
        let chat = provider(ApiFormat::OpenaiChat, "https://example.com/v1");
        let anth = provider(ApiFormat::Anthropic, "https://example.com");
        let resp = provider(ApiFormat::OpenaiResponses, "https://example.com/v1");
        assert!(should_convert_codex_responses_to_chat(&chat, "/v1/responses"));
        assert!(!should_convert_codex_responses_to_chat(&resp, "/v1/responses"));
        assert!(should_convert_codex_responses_to_anthropic(
            &anth,
            "/responses/compact"
        ));
        assert!(!should_convert_codex_responses_to_anthropic(
            &resp,
            "/responses"
        ));
    }

    #[test]
    fn xai_flatten_only_for_responses_on_xai_host() {
        let xai = provider(ApiFormat::OpenaiResponses, "https://api.x.ai/v1");
        let other = provider(ApiFormat::OpenaiResponses, "https://api.openai.com/v1");
        let chat_xai = provider(ApiFormat::OpenaiChat, "https://api.x.ai/v1");
        assert!(provider_needs_responses_namespace_flatten(&xai));
        assert!(!provider_needs_responses_namespace_flatten(&other));
        assert!(!provider_needs_responses_namespace_flatten(&chat_xai));
    }

    #[test]
    fn reasoning_config_infers_deepseek_and_openrouter() {
        let deepseek = named_provider(
            "deepseek",
            ApiFormat::OpenaiChat,
            "https://api.deepseek.com/v1",
        );
        let cfg = resolve_codex_chat_reasoning_config(
            &deepseek,
            &json!({ "model": "deepseek-reasoner" }),
        )
        .unwrap();
        assert_eq!(cfg.thinking_param.as_deref(), Some("thinking"));
        assert_eq!(cfg.effort_param.as_deref(), Some("reasoning_effort"));

        let openrouter = named_provider(
            "or",
            ApiFormat::OpenaiChat,
            "https://openrouter.ai/api/v1",
        );
        let cfg =
            resolve_codex_chat_reasoning_config(&openrouter, &json!({ "model": "any-model" }))
                .unwrap();
        assert_eq!(cfg.effort_param.as_deref(), Some("reasoning.effort"));
        assert_eq!(cfg.effort_value_mode.as_deref(), Some("openrouter"));
    }

    #[test]
    fn reasoning_explicit_config_overrides_inference() {
        let mut deepseek = named_provider(
            "deepseek",
            ApiFormat::OpenaiChat,
            "https://api.deepseek.com/v1",
        );
        deepseek.codex_chat_reasoning = Some(CodexChatReasoningConfig {
            supports_thinking: Some(false),
            supports_effort: Some(false),
            thinking_param: Some("none".into()),
            effort_param: Some("none".into()),
            effort_value_mode: None,
            output_format: Some("auto".into()),
            effort_levels: None,
        });
        let cfg = resolve_codex_chat_reasoning_config(
            &deepseek,
            &json!({ "model": "deepseek-v4-pro" }),
        )
        .unwrap();
        assert_eq!(cfg.supports_thinking, Some(false));
        assert_eq!(cfg.thinking_param.as_deref(), Some("none"));
    }

    #[test]
    fn reasoning_openrouter_platform_overrides_deepseek_model_name() {
        let openrouter = named_provider(
            "openrouter",
            ApiFormat::OpenaiChat,
            "https://openrouter.ai/api/v1",
        );
        let cfg = resolve_codex_chat_reasoning_config(
            &openrouter,
            &json!({ "model": "deepseek/deepseek-chat-v3.1" }),
        )
        .unwrap();
        assert_eq!(cfg.thinking_param.as_deref(), Some("none"));
        assert_eq!(cfg.effort_param.as_deref(), Some("reasoning.effort"));
        assert_eq!(cfg.effort_value_mode.as_deref(), Some("openrouter"));
    }

    #[test]
    fn reasoning_zen_attaches_per_model_effort_levels() {
        let mut zen = named_provider(
            "zen",
            ApiFormat::OpenaiChat,
            "https://opencode.ai/zen/v1",
        );
        zen.model_catalog = Some(json!({
            "models": [
                { "model": "GLM-5.2", "reasoningLevels": ["high", "max"] },
                { "model": "kimi-k3", "reasoningLevels": ["max"] },
                { "model": "glm-5.1" }
            ]
        }));
        let cfg =
            resolve_codex_chat_reasoning_config(&zen, &json!({ "model": "GLM-5.2" })).unwrap();
        assert_eq!(cfg.effort_value_mode.as_deref(), Some("zen"));
        assert_eq!(
            cfg.effort_levels.as_deref(),
            Some(["high".to_string(), "max".to_string()].as_slice())
        );

        let cfg =
            resolve_codex_chat_reasoning_config(&zen, &json!({ "model": "glm-5.1" })).unwrap();
        assert_eq!(cfg.effort_levels, None);

        let cfg =
            resolve_codex_chat_reasoning_config(&zen, &json!({ "model": "kimi-k3" })).unwrap();
        assert_eq!(cfg.effort_levels.as_deref(), Some(["max".to_string()].as_slice()));
    }

    #[test]
    fn reasoning_zen_levels_attach_on_explicit_config_too() {
        let mut zen = named_provider(
            "zen",
            ApiFormat::OpenaiChat,
            "https://opencode.ai/zen/v1",
        );
        zen.model_catalog = Some(json!({
            "models": [{ "model": "glm-5.2", "reasoning_levels": ["high", "max"] }]
        }));
        zen.codex_chat_reasoning = Some(CodexChatReasoningConfig {
            supports_thinking: Some(true),
            supports_effort: Some(true),
            thinking_param: Some("none".into()),
            effort_param: Some("reasoning_effort".into()),
            effort_value_mode: Some("zen".into()),
            output_format: Some("reasoning_content".into()),
            effort_levels: None,
        });
        let cfg =
            resolve_codex_chat_reasoning_config(&zen, &json!({ "model": "glm-5.2" })).unwrap();
        assert_eq!(
            cfg.effort_levels.as_deref(),
            Some(["high".to_string(), "max".to_string()].as_slice())
        );
    }

    #[test]
    fn reasoning_infers_from_upstream_model_when_body_omits_model() {
        let mut deepseek = named_provider(
            "x",
            ApiFormat::OpenaiChat,
            "https://relay.example/v1",
        );
        deepseek.upstream_model = Some("deepseek-reasoner".into());
        let cfg = resolve_codex_chat_reasoning_config(&deepseek, &json!({})).unwrap();
        assert_eq!(cfg.effort_value_mode.as_deref(), Some("deepseek"));
    }

    #[test]
    fn reasoning_stepfun_per_model_effort() {
        let step = named_provider(
            "stepfun",
            ApiFormat::OpenaiChat,
            "https://api.stepfun.com/v1",
        );
        let cfg = resolve_codex_chat_reasoning_config(
            &step,
            &json!({ "model": "step-3.7-flash" }),
        )
        .unwrap();
        assert_eq!(cfg.supports_effort, Some(true));
        assert_eq!(cfg.effort_value_mode.as_deref(), Some("passthrough"));

        let cfg = resolve_codex_chat_reasoning_config(
            &step,
            &json!({ "model": "step-3.5-flash-2603" }),
        )
        .unwrap();
        assert_eq!(cfg.effort_value_mode.as_deref(), Some("low_high"));

        let cfg = resolve_codex_chat_reasoning_config(
            &step,
            &json!({ "model": "step-3.5-flash" }),
        )
        .unwrap();
        assert_eq!(cfg.supports_effort, Some(false));
    }

    #[test]
    fn prompt_cache_key_only_for_allowlisted_hosts() {
        let openai = provider(ApiFormat::OpenaiChat, "https://api.openai.com/v1");
        let other = provider(ApiFormat::OpenaiChat, "https://example.com/v1");
        let mut body = json!({ "model": "gpt" });
        assert!(inject_codex_chat_prompt_cache_key(
            &openai,
            &mut body,
            Some("sess-1"),
            None
        ));
        assert_eq!(body["prompt_cache_key"], "sess-1");

        let mut body = json!({ "model": "gpt" });
        assert!(!inject_codex_chat_prompt_cache_key(
            &other,
            &mut body,
            Some("sess-1"),
            None
        ));
        assert!(body.get("prompt_cache_key").is_none());
    }

    #[test]
    fn client_session_prefers_header_then_metadata() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-session-id",
            HeaderValue::from_static("0123456789abcdef0123456789"),
        );
        let body = json!({ "metadata": { "session_id": "meta-session" } });
        assert_eq!(
            client_provided_codex_session_id(&headers, &body).as_deref(),
            Some("codex_0123456789abcdef0123456789")
        );

        let headers = HeaderMap::new();
        assert_eq!(
            client_provided_codex_session_id(&headers, &body).as_deref(),
            Some("codex_meta-session")
        );
    }
}
