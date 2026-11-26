//! Herdr plugin support: manifest model, skill installer, paths.
//!
//! The plugin is distributed as a `herdr-plugin.toml` manifest plus the
//! `herdr-pool` / `herdr-routerd` binaries. This crate owns parsing the
//! manifest, resolving Herdr-injected plugin/pane environment, and
//! installing the `herdr-messaging` agent skill into per-agent skill dirs.

// No `unwrap()`/`expect()` in library code; inline `#[cfg(test)]` modules
// are exempted per-module below.
#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod config;
pub mod error;
pub mod manifest;
pub mod paths;
pub mod skill;

pub use error::{PluginError, Result};
