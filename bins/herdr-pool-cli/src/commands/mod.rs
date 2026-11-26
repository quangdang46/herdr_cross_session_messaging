//! Subcommand implementations. Each module exposes `run(...) -> CmdResult<()>`
//! and prints human or `--json` output via [`crate::output`].

pub mod broadcast;
pub mod daemon;
pub mod deliver;
pub mod doctor;
pub mod inbox;
pub mod install;
pub mod message;
pub mod peers;
pub mod reply;
pub mod send;
pub mod threads;
pub mod version;
pub mod whoami;

#[cfg(test)]
mod tests;
