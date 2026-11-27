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
//! Scope always derives from the *pane/terminal's* launch cwd as reported
//! by the backend (`RuntimeAgent.cwd` → `foreground_cwd` → process-info
//! fallback), never from the CLI process's own cwd — a `herdr-pool`
//! invocation inside a pane must land in the same pool as the agent that
//! spawned it, even if the shell `cd`ed. Outside a managed
//! pane/terminal there is no agent identity, so pool commands fail with a
//! clear `MissingEnv` error; `install`/`uninstall`/`doctor` accept
//! `--project`/cwd for the cases that don't need an agent identity.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use herdr_plugin::paths::{self, EnvLookup};
use herdr_pool_core::{CoreError, PoolContext, SelfIdentity};
use herdr_pool_store::AgentRecord;
use herdr_protocol::HerdrClient;
use herdr_runtime::HerdrRuntime;

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
                "missing env var {var}: not running inside a managed pane \
                 or terminal (HERDR_PANE_ID/WORKSPACE_ID/TAB_ID or HC_TERMINAL_ID)"
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

    /// The backend runtime for this client. Pool-core identity/scope/
    /// registry helpers take `&dyn AgentRuntime`; wrapping the concrete
    /// `HerdrClient` here keeps herdr behavior identical until
    /// `cli-context-port` swaps `Ctx` onto a runtime selection.
    pub fn runtime(&self) -> HerdrRuntime {
        HerdrRuntime::new(self.client.clone())
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

/// Resolve this process's backend identity (env → `AgentRuntime::
/// get_pane`/`get_agent` → scope). Fails with exit-code-2 `MissingEnv`
/// outside a managed pane/terminal.
pub fn resolve_identity(ctx: &Ctx, env: &EnvLookup<'_>) -> CmdResult<SelfIdentity> {
    Ok(herdr_pool_core::resolve_self(&ctx.runtime(), env)?)
}

/// Open the pool for an already-resolved identity's scope.
pub fn open_identity_pool(ctx: &Ctx, identity: &SelfIdentity) -> CmdResult<PoolContext> {
    Ok(PoolContext::for_resolved(
        &ctx.state_dir,
        identity.scope.clone(),
    )?)
}

/// Upsert *this* process into the pool registry and return its record.
/// Delegates to [`herdr_pool_core::upsert_self`] — agent record when the
/// self handle hosts one, pane/terminal record for plain shells.
pub fn upsert_self(pool: &PoolContext, identity: &SelfIdentity) -> CmdResult<AgentRecord> {
    Ok(herdr_pool_core::upsert_self(pool, identity)?)
}

/// Cheap registry reconcile via `AgentRuntime::list_agents` → upsert
/// every agent whose scope matches this pool, then mark stored live
/// *agent* records that vanished offline. See
/// [`herdr_pool_core::refresh_registry`].
pub fn refresh_registry(ctx: &Ctx, pool: &PoolContext) -> CmdResult<usize> {
    Ok(herdr_pool_core::refresh_registry(&ctx.runtime(), pool)?)
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

/// Peer-facing display handle for a registry record: the pane id first
/// (the id agents see in the Herdr UI and can paste into `send`), then
/// the agent name, then the stable `ag_*` pool id.
pub fn agent_handle(record: &AgentRecord) -> String {
    record
        .pane_id
        .clone()
        .filter(|p| !p.is_empty())
        .or_else(|| record.name.clone().filter(|n| !n.is_empty()))
        .unwrap_or_else(|| record.agent_id.clone())
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

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::OsString;

    use herdr_pool_core::ENV_PLUGIN_CONTEXT_JSON;

    use super::*;
    use crate::testutil::{agent_value, pane_get_result, Fixture};

    /// Enough scripted `pane.get` responders for the socket calls one
    /// test makes (resolve_self calls it once per command under test).
    const PANE_DEPTH: usize = 8;

    /// Env map for a plugin pane: the plugin surface's own HERDR_*_ID vars
    /// plus HERDR_PLUGIN_CONTEXT_JSON naming `focused` as the real pane.
    fn plugin_env(plugin_pane: &str, focused: &str) -> HashMap<String, OsString> {
        let ws = focused.split(':').next().unwrap_or("w1");
        let mut map: HashMap<String, OsString> = [
            ("HERDR_PANE_ID", plugin_pane),
            ("HERDR_WORKSPACE_ID", ws),
            ("HERDR_TAB_ID", &format!("{ws}:t1")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), OsString::from(v)))
        .collect();
        map.insert(
            ENV_PLUGIN_CONTEXT_JSON.to_string(),
            OsString::from(format!(
                r#"{{"workspace_id":"{ws}","tab_id":"{ws}:t1","focused_pane_id":"{focused}"}}"#
            )),
        );
        map
    }

    fn direct_env(pane: &str) -> HashMap<String, OsString> {
        let ws = pane.split(':').next().unwrap_or("w1");
        [
            ("HERDR_PANE_ID", pane),
            ("HERDR_WORKSPACE_ID", ws),
            ("HERDR_TAB_ID", &format!("{ws}:t1")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), OsString::from(v)))
        .collect()
    }

    fn script_pane(f: &Fixture, pane: &str, term: &str, cwd: &str) {
        let result = pane_get_result(pane, term, cwd);
        for _ in 0..PANE_DEPTH {
            f.server.on("pane.get").respond(result.clone());
        }
    }

    #[test]
    fn upsert_self_plugin_context_agent_pane_refreshes_real_agent() {
        let f = Fixture::new();
        let cwd = f.project_cwd();
        script_pane(&f, "w1:p9", "term_agent", &cwd);
        f.script_agent_get(
            "w1:p9",
            agent_value("w1:p9", "term_agent", "claude", Some("agent"), &cwd, "idle"),
        );

        let ctx = f.ctx();
        let env_map = plugin_env("w1:pPlugin", "w1:p9");
        let lookup = env_from_map(&env_map);
        let identity = resolve_identity(&ctx, &lookup).unwrap();
        assert!(identity.env.from_plugin_context);
        assert_eq!(identity.env.handle, "w1:p9");

        let pool = open_identity_pool(&ctx, &identity).unwrap();
        let record = upsert_self(&pool, &identity).unwrap();
        assert_eq!(record.pane_id.as_deref(), Some("w1:p9"));

        let stored = pool.pool.registry().list_agents(false).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].agent_id, record.agent_id);
        assert_eq!(stored[0].agent_kind.as_deref(), Some("claude"));
    }

    #[test]
    fn upsert_self_plugin_context_shell_pane_mints_no_ghost() {
        let f = Fixture::new();
        let cwd = f.project_cwd();
        // Focused pane exists but hosts no agent (agent.get → not found).
        script_pane(&f, "w1:p9", "term_shell", &cwd);

        let ctx = f.ctx();
        let env_map = plugin_env("w1:pPlugin", "w1:p9");
        let lookup = env_from_map(&env_map);
        let identity = resolve_identity(&ctx, &lookup).unwrap();
        assert!(identity.env.from_plugin_context);
        assert!(identity.agent.is_none());

        let pool = open_identity_pool(&ctx, &identity).unwrap();
        let record = upsert_self(&pool, &identity).unwrap();
        assert!(record.agent_id.starts_with("ag_"));

        // Nothing persisted: the plugin surface must not appear as a peer.
        assert!(pool.pool.registry().list_agents(false).unwrap().is_empty());
    }

    #[test]
    fn upsert_self_direct_env_shell_pane_still_registers() {
        let f = Fixture::new();
        let cwd = f.project_cwd();
        script_pane(&f, "w1:p9", "term_shell", &cwd);

        let ctx = f.ctx();
        let env_map = direct_env("w1:p9");
        let lookup = env_from_map(&env_map);
        let identity = resolve_identity(&ctx, &lookup).unwrap();
        assert!(!identity.env.from_plugin_context);
        assert!(identity.agent.is_none());

        let pool = open_identity_pool(&ctx, &identity).unwrap();
        upsert_self(&pool, &identity).unwrap();

        // Plain shell panes invoked directly stay addressable — only the
        // plugin-context path skips the write.
        assert_eq!(pool.pool.registry().list_agents(false).unwrap().len(), 1);
    }
}
