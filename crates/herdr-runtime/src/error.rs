//! Runtime error type — backend-agnostic transport/API failures.
//!
//! `SendOutcome` covers *delivery* failures; `RuntimeError` covers
//! everything else (transport, protocol, waits, reads). Delivery code maps
//! `RuntimeError` through its existing retry classification.

use herdr_protocol::HerdrError;

/// Errors from any [`crate::AgentRuntime`] backend.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// Socket/IO failure talking to the backend.
    #[error("transport: {0}")]
    Transport(#[from] std::io::Error),
    /// Connection closed before a response arrived.
    #[error("connection closed")]
    Closed,
    /// Request timed out.
    #[error("timeout")]
    Timeout,
    /// Response did not match the request id or failed to decode.
    #[error("protocol: {0}")]
    Protocol(String),
    /// Backend returned an error envelope.
    #[error("backend api {code}: {message}")]
    Api { code: String, message: String },
    /// The handle does not resolve to a live record.
    #[error("not found: {handle}")]
    NotFound { handle: String },
    /// The backend cannot perform this operation at all.
    #[error("unsupported: {0}")]
    Unsupported(String),
}

impl RuntimeError {
    /// True when retrying the same call may succeed (transport-class and
    /// explicitly transient API codes).
    pub fn is_retryable(&self) -> bool {
        match self {
            RuntimeError::Closed | RuntimeError::Timeout | RuntimeError::Transport(_) => true,
            RuntimeError::Api { code, .. } => matches!(
                code.as_str(),
                "timeout" | "server_unavailable" | "agent_not_ready" | "agent_prompt_stalled"
            ),
            _ => false,
        }
    }
}

impl From<crate::hc::error::HcError> for RuntimeError {
    fn from(err: crate::hc::error::HcError) -> Self {
        use crate::hc::error::HcError;
        match err {
            HcError::Transport(e) => RuntimeError::Transport(e),
            HcError::Closed => RuntimeError::Closed,
            HcError::Timeout { .. } => RuntimeError::Timeout,
            HcError::Protocol(msg) => RuntimeError::Protocol(msg),
            // Prefer the machine-readable `data.kind` as the code so
            // callers/tests can match on "blocked"/"not_found"/etc.
            HcError::Api {
                code,
                message,
                kind,
            } => RuntimeError::Api {
                code: kind.unwrap_or_else(|| code.to_string()),
                message,
            },
        }
    }
}

impl From<HerdrError> for RuntimeError {
    fn from(err: HerdrError) -> Self {
        match err {
            HerdrError::Transport(e) => RuntimeError::Transport(e),
            HerdrError::Closed => RuntimeError::Closed,
            HerdrError::Timeout => RuntimeError::Timeout,
            HerdrError::Protocol(msg) => RuntimeError::Protocol(msg),
            HerdrError::Api { code, message } => RuntimeError::Api { code, message },
            HerdrError::Unsupported(msg) => RuntimeError::Unsupported(msg),
        }
    }
}
