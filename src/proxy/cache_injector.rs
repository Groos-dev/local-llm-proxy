//! Anthropic `cache_control` breakpoint injector (Codex → Messages path).

use serde_json::{json, Value};

/// Inject ephemeral cache breakpoints when enabled.
pub fn inject(body: &mut Value, enabled: bool) {
    if !enabled {
        return;
    }

    let existing = count_existing(body);

    if existing > 4 {
        log::warn!(
            "[OPT] cache: existing breakpoint count {existing} exceeds the supported total of 4; preserving caller input"
        );
    }

    let mut budget = 4_usize.saturating_sub(existing);
    if budget == 0 {
        log::info!("[OPT] cache: no-op(existing={existing})");
        return;
    }

    let mut injected = Vec::new();

    if budget > 0 {
        if let Some(tools) = body.get_mut("tools").and_then(|t| t.as_array_mut()) {
            if let Some(last) = tools.last_mut() {
                if last.get("cache_control").is_none() {
                    if let Some(o) = last.as_object_mut() {
                        o.insert("cache_control".to_string(), make_cache_control());
                    }
                    budget -= 1;
                    injected.push("tools");
                }
            }
        }
    }

    if budget > 0 {
        if let Some(text) = body
            .get("system")
            .and_then(|s| s.as_str())
            .map(str::to_string)
        {
            body["system"] = json!([{"type": "text", "text": text}]);
        }

        if let Some(system) = body.get_mut("system").and_then(|s| s.as_array_mut()) {
            if let Some(last) = system.last_mut() {
                if last.get("cache_control").is_none() {
                    if let Some(o) = last.as_object_mut() {
                        o.insert("cache_control".to_string(), make_cache_control());
                    }
                    budget -= 1;
                    injected.push("system");
                }
            }
        }
    }

    if budget > 0 {
        if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
            for message in messages.iter_mut().rev() {
                if inject_message_breakpoint(message) {
                    budget -= 1;
                    injected.push("msgs-latest");
                    break;
                }
            }

            if budget > 0 && messages.len() >= 4 {
                let mut user_count = 0;
                for message in messages.iter_mut().rev() {
                    if message.get("role").and_then(Value::as_str) != Some("user") {
                        continue;
                    }
                    user_count += 1;
                    if user_count == 2 {
                        if inject_message_breakpoint(message) {
                            injected.push("msgs-prior-user");
                        }
                        break;
                    }
                }
            }
        }
    }

    log::info!(
        "[OPT] cache: {}bp({},{},pre={existing})",
        injected.len(),
        injected.join("+"),
        "5m",
    );
}

/// Codex→Anthropic path defaults to on; set `AGENT_PROXY_ANTHROPIC_CACHE=0` to disable.
pub fn anthropic_cache_injection_enabled() -> bool {
    !matches!(
        std::env::var("AGENT_PROXY_ANTHROPIC_CACHE").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

fn inject_message_breakpoint(message: &mut Value) -> bool {
    let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
        return false;
    };
    let Some(block) = content.iter_mut().rev().find(|block| {
        !matches!(
            block.get("type").and_then(Value::as_str),
            Some("thinking" | "redacted_thinking")
        )
    }) else {
        return false;
    };
    if block.get("cache_control").is_some() {
        return false;
    }
    let Some(object) = block.as_object_mut() else {
        return false;
    };
    object.insert("cache_control".to_string(), make_cache_control());
    true
}

fn make_cache_control() -> Value {
    json!({"type": "ephemeral"})
}

fn count_existing(body: &Value) -> usize {
    let mut count = 0;

    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        count += tools
            .iter()
            .filter(|t| t.get("cache_control").is_some())
            .count();
    }

    if let Some(system) = body.get("system").and_then(|s| s.as_array()) {
        count += system
            .iter()
            .filter(|b| b.get("cache_control").is_some())
            .count();
    }

    if let Some(messages) = body.get("messages").and_then(|m| m.as_array()) {
        for msg in messages {
            if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
                count += content
                    .iter()
                    .filter(|b| b.get("cache_control").is_some())
                    .count();
            }
        }
    }

    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn injects_breakpoints_when_enabled() {
        let mut body = json!({
            "model": "test",
            "tools": [{"name": "tool1"}, {"name": "tool2"}],
            "system": [{"type": "text", "text": "sys prompt"}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hi"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "hello"}]}
            ]
        });
        inject(&mut body, true);
        assert!(body["tools"][1].get("cache_control").is_some());
        assert!(body["system"][0].get("cache_control").is_some());
        assert!(body["messages"][1]["content"][0]
            .get("cache_control")
            .is_some());
    }

    #[test]
    fn disabled_is_noop() {
        let mut body = json!({
            "tools": [{"name": "tool1"}],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
        });
        let original = body.clone();
        inject(&mut body, false);
        assert_eq!(body, original);
    }

    #[test]
    fn converts_string_system_to_array() {
        let mut body = json!({
            "system": "You are helpful",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
        });
        inject(&mut body, true);
        assert!(body["system"].is_array());
        assert!(body["system"][0].get("cache_control").is_some());
    }
}
