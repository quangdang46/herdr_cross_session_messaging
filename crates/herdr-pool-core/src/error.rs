//! Typed error enums for each crate.

/// Errors from the core crate.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// Store failure.
    #[error("store: {0}")]
    Store(#[from] herdr_pool_store::StoreError),
    /// Protocol failure.
    #[error("protocol: {0}")]
    Protocol(#[from] herdr_protocol::HerdrError),
    /// Backend runtime failure (any [`herdr_runtime::AgentRuntime`]).
    #[error("runtime: {0}")]
    Runtime(#[from] herdr_runtime::RuntimeError),
    /// Required `HERDR_*` env var is absent or empty.
    #[error("missing env var: {0}")]
    MissingEnv(String),
    /// A `HERDR_*` env var is present but malformed.
    #[error("invalid env var: {0}")]
    InvalidEnv(String),
    /// Could not determine a project scope for the pane/agent.
    #[error("scope unresolved: {0}")]
    ScopeUnresolved(String),
    /// Peer lookup failed.
    #[error("peer not found: {0}")]
    PeerNotFound(String),
    /// Peer lookup was ambiguous.
    #[error("peer ambiguous: {0}")]
    PeerAmbiguous(String),
    /// Target belongs to a different project scope.
    #[error("cross-scope denied: {0}")]
    CrossScopeDenied(String),
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, CoreError>;
