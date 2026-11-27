//! `herdr-routerd` — router daemon lifecycle binary.
//!
//! Subcommands:
//!
//! * `start` — spawn `herdr-routerd run` detached (log →
//!   `<state>/logs/routerd.log`). No-op when already running.
//! * `stop` — write the shutdown sentinel + SIGTERM the pid; the daemon
//!   finishes in-flight work and exits.
//! * `status` — print pid / state / uptime.
//! * `tick` — one-shot snapshot + registry reconcile (plugin `[[events]]`
//!   hook / manual diagnostic; no daemon required).
//! * `run` — foreground daemon loop (what `start` detaches into).
//!
//! State dir resolution is shared with the plugin: `HERDR_POOL_STATE_DIR`
//! → XDG → `~/.local/state/herdr-cross-session-messaging` (see
//! `herdr_plugin::paths::state_dir`). `HERDR_PLUGIN_STATE_DIR` is ignored so
//! plugin and agent processes share one pool. The agent runtime backend is
//! selected by `HERDR_POOL_RUNTIME` (`herdr` default, `hc` opt-in — same
//! flag the `herdr-pool` CLI reads); the Herdr socket comes from the
//! environment (`HERDR_SOCKET_PATH` / `HERDR_SESSION` / XDG config) and the
//! hc backend honors `HC_CONNECT_CMD`.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use herdr_plugin::paths::state_dir as resolve_state_dir;
use herdr_plugin::skill::{self, SkillScope};
use herdr_router::{
    clear_stop_sentinel, daemon_status, pid_alive, read_pid_file, registry_sync::run_tick,
    remove_pid_file, sigterm, write_pid_file, write_stop_sentinel, Daemon, DaemonConfig,
    DaemonStatus, InstanceGuard, LOG_FILE_NAME,
};
use herdr_runtime::{connect_runtime, runtime_kind_from_env};

#[derive(Parser)]
#[command(
    name = "herdr-routerd",
    version,
    about = "Router daemon for herdr-cross-session-messaging",
    after_help = "Environment:\n  \
        HERDR_POOL_RUNTIME    agent backend: herdr (default) | hc\n  \
        HERDR_POOL_STATE_DIR  state dir override\n  \
        HERDR_SOCKET_PATH     Herdr socket path (herdr backend)\n  \
        HC_CONNECT_CMD        hc connect argv override (hc backend)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the daemon (detached; logs to <state>/logs/routerd.log).
    Start,
    /// Stop the running daemon (sentinel + SIGTERM, graceful).
    Stop,
    /// Show daemon status.
    Status,
    /// Run one reconcile pass (used by plugin event hooks).
    Tick,
    /// Run the event loop in the foreground.
    Run,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
    match cli.command {
        Command::Start => cmd_start(),
        Command::Stop => cmd_stop(),
        Command::Status => cmd_status(),
        Command::Tick => cmd_tick(),
        Command::Run => cmd_run(),
    }
}

fn state_dir() -> Result<PathBuf> {
    resolve_state_dir().context("resolve plugin state dir")
}

fn cmd_start() -> Result<()> {
    let state_dir = state_dir()?;
    std::fs::create_dir_all(state_dir.join("logs"))?;
    match daemon_status(&state_dir) {
        DaemonStatus::Running { pid } => {
            println!("herdr-routerd already running (pid {pid})");
            return Ok(());
        }
        DaemonStatus::Stopped => {}
    }

    // Detached child: `herdr-routerd run` acquires the instance lock and
    // writes the pidfile itself, so `status`/`stop` observe the real daemon
    // pid. stdout/stderr go to the log file; stdin is closed.
    let log_path = state_dir.join("logs").join(LOG_FILE_NAME);
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open log {}", log_path.display()))?;
    let exe = std::env::current_exe().context("locate herdr-routerd binary")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("run")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log.try_clone()?));
    // New process group so terminal signals (Ctrl-C) do not propagate.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd.spawn().context("spawn herdr-routerd run")?;
    writeln!(
        log,
        "--- herdr-routerd start (child pid {}) ---",
        child.id()
    )?;
    println!("herdr-routerd started (pid {})", child.id());
    println!("log: {}", log_path.display());
    Ok(())
}

fn cmd_stop() -> Result<()> {
    let state_dir = state_dir()?;
    let DaemonStatus::Running { pid } = daemon_status(&state_dir) else {
        println!("herdr-routerd is not running");
        return Ok(());
    };
    // Sentinel first (authoritative graceful path), then SIGTERM as a
    // best-effort accelerator. The daemon polls both.
    write_stop_sentinel(&state_dir)?;
    sigterm(pid);
    println!("stop requested for herdr-routerd (pid {pid})");

    // Wait briefly for a clean exit so scripts see a stopped daemon.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !pid_alive(pid) {
            println!("herdr-routerd stopped");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("herdr-routerd is still shutting down (pid {pid})");
    Ok(())
}

fn cmd_status() -> Result<()> {
    let state_dir = state_dir()?;
    match daemon_status(&state_dir) {
        DaemonStatus::Running { pid } => {
            let uptime = read_pid_file(&state_dir)
                .and_then(|(_, started)| started)
                .map(|started| {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    now.saturating_sub(started)
                });
            match uptime {
                Some(secs) => println!("herdr-routerd running (pid {pid}, uptime {secs}s)"),
                None => println!("herdr-routerd running (pid {pid})"),
            }
        }
        DaemonStatus::Stopped => println!("herdr-routerd is not running"),
    }
    Ok(())
}

fn cmd_tick() -> Result<()> {
    let state_dir = state_dir()?;
    let env = |key: &str| std::env::var_os(key);
    let kind = runtime_kind_from_env(&env).map_err(anyhow::Error::msg)?;
    let connected = connect_runtime(kind, None, &env)?;
    let bootstrap_enabled = herdr_plugin::config::PoolConfig::load().bootstrap_enabled;
    let synced = run_tick(
        connected.runtime.as_ref(),
        connected.herdr_client.clone(),
        &state_dir,
        bootstrap_enabled,
    )?;
    println!("tick: synced {synced} agents (runtime: {})", kind.as_str());
    Ok(())
}

/// Best-effort install of the `herdr-messaging` skill into the user's global
/// agent skill dirs (`~/.agents/skills`, `~/.claude/skills`) so agents pick it
/// up without a manual `herdr-pool install`. Idempotent and conflict-safe:
/// user-owned or modified dirs are left untouched and only logged. Runs on
/// every daemon start so skill payload updates propagate automatically.
fn ensure_skill_installed() {
    // `SkillScope::Global` ignores `target_root`; pass an empty path.
    match skill::install_skill(std::path::Path::new(""), SkillScope::Global) {
        Ok(outcome) => {
            for target in &outcome.targets {
                match target {
                    skill::TargetOutcome::Installed { path } => {
                        tracing::info!(path = %path.display(), "skill installed (global)");
                    }
                    skill::TargetOutcome::Updated { path } => {
                        tracing::info!(path = %path.display(), "skill updated (global)");
                    }
                    skill::TargetOutcome::Conflict { path, reason } => {
                        tracing::warn!(
                            path = %path.display(),
                            reason = %reason,
                            "skill install conflict; leaving user-owned dir untouched"
                        );
                    }
                    skill::TargetOutcome::AlreadyInstalled { .. } => {}
                }
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "global skill install failed; agents may lack the herdr-messaging skill");
        }
    }
}

fn cmd_run() -> Result<()> {
    let state_dir = state_dir()?;
    std::fs::create_dir_all(&state_dir)?;
    // Single instance: bail if another daemon holds the lock.
    let _guard = InstanceGuard::acquire(&state_dir)?;
    clear_stop_sentinel(&state_dir);
    write_pid_file(&state_dir, std::process::id())?;

    ensure_skill_installed();

    // from_env_at (not for_state_dir) so HERDR_POOL_NOTIFY_DELIVERY,
    // HERDR_POOL_RUNTIME, and the config.toml bootstrap toggle actually
    // reach the running daemon.
    let config = DaemonConfig::from_env_at(state_dir.clone())?;
    tracing::info!(runtime = %config.runtime_kind.as_str(), "routerd starting");
    let result = Daemon::new(config).and_then(|daemon| daemon.run());
    remove_pid_file(&state_dir);
    if let Err(err) = &result {
        bail!("daemon exited with error: {err}");
    }
    Ok(())
}
