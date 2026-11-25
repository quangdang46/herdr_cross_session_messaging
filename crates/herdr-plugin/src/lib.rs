//! Herdr plugin support: manifest model, skill installer, paths.
//!
//! The plugin is distributed as a `herdr-plugin.toml` manifest plus the
//! `herdr-pool` / `herdr-routerd` binaries. This crate owns parsing the
//! manifest, resolving Herdr-injected plugin/pane environment, and
//! installing the `herdr-messaging` agent skill into per-agent skill dirs.

pub mod error;
pub mod manifest;
pub mod paths;
pub mod skill;

pub use error::{PluginError, Result};
