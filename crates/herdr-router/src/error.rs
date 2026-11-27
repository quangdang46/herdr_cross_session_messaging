//! Typed error enums for each crate.

/// Errors from the router crate.
#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    /// Protocol failure.
    #[error("protocol: {0}")]
    Protocol(#[from] herdr_protocol::HerdrError),
    /// Store failure.
    #[error("store: {0}")]
    Store(#[from] herdr_pool_store::StoreError),
    /// Core failure.
    #[error("core: {0}")]
    Core(#[from] herdr_pool_core::CoreError),
    /// Plugin failure.
    #[error("plugin: {0}")]
    Plugin(#[from] herdr_plugin::PluginError),
    /// Runtime failure (agent runtime abstraction).
    #[error("runtime: {0}")]
    Runtime(#[from] herdr_runtime::RuntimeError),
    /// IO failure (state dir, pidfile, log file).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Daemon lifecycle failure.
    #[error("daemon: {0}")]
    Daemon(String),
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, RouterError>;
