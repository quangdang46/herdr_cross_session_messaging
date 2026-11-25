//! Core domain logic: project scope resolution and peer registry.
//!
//! Scope = canonical launch cwd of a pane, hashed to a scope key. All
//! messaging data lives in per-scope SQLite pools so agents in different
//! project folders can never see or message each other.
//!
//! - [`scope`]: canonicalization, scope-key derivation, and resolution from
//!   `AgentInfo`/`PaneInfo`/`PaneProcessInfo` records.
//! - [`pool`]: [`PoolContext`]/[`open_pool`] — canonicalize → scope_key →
//!   `ScopeIndex::register` → `PoolStore::open`.
//! - [`identity`]: `HERDR_*` env parsing and [`resolve_self`] for `whoami`.

pub mod error;
pub mod identity;
pub mod pool;
pub mod scope;

pub use error::{CoreError, Result};
pub use identity::{
    resolve_self, self_env_from, SelfEnv, SelfIdentity, ENV_PANE_ID, ENV_TAB_ID, ENV_WORKSPACE_ID,
};
pub use pool::{open_pool, PoolContext};
pub use scope::{
    canonicalize_path, resolve_scope, resolve_scope_for_agent, resolve_scope_for_pane, scope_key,
    ResolvedScope, ScopeKey,
};
