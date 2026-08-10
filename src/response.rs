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

    #[test]
    fn glm_channel_strips_reasoning_content_from_tool_calls() {
        let route = route(ChannelKind::Glm);
        let mut body = json!({
            "model": "glm-5.2-discount",
            "output": [{
                "type": "function_call",
                "name": "wait",
                "arguments": "{\"ms\":1}",
                "reasoning_content": "think"
            }]
        });

        normalize_response_for_client(&route, &mut body);

        assert_eq!(body["model"], "public-model");
        assert!(body["output"][0].get("reasoning_content").is_none());
        assert_eq!(body["output"][0]["arguments"], "{\"ms\":1}");
    }

    #[test]
    fn deepseek_rewrites_exec_command_and_json_exec_input_to_js() {
        let route = route(ChannelKind::DeepSeek);
        let mut body = json!({
            "model": "ep-1",
            "output": [
                {
                    "type": "function_call",
                    "id": "c1",
                    "call_id": "c1",
                    "name": "exec_command",
                    "arguments": "{\"cmd\":\"pwd\",\"workdir\":\"/tmp\"}",
                    "reasoning_content": "run"
                },
                {
                    "type": "custom_tool_call",
                    "id": "c2",
                    "call_id": "c2",
                    "name": "exec",
                    "input": "{\"cmd\":\"ls\"}"
                }
            ]
        });

        normalize_response_for_client(&route, &mut body);

        assert_eq!(body["output"][0]["type"], "custom_tool_call");
        assert_eq!(body["output"][0]["name"], "exec");
        assert!(
            body["output"][0]["input"]
                .as_str()
                .unwrap()
                .starts_with("await tools.exec_command(JSON.parse(")
        );
        assert!(body["output"][0].get("arguments").is_none());
        assert!(body["output"][0].get("reasoning_content").is_none());
        assert!(
            body["output"][1]["input"]
                .as_str()
                .unwrap()
                .starts_with("await tools.exec_command(JSON.parse(")
        );
    }

    #[test]
    fn deepseek_rewrites_apply_patch_function_call_to_exec_js() {
        let route = route(ChannelKind::DeepSeek);
        let mut body = json!({
            "model": "ep-1",
            "output": [{
                "type": "function_call",
                "id": "c1",
                "call_id": "c1",
                "name": "apply_patch",
                "arguments": "{\"input\":\"*** Begin Patch\\n*** Update File: a.txt\\n@@\\n-old\\n+new\\n*** End Patch\"}"
            }]
        });

        normalize_response_for_client(&route, &mut body);

        assert_eq!(body["output"][0]["type"], "custom_tool_call");
        assert_eq!(body["output"][0]["name"], "exec");
        let input = body["output"][0]["input"].as_str().unwrap();
        assert!(input.starts_with("await tools.apply_patch("));
        assert!(input.contains("*** Begin Patch"));
        assert!(input.contains("*** Update File: a.txt"));
        assert!(body["output"][0].get("arguments").is_none());
    }

    #[test]
    fn glm_rewrites_apply_patch_function_call_to_exec_js() {
        let route = route(ChannelKind::Glm);
        let mut body = json!({
            "model": "glm",
            "output": [{
                "type": "function_call",
                "id": "c1",
                "call_id": "c1",
                "name": "apply_patch",
                "arguments": "{\"input\":\"*** Begin Patch\\n*** Update File: b.txt\\n@@\\n-a\\n+b\\n*** End Patch\"}"
            }]
        });

        normalize_response_for_client(&route, &mut body);

        assert_eq!(body["output"][0]["type"], "custom_tool_call");
        assert_eq!(body["output"][0]["name"], "exec");
        assert!(
            body["output"][0]["input"]
                .as_str()
                .unwrap()
                .starts_with("await tools.apply_patch(")
        );
    }
}
