use crate::ChannelKind;
use crate::channel::tool_compat::{NestedFnRewrite, nested_fn_rewrite_for_name, wrap_nested_fn_js};
use crate::model::ModelRoute;
use crate::response::normalize_response_for_client;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Default)]
pub struct SseModelRestorer {
    pending: Vec<u8>,
    /// DeepSeek/GLM: item ids from top-level `function_call` we rewrite into `exec`.
    nested_fn_ids: HashMap<String, NestedFnRewrite>,
    /// Buffered JSON argument fragments; flushed as wrapped JS on arguments.done.
    nested_fn_arg_bufs: HashMap<String, String>,
}

impl SseModelRestorer {
    pub fn push(&mut self, chunk: &[u8], route: &ModelRoute) -> Vec<Vec<u8>> {
        self.pending.extend_from_slice(chunk);
        let mut events = Vec::new();

        while let Some(end) = find_sse_event_end(&self.pending) {
            let event = self.pending.drain(..end).collect::<Vec<_>>();
            if let Some(rewritten) = normalize_sse_event(event, route, self) {
                events.push(rewritten);
            }
        }

        events
    }

    pub fn finish(mut self, route: &ModelRoute) -> Option<Vec<u8>> {
        if self.pending.is_empty() {
            return None;
        }
        normalize_sse_event(std::mem::take(&mut self.pending), route, &mut self)
    }
}

fn rewrites_nested_fn_calls(channel: ChannelKind) -> bool {
    matches!(channel, ChannelKind::DeepSeek | ChannelKind::Glm)
}

fn find_sse_event_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
        .or_else(|| {
            bytes
                .windows(2)
                .position(|window| window == b"\n\n")
                .map(|index| index + 2)
        })
}

fn normalize_sse_event(
    event: Vec<u8>,
    route: &ModelRoute,
    restorer: &mut SseModelRestorer,
) -> Option<Vec<u8>> {
    let Ok(text) = std::str::from_utf8(&event) else {
        return Some(event);
    };
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut rewritten = Vec::new();
    let mut data_type: Option<String> = None;
    let mut drop_event = false;

    for line in text.split_inclusive(newline) {
        let Some(data) = line.strip_prefix("data: ") else {
            rewritten.push(line.to_string());
            continue;
        };
        let data = data.trim_end_matches(['\r', '\n']);
        let Ok(mut body) = serde_json::from_str::<Value>(data) else {
            rewritten.push(line.to_string());
            continue;
        };

        if rewrites_nested_fn_calls(route.channel) {
            track_nested_fn_item(&body, restorer);
            match nested_fn_sse_action(&mut body, restorer) {
                Some(SseAction::Drop) => {
                    drop_event = true;
                    break;
                }
                Some(SseAction::Replace(replacement)) => body = replacement,
                None => {}
            }
        }

        normalize_response_for_client(route, &mut body);
        data_type = body
            .get("type")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        rewritten.push(format!("data: {}{newline}", body));
    }

    if drop_event {
        return None;
    }

    if let Some(data_type) = data_type.as_deref() {
        for line in &mut rewritten {
            if let Some(rest) = line.strip_prefix("event: ") {
                let old = rest.trim_end_matches(['\r', '\n']);
                if old != data_type
                    && old.starts_with("response.function_call_arguments.")
                    && data_type.starts_with("response.custom_tool_call_input.")
                {
                    *line = format!("event: {data_type}{newline}");
                }
            }
        }
    }

    Some(rewritten.concat().into_bytes())
}

enum SseAction {
    Drop,
    Replace(Value),
}

fn track_nested_fn_item(body: &Value, restorer: &mut SseModelRestorer) {
    if body.get("type").and_then(Value::as_str) != Some("response.output_item.added") {
        return;
    }
    let Some(item) = body.get("item") else {
        return;
    };
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return;
    }
    let Some(name) = item.get("name").and_then(Value::as_str) else {
        return;
    };
    let Some(kind) = nested_fn_rewrite_for_name(name) else {
        return;
    };
    if let Some(id) = item
        .get("id")
        .or_else(|| item.get("call_id"))
        .and_then(Value::as_str)
    {
        restorer.nested_fn_ids.insert(id.to_string(), kind);
    }
}

fn nested_fn_sse_action(body: &mut Value, restorer: &mut SseModelRestorer) -> Option<SseAction> {
    let event_type = body.get("type").and_then(Value::as_str)?;
    let item_id = body.get("item_id").and_then(Value::as_str)?.to_string();
    let kind = *restorer.nested_fn_ids.get(&item_id)?;

    match event_type {
        "response.function_call_arguments.delta" => {
            let delta = body.get("delta").and_then(Value::as_str).unwrap_or("");
            restorer
                .nested_fn_arg_bufs
                .entry(item_id)
                .or_default()
                .push_str(delta);
            Some(SseAction::Drop)
        }
        "response.function_call_arguments.done" => {
            let args = restorer
                .nested_fn_arg_bufs
                .remove(&item_id)
                .or_else(|| {
                    body.get("arguments")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_default();
            let input = wrap_nested_fn_js(kind, &args);
            let mut replacement = body.clone();
            if let Some(object) = replacement.as_object_mut() {
                object.insert(
                    "type".to_string(),
                    Value::String("response.custom_tool_call_input.done".to_string()),
                );
                object.insert("input".to_string(), Value::String(input));
                object.remove("arguments");
            }
            Some(SseAction::Replace(replacement))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChannelKind, ModelRoute};

    fn route() -> ModelRoute {
        ModelRoute {
            origin_model: "upstream-model".to_string(),
            public_model: "public-model".to_string(),
            provider_name: "provider".to_string(),
            upstream_base_url: "https://example.com/v1".to_string(),
            api_key: "secret".to_string(),
            channel: ChannelKind::DeepSeek,
            supports_compact: false,
        }
    }

    #[test]
    fn restores_internal_deployment_ids_in_split_sse_events() {
        let route = route();
        let event = concat!(
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"ep-07p4u7vn\"}}\n\n"
        );
        let mut restorer = SseModelRestorer::default();
        let mut output = restorer.push(&event.as_bytes()[..42], &route);
        assert!(output.is_empty());
        output.extend(restorer.push(&event.as_bytes()[42..], &route));

        assert_eq!(output.len(), 1);
        let body = std::str::from_utf8(&output[0]).unwrap();
        assert!(body.contains("public-model"));
        assert!(!body.contains("ep-07p4u7vn"));
    }

    #[test]
    fn preserves_codex_event_types_and_crlf_framing() {
        let route = route();
        let stream = concat!(
            "event: response.created\r\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\r\n\r\n",
            "event: response.reasoning_summary_text.delta\r\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"thinking\",\"summary_index\":0}\r\n\r\n",
            "event: response.custom_tool_call_input.delta\r\n",
            "data: {\"type\":\"response.custom_tool_call_input.delta\",\"item_id\":\"i\",\"delta\":\"{}\"}\r\n\r\n",
            "event: response.failed\r\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"model\":\"deployment-id\"}}\r\n\r\n"
        );
        let mut restorer = SseModelRestorer::default();
        let events = restorer.push(stream.as_bytes(), &route);
        assert_eq!(events.len(), 4);
        let text = String::from_utf8(events.concat()).unwrap();
        assert!(text.contains("response.created"));
        assert!(text.contains("response.reasoning_summary_text.delta"));
        assert!(text.contains("response.custom_tool_call_input.delta"));
        assert!(text.contains("response.failed"));
        assert!(text.contains("public-model"));
        assert!(!text.contains("deployment-id"));
    }

    #[test]
    fn keeps_reasoning_content_in_sse_output_item_events() {
        let route = route();
        let stream = concat!(
            "event: response.output_item.done\r\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"c\",\"call_id\":\"c\",\"name\":\"get_weather\",\"arguments\":\"{}\",\"reasoning_content\":\"think\"}}\r\n\r\n",
            "event: response.completed\r\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"ep-5e9quh5a\",\"output\":[{\"type\":\"reasoning\",\"id\":\"rs\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"think\"}]},{\"type\":\"function_call\",\"id\":\"c\",\"call_id\":\"c\",\"name\":\"get_weather\",\"arguments\":\"{}\",\"reasoning_content\":\"think\"}]}}\r\n\r\n"
        );
        let mut restorer = SseModelRestorer::default();
        let events = restorer.push(stream.as_bytes(), &route);
        let text = String::from_utf8(events.concat()).unwrap();

        assert!(text.contains("\"type\":\"reasoning\""));
        assert!(text.contains("public-model"));
        assert!(text.contains("reasoning_content"));
        assert!(!text.contains("ep-5e9quh5a"));
    }

    #[test]
    fn rewrites_exec_command_sse_stream_to_custom_exec_js() {
        let route = route();
        let stream = concat!(
            "event: response.output_item.added\r\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"call_1\",\"call_id\":\"call_1\",\"name\":\"exec_command\",\"arguments\":\"\"}}\r\n\r\n",
            "event: response.function_call_arguments.delta\r\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"call_1\",\"delta\":\"{\\\"cmd\\\":\\\"pwd\\\"}\"}\r\n\r\n",
            "event: response.function_call_arguments.done\r\n",
            "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"call_1\",\"arguments\":\"{\\\"cmd\\\":\\\"pwd\\\"}\"}\r\n\r\n",
            "event: response.output_item.done\r\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"call_1\",\"call_id\":\"call_1\",\"name\":\"exec_command\",\"arguments\":\"{\\\"cmd\\\":\\\"pwd\\\"}\"}}\r\n\r\n"
        );
        let mut restorer = SseModelRestorer::default();
        let events = restorer.push(stream.as_bytes(), &route);
        let text = String::from_utf8(events.concat()).unwrap();

        assert!(!text.contains("function_call_arguments.delta"));
        assert!(text.contains("response.custom_tool_call_input.done"));
        assert!(text.contains("await tools.exec_command(JSON.parse("));
        assert!(text.contains("\"type\":\"custom_tool_call\""));
        assert!(text.contains("\"name\":\"exec\""));
        assert!(!text.contains("\"name\":\"exec_command\""));
    }

    #[test]
    fn rewrites_apply_patch_sse_stream_to_custom_exec_js() {
        let route = route();
        let stream = concat!(
            "event: response.output_item.added\r\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"call_p\",\"call_id\":\"call_p\",\"name\":\"apply_patch\",\"arguments\":\"\"}}\r\n\r\n",
            "event: response.function_call_arguments.delta\r\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"call_p\",\"delta\":\"{\\\"input\\\":\\\"*** Begin Patch\\\\n*** Update File: a.txt\\\\n@@\\\\n-old\\\\n+new\\\\n*** End Patch\\\"}\"}\r\n\r\n",
            "event: response.function_call_arguments.done\r\n",
            "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"call_p\",\"arguments\":\"{\\\"input\\\":\\\"*** Begin Patch\\\\n*** Update File: a.txt\\\\n@@\\\\n-old\\\\n+new\\\\n*** End Patch\\\"}\"}\r\n\r\n",
            "event: response.output_item.done\r\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"call_p\",\"call_id\":\"call_p\",\"name\":\"apply_patch\",\"arguments\":\"{\\\"input\\\":\\\"*** Begin Patch\\\\n*** Update File: a.txt\\\\n@@\\\\n-old\\\\n+new\\\\n*** End Patch\\\"}\"}}\r\n\r\n"
        );
        let mut restorer = SseModelRestorer::default();
        let events = restorer.push(stream.as_bytes(), &route);
        let text = String::from_utf8(events.concat()).unwrap();

        assert!(!text.contains("function_call_arguments.delta"));
        assert!(text.contains("response.custom_tool_call_input.done"));
        assert!(text.contains("await tools.apply_patch("));
        assert!(text.contains("*** Begin Patch"));
        assert!(text.contains("\"type\":\"custom_tool_call\""));
        assert!(text.contains("\"name\":\"exec\""));
        assert!(!text.contains("\"name\":\"apply_patch\""));
    }
}
