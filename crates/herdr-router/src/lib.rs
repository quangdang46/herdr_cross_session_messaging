//! Router daemon library: event loop, registry sync, delivery worker.
//!
//! The daemon holds one long-lived `events.subscribe` connection to the
//! Herdr socket, keeps per-scope agent registries fresh, and drains the
//! message outbox via `agent.prompt`. All logic lives here so it can be
//! exercised in tests with a fake socket server; `herdr-routerd` is a
//! thin binary wrapper.
//!
//! ## Module map
//!
//! - [`envelope`]: the `[herdr-pool] msg ...` prompt text format (wire
//!   contract shared with the CLI agent and the skill).
//! - [`pools`]: [`Pools`] — lazy per-scope `PoolStore` cache shared by
//!   all daemon threads.
//! - [`registry_sync`]: [`RegistrySync`] (snapshot/event → registry +
//!   bootstrap) and [`run_tick`] (one-shot reconcile).
//! - [`delivery`]: [`DeliveryWorker`] outbox drain + backoff policy.
//! - [`events`]: the `events.subscribe` subscription set.
//! - [`daemon`]: [`Daemon`] threads, graceful shutdown, pidfile/lock
//!   lifecycle helpers.

pub mod daemon;
pub mod delivery;
pub mod envelope;
pub mod error;
pub mod events;
pub mod pools;
pub mod registry_sync;

pub use daemon::{
    clear_stop_sentinel, daemon_status, pid_alive, read_pid_file, remove_pid_file, sigterm,
    stop_requested, write_pid_file, write_stop_sentinel, Daemon, DaemonConfig, DaemonHandle,
    DaemonStatus, InstanceGuard, EVENT_WAKEUP, LOCK_DIR_NAME, LOG_FILE_NAME, MAX_CONNECT_BACKOFF,
    PID_FILE_NAME, RECONCILE_INTERVAL, SHUTDOWN_FILE_NAME,
};
pub use delivery::{
    retry_delay, retry_delay_jittered, DeliveryWorker, BACKOFF_BASE, BACKOFF_CAP, BLOCKED_BACKOFF,
    DEAD_NOTIFY_INTERVAL, DEFAULT_POLL_INTERVAL, OFFLINE_DEAD_AFTER,
};
pub use envelope::{bootstrap_text, message_envelope, BOOTSTRAP_VERSION, ENVELOPE_PREFIX};
pub use error::{Result, RouterError};
pub use events::{static_subscriptions, subscriptions_for};
pub use pools::Pools;
pub use registry_sync::{run_tick, Bootstrapper, Notify, RegistrySync, SyncOutcome};
