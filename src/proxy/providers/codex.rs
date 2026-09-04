//! Codex provider routing helpers (aligned with cc-switch `providers/codex.rs`).

use crate::config::{ApiFormat, Provider};

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn provider(format: ApiFormat, base_url: &str) -> Provider {
        Provider {
            name: "t".into(),
            base_url: base_url.into(),
            api_key: "k".into(),
            api_format: format,
            max_output_tokens: None,
            model_mappings: HashMap::new(),
        }
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
}
