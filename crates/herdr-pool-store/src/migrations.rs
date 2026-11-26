//! Hand-rolled SQL migrations.
//!
//! refinery was removed due to a dependency conflict, so migrations are a
//! small ordered list of embedded SQL strings applied inside a single
//! transaction. Progress is tracked with `PRAGMA user_version`, which is
//! bumped in the same transaction as the schema change — a failed migration
//! rolls back cleanly and the next open retries from the last good version.
//! Re-opening an up-to-date database is a no-op.

use rusqlite::Connection;

use crate::error::{Result, StoreError};
use crate::store::is_busy;

const V1_SQL: &str = include_str!("../migrations/V1__init.sql");
const V2_SQL: &str = include_str!("../migrations/V2__rescue.sql");
const V3_SQL: &str = include_str!("../migrations/V3__rescue_captured_at.sql");
const V4_SQL: &str = include_str!("../migrations/V4__rescue_dispatch_token.sql");
const V5_SQL: &str = include_str!("../migrations/V5__agent_readiness.sql");

struct Migration {
    version: i64,
    sql: &'static str,
}

/// Migrations in application order. Append new entries; never edit applied ones.
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: V1_SQL,
    },
    Migration {
        version: 2,
        sql: V2_SQL,
    },
    Migration {
        version: 3,
        sql: V3_SQL,
    },
    Migration {
        version: 4,
        sql: V4_SQL,
    },
    Migration {
        version: 5,
        sql: V5_SQL,
    },
];

/// Apply all pending migrations to `conn`. Idempotent.
pub(crate) fn migrate(conn: &mut Connection) -> Result<()> {
    // BEGIN IMMEDIATE makes the loser of a concurrent first-open wait for
    // the winner's commit, but SQLite can still report BUSY/LOCKED before
    // the busy handler engages — retry the whole migration inside the busy
    // budget rather than failing the open.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match migrate_once(conn) {
            Err(StoreError::Sqlite(err)) if is_busy(&err) => {
                if std::time::Instant::now() >= deadline {
                    return Err(StoreError::Sqlite(err));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            result => return result,
        }
    }
}

fn migrate_once(conn: &mut Connection) -> Result<()> {
    // BEGIN IMMEDIATE + read user_version inside the tx: the CLI and the
    // daemon can open the same brand-new pool.db concurrently. A deferred
    // transaction would read a stale user_version, then fail mid-batch on
    // objects the winning migrator already created ("table already
    // exists"). With IMMEDIATE the second opener waits (busy_timeout) for
    // the winner's commit, then sees the bumped version and no-ops.
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let current: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    for migration in MIGRATIONS.iter().filter(|m| m.version > current) {
        tx.execute_batch(migration.sql).map_err(|e| {
            StoreError::Migration(format!("V{} apply failed: {e}", migration.version))
        })?;
        tx.execute_batch(&format!("PRAGMA user_version = {}", migration.version))
            .map_err(|e| {
                StoreError::Migration(format!(
                    "V{} user_version bump failed: {e}",
                    migration.version
                ))
            })?;
        tracing::debug!(version = migration.version, "applied migration");
    }
    tx.commit()?;
    Ok(())
}
