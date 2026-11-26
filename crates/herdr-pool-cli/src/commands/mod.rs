//! Subcommand implementations. Each module exposes `run(...) -> CmdResult<()>`
//! and prints human or `--json` output via [`crate::output`].

pub mod broadcast;
pub mod clean;
pub mod daemon;
pub mod deliver;
pub mod doctor;
pub mod inbox;
pub mod install;
pub mod message;
pub mod panel;
pub mod peers;
pub mod reply;
pub mod rescue;
pub mod send;
pub mod threads;
pub mod update;
pub mod version;
pub mod whoami;

#[cfg(all(test, unix))]
mod tests;
