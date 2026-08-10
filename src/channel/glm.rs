use super::UpstreamChannel;
use super::tool_compat::{
    promote_additional_tools, rewrite_exec_tool_description, serialize_parallel_tool_calls,
    normalize_exec_tool_calls, strip_tool_call_reasoning_content,
};
use serde_json::Value;

pub struct GlmChannel;

impl UpstreamChannel for GlmChannel {
    fn normalize_request(&self, body: &mut Value) {
        // Ada GLM rejects non-top-level tools (additional_tools → 400).
        promote_additional_tools(body);
        rewrite_exec_tool_description(body);
        // Ada GLM rejects parallel tool-call batches; serialize to call/output pairs.
        serialize_parallel_tool_calls(body);
    }

    fn normalize_response(&self, body: &mut Value) {
        normalize_exec_tool_calls(body);
        strip_tool_call_reasoning_content(body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn promotes_additional_tools_and_serializes_parallel_calls() {
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
                                    "type": "function",
                                    "name": "wait",
                                    "description": "Wait",
                                    "parameters": {"type": "object", "properties": {}}
                                }
                            ]
                        }
                    ]
                },
                {"type": "function_call", "call_id": "call_a", "name": "wait", "arguments": "{}"},
                {"type": "function_call", "call_id": "call_b", "name": "wait", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_b", "output": "b"},
                {"type": "function_call_output", "call_id": "call_a", "output": "a"}
            ]
        });

        GlmChannel.normalize_request(&mut request);

        assert_eq!(request["tools"][0]["name"], "wait");
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
    fn does_not_force_reasoning_or_strip_store() {
        let mut request = json!({
            "store": true,
            "tools": [{"type": "function", "name": "wait"}],
            "tool_choice": "required",
            "reasoning": {"effort": "high"},
            "include": ["reasoning.encrypted_content"],
            "text": {
                "format": {
                    "type": "json_schema",
                    "name": "x",
                    "strict": true,
                    "schema": {"type": "object"}
                }
            },
            "input": [{"role": "user", "content": "hi"}]
        });

        GlmChannel.normalize_request(&mut request);

        assert_eq!(request["store"], true);
        assert_eq!(request["reasoning"]["effort"], "high");
        assert_eq!(request["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(request["text"]["format"]["type"], "json_schema");
    }

    #[test]
    fn strips_reasoning_content_from_tool_calls() {
        let mut body = json!({
            "output": [
                {
                    "type": "function_call",
                    "name": "wait",
                    "arguments": "{\"ms\":1}",
                    "reasoning_content": "think"
                },
                {
                    "type": "custom_tool_call",
                    "name": "exec",
                    "input": "text('hi')",
                    "reasoning_content": "run"
                },
                {
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": "keep"}]
                }
            ]
        });

        GlmChannel.normalize_response(&mut body);

        assert!(body["output"][0].get("reasoning_content").is_none());
        assert!(body["output"][1].get("reasoning_content").is_none());
        assert_eq!(body["output"][2]["summary"][0]["text"], "keep");
    }
}
