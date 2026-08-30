use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("forward failed: {0}")]
    ForwardFailed(String),
    #[error("config error: {0}")]
    ConfigError(String),
    #[error("transform error: {0}")]
    TransformError(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("timeout: {0}")]
    Timeout(String),
    #[error("auth error: {0}")]
    AuthError(String),
    #[error("internal error: {0}")]
    Internal(String),
    #[error("upstream error (status {status}): {body:?}")]
    UpstreamError { status: u16, body: Option<String> },
}
