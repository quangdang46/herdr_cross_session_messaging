//! SQLite storage layer for the message pools.
//!
//! One database per project scope at `<state>/pools/<scope_key>/pool.db`,
//! plus a global `<state>/index.db` mapping scope keys to canonical paths.
//! WAL journal mode allows concurrent CLI readers alongside the daemon.

pub mod error;
pub mod migrations;
pub mod pool;
pub mod registry;
pub mod store;

pub use error::{Result, StoreError};
pub use pool::PoolStore;
