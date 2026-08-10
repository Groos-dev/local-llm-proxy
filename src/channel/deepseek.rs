use super::UpstreamChannel;
use super::tool_compat::{
    normalize_exec_tool_calls, promote_additional_tools, rewrite_exec_tool_description,
    serialize_parallel_tool_calls, strip_tool_call_reasoning_content,
};
use serde_json::Value;

pub struct DeepSeekChannel;

impl UpstreamChannel for DeepSeekChannel {
    fn normalize_request(&self, body: &mut Value) {
        // Responses Lite puts tools under input.additional_tools; promote to top-level tools.
        promote_additional_tools(body);
        rewrite_exec_tool_description(body);
        // Ada DeepSeek session state is unreliable; Codex also defaults to store=false.
        if body.get("store").is_some() {
            body["store"] = Value::Bool(false);
        }
        strip_unsupported_include(body);
        // Keep custom_tool_call as-is (upstream accepts top-level custom tools).
        // Ada rejects parallel tool-call batches; serialize to call/output pairs.
        serialize_parallel_tool_calls(body);
        // Ada cannot take Responses json_schema (flat fails deserialize; nested is
        // "unavailable"); downgrade to json_object and keep schema guidance in prompt.
        downgrade_json_schema_text_format(body);
        if !has_tools(body) || !has_forced_tool_choice(body) {
            return;
        }
        match body.get_mut("reasoning") {
            Some(Value::Object(reasoning)) => {
                reasoning.insert("effort".to_string(), Value::String("none".to_string()));
            }
            _ => {
                body["reasoning"] = serde_json::json!({ "effort": "none" });
            }
        }
    }

    fn normalize_response(&self, body: &mut Value) {
        normalize_exec_tool_calls(body);
        strip_tool_call_reasoning_content(body);
    }
}

fn has_tools(body: &Value) -> bool {
    body.get("tools")
        .and_then(|tools| tools.as_array())
        .is_some_and(|tools| !tools.is_empty())
}

fn strip_unsupported_include(body: &mut Value) {
    let Some(include) = body
        .get_mut("include")
        .and_then(|include| include.as_array_mut())
    else {
        return;
    };
    include.retain(|item| {
        item.as_str()
            .is_none_or(|value| value != "reasoning.encrypted_content")
    });
}

fn downgrade_json_schema_text_format(body: &mut Value) {
    let Some(format) = body
        .pointer_mut("/text/format")
        .and_then(|format| format.as_object_mut())
    else {
        return;
    };
    if format.get("type").and_then(|value| value.as_str()) != Some("json_schema") {
        return;
    }
    format.clear();
    format.insert("type".to_string(), Value::String("json_object".to_string()));
}

fn has_forced_tool_choice(body: &Value) -> bool {
    match body.get("tool_choice") {
        Some(Value::String(choice)) => choice == "required",
        Some(Value::Object(_)) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serializes_parallel_function_calls_into_call_output_pairs() {
        let mut request = json!({
            "input": [
                {"type": "message", "role": "assistant", "content": "ok"},
                {"type": "function_call", "call_id": "call_a", "name": "wait", "arguments": "{}"},
                {"type": "function_call", "call_id": "call_b", "name": "wait", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_a", "output": "a"},
                {"type": "function_call_output", "call_id": "call_b", "output": "b"},
                {"type": "message", "role": "user", "content": "next"}
            ]
        });

        DeepSeekChannel.normalize_request(&mut request);

        assert_eq!(
            request["input"],
            json!([
                {"type": "message", "role": "assistant", "content": "ok"},
                {"type": "function_call", "call_id": "call_a", "name": "wait", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_a", "output": "a"},
                {"type": "function_call", "call_id": "call_b", "name": "wait", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_b", "output": "b"},
                {"type": "message", "role": "user", "content": "next"}
            ])
        );
    }

    #[test]
    fn serializes_parallel_custom_tool_calls_without_converting_to_function() {
        let mut request = json!({
            "input": [
                {"type": "message", "role": "user", "content": "go"},
                {
                    "type": "custom_tool_call",
                    "call_id": "call_a",
                    "id": "ctc_a",
                    "name": "exec",
                    "input": "text('a')",
                    "status": "completed"
                },
                {
                    "type": "custom_tool_call",
                    "call_id": "call_b",
                    "id": "ctc_b",
                    "name": "exec",
                    "input": "text('b')",
                    "status": "completed"
                },
                {
                    "type": "custom_tool_call_output",
                    "call_id": "call_a",
                    "id": "ctco_a",
                    "output": [{"type": "input_text", "text": "a1"}]
                },
                {
                    "type": "custom_tool_call_output",
                    "call_id": "call_b",
                    "id": "ctco_b",
                    "output": "b-ok"
                },
                {"type": "message", "role": "user", "content": "next"}
            ]
        });

        DeepSeekChannel.normalize_request(&mut request);

        assert_eq!(
            request["input"],
            json!([
                {"type": "message", "role": "user", "content": "go"},
                {
                    "type": "custom_tool_call",
                    "call_id": "call_a",
                    "id": "ctc_a",
                    "name": "exec",
                    "input": "text('a')",
                    "status": "completed"
                },
                {
                    "type": "custom_tool_call_output",
                    "call_id": "call_a",
                    "id": "ctco_a",
                    "output": [{"type": "input_text", "text": "a1"}]
                },
                {
                    "type": "custom_tool_call",
                    "call_id": "call_b",
                    "id": "ctc_b",
                    "name": "exec",
                    "input": "text('b')",
                    "status": "completed"
                },
                {
                    "type": "custom_tool_call_output",
                    "call_id": "call_b",
                    "id": "ctco_b",
                    "output": "b-ok"
                },
                {"type": "message", "role": "user", "content": "next"}
            ])
        );
    }

    #[test]
    fn promotes_additional_tools_keeping_custom_type() {
        let mut request = json!({
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
                                    "format": {"type": "grammar", "syntax": "lark", "definition": "start: SOURCE\n"}
                                },
                                {
                                    "type": "function",
                                    "name": "wait",
                                    "description": "Wait",
                                    "parameters": {"type": "object", "properties": {}}
                                }
                            ]
                        }
                    ]
                },
                {"role": "user", "content": "hi"}
            ]
        });

        DeepSeekChannel.normalize_request(&mut request);

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
        assert_eq!(request["tools"][1]["type"], "function");
        assert_eq!(request["tools"][1]["name"], "wait");
        assert_eq!(request["input"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn serializes_parallel_function_calls_when_outputs_are_out_of_order() {
        let mut request = json!({
            "input": [
                {"type": "function_call", "call_id": "call_a", "name": "wait", "arguments": "{}"},
                {"type": "function_call", "call_id": "call_b", "name": "wait", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_b", "output": "b"},
                {"type": "function_call_output", "call_id": "call_a", "output": "a"}
            ]
        });

        DeepSeekChannel.normalize_request(&mut request);

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
    fn leaves_already_serial_function_calls_unchanged() {
        let mut request = json!({
            "input": [
                {"type": "function_call", "call_id": "call_a", "name": "wait", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_a", "output": "a"},
                {"type": "function_call", "call_id": "call_b", "name": "wait", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_b", "output": "b"}
            ]
        });
        let original = request.clone();

        DeepSeekChannel.normalize_request(&mut request);

        assert_eq!(request, original);
    }

    #[test]
    fn leaves_incomplete_parallel_function_calls_unchanged() {
        let mut request = json!({
            "input": [
                {"type": "function_call", "call_id": "call_a", "name": "wait", "arguments": "{}"},
                {"type": "function_call", "call_id": "call_b", "name": "wait", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_a", "output": "a"},
                {"type": "message", "role": "user", "content": "still running"}
            ]
        });
        let original = request.clone();

        DeepSeekChannel.normalize_request(&mut request);

        assert_eq!(request, original);
    }

    #[test]
    fn downgrades_json_schema_text_format_to_json_object() {
        let mut request = json!({
            "text": {
                "verbosity": "low",
                "format": {
                    "type": "json_schema",
                    "name": "codex_output_schema",
                    "strict": true,
                    "schema": {
                        "type": "object",
                        "properties": {"raw_memory": {"type": "string"}},
                        "required": ["raw_memory"],
                        "additionalProperties": false
                    }
                }
            }
        });

        DeepSeekChannel.normalize_request(&mut request);

        assert_eq!(request["text"]["verbosity"], "low");
        assert_eq!(request["text"]["format"], json!({ "type": "json_object" }));
    }

    #[test]
    fn leaves_json_object_text_format_unchanged() {
        let mut request = json!({
            "text": { "format": { "type": "json_object" } }
        });
        let original = request.clone();

        DeepSeekChannel.normalize_request(&mut request);

        assert_eq!(request, original);
    }
}
