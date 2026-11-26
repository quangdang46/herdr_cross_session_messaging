//! Per-scope pool database handle plus the global scope index.
//!
//! Layout under the plugin state dir:
//!
//! ```text
//! <state>/index.db                  # ScopeIndex: scope_key -> canonical_path
//! <state>/pools/<scope_key>/pool.db # one PoolStore per project scope (WAL)
//! ```
//!
//! ## Thread-safety
//!
//! `rusqlite::Connection` is `!Sync`, so each [`PoolStore`] wraps its
//! connection in a `Mutex` and serializes access. Handles are cheap to clone
//! by reference (`&PoolStore` is `Send + Sync`), and all multi-write
//! operations run inside a single SQLite transaction while holding the lock.
//! For higher concurrency, open additional `PoolStore` handles to the same
//! path — WAL mode permits concurrent readers and a single writer across
//! connections, and `busy_timeout=5000` absorbs writer contention.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::Result;
use crate::migrations;
use crate::registry::RegistryStore;
use crate::rescue::RescueStore;
use crate::store::{configure_connection, lock, now_millis, MessageStore};

/// Row counts removed (or that would be removed) by [`PoolStore::prune`].
#[derive(Debug, Clone)]
pub struct PruneReport {
    pub scope_key: String,
    pub messages_deleted: usize,
    pub agents_deleted: usize,
    pub threads_deleted: usize,
    pub dry_run: bool,
}

/// Handle to one scope's pool database (`<state>/pools/<scope_key>/pool.db`).
///
/// `Send + Sync`: all access is serialized through an internal
/// `Mutex<Connection>`; see module docs for the concurrency model.
#[derive(Debug)]
pub struct PoolStore {
    scope_key: String,
    path: PathBuf,
    conn: Mutex<Connection>,
}

impl PoolStore {
    /// Open (creating if needed) the pool db for `scope_key` under `state_dir`.
    ///
    /// Creates `<state>/pools/<scope_key>/`, applies pragmas
    /// (`journal_mode=WAL`, `busy_timeout=5000`, `foreign_keys=ON`,
    /// `synchronous=NORMAL`) and runs pending migrations. Idempotent: safe to
    /// call on every CLI invocation.
    pub fn open(scope_key: &str, state_dir: impl AsRef<Path>) -> Result<Self> {
        let dir = state_dir.as_ref().join("pools").join(scope_key);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("pool.db");
        let mut conn = Connection::open(&path)?;
        configure_connection(&conn)?;
        migrations::migrate(&mut conn)?;
        tracing::debug!(scope_key, path = %path.display(), "opened pool store");
        Ok(Self {
            scope_key: scope_key.to_string(),
            path,
            conn: Mutex::new(conn),
        })
    }

    /// Convenience alias for [`PoolStore::open`].
    pub fn for_scope(scope_key: &str, state_dir: impl AsRef<Path>) -> Result<Self> {
        Self::open(scope_key, state_dir)
    }

    /// The scope key this pool belongs to.
    pub fn scope_key(&self) -> &str {
        &self.scope_key
    }

    /// Filesystem path of the underlying `pool.db`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Borrow the message/thread repository for this scope.
    pub fn messages(&self) -> MessageStore<'_> {
        MessageStore::new(self)
    }

    /// Borrow the agent registry repository for this scope.
    pub fn registry(&self) -> RegistryStore<'_> {
        RegistryStore::new(self)
    }

    /// Borrow the rescue job repository for this scope.
    pub fn rescues(&self) -> RescueStore<'_> {
        RescueStore::new(self)
    }

    /// Delete old terminal-state rows to bound database growth.
    ///
    /// * `messages`: rows in `acknowledged`/`dead` status created before
    ///   `cutoff_ms` (live `pending`/`delivered`/`failed` mail is kept).
    /// * `agents`: `offline`/`unknown` records last seen before `cutoff_ms`
    ///   that no remaining message references (keeps peer names resolvable
    ///   for surviving history).
    /// * `threads`: threads left with zero messages.
    ///
    /// `dry_run` rolls the transaction back after counting. On a real run
    /// the db is `VACUUM`ed when anything was deleted.
    pub fn prune(&self, cutoff_ms: i64, dry_run: bool) -> Result<PruneReport> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let messages = tx.execute(
            "DELETE FROM messages WHERE status IN ('acknowledged','dead') AND created_at < ?",
            params![cutoff_ms],
        )?;
        let agents = tx.execute(
            "DELETE FROM agents WHERE status IN ('offline','unknown') \
             AND last_seen_at < ? \
             AND agent_id NOT IN ( \
                 SELECT from_agent FROM messages \
                 UNION SELECT to_agent FROM messages \
             )",
            params![cutoff_ms],
        )?;
        let threads = tx.execute(
            "DELETE FROM threads WHERE thread_id NOT IN (SELECT thread_id FROM messages)",
            [],
        )?;
        if dry_run {
            drop(tx);
        } else {
            tx.commit()?;
            if messages > 0 || agents > 0 || threads > 0 {
                conn.execute_batch("VACUUM")?;
            }
        }
        Ok(PruneReport {
            scope_key: self.scope_key.clone(),
            messages_deleted: messages,
            agents_deleted: agents,
            threads_deleted: threads,
            dry_run,
        })
    }

    /// Lock the underlying connection. Crate-internal; exposed to sibling
    /// modules (`store`, `registry`) only.
    pub(crate) fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        lock(&self.conn)
    }
}

/// A row from the global `scopes` table in `index.db`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeEntry {
    /// Hashed scope key (`pools/<scope_key>/` directory name).
    pub scope_key: String,
    /// Canonical project path the scope key was derived from.
    pub canonical_path: String,
    /// First registration time (unix millis).
    pub created_at: i64,
}

/// Global index mapping `scope_key -> canonical_path`, stored at
/// `<state>/index.db`. Kept separate from per-scope pools so scope lookup
/// does not require knowing the pool layout internals.
#[derive(Debug)]
pub struct ScopeIndex {
    conn: Mutex<Connection>,
}

impl ScopeIndex {
    /// Open (creating if needed) `<state>/index.db` and ensure its schema.
    #[tracing::instrument(skip_all, name = "store.query", level = "debug")]
    pub fn open(state_dir: impl AsRef<Path>) -> Result<Self> {
        std::fs::create_dir_all(state_dir.as_ref())?;
        let conn = Connection::open(state_dir.as_ref().join("index.db"))?;
        configure_connection(&conn)?;
        {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS scopes (
                    scope_key      TEXT PRIMARY KEY,
                    canonical_path TEXT NOT NULL,
                    created_at     INTEGER NOT NULL
                );",
            )?;
            tx.commit()?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Record (or refresh) the canonical path for a scope key. Idempotent:
    /// re-registering the same pair is a no-op; `created_at` is preserved.
    #[tracing::instrument(skip_all, name = "store.query", level = "debug")]
    pub fn register(&self, scope_key: &str, canonical_path: &str) -> Result<()> {
        let conn = lock(&self.conn);
        conn.execute(
            "INSERT INTO scopes (scope_key, canonical_path, created_at) \
             VALUES (?1, ?2, ?3) \
             ON CONFLICT(scope_key) DO UPDATE SET canonical_path = excluded.canonical_path",
            params![scope_key, canonical_path, now_millis()],
        )?;
        Ok(())
    }

    /// Look up the canonical path for a scope key.
    #[tracing::instrument(skip_all, name = "store.query", level = "debug")]
    pub fn resolve(&self, scope_key: &str) -> Result<Option<String>> {
        let conn = lock(&self.conn);
        let path = conn
            .query_row(
                "SELECT canonical_path FROM scopes WHERE scope_key = ?1",
                params![scope_key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(path)
    }

    /// All registered scopes.
    #[tracing::instrument(skip_all, name = "store.query", level = "debug")]
    pub fn list(&self) -> Result<Vec<ScopeEntry>> {
        let conn = lock(&self.conn);
        let mut stmt = conn.prepare(
            "SELECT scope_key, canonical_path, created_at FROM scopes \
             ORDER BY created_at ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ScopeEntry {
                    scope_key: row.get(0)?,
                    canonical_path: row.get(1)?,
                    created_at: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}
