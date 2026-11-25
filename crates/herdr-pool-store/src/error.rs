//! Typed error enums for each crate.

/// Errors from the store crate.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite failure.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Migration failure.
    #[error("migration: {0}")]
    Migration(String),
    /// IO failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Invalid state transition or constraint violation.
    #[error("invalid: {0}")]
    Invalid(String),
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, StoreError>;
