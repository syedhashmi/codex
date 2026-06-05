use thiserror::Error;

pub type Result<T> = std::result::Result<T, DoSessionsError>;

#[derive(Debug, Error)]
pub enum DoSessionsError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("missing DO IAM token: {0}")]
    MissingToken(String),

    #[error("http transport error: {0}")]
    Transport(String),

    #[error("harness-api returned {status}: {body}")]
    Api { status: u16, body: String },

    #[error("failed to decode response: {0}")]
    Decode(String),

    #[error("event stream error: {0}")]
    Stream(String),
}
