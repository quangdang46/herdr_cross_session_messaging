//! Typed errors for the hc spike client.

#[derive(Debug, thiserror::Error)]
pub enum HcError {
    /// Failed to spawn `hc connect` or do process IO.
    #[error("transport: {0}")]
    Transport(#[from] std::io::Error),
    /// `hc connect` exited or closed stdout before responding.
    #[error("connection closed")]
    Closed,
    /// No response within the request timeout.
    #[error("timeout waiting for response to {method}")]
    Timeout { method: String },
    /// Malformed JSON-RPC frame from the core.
    #[error("protocol: {0}")]
    Protocol(String),
    /// JSON-RPC error envelope returned by the core.
    #[error("hc api error {code}: {message}")]
    Api { code: i64, message: String },
}

pub type Result<T> = std::result::Result<T, HcError>;
