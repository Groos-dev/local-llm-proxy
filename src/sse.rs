use crate::model::ModelRoute;
use crate::response::normalize_response_for_client;

#[derive(Default)]
pub struct SseModelRestorer {
    pending: Vec<u8>,
}

impl SseModelRestorer {
    pub fn push(&mut self, chunk: &[u8], route: ModelRoute) -> Vec<Vec<u8>> {
        self.pending.extend_from_slice(chunk);
        let mut events = Vec::new();

        while let Some(end) = find_sse_event_end(&self.pending) {
            let event = self.pending.drain(..end).collect::<Vec<_>>();
            events.push(normalize_sse_event(event, route));
        }

        events
    }

    pub fn finish(self, route: ModelRoute) -> Option<Vec<u8>> {
        (!self.pending.is_empty()).then(|| normalize_sse_event(self.pending, route))
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

fn normalize_sse_event(event: Vec<u8>, route: ModelRoute) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(&event) else {
        return event;
    };
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut rewritten = Vec::new();

    for line in text.split_inclusive(newline) {
        let Some(data) = line.strip_prefix("data: ") else {
            rewritten.push(line.to_string());
            continue;
        };
        let data = data.trim_end_matches(['\r', '\n']);
        let Ok(mut body) = serde_json::from_str(data) else {
            rewritten.push(line.to_string());
            continue;
        };
        normalize_response_for_client(route, &mut body);
        rewritten.push(format!("data: {}{newline}", body));
    }

    rewritten.concat().into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::route_for_public_model;

    #[test]
    fn restores_internal_deployment_ids_in_split_sse_events() {
        let route = route_for_public_model("gpt-5.6-luna").unwrap();
        let event = concat!(
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"ep-07p4u7vn\"}}\n\n"
        );
        let mut restorer = SseModelRestorer::default();
        let mut output = restorer.push(&event.as_bytes()[..42], route);
        assert!(output.is_empty());
        output.extend(restorer.push(&event.as_bytes()[42..], route));

        assert_eq!(output.len(), 1);
        let body = std::str::from_utf8(&output[0]).unwrap();
        assert!(body.contains("gpt-5.6-luna"));
        assert!(!body.contains("ep-07p4u7vn"));
    }

    #[test]
    fn preserves_codex_event_types_and_crlf_framing() {
        let route = route_for_public_model("gpt-5.6-terra").unwrap();
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
        let events = restorer.push(stream.as_bytes(), route);
        assert_eq!(events.len(), 4);
        let text = String::from_utf8(events.concat()).unwrap();
        assert!(text.contains("response.created"));
        assert!(text.contains("response.reasoning_summary_text.delta"));
        assert!(text.contains("response.custom_tool_call_input.delta"));
        assert!(text.contains("response.failed"));
        assert!(text.contains("gpt-5.6-terra"));
        assert!(!text.contains("deployment-id"));
    }

    #[test]
    fn strips_reasoning_content_in_sse_output_item_events() {
        let route = route_for_public_model("gpt-5.6-terra").unwrap();
        let stream = concat!(
            "event: response.output_item.done\r\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"c\",\"call_id\":\"c\",\"name\":\"get_weather\",\"arguments\":\"{}\",\"reasoning_content\":\"think\"}}\r\n\r\n",
            "event: response.completed\r\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"ep-5e9quh5a\",\"output\":[{\"type\":\"reasoning\",\"id\":\"rs\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"think\"}]},{\"type\":\"function_call\",\"id\":\"c\",\"call_id\":\"c\",\"name\":\"get_weather\",\"arguments\":\"{}\",\"reasoning_content\":\"think\"}]}}\r\n\r\n"
        );
        let mut restorer = SseModelRestorer::default();
        let events = restorer.push(stream.as_bytes(), route);
        let text = String::from_utf8(events.concat()).unwrap();

        assert!(text.contains("\"type\":\"reasoning\""));
        assert!(text.contains("gpt-5.6-terra"));
        assert!(!text.contains("reasoning_content"));
        assert!(!text.contains("ep-5e9quh5a"));
    }
}
