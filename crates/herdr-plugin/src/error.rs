//! Typed error enums for each crate.

/// Errors from the plugin crate.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// TOML parse/serialize failure.
    #[error("manifest: {0}")]
    Manifest(String),
    /// IO failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Skill install conflict (user-owned directory).
    #[error("skill conflict: {0}")]
    SkillConflict(String),
    /// Required Herdr/plugin env var missing.
    #[error("missing env: {0}")]
    MissingEnv(&'static str),
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, PluginError>;
