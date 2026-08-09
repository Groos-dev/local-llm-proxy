use crate::channel::ChannelKind;
use serde_json::Value;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRoute {
    pub origin_model: String,
    pub public_model: String,
    pub provider_name: String,
    pub upstream_base_url: String,
    pub api_key: String,
    pub channel: ChannelKind,
    pub supports_compact: bool,
}

pub fn rewrite_request_model(route: &ModelRoute, body: &mut Value) {
    body["model"] = Value::String(route.origin_model.clone());
}

pub fn restore_public_model(route: &ModelRoute, body: &mut Value) {
    if let Some(response) = body.get_mut("response") {
        restore_response_model(route, response);
    } else {
        restore_response_model(route, body);
    }
}

pub(crate) fn restore_response_model(route: &ModelRoute, response: &mut Value) {
    if response.get("model").is_some() {
        response["model"] = Value::String(route.public_model.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn route() -> ModelRoute {
        ModelRoute {
            origin_model: "DeepSeek-V4-Pro-discount".to_string(),
            public_model: "gpt-5.6-terra".to_string(),
            provider_name: "ada".to_string(),
            upstream_base_url: "https://example.com/v1".to_string(),
            api_key: "secret".to_string(),
            channel: ChannelKind::DeepSeek,
            supports_compact: false,
        }
    }

    #[test]
    fn rewrites_request_model_without_touching_nested_values() {
        let mut request = json!({
            "model": "gpt-5.6-terra",
            "input": [{"role": "user", "content": "hello"}],
            "metadata": {"model": "gpt-5.6-terra"}
        });

        let route = route();
        rewrite_request_model(&route, &mut request);

        assert_eq!(route.origin_model, "DeepSeek-V4-Pro-discount");
        assert_eq!(request["model"], "DeepSeek-V4-Pro-discount");
        assert_eq!(request["metadata"]["model"], "gpt-5.6-terra");
    }

    #[test]
    fn restores_only_the_response_model_field() {
        let route = route();
        let mut event = json!({
            "type": "response.completed",
            "response": {
                "model": "DeepSeek-V4-Flash-0731",
                "output": [{"arguments": "{\"model\":\"DeepSeek-V4-Flash-0731\"}"}]
            }
        });

        restore_public_model(&route, &mut event);

        assert_eq!(event["response"]["model"], "gpt-5.6-terra");
        assert_eq!(
            event["response"]["output"][0]["arguments"],
            "{\"model\":\"DeepSeek-V4-Flash-0731\"}"
        );
    }
}
