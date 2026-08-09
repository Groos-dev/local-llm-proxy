use crate::model::{ModelRoute, restore_public_model};
use serde_json::Value;

pub fn normalize_response_for_client(route: &ModelRoute, body: &mut Value) {
    restore_public_model(route, body);
    route.channel.normalize_response(body);
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

    #[test]
    fn strips_reasoning_content_from_tool_calls_and_keeps_reasoning_item() {
        let route = route(ChannelKind::DeepSeek);
        let mut body = json!({
            "model": "ep-07p4u7vn",
            "output": [
                {
                    "type": "reasoning",
                    "id": "rs_1",
                    "summary": [{"type": "summary_text", "text": "think"}]
                },
                {
                    "type": "function_call",
                    "id": "call_1",
                    "call_id": "call_1",
                    "name": "get_weather",
                    "arguments": "{\"city\":\"Shanghai\"}",
                    "reasoning_content": "think"
                },
                {
                    "type": "custom_tool_call",
                    "id": "ctc_1",
                    "call_id": "call_2",
                    "name": "shell",
                    "input": "echo hi",
                    "reasoning_content": "run it"
                }
            ]
        });

        normalize_response_for_client(&route, &mut body);

        assert_eq!(body["model"], "public-model");
        assert_eq!(body["output"][0]["type"], "reasoning");
        assert_eq!(body["output"][0]["summary"][0]["text"], "think");
        assert!(body["output"][1].get("reasoning_content").is_none());
        assert!(body["output"][2].get("reasoning_content").is_none());
        assert_eq!(body["output"][1]["arguments"], "{\"city\":\"Shanghai\"}");
        assert_eq!(body["output"][2]["input"], "echo hi");
    }

    #[test]
    fn standard_channel_keeps_reasoning_content_and_only_restores_model() {
        let route = route(ChannelKind::Standard);
        let mut body = json!({
            "model": "glm-5.2-discount",
            "output": [{
                "type": "function_call",
                "name": "get_weather",
                "arguments": "{}",
                "reasoning_content": "keep me"
            }]
        });

        normalize_response_for_client(&route, &mut body);

        assert_eq!(body["model"], "public-model");
        assert_eq!(body["output"][0]["reasoning_content"], "keep me");
    }
}
