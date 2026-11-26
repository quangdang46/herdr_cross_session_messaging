//! Shared runtime context and cross-command helpers.
//!
//! [`Ctx`] bundles what every command needs: the [`HerdrClient`] (built from
//! `--socket-path` or the standard env resolution) and the plugin state dir
//! (`--state-dir` or [`herdr_plugin::paths::state_dir`]). Commands that talk
//! to the pool additionally resolve *self* ([`resolve_identity`]) and open
//! the scope's [`PoolContext`].
//!
//! ## Project-path / scope resolution decision
//!
//! Scope always derives from the *pane's* launch cwd as reported by Herdr
//! (`AgentInfo.cwd` → `PaneInfo.cwd` → `pane.process_info`), never from the
//! CLI process's own cwd — a `herdr-pool` invocation inside a pane must land
//! in the same pool as the agent that spawned it, even if the shell `cd`ed.
//! Outside Herdr there is no agent identity, so pool commands fail with a
//! clear `MissingEnv` error; `install`/`uninstall`/`doctor` accept
//! `--project`/cwd for the cases that don't need an agent identity.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use herdr_plugin::paths::{self, EnvLookup};
use herdr_pool_core::{
    resolve_scope_for_agent, resolve_self, CoreError, PoolContext, ResolvedScope, SelfIdentity,
};
use herdr_pool_store::{new_agent_id, AgentRecord, STATUS_OFFLINE};
use herdr_protocol::{AgentInfo, HerdrClient};

use crate::cli::Globals;

/// Unix epoch milliseconds — matches the store's timestamp convention.
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A command failure with a process exit code.
///
/// Exit codes follow the herdcluster convention: `0` ok, `1` runtime,
/// `2` usage. `kind` is a stable machine string emitted under `--json`.
#[derive(Debug)]
pub struct Failure {
    pub message: String,
    pub code: i32,
    pub kind: &'static str,
}

impl Failure {
    /// Runtime failure (exit 1).
    pub fn runtime(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 1,
            kind: "runtime",
        }
    }

    /// Usage/precondition failure (exit 2): bad arguments, missing
    /// `HERDR_*` env, unknown peer/message/thread.
    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 2,
            kind: "usage",
        }
    }

    fn from_core(err: CoreError) -> Self {
        match err {
            CoreError::MissingEnv(var) => Self::usage(format!(
                "missing env var {var}: not running inside a Herdr pane \
                 (HERDR_PANE_ID/WORKSPACE_ID/TAB_ID are injected by Herdr)"
            )),
            CoreError::PeerNotFound(msg) | CoreError::PeerAmbiguous(msg) => {
                Self::usage(format!("peer: {msg}"))
            }
            CoreError::CrossScopeDenied(msg) => Self::usage(format!("cross-scope: {msg}")),
            other => Self::runtime(other.to_string()),
        }
    }

    fn from_store(err: herdr_pool_store::StoreError) -> Self {
        use herdr_pool_store::StoreError as S;
        match err {
            S::NotFound(msg) | S::Ambiguous(msg) => Self::usage(msg),
            other => Self::runtime(other.to_string()),
        }
    }
}

impl From<anyhow::Error> for Failure {
    fn from(err: anyhow::Error) -> Self {
        Self::runtime(format!("{err:#}"))
    }
}

impl From<herdr_protocol::HerdrError> for Failure {
    fn from(err: herdr_protocol::HerdrError) -> Self {
        Self::runtime(err.to_string())
    }
}

impl From<CoreError> for Failure {
    fn from(err: CoreError) -> Self {
        Self::from_core(err)
    }
}

impl From<herdr_pool_store::StoreError> for Failure {
    fn from(err: herdr_pool_store::StoreError) -> Self {
        Self::from_store(err)
    }
}

impl From<herdr_plugin::PluginError> for Failure {
    fn from(err: herdr_plugin::PluginError) -> Self {
        Self::runtime(err.to_string())
    }
}

/// Command result alias.
pub type CmdResult<T> = Result<T, Failure>;

/// Everything a command needs that isn't per-subcommand arguments.
pub struct Ctx {
    /// `--json` human/machine output switch.
    pub json: bool,
    /// Socket client (`--socket-path` or env-resolved).
    pub client: HerdrClient,
    /// Plugin state dir (`--state-dir` or env-resolved).
    pub state_dir: PathBuf,
    /// `--socket-path` override, propagated to spawned `herdr-routerd` via
    /// `HERDR_SOCKET_PATH` (env-resolved paths need no propagation — the
    /// child inherits the environment).
    pub(crate) socket_path_override: Option<PathBuf>,
    /// When true, delivery-oriented commands may spawn `herdr-routerd`
    /// if no daemon is running (see [`ensure_daemon`]). Tests set false.
    pub autostart_daemon: bool,
}

impl Ctx {
    /// Build a context from the global CLI flags.
    pub fn from_globals(globals: &Globals) -> CmdResult<Self> {
        let client = match &globals.socket_path {
            Some(path) => HerdrClient::from_socket_path(path.clone()),
            None => HerdrClient::new()
                .map_err(|e| Failure::runtime(format!("resolve Herdr socket path: {e}")))?,
        };
        let state_dir = match &globals.state_dir {
            Some(dir) => dir.clone(),
            None => paths::state_dir()
                .map_err(|e| Failure::runtime(format!("resolve state dir: {e}")))?,
        };
        Ok(Self {
            json: globals.json,
            client,
            state_dir,
            socket_path_override: globals.socket_path.clone(),
            autostart_daemon: true,
        })
    }

    /// Explicit constructor (unit tests).
    #[cfg(all(test, unix))]
    pub fn for_test(json: bool, client: HerdrClient, state_dir: PathBuf) -> Self {
        Self {
            json,
            client,
            state_dir,
            socket_path_override: None,
            autostart_daemon: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Identity / scope / registry helpers shared by pool commands
// ---------------------------------------------------------------------------

/// Resolve this process's Herdr identity (env → pane.current → agent.get →
/// scope). Fails with exit-code-2 `MissingEnv` outside Herdr.
pub fn resolve_identity(ctx: &Ctx, env: &EnvLookup<'_>) -> CmdResult<SelfIdentity> {
    Ok(resolve_self(&ctx.client, env)?)
}

/// Open the pool for an already-resolved identity's scope.
pub fn open_identity_pool(ctx: &Ctx, identity: &SelfIdentity) -> CmdResult<PoolContext> {
    Ok(PoolContext::for_resolved(
        &ctx.state_dir,
        identity.scope.clone(),
    )?)
}

/// Resolve an `AgentInfo`'s scope, falling back to `pane.process_info` once
/// when the record carries no cwd fields (mirrors the router's
/// `resolve_agent_scope`). Returns [`CoreError`] so callers can distinguish
/// [`CoreError::ScopeUnresolved`] (skip the agent) from real failures.
pub fn scope_for_agent(ctx: &Ctx, agent: &AgentInfo) -> Result<ResolvedScope, CoreError> {
    match resolve_scope_for_agent(agent, None) {
        Ok(scope) => Ok(scope),
        Err(CoreError::ScopeUnresolved(_)) => {
            let process_info = ctx
                .client
                .pane_process_info(Some(&agent.pane_id))
                .map_err(CoreError::Protocol)?;
            resolve_scope_for_agent(agent, Some(&process_info))
        }
        Err(err) => Err(err),
    }
}

/// Build (or merge into) the [`AgentRecord`] for an `AgentInfo`, preserving
/// `agent_id`/`first_seen_at`/`bootstrap_version` from any existing record
/// matched by `terminal_id` then live `pane_id`. Mirrors the router's
/// `merge_record`; kept local because `herdr-router`'s copy is private.
pub fn record_for_agent(
    registry: &herdr_pool_store::RegistryStore<'_>,
    agent: &AgentInfo,
    resolved: &ResolvedScope,
) -> AgentRecord {
    let existing = registry
        .get_by_terminal_any(&agent.terminal_id)
        .ok()
        .flatten()
        .or_else(|| {
            registry
                .find_by_pane(&agent.pane_id)
                .ok()?
                .into_iter()
                .find(|r| r.status != STATUS_OFFLINE)
        });

    let now = now_millis();
    AgentRecord {
        agent_id: existing
            .as_ref()
            .map(|e| e.agent_id.clone())
            .unwrap_or_else(new_agent_id),
        scope_key: resolved.scope_key.as_str().to_string(),
        terminal_id: Some(agent.terminal_id.clone()),
        pane_id: Some(agent.pane_id.clone()),
        agent_kind: agent.agent.clone().or_else(|| agent.display_agent.clone()),
        name: agent.name.clone(),
        cwd: Some(resolved.canonical_path.to_string_lossy().into_owned()),
        status: agent.agent_status.as_str().to_string(),
        interactive_ready: agent.interactive_ready,
        launch_pending: agent.launch_pending,
        bootstrap_version: existing.as_ref().and_then(|e| e.bootstrap_version),
        first_seen_at: existing.map(|e| e.first_seen_at).unwrap_or(now),
        last_seen_at: now,
        offline_at: None,
    }
}

/// Upsert *this* process into the pool registry and return its record.
///
/// With an agent record (`agent.get` succeeded) the record mirrors
/// [`record_for_agent`]; for plain shell panes (`agent: None`) a record is
/// still created keyed by `terminal_id` so the pane is addressable.
pub fn upsert_self(pool: &PoolContext, identity: &SelfIdentity) -> CmdResult<AgentRecord> {
    let registry = pool.pool.registry();
    let existing = match &identity.agent {
        Some(agent) => registry
            .get_by_terminal_any(&agent.terminal_id)
            .ok()
            .flatten()
            .or_else(|| {
                registry
                    .find_by_pane(&agent.pane_id)
                    .ok()?
                    .into_iter()
                    .find(|r| r.status != STATUS_OFFLINE)
            }),
        None => {
            let pane = &identity.pane;
            registry
                .get_by_terminal_any(&pane.terminal_id)
                .ok()
                .flatten()
                .or_else(|| {
                    registry
                        .find_by_pane(&pane.pane_id)
                        .ok()?
                        .into_iter()
                        .find(|r| r.status != STATUS_OFFLINE)
                })
        }
    };

    let now = now_millis();
    let record = match &identity.agent {
        Some(agent) => AgentRecord {
            agent_id: existing
                .as_ref()
                .map(|e| e.agent_id.clone())
                .unwrap_or_else(new_agent_id),
            scope_key: pool.scope_key.as_str().to_string(),
            terminal_id: Some(agent.terminal_id.clone()),
            pane_id: Some(agent.pane_id.clone()),
            agent_kind: agent.agent.clone().or_else(|| agent.display_agent.clone()),
            name: agent.name.clone(),
            cwd: Some(pool.canonical_path.to_string_lossy().into_owned()),
            status: agent.agent_status.as_str().to_string(),
            interactive_ready: agent.interactive_ready,
            launch_pending: agent.launch_pending,
            bootstrap_version: existing.as_ref().and_then(|e| e.bootstrap_version),
            first_seen_at: existing.map(|e| e.first_seen_at).unwrap_or(now),
            last_seen_at: now,
            offline_at: None,
        },
        None => {
            let pane = &identity.pane;
            AgentRecord {
                agent_id: existing
                    .as_ref()
                    .map(|e| e.agent_id.clone())
                    .unwrap_or_else(new_agent_id),
                scope_key: pool.scope_key.as_str().to_string(),
                terminal_id: Some(pane.terminal_id.clone()),
                pane_id: Some(pane.pane_id.clone()),
                agent_kind: pane.display_agent.clone(),
                name: pane.label.clone(),
                cwd: Some(pool.canonical_path.to_string_lossy().into_owned()),
                status: pane.agent_status.as_str().to_string(),
                // Plain panes carry no agent readiness — preserve any
                // stored values, defaulting to ready (no launch pending).
                interactive_ready: existing
                    .as_ref()
                    .map(|e| e.interactive_ready)
                    .unwrap_or(true),
                launch_pending: existing.as_ref().map(|e| e.launch_pending).unwrap_or(false),
                bootstrap_version: existing.as_ref().and_then(|e| e.bootstrap_version),
                first_seen_at: existing.map(|e| e.first_seen_at).unwrap_or(now),
                last_seen_at: now,
                offline_at: None,
            }
        }
    };
    registry.upsert_agent(&record)?;
    Ok(record)
}

/// Cheap registry reconcile: `agent.list` → upsert every agent whose scope
/// matches this pool, then mark stored live *agent* records that vanished
/// offline.
///
/// Records with `agent_kind: None` (plain shell panes registered via
/// `whoami`) are never marked offline here — `agent.list` only reports
/// detected agents; the router daemon owns authoritative offline marking.
///
/// Returns the number of agent records upserted.
pub fn refresh_registry(ctx: &Ctx, pool: &PoolContext) -> CmdResult<usize> {
    let agents = ctx.client.agent_list()?;
    let registry = pool.pool.registry();
    let mut synced = 0usize;
    // (terminal_id, pane_id) pairs seen live in *this* scope.
    let mut live_terms: Vec<String> = Vec::new();
    let mut live_panes: Vec<String> = Vec::new();

    for agent in &agents {
        let scope = match scope_for_agent(ctx, agent) {
            Ok(scope) => scope,
            // Agent's cwd is gone/unresolvable — skip, don't fail the command.
            Err(CoreError::ScopeUnresolved(_)) => continue,
            Err(err) => return Err(err.into()),
        };
        if scope.scope_key != pool.scope_key {
            continue; // never cross scopes
        }
        let record = record_for_agent(&registry, agent, &scope);
        registry.upsert_agent(&record)?;
        live_terms.push(agent.terminal_id.clone());
        live_panes.push(agent.pane_id.clone());
        synced += 1;
    }

    // Mark offline: stored live agent-kind records absent from agent.list.
    for stored in registry.list_agents(false)? {
        if stored.agent_kind.is_none() {
            continue;
        }
        let term_live = stored
            .terminal_id
            .as_deref()
            .is_some_and(|t| live_terms.iter().any(|x| x == t));
        let pane_live = stored
            .pane_id
            .as_deref()
            .is_some_and(|p| live_panes.iter().any(|x| x == p));
        if !term_live && !pane_live {
            if let Err(err) = registry.mark_offline(&stored.agent_id) {
                tracing::debug!(agent = %stored.agent_id, error = %err, "mark_offline skipped");
            }
        }
    }
    Ok(synced)
}

/// Resolve a peer query (`ag_*` id, pane/terminal id, name) inside this
/// scope's registry. The registry is per-scope by construction, so a
/// cross-scope hit is impossible; a foreign-looking query simply finds
/// nothing → `NotFound`/`Ambiguous` (exit 2).
pub fn resolve_peer(pool: &PoolContext, query: &str) -> CmdResult<AgentRecord> {
    pool.pool
        .registry()
        .resolve_peer(query)
        .map_err(Failure::from)
}

/// Current delivery target for a registry record. `agent.prompt` accepts
/// pane_id and agent name but NOT terminal_id (term_* → agent_not_found),
/// so pane_id is preferred (kept current by pane.moved rekeys), then name,
/// then terminal_id as a last resort. Mirrors the router delivery worker.
pub fn delivery_target(record: &AgentRecord) -> Option<String> {
    record
        .pane_id
        .clone()
        .filter(|p| !p.is_empty())
        .or_else(|| record.name.clone().filter(|n| !n.is_empty()))
        .or_else(|| record.terminal_id.clone().filter(|t| !t.is_empty()))
}

/// Ensure the router daemon is running; used by delivery-oriented commands
/// so a stopped daemon doesn't strand `pending` mail. Best-effort: failure
/// to start is logged, not fatal — the CLI still attempts direct delivery.
pub fn ensure_daemon(ctx: &Ctx) -> CmdResult<()> {
    if !ctx.autostart_daemon {
        return Ok(());
    }
    if matches!(
        herdr_router::daemon_status(&ctx.state_dir),
        herdr_router::DaemonStatus::Running { .. }
    ) {
        return Ok(());
    }
    let Some(bin) = routerd_bin() else {
        tracing::debug!("herdr-routerd binary not found; delivery worker unavailable");
        return Ok(());
    };
    let mut cmd = std::process::Command::new(bin);
    cmd.arg("start");
    // Propagate the resolved state dir so the daemon and this CLI agree even
    // when --state-dir overrode the env resolution. Same for --socket-path:
    // the daemon resolves its socket from HERDR_SOCKET_PATH, so an explicit
    // flag must be exported or the daemon would attach to a different server.
    cmd.env("HERDR_POOL_STATE_DIR", &ctx.state_dir);
    if let Some(path) = &ctx.socket_path_override {
        cmd.env("HERDR_SOCKET_PATH", path);
    }
    // The child's stdout would corrupt `--json` output — `herdr-routerd
    // start` prints a "started (pid N)" line. Daemon logs already go to
    // <state>/logs/routerd.log, so silence the wrapper entirely.
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match cmd.status() {
        Ok(status) if status.success() => {
            tracing::info!("router daemon started");
        }
        Ok(status) => {
            tracing::debug!(code = ?status.code(), "herdr-routerd start failed");
        }
        Err(err) => {
            tracing::debug!(error = %err, "herdr-routerd start spawn failed");
        }
    }
    Ok(())
}

/// Locate the `herdr-routerd` binary: sibling of this executable first
/// (same `target/` dir in dev), then `PATH`.
pub fn routerd_bin() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("herdr-routerd");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("herdr-routerd");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Helper to build an env lookup from a map in tests.
#[cfg(all(test, unix))]
pub fn env_from_map<'a>(
    map: &'a std::collections::HashMap<String, std::ffi::OsString>,
) -> impl Fn(&str) -> Option<std::ffi::OsString> + 'a {
    move |key| map.get(key).cloned()
}
