use crate::model::ModelRoute;
use serde_json::{Value, json};

const DEFAULT_COMPACT_PROMPT: &str = "Summarize the conversation so far. Preserve the user's goals, important decisions, completed work, unresolved issues, and the next steps needed to continue. Be concise but retain details that are necessary for the next model turn.";

pub fn build_model_fallback_request(route: &ModelRoute, request: &Value) -> Value {
    let mut body = request.clone();
    body["model"] = Value::String(route.origin_model.clone());

    let input = body
        .get_mut("input")
        .and_then(Value::as_array_mut)
        .map(|input| {
            input.push(json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": DEFAULT_COMPACT_PROMPT}]
            }));
            input.clone()
        })
        .unwrap_or_else(|| {
            vec![json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": DEFAULT_COMPACT_PROMPT}]
            })]
        });
    body["input"] = Value::Array(input);

    body["tools"] = json!([]);
    body["parallel_tool_calls"] = Value::Bool(false);
    if body.get("tool_choice").is_none() {
        body["tool_choice"] = Value::String("auto".to_string());
    }
    body["store"] = Value::Bool(false);
    body["stream"] = Value::Bool(true);
    if body.get("include").is_none() {
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    body
}

pub fn compact_response_from_model_response(route: &ModelRoute, response: Value) -> Option<Value> {
    let output = response
        .get("output")
        .or_else(|| response.pointer("/response/output"))
        .and_then(Value::as_array)?
        .clone();
    if output.is_empty() {
        return None;
    }
    Some(json!({
        "model": route.public_model,
        "output": output,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChannelKind;

    fn route(channel: ChannelKind, public_model: &str, origin_model: &str) -> ModelRoute {
        ModelRoute {
            origin_model: origin_model.to_string(),
            public_model: public_model.to_string(),
            provider_name: "provider".to_string(),
            upstream_base_url: "https://example.com/v1".to_string(),
            api_key: "secret".to_string(),
            channel,
            supports_compact: false,
        }
    }

    #[test]
    fn model_fallback_request_matches_normal_codex_compaction_request() {
        let route = route(
            ChannelKind::DeepSeek,
            "gpt-5.6-terra",
            "DeepSeek-V4-Pro-discount",
        );
        let request = json!({
            "model": "DeepSeek-V4-Pro-discount",
            "instructions": "base instructions",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]}],
            "tools": [{"type": "function", "name": "exec"}],
            "parallel_tool_calls": true,
            "reasoning": {"effort": "high"},
            "service_tier": "priority",
            "prompt_cache_key": "cache-key",
            "text": {"verbosity": "low"}
        });

        let body = build_model_fallback_request(&route, &request);
        let input = body["input"].as_array().unwrap();

        assert_eq!(body["model"], "DeepSeek-V4-Pro-discount");
        assert_eq!(body["instructions"], "base instructions");
        assert_eq!(body["tools"], json!([]));
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["service_tier"], "priority");
        assert_eq!(body["prompt_cache_key"], "cache-key");
        assert_eq!(body["text"]["verbosity"], "low");
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["content"][0]["text"], "hello");
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["role"], "user");
        assert!(
            input[1]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("conversation")
        );
    }

    #[test]
    fn model_response_is_wrapped_as_compact_response() {
        let route = route(ChannelKind::Standard, "gpt-5.6-sol", "glm-5.2-discount");
        let response = json!({
            "id": "resp-1",
            "model": "glm-5.2-discount",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "summary"}]
            }]
        });

        let compact = compact_response_from_model_response(&route, response).unwrap();

        assert_eq!(compact["model"], "gpt-5.6-sol");
        assert_eq!(compact["output"][0]["role"], "assistant");
        assert_eq!(compact["output"][0]["content"][0]["text"], "summary");
        assert_eq!(compact.as_object().unwrap().len(), 2);
    }

    #[test]
    fn invalid_model_response_cannot_be_returned_as_compact_response() {
        let route = route(ChannelKind::Standard, "gpt-5.6-sol", "glm-5.2-discount");

        assert!(compact_response_from_model_response(&route, json!({"id": "resp-1"})).is_none());
        assert!(compact_response_from_model_response(&route, json!({"output": []})).is_none());
    }
}
