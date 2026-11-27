//! Shared runtime context and cross-command helpers.
//!
//! [`Ctx`] bundles what every command needs: the selected [`AgentRuntime`]
//! (herdr socket by default, hc via `HERDR_POOL_RUNTIME=hc`) and the plugin
//! state dir (`--state-dir` or [`herdr_plugin::paths::state_dir`]). Commands
//! that talk to the pool additionally resolve *self* ([`resolve_identity`])
//! and open the scope's [`PoolContext`].
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

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use herdr_plugin::paths::{self, EnvLookup};
use herdr_pool_core::{CoreError, PoolContext, SelfIdentity};
use herdr_pool_store::AgentRecord;
use herdr_protocol::HerdrClient;
use herdr_runtime::{AgentRuntime, HcRuntime, HerdrRuntime};

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

/// Backend selected for this CLI invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeKind {
    /// Herdr socket API (`HERDR_*` pane env, `HERDR_PANE_ID` handles).
    Herdr,
    /// hc terminal runtime (`HC_TERMINAL_ID`, `t7`/`machine/t7` handles).
    Hc,
}

impl RuntimeKind {
    /// Stable name for env propagation and diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Herdr => "herdr",
            Self::Hc => "hc",
        }
    }
}

/// Env var selecting the agent runtime (`herdr` default, `hc` opt-in).
pub const RUNTIME_ENV_VAR: &str = "HERDR_POOL_RUNTIME";
/// Optional override for the `hc connect` argv (JSON-RPC over stdio).
pub const HC_CONNECT_CMD_ENV_VAR: &str = "HC_CONNECT_CMD";

enum RuntimeFactory {
    Herdr(HerdrClient),
    Hc {
        program: OsString,
        args: Vec<OsString>,
    },
}

struct RuntimeCell {
    factory: Option<RuntimeFactory>,
    runtime: Option<Arc<dyn AgentRuntime>>,
}

impl RuntimeFactory {
    fn hc_command(&self) -> Option<std::process::Command> {
        let RuntimeFactory::Hc { program, args } = self else {
            return None;
        };
        let mut cmd = std::process::Command::new(program);
        cmd.args(args);
        Some(cmd)
    }

    fn build(&self) -> CmdResult<Arc<dyn AgentRuntime>> {
        match self {
            Self::Herdr(client) => {
                let runtime: Arc<dyn AgentRuntime> = Arc::new(HerdrRuntime::new(client.clone()));
                Ok(runtime)
            }
            Self::Hc { .. } => {
                let cmd = self
                    .hc_command()
                    .ok_or_else(|| Failure::runtime("invalid hc runtime factory"))?;
                let runtime = HcRuntime::connect(cmd)
                    .map_err(|e| Failure::runtime(format!("connect hc runtime: {e}")))?;
                let runtime: Arc<dyn AgentRuntime> = Arc::new(runtime);
                Ok(runtime)
            }
        }
    }
}

/// Everything a command needs that isn't per-subcommand arguments.
pub struct Ctx {
    /// `--json` human/machine output switch.
    pub json: bool,
    runtime_kind: RuntimeKind,
    runtime: Mutex<RuntimeCell>,
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
        Self::from_globals_with_env(globals, &|key| std::env::var_os(key))
    }

    /// Build a context with an explicit env lookup (tests).
    pub fn from_globals_with_env(globals: &Globals, env: &EnvLookup<'_>) -> CmdResult<Self> {
        let runtime_kind = runtime_kind_from_env(env)?;
        let socket_path_override = globals.socket_path.clone();
        if runtime_kind == RuntimeKind::Hc && socket_path_override.is_some() {
            return Err(Failure::usage(
                "--socket-path requires HERDR_POOL_RUNTIME=herdr",
            ));
        }
        let factory = match runtime_kind {
            RuntimeKind::Herdr => {
                let client = match &socket_path_override {
                    Some(path) => HerdrClient::from_socket_path(path.clone()),
                    None => HerdrClient::new()
                        .map_err(|e| Failure::runtime(format!("resolve Herdr socket path: {e}")))?,
                };
                RuntimeFactory::Herdr(client)
            }
            RuntimeKind::Hc => hc_factory(env)?,
        };
        let state_dir = match &globals.state_dir {
            Some(dir) => dir.clone(),
            None => paths::state_dir()
                .map_err(|e| Failure::runtime(format!("resolve state dir: {e}")))?,
        };
        Ok(Self {
            json: globals.json,
            runtime_kind,
            runtime: Mutex::new(RuntimeCell {
                factory: Some(factory),
                runtime: None,
            }),
            state_dir,
            socket_path_override,
            autostart_daemon: true,
        })
    }

    /// The selected backend kind.
    pub fn runtime_kind(&self) -> RuntimeKind {
        self.runtime_kind
    }

    /// The agent runtime for this client. Herdr is cheap to construct;
    /// hc defers `hc connect` until the first runtime call so commands that
    /// never touch the backend (`version`, `install`, daemon controls) do
    /// not require `hc` on PATH.
    pub fn runtime(&self) -> CmdResult<Arc<dyn AgentRuntime>> {
        let mut cell = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(runtime) = &cell.runtime {
            return Ok(runtime.clone());
        }
        let factory = cell
            .factory
            .as_ref()
            .ok_or_else(|| Failure::runtime("runtime backend already consumed"))?;
        let runtime = factory.build()?;
        cell.runtime = Some(runtime.clone());
        cell.factory = None;
        Ok(runtime)
    }

    /// The underlying Herdr client for herdr-only surfaces (plugin panel,
    /// socket diagnostics). Returns `None` on hc without connecting.
    pub fn herdr_client(&self) -> Option<HerdrClient> {
        let cell = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(runtime) = &cell.runtime {
            return runtime.herdr_client().cloned();
        }
        match &cell.factory {
            Some(RuntimeFactory::Herdr(client)) => Some(client.clone()),
            _ => None,
        }
    }

    /// Explicit constructor (unit tests).
    #[cfg(all(test, unix))]
    pub fn for_test(json: bool, client: HerdrClient, state_dir: PathBuf) -> Self {
        let runtime: Arc<dyn AgentRuntime> = Arc::new(HerdrRuntime::new(client));
        Self::for_test_runtime(json, runtime, RuntimeKind::Herdr, state_dir)
    }

    /// Explicit constructor for an injected runtime (unit tests).
    #[cfg(all(test, unix))]
    pub fn for_test_runtime(
        json: bool,
        runtime: Arc<dyn AgentRuntime>,
        runtime_kind: RuntimeKind,
        state_dir: PathBuf,
    ) -> Self {
        Self {
            json,
            runtime_kind,
            runtime: Mutex::new(RuntimeCell {
                factory: None,
                runtime: Some(runtime),
            }),
            state_dir,
            socket_path_override: None,
            autostart_daemon: false,
        }
    }
}

fn env_value(env: &EnvLookup<'_>, key: &str) -> Option<OsString> {
    env(key).filter(|value| !value.is_empty())
}

fn runtime_kind_from_env(env: &EnvLookup<'_>) -> CmdResult<RuntimeKind> {
    let Some(value) = env_value(env, RUNTIME_ENV_VAR) else {
        return Ok(RuntimeKind::Herdr);
    };
    let value = value.to_string_lossy();
    if value.eq_ignore_ascii_case("herdr") {
        Ok(RuntimeKind::Herdr)
    } else if value.eq_ignore_ascii_case("hc") {
        Ok(RuntimeKind::Hc)
    } else {
        Err(Failure::usage(format!(
            "invalid {RUNTIME_ENV_VAR} '{value}' (expected herdr|hc)"
        )))
    }
}

fn hc_factory(env: &EnvLookup<'_>) -> CmdResult<RuntimeFactory> {
    if let Some(connect) = env_value(env, HC_CONNECT_CMD_ENV_VAR) {
        return Ok(RuntimeFactory::Hc {
            program: OsString::from("sh"),
            args: vec![OsString::from("-c"), connect],
        });
    }
    Ok(RuntimeFactory::Hc {
        program: OsString::from("hc"),
        args: vec![OsString::from("connect"), OsString::from("--json")],
    })
}

// ---------------------------------------------------------------------------
// Identity / scope / registry helpers shared by pool commands
// ---------------------------------------------------------------------------

/// Resolve this process's backend identity (env → `AgentRuntime::
/// get_pane`/`get_agent` → scope). Fails with exit-code-2 `MissingEnv`
/// outside a managed pane/terminal.
pub fn resolve_identity(ctx: &Ctx, env: &EnvLookup<'_>) -> CmdResult<SelfIdentity> {
    Ok(herdr_pool_core::resolve_self(ctx.runtime()?.as_ref(), env)?)
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
    Ok(herdr_pool_core::refresh_registry(
        ctx.runtime()?.as_ref(),
        pool,
    )?)
}

/// Resolve a peer query (`ag_*` id, runtime handle/pane id/terminal
/// address, name) inside this scope's registry. The registry is
/// per-scope by construction, so a cross-scope hit is impossible; a
/// foreign-looking query simply finds nothing → `NotFound`/`Ambiguous`
/// (exit 2).
pub fn resolve_peer(pool: &PoolContext, query: &str) -> CmdResult<AgentRecord> {
    pool.pool
        .registry()
        .resolve_peer(query)
        .map_err(Failure::from)
}

/// Peer-facing display handle for a registry record: the runtime handle
/// first (pane id on herdr, terminal address on hc — the id agents see in
/// the UI and can paste into `send`), then the agent name, then the
/// stable `ag_*` pool id.
pub fn agent_handle(record: &AgentRecord) -> String {
    record
        .pane_id
        .clone()
        .filter(|p| !p.is_empty())
        .or_else(|| record.name.clone().filter(|n| !n.is_empty()))
        .unwrap_or_else(|| record.agent_id.clone())
}

/// Current delivery target for a registry record. The `pane_id` column
/// stores the runtime handle verbatim (pane id on herdr, terminal address
/// on hc); herdr's `agent.prompt` accepts pane_id and agent name but NOT
/// terminal_id (term_* → agent_not_found), so pane_id is preferred (kept
/// current by pane.moved rekeys), then name, then terminal_id as a last
/// resort for legacy herdr records. Mirrors the router delivery worker.
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
    // HERDR_POOL_RUNTIME selects the same backend inside routerd.
    cmd.env("HERDR_POOL_STATE_DIR", &ctx.state_dir);
    cmd.env(RUNTIME_ENV_VAR, ctx.runtime_kind.as_str());
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

    // -- runtime selection ----------------------------------------------------

    fn globals(state_dir: &std::path::Path, socket_path: Option<PathBuf>) -> Globals {
        Globals {
            json: false,
            socket_path,
            state_dir: Some(state_dir.to_path_buf()),
            verbose: false,
        }
    }

    #[test]
    fn runtime_defaults_to_herdr() {
        let f = Fixture::new();
        let globals = globals(f.state.path(), Some(f.server.socket_path().to_path_buf()));
        let ctx =
            Ctx::from_globals_with_env(&globals, &|_: &str| -> Option<OsString> { None }).unwrap();
        assert_eq!(ctx.runtime_kind(), RuntimeKind::Herdr);
        assert!(ctx.herdr_client().is_some());
    }

    #[test]
    fn runtime_hc_selection_defers_connect() {
        let f = Fixture::new();
        let mut map = HashMap::new();
        map.insert(RUNTIME_ENV_VAR.to_string(), OsString::from("hc"));
        let lookup = env_from_map(&map);
        let globals = globals(f.state.path(), None);
        // Building the context must not spawn `hc connect` — the runtime
        // connects lazily on the first backend call.
        let ctx = Ctx::from_globals_with_env(&globals, &lookup).unwrap();
        assert_eq!(ctx.runtime_kind(), RuntimeKind::Hc);
        assert!(ctx.herdr_client().is_none());
    }

    #[test]
    fn runtime_hc_rejects_socket_path_override() {
        let f = Fixture::new();
        let mut map = HashMap::new();
        map.insert(RUNTIME_ENV_VAR.to_string(), OsString::from("hc"));
        let lookup = env_from_map(&map);
        let globals = globals(f.state.path(), Some(PathBuf::from("/tmp/herdr.sock")));
        let err = Ctx::from_globals_with_env(&globals, &lookup)
            .err()
            .expect("hc + --socket-path must fail");
        assert_eq!(err.code, 2);
        assert!(err.message.contains("--socket-path"));
    }

    #[test]
    fn runtime_kind_rejects_unknown_value() {
        let f = Fixture::new();
        let mut map = HashMap::new();
        map.insert(RUNTIME_ENV_VAR.to_string(), OsString::from("bogus"));
        let lookup = env_from_map(&map);
        let globals = globals(f.state.path(), None);
        let err = Ctx::from_globals_with_env(&globals, &lookup)
            .err()
            .expect("unknown runtime must fail");
        assert_eq!(err.code, 2);
        assert!(err.message.contains("invalid"));
    }

    // -- runtime-agnostic identity --------------------------------------------

    /// Minimal in-memory [`AgentRuntime`] — just enough record surface for
    /// `resolve_self` (pane + optional agent record), everything else
    /// unsupported.
    struct StubRuntime {
        pane: herdr_runtime::RuntimeAgent,
        agent: Option<herdr_runtime::RuntimeAgent>,
    }

    impl StubRuntime {
        fn agent(handle: &str, cwd: &str) -> herdr_runtime::RuntimeAgent {
            herdr_runtime::RuntimeAgent {
                handle: handle.to_string(),
                terminal_id: None,
                kind: Some("claude".to_string()),
                name: Some("self".to_string()),
                cwd: Some(cwd.to_string()),
                foreground_cwd: Some(cwd.to_string()),
                status: herdr_runtime::RuntimeStatus::Idle,
                interactive_ready: true,
                launch_pending: false,
                extra: Default::default(),
            }
        }
    }

    impl AgentRuntime for StubRuntime {
        fn list_agents(
            &self,
        ) -> Result<Vec<herdr_runtime::RuntimeAgent>, herdr_runtime::RuntimeError> {
            Ok(self.agent.iter().cloned().collect())
        }

        fn get_agent(
            &self,
            handle: &str,
        ) -> Result<herdr_runtime::RuntimeAgent, herdr_runtime::RuntimeError> {
            self.agent
                .clone()
                .filter(|a| a.handle == handle)
                .ok_or_else(|| herdr_runtime::RuntimeError::NotFound {
                    handle: handle.to_string(),
                })
        }

        fn get_pane(
            &self,
            _handle: &str,
        ) -> Result<herdr_runtime::RuntimeAgent, herdr_runtime::RuntimeError> {
            Ok(self.pane.clone())
        }

        fn send_prompt(
            &self,
            _handle: &str,
            _text: &str,
        ) -> Result<herdr_runtime::SendOutcome, herdr_runtime::RuntimeError> {
            Err(herdr_runtime::RuntimeError::Unsupported("stub".to_string()))
        }

        fn wait_agent(
            &self,
            _handle: &str,
            _req: herdr_runtime::WaitRequest,
        ) -> Result<herdr_runtime::WaitOutcome, herdr_runtime::RuntimeError> {
            Err(herdr_runtime::RuntimeError::Unsupported("stub".to_string()))
        }

        fn read_agent(
            &self,
            _handle: &str,
            _lines: u32,
        ) -> Result<String, herdr_runtime::RuntimeError> {
            Err(herdr_runtime::RuntimeError::Unsupported("stub".to_string()))
        }

        fn send_keys(
            &self,
            _handle: &str,
            _keys: &[String],
        ) -> Result<(), herdr_runtime::RuntimeError> {
            Err(herdr_runtime::RuntimeError::Unsupported("stub".to_string()))
        }

        fn self_identity(
            &self,
            _env: &EnvLookup<'_>,
        ) -> Result<herdr_runtime::SelfIdentity, herdr_runtime::RuntimeError> {
            Err(herdr_runtime::RuntimeError::Unsupported("stub".to_string()))
        }

        fn subscribe_events(
            &self,
            _watched: &[String],
        ) -> Result<herdr_runtime::EventStream, herdr_runtime::RuntimeError> {
            Err(herdr_runtime::RuntimeError::Unsupported("stub".to_string()))
        }
    }

    #[test]
    fn resolve_identity_works_through_injected_runtime() {
        let f = Fixture::new();
        let cwd = f.project_cwd();
        let runtime: Arc<dyn AgentRuntime> = Arc::new(StubRuntime {
            pane: StubRuntime::agent("t7", &cwd),
            agent: Some(StubRuntime::agent("t7", &cwd)),
        });
        let ctx = Ctx::for_test_runtime(
            false,
            runtime,
            RuntimeKind::Hc,
            f.state.path().to_path_buf(),
        );

        // hc identity is the bare terminal address — no HERDR_* vars.
        let mut env_map = HashMap::new();
        env_map.insert("HC_TERMINAL_ID".to_string(), OsString::from("t7"));
        let lookup = env_from_map(&env_map);
        let identity = resolve_identity(&ctx, &lookup).unwrap();
        assert_eq!(identity.env.handle, "t7");
        assert!(identity.workspace.is_none());
        assert!(identity.tab.is_none());
        assert_eq!(
            identity.agent.as_ref().map(|a| a.name.as_deref()),
            Some(Some("self"))
        );
        assert_eq!(
            identity.canonical_path,
            std::fs::canonicalize(f.project.path()).unwrap()
        );
    }
}
