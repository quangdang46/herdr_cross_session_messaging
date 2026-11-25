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

const V1_SQL: &str = include_str!("../migrations/V1__init.sql");

struct Migration {
    version: i64,
    sql: &'static str,
}

/// Migrations in application order. Append new entries; never edit applied ones.
const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    sql: V1_SQL,
}];

/// Apply all pending migrations to `conn`. Idempotent.
pub(crate) fn migrate(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let tx = conn.transaction()?;
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
