use super::UpstreamChannel;
use serde_json::Value;

pub struct DeepSeekChannel;

impl UpstreamChannel for DeepSeekChannel {
    fn normalize_request(&self, body: &mut Value) {
        // Ada DeepSeek session state is unreliable; Codex also defaults to store=false.
        if body.get("store").is_some() {
            body["store"] = Value::Bool(false);
        }
        strip_unsupported_include(body);
        if !has_tools(body) || !has_forced_tool_choice(body) {
            return;
        }
        match body.get_mut("reasoning") {
            Some(Value::Object(reasoning)) => {
                reasoning.insert(
                    "effort".to_string(),
                    Value::String("none".to_string()),
                );
            }
            _ => {
                body["reasoning"] = serde_json::json!({ "effort": "none" });
            }
        }
    }

    fn normalize_response(&self, body: &mut Value) {
        strip_tool_call_reasoning_content(body);
    }
}

fn strip_unsupported_include(body: &mut Value) {
    let Some(include) = body.get_mut("include").and_then(|include| include.as_array_mut()) else {
        return;
    };
    include.retain(|item| {
        item.as_str()
            .is_none_or(|value| value != "reasoning.encrypted_content")
    });
}

fn has_tools(body: &Value) -> bool {
    body.get("tools")
        .and_then(|tools| tools.as_array())
        .is_some_and(|tools| !tools.is_empty())
}

fn has_forced_tool_choice(body: &Value) -> bool {
    match body.get("tool_choice") {
        Some(Value::String(choice)) => choice == "required",
        Some(Value::Object(_)) => true,
        _ => false,
    }
}

fn strip_tool_call_reasoning_content(body: &mut Value) {
    if let Some(item) = body.get_mut("item") {
        strip_reasoning_content_from_item(item);
    }
    strip_reasoning_content_from_output(body);
    if let Some(response) = body.get_mut("response") {
        strip_reasoning_content_from_output(response);
    }
}

fn strip_reasoning_content_from_output(value: &mut Value) {
    let Some(output) = value.get_mut("output").and_then(|output| output.as_array_mut()) else {
        return;
    };
    for item in output {
        strip_reasoning_content_from_item(item);
    }
}

fn strip_reasoning_content_from_item(item: &mut Value) {
    let is_tool_call = matches!(
        item.get("type").and_then(|value| value.as_str()),
        Some("function_call" | "custom_tool_call")
    );
    if is_tool_call {
        if let Some(object) = item.as_object_mut() {
            object.remove("reasoning_content");
        }
    }
}
