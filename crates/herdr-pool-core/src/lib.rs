//! Core domain logic: project scope resolution and peer registry.
//!
//! Scope = canonical launch cwd of a pane (or its git toplevel when
//! `HERDR_POOL_SCOPE_MODE=git`), hashed to a scope key. All
//! messaging data lives in per-scope SQLite pools so agents in different
//! project folders can never see or message each other.
//!
//! - [`scope`]: canonicalization, scope-key derivation, and resolution from
//!   runtime records ([`herdr_runtime::RuntimeAgent`]) and, for the
//!   herdr-only router path, `AgentInfo`/`PaneInfo`/`PaneProcessInfo`.
//! - [`pool`]: [`PoolContext`]/[`open_pool`] — canonicalize → scope_key →
//!   `ScopeIndex::register` → `PoolStore::open`.
//! - [`identity`]: backend env parsing (`HERDR_*` / `HC_TERMINAL_ID`) and
//!   [`resolve_self`] over [`herdr_runtime::AgentRuntime`] for `whoami`.
//! - [`registry`]: [`AgentRecord`] construction/`upsert_self`/
//!   `refresh_registry` over [`RuntimeAgent`]s — backend-agnostic.

// No `unwrap()`/`expect()` in library code; inline `#[cfg(test)]` modules
// are exempted per-module below.
#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod error;
pub mod identity;
pub mod pool;
pub mod registry;
pub mod scope;

pub use error::{CoreError, Result};
pub use identity::{
    resolve_self, self_env_from, SelfEnv, SelfIdentity, ENV_HC_TERMINAL_ID, ENV_PANE_ID,
    ENV_PLUGIN_CONTEXT_JSON, ENV_TAB_ID, ENV_WORKSPACE_ID,
};
pub use pool::{open_pool, PoolContext};
pub use registry::{record_for_agent, refresh_registry, upsert_self};
pub use scope::{
    canonicalize_path, resolve_scope, resolve_scope_for_agent, resolve_scope_for_pane,
    resolve_scope_for_runtime, resolve_scope_for_runtime_with_mode, resolve_scope_with_mode,
    scope_key, scope_mode, ResolvedScope, ScopeKey, ScopeMode, SCOPE_MODE_ENV,
};
