use crate::channel::ChannelKind;
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelRoute {
    pub origin_model: &'static str,
    pub public_model: &'static str,
    pub channel: ChannelKind,
}

pub const MODEL_ROUTES: [ModelRoute; 3] = [
    ModelRoute {
        origin_model: "DeepSeek-V4-Flash-0731",
        public_model: "gpt-5.6-luna",
        channel: ChannelKind::DeepSeek,
    },
    ModelRoute {
        origin_model: "DeepSeek-V4-Pro-discount",
        public_model: "gpt-5.6-terra",
        channel: ChannelKind::DeepSeek,
    },
    ModelRoute {
        origin_model: "glm-5.2-discount",
        public_model: "gpt-5.6-sol",
        channel: ChannelKind::Standard,
    },
];

pub fn route_for_public_model(public_model: &str) -> Option<ModelRoute> {
    MODEL_ROUTES
        .into_iter()
        .find(|route| route.public_model == public_model)
}

pub fn rewrite_request_model(body: &mut Value) -> Option<ModelRoute> {
    let model = body.get("model")?.as_str()?;
    let route = route_for_public_model(model)?;
    body["model"] = Value::String(route.origin_model.to_string());
    Some(route)
}

pub fn restore_public_model(route: ModelRoute, body: &mut Value) {
    if let Some(response) = body.get_mut("response") {
        restore_response_model(route, response);
    } else {
        restore_response_model(route, body);
    }
}

pub fn public_models_list() -> Value {
    let created = 1_700_000_000i64;
    let data = MODEL_ROUTES
        .into_iter()
        .map(|route| {
            serde_json::json!({
                "id": route.public_model,
                "object": "model",
                "created": created,
                "owned_by": "local-llm-proxy",
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "object": "list",
        "data": data,
    })
}

pub(crate) fn restore_response_model(route: ModelRoute, response: &mut Value) {
    if response.get("model").is_some() {
        response["model"] = Value::String(route.public_model.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn routes_public_models_to_origin() {
        assert_eq!(
            route_for_public_model("gpt-5.6-luna").unwrap().origin_model,
            "DeepSeek-V4-Flash-0731"
        );
        assert_eq!(
            route_for_public_model("gpt-5.6-terra")
                .unwrap()
                .origin_model,
            "DeepSeek-V4-Pro-discount"
        );
        assert_eq!(
            route_for_public_model("gpt-5.6-sol").unwrap().origin_model,
            "glm-5.2-discount"
        );
        assert_eq!(
            route_for_public_model("gpt-5.6-luna").unwrap().channel,
            ChannelKind::DeepSeek
        );
        assert_eq!(
            route_for_public_model("gpt-5.6-sol").unwrap().channel,
            ChannelKind::Standard
        );
    }

    #[test]
    fn does_not_match_another_provider_or_unknown_model() {
        assert!(route_for_public_model("gpt-5.6").is_none());
    }

    #[test]
    fn rewrites_only_the_request_model() {
        let mut request = json!({
            "model": "gpt-5.6-terra",
            "input": [{"role": "user", "content": "hello"}],
            "metadata": {"model": "gpt-5.6-terra"}
        });

        let route = rewrite_request_model(&mut request).unwrap();

        assert_eq!(route.origin_model, "DeepSeek-V4-Pro-discount");
        assert_eq!(request["model"], "DeepSeek-V4-Pro-discount");
        assert_eq!(request["metadata"]["model"], "gpt-5.6-terra");
    }

    #[test]
    fn restores_only_the_response_model_field() {
        let route = route_for_public_model("gpt-5.6-luna").unwrap();
        let mut event = json!({
            "type": "response.completed",
            "response": {
                "model": "DeepSeek-V4-Flash-0731",
                "output": [{"arguments": "{\"model\":\"DeepSeek-V4-Flash-0731\"}"}]
            }
        });

        restore_public_model(route, &mut event);

        assert_eq!(event["response"]["model"], "gpt-5.6-luna");
        assert_eq!(
            event["response"]["output"][0]["arguments"],
            "{\"model\":\"DeepSeek-V4-Flash-0731\"}"
        );
    }

    #[test]
    fn lists_public_models_in_openai_shape() {
        let body = public_models_list();
        assert_eq!(body["object"], "list");
        let ids = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["gpt-5.6-luna", "gpt-5.6-terra", "gpt-5.6-sol"]);
        assert_eq!(body["data"][1]["owned_by"], "local-llm-proxy");
    }
}
