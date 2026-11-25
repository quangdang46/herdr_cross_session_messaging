//! Typed error enums for each crate.

/// Errors from the protocol crate.
#[derive(Debug, thiserror::Error)]
pub enum HerdrError {
    /// Socket/IO failure.
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
    /// Herdr returned an error envelope.
    #[error("herdr api {code}: {message}")]
    Api { code: String, message: String },
    /// The platform or configuration does not support this operation
    /// (e.g. Windows named pipes are not implemented yet).
    #[error("unsupported: {0}")]
    Unsupported(String),
}

impl HerdrError {
    /// True when Herdr rejected `agent.prompt` because the agent is blocked.
    pub fn is_agent_blocked(&self) -> bool {
        matches!(self, HerdrError::Api { code, .. } if code == "agent_blocked")
    }
    /// True when the agent target could not be resolved.
    pub fn is_agent_not_found(&self) -> bool {
        matches!(self, HerdrError::Api { code, .. } if code == "agent_not_found")
    }
    /// True for timeout-class failures.
    pub fn is_timeout(&self) -> bool {
        matches!(self, HerdrError::Timeout)
            || matches!(self, HerdrError::Api { code, .. } if code == "timeout")
    }
    /// True when a retry may succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            HerdrError::Closed | HerdrError::Timeout | HerdrError::Transport(_)
        ) || matches!(self, HerdrError::Api { code, .. }
                if matches!(code.as_str(),
                    "agent_blocked" | "agent_not_ready" | "agent_prompt_stalled"
                    | "agent_not_running" | "timeout" | "server_unavailable"))
    }
    /// True when the agent target was ambiguous (2+ matches).
    pub fn is_agent_target_ambiguous(&self) -> bool {
        matches!(self, HerdrError::Api { code, .. } if code == "agent_target_ambiguous")
    }
    /// True when the agent is not in a promptable state.
    pub fn is_agent_not_ready(&self) -> bool {
        matches!(self, HerdrError::Api { code, .. } if code == "agent_not_ready")
    }
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, HerdrError>;
