//! `herdr-pool doctor [--project DIR] [--json]` — read-only diagnostics.
//!
//! Checks: `HERDR_*` pane env, socket `ping` (version/protocol), scope
//! resolution, pool db migration state, router daemon liveness, agent skill
//! dirs, and version alignment (CLI vs plugin manifest vs skill payload).
//! Nothing is mutated; `--fix` is intentionally not implemented (MVP).

use std::path::{Path, PathBuf};

use herdr_plugin::manifest::Manifest;
use herdr_plugin::paths::EnvLookup;
use herdr_plugin::skill::{self, SkillScope, TargetStatus};
use herdr_router::{daemon_status, DaemonStatus};
use serde::Serialize;

use crate::context::{open_identity_pool, resolve_identity, CmdResult, Ctx, Failure};
use crate::output::emit_json;

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum CheckStatus {
    Ok,
    Warn,
    Fail,
}

impl CheckStatus {
    fn label(self) -> &'static str {
        match self {
            CheckStatus::Ok => "ok",
            CheckStatus::Warn => "warn",
            CheckStatus::Fail => "fail",
        }
    }
}

#[derive(Serialize)]
struct Check {
    name: String,
    status: CheckStatus,
    detail: String,
}

impl Check {
    fn new(name: &str, status: CheckStatus, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            status,
            detail: detail.into(),
        }
    }
}

#[derive(Serialize)]
struct DoctorJson {
    ok: bool,
    checks: Vec<Check>,
}

pub fn run(ctx: &Ctx, env: &EnvLookup<'_>, project: Option<&Path>) -> CmdResult<()> {
    let mut checks: Vec<Check> = Vec::new();

    // -- env ----------------------------------------------------------------
    let herdr_env = env("HERDR_ENV").map(|v| v.to_string_lossy().into_owned());
    checks.push(match &herdr_env {
        Some(v) => Check::new("env.HERDR_ENV", CheckStatus::Ok, format!("HERDR_ENV={v}")),
        None => Check::new(
            "env.HERDR_ENV",
            CheckStatus::Warn,
            "HERDR_ENV not set (expected inside Herdr panes)",
        ),
    });

    let pane_env = herdr_pool_core::self_env_from(env);
    checks.push(match &pane_env {
        Ok(ids) => Check::new(
            "env.pane",
            CheckStatus::Ok,
            format!(
                "handle {} (workspace {}, tab {})",
                ids.handle,
                ids.workspace_id.as_deref().unwrap_or("-"),
                ids.tab_id.as_deref().unwrap_or("-")
            ),
        ),
        Err(err) => Check::new("env.pane", CheckStatus::Fail, err.to_string()),
    });

    // -- socket --------------------------------------------------------------
    match ctx.client.ping() {
        Ok(pong) => checks.push(Check::new(
            "socket.ping",
            CheckStatus::Ok,
            format!("version {} protocol {}", pong.version, pong.protocol),
        )),
        Err(err) => checks.push(Check::new(
            "socket.ping",
            CheckStatus::Fail,
            format!("{} ({})", err, ctx.client.socket_path().display()),
        )),
    }

    // -- identity + scope -----------------------------------------------------
    let identity = resolve_identity(ctx, env).ok();
    match &identity {
        Some(id) => checks.push(Check::new(
            "scope",
            CheckStatus::Ok,
            format!(
                "scope_key {} <- {}",
                id.scope.scope_key,
                id.canonical_path.display()
            ),
        )),
        None => checks.push(Check::new(
            "scope",
            CheckStatus::Fail,
            "cannot resolve self (missing HERDR_* env or socket error)",
        )),
    }

    // -- pool db ---------------------------------------------------------------
    let project_root = project
        .map(Path::to_path_buf)
        .or_else(|| identity.as_ref().map(|i| i.canonical_path.clone()))
        .or_else(|| std::env::current_dir().ok());

    match &identity {
        Some(id) => match open_identity_pool(ctx, id) {
            Ok(pool) => {
                checks.push(Check::new(
                    "pool.db",
                    CheckStatus::Ok,
                    format!("migrated: {}", pool.pool.path().display()),
                ));
            }
            Err(err) => checks.push(Check::new("pool.db", CheckStatus::Fail, err.message)),
        },
        None => {
            // No identity: still verify the state dir/index is usable by
            // opening a pool for the project root if we have one.
            match &project_root {
                Some(root) => match herdr_pool_core::open_pool(&ctx.state_dir, root) {
                    Ok(pool) => checks.push(Check::new(
                        "pool.db",
                        CheckStatus::Ok,
                        format!("migrated: {}", pool.pool.path().display()),
                    )),
                    Err(err) => {
                        checks.push(Check::new("pool.db", CheckStatus::Fail, format!("{err}")))
                    }
                },
                None => checks.push(Check::new(
                    "pool.db",
                    CheckStatus::Fail,
                    "no project root to open a pool for",
                )),
            }
        }
    }

    // -- daemon -----------------------------------------------------------------
    match daemon_status(&ctx.state_dir) {
        DaemonStatus::Running { pid } => checks.push(Check::new(
            "daemon",
            CheckStatus::Ok,
            format!("herdr-routerd running (pid {pid})"),
        )),
        DaemonStatus::Stopped => checks.push(Check::new(
            "daemon",
            CheckStatus::Warn,
            "herdr-routerd not running (CLI delivery still works; retries need the daemon)",
        )),
    }

    // -- skills -----------------------------------------------------------------
    if let Some(root) = &project_root {
        for scope in [SkillScope::Project, SkillScope::Global] {
            match skill::skill_status(root, scope) {
                Ok(statuses) => {
                    let label = match scope {
                        SkillScope::Project => "skills.project",
                        SkillScope::Global => "skills.global",
                    };
                    let mut installed = 0usize;
                    let mut outdated = 0usize;
                    let mut modified = 0usize;
                    let mut unmanaged = 0usize;
                    let mut missing = 0usize;
                    let mut detail_parts = Vec::new();
                    for s in &statuses {
                        match s {
                            TargetStatus::Installed { version, .. } => {
                                installed += 1;
                                detail_parts.push(format!("installed v{version}"));
                            }
                            TargetStatus::Outdated { version, .. } => {
                                outdated += 1;
                                detail_parts.push(format!("outdated v{version}"));
                            }
                            TargetStatus::Modified { version, .. } => {
                                modified += 1;
                                detail_parts.push(format!("modified v{version}"));
                            }
                            TargetStatus::Unmanaged { .. } => {
                                unmanaged += 1;
                                detail_parts.push("unmanaged dir".to_string());
                            }
                            TargetStatus::Missing { .. } => missing += 1,
                        }
                    }
                    let status = if unmanaged > 0 || modified > 0 || outdated > 0 {
                        CheckStatus::Warn
                    } else if installed > 0 {
                        CheckStatus::Ok
                    } else {
                        CheckStatus::Warn
                    };
                    let detail = if detail_parts.is_empty() {
                        "skill not installed".to_string()
                    } else {
                        format!("{} ({} missing)", detail_parts.join(", "), missing)
                    };
                    checks.push(Check::new(label, status, detail));
                }
                Err(err) => checks.push(Check::new(
                    "skills",
                    CheckStatus::Warn,
                    format!("skill_status failed: {err}"),
                )),
            }
        }
    } else {
        checks.push(Check::new(
            "skills",
            CheckStatus::Warn,
            "no project root; skill dirs not checked",
        ));
    }

    // -- versions -----------------------------------------------------------------
    let cli_version = env!("CARGO_PKG_VERSION");
    let mut version_detail = format!(
        "herdr-pool {cli_version}, skill payload v{}",
        skill::SKILL_VERSION
    );
    let mut version_status = CheckStatus::Ok;
    match find_plugin_manifest() {
        Some(path) => match Manifest::load(&path) {
            Ok(manifest) => {
                version_detail.push_str(&format!(
                    ", plugin manifest v{} ({})",
                    manifest.version,
                    path.display()
                ));
                if manifest.version != cli_version {
                    version_status = CheckStatus::Warn;
                    version_detail.push_str(" — version mismatch");
                }
            }
            Err(err) => {
                version_status = CheckStatus::Warn;
                version_detail.push_str(&format!(", manifest unreadable: {err}"));
            }
        },
        None => {
            version_status = CheckStatus::Warn;
            version_detail.push_str(", herdr-plugin.toml not found");
        }
    }
    checks.push(Check::new("versions", version_status, version_detail));

    // -- report ---------------------------------------------------------------------
    let ok = !checks.iter().any(|c| c.status == CheckStatus::Fail);
    if ctx.json {
        emit_json(&DoctorJson { ok, checks });
    } else {
        for c in &checks {
            println!("{:<4} {:<16} {}", c.status.label(), c.name, c.detail);
        }
        println!("{}", if ok { "doctor: ok" } else { "doctor: FAILED" });
    }
    if !ok {
        return Err(Failure::runtime("doctor found failing checks"));
    }
    Ok(())
}

/// Locate `herdr-plugin.toml`: walk ancestors of the current exe (covers
/// `target/debug/herdr-pool` → repo root) then of the cwd.
fn find_plugin_manifest() -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            roots.extend(dir.ancestors().map(Path::to_path_buf));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        roots.extend(cwd.ancestors().map(Path::to_path_buf));
    }
    for root in roots {
        let candidate = root.join("herdr-plugin.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
