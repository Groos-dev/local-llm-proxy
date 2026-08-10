use serde_json::Value;
use std::collections::HashMap;

/// Responses Lite puts tools under `input.additional_tools`; promote to top-level `tools`.
pub(crate) fn promote_additional_tools(body: &mut Value) {
    let already_has_tools = has_tools(body);
    let Some(input) = body.get_mut("input").and_then(|input| input.as_array_mut()) else {
        return;
    };

    let mut promoted = Vec::new();
    let mut kept = Vec::new();
    for item in input.drain(..) {
        let is_additional_tools =
            item.get("type").and_then(|value| value.as_str()) == Some("additional_tools");
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

/// Ada rejects parallel tool-call batches; serialize to call/output pairs by `call_id`.
pub(crate) fn serialize_parallel_tool_calls(body: &mut Value) {
    let Some(input) = body.get_mut("input").and_then(|input| input.as_array_mut()) else {
        return;
    };

    let mut rewritten = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if !is_tool_call(&input[index]) {
            rewritten.push(input[index].clone());
            index += 1;
            continue;
        }

        let calls_start = index;
        while index < input.len() && is_tool_call(&input[index]) {
            index += 1;
        }
        let calls_end = index;
        let outputs_start = index;
        while index < input.len() && is_tool_call_output(&input[index]) {
            index += 1;
        }
        let outputs_end = index;

        let call_count = calls_end - calls_start;
        let output_count = outputs_end - outputs_start;
        if call_count <= 1 || call_count != output_count {
            rewritten.extend(input[calls_start..outputs_end].iter().cloned());
            continue;
        }

        let mut outputs_by_id = HashMap::with_capacity(output_count);
        let mut complete = true;
        for output in &input[outputs_start..outputs_end] {
            let Some(id) = call_id(output) else {
                complete = false;
                break;
            };
            if outputs_by_id.insert(id, output.clone()).is_some() {
                complete = false;
                break;
            }
        }
        if !complete {
            rewritten.extend(input[calls_start..outputs_end].iter().cloned());
            continue;
        }

        for call in &input[calls_start..calls_end] {
            let Some(id) = call_id(call) else {
                complete = false;
                break;
            };
            if !outputs_by_id.contains_key(id) {
                complete = false;
                break;
            }
        }
        if !complete || outputs_by_id.len() != call_count {
            rewritten.extend(input[calls_start..outputs_end].iter().cloned());
            continue;
        }

        for call in &input[calls_start..calls_end] {
            let id = call_id(call).expect("call_id checked");
            let output = outputs_by_id.remove(id).expect("output checked");
            rewritten.push(call.clone());
            rewritten.push(output);
        }
    }

    *input = rewritten;
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

fn is_tool_call(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some("function_call" | "custom_tool_call")
    )
}

fn is_tool_call_output(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some("function_call_output" | "custom_tool_call_output")
    )
}

fn call_id(item: &Value) -> Option<&str> {
    item.get("call_id").and_then(Value::as_str)
}

/// Ada may attach `reasoning_content` on tool calls; Codex does not expect it there.
pub(crate) fn strip_tool_call_reasoning_content(body: &mut Value) {
    if let Some(item) = body.get_mut("item") {
        strip_reasoning_content_from_item(item);
    }
    strip_reasoning_content_from_output(body);
    if let Some(response) = body.get_mut("response") {
        strip_reasoning_content_from_output(response);
    }
}

fn strip_reasoning_content_from_output(value: &mut Value) {
    let Some(output) = value
        .get_mut("output")
        .and_then(|output| output.as_array_mut())
    else {
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
