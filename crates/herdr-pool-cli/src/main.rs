//! `herdr-pool` — peer-to-peer messaging CLI for coding agents inside Herdr.
//!
//! Module layout:
//! - [`cli`]: clap derive definitions (the whole command surface).
//! - [`context`]: shared runtime context (socket client, state dir), failure
//!   type, and the identity/scope/registry helpers every command uses.
//! - [`output`]: human-readable tables vs `--json` serde output, plus the
//!   stable JSON DTOs.
//! - [`commands`]: one module per subcommand group.

// Binary crate: `unwrap`/`expect` are allowed in `#[cfg(test)]` modules;
// production paths return `Failure`/`anyhow` errors instead.
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod cli;
mod commands;
mod context;
mod output;
#[cfg(all(test, unix))]
mod testutil;

use std::process::ExitCode;

use clap::Parser;

use context::{Ctx, Failure};

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    init_tracing(cli.globals.verbose);

    let globals = cli.globals();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            emit_failure(globals, &failure);
            ExitCode::from(failure.code.min(255) as u8)
        }
    }
}

fn init_tracing(verbose: bool) {
    let filter = if verbose {
        tracing_subscriber::EnvFilter::new("debug")
    } else {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"))
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

/// Process-environment lookup matching [`herdr_plugin::paths::EnvLookup`].
fn proc_env(key: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(key)
}

fn run(cli: &cli::Cli) -> Result<(), Failure> {
    let globals = cli.globals();
    let ctx = Ctx::from_globals(globals)?;

    match &cli.command {
        cli::Command::Whoami => commands::whoami::run(&ctx, &proc_env),
        cli::Command::Peers => commands::peers::run(&ctx, &proc_env),
        cli::Command::Send { peer, text, thread } => {
            commands::send::run(&ctx, &proc_env, peer, text, thread.as_deref())
        }
        cli::Command::Broadcast { text } => commands::broadcast::run(&ctx, &proc_env, text),
        cli::Command::Inbox {
            unread,
            follow,
            action,
        } => commands::inbox::run(&ctx, &proc_env, *unread, *follow, action.as_ref()),
        cli::Command::Reply { target, text } => commands::reply::run(&ctx, &proc_env, target, text),
        cli::Command::Rescue { id, action } => commands::rescue::run(&ctx, &proc_env, id, action),
        cli::Command::Threads => commands::threads::run(&ctx, &proc_env),
        cli::Command::Message { action } => commands::message::run(&ctx, &proc_env, action),
        cli::Command::Daemon { action } => commands::daemon::run(&ctx, action),
        cli::Command::Doctor { project } => {
            commands::doctor::run(&ctx, &proc_env, project.as_deref())
        }
        cli::Command::Install {
            global,
            agents,
            project,
        } => commands::install::run(
            &ctx,
            InstallMode::Install,
            *global,
            agents,
            project.as_deref(),
        ),
        cli::Command::Uninstall {
            global,
            agents,
            project,
        } => commands::install::run(
            &ctx,
            InstallMode::Uninstall,
            *global,
            agents,
            project.as_deref(),
        ),
        cli::Command::Version => commands::version::run(&ctx),
    }
}

pub enum InstallMode {
    Install,
    Uninstall,
}

/// Render a failure on stderr — or as a JSON error object under `--json`.
fn emit_failure(globals: &cli::Globals, failure: &Failure) {
    if globals.json {
        let payload = serde_json::json!({
            "error": {
                "kind": failure.kind,
                "message": failure.message,
            }
        });
        println!("{}", serde_json::to_string(&payload).unwrap_or_default());
    } else {
        eprintln!("herdr-pool: {}", failure.message);
    }
}
