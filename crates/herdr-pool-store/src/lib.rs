//! SQLite storage layer for the message pools.
//!
//! One database per project scope at `<state>/pools/<scope_key>/pool.db`,
//! plus a global `<state>/index.db` mapping scope keys to canonical paths.
//! WAL journal mode allows concurrent CLI readers alongside the daemon.
//!
//! All timestamps are unix epoch **milliseconds** (`i64`). Message/thread/agent
//! IDs are `msg_`/`th_`/`ag_`-prefixed uuidv7 values. See [`store`] for the
//! message status state machine.

// No `unwrap()`/`expect()` in library code; integration tests live in
// `tests/` (separate crates, unaffected by this lint).
#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod error;
pub mod migrations;
pub mod pool;
pub mod registry;
pub mod rescue;
pub mod store;

pub use error::{Result, StoreError};
pub use pool::{PoolStore, PruneReport, ScopeEntry, ScopeIndex};
pub use registry::{new_agent_id, AgentRecord, RegistryStore, STATUS_OFFLINE};
pub use rescue::{new_rescue_id, HelperReport, RescueJob, RescueStatus, RescueStore};
pub use store::{Message, MessageStatus, MessageStore, NudgeRecord, Thread, MAX_DELIVERY_ATTEMPTS};
