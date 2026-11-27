//! hc-spike: synchronous JSON-RPC client for `herdr-core` via `hc connect`.
//!
//! Spike crate (bead `hc-client-spike-q9r`). Proves that a sync client can
//! drive the hc OpenRPC API over one persistent `hc connect --json`
//! subprocess: newline-delimited JSON-RPC, id-correlated responses,
//! `{"method":"event"}` notifications demuxed to subscribers.
//!
//! NOT wired into herdr-pool/herdr-router — spike only.

#![deny(clippy::unwrap_used, clippy::expect_used)]

mod client;
mod error;

pub use client::HcClient;
pub use error::{HcError, Result};
