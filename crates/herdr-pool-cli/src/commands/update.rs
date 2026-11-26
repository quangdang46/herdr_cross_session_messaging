//! `herdr-pool update` — self-update the plugin installation.
//!
//! GitHub-installed plugins: `herdr plugin update --yes <plugin>` refetches
//! and rebuilds via the manifest's `[[build]]` commands (`cargo install`
//! fresh `herdr-pool`/`herdr-routerd` binaries). Locally linked plugins
//! (`source.kind = "local"`) cannot use that path, so the command runs
//! `git pull --ff-only` in the plugin checkout followed by the same
//! `cargo install --path` builds the manifest declares.
//!
//! Either way the tail is identical: reinstall the embedded
//! `herdr-messaging` skill with the *new* binary, restart the router
//! daemon if it was running, and report before/after versions.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Serialize;

use crate::context::{routerd_bin, CmdResult, Ctx, Failure};
use crate::output::emit_json;

#[derive(Serialize)]
struct UpdateJson {
    plugin: String,
    source_kind: Option<String>,
    version_before: String,
    version_after: Option<String>,
    skills_project: String,
    skills_global: String,
    daemon: String,
}

/// What `herdr plugin list --json` told us about the plugin.
struct PluginInfo {
    kind: String,
    root: Option<PathBuf>,
}

pub fn run(ctx: &Ctx, plugin: &str, global: bool) -> CmdResult<()> {
    let herdr = which("herdr").ok_or_else(|| {
        Failure::runtime("herdr CLI not found on PATH (required for `herdr plugin update`)")
    })?;

    let version_before = format!(
        "{} (skill payload v{})",
        env!("CARGO_PKG_VERSION"),
        herdr_plugin::skill::SKILL_VERSION
    );

    let info = plugin_info(&herdr, plugin);
    let source_kind = info.as_ref().map(|i| i.kind.clone());

    match info.as_ref().map(|i| i.kind.as_str()) {
        // Local link: Herdr refuses `plugin update`; pull + rebuild instead.
        Some("local") => {
            let root = info.as_ref().and_then(|i| i.root.clone()).ok_or_else(|| {
                Failure::runtime(format!(
                    "plugin {plugin} is locally linked but its checkout path is unknown; \
                         run `git pull` in the plugin directory and `cargo install --path \
                         crates/herdr-pool-cli --locked` + `--path crates/herdr-routerd` manually"
                ))
            })?;
            local_update(&root, ctx.json)?;
        }
        // GitHub-managed (or unknown — let `plugin update` report).
        _ => github_update(&herdr, plugin, ctx.json)?,
    }

    // Reinstall skills with the NEW binary: the update replaced the file
    // at current_exe, so spawning ourselves runs the updated code (and
    // its updated embedded payload).
    let self_bin = std::env::current_exe()
        .map_err(|e| Failure::runtime(format!("resolve own executable: {e}")))?;
    let skills_project = run_self(&self_bin, &["install"], ctx.json);
    let skills_global = if global {
        run_self(&self_bin, &["install", "--global"], ctx.json)
    } else {
        "skipped".to_string()
    };

    // Restart the daemon only if it was running — a stopped daemon stays
    // stopped and picks up the new binary on its next start.
    let daemon = restart_daemon_if_running(ctx);

    let version_after = self_version(&self_bin);

    if ctx.json {
        emit_json(&UpdateJson {
            plugin: plugin.to_string(),
            source_kind,
            version_before,
            version_after: version_after.clone(),
            skills_project,
            skills_global,
            daemon,
        });
    } else {
        println!("plugin update: ok");
        if let Some(v) = &version_after {
            println!("herdr-pool: {version_before} -> {v}");
        }
        println!("skills: project={skills_project} global={skills_global}");
        println!("daemon: {daemon}");
    }
    Ok(())
}

/// `herdr plugin update` for GitHub-managed installs. Streams build
/// output live unless `--json`.
fn github_update(herdr: &Path, plugin: &str, json: bool) -> CmdResult<()> {
    let update = Command::new(herdr)
        .args(["plugin", "update", "--yes", plugin])
        .stdin(Stdio::null())
        .stdout(if json {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .stderr(if json {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .output()
        .map_err(|e| Failure::runtime(format!("spawn `herdr plugin update`: {e}")))?;
    if json {
        eprint!(
            "{}{}",
            String::from_utf8_lossy(&update.stdout),
            String::from_utf8_lossy(&update.stderr)
        );
    }
    if !update.status.success() {
        let stderr = String::from_utf8_lossy(&update.stderr);
        let tail = stderr.lines().last().unwrap_or("");
        return Err(Failure::runtime(format!(
            "herdr plugin update {plugin} failed ({}){}",
            update.status,
            if tail.is_empty() {
                String::new()
            } else {
                format!(": {tail}")
            }
        )));
    }
    Ok(())
}

/// Update a locally linked plugin: fast-forward the checkout, then run
/// the same `cargo install --path` builds the manifest's `[[build]]`
/// section declares (paths are relative to the plugin root).
fn local_update(root: &Path, json: bool) -> CmdResult<()> {
    run_streamed(
        "git",
        &["-C", &root.to_string_lossy(), "pull", "--ff-only"],
        root,
        json,
        "git pull",
    )?;
    for crate_dir in ["crates/herdr-pool-cli", "crates/herdr-routerd"] {
        if !root.join(crate_dir).join("Cargo.toml").is_file() {
            continue;
        }
        run_streamed(
            "cargo",
            &["install", "--path", crate_dir, "--locked"],
            root,
            json,
            "cargo install",
        )?;
    }
    Ok(())
}

/// Spawn a command with live stdio (or captured under `--json`) and map
/// failure to a [`Failure`].
fn run_streamed(
    program: &str,
    args: &[&str],
    cwd: &Path,
    json: bool,
    label: &str,
) -> CmdResult<()> {
    let out = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(if json {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .stderr(if json {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .output()
        .map_err(|e| Failure::runtime(format!("spawn {label}: {e}")))?;
    if json {
        eprint!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    if !out.status.success() {
        return Err(Failure::runtime(format!("{label} failed ({})", out.status)));
    }
    Ok(())
}

/// Locate an executable on PATH.
fn which(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// `source.kind` + `plugin_root` of the plugin from
/// `herdr plugin list --json`, or `None` when the plugin is not listed /
/// output is unparseable.
fn plugin_info(herdr: &Path, plugin: &str) -> Option<PluginInfo> {
    let out = Command::new(herdr)
        .args(["plugin", "list", "--json"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let plugins = v.pointer("/result/plugins")?.as_array()?;
    for p in plugins {
        // Match on plugin_id, or on any string field inside `source`
        // (github installs are addressed as `owner/repo[/subdir]`).
        let mut candidates: Vec<&str> = Vec::new();
        if let Some(id) = p.get("plugin_id").and_then(|s| s.as_str()) {
            candidates.push(id);
        }
        if let Some(obj) = p.get("source").and_then(|s| s.as_object()) {
            for v in obj.values() {
                if let Some(s) = v.as_str() {
                    candidates.push(s);
                }
            }
        }
        if !candidates.contains(&plugin) {
            continue;
        }
        let kind = p
            .pointer("/source/kind")
            .and_then(|k| k.as_str())
            .unwrap_or("unknown")
            .to_string();
        let root = p
            .get("plugin_root")
            .and_then(|r| r.as_str())
            .map(PathBuf::from);
        return Some(PluginInfo { kind, root });
    }
    None
}

/// Spawn `self_bin <args>` capturing output; returns "ok"/"failed".
/// Child output is replayed to stderr under `--json`, stdout otherwise.
fn run_self(self_bin: &PathBuf, args: &[&str], json: bool) -> String {
    match Command::new(self_bin)
        .args(args)
        .stdin(Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            if json {
                eprint!("{text}");
            } else {
                print!("{text}");
            }
            "ok".to_string()
        }
        Ok(o) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            eprint!("{text}");
            "failed".to_string()
        }
        Err(_) => "failed".to_string(),
    }
}

/// `herdr-pool --json version` → `version` field, best effort.
fn self_version(self_bin: &PathBuf) -> Option<String> {
    let out = Command::new(self_bin)
        .args(["--json", "version"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    v.get("version")?.as_str().map(str::to_string)
}

/// Stop+start the router daemon iff it is currently running. Returns a
/// short status string for reporting.
fn restart_daemon_if_running(ctx: &Ctx) -> String {
    use herdr_router::{daemon_status, DaemonStatus};
    if !matches!(daemon_status(&ctx.state_dir), DaemonStatus::Running { .. }) {
        return "not_running".to_string();
    }
    let Some(bin) = routerd_bin() else {
        return "restart_failed(no binary)".to_string();
    };
    for sub in ["stop", "start"] {
        let ok = Command::new(&bin)
            .arg(sub)
            .env("HERDR_POOL_STATE_DIR", &ctx.state_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return format!("restart_failed({sub})");
        }
    }
    // `start` returns before the daemon writes its pidfile — poll briefly
    // instead of reporting a false failure.
    for _ in 0..30 {
        if matches!(daemon_status(&ctx.state_dir), DaemonStatus::Running { .. }) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    match daemon_status(&ctx.state_dir) {
        DaemonStatus::Running { pid } => format!("restarted(pid {pid})"),
        DaemonStatus::Stopped => "restart_failed(still stopped)".to_string(),
    }
}
