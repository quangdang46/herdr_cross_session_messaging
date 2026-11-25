//! `herdr-pool daemon <start|stop|status|logs>` — wrapper around
//! `herdr-routerd`.
//!
//! The router daemon owns the event loop, registry sync, and outbox
//! delivery worker; this command only locates the binary (sibling of this
//! executable, then `PATH`), forwards the subcommand, and propagates
//! `--state-dir` via `HERDR_POOL_STATE_DIR`.
//!
//! `logs` has no routerd counterpart: it prints `<state>/logs/routerd.log`
//! (path always; last 80 lines on stderr→stdout tail).

use std::io::Read;
use std::path::PathBuf;
use std::process::Stdio;

use herdr_router::{daemon_status, DaemonStatus, LOG_FILE_NAME};
use serde::Serialize;

use crate::cli::DaemonAction;
use crate::context::{routerd_bin, CmdResult, Ctx, Failure};
use crate::output::emit_json;

#[derive(Serialize)]
struct DaemonStatusJson {
    running: bool,
    pid: Option<u32>,
}

#[derive(Serialize)]
struct DaemonAckJson {
    ok: bool,
    pid: Option<u32>,
}

#[derive(Serialize)]
struct DaemonLogsJson {
    path: String,
    lines: Vec<String>,
}

pub fn run(ctx: &Ctx, action: &DaemonAction) -> CmdResult<()> {
    match action {
        DaemonAction::Status => status(ctx),
        DaemonAction::Logs => logs(ctx),
        DaemonAction::Start | DaemonAction::Stop => forward(ctx, action),
    }
}

fn status(ctx: &Ctx) -> CmdResult<()> {
    match daemon_status(&ctx.state_dir) {
        DaemonStatus::Running { pid } => {
            if ctx.json {
                emit_json(&DaemonStatusJson {
                    running: true,
                    pid: Some(pid),
                });
            } else {
                println!("herdr-routerd running (pid {pid})");
                println!("state dir: {}", ctx.state_dir.display());
            }
        }
        DaemonStatus::Stopped => {
            if ctx.json {
                emit_json(&DaemonStatusJson {
                    running: false,
                    pid: None,
                });
            } else {
                println!("herdr-routerd is not running");
            }
        }
    }
    Ok(())
}

fn logs(ctx: &Ctx) -> CmdResult<()> {
    let path: PathBuf = ctx.state_dir.join("logs").join(LOG_FILE_NAME);
    let lines = read_tail(&path, 80);
    if ctx.json {
        emit_json(&DaemonLogsJson {
            path: path.to_string_lossy().into_owned(),
            lines,
        });
    } else {
        println!("log: {}", path.display());
        if lines.is_empty() {
            println!("(log file is empty or missing)");
        } else {
            for line in &lines {
                println!("{line}");
            }
        }
    }
    Ok(())
}

/// Last `n` lines of `path` (best effort; missing file → empty).
fn read_tail(path: &PathBuf, n: usize) -> Vec<String> {
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut buf = String::new();
    if file.read_to_string(&mut buf).is_err() {
        return Vec::new();
    }
    let all: Vec<&str> = buf.lines().collect();
    let start = all.len().saturating_sub(n);
    all[start..].iter().map(|s| s.to_string()).collect()
}

fn forward(ctx: &Ctx, action: &DaemonAction) -> CmdResult<()> {
    let bin = routerd_bin().ok_or_else(|| {
        Failure::runtime(
            "herdr-routerd binary not found (checked sibling of this executable and PATH)",
        )
    })?;
    let sub = match action {
        DaemonAction::Start => "start",
        DaemonAction::Stop => "stop",
        _ => unreachable!(),
    };
    let status = std::process::Command::new(bin)
        .arg(sub)
        .env("HERDR_POOL_STATE_DIR", &ctx.state_dir)
        .stdin(Stdio::null())
        .status()
        .map_err(|e| Failure::runtime(format!("spawn herdr-routerd {sub}: {e}")))?;
    if !status.success() {
        return Err(Failure::runtime(format!(
            "herdr-routerd {sub} exited with {status}"
        )));
    }
    if ctx.json {
        // Re-inspect state after start/stop for a stable JSON ack.
        let pid = match daemon_status(&ctx.state_dir) {
            DaemonStatus::Running { pid } => Some(pid),
            DaemonStatus::Stopped => None,
        };
        emit_json(&DaemonAckJson { ok: true, pid });
    }
    Ok(())
}
