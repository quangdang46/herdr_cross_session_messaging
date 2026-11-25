//! Open a scope's pool: canonicalize → scope_key → register → PoolStore.
//!
//! [`PoolContext`] is the convenience bundle the CLI and router use at
//! startup: it resolves a project directory to its [`ScopeKey`], records
//! the mapping in the global [`ScopeIndex`] (`<state>/index.db`), and opens
//! the per-scope [`PoolStore`] (`<state>/pools/<scope_key>/pool.db`).
//!
//! Raw filesystem paths never appear in pool directory names — only the
//! 32-hex-char scope key does.

use std::path::{Path, PathBuf};

use herdr_pool_store::{PoolStore, ScopeIndex};

use crate::error::Result;
use crate::scope::{canonicalize_path, scope_key, ResolvedScope, ScopeKey};

/// Everything needed to talk to one project's message pool.
///
/// Cheap to construct per CLI invocation; `PoolStore`/`ScopeIndex` are
/// `Send + Sync` handles over `Mutex<Connection>`.
#[derive(Debug)]
pub struct PoolContext {
    /// Plugin state dir that owns `index.db` and `pools/`.
    pub state_dir: PathBuf,
    /// Canonical project path this scope was derived from.
    pub canonical_path: PathBuf,
    /// Hashed scope key (`pools/<scope_key>/` directory name).
    pub scope_key: ScopeKey,
    /// Global `index.db` handle (scope_key ↔ canonical_path).
    pub index: ScopeIndex,
    /// This scope's `pool.db` handle.
    pub pool: PoolStore,
}

impl PoolContext {
    /// Resolve `project_path` to a scope and open its pool under `state_dir`.
    ///
    /// Steps: [`canonicalize_path`] → [`scope_key`] →
    /// [`ScopeIndex::register`] → [`PoolStore::open`]. Idempotent — safe to
    /// call on every command; re-registering an existing scope is a no-op.
    pub fn open(state_dir: impl AsRef<Path>, project_path: impl AsRef<Path>) -> Result<Self> {
        let canonical_path = canonicalize_path(project_path);
        Self::for_canonical(state_dir, canonical_path)
    }

    /// Same as [`PoolContext::open`] but starting from an already-resolved
    /// [`ResolvedScope`] (e.g. the output of [`resolve_scope`](crate::scope::resolve_scope)).
    pub fn for_resolved(state_dir: impl AsRef<Path>, resolved: ResolvedScope) -> Result<Self> {
        Self::for_canonical(state_dir, resolved.canonical_path)
    }

    /// Core path: canonical dir in, registered+opened pool out.
    fn for_canonical(state_dir: impl AsRef<Path>, canonical_path: PathBuf) -> Result<Self> {
        let state_dir = state_dir.as_ref().to_path_buf();
        let key = scope_key(&canonical_path);
        let index = ScopeIndex::open(&state_dir)?;
        // Store the lossy string form; scope identity is the key, the path
        // column is informational. Non-UTF8 paths still get a stable key.
        index.register(key.as_str(), &canonical_path.to_string_lossy())?;
        let pool = PoolStore::open(key.as_str(), &state_dir)?;
        tracing::debug!(
            scope_key = %key,
            path = %canonical_path.display(),
            "opened pool context"
        );
        Ok(Self {
            state_dir,
            canonical_path,
            scope_key: key,
            index,
            pool,
        })
    }
}

/// Convenience free function: open (or create) the pool for `project_path`.
///
/// Equivalent to [`PoolContext::open`]; returns the full context so callers
/// can reach `index`, `pool`, `canonical_path`, and `scope_key`.
pub fn open_pool(
    state_dir: impl AsRef<Path>,
    project_path: impl AsRef<Path>,
) -> Result<PoolContext> {
    PoolContext::open(state_dir, project_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::resolve_scope_for_pane;
    use herdr_protocol::{AgentStatus, MetadataMap, PaneInfo};

    fn pane(cwd: Option<&str>) -> PaneInfo {
        PaneInfo {
            pane_id: "w1:p1".into(),
            workspace_id: "w1".into(),
            tab_id: "w1:t1".into(),
            terminal_id: "t1".into(),
            agent_status: AgentStatus::Idle,
            revision: 0,
            focused: false,
            agent: None,
            agent_session: None,
            cwd: cwd.map(str::to_string),
            foreground_cwd: None,
            display_agent: None,
            label: None,
            restore_error: None,
            scroll: None,
            terminal_title: None,
            terminal_title_stripped: None,
            title: None,
            tokens: MetadataMap::new(),
            state_labels: MetadataMap::new(),
        }
    }

    #[test]
    fn open_pool_registers_scope_and_creates_db() {
        let state = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();

        let ctx = open_pool(state.path(), project.path()).unwrap();

        // Pool db exists under pools/<scope_key>/pool.db — key only, no raw path.
        let db = state
            .path()
            .join("pools")
            .join(ctx.scope_key.as_str())
            .join("pool.db");
        assert!(db.exists());
        assert!(!ctx.scope_key.as_str().contains('/'));

        // index.db maps the key back to the canonical path.
        let resolved = ctx.index.resolve(ctx.scope_key.as_str()).unwrap();
        assert_eq!(
            resolved.as_deref(),
            Some(ctx.canonical_path.to_string_lossy().as_ref())
        );

        // Re-opening is idempotent.
        let ctx2 = PoolContext::open(state.path(), project.path()).unwrap();
        assert_eq!(ctx.scope_key, ctx2.scope_key);
    }

    #[test]
    fn two_spellings_of_same_project_share_one_pool() {
        let state = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let sub = project.path().join("sub");
        std::fs::create_dir(&sub).unwrap();

        let spelled = format!("{}/./sub/", project.path().display());
        let ctx_a = open_pool(state.path(), &sub).unwrap();
        let ctx_b = open_pool(state.path(), &spelled).unwrap();
        assert_eq!(ctx_a.scope_key, ctx_b.scope_key);
        assert_eq!(ctx_a.pool.path(), ctx_b.pool.path());
    }

    #[test]
    fn open_pool_from_agent_record() {
        let state = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let cwd = project.path().to_string_lossy().to_string();

        let resolved = resolve_scope_for_pane(&pane(Some(&cwd)), None).unwrap();
        let ctx = PoolContext::for_resolved(state.path(), resolved).unwrap();
        assert_eq!(
            ctx.canonical_path,
            std::fs::canonicalize(project.path()).unwrap()
        );
    }

    #[test]
    fn scope_index_lists_registered_scopes() {
        let state = tempfile::tempdir().unwrap();
        let p1 = tempfile::tempdir().unwrap();
        let p2 = tempfile::tempdir().unwrap();
        open_pool(state.path(), p1.path()).unwrap();
        open_pool(state.path(), p2.path()).unwrap();

        let index = ScopeIndex::open(state.path()).unwrap();
        let entries = index.list().unwrap();
        assert_eq!(entries.len(), 2);
        for e in &entries {
            assert_eq!(e.scope_key.len(), 32);
            assert!(!e.canonical_path.contains("pools"));
        }
    }
}
