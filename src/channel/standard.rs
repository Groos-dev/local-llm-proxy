use super::UpstreamChannel;

/// Passthrough channel for upstreams that already speak standard Responses.
pub struct StandardChannel;

impl UpstreamChannel for StandardChannel {}
