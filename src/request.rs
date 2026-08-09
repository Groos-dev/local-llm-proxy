use crate::model::ModelRoute;
use serde_json::Value;

/// Codex-facing request prep, then upstream-channel-specific adaptations.
pub fn normalize_request_for_upstream(route: ModelRoute, body: &mut Value) {
    // Codex client quirk (all channels): tools often arrive as additional_tools.
    promote_additional_tools(body);
    route.channel.normalize_request(body);
}

fn promote_additional_tools(body: &mut Value) {
    let already_has_tools = has_tools(body);
    let Some(input) = body.get_mut("input").and_then(|input| input.as_array_mut()) else {
        return;
    };

    let mut promoted = Vec::new();
    let mut kept = Vec::new();
    for item in input.drain(..) {
        let is_additional_tools = item.get("type").and_then(|value| value.as_str())
            == Some("additional_tools");
        if !is_additional_tools {
            kept.push(item);
            continue;
        }
        promoted.extend(tools_from_additional_tools_item(&item));
    }
    *input = kept;

    if already_has_tools || promoted.is_empty() {
        return;
    }
    body["tools"] = Value::Array(promoted);
}

fn tools_from_additional_tools_item(item: &Value) -> Vec<Value> {
    let Some(entries) = item.get("tools").and_then(|tools| tools.as_array()) else {
        return Vec::new();
    };

    let mut promoted = Vec::new();
    for entry in entries {
        match entry.get("type").and_then(|value| value.as_str()) {
            Some("namespace") => {
                for tool in entry
                    .get("tools")
                    .and_then(|tools| tools.as_array())
                    .into_iter()
                    .flatten()
                {
                    promoted.push(tool.clone());
                }
            }
            // Codex Desktop: flat function/custom tools under additional_tools.tools[]
            Some(_) if entry.get("name").and_then(|value| value.as_str()).is_some() => {
                promoted.push(entry.clone());
            }
            _ => {}
        }
    }
    promoted
}

fn has_tools(body: &Value) -> bool {
    body.get("tools")
        .and_then(|tools| tools.as_array())
        .is_some_and(|tools| !tools.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::route_for_public_model;
    use serde_json::json;

    fn deepseek() -> ModelRoute {
        route_for_public_model("gpt-5.6-luna").unwrap()
    }

    fn standard() -> ModelRoute {
        route_for_public_model("gpt-5.6-sol").unwrap()
    }

    #[test]
    fn forces_effort_none_when_tool_choice_required() {
        let mut request = json!({
            "model": "DeepSeek-V4-Flash-0731",
            "tools": [{"type": "function", "name": "get_weather"}],
            "tool_choice": "required",
            "reasoning": {"effort": "high", "summary": "detailed"}
        });

        normalize_request_for_upstream(deepseek(), &mut request);

        assert_eq!(request["reasoning"]["effort"], "none");
        assert_eq!(request["reasoning"]["summary"], "detailed");
    }

    #[test]
    fn forces_effort_none_for_forced_tool_object_even_without_reasoning() {
        let mut request = json!({
            "tools": [{"type": "custom", "name": "shell"}],
            "tool_choice": {"type": "custom", "name": "shell"}
        });

        normalize_request_for_upstream(deepseek(), &mut request);

        assert_eq!(request["reasoning"]["effort"], "none");
    }

    #[test]
    fn leaves_effort_unchanged_for_auto_tool_choice() {
        let mut request = json!({
            "tools": [{"type": "function", "name": "get_weather"}],
            "tool_choice": "auto",
            "reasoning": {"effort": "high"}
        });

        normalize_request_for_upstream(deepseek(), &mut request);

        assert_eq!(request["reasoning"]["effort"], "high");
    }

    #[test]
    fn leaves_effort_unchanged_without_tools() {
        let mut request = json!({
            "tool_choice": "required",
            "reasoning": {"effort": "high"}
        });

        normalize_request_for_upstream(deepseek(), &mut request);

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

        normalize_request_for_upstream(standard(), &mut request);

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

        normalize_request_for_upstream(standard(), &mut request);

        assert_eq!(
            request["tools"],
            json!([
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
            ])
        );
        assert_eq!(request["input"].as_array().unwrap().len(), 1);
        assert_eq!(request["input"][0]["content"], "ls");
        assert!(
            request
                .get("tools")
                .and_then(|t| t.as_array())
                .unwrap()
                .iter()
                .any(|t| t["name"] == "exec")
        );
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

        normalize_request_for_upstream(deepseek(), &mut request);

        assert_eq!(
            request["tools"],
            json!([
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
                },
                {
                    "type": "function",
                    "name": "spawn_agent",
                    "description": "Spawn",
                    "parameters": {"type": "object", "properties": {}}
                }
            ])
        );
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

        normalize_request_for_upstream(standard(), &mut request);

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

        normalize_request_for_upstream(deepseek(), &mut request);

        assert_eq!(request["tools"][0]["name"], "exec");
        assert_eq!(request["reasoning"]["effort"], "none");
    }

    #[test]
    fn forces_store_false_and_strips_encrypted_include() {
        let mut request = json!({
            "store": true,
            "include": ["reasoning.encrypted_content", "file_search_call.results"],
            "reasoning": {"effort": "high"}
        });

        normalize_request_for_upstream(deepseek(), &mut request);

        assert_eq!(request["store"], false);
        assert_eq!(request["include"], json!(["file_search_call.results"]));
        assert_eq!(request["reasoning"]["effort"], "high");
    }
}
