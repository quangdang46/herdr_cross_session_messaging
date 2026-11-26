//! Message and thread storage API.
//!
//! [`MessageStore`] is a scoped view over a [`PoolStore`]'s connection. All
//! writes run inside transactions; every public method locks the pool's
//! `Mutex<Connection>` for the duration of one short transaction.
//!
//! ## Status flow
//!
//! ```text
//! pending --mark_delivered--> delivered --mark_acknowledged--> acknowledged
//!    |  \                          ^
//!    |   \-- mark_acknowledged     | record_failure is NOT allowed here
//!    |      (explicit read =        |
//!    |       consumed; cancels      |
//!    |       pending delivery)      |
//!    | record_failure              |
//!    | defer (agent_blocked:       |
//!    |  reschedule, no attempt)    |
//!    v                            |
//!  failed --record_failure--> failed (until MAX_DELIVERY_ATTEMPTS) -> dead
//!    |                            |
//!    +--- retry() ----> pending <-+   (failed|dead|pending-deferred only;
//!                                    delivered/acknowledged can never go
//!                                    back to pending)
//! ```
//!
//! `failed` rows carry `next_retry_at` and are picked up again by
//! [`MessageStore::outbox_due`]; `dead` rows are terminal until an explicit
//! [`MessageStore::retry`]. The schema `CHECK` constraint is a backstop —
//! transition legality is enforced here in code.
//!
//! ## IDs and timestamps
//!
//! - Message IDs: `msg_<uuidv7-simple>` (32 lowercase hex chars).
//! - Thread IDs: `th_<uuidv7-simple>`.
//! - All `*_at` columns are **unix epoch milliseconds** (`i64`).

use std::str::FromStr;
use std::sync::{Mutex, MutexGuard};

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::error::{Result, StoreError};
use crate::pool::PoolStore;

/// Maximum recorded delivery failures before a message is dead-lettered.
pub const MAX_DELIVERY_ATTEMPTS: i64 = 8;

/// Current time as unix epoch **milliseconds**. All timestamp columns use this.
pub(crate) fn now_millis() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

pub(crate) fn new_message_id() -> String {
    format!("msg_{}", uuid::Uuid::now_v7().simple())
}

pub(crate) fn new_thread_id() -> String {
    format!("th_{}", uuid::Uuid::now_v7().simple())
}

/// True when `err` is a transient SQLite lock conflict worth retrying.
pub(crate) fn is_busy(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(f, _)
            if matches!(
                f.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

/// Apply the standard connection pragmas. Must run before migrations.
pub(crate) fn configure_connection(conn: &Connection) -> Result<()> {
    // busy_timeout goes first: switching a brand-new database to WAL needs
    // an exclusive lock, and racing first-openers (CLI + daemon) would
    // otherwise get an instant SQLITE_BUSY — the pragma below only takes
    // effect for statements after it. journal_mode can still report BUSY
    // immediately in that window, so retry within the same budget.
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    // journal_mode returns a row, so use execute_batch (which tolerates it)
    // rather than pragma_update.
    loop {
        match conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;
             PRAGMA synchronous=NORMAL;",
        ) {
            Err(err) if is_busy(&err) && std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            result => return result.map_err(StoreError::from),
        }
    }
}

/// Lock a connection mutex, recovering from poisoning. A poisoned mutex means
/// a previous panic mid-transaction; SQLite itself stays consistent because
/// uncommitted transactions roll back, so recovering the guard is safe.
pub(crate) fn lock<'a>(mutex: &'a Mutex<Connection>) -> MutexGuard<'a, Connection> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Message delivery status. Persisted as TEXT; see module docs for transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageStatus {
    /// In the outbox, never attempted (or explicitly retried).
    Pending,
    /// `agent.prompt` submission succeeded (or queued by the harness).
    Delivered,
    /// Recipient read/replied to the message.
    Acknowledged,
    /// Last delivery attempt failed; `next_retry_at` schedules the retry.
    Failed,
    /// Gave up after [`MAX_DELIVERY_ATTEMPTS`] failures. Terminal until `retry()`.
    Dead,
}

impl MessageStatus {
    /// Stable string form stored in the `status` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Delivered => "delivered",
            Self::Acknowledged => "acknowledged",
            Self::Failed => "failed",
            Self::Dead => "dead",
        }
    }
}

impl std::fmt::Display for MessageStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MessageStatus {
    type Err = StoreError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "pending" => Ok(Self::Pending),
            "delivered" => Ok(Self::Delivered),
            "acknowledged" => Ok(Self::Acknowledged),
            "failed" => Ok(Self::Failed),
            "dead" => Ok(Self::Dead),
            other => Err(StoreError::Invalid(format!(
                "unknown message status '{other}'"
            ))),
        }
    }
}

impl FromSql for MessageStatus {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = String::column_result(value)?;
        s.parse()
            .map_err(|e: StoreError| FromSqlError::Other(Box::new(e)))
    }
}

impl ToSql for MessageStatus {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.as_str().to_sql()
    }
}

/// A row from the `messages` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// `msg_<uuidv7>` primary key.
    pub id: String,
    /// Owning thread (`th_<uuidv7>`); `None` only for legacy/imported rows.
    pub thread_id: Option<String>,
    /// Sender `agent_id`.
    pub from_agent: String,
    /// Recipient `agent_id`.
    pub to_agent: String,
    /// Free-form kind (`message`, `reply`, `notice`, ...).
    pub kind: String,
    pub content: String,
    pub status: MessageStatus,
    /// Number of recorded delivery failures.
    pub attempt_count: i64,
    /// Earliest time a `failed` message may be retried (unix millis).
    pub next_retry_at: Option<i64>,
    /// When `agent.prompt` delivery succeeded (unix millis).
    pub delivered_at: Option<i64>,
    /// When the recipient acknowledged (unix millis).
    pub acked_at: Option<i64>,
    /// Creation time (unix millis).
    pub created_at: i64,
}

/// A row from the `threads` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    /// `th_<uuidv7>` primary key.
    pub thread_id: String,
    pub scope_key: String,
    pub subject: Option<String>,
    pub created_at: i64,
    pub last_activity_at: i64,
}

fn message_from_row(row: &Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: row.get("id")?,
        thread_id: row.get("thread_id")?,
        from_agent: row.get("from_agent")?,
        to_agent: row.get("to_agent")?,
        kind: row.get("kind")?,
        content: row.get("content")?,
        status: row.get("status")?,
        attempt_count: row.get("attempt_count")?,
        next_retry_at: row.get("next_retry_at")?,
        delivered_at: row.get("delivered_at")?,
        acked_at: row.get("acked_at")?,
        created_at: row.get("created_at")?,
    })
}

fn thread_from_row(row: &Row<'_>) -> rusqlite::Result<Thread> {
    Ok(Thread {
        thread_id: row.get("thread_id")?,
        scope_key: row.get("scope_key")?,
        subject: row.get("subject")?,
        created_at: row.get("created_at")?,
        last_activity_at: row.get("last_activity_at")?,
    })
}

const MESSAGE_COLUMNS: &str =
    "id, thread_id, from_agent, to_agent, kind, content, status, attempt_count, \
     next_retry_at, delivered_at, acked_at, created_at";

const THREAD_COLUMNS: &str = "thread_id, scope_key, subject, created_at, last_activity_at";

/// Scoped repository over the `messages` and `threads` tables of one pool db.
///
/// Cheap to construct (`MessageStore::from(&pool)`-style borrow); all state
/// lives in the [`PoolStore`].
pub struct MessageStore<'a> {
    pool: &'a PoolStore,
}

impl<'a> MessageStore<'a> {
    pub(crate) fn new(pool: &'a PoolStore) -> Self {
        Self { pool }
    }

    // ----- threads ---------------------------------------------------------

    /// Create an empty thread. `subject` is optional display metadata.
    #[tracing::instrument(skip_all, name = "store.query", level = "debug", fields(scope_key = %self.pool.scope_key()))]
    pub fn create_thread(&self, subject: Option<&str>) -> Result<Thread> {
        let conn = self.pool.conn();
        let tx = conn.unchecked_transaction()?;
        let thread = insert_thread(&tx, self.pool.scope_key(), subject)?;
        tx.commit()?;
        Ok(thread)
    }

    /// Fetch a thread by id.
    #[tracing::instrument(skip_all, name = "store.query", level = "debug", fields(scope_key = %self.pool.scope_key()))]
    pub fn thread(&self, thread_id: &str) -> Result<Option<Thread>> {
        let conn = self.pool.conn();
        let row = conn
            .query_row(
                &format!("SELECT {THREAD_COLUMNS} FROM threads WHERE thread_id = ?1"),
                params![thread_id],
                thread_from_row,
            )
            .optional()?;
        Ok(row)
    }

    /// All threads in this scope, most recently active first.
    #[tracing::instrument(skip_all, name = "store.query", level = "debug", fields(scope_key = %self.pool.scope_key()))]
    pub fn threads(&self) -> Result<Vec<Thread>> {
        let conn = self.pool.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {THREAD_COLUMNS} FROM threads ORDER BY last_activity_at DESC, created_at DESC"
        ))?;
        let rows = stmt
            .query_map([], thread_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ----- sending ---------------------------------------------------------

    /// Send a message, creating a new thread. The thread subject defaults to
    /// the first line of `content` (truncated to 80 chars) for `threads` display.
    #[tracing::instrument(skip_all, name = "store.query", level = "debug", fields(scope_key = %self.pool.scope_key()))]
    pub fn send(&self, from_agent: &str, to_agent: &str, content: &str) -> Result<Message> {
        self.insert_message(None, None, from_agent, to_agent, "message", content)
    }

    /// Send a message into an existing thread.
    ///
    /// `kind` is free-form (`message`, `notice`, `reply`, ...).
    #[tracing::instrument(skip_all, name = "store.query", level = "debug", fields(scope_key = %self.pool.scope_key()))]
    pub fn send_in_thread(
        &self,
        thread_id: &str,
        from_agent: &str,
        to_agent: &str,
        kind: &str,
        content: &str,
    ) -> Result<Message> {
        self.insert_message(Some(thread_id), None, from_agent, to_agent, kind, content)
    }

    /// Reply to a message: same thread, recipient = original sender.
    ///
    /// Accepts the original message id (`msg_...`). The returned message has
    /// `kind = "reply"`.
    #[tracing::instrument(skip_all, name = "store.query", level = "debug", fields(scope_key = %self.pool.scope_key()))]
    pub fn reply(&self, message_id: &str, from_agent: &str, content: &str) -> Result<Message> {
        let original = self
            .message(message_id)?
            .ok_or_else(|| StoreError::NotFound(format!("message '{message_id}'")))?;
        self.insert_message(
            original.thread_id.as_deref(),
            None,
            from_agent,
            &original.from_agent,
            "reply",
            content,
        )
    }

    fn insert_message(
        &self,
        thread_id: Option<&str>,
        subject: Option<&str>,
        from_agent: &str,
        to_agent: &str,
        kind: &str,
        content: &str,
    ) -> Result<Message> {
        let conn = self.pool.conn();
        let tx = conn.unchecked_transaction()?;

        let now = now_millis();
        let message_id = new_message_id();
        let resolved_thread_id = match thread_id {
            Some(tid) => {
                let exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM threads WHERE thread_id = ?1)",
                    params![tid],
                    |row| row.get(0),
                )?;
                if !exists {
                    return Err(StoreError::NotFound(format!("thread '{tid}'")));
                }
                tid.to_string()
            }
            None => {
                // Default thread subject: first line of content, truncated.
                let derived = content.lines().next().map(|line| {
                    let line = line.trim();
                    if line.chars().count() > 80 {
                        line.chars().take(80).collect::<String>()
                    } else {
                        line.to_string()
                    }
                });
                insert_thread(
                    &tx,
                    self.pool.scope_key(),
                    subject.map(str::to_string).or(derived).as_deref(),
                )?
                .thread_id
            }
        };

        tx.execute(
            "INSERT INTO messages \
             (id, thread_id, from_agent, to_agent, kind, content, status, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7)",
            params![
                message_id,
                resolved_thread_id,
                from_agent,
                to_agent,
                kind,
                content,
                now
            ],
        )?;
        tx.execute(
            "UPDATE threads SET last_activity_at = ?1 WHERE thread_id = ?2",
            params![now, resolved_thread_id],
        )?;
        tx.commit()?;

        Ok(Message {
            id: message_id,
            thread_id: Some(resolved_thread_id),
            from_agent: from_agent.to_string(),
            to_agent: to_agent.to_string(),
            kind: kind.to_string(),
            content: content.to_string(),
            status: MessageStatus::Pending,
            attempt_count: 0,
            next_retry_at: None,
            delivered_at: None,
            acked_at: None,
            created_at: now,
        })
    }

    // ----- reads -----------------------------------------------------------

    /// Fetch a message by id.
    #[tracing::instrument(skip_all, name = "store.query", level = "debug", fields(scope_key = %self.pool.scope_key()))]
    pub fn message(&self, id: &str) -> Result<Option<Message>> {
        let conn = self.pool.conn();
        let row = conn
            .query_row(
                &format!("SELECT {MESSAGE_COLUMNS} FROM messages WHERE id = ?1"),
                params![id],
                message_from_row,
            )
            .optional()?;
        Ok(row)
    }

    /// All messages in a thread, oldest first.
    #[tracing::instrument(skip_all, name = "store.query", level = "debug", fields(scope_key = %self.pool.scope_key()))]
    pub fn thread_messages(&self, thread_id: &str) -> Result<Vec<Message>> {
        let conn = self.pool.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages WHERE thread_id = ?1 \
             ORDER BY created_at ASC, id ASC"
        ))?;
        let rows = stmt
            .query_map(params![thread_id], message_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Inbox for `agent_id`: all messages addressed to it that are not
    /// dead-lettered (`pending`, `delivered`, `acknowledged`), newest first.
    ///
    /// With `unread_only = true`, only `pending` + `delivered` are returned
    /// (i.e. not yet acknowledged).
    #[tracing::instrument(skip_all, name = "store.query", level = "debug", fields(scope_key = %self.pool.scope_key()))]
    pub fn inbox(&self, agent_id: &str, unread_only: bool) -> Result<Vec<Message>> {
        let conn = self.pool.conn();
        let sql = if unread_only {
            format!(
                "SELECT {MESSAGE_COLUMNS} FROM messages \
                 WHERE to_agent = ?1 AND status IN ('pending', 'delivered') \
                 ORDER BY created_at DESC, id DESC"
            )
        } else {
            format!(
                "SELECT {MESSAGE_COLUMNS} FROM messages \
                 WHERE to_agent = ?1 \
                   AND status IN ('pending', 'delivered', 'acknowledged') \
                 ORDER BY created_at DESC, id DESC"
            )
        };
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![agent_id], message_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Outbox drain: messages deliverable at `now_millis` (unix millis).
    ///
    /// Returns `pending` rows plus `failed` rows whose `next_retry_at` is NULL
    /// or <= `now_millis`, oldest first. `delivered`, `acknowledged`, and
    /// `dead` rows are never returned.
    #[tracing::instrument(skip_all, name = "store.query", level = "debug", fields(scope_key = %self.pool.scope_key()))]
    pub fn outbox_due(&self, now_millis: i64) -> Result<Vec<Message>> {
        let conn = self.pool.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages \
             WHERE status IN ('pending', 'failed') \
               AND (next_retry_at IS NULL OR next_retry_at <= ?1) \
             ORDER BY created_at ASC, id ASC"
        ))?;
        let rows = stmt
            .query_map(params![now_millis], message_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ----- status transitions ----------------------------------------------

    /// pending|failed -> delivered. Sets `delivered_at`.
    ///
    /// This is the only path that marks a message delivered; it is what the
    /// delivery worker calls after `agent.prompt` succeeds.
    #[tracing::instrument(skip_all, name = "store.query", level = "debug", fields(scope_key = %self.pool.scope_key()))]
    pub fn mark_delivered(&self, id: &str) -> Result<()> {
        self.transition(
            id,
            &[MessageStatus::Pending, MessageStatus::Failed],
            "delivered",
            |tx, now| {
                tx.execute(
                    "UPDATE messages SET status = 'delivered', delivered_at = ?1, \
                     next_retry_at = NULL WHERE id = ?2",
                    params![now, id],
                )?;
                Ok(())
            },
        )
    }

    /// delivered|pending -> acknowledged. Sets `acked_at`.
    ///
    /// Called when the recipient reads or replies to the message.
    /// `pending -> acknowledged` is allowed so a recipient that reads mail
    /// via `inbox` *before* the delivery worker prompts it does not get a
    /// duplicate prompt later — acknowledging pending mail consumes it.
    #[tracing::instrument(skip_all, name = "store.query", level = "debug", fields(scope_key = %self.pool.scope_key()))]
    pub fn mark_acknowledged(&self, id: &str) -> Result<()> {
        self.transition(
            id,
            &[MessageStatus::Delivered, MessageStatus::Pending],
            "acknowledged",
            |tx, now| {
                tx.execute(
                    "UPDATE messages SET status = 'acknowledged', acked_at = ?1 WHERE id = ?2",
                    params![now, id],
                )?;
                Ok(())
            },
        )
    }

    /// pending|failed -> failed, incrementing `attempt_count` and scheduling
    /// `next_retry_at`. Returns the resulting status: `Failed`, or `Dead` once
    /// [`MAX_DELIVERY_ATTEMPTS`] failures have been recorded.
    ///
    /// `next_retry_at` is unix millis; pass `None` to make the row immediately
    /// due on the next [`Self::outbox_due`] sweep.
    #[tracing::instrument(skip_all, name = "store.query", level = "debug", fields(scope_key = %self.pool.scope_key()))]
    pub fn record_failure(&self, id: &str, next_retry_at: Option<i64>) -> Result<MessageStatus> {
        let conn = self.pool.conn();
        let tx = conn.unchecked_transaction()?;
        let (status, attempts): (MessageStatus, i64) = tx
            .query_row(
                "SELECT status, attempt_count FROM messages WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("message '{id}'")))?;
        if !matches!(status, MessageStatus::Pending | MessageStatus::Failed) {
            return Err(StoreError::Invalid(format!(
                "cannot record failure on {status} message '{id}'"
            )));
        }
        let attempts = attempts + 1;
        let new_status = if attempts >= MAX_DELIVERY_ATTEMPTS {
            MessageStatus::Dead
        } else {
            MessageStatus::Failed
        };
        // Dead rows drop their retry schedule; failed rows keep it.
        let retry_at = if new_status == MessageStatus::Dead {
            None
        } else {
            next_retry_at
        };
        tx.execute(
            "UPDATE messages SET status = ?1, attempt_count = ?2, next_retry_at = ?3 \
             WHERE id = ?4",
            params![new_status.as_str(), attempts, retry_at, id],
        )?;
        tx.commit()?;
        tracing::debug!(message_id = %id, %new_status, attempts, "recorded delivery failure");
        Ok(new_status)
    }

    /// pending|failed -> same status with a new `next_retry_at`, WITHOUT
    /// incrementing `attempt_count` or risking dead-letter.
    ///
    /// Used for `agent_blocked`: the recipient is alive but busy — a
    /// delivery deferral, not a failure — so it must not consume the
    /// [`MAX_DELIVERY_ATTEMPTS`] budget.
    #[tracing::instrument(skip_all, name = "store.query", level = "debug", fields(scope_key = %self.pool.scope_key()))]
    pub fn defer(&self, id: &str, next_retry_at: Option<i64>) -> Result<()> {
        self.transition(
            id,
            &[MessageStatus::Pending, MessageStatus::Failed],
            "deferred",
            |tx, _now| {
                tx.execute(
                    "UPDATE messages SET next_retry_at = ?1 WHERE id = ?2",
                    params![next_retry_at, id],
                )?;
                Ok(())
            },
        )
    }

    /// failed|dead|pending -> pending. The explicit retry path: clears the
    /// failure/defer schedule so the next `outbox_due` sweep picks the
    /// message up. On an already-`pending` row this is a no-op status-wise —
    /// it just clears `next_retry_at` ("deliver now").
    ///
    /// `delivered` and `acknowledged` messages can never return to `pending`
    /// (that would risk duplicate delivery of already-read mail).
    #[tracing::instrument(skip_all, name = "store.query", level = "debug", fields(scope_key = %self.pool.scope_key()))]
    pub fn retry(&self, id: &str) -> Result<()> {
        self.transition(
            id,
            &[
                MessageStatus::Failed,
                MessageStatus::Dead,
                MessageStatus::Pending,
            ],
            "pending",
            |tx, _now| {
                tx.execute(
                    "UPDATE messages SET status = 'pending', next_retry_at = NULL \
                     WHERE id = ?1",
                    params![id],
                )?;
                Ok(())
            },
        )
    }

    /// Validate `id`'s current status is in `allowed`, then run `apply`.
    fn transition<F>(
        &self,
        id: &str,
        allowed: &[MessageStatus],
        target: &str,
        apply: F,
    ) -> Result<()>
    where
        F: FnOnce(&rusqlite::Transaction<'_>, i64) -> Result<()>,
    {
        let conn = self.pool.conn();
        let tx = conn.unchecked_transaction()?;
        let status: Option<MessageStatus> = tx
            .query_row(
                "SELECT status FROM messages WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        let status = status.ok_or_else(|| StoreError::NotFound(format!("message '{id}'")))?;
        if !allowed.contains(&status) {
            return Err(StoreError::Invalid(format!(
                "cannot transition message '{id}' from {status} to {target}"
            )));
        }
        apply(&tx, now_millis())?;
        tx.commit()?;
        tracing::debug!(message_id = %id, from = %status, to = target, "message status transition");
        Ok(())
    }
}

/// Insert a thread row inside an existing transaction.
fn insert_thread(
    tx: &rusqlite::Transaction<'_>,
    scope_key: &str,
    subject: Option<&str>,
) -> Result<Thread> {
    let now = now_millis();
    let thread_id = new_thread_id();
    tx.execute(
        "INSERT INTO threads (thread_id, scope_key, subject, created_at, last_activity_at) \
         VALUES (?1, ?2, ?3, ?4, ?4)",
        params![thread_id, scope_key, subject, now],
    )?;
    Ok(Thread {
        thread_id,
        scope_key: scope_key.to_string(),
        subject: subject.map(str::to_string),
        created_at: now,
        last_activity_at: now,
    })
}
