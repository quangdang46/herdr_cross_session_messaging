//! Per-scope pool registry: lazily opened `PoolStore`s shared by the event
//! loop, delivery worker, and `tick` one-shot.
//!
//! The daemon learns scopes at runtime from `session.snapshot` /
//! `agent.list` rather than a fixed config, so [`Pools`] is a cache:
//! [`Pools::for_resolved`] opens (or returns) the pool for a scope, and
//! [`Pools::all`] enumerates every pool database under the state dir —
//! including ones created earlier by CLI invocations.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use herdr_pool_core::{ResolvedScope, ScopeKey};
use herdr_pool_store::{PoolStore, ScopeIndex};

use crate::error::Result;

/// Shared access to every open (or openable) per-scope pool database.
///
/// `Send + Sync`: stores are `Arc<PoolStore>` handles; each `PoolStore`
/// serializes its own SQLite connection internally (WAL mode permits
/// concurrent readers/writers across connections).
#[derive(Debug)]
pub struct Pools {
    state_dir: PathBuf,
    index: ScopeIndex,
    stores: Mutex<HashMap<String, Arc<PoolStore>>>,
}

impl Pools {
    /// Open the global `index.db` under `state_dir` (created if needed).
    pub fn open(state_dir: impl AsRef<Path>) -> Result<Self> {
        let state_dir = state_dir.as_ref().to_path_buf();
        let index = ScopeIndex::open(&state_dir)?;
        Ok(Self {
            state_dir,
            index,
            stores: Mutex::new(HashMap::new()),
        })
    }

    /// The state dir this registry is rooted at.
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// The global scope index (`index.db`).
    pub fn index(&self) -> &ScopeIndex {
        &self.index
    }

    /// Get (opening on first use) the pool for an already-resolved scope.
    ///
    /// Registers the scope in `index.db` and opens
    /// `<state>/pools/<scope_key>/pool.db` with migrations applied.
    pub fn for_resolved(&self, resolved: &ResolvedScope) -> Result<Arc<PoolStore>> {
        self.for_scope_key(resolved.scope_key.as_str(), &resolved.canonical_path)
    }

    /// Get (opening on first use) the pool for a bare scope key.
    pub fn for_scope_key(&self, scope_key: &str, canonical_path: &Path) -> Result<Arc<PoolStore>> {
        {
            let stores = self.stores.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(store) = stores.get(scope_key) {
                return Ok(Arc::clone(store));
            }
        }
        self.index
            .register(scope_key, &canonical_path.to_string_lossy())?;
        let store = Arc::new(PoolStore::open(scope_key, &self.state_dir)?);
        let mut stores = self.stores.lock().unwrap_or_else(|e| e.into_inner());
        Ok(Arc::clone(
            stores.entry(scope_key.to_string()).or_insert(store),
        ))
    }

    /// All pools currently known: every scope key in `index.db` plus every
    /// `pools/<key>/` directory on disk (covers pools created by CLI
    /// processes while the daemon was down). Individual open failures are
    /// logged and skipped so one corrupt pool cannot stall the daemon.
    pub fn all(&self) -> Vec<Arc<PoolStore>> {
        let mut keys: Vec<String> = self
            .index
            .list()
            .map(|entries| entries.into_iter().map(|e| e.scope_key).collect())
            .unwrap_or_default();
        if let Ok(entries) = std::fs::read_dir(self.state_dir.join("pools")) {
            for entry in entries.flatten() {
                if let Ok(name) = entry.file_name().into_string() {
                    if is_scope_key_dir(&name) && entry.path().join("pool.db").exists() {
                        keys.push(name);
                    }
                }
            }
        }
        keys.sort();
        keys.dedup();

        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            match self.open_existing(&key) {
                Ok(store) => out.push(store),
                Err(err) => {
                    tracing::warn!(scope_key = %key, error = %err, "skipping unreadable pool");
                }
            }
        }
        out
    }

    /// Open a pool for a scope key without a canonical path (the index may
    /// already know it; otherwise register a placeholder path derived from
    /// the key so `PoolStore::open` still works — the key, not the path, is
    /// the pool identity).
    fn open_existing(&self, scope_key: &str) -> Result<Arc<PoolStore>> {
        if let Some(cached) = self
            .stores
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(scope_key)
        {
            return Ok(Arc::clone(cached));
        }
        let path = self
            .index
            .resolve(scope_key)?
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("scope:{scope_key}")));
        self.for_scope_key(scope_key, &path)
    }
}

/// Scope keys are 32 lowercase hex chars — matches `ScopeKey` generation.
fn is_scope_key_dir(name: &str) -> bool {
    name.len() == 32 && name.chars().all(|c| c.is_ascii_hexdigit())
}

/// Convenience: build a [`ScopeKey`] check helper for callers holding a
/// `ScopeKey` rather than a string.
pub fn scope_key_of(resolved: &ResolvedScope) -> &ScopeKey {
    &resolved.scope_key
}
