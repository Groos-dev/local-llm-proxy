use crate::model::ModelRoute;
use serde_json::Value;

const NOTOOL_CALL_ID: &str = "call_proxy_notool";

/// Returns true when the last assistant message in the input has no following tool call
/// or tool output, and no prior injection (identified by NOTOOL_CALL_ID) exists.
pub fn should_inject_notool(body: &Value) -> bool {
    let Some(input) = body.get("input").and_then(Value::as_array) else {
        return false;
    };
    if input.is_empty() {
        return false;
    }
    // Already injected once: look for any item whose call_id matches.
    let already_injected = input
        .iter()
        .any(|item| item.get("call_id").and_then(Value::as_str) == Some(NOTOOL_CALL_ID));
    if already_injected {
        return false;
    }
    // Find the last assistant message; check nothing follows it that is a tool call or output.
    let last_assistant_idx = input.iter().rposition(|item| {
        item.get("type").and_then(Value::as_str) == Some("message")
            && item.get("role").and_then(Value::as_str) == Some("assistant")
    });
    let Some(idx) = last_assistant_idx else {
        return false;
    };
    let has_following_tool = input[idx + 1..].iter().any(|item| {
        item.get("type").and_then(Value::as_str).is_some_and(|t| {
            t == "custom_tool_call"
                || t == "function_call"
                || t == "custom_tool_call_output"
                || t == "function_call_output"
        })
    });
    !has_following_tool
}

/// Codex-facing request prep, then upstream-channel-specific adaptations.
pub fn normalize_request_for_upstream(route: &ModelRoute, body: &mut Value) {
    route.channel.normalize_request(body);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::ChannelKind;
    use serde_json::json;

    fn route(channel: ChannelKind) -> ModelRoute {
        ModelRoute {
            origin_model: "upstream-model".to_string(),
            public_model: "public-model".to_string(),
            provider_name: "provider".to_string(),
            upstream_base_url: "https://example.com/v1".to_string(),
            api_key: "secret".to_string(),
            channel,
            supports_compact: false,
        }
    }

    fn deepseek() -> ModelRoute {
        route(ChannelKind::DeepSeek)
    }

    fn standard() -> ModelRoute {
        route(ChannelKind::Standard)
    }

    fn glm() -> ModelRoute {
        route(ChannelKind::Glm)
    }

    #[test]
    fn forces_effort_none_when_tool_choice_required() {
        let mut request = json!({
            "model": "DeepSeek-V4-Flash-0731",
            "tools": [{"type": "function", "name": "get_weather"}],
            "tool_choice": "required",
            "reasoning": {"effort": "high", "summary": "detailed"}
        });

        normalize_request_for_upstream(&deepseek(), &mut request);

        assert_eq!(request["reasoning"]["effort"], "none");
        assert_eq!(request["reasoning"]["summary"], "detailed");
    }

    #[test]
    fn forces_effort_none_for_forced_tool_object_even_without_reasoning() {
        let mut request = json!({
            "tools": [{"type": "custom", "name": "shell"}],
            "tool_choice": {"type": "custom", "name": "shell"}
        });

        normalize_request_for_upstream(&deepseek(), &mut request);

        assert_eq!(request["reasoning"]["effort"], "none");
    }

    #[test]
    fn leaves_effort_unchanged_for_auto_tool_choice() {
        let mut request = json!({
            "tools": [{"type": "function", "name": "get_weather"}],
            "tool_choice": "auto",
            "reasoning": {"effort": "high"}
        });

        normalize_request_for_upstream(&deepseek(), &mut request);

        assert_eq!(request["reasoning"]["effort"], "high");
    }

    #[test]
    fn should_inject_when_last_item_is_assistant_message_with_no_tool_call() {
        let request = json!({
            "input": [
                {"role": "user", "content": "do something"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "I will read the file."}]}
            ]
        });
        assert!(should_inject_notool(&request));
    }

    #[test]
    fn should_not_inject_when_assistant_message_followed_by_tool_output() {
        let request = json!({
            "input": [
                {"role": "user", "content": "do something"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "reading."}]},
                {"type": "custom_tool_call", "call_id": "call_1", "name": "exec", "input": "", "status": "completed"},
                {"type": "custom_tool_call_output", "call_id": "call_1", "output": "result"}
            ]
        });
        assert!(!should_inject_notool(&request));
    }

    #[test]
    fn should_not_inject_when_already_injected() {
        let request = json!({
            "input": [
                {"role": "user", "content": "do something"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "planning."}]},
                {"type": "custom_tool_call", "call_id": "call_proxy_notool", "name": "exec", "input": "", "status": "completed"},
                {"type": "custom_tool_call_output", "call_id": "call_proxy_notool", "output": "continue"}
            ]
        });
        assert!(!should_inject_notool(&request));
    }

    #[test]
    fn should_not_inject_when_no_input() {
        let request = json!({"model": "gpt-5.6"});
        assert!(!should_inject_notool(&request));
    }

    #[test]
    fn leaves_effort_unchanged_without_tools() {
        let mut request = json!({
            "tool_choice": "required",
            "reasoning": {"effort": "high"}
        });

        normalize_request_for_upstream(&deepseek(), &mut request);

        assert_eq!(request["reasoning"]["effort"], "high");
    }

    #[test]
    fn standard_channel_skips_deepseek_request_rewrites() {
        let mut request = json!({
            "store": true,
            "include": ["reasoning.encrypted_content", "file_search_call.results"],
            "tools": [{"type": "function", "name": "get_weather"}],
            "tool_choice": "required",
            "reasoning": {"effort": "high"}
        });

        normalize_request_for_upstream(&standard(), &mut request);

        assert_eq!(request["store"], true);
        assert_eq!(
            request["include"],
            json!(["reasoning.encrypted_content", "file_search_call.results"])
        );
        assert_eq!(request["reasoning"]["effort"], "high");
    }

    #[test]
    fn promotes_flat_additional_tools_into_top_level_tools() {
        let mut request = json!({
            "tool_choice": "auto",
            "reasoning": {"effort": "high"},
            "input": [
                {
                    "type": "additional_tools",
                    "role": "developer",
                    "tools": [
                        {
                            "type": "custom",
                            "name": "exec",
                            "description": "Run JS",
                            "format": {
                                "type": "grammar",
                                "syntax": "lark",
                                "definition": "start: SOURCE\n"
                            }
                        },
                        {
                            "type": "function",
                            "name": "wait",
                            "description": "Wait",
                            "parameters": {"type": "object", "properties": {}}
                        },
                        {
                            "type": "function",
                            "name": "request_user_input",
                            "description": "Ask user",
                            "parameters": {"type": "object", "properties": {}}
                        }
                    ]
                },
                {"role": "user", "content": "ls"}
            ]
        });

        normalize_request_for_upstream(&deepseek(), &mut request);

        assert_eq!(request["tools"].as_array().unwrap().len(), 3);
        assert_eq!(request["tools"][0]["type"], "custom");
        assert_eq!(request["tools"][0]["name"], "exec");
        assert!(
            request["tools"][0]["description"]
                .as_str()
                .unwrap()
                .starts_with("HARD RULES for `exec`")
        );
        assert!(
            request["tools"][0]["description"]
                .as_str()
                .unwrap()
                .contains("Run JS")
        );
        assert_eq!(request["tools"][1]["name"], "wait");
        assert_eq!(request["tools"][2]["name"], "request_user_input");
        assert_eq!(request["input"].as_array().unwrap().len(), 1);
        assert_eq!(request["input"][0]["content"], "ls");
    }

    #[test]
    fn promotes_additional_tools_namespaces_into_top_level_tools() {
        let mut request = json!({
            "tool_choice": "auto",
            "reasoning": {"effort": "high"},
            "input": [
                {
                    "type": "additional_tools",
                    "role": "developer",
                    "tools": [
                        {
                            "type": "namespace",
                            "name": "functions",
                            "tools": [
                                {
                                    "type": "custom",
                                    "name": "exec",
                                    "description": "Run JS",
                                    "format": {"type": "text"}
                                },
                                {
                                    "type": "function",
                                    "name": "wait",
                                    "description": "Wait",
                                    "parameters": {"type": "object", "properties": {}}
                                }
                            ]
                        },
                        {
                            "type": "namespace",
                            "name": "collaboration",
                            "tools": [
                                {
                                    "type": "function",
                                    "name": "spawn_agent",
                                    "description": "Spawn",
                                    "parameters": {"type": "object", "properties": {}}
                                }
                            ]
                        }
                    ]
                },
                {"role": "user", "content": "hi"}
            ]
        });

        normalize_request_for_upstream(&deepseek(), &mut request);

        assert_eq!(request["tools"].as_array().unwrap().len(), 3);
        assert_eq!(request["tools"][0]["name"], "exec");
        assert!(
            request["tools"][0]["description"]
                .as_str()
                .unwrap()
                .starts_with("HARD RULES for `exec`")
        );
        assert_eq!(request["tools"][1]["name"], "wait");
        assert_eq!(request["tools"][2]["name"], "spawn_agent");
        assert_eq!(request["input"].as_array().unwrap().len(), 1);
        assert_eq!(request["input"][0]["role"], "user");
        assert_eq!(request["reasoning"]["effort"], "high");
    }

    #[test]
    fn does_not_overwrite_existing_top_level_tools() {
        let mut request = json!({
            "tools": [{"type": "function", "name": "keep_me"}],
            "input": [
                {
                    "type": "additional_tools",
                    "role": "developer",
                    "tools": [
                        {
                            "type": "namespace",
                            "name": "functions",
                            "tools": [{"type": "custom", "name": "exec"}]
                        }
                    ]
                }
            ]
        });

        normalize_request_for_upstream(&deepseek(), &mut request);

        assert_eq!(
            request["tools"],
            json!([{"type": "function", "name": "keep_me"}])
        );
        assert!(request["input"].as_array().unwrap().is_empty());
    }

    #[test]
    fn forces_effort_none_after_promoting_additional_tools_with_required_choice() {
        let mut request = json!({
            "tool_choice": "required",
            "reasoning": {"effort": "high"},
            "input": [
                {
                    "type": "additional_tools",
                    "role": "developer",
                    "tools": [
                        {
                            "type": "namespace",
                            "name": "functions",
                            "tools": [{"type": "custom", "name": "exec"}]
                        }
                    ]
                }
            ]
        });

        normalize_request_for_upstream(&deepseek(), &mut request);

        assert_eq!(request["tools"][0]["name"], "exec");
        assert_eq!(request["reasoning"]["effort"], "none");
    }

    #[test]
    fn standard_channel_preserves_additional_tools_item() {
        let mut request = json!({
            "model": "gpt-5.6-sol",
            "instructions": "Keep the latest Responses API shape.",
            "input": [
                {
                    "type": "additional_tools",
                    "role": "developer",
                    "tools": [
                        {
                            "type": "namespace",
                            "name": "functions",
                            "tools": [{"type": "function", "name": "wait"}]
                        }
                    ]
                },
                {"role": "user", "content": "hello"}
            ]
        });
        let original = request.clone();

        normalize_request_for_upstream(&standard(), &mut request);

        assert_eq!(request, original);
    }

    #[test]
    fn glm_channel_promotes_additional_tools_without_deepseek_rewrites() {
        let mut request = json!({
            "store": true,
            "tool_choice": "required",
            "reasoning": {"effort": "high"},
            "include": ["reasoning.encrypted_content"],
            "input": [
                {
                    "type": "additional_tools",
                    "role": "developer",
                    "tools": [
                        {
                            "type": "namespace",
                            "name": "functions",
                            "tools": [{"type": "function", "name": "wait"}]
                        }
                    ]
                },
                {"type": "function_call", "call_id": "call_a", "name": "wait", "arguments": "{}"},
                {"type": "function_call", "call_id": "call_b", "name": "wait", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_b", "output": "b"},
                {"type": "function_call_output", "call_id": "call_a", "output": "a"}
            ]
        });

        normalize_request_for_upstream(&glm(), &mut request);

        assert_eq!(request["tools"][0]["name"], "wait");
        assert_eq!(request["store"], true);
        assert_eq!(request["reasoning"]["effort"], "high");
        assert_eq!(request["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(
            request["input"],
            json!([
                {"type": "function_call", "call_id": "call_a", "name": "wait", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_a", "output": "a"},
                {"type": "function_call", "call_id": "call_b", "name": "wait", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_b", "output": "b"}
            ])
        );
    }

    #[test]
    fn forces_store_false_and_strips_encrypted_include() {
        let mut request = json!({
            "store": true,
            "include": ["reasoning.encrypted_content", "file_search_call.results"],
            "reasoning": {"effort": "high"}
        });

        normalize_request_for_upstream(&deepseek(), &mut request);

        assert_eq!(request["store"], false);
        assert_eq!(request["include"], json!(["file_search_call.results"]));
        assert_eq!(request["reasoning"]["effort"], "high");
    }
}
