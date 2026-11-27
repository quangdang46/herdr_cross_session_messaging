//! `herdr-pool panel <open|close|toggle|ensure|status>` — left-docked inbox.
//!
//! The panel is a plugin pane (`placement = "split"`, entrypoint `panel`)
//! running `herdr-pool inbox --follow`, docked as a narrow left column of
//! the caller's tab. Herdr has no native sidebar surface, so docking uses
//! the socket API (the same recipe herdr-sidebar uses):
//!
//! 1. `pane.layout` → pick the leftmost-topmost pane and compute the split
//!    share that yields [`PoolConfig::panel_width`] columns.
//! 2. `plugin.pane.open` → Herdr splits the target 50/50 to its right.
//! 3. `pane.swap` → the new pane takes the left slot.
//! 4. `pane.resize` → move the interior edge to the computed share
//!    (`plugin.pane.open` exposes no ratio knob).
//!
//! Per-tab state lives in `<state>/panel/<tab_id>` (the pane id); a missing
//! or stale file is treated as "no panel". `ensure` is the `[[events]]`
//! hook entry point: it no-ops unless `[panel] autostart` is on and never
//! steals focus.

use std::path::{Path, PathBuf};

use herdr_plugin::config::PoolConfig;
use herdr_plugin::paths::EnvLookup;
use herdr_pool_core::identity;
use herdr_protocol::results::ResponseResult;
use herdr_protocol::{
    PaneDirection, PaneInfo, PaneLayoutPane, PaneLayoutSnapshot, PaneResizeParams, PaneSwapParams,
};
use serde::Serialize;

use crate::cli::PanelAction;
use crate::context::{CmdResult, Ctx, Failure};
use crate::output::emit_json;

/// Plugin id this binary is installed under (matches `herdr-plugin.toml`).
pub const PLUGIN_ID: &str = "herdr.cross-session-messaging";
/// `[[panes]]` entrypoint backing the panel.
pub const ENTRYPOINT: &str = "panel";
/// Pane title the manifest gives the panel (fallback label match).
pub const PANEL_TITLE: &str = "Pool";

/// Split share bounds: a panel narrower than 15% or wider than 50% of its
/// column is unusable; same clamp herdr-sidebar applies.
const MIN_SHARE: f64 = 0.15;
const MAX_SHARE: f64 = 0.5;
/// `plugin.pane.open` always splits 50/50; smaller deltas are noise.
const RESIZE_EPSILON: f64 = 0.005;
/// Env var carrying the plugin event payload for `[[events]]` hooks.
const ENV_PLUGIN_EVENT_JSON: &str = "HERDR_PLUGIN_EVENT_JSON";

#[derive(Serialize)]
struct PanelStatusJson {
    open: bool,
    pane_id: Option<String>,
    tab_id: String,
    workspace_id: String,
}

pub fn run(ctx: &Ctx, env: &EnvLookup<'_>, action: &PanelAction) -> CmdResult<()> {
    match action {
        PanelAction::Ensure => ensure_hook(ctx, env),
        PanelAction::Open | PanelAction::Close | PanelAction::Toggle | PanelAction::Status => {
            let pane = context_pane(ctx, env)?
                .ok_or_else(|| Failure::usage("panel commands need a Herdr pane context"))?;
            match action {
                PanelAction::Open => {
                    let id = ensure_open(ctx, &pane, true)?;
                    report(ctx, &pane, Some(&id), true)
                }
                PanelAction::Close => {
                    close_panel(ctx, &pane.tab_id);
                    report(ctx, &pane, None, false)
                }
                PanelAction::Toggle => {
                    if let Some(existing) = find_panel(ctx, &pane.tab_id)? {
                        if tab_focused_pane(ctx, &pane)? == Some(existing.clone()) {
                            close_panel(ctx, &pane.tab_id);
                            report(ctx, &pane, None, false)
                        } else {
                            ctx.client.pane_focus(&existing)?;
                            report(ctx, &pane, Some(&existing), true)
                        }
                    } else {
                        let id = ensure_open(ctx, &pane, true)?;
                        report(ctx, &pane, Some(&id), true)
                    }
                }
                PanelAction::Status => {
                    let panel = find_panel(ctx, &pane.tab_id)?;
                    let open = panel.is_some();
                    report(ctx, &pane, panel.as_deref(), open)
                }
                PanelAction::Ensure => unreachable!(),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Context resolution
// ---------------------------------------------------------------------------

/// The pane whose tab the panel docks into. Interactive commands resolve
/// the caller's own pane (agent env vars or plugin context JSON); `ensure`
/// additionally falls back to the `[[events]]` payload.
fn context_pane(ctx: &Ctx, env: &EnvLookup<'_>) -> CmdResult<Option<PaneInfo>> {
    if let Ok(self_env) = identity::self_env_from(env) {
        match ctx.client.pane_get(&self_env.handle) {
            Ok(pane) => return Ok(Some(pane)),
            Err(err) => {
                tracing::debug!(pane = %self_env.handle, error = %err, "context pane.get failed");
            }
        }
    }
    event_pane(ctx, env)
}

/// Resolve a pane inside the tab named by `HERDR_PLUGIN_EVENT_JSON`
/// (`tab.created` / `workspace.created` hooks). `None` when the payload
/// names no usable tab/workspace.
fn event_pane(ctx: &Ctx, env: &EnvLookup<'_>) -> CmdResult<Option<PaneInfo>> {
    let Some(raw) = env(ENV_PLUGIN_EVENT_JSON)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string_lossy().into_owned())
    else {
        return Ok(None);
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Ok(None);
    };
    let scope = EventScope::find(&value);
    let panes = match (&scope.workspace_id, &scope.tab_id) {
        (Some(ws), _) => ctx.client.pane_list(Some(ws))?,
        (None, Some(_)) => ctx.client.pane_list(None)?,
        (None, None) => return Ok(None),
    };
    Ok(match &scope.tab_id {
        Some(tab) => panes.into_iter().find(|p| p.tab_id == *tab),
        None => panes.into_iter().next(),
    })
}

/// Tab/workspace ids dug out of an event payload (nested objects included —
/// `workspace.created` wraps a `WorkspaceInfo`).
struct EventScope {
    tab_id: Option<String>,
    workspace_id: Option<String>,
}

impl EventScope {
    fn find(value: &serde_json::Value) -> Self {
        fn dig<'a>(v: &'a serde_json::Value, key: &str, out: &mut Vec<&'a str>) {
            match v {
                serde_json::Value::Object(map) => {
                    if let Some(s) = map.get(key).and_then(|v| v.as_str()) {
                        out.push(s);
                    }
                    for child in map.values() {
                        dig(child, key, out);
                    }
                }
                serde_json::Value::Array(items) => {
                    for item in items {
                        dig(item, key, out);
                    }
                }
                _ => {}
            }
        }
        let mut tabs = Vec::new();
        let mut workspaces = Vec::new();
        dig(value, "tab_id", &mut tabs);
        dig(value, "workspace_id", &mut workspaces);
        Self {
            tab_id: tabs.first().map(|s| (*s).to_string()),
            workspace_id: workspaces.first().map(|s| (*s).to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Panel state (per tab)
// ---------------------------------------------------------------------------

fn state_path(state_dir: &Path, tab_id: &str) -> PathBuf {
    state_dir.join("panel").join(format!("{tab_id}.json"))
}

fn read_state(ctx: &Ctx, tab_id: &str) -> Option<String> {
    let text = std::fs::read_to_string(state_path(&ctx.state_dir, tab_id)).ok()?;
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()?
        .get("pane_id")?
        .as_str()
        .map(str::to_string)
}

fn write_state(ctx: &Ctx, tab_id: &str, pane_id: &str) {
    let path = state_path(&ctx.state_dir, tab_id);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body = serde_json::json!({ "pane_id": pane_id, "tab_id": tab_id }).to_string();
    let _ = std::fs::write(path, body);
}

fn clear_state(ctx: &Ctx, tab_id: &str) {
    let _ = std::fs::remove_file(state_path(&ctx.state_dir, tab_id));
}

/// The live panel pane for `tab_id`, verified against the server. Falls
/// back to a label scan when the state file is missing or stale (e.g. the
/// state dir was reset while the pane survived).
fn find_panel(ctx: &Ctx, tab_id: &str) -> CmdResult<Option<String>> {
    if let Some(id) = read_state(ctx, tab_id) {
        match ctx.client.pane_get(&id) {
            Ok(pane) if pane.tab_id == tab_id => return Ok(Some(id)),
            _ => clear_state(ctx, tab_id),
        }
    }
    let panes = ctx.client.pane_list(None)?;
    Ok(panes
        .into_iter()
        .find(|p| p.tab_id == tab_id && p.label.as_deref() == Some(PANEL_TITLE))
        .map(|p| p.pane_id))
}

/// The tab's currently focused pane id (via any pane's layout snapshot).
fn tab_focused_pane(ctx: &Ctx, pane: &PaneInfo) -> CmdResult<Option<String>> {
    let layout = ctx.client.pane_layout(Some(&pane.pane_id))?;
    Ok(Some(layout.focused_pane_id))
}

fn close_panel(ctx: &Ctx, tab_id: &str) {
    if let Ok(Some(id)) = find_panel(ctx, tab_id) {
        if let Err(err) = ctx.client.pane_close(&id) {
            tracing::debug!(pane = %id, error = %err, "panel close failed");
        }
    }
    clear_state(ctx, tab_id);
}

// ---------------------------------------------------------------------------
// Open / dock
// ---------------------------------------------------------------------------

/// Open the panel in `ctx_pane`'s tab when absent; focus it when `focus`.
/// Returns the panel pane id. Idempotent: an existing panel is just focused.
fn ensure_open(ctx: &Ctx, ctx_pane: &PaneInfo, focus: bool) -> CmdResult<String> {
    if let Some(existing) = find_panel(ctx, &ctx_pane.tab_id)? {
        if focus {
            ctx.client.pane_focus(&existing)?;
        }
        return Ok(existing);
    }

    let config = PoolConfig::load();
    let layout = ctx.client.pane_layout(Some(&ctx_pane.pane_id))?;
    let edge = edge_pane(&layout).ok_or_else(|| {
        Failure::runtime(format!(
            "tab {} has no sizeable pane to split",
            layout.tab_id
        ))
    })?;
    let share = split_share(edge, config.panel_width);

    let mut params = serde_json::json!({
        "plugin_id": PLUGIN_ID,
        "entrypoint": ENTRYPOINT,
        "placement": "split",
        "target_pane_id": edge.pane_id,
        "direction": "right",
        "focus": false,
    });
    if let Some(cwd) = ctx_pane
        .cwd
        .as_deref()
        .or(ctx_pane.foreground_cwd.as_deref())
    {
        params["cwd"] = serde_json::Value::String(cwd.to_string());
    }
    let pane_id = plugin_pane_open(ctx, &params)?;

    // Move the new pane into the left slot, then move its interior edge to
    // the computed share (plugin.pane.open always splits 50/50).
    ctx.client.pane_swap(&PaneSwapParams {
        pane_id: None,
        source_pane_id: Some(pane_id.clone()),
        target_pane_id: Some(edge.pane_id.clone()),
        direction: None,
    })?;
    let delta = (share - 0.5).abs();
    if delta >= RESIZE_EPSILON {
        let _ = ctx.client.pane_resize(&PaneResizeParams {
            direction: PaneDirection::Left,
            pane_id: Some(pane_id.clone()),
            amount: Some(delta),
        });
    }

    write_state(ctx, &ctx_pane.tab_id, &pane_id);
    if focus {
        ctx.client.pane_focus(&pane_id)?;
    } else {
        // plugin.pane.open/swap can still move focus on some versions —
        // restore the pane that had it before we touched the layout.
        let _ = ctx.client.pane_focus(&layout.focused_pane_id);
    }
    Ok(pane_id)
}

/// The pane a left dock splits: leftmost column, topmost row (same pick as
/// herdr-sidebar's `open_plan`).
fn edge_pane(layout: &PaneLayoutSnapshot) -> Option<&PaneLayoutPane> {
    layout
        .panes
        .iter()
        .filter(|p| p.rect.width > 0)
        .min_by(|a, b| (a.rect.x, a.rect.y).cmp(&(b.rect.x, b.rect.y)))
}

/// Panel share of the edge pane's column: `width` columns clamped into
/// [MIN_SHARE, MAX_SHARE] of that pane's width.
fn split_share(edge: &PaneLayoutPane, width_cols: u16) -> f64 {
    (f64::from(width_cols) / edge.rect.width as f64).clamp(MIN_SHARE, MAX_SHARE)
}

/// `plugin.pane.open` via the escape-hatch `call_raw` — the result type
/// (`plugin_pane_opened`) is newer than this client's typed enum, so it
/// arrives as [`ResponseResult::Unknown`] with the raw object preserved.
fn plugin_pane_open(ctx: &Ctx, params: &serde_json::Value) -> CmdResult<String> {
    match ctx.client.call_raw("plugin.pane.open", params)? {
        ResponseResult::Unknown { raw, .. } => raw
            .pointer("/plugin_pane/pane/pane_id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| {
                Failure::runtime(format!("plugin.pane.open returned no pane id: {raw}"))
            }),
        other => Err(Failure::runtime(format!(
            "plugin.pane.open: unexpected result {}",
            other.kind()
        ))),
    }
}

// ---------------------------------------------------------------------------
// Event hook (`ensure`)
// ---------------------------------------------------------------------------

/// `panel ensure` — the `[[events]]` entry point. Docks the panel into the
/// tab the event names, but only when `[panel] autostart` is enabled and
/// only once per tab. Never steals focus; failures are logged, not fatal
/// (an event hook must not break the host).
fn ensure_hook(ctx: &Ctx, env: &EnvLookup<'_>) -> CmdResult<()> {
    if !PoolConfig::load().panel_autostart {
        return Ok(());
    }
    let Some(pane) = context_pane(ctx, env)? else {
        return Ok(());
    };
    match ensure_open(ctx, &pane, false) {
        Ok(id) => {
            if ctx.json {
                emit_json(&serde_json::json!({ "open": true, "pane_id": id }));
            }
            Ok(())
        }
        Err(err) => {
            tracing::debug!(error = %err.message, "panel ensure skipped");
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

fn report(ctx: &Ctx, pane: &PaneInfo, panel: Option<&str>, open: bool) -> CmdResult<()> {
    if ctx.json {
        emit_json(&PanelStatusJson {
            open,
            pane_id: panel.map(str::to_string),
            tab_id: pane.tab_id.clone(),
            workspace_id: pane.workspace_id.clone(),
        });
    } else {
        match panel {
            Some(id) if open => println!("panel open (pane {id}) in tab {}", pane.tab_id),
            _ => println!("panel closed in tab {}", pane.tab_id),
        }
    }
    Ok(())
}
