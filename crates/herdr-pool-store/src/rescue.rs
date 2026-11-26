//! Rescue job storage: one `rescue_jobs` table per scope pool db.
//!
//! A rescue job hands a blocked agent's captured TUI snapshot to a healthy
//! peer helper, which then drives the source pane itself via Herdr. Jobs
//! are durable so a daemon restart resumes pending work instead of losing
//! it, and the partial unique indexes guarantee at most one open job per
//! source agent and per helper (the reservation that prevents rescue
//! loops like A↔B or double-booking a helper).

use rusqlite::{params, OptionalExtension, Row};

use crate::error::{Result, StoreError};
use crate::pool::PoolStore;
use crate::store::now_millis;

/// Status strings for `rescue_jobs.status` (see `RescueStatus::as_str`).
pub const RESCUE_PENDING: &str = "pending";
pub const RESCUE_ASSIGNED: &str = "assigned";
pub const RESCUE_RUNNING: &str = "running";
pub const RESCUE_VERIFYING: &str = "verifying";
pub const RESCUE_RESOLVED: &str = "resolved";
pub const RESCUE_NEEDS_HUMAN: &str = "needs_human";
pub const RESCUE_FAILED: &str = "failed";

/// Statuses that keep a job "open": still eligible for dispatch, holding a
/// helper reservation, or awaiting verification. Terminal statuses are
/// `resolved`, `needs_human`, `failed`.
const OPEN_STATUSES: &str = "'pending', 'assigned', 'running', 'verifying'";
/// Statuses that hold a helper reservation (a pending job has none yet).
const HELPER_HOLDING_STATUSES: &str = "'assigned', 'running', 'verifying'";

/// Lifecycle of a rescue job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RescueStatus {
    /// Waiting for an eligible helper.
    Pending,
    /// Helper chosen and prompt sent; awaiting helper pickup.
    Assigned,
    /// Helper acknowledged/working the rescue.
    Running,
    /// Source pane interaction done; verifying the source unblocked.
    Verifying,
    /// Source agent left `blocked` (by helper or on its own).
    Resolved,
    /// No safe automatic path — human attention required.
    NeedsHuman,
    /// Job could not proceed (source gone, helper unusable, attempts out).
    Failed,
}

impl RescueStatus {
    /// Parse a stored status string. Unknown values map to `Failed` —
    /// safer than resurrecting a job whose state we cannot interpret.
    pub fn parse(s: &str) -> Self {
        match s {
            RESCUE_PENDING => Self::Pending,
            RESCUE_ASSIGNED => Self::Assigned,
            RESCUE_RUNNING => Self::Running,
            RESCUE_VERIFYING => Self::Verifying,
            RESCUE_RESOLVED => Self::Resolved,
            RESCUE_NEEDS_HUMAN => Self::NeedsHuman,
            _ => Self::Failed,
        }
    }

    /// Whether this status keeps the job open (non-terminal).
    pub fn is_open(self) -> bool {
        matches!(
            self,
            Self::Pending | Self::Assigned | Self::Running | Self::Verifying
        )
    }
}

/// A row from the `rescue_jobs` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RescueJob {
    /// `rs_<uuidv7>` primary key.
    pub rescue_id: String,
    /// Scope this job belongs to (denormalized; the db path is authoritative).
    pub scope_key: String,
    /// Blocked agent being rescued.
    pub source_agent_id: String,
    /// Source pane captured at job creation (may go stale on `pane.move`).
    pub source_pane_id: Option<String>,
    /// Helper agent holding the reservation, once assigned.
    pub helper_agent_id: Option<String>,
    /// Helper pane captured at assignment time.
    pub helper_pane_id: Option<String>,
    /// Captured TUI snapshot handed to the helper as context.
    pub snapshot: Option<String>,
    /// Which read produced `snapshot` (`visible`, `recent-unwrapped`, ...).
    pub snapshot_source: Option<String>,
    /// Lifecycle status.
    pub status: RescueStatus,
    /// Dispatch/assignment attempts used.
    pub attempts: u32,
    /// Job creation (unix millis).
    pub created_at: i64,
    /// Last status/timestamp update (unix millis).
    pub updated_at: i64,
    /// When the job reached a terminal status (unix millis), if ever.
    pub resolved_at: Option<i64>,
}

/// Generate a fresh rescue id: `rs_<uuidv7 hex>`.
pub fn new_rescue_id() -> String {
    format!("rs_{}", uuid::Uuid::now_v7().simple())
}

fn rescue_from_row(row: &Row<'_>) -> rusqlite::Result<RescueJob> {
    let status: String = row.get("status")?;
    let attempts: i64 = row.get("attempts")?;
    Ok(RescueJob {
        rescue_id: row.get("rescue_id")?,
        scope_key: row.get("scope_key")?,
        source_agent_id: row.get("source_agent_id")?,
        source_pane_id: row.get("source_pane_id")?,
        helper_agent_id: row.get("helper_agent_id")?,
        helper_pane_id: row.get("helper_pane_id")?,
        snapshot: row.get("snapshot")?,
        snapshot_source: row.get("snapshot_source")?,
        status: RescueStatus::parse(&status),
        attempts: u32::try_from(attempts).unwrap_or(u32::MAX),
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        resolved_at: row.get("resolved_at")?,
    })
}

const RESCUE_COLUMNS: &str = "rescue_id, scope_key, source_agent_id, source_pane_id, \
     helper_agent_id, helper_pane_id, snapshot, snapshot_source, status, attempts, \
     created_at, updated_at, resolved_at";

/// Scoped repository over the `rescue_jobs` table of one pool db.
pub struct RescueStore<'a> {
    pool: &'a PoolStore,
}

impl<'a> RescueStore<'a> {
    pub(crate) fn new(pool: &'a PoolStore) -> Self {
        Self { pool }
    }

    /// Insert a pending rescue job.
    ///
    /// Returns `Ok(false)` without inserting when an open job already
    /// exists for `source_agent_id` (the partial unique index is the
    /// backstop; the pre-check keeps the common path quiet). Fails with
    /// [`StoreError::Sqlite`] if the helper reservation index is violated —
    /// callers must treat that as "helper already reserved".
    #[tracing::instrument(skip_all, name = "store.query", level = "debug", fields(scope_key = %self.pool.scope_key()))]
    pub fn create_pending(
        &self,
        source_agent_id: &str,
        source_pane_id: Option<&str>,
        snapshot: Option<String>,
        snapshot_source: Option<String>,
    ) -> Result<bool> {
        if self.open_for_source(source_agent_id)?.is_some() {
            return Ok(false);
        }
        let now = now_millis();
        self.pool.conn().execute(
            "INSERT INTO rescue_jobs \
             (rescue_id, scope_key, source_agent_id, source_pane_id, snapshot, \
              snapshot_source, status, attempts, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', 0, ?7, ?7)",
            params![
                new_rescue_id(),
                self.pool.scope_key(),
                source_agent_id,
                source_pane_id,
                snapshot,
                snapshot_source,
                now,
            ],
        )?;
        Ok(true)
    }

    /// The open job for a source agent, if any.
    #[tracing::instrument(skip_all, name = "store.query", level = "debug", fields(scope_key = %self.pool.scope_key()))]
    pub fn open_for_source(&self, source_agent_id: &str) -> Result<Option<RescueJob>> {
        let conn = self.pool.conn();
        let row = conn
            .query_row(
                &format!(
                    "SELECT {RESCUE_COLUMNS} FROM rescue_jobs \
                     WHERE source_agent_id = ?1 AND status IN ({OPEN_STATUSES})"
                ),
                params![source_agent_id],
                rescue_from_row,
            )
            .optional()?;
        Ok(row)
    }

    /// The open job holding `helper_agent_id` as its helper, if any.
    #[tracing::instrument(skip_all, name = "store.query", level = "debug", fields(scope_key = %self.pool.scope_key()))]
    pub fn open_for_helper(&self, helper_agent_id: &str) -> Result<Option<RescueJob>> {
        let conn = self.pool.conn();
        let row = conn
            .query_row(
                &format!(
                    "SELECT {RESCUE_COLUMNS} FROM rescue_jobs \
                     WHERE helper_agent_id = ?1 AND status IN ({HELPER_HOLDING_STATUSES})"
                ),
                params![helper_agent_id],
                rescue_from_row,
            )
            .optional()?;
        Ok(row)
    }

    /// All open jobs, oldest first — the dispatch order.
    #[tracing::instrument(skip_all, name = "store.query", level = "debug", fields(scope_key = %self.pool.scope_key()))]
    pub fn list_open(&self) -> Result<Vec<RescueJob>> {
        let conn = self.pool.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {RESCUE_COLUMNS} FROM rescue_jobs \
             WHERE status IN ({OPEN_STATUSES}) ORDER BY created_at, rescue_id"
        ))?;
        let rows = stmt
            .query_map([], rescue_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// All jobs (open and terminal), newest first — for status reporting.
    #[tracing::instrument(skip_all, name = "store.query", level = "debug", fields(scope_key = %self.pool.scope_key()))]
    pub fn list_all(&self) -> Result<Vec<RescueJob>> {
        let conn = self.pool.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {RESCUE_COLUMNS} FROM rescue_jobs \
             ORDER BY created_at DESC, rescue_id DESC"
        ))?;
        let rows = stmt
            .query_map([], rescue_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Fetch one job by id.
    #[tracing::instrument(skip_all, name = "store.query", level = "debug", fields(scope_key = %self.pool.scope_key()))]
    pub fn get(&self, rescue_id: &str) -> Result<Option<RescueJob>> {
        let conn = self.pool.conn();
        let row = conn
            .query_row(
                &format!("SELECT {RESCUE_COLUMNS} FROM rescue_jobs WHERE rescue_id = ?1"),
                params![rescue_id],
                rescue_from_row,
            )
            .optional()?;
        Ok(row)
    }

    /// Claim a pending job for `helper_agent_id`/`helper_pane_id`.
    ///
    /// Atomic `UPDATE ... WHERE status = 'pending'`: returns `false` when
    /// the job is no longer pending (another dispatcher won, or it was
    /// resolved meanwhile). The helper-reservation unique index may still
    /// reject the update if the helper is already held — surfaced as
    /// [`StoreError::Sqlite`]; callers should treat that like `false`.
    #[tracing::instrument(skip_all, name = "store.query", level = "debug", fields(scope_key = %self.pool.scope_key()))]
    pub fn assign(
        &self,
        rescue_id: &str,
        helper_agent_id: &str,
        helper_pane_id: Option<&str>,
    ) -> Result<bool> {
        let conn = self.pool.conn();
        let changed = conn.execute(
            "UPDATE rescue_jobs SET status = 'assigned', helper_agent_id = ?1, \
             helper_pane_id = ?2, attempts = attempts + 1, updated_at = ?3 \
             WHERE rescue_id = ?4 AND status = 'pending'",
            params![helper_agent_id, helper_pane_id, now_millis(), rescue_id],
        )?;
        Ok(changed == 1)
    }

    /// Release a job back to `pending` (helper prompt failed before the
    /// helper took over). Clears the helper reservation; `attempts` was
    /// already bumped by [`Self::assign`].
    #[tracing::instrument(skip_all, name = "store.query", level = "debug", fields(scope_key = %self.pool.scope_key()))]
    pub fn unassign(&self, rescue_id: &str) -> Result<()> {
        let conn = self.pool.conn();
        conn.execute(
            "UPDATE rescue_jobs SET status = 'pending', helper_agent_id = NULL, \
             helper_pane_id = NULL, updated_at = ?1 \
             WHERE rescue_id = ?2 AND status = 'assigned'",
            params![now_millis(), rescue_id],
        )?;
        Ok(())
    }

    /// Transition a job to a new status. Terminal statuses stamp
    /// `resolved_at`; `pending`/`assigned` keep any helper fields as-is
    /// (use [`Self::unassign`] to release a reservation explicitly).
    ///
    /// Fails with [`StoreError::NotFound`] when no row matches.
    #[tracing::instrument(skip_all, name = "store.query", level = "debug", fields(scope_key = %self.pool.scope_key()))]
    pub fn set_status(&self, rescue_id: &str, status: RescueStatus) -> Result<()> {
        let terminal = !status.is_open();
        let conn = self.pool.conn();
        let changed = conn.execute(
            "UPDATE rescue_jobs SET status = ?1, updated_at = ?2, \
             resolved_at = CASE WHEN ?3 THEN ?2 ELSE resolved_at END \
             WHERE rescue_id = ?4",
            params![status_str(status), now_millis(), terminal, rescue_id,],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("rescue job '{rescue_id}'")));
        }
        Ok(())
    }

    /// Refresh the recorded source pane (after `pane.move` re-keys it) and
    /// optionally the captured snapshot. Used by the monitor so the helper
    /// always sees the current pane identity (R9).
    #[tracing::instrument(skip_all, name = "store.query", level = "debug", fields(scope_key = %self.pool.scope_key()))]
    pub fn update_source(
        &self,
        rescue_id: &str,
        source_pane_id: Option<&str>,
        snapshot: Option<String>,
        snapshot_source: Option<String>,
    ) -> Result<()> {
        let conn = self.pool.conn();
        conn.execute(
            "UPDATE rescue_jobs SET source_pane_id = COALESCE(?1, source_pane_id), \
             snapshot = COALESCE(?2, snapshot), \
             snapshot_source = COALESCE(?3, snapshot_source), updated_at = ?4 \
             WHERE rescue_id = ?5",
            params![
                source_pane_id,
                snapshot,
                snapshot_source,
                now_millis(),
                rescue_id
            ],
        )?;
        Ok(())
    }
}

fn status_str(status: RescueStatus) -> &'static str {
    match status {
        RescueStatus::Pending => RESCUE_PENDING,
        RescueStatus::Assigned => RESCUE_ASSIGNED,
        RescueStatus::Running => RESCUE_RUNNING,
        RescueStatus::Verifying => RESCUE_VERIFYING,
        RescueStatus::Resolved => RESCUE_RESOLVED,
        RescueStatus::NeedsHuman => RESCUE_NEEDS_HUMAN,
        RescueStatus::Failed => RESCUE_FAILED,
    }
}
