//! Core domain logic: project scope resolution and peer registry.
//!
//! Scope = canonical launch cwd of a pane, hashed to a scope key. All
//! messaging data lives in per-scope SQLite pools so agents in different
//! project folders can never see or message each other.

pub mod error;
pub mod scope;

pub use error::{CoreError, Result};
pub use scope::{resolve_scope, ScopeKey};
