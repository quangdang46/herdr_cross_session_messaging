//! Core domain logic: project scope resolution and peer registry.
//!
//! Scope = canonical launch cwd of a pane (or its git toplevel when
//! `HERDR_POOL_SCOPE_MODE=git`), hashed to a scope key. All
//! messaging data lives in per-scope SQLite pools so agents in different
//! project folders can never see or message each other.
//!
//! - [`scope`]: canonicalization, scope-key derivation, and resolution from
//!   `AgentInfo`/`PaneInfo`/`PaneProcessInfo` records.
//! - [`pool`]: [`PoolContext`]/[`open_pool`] — canonicalize → scope_key →
//!   `ScopeIndex::register` → `PoolStore::open`.
//! - [`identity`]: `HERDR_*` env parsing and [`resolve_self`] for `whoami`.

// No `unwrap()`/`expect()` in library code; inline `#[cfg(test)]` modules
// are exempted per-module below.
#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod error;
pub mod identity;
pub mod pool;
pub mod scope;

pub use error::{CoreError, Result};
pub use identity::{
    resolve_self, self_env_from, SelfEnv, SelfIdentity, ENV_PANE_ID, ENV_PLUGIN_CONTEXT_JSON,
    ENV_TAB_ID, ENV_WORKSPACE_ID,
};
pub use pool::{open_pool, PoolContext};
pub use scope::{
    canonicalize_path, resolve_scope, resolve_scope_for_agent, resolve_scope_for_pane,
    resolve_scope_with_mode, scope_key, scope_mode, ResolvedScope, ScopeKey, ScopeMode,
    SCOPE_MODE_ENV,
};
