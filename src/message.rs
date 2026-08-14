use crate::model::{ModelRoute, restore_public_model, rewrite_request_model};
use serde_json::Value;

/// Client-facing Messages API request prep before forwarding upstream.
pub fn normalize_message_request_for_upstream(route: &ModelRoute, body: &mut Value) {
    rewrite_request_model(route, body);
}

/// Restore public model names in upstream Messages API responses.
pub fn normalize_message_response_for_client(route: &ModelRoute, body: &mut Value) {
    restore_public_model(route, body);
    if let Some(message) = body.get_mut("message") {
        restore_public_model(route, message);
    }
}

#[derive(Default)]
pub struct SseMessageRestorer {
    pending: Vec<u8>,
}

impl SseMessageRestorer {
    pub fn push(&mut self, chunk: &[u8], route: &ModelRoute) -> Vec<Vec<u8>> {
        self.pending.extend_from_slice(chunk);
        let mut events = Vec::new();

        while let Some(end) = find_sse_event_end(&self.pending) {
            let event = self.pending.drain(..end).collect::<Vec<_>>();
            if let Some(rewritten) = normalize_message_sse_event(event, route) {
                events.push(rewritten);
            }
        }

        events
    }

    pub fn finish(mut self, route: &ModelRoute) -> Option<Vec<u8>> {
        if self.pending.is_empty() {
            return None;
        }
        normalize_message_sse_event(std::mem::take(&mut self.pending), route)
    }
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

fn normalize_message_sse_event(event: Vec<u8>, route: &ModelRoute) -> Option<Vec<u8>> {
    let Ok(text) = std::str::from_utf8(&event) else {
        return Some(event);
    };
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut rewritten = Vec::new();

    for line in text.split_inclusive(newline) {
        let Some(data) = line.strip_prefix("data: ") else {
            rewritten.push(line.to_string());
            continue;
        };
        let data = data.trim_end_matches(['\r', '\n']);
        if data == "[DONE]" {
            rewritten.push(line.to_string());
            continue;
        }
        let Ok(mut body) = serde_json::from_str::<Value>(data) else {
            rewritten.push(line.to_string());
            continue;
        };

        normalize_message_response_for_client(route, &mut body);
        rewritten.push(format!("data: {}{newline}", body));
    }

    Some(rewritten.concat().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::ChannelKind;
    use serde_json::json;

    fn route() -> ModelRoute {
        ModelRoute {
            origin_model: "upstream-model".to_string(),
            public_model: "public-model".to_string(),
            provider_name: "provider".to_string(),
            upstream_base_url: "https://example.com/v1".to_string(),
            api_key: "secret".to_string(),
            channel: ChannelKind::Standard,
            supports_compact: false,
        }
    }

    #[test]
    fn rewrites_request_model_for_messages() {
        let mut request = json!({
            "model": "public-model",
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": 1024
        });

        normalize_message_request_for_upstream(&route(), &mut request);

        assert_eq!(request["model"], "upstream-model");
    }

    #[test]
    fn restores_public_model_in_message_response() {
        let mut response = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "upstream-model",
            "content": [{"type": "text", "text": "hi"}]
        });

        normalize_message_response_for_client(&route(), &mut response);

        assert_eq!(response["model"], "public-model");
    }

    #[test]
    fn restores_public_model_in_message_sse_events() {
        let route = route();
        let stream = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"upstream-model\",\"content\":[]}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n"
        );
        let mut restorer = SseMessageRestorer::default();
        let events = restorer.push(stream.as_bytes(), &route);
        let text = String::from_utf8(events.concat()).unwrap();

        assert!(text.contains("public-model"));
        assert!(!text.contains("upstream-model"));
        assert!(text.contains("content_block_delta"));
    }
}
