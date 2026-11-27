//! Self-identity resolution: which pane/agent/scope is *this* process?
//!
//! Backends inject a pane-equivalent handle, never a directory: herdr
//! sets `HERDR_PANE_ID`/`HERDR_WORKSPACE_ID`/`HERDR_TAB_ID` (plus
//! `HERDR_PLUGIN_CONTEXT_JSON` on plugin surfaces); hc sets
//! `HC_TERMINAL_ID` inside terminals. [`self_env_from`] parses them
//! (pure, unit testable); [`resolve_self`] then queries the
//! [`AgentRuntime`] for the pane/agent records and resolves the project
//! scope — the same path the `herdr-pool whoami` command uses.
//!
//! These env vars identify the *handle*, not a directory: scope still
//! comes exclusively from record `cwd`/`foreground_cwd` (plus backend
//! process-info fallbacks) per [`crate::scope`].

use std::path::PathBuf;

use herdr_plugin::paths::EnvLookup;
use herdr_runtime::{AgentRuntime, RuntimeAgent, RuntimeError};

use crate::error::{CoreError, Result};
use crate::scope::{resolve_scope_for_runtime, ResolvedScope};

/// Env var carrying the caller's pane id (`wN:pM`) — herdr backend.
pub const ENV_PANE_ID: &str = "HERDR_PANE_ID";
/// Env var carrying the caller's workspace id — herdr backend.
pub const ENV_WORKSPACE_ID: &str = "HERDR_WORKSPACE_ID";
/// Env var carrying the caller's tab id — herdr backend.
pub const ENV_TAB_ID: &str = "HERDR_TAB_ID";
/// Env var carrying the plugin invocation context JSON. Plugin panes and
/// popups get BOTH their own `HERDR_PANE_ID` (the plugin surface's pane,
/// which is never a messaging identity) AND this JSON, whose
/// `focused_pane_id` names the underlying tiled agent pane.
pub const ENV_PLUGIN_CONTEXT_JSON: &str = "HERDR_PLUGIN_CONTEXT_JSON";
/// Env var carrying the caller's terminal address (`t7`, or
/// `machine/t7` for remote cores) — hc backend. hc has no workspace/tab
/// concept, so it is the only identity var that backend injects.
pub const ENV_HC_TERMINAL_ID: &str = "HC_TERMINAL_ID";

/// Pane-equivalent identity parsed from the backend-injected environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfEnv {
    /// Runtime handle of self: `wN:pM` on herdr, `t7`/`machine/t7` on hc.
    pub handle: String,
    /// Workspace hint — `Some` on herdr, `None` on hc (no workspaces).
    pub workspace_id: Option<String>,
    /// Tab hint — `Some` on herdr, `None` on hc (no tabs).
    pub tab_id: Option<String>,
    /// True when the ids came from `HERDR_PLUGIN_CONTEXT_JSON` (a plugin
    /// pane/popup) rather than the direct `HERDR_*_ID` vars.
    pub from_plugin_context: bool,
}

/// Parse the backend identity vars. The herdr triple
/// (`HERDR_PANE_ID`/`HERDR_WORKSPACE_ID`/`HERDR_TAB_ID`) or the hc
/// `HC_TERMINAL_ID` must be present and non-empty; neither yields
/// [`CoreError::MissingEnv`] — the process is not running inside a
/// managed pane/terminal.
///
/// Plugin panes (including popups) are the exception: they get their own
/// `HERDR_*_ID` vars AND `HERDR_PLUGIN_CONTEXT_JSON`, whose
/// `focused_pane_id` names the underlying tiled agent pane. A *parseable*
/// context therefore wins over the direct vars — the plugin pane's own id
/// is never the messaging identity. When the context is missing or
/// malformed, the direct vars are used instead.
///
/// When both backends' vars are present the herdr triple wins (a process
/// inside Herdr is authoritative); a partial herdr triple falls through
/// to `HC_TERMINAL_ID` before erroring.
pub fn self_env_from(env: &EnvLookup<'_>) -> Result<SelfEnv> {
    let mut context_err = None;
    if let Some(context) = env(ENV_PLUGIN_CONTEXT_JSON)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string_lossy().into_owned())
    {
        match plugin_context_env(&context) {
            Ok(ids) => return Ok(ids),
            Err(err) => context_err = Some(err),
        }
    }

    let pane_id = env_value(env, ENV_PANE_ID);
    let workspace_id = env_value(env, ENV_WORKSPACE_ID);
    let tab_id = env_value(env, ENV_TAB_ID);
    if let (Some(pane_id), Some(workspace_id), Some(tab_id)) = (&pane_id, &workspace_id, &tab_id) {
        return Ok(SelfEnv {
            handle: pane_id.clone(),
            workspace_id: Some(workspace_id.clone()),
            tab_id: Some(tab_id.clone()),
            from_plugin_context: false,
        });
    }

    // hc backend: the terminal address alone is the whole identity —
    // there are no workspace/tab env vars (labels on the terminal record
    // carry hints instead; see [`resolve_self`]).
    if let Some(handle) = env_value(env, ENV_HC_TERMINAL_ID) {
        return Ok(SelfEnv {
            handle,
            workspace_id: None,
            tab_id: None,
            from_plugin_context: false,
        });
    }

    // No usable identity at all. A plugin-context error (malformed JSON or
    // missing context fields) is more informative than a bare missing-var
    // report, so prefer it when present.
    if let Some(err) = context_err {
        return Err(err);
    }

    // Report the first missing var so the error names what is absent.
    for (var, value) in [
        (ENV_PANE_ID, pane_id),
        (ENV_WORKSPACE_ID, workspace_id),
        (ENV_TAB_ID, tab_id),
    ] {
        if value.is_none() {
            return Err(CoreError::MissingEnv(var.to_string()));
        }
    }
    unreachable!("at least one id var was missing above");
}

fn env_value(env: &EnvLookup<'_>, var: &str) -> Option<String> {
    env(var)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string_lossy().into_owned())
}

/// Extract pane identity from `HERDR_PLUGIN_CONTEXT_JSON`.
///
/// The context names the *invocation's* underlying tiled pane
/// (`focused_pane_id`), which is the right identity for a plugin UI: the
/// inbox popup shows the focused agent's mail.
fn plugin_context_env(context_json: &str) -> Result<SelfEnv> {
    #[derive(serde::Deserialize)]
    struct PluginContext {
        workspace_id: Option<String>,
        tab_id: Option<String>,
        focused_pane_id: Option<String>,
    }
    let ctx: PluginContext = serde_json::from_str(context_json)
        .map_err(|e| CoreError::InvalidEnv(format!("{ENV_PLUGIN_CONTEXT_JSON}: {e}")))?;
    let pick = |v: Option<String>| {
        v.filter(|s| !s.is_empty())
            .ok_or_else(|| CoreError::MissingEnv(ENV_PANE_ID.to_string()))
    };
    Ok(SelfEnv {
        handle: pick(ctx.focused_pane_id)?,
        workspace_id: Some(pick(ctx.workspace_id)?),
        tab_id: Some(pick(ctx.tab_id)?),
        from_plugin_context: true,
    })
}

/// Everything `whoami` needs: env identity, live records, resolved scope.
#[derive(Debug, Clone)]
pub struct SelfIdentity {
    /// Identity parsed from the backend env vars.
    pub env: SelfEnv,
    /// The pane/terminal record for the self handle (`AgentRuntime::
    /// get_pane`) — present even for agent-less shells, which still carry
    /// `cwd`/`terminal_id`/`name`.
    pub pane: RuntimeAgent,
    /// The agent record (`AgentRuntime::get_agent`) when this handle
    /// hosts a known agent. `None` for plain shells/terminals or when the
    /// backend reports the handle as not found.
    pub agent: Option<RuntimeAgent>,
    /// Workspace display hint: the env id on herdr, else the `workspace`
    /// terminal label on hc. `None` when neither source has one.
    pub workspace: Option<String>,
    /// Tab display hint: the env id on herdr, else the `tab` terminal
    /// label on hc.
    pub tab: Option<String>,
    /// Canonical project path the scope was derived from.
    pub canonical_path: PathBuf,
    /// The resolved project scope.
    pub scope: ResolvedScope,
}

/// Resolve this process's identity against a live backend.
///
/// Flow:
/// 1. Parse the backend env vars ([`self_env_from`]) — herdr triple,
///    plugin-context JSON, or `HC_TERMINAL_ID`.
/// 2. `AgentRuntime::get_pane` for the handle — the pane/terminal record
///    (hard error when the handle does not resolve at all).
/// 3. `AgentRuntime::get_agent` — tolerated as `None` on
///    [`RuntimeError::NotFound`] (plain shells have no agent record).
/// 4. Scope resolution prefers the agent record's cwd fields, then the
///    pane record's; if both are empty, retries once with
///    `AgentRuntime::process_info_cwds` before failing with
///    [`CoreError::ScopeUnresolved`].
pub fn resolve_self(runtime: &dyn AgentRuntime, env: &EnvLookup<'_>) -> Result<SelfIdentity> {
    let ids = self_env_from(env)?;

    let pane = runtime.get_pane(&ids.handle)?;

    let agent = match runtime.get_agent(&ids.handle) {
        Ok(agent) => Some(agent),
        Err(RuntimeError::NotFound { .. }) => None,
        Err(err) => return Err(err.into()),
    };

    // Prefer the agent record's cwd fields (agent-aware reporting may be
    // more current), then the pane record's — matching the historical
    // agent-then-pane order. On ScopeUnresolved, retry once with the
    // backend's process-info cwds before failing.
    let primary = agent.as_ref().unwrap_or(&pane);
    let scope = match resolve_scope_for_runtime(primary, &[]) {
        Ok(scope) => scope,
        Err(CoreError::ScopeUnresolved(_)) => {
            let cwds = runtime.process_info_cwds(&ids.handle)?;
            resolve_scope_for_runtime(primary, &cwds)?
        }
        Err(err) => return Err(err),
    };

    // Workspace/tab hints: env ids win; on hc (no such env vars) fall
    // back to `workspace`/`tab` labels carried in the records' `extra`
    // (agent record first, then the pane/terminal record).
    let hint = |key: &str| {
        [agent.as_ref(), Some(&pane)]
            .into_iter()
            .flatten()
            .find_map(|r| r.extra.get(key).filter(|v| !v.is_empty()).cloned())
    };
    let workspace = ids.workspace_id.clone().or_else(|| hint("workspace"));
    let tab = ids.tab_id.clone().or_else(|| hint("tab"));

    Ok(SelfIdentity {
        env: ids,
        pane,
        agent,
        workspace,
        tab,
        canonical_path: scope.canonical_path.clone(),
        scope,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use herdr_runtime::{
        AgentRuntime, EnvLookup as RuntimeEnvLookup, EventStream, RuntimeAgent, RuntimeError,
        RuntimeStatus, SelfIdentity as RuntimeSelfIdentity, SendOutcome, WaitOutcome, WaitRequest,
    };
    use std::collections::HashMap;
    use std::ffi::OsString;
    #[cfg(unix)]
    use std::io::{BufRead, BufReader, Write};
    #[cfg(unix)]
    use std::os::unix::net::UnixListener;
    #[cfg(unix)]
    use std::path::Path;
    #[cfg(unix)]
    use std::sync::mpsc;
    #[cfg(unix)]
    use std::thread;

    fn env<'a>(map: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        let owned: HashMap<String, OsString> = map
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        move |key| owned.get(key).cloned()
    }

    // ----- env parsing ------------------------------------------------------

    #[test]
    fn self_env_requires_all_three_herdr_vars() {
        let full = env(&[
            ("HERDR_PANE_ID", "w1:p1"),
            ("HERDR_WORKSPACE_ID", "w1"),
            ("HERDR_TAB_ID", "w1:t1"),
        ]);
        let ids = self_env_from(&full).unwrap();
        assert_eq!(ids.handle, "w1:p1");
        assert_eq!(ids.workspace_id.as_deref(), Some("w1"));
        assert_eq!(ids.tab_id.as_deref(), Some("w1:t1"));
        assert!(!ids.from_plugin_context);
    }

    #[test]
    fn self_env_hc_terminal_id_alone() {
        let e = env(&[(ENV_HC_TERMINAL_ID, "t7")]);
        let ids = self_env_from(&e).unwrap();
        assert_eq!(ids.handle, "t7");
        assert_eq!(ids.workspace_id, None);
        assert_eq!(ids.tab_id, None);
        assert!(!ids.from_plugin_context);
    }

    #[test]
    fn self_env_hc_remote_address() {
        let e = env(&[(ENV_HC_TERMINAL_ID, "machine/t7")]);
        let ids = self_env_from(&e).unwrap();
        assert_eq!(ids.handle, "machine/t7");
    }

    #[test]
    fn self_env_herdr_wins_over_hc() {
        let e = env(&[
            ("HERDR_PANE_ID", "w1:p1"),
            ("HERDR_WORKSPACE_ID", "w1"),
            ("HERDR_TAB_ID", "w1:t1"),
            (ENV_HC_TERMINAL_ID, "t7"),
        ]);
        let ids = self_env_from(&e).unwrap();
        assert_eq!(ids.handle, "w1:p1");
        assert_eq!(ids.workspace_id.as_deref(), Some("w1"));
    }

    #[test]
    fn self_env_partial_herdr_falls_through_to_hc() {
        let e = env(&[("HERDR_PANE_ID", "w1:p1"), (ENV_HC_TERMINAL_ID, "t7")]);
        let ids = self_env_from(&e).unwrap();
        assert_eq!(ids.handle, "t7");
    }

    #[test]
    fn self_env_missing_var_is_missing_env_error() {
        for missing in [ENV_PANE_ID, ENV_WORKSPACE_ID, ENV_TAB_ID] {
            let vars: Vec<(&str, &str)> = [
                (ENV_PANE_ID, "w1:p1"),
                (ENV_WORKSPACE_ID, "w1"),
                (ENV_TAB_ID, "w1:t1"),
            ]
            .into_iter()
            .filter(|(k, _)| *k != missing)
            .collect();
            let e = env(&vars);
            let err = self_env_from(&e).unwrap_err();
            assert!(
                matches!(&err, CoreError::MissingEnv(v) if v == missing),
                "expected MissingEnv({missing}), got {err:?}"
            );
        }
    }

    #[test]
    fn self_env_empty_value_is_missing() {
        let e = env(&[
            ("HERDR_PANE_ID", ""),
            ("HERDR_WORKSPACE_ID", "w1"),
            ("HERDR_TAB_ID", "w1:t1"),
        ]);
        assert!(matches!(
            self_env_from(&e),
            Err(CoreError::MissingEnv(v)) if v == ENV_PANE_ID
        ));
    }

    #[test]
    fn self_env_falls_back_to_plugin_context_json() {
        // Plugin panes/popups carry HERDR_PLUGIN_CONTEXT_JSON naming the
        // underlying tiled pane.
        let e = env(&[(
            ENV_PLUGIN_CONTEXT_JSON,
            r#"{"workspace_id":"w2","tab_id":"w2:t9","focused_pane_id":"w2:p1"}"#,
        )]);
        let ids = self_env_from(&e).unwrap();
        assert_eq!(ids.handle, "w2:p1");
        assert_eq!(ids.workspace_id.as_deref(), Some("w2"));
        assert_eq!(ids.tab_id.as_deref(), Some("w2:t9"));
        assert!(ids.from_plugin_context);
    }

    #[test]
    fn self_env_plugin_context_wins_over_direct_vars() {
        // Plugin panes get BOTH: their own HERDR_*_ID vars (the plugin
        // surface's pane) and the context JSON (the focused agent pane).
        // The context must win — the plugin pane id is never a messaging
        // identity.
        let e = env(&[
            ("HERDR_PANE_ID", "w1:pPlugin"),
            ("HERDR_WORKSPACE_ID", "w1"),
            ("HERDR_TAB_ID", "w1:t1"),
            (
                ENV_PLUGIN_CONTEXT_JSON,
                r#"{"workspace_id":"w2","tab_id":"w2:t9","focused_pane_id":"w2:p1"}"#,
            ),
        ]);
        let ids = self_env_from(&e).unwrap();
        assert_eq!(ids.handle, "w2:p1");
        assert_eq!(ids.workspace_id.as_deref(), Some("w2"));
        assert_eq!(ids.tab_id.as_deref(), Some("w2:t9"));
        assert!(ids.from_plugin_context);
    }

    #[test]
    fn self_env_malformed_plugin_context_falls_back_to_direct_vars() {
        let e = env(&[
            ("HERDR_PANE_ID", "w1:p1"),
            ("HERDR_WORKSPACE_ID", "w1"),
            ("HERDR_TAB_ID", "w1:t1"),
            (ENV_PLUGIN_CONTEXT_JSON, "not json"),
        ]);
        let ids = self_env_from(&e).unwrap();
        assert_eq!(ids.handle, "w1:p1");
        assert!(!ids.from_plugin_context);
    }

    #[test]
    fn self_env_incomplete_plugin_context_falls_back_to_direct_vars() {
        let e = env(&[
            ("HERDR_PANE_ID", "w1:p1"),
            ("HERDR_WORKSPACE_ID", "w1"),
            ("HERDR_TAB_ID", "w1:t1"),
            (ENV_PLUGIN_CONTEXT_JSON, r#"{"workspace_id":"w2"}"#),
        ]);
        let ids = self_env_from(&e).unwrap();
        assert_eq!(ids.handle, "w1:p1");
        assert!(!ids.from_plugin_context);
    }

    #[test]
    fn self_env_plugin_context_missing_fields_is_missing_env() {
        let e = env(&[(ENV_PLUGIN_CONTEXT_JSON, r#"{"workspace_id":"w2"}"#)]);
        assert!(matches!(self_env_from(&e), Err(CoreError::MissingEnv(_))));
    }

    #[test]
    fn self_env_plugin_context_malformed_is_invalid_env() {
        let e = env(&[(ENV_PLUGIN_CONTEXT_JSON, "not json")]);
        assert!(matches!(self_env_from(&e), Err(CoreError::InvalidEnv(_))));
    }

    // ----- mock runtime ------------------------------------------------------

    /// Minimal [`AgentRuntime`] for identity tests: canned pane/agent
    /// records plus optional process-info cwds. `get_pane` prefers the
    /// pane record and falls back to the agent record — the herdr shape
    /// where shells have a pane but no agent.
    struct MockRuntime {
        pane: Option<RuntimeAgent>,
        agent: Option<RuntimeAgent>,
        cwds: Vec<String>,
    }

    impl MockRuntime {
        fn new(handle: &str) -> Self {
            Self {
                pane: Some(runtime_agent(handle, None, None)),
                agent: None,
                cwds: Vec::new(),
            }
        }
    }

    fn runtime_agent(
        handle: &str,
        cwd: Option<&str>,
        foreground_cwd: Option<&str>,
    ) -> RuntimeAgent {
        RuntimeAgent {
            handle: handle.to_string(),
            terminal_id: Some(format!("term-{handle}")),
            kind: None,
            name: None,
            cwd: cwd.map(str::to_string),
            foreground_cwd: foreground_cwd.map(str::to_string),
            status: RuntimeStatus::Idle,
            interactive_ready: true,
            launch_pending: false,
            extra: Default::default(),
        }
    }

    impl AgentRuntime for MockRuntime {
        fn list_agents(&self) -> std::result::Result<Vec<RuntimeAgent>, RuntimeError> {
            Ok(Vec::new())
        }

        fn get_agent(&self, handle: &str) -> std::result::Result<RuntimeAgent, RuntimeError> {
            self.agent.clone().ok_or_else(|| RuntimeError::NotFound {
                handle: handle.to_string(),
            })
        }

        fn get_pane(&self, handle: &str) -> std::result::Result<RuntimeAgent, RuntimeError> {
            self.pane
                .clone()
                .or_else(|| self.agent.clone())
                .ok_or_else(|| RuntimeError::NotFound {
                    handle: handle.to_string(),
                })
        }

        fn send_prompt(
            &self,
            _handle: &str,
            _text: &str,
        ) -> std::result::Result<SendOutcome, RuntimeError> {
            Err(RuntimeError::Unsupported("mock".into()))
        }

        fn wait_agent(
            &self,
            _handle: &str,
            _req: WaitRequest,
        ) -> std::result::Result<WaitOutcome, RuntimeError> {
            Err(RuntimeError::Unsupported("mock".into()))
        }

        fn read_agent(
            &self,
            _handle: &str,
            _lines: u32,
        ) -> std::result::Result<String, RuntimeError> {
            Err(RuntimeError::Unsupported("mock".into()))
        }

        fn send_keys(
            &self,
            _handle: &str,
            _keys: &[String],
        ) -> std::result::Result<(), RuntimeError> {
            Err(RuntimeError::Unsupported("mock".into()))
        }

        fn self_identity(
            &self,
            _env: &RuntimeEnvLookup<'_>,
        ) -> std::result::Result<RuntimeSelfIdentity, RuntimeError> {
            Ok(RuntimeSelfIdentity {
                handle: None,
                agent: None,
            })
        }

        fn subscribe_events(
            &self,
            _watched: &[String],
        ) -> std::result::Result<EventStream, RuntimeError> {
            Err(RuntimeError::Unsupported("mock".into()))
        }

        fn process_info_cwds(
            &self,
            _handle: &str,
        ) -> std::result::Result<Vec<String>, RuntimeError> {
            Ok(self.cwds.clone())
        }
    }

    #[test]
    fn resolve_self_hc_terminal_with_labels() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        std::fs::create_dir(&project).unwrap();
        let cwd = project.to_string_lossy().to_string();

        let mut rt = MockRuntime::new("t7");
        rt.pane = Some({
            let mut a = runtime_agent("t7", Some(&cwd), None);
            a.extra.insert("workspace".into(), "ws-9".into());
            a.extra.insert("tab".into(), "tab-3".into());
            a
        });
        rt.agent = Some(runtime_agent("t7", Some(&cwd), None));

        let e = env(&[(ENV_HC_TERMINAL_ID, "t7")]);
        let identity = resolve_self(&rt, &e).unwrap();

        assert_eq!(identity.env.handle, "t7");
        assert!(identity.agent.is_some());
        // hc has no workspace/tab env vars — labels supply the hints.
        assert_eq!(identity.workspace.as_deref(), Some("ws-9"));
        assert_eq!(identity.tab.as_deref(), Some("tab-3"));
        assert_eq!(
            identity.canonical_path,
            std::fs::canonicalize(&project).unwrap()
        );
        assert_eq!(identity.scope.scope_key.as_str().len(), 32);
    }

    #[test]
    fn resolve_self_shell_pane_has_no_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy().to_string();

        let mut rt = MockRuntime::new("w1:p1");
        rt.pane = Some(runtime_agent("w1:p1", Some(&cwd), None));
        // agent stays None → get_agent returns NotFound.

        let e = env(&[
            ("HERDR_PANE_ID", "w1:p1"),
            ("HERDR_WORKSPACE_ID", "w1"),
            ("HERDR_TAB_ID", "w1:t1"),
        ]);
        let identity = resolve_self(&rt, &e).unwrap();
        assert!(identity.agent.is_none());
        assert_eq!(identity.workspace.as_deref(), Some("w1"));
        assert_eq!(identity.tab.as_deref(), Some("w1:t1"));
        assert_eq!(identity.pane.handle, "w1:p1");
    }

    #[test]
    fn resolve_self_falls_back_to_process_info_cwds() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("procproj");
        std::fs::create_dir(&project).unwrap();
        let cwd = project.to_string_lossy().to_string();

        let mut rt = MockRuntime::new("t3");
        // Records carry no cwd at all → only process-info cwds resolve.
        rt.pane = Some(runtime_agent("t3", None, None));
        rt.agent = Some(runtime_agent("t3", None, None));
        rt.cwds = vec![cwd];

        let e = env(&[(ENV_HC_TERMINAL_ID, "t3")]);
        let identity = resolve_self(&rt, &e).unwrap();
        assert_eq!(
            identity.canonical_path,
            std::fs::canonicalize(&project).unwrap()
        );
    }

    #[test]
    fn resolve_self_scope_unresolved_errors() {
        let rt = MockRuntime::new("t3");
        let e = env(&[(ENV_HC_TERMINAL_ID, "t3")]);
        assert!(matches!(
            resolve_self(&rt, &e),
            Err(CoreError::ScopeUnresolved(_))
        ));
    }

    // ----- fake socket -------------------------------------------------------

    /// Minimal fake Herdr server: accepts one connection per request and
    /// replies with a canned `result` for the request's `method`.
    /// `methods` maps method name → result JSON (the `result` object
    /// including its `type` tag). Runs until the test process exits; the
    /// listener thread is detached.
    #[cfg(unix)]
    fn fake_server(
        socket_path: &Path,
        methods: HashMap<String, serde_json::Value>,
    ) -> mpsc::Receiver<std::io::Result<()>> {
        let _ = std::fs::remove_file(socket_path);
        let listener = UnixListener::bind(socket_path).unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(match stream.try_clone() {
                    Ok(s) => s,
                    Err(_) => break,
                });
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let request: serde_json::Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let id = request
                    .get("id")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let method = request
                    .get("method")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string();
                let response = match methods.get(&method) {
                    Some(result) => serde_json::json!({ "id": id, "result": result }),
                    None => serde_json::json!({
                        "id": id,
                        "error": { "code": "method_not_found", "message": method }
                    }),
                };
                let _ = writeln!(stream, "{response}");
                let _ = stream.flush();
                let _ = tx.send(Ok(()));
            }
        });
        rx
    }

    #[cfg(unix)]
    fn pane_json(cwd: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "type": "pane_info",
            "pane": {
                "pane_id": "w1:p1",
                "workspace_id": "w1",
                "tab_id": "w1:t1",
                "terminal_id": "term-1",
                "agent_status": "idle",
                "revision": 1,
                "focused": true,
                "cwd": cwd,
            }
        })
    }

    #[test]
    #[cfg(unix)]
    fn resolve_self_against_fake_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("herdr.sock");
        let project = tmp.path().join("proj");
        std::fs::create_dir(&project).unwrap();
        let cwd = project.to_string_lossy().to_string();

        let mut methods = HashMap::new();
        methods.insert("pane.get".to_string(), pane_json(Some(&cwd)));
        methods.insert(
            "agent.get".to_string(),
            serde_json::json!({
                "type": "agent_info",
                "agent": {
                    "pane_id": "w1:p1",
                    "workspace_id": "w1",
                    "tab_id": "w1:t1",
                    "terminal_id": "term-1",
                    "agent_status": "working",
                    "revision": 2,
                    "focused": true,
                    "agent": "claude",
                    "name": "builder",
                    "cwd": cwd,
                }
            }),
        );
        let _rx = fake_server(&sock, methods);

        let runtime =
            herdr_runtime::HerdrRuntime::new(herdr_protocol::HerdrClient::from_socket_path(&sock));
        let e = env(&[
            ("HERDR_PANE_ID", "w1:p1"),
            ("HERDR_WORKSPACE_ID", "w1"),
            ("HERDR_TAB_ID", "w1:t1"),
        ]);
        let identity = resolve_self(&runtime, &e).unwrap();

        assert_eq!(identity.env.handle, "w1:p1");
        assert_eq!(identity.pane.handle, "w1:p1");
        assert_eq!(
            identity.agent.as_ref().unwrap().name.as_deref(),
            Some("builder")
        );
        assert_eq!(identity.workspace.as_deref(), Some("w1"));
        assert_eq!(identity.tab.as_deref(), Some("w1:t1"));
        assert_eq!(
            identity.canonical_path,
            std::fs::canonicalize(&project).unwrap()
        );
        assert_eq!(identity.scope.scope_key.as_str().len(), 32);
    }

    #[test]
    #[cfg(unix)]
    fn resolve_self_falls_back_to_process_info() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("herdr.sock");
        let project = tmp.path().join("procproj");
        std::fs::create_dir(&project).unwrap();
        let cwd = project.to_string_lossy().to_string();

        let mut methods = HashMap::new();
        // Pane has NO cwd fields → first resolve fails, then
        // pane.process_info supplies the foreground cwd.
        methods.insert("pane.get".to_string(), pane_json(None));
        methods.insert(
            "agent.get".to_string(),
            serde_json::json!({
                "type": "agent_info",
                "agent": {
                    "pane_id": "w1:p1",
                    "workspace_id": "w1",
                    "tab_id": "w1:t1",
                    "terminal_id": "term-1",
                    "agent_status": "idle",
                    "revision": 1,
                    "focused": true,
                }
            }),
        );
        methods.insert(
            "pane.process_info".to_string(),
            serde_json::json!({
                "type": "pane_process_info",
                "process_info": {
                    "pane_id": "w1:p1",
                    "shell_pid": 100,
                    "foreground_process_group_id": 200,
                    "tty": "/dev/ttys001",
                    "foreground_processes": [
                        { "pid": 200, "name": "claude", "cwd": cwd }
                    ]
                }
            }),
        );
        let _rx = fake_server(&sock, methods);

        let runtime =
            herdr_runtime::HerdrRuntime::new(herdr_protocol::HerdrClient::from_socket_path(&sock));
        let e = env(&[
            ("HERDR_PANE_ID", "w1:p1"),
            ("HERDR_WORKSPACE_ID", "w1"),
            ("HERDR_TAB_ID", "w1:t1"),
        ]);
        let identity = resolve_self(&runtime, &e).unwrap();
        assert_eq!(
            identity.canonical_path,
            std::fs::canonicalize(&project).unwrap()
        );
    }

    #[test]
    fn resolve_self_without_env_vars_errors_before_runtime() {
        struct PanicRuntime;
        impl AgentRuntime for PanicRuntime {
            fn list_agents(&self) -> std::result::Result<Vec<RuntimeAgent>, RuntimeError> {
                panic!("must not be called")
            }
            fn get_agent(&self, _h: &str) -> std::result::Result<RuntimeAgent, RuntimeError> {
                panic!("must not be called")
            }
            fn send_prompt(
                &self,
                _h: &str,
                _t: &str,
            ) -> std::result::Result<SendOutcome, RuntimeError> {
                panic!("must not be called")
            }
            fn wait_agent(
                &self,
                _h: &str,
                _r: WaitRequest,
            ) -> std::result::Result<WaitOutcome, RuntimeError> {
                panic!("must not be called")
            }
            fn read_agent(&self, _h: &str, _l: u32) -> std::result::Result<String, RuntimeError> {
                panic!("must not be called")
            }
            fn send_keys(&self, _h: &str, _k: &[String]) -> std::result::Result<(), RuntimeError> {
                panic!("must not be called")
            }
            fn self_identity(
                &self,
                _e: &RuntimeEnvLookup<'_>,
            ) -> std::result::Result<RuntimeSelfIdentity, RuntimeError> {
                panic!("must not be called")
            }
            fn subscribe_events(
                &self,
                _watched: &[String],
            ) -> std::result::Result<EventStream, RuntimeError> {
                panic!("must not be called")
            }
        }
        let e = env(&[]);
        assert!(matches!(
            resolve_self(&PanicRuntime, &e),
            Err(CoreError::MissingEnv(_))
        ));
    }
}
