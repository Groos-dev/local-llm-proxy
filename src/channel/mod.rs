mod deepseek;
mod standard;

use serde_json::Value;

/// Upstream-specific Responses adaptations.
/// Standard OpenAI-compatible Responses channels only need model rename (handled elsewhere).
pub trait UpstreamChannel {
    fn normalize_request(&self, _body: &mut Value) {}
    fn normalize_response(&self, _body: &mut Value) {}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelKind {
    /// Standard Responses API: no request/response rewriting beyond model mapping.
    Standard,
    /// DeepSeek via Ada: tool/thinking quirks and non-standard fields.
    DeepSeek,
}

impl ChannelKind {
    pub fn normalize_request(self, body: &mut Value) {
        match self {
            Self::Standard => standard::StandardChannel.normalize_request(body),
            Self::DeepSeek => deepseek::DeepSeekChannel.normalize_request(body),
        }
    }

    pub fn normalize_response(self, body: &mut Value) {
        match self {
            Self::Standard => standard::StandardChannel.normalize_response(body),
            Self::DeepSeek => deepseek::DeepSeekChannel.normalize_response(body),
        }
    }
}
