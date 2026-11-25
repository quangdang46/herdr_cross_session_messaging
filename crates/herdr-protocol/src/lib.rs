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

pub mod client;
pub mod error;
pub mod events;
pub mod types;

pub use client::HerdrClient;
pub use error::{HerdrError, Result};
