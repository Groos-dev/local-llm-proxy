use crate::channel::ChannelKind;
use crate::model::ModelRoute;
use serde_json::Value;

/// Channel-aware compact fallback when upstream `/responses/compact` fails.
/// DeepSeek (Ada) has no compact endpoint; Standard channels pass the failure through.
pub fn compact_fallback(route: ModelRoute, request: &Value) -> Option<Value> {
    match route.channel {
        ChannelKind::DeepSeek => Some(build_local_compact_response(route, request)),
        ChannelKind::Standard => None,
    }
}

/// Build a Codex-compatible compact response when upstream `/responses/compact` is unavailable.
/// Keeps user/developer messages and appends one `compaction` item.
pub fn build_local_compact_response(route: ModelRoute, request: &Value) -> Value {
    let input = request
        .get("input")
        .and_then(|input| input.as_array())
        .cloned()
        .unwrap_or_default();

    let mut output = Vec::new();
    let mut omitted = 0usize;
    for item in input {
        if is_retained_compact_message(&item) {
            output.push(item);
        } else {
            omitted += 1;
        }
    }

    let mut compaction = serde_json::json!({
        "type": "compaction",
        "encrypted_content": format!(
            "local-llm-proxy compacted {omitted} items for {}",
            route.origin_model
        ),
    });
    if let Some(turn_id) = request.pointer("/client_metadata/turn_id") {
        compaction["internal_chat_message_metadata_passthrough"] =
            serde_json::json!({ "turn_id": turn_id.clone() });
    }
    output.push(compaction);

    serde_json::json!({
        "model": route.public_model,
        "output": output,
    })
}

fn is_retained_compact_message(item: &Value) -> bool {
    let role = item.get("role").and_then(|role| role.as_str());
    let is_user_or_developer = matches!(role, Some("user" | "developer"));
    if !is_user_or_developer {
        return false;
    }
    match item.get("type").and_then(|value| value.as_str()) {
        None | Some("message") => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::route_for_public_model;
    use serde_json::json;

    #[test]
    fn builds_local_compact_keeping_user_developer_and_appending_compaction() {
        let route = route_for_public_model("gpt-5.6-terra").unwrap();
        let request = json!({
            "model": "DeepSeek-V4-Pro-discount",
            "client_metadata": {"turn_id": "turn-123"},
            "input": [
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "sys"}]},
                {"type": "function_call", "name": "exec", "call_id": "c1", "arguments": "{}"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "hi"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "keep me"}]},
                {"type": "custom_tool_call_output", "call_id": "c1", "output": "out"}
            ]
        });

        let body = build_local_compact_response(route, &request);
        let output = body["output"].as_array().unwrap();

        assert_eq!(output.len(), 3);
        assert_eq!(output[0]["role"], "developer");
        assert_eq!(output[1]["role"], "user");
        assert_eq!(output[1]["content"][0]["text"], "keep me");
        assert_eq!(output[2]["type"], "compaction");
        assert!(
            output[2]["encrypted_content"]
                .as_str()
                .unwrap()
                .contains("DeepSeek-V4-Pro-discount")
        );
        assert_eq!(
            output[2]["internal_chat_message_metadata_passthrough"]["turn_id"],
            "turn-123"
        );
        assert_eq!(body["model"], "gpt-5.6-terra");
    }

    #[test]
    fn local_compact_keeps_role_only_messages_without_type() {
        let route = route_for_public_model("gpt-5.6-luna").unwrap();
        let request = json!({
            "input": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "ignored"}
            ]
        });

        let body = build_local_compact_response(route, &request);
        let output = body["output"].as_array().unwrap();
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["role"], "user");
        assert_eq!(output[1]["type"], "compaction");
    }

    #[test]
    fn deepseek_channel_provides_compact_fallback() {
        let route = route_for_public_model("gpt-5.6-terra").unwrap();
        let request = json!({"input": [{"role": "user", "content": "hi"}]});
        let body = compact_fallback(route, &request).unwrap();
        assert_eq!(body["model"], "gpt-5.6-terra");
        assert_eq!(body["output"].as_array().unwrap().last().unwrap()["type"], "compaction");
    }

    #[test]
    fn standard_channel_has_no_compact_fallback() {
        let route = route_for_public_model("gpt-5.6-sol").unwrap();
        let request = json!({"input": [{"role": "user", "content": "hi"}]});
        assert!(compact_fallback(route, &request).is_none());
    }
}
