//! Router daemon library: event loop, registry sync, delivery worker.
//!
//! The daemon holds one long-lived `events.subscribe` connection to the
//! Herdr socket, keeps per-scope agent registries fresh, and drains the
//! message outbox via `agent.prompt`. All logic lives here so it can be
//! exercised in tests with a fake socket server; `herdr-routerd` is a
//! thin binary wrapper.

pub mod daemon;
pub mod delivery;
pub mod error;
pub mod events;
pub mod registry_sync;

pub use error::{Result, RouterError};
