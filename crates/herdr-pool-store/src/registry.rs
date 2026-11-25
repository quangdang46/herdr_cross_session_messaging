//! Agent registry storage: one `agents` table per scope pool db.
//!
//! Records are plain structs owned by this crate — deliberately *not* tied to
//! `herdr-protocol` types, so the store stays usable before/without the
//! protocol layer. All fields the router needs to re-resolve a live Herdr
//! agent are present: `terminal_id` is the stable re-resolution anchor,
//! `pane_id` changes on `pane.move` (see [`RegistryStore::rekey_pane`]).

use std::collections::BTreeMap;

use rusqlite::{params, OptionalExtension, Row};

use crate::error::{Result, StoreError};
use crate::pool::PoolStore;
use crate::store::now_millis;

/// Status string used for agents that have exited or vanished from Herdr.
pub const STATUS_OFFLINE: &str = "offline";

/// Generate a fresh agent id: `ag_<first 12 hex chars of uuidv7>`.
///
/// 48 bits of time-ordered randomness — plenty for per-scope agent counts.
/// Callers persist the returned id in [`AgentRecord::agent_id`]; it stays
/// stable for the life of the record even across `pane.move` and agent
/// restarts (look up by `terminal_id` via [`RegistryStore::get_by_terminal`]).
pub fn new_agent_id() -> String {
    let simple = uuid::Uuid::now_v7().simple().to_string();
    format!("ag_{}", &simple[..12])
}

/// A row from the `agents` table.
///
/// Timestamps are unix epoch milliseconds. `first_seen_at` is set on insert
/// and preserved across upserts; `last_seen_at`/`offline_at` track liveness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRecord {
    /// `ag_<uuidv7-short>` primary key. Stable for the life of the record.
    pub agent_id: String,
    /// Scope this record belongs to (denormalized; the db path is authoritative).
    pub scope_key: String,
    /// Herdr terminal id — stable re-resolution anchor across `pane.move`.
    pub terminal_id: Option<String>,
    /// Herdr pane id — changes when the pane is moved between workspaces.
    pub pane_id: Option<String>,
    /// Agent kind as reported by Herdr (`claude`, `codex`, ...).
    pub agent_kind: Option<String>,
    /// Agent display name, if assigned.
    pub name: Option<String>,
    /// Canonical project cwd the agent was scoped under.
    pub cwd: Option<String>,
    /// Herdr agent status (`idle`, `working`, `blocked`, `done`, `unknown`)
    /// or [`STATUS_OFFLINE`].
    pub status: String,
    /// Bootstrap prompt version already delivered to this agent, if any.
    pub bootstrap_version: Option<u32>,
    /// First registration (unix millis).
    pub first_seen_at: i64,
    /// Most recent sighting/upsert (unix millis).
    pub last_seen_at: i64,
    /// When the agent was marked offline (unix millis), if ever.
    pub offline_at: Option<i64>,
}

fn agent_from_row(row: &Row<'_>) -> rusqlite::Result<AgentRecord> {
    let bootstrap: Option<i64> = row.get("bootstrap_version")?;
    Ok(AgentRecord {
        agent_id: row.get("agent_id")?,
        scope_key: row.get("scope_key")?,
        terminal_id: row.get("terminal_id")?,
        pane_id: row.get("pane_id")?,
        agent_kind: row.get("agent_kind")?,
        name: row.get("name")?,
        cwd: row.get("cwd")?,
        status: row.get("status")?,
        bootstrap_version: bootstrap.and_then(|v| u32::try_from(v).ok()),
        first_seen_at: row.get("first_seen_at")?,
        last_seen_at: row.get("last_seen_at")?,
        offline_at: row.get("offline_at")?,
    })
}

const AGENT_COLUMNS: &str = "agent_id, scope_key, terminal_id, pane_id, agent_kind, name, \
     cwd, status, bootstrap_version, first_seen_at, last_seen_at, offline_at";

/// Scoped repository over the `agents` table of one pool db.
pub struct RegistryStore<'a> {
    pool: &'a PoolStore,
}

impl<'a> RegistryStore<'a> {
    pub(crate) fn new(pool: &'a PoolStore) -> Self {
        Self { pool }
    }

    /// Insert or update an agent record, keyed on `agent_id`.
    ///
    /// Idempotent: re-upserting the same record converges to the same row.
    /// `first_seen_at` is preserved from the original insert; all other
    /// mutable fields (including `last_seen_at`) take the new values.
    pub fn upsert_agent(&self, agent: &AgentRecord) -> Result<()> {
        let conn = self.pool.conn();
        conn.execute(
            "INSERT INTO agents \
             (agent_id, scope_key, terminal_id, pane_id, agent_kind, name, cwd, status, \
              bootstrap_version, first_seen_at, last_seen_at, offline_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
             ON CONFLICT(agent_id) DO UPDATE SET \
               scope_key         = excluded.scope_key, \
               terminal_id       = excluded.terminal_id, \
               pane_id           = excluded.pane_id, \
               agent_kind        = excluded.agent_kind, \
               name              = excluded.name, \
               cwd               = excluded.cwd, \
               status            = excluded.status, \
               bootstrap_version = excluded.bootstrap_version, \
               last_seen_at      = excluded.last_seen_at, \
               offline_at        = excluded.offline_at",
            params![
                agent.agent_id,
                agent.scope_key,
                agent.terminal_id,
                agent.pane_id,
                agent.agent_kind,
                agent.name,
                agent.cwd,
                agent.status,
                agent.bootstrap_version.map(|v| v as i64),
                agent.first_seen_at,
                agent.last_seen_at,
                agent.offline_at,
            ],
        )?;
        Ok(())
    }

    /// Fetch one agent by `agent_id` (any status, including offline).
    pub fn get_agent(&self, agent_id: &str) -> Result<Option<AgentRecord>> {
        let conn = self.pool.conn();
        let row = conn
            .query_row(
                &format!("SELECT {AGENT_COLUMNS} FROM agents WHERE agent_id = ?1"),
                params![agent_id],
                agent_from_row,
            )
            .optional()?;
        Ok(row)
    }

    /// All agents in this scope. Offline agents are included only when
    /// `include_offline` is true.
    pub fn list_agents(&self, include_offline: bool) -> Result<Vec<AgentRecord>> {
        let conn = self.pool.conn();
        let sql = if include_offline {
            format!("SELECT {AGENT_COLUMNS} FROM agents ORDER BY agent_id")
        } else {
            format!(
                "SELECT {AGENT_COLUMNS} FROM agents WHERE status != '{STATUS_OFFLINE}' \
                 ORDER BY agent_id"
            )
        };
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], agent_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Agents matching a pane id (usually 0 or 1 live row; history may exist).
    pub fn find_by_pane(&self, pane_id: &str) -> Result<Vec<AgentRecord>> {
        self.find_agents("pane_id", pane_id)
    }

    /// Agents matching a terminal id.
    pub fn find_by_terminal(&self, terminal_id: &str) -> Result<Vec<AgentRecord>> {
        self.find_agents("terminal_id", terminal_id)
    }

    /// Most recently seen **live** (non-offline) agent for a terminal id.
    ///
    /// This is the stable-identity lookup `whoami` needs: the same Herdr
    /// terminal keeps the same `agent_id` across `pane.move` and restarts of
    /// the agent process.
    pub fn get_by_terminal(&self, terminal_id: &str) -> Result<Option<AgentRecord>> {
        let conn = self.pool.conn();
        let row = conn
            .query_row(
                &format!(
                    "SELECT {AGENT_COLUMNS} FROM agents \
                     WHERE terminal_id = ?1 AND status != '{STATUS_OFFLINE}' \
                     ORDER BY last_seen_at DESC LIMIT 1"
                ),
                params![terminal_id],
                agent_from_row,
            )
            .optional()?;
        Ok(row)
    }

    /// Agents matching an agent name (names are unique among *live* agents but
    /// may repeat historically).
    pub fn find_by_name(&self, name: &str) -> Result<Vec<AgentRecord>> {
        self.find_agents("name", name)
    }

    fn find_agents(&self, column: &str, value: &str) -> Result<Vec<AgentRecord>> {
        // `column` is only ever one of the fixed literals passed above.
        let sql = format!(
            "SELECT {AGENT_COLUMNS} FROM agents WHERE {column} = ?1 \
             ORDER BY last_seen_at DESC"
        );
        let conn = self.pool.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![value], agent_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Resolve a peer target to exactly one live agent record.
    ///
    /// Resolution order:
    /// 1. Exact `agent_id` match (returned even if offline — the caller can
    ///    inspect `status`).
    /// 2. Union of `pane_id`, `terminal_id`, and `name` matches among
    ///    non-offline agents. More than one distinct `agent_id` →
    ///    [`StoreError::Ambiguous`]; none → [`StoreError::NotFound`].
    pub fn resolve_peer(&self, target: &str) -> Result<AgentRecord> {
        let conn = self.pool.conn();

        if let Some(agent) = conn
            .query_row(
                &format!("SELECT {AGENT_COLUMNS} FROM agents WHERE agent_id = ?1"),
                params![target],
                agent_from_row,
            )
            .optional()?
        {
            return Ok(agent);
        }

        // Dedupe by agent_id: one agent may match via several fields.
        let mut matches: BTreeMap<String, AgentRecord> = BTreeMap::new();
        let mut stmt = conn.prepare(&format!(
            "SELECT {AGENT_COLUMNS} FROM agents \
             WHERE status != '{STATUS_OFFLINE}' \
               AND (pane_id = ?1 OR terminal_id = ?1 OR name = ?1)"
        ))?;
        let rows = stmt.query_map(params![target], agent_from_row)?;
        for row in rows {
            let agent = row?;
            matches.entry(agent.agent_id.clone()).or_insert(agent);
        }

        match matches.len() {
            0 => Err(StoreError::NotFound(format!(
                "no live agent matches '{target}' in this scope"
            ))),
            1 => matches.into_values().next().ok_or_else(|| {
                StoreError::Invalid("internal: non-empty match set yielded no agent".into())
            }),
            n => Err(StoreError::Ambiguous(format!(
                "'{target}' matches {n} agents: {}",
                matches.keys().cloned().collect::<Vec<_>>().join(", ")
            ))),
        }
    }

    /// Update an agent's `pane_id` after `pane.move`, preserving `agent_id`.
    ///
    /// Also refreshes `last_seen_at`. Fails with [`StoreError::NotFound`] if
    /// the agent is unknown.
    pub fn rekey_pane(&self, agent_id: &str, new_pane_id: &str) -> Result<()> {
        let conn = self.pool.conn();
        let changed = conn.execute(
            "UPDATE agents SET pane_id = ?1, last_seen_at = ?2 WHERE agent_id = ?3",
            params![new_pane_id, now_millis(), agent_id],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("agent '{agent_id}'")));
        }
        Ok(())
    }

    /// Mark an agent offline: sets `status = 'offline'` and `offline_at`.
    /// Offline agents are excluded from [`Self::list_agents`] (default),
    /// [`Self::get_by_terminal`], and [`Self::resolve_peer`] candidate
    /// matching. Fails with [`StoreError::NotFound`] for unknown ids.
    pub fn mark_offline(&self, agent_id: &str) -> Result<()> {
        let conn = self.pool.conn();
        let now = now_millis();
        let changed = conn.execute(
            "UPDATE agents SET status = ?1, offline_at = ?2, last_seen_at = ?2 \
             WHERE agent_id = ?3",
            params![STATUS_OFFLINE, now, agent_id],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("agent '{agent_id}'")));
        }
        Ok(())
    }
}
