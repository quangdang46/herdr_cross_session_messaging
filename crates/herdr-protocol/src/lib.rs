//! Synchronous NDJSON client for the Herdr TUI socket API.
//!
//! Wire protocol: one JSON object per line over a Unix domain socket
//! (`$HERDR_SOCKET_PATH`, default `~/.config/herdr/herdr.sock`).
//! Requests look like `{"id":"...","method":"agent.list","params":{}}`;
//! responses echo the id with either `result` or `error`.
//!
//! One request per connection: the server closes after a single response,
//! except long-lived calls (`events.subscribe`, `events.wait`, `agent.wait`,
//! `agent.prompt` with `wait`, `pane.wait_for_output`) which stream frames.

// No `unwrap()`/`expect()` in library code; integration tests live in
// `tests/` (separate crates, unaffected by this lint).
#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod client;
pub mod error;
pub mod events;
pub mod params;
pub mod results;
pub mod types;

pub use client::{
    is_valid_agent_name, resolve_socket_path, HerdrClient, SubscriptionStream,
    DEFAULT_SESSION_NAME, MAX_LINE_BYTES, SESSION_ENV_VAR, SOCKET_PATH_ENV_VAR,
};
pub use error::{HerdrError, Result};
pub use events::{
    EventData, EventEnvelope, EventKind, EventMatch, OutputMatch, PaneAgentStatusChangedEvent,
    PaneOutputMatchedEvent, PaneScrollChangedEvent, Subscription, SubscriptionEventData,
    SubscriptionEventEnvelope, SubscriptionEventKind, SubscriptionFrame,
};
pub use params::*;
pub use results::{ErrorBody, Request, ResponseEnvelope, ResponseResult};
pub use types::*;
