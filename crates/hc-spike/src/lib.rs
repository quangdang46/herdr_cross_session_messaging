//! hc-spike: synchronous JSON-RPC client for `herdr-core` via `hc connect`.
//!
//! Spike crate (bead `hc-client-spike-q9r`). The client was promoted into
//! `herdr-runtime` (`herdr_runtime::hc`) for the `HcRuntime` adapter;
//! this crate remains as the manual e2e probe (`hc-spike` bin) and a
//! stable re-export.

#![deny(clippy::unwrap_used, clippy::expect_used)]

pub use herdr_runtime::hc::client::HcClient;
pub use herdr_runtime::hc::error::{HcError, Result};
