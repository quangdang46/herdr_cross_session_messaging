//! `herdr-pool install|uninstall [--global] [--agent KIND]... [--project DIR]`.
//!
//! Thin wrapper over [`herdr_plugin::skill`]: copies the embedded
//! `herdr-messaging` skill payload into per-agent skill dirs
//! (`.agents/skills/` + `.claude/skills/`), guarded by the managed marker.
//!
//! `--agent` is accepted for forward compatibility: the installer covers
//! every supported agent dir at once (`.agents` serves Codex/Gemini/
//! OpenCode/Cursor, `.claude` serves Claude), so there is nothing to filter.
//! `--project` selects the project root (default: process cwd — for
//! non-Herdr usage; inside Herdr prefer the agent's launch cwd via the
//! pool commands).

use std::path::{Path, PathBuf};

use herdr_plugin::skill::{self, SkillScope};
use serde::Serialize;

use crate::context::{CmdResult, Ctx, Failure};
use crate::output::emit_json;
use crate::InstallMode;

#[derive(Serialize)]
struct TargetJson {
    path: String,
    outcome: String,
    reason: Option<String>,
}

#[derive(Serialize)]
struct SkillJson {
    action: String,
    scope: String,
    targets: Vec<TargetJson>,
    all_ok: bool,
}

pub fn run(
    ctx: &Ctx,
    mode: InstallMode,
    global: bool,
    agents: &[String],
    project: Option<&Path>,
) -> CmdResult<()> {
    let scope = if global {
        SkillScope::Global
    } else {
        SkillScope::Project
    };
    let root = resolve_root(project)?;

    if !agents.is_empty() {
        eprintln!(
            "note: --agent is accepted for compatibility; the installer covers \
             all supported agent skill dirs (.agents + .claude) at once"
        );
    }

    match mode {
        InstallMode::Install => {
            let outcome = skill::install_skill(&root, scope)?;
            report(
                ctx,
                "install",
                scope,
                &root,
                outcome
                    .targets
                    .iter()
                    .map(|t| {
                        let (outcome, reason) = match t {
                            skill::TargetOutcome::Installed { .. } => ("installed", None),
                            skill::TargetOutcome::Updated { .. } => ("updated", None),
                            skill::TargetOutcome::AlreadyInstalled { .. } => {
                                ("already_installed", None)
                            }
                            skill::TargetOutcome::Conflict { reason, .. } => {
                                ("conflict", Some(reason.clone()))
                            }
                        };
                        TargetJson {
                            path: t.path().to_string_lossy().into_owned(),
                            outcome: outcome.to_string(),
                            reason,
                        }
                    })
                    .collect::<Vec<_>>(),
                outcome.all_installed(),
            )
        }
        InstallMode::Uninstall => {
            let outcomes = skill::uninstall_skill(&root, scope)?;
            let all_ok = outcomes.iter().all(|o| {
                matches!(
                    o,
                    skill::UninstallOutcome::Removed { .. }
                        | skill::UninstallOutcome::Missing { .. }
                )
            });
            report(
                ctx,
                "uninstall",
                scope,
                &root,
                outcomes
                    .iter()
                    .map(|o| {
                        let outcome = match o {
                            skill::UninstallOutcome::Removed { .. } => "removed",
                            skill::UninstallOutcome::Skipped { .. } => "skipped_unmanaged",
                            skill::UninstallOutcome::Missing { .. } => "missing",
                        };
                        TargetJson {
                            path: o.path().to_string_lossy().into_owned(),
                            outcome: outcome.to_string(),
                            reason: None,
                        }
                    })
                    .collect(),
                all_ok,
            )
        }
    }
}

fn resolve_root(project: Option<&Path>) -> CmdResult<PathBuf> {
    let raw = match project {
        Some(p) => p.to_path_buf(),
        None => {
            std::env::current_dir().map_err(|e| Failure::runtime(format!("resolve cwd: {e}")))?
        }
    };
    Ok(herdr_pool_core::canonicalize_path(&raw))
}

fn report(
    ctx: &Ctx,
    action: &str,
    scope: SkillScope,
    root: &Path,
    targets: Vec<TargetJson>,
    all_ok: bool,
) -> CmdResult<()> {
    let scope_name = match scope {
        SkillScope::Project => "project",
        SkillScope::Global => "global",
    };
    if ctx.json {
        emit_json(&SkillJson {
            action: action.to_string(),
            scope: scope_name.to_string(),
            targets,
            all_ok,
        });
    } else {
        for t in &targets {
            let suffix = t
                .reason
                .as_deref()
                .map(|r| format!(" — {r}"))
                .unwrap_or_default();
            println!("{:<18} {}{}", t.outcome, t.path, suffix);
        }
        println!("scope: {scope_name} ({})", root.display());
    }
    if !all_ok {
        return Err(Failure::runtime(format!(
            "{action} finished with conflicts or skipped unmanaged dirs"
        )));
    }
    Ok(())
}
