//! Self-identity resolution: which pane/agent/scope is *this* process?
//!
//! Herdr injects `HERDR_PANE_ID`, `HERDR_WORKSPACE_ID`, and `HERDR_TAB_ID`
//! into agent/pane processes. [`self_env_from`] parses them (pure, unit
//! testable); [`resolve_self`] then asks the Herdr socket for the pane and
//! agent records and resolves the project scope — the same path the
//! `herdr-pool whoami` command uses.
//!
//! These env vars identify the *pane*, never a directory: scope still comes
//! exclusively from `cwd`/`foreground_cwd`/`process_info` per
//! [`crate::scope`].

use std::path::PathBuf;

use herdr_plugin::paths::EnvLookup;
use herdr_protocol::{AgentInfo, HerdrClient, PaneInfo};

use crate::error::{CoreError, Result};
use crate::scope::{resolve_scope_for_agent, resolve_scope_for_pane, ResolvedScope};

/// Env var carrying the caller's pane id (`wN:pM`).
pub const ENV_PANE_ID: &str = "HERDR_PANE_ID";
/// Env var carrying the caller's workspace id.
pub const ENV_WORKSPACE_ID: &str = "HERDR_WORKSPACE_ID";
/// Env var carrying the caller's tab id.
pub const ENV_TAB_ID: &str = "HERDR_TAB_ID";

/// Pane identity parsed from the Herdr-injected environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfEnv {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
}

/// Parse the `HERDR_*` identity vars. All three must be present and
/// non-empty; a missing var yields [`CoreError::MissingEnv`] — the process
/// is not running inside a Herdr pane.
pub fn self_env_from(env: &EnvLookup<'_>) -> Result<SelfEnv> {
    Ok(SelfEnv {
        pane_id: require_env(env, ENV_PANE_ID)?,
        workspace_id: require_env(env, ENV_WORKSPACE_ID)?,
        tab_id: require_env(env, ENV_TAB_ID)?,
    })
}

fn require_env(env: &EnvLookup<'_>, var: &str) -> Result<String> {
    env(var)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string_lossy().into_owned())
        .ok_or_else(|| CoreError::MissingEnv(var.to_string()))
}

/// Everything `whoami` needs: env identity, live records, resolved scope.
#[derive(Debug, Clone)]
pub struct SelfIdentity {
    /// Identity parsed from `HERDR_*` env vars.
    pub env: SelfEnv,
    /// The pane record (`pane.current`).
    pub pane: PaneInfo,
    /// The agent record (`agent.get`), when this pane hosts a known agent.
    /// `None` for plain shell panes or when Herdr reports `agent_not_found`.
    pub agent: Option<AgentInfo>,
    /// Canonical project path the scope was derived from.
    pub canonical_path: PathBuf,
    /// The resolved project scope.
    pub scope: ResolvedScope,
}

/// Resolve this process's identity against a live Herdr server.
///
/// Flow:
/// 1. Parse `HERDR_PANE_ID`/`HERDR_WORKSPACE_ID`/`HERDR_TAB_ID`.
/// 2. `pane.current` with the pane id as `caller_pane_id`.
/// 3. `agent.get` for the pane — tolerated as `None` on
///    `agent_not_found` (plain shell panes have no agent record).
/// 4. Scope resolution tries the agent record's cwd fields first, then the
///    pane's; if both are empty, fetches `pane.process_info` once and
///    retries before giving up with [`CoreError::ScopeUnresolved`].
pub fn resolve_self(client: &HerdrClient, env: &EnvLookup<'_>) -> Result<SelfIdentity> {
    let ids = self_env_from(env)?;

    let pane = client.pane_current(Some(&ids.pane_id))?;

    let agent = match client.agent_get(&ids.pane_id) {
        Ok(agent) => Some(agent),
        Err(err) if err.is_agent_not_found() => None,
        Err(err) => return Err(err.into()),
    };

    // Prefer the agent record's cwd fields (agent-aware reporting may be
    // more current), then the pane's. On ScopeUnresolved, retry once with
    // pane.process_info before failing.
    let scope = match agent.as_ref() {
        Some(agent) => resolve_scope_for_agent(agent, None),
        None => resolve_scope_for_pane(&pane, None),
    };
    let scope = match scope {
        Ok(scope) => scope,
        Err(CoreError::ScopeUnresolved(_)) => {
            // Pane record lacks cwd fields — ask the server for the
            // foreground process table and retry.
            let process_info = client.pane_process_info(Some(&pane.pane_id))?;
            match agent.as_ref() {
                Some(agent) => resolve_scope_for_agent(agent, Some(&process_info)),
                None => resolve_scope_for_pane(&pane, Some(&process_info)),
            }?
        }
        Err(err) => return Err(err),
    };

    Ok(SelfIdentity {
        env: ids,
        pane,
        agent,
        canonical_path: scope.canonical_path.clone(),
        scope,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::path::Path;
    use std::sync::mpsc;
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
    fn self_env_requires_all_three_vars() {
        let full = env(&[
            ("HERDR_PANE_ID", "w1:p1"),
            ("HERDR_WORKSPACE_ID", "w1"),
            ("HERDR_TAB_ID", "w1:t1"),
        ]);
        let ids = self_env_from(&full).unwrap();
        assert_eq!(ids.pane_id, "w1:p1");
        assert_eq!(ids.workspace_id, "w1");
        assert_eq!(ids.tab_id, "w1:t1");
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
            "type": "pane_current",
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

        // pane.current → pane with cwd; agent.get → error agent_not_found;
        // pane.process_info → not needed since cwd resolves.
        // The fake server only supports result envelopes, so agent_not_found
        // is simulated by a second listener? Simpler: give agent.get a
        // valid agent_info result too.
        let mut methods = HashMap::new();
        methods.insert("pane.current".to_string(), pane_json(Some(&cwd)));
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

        let client = HerdrClient::from_socket_path(&sock);
        let e = env(&[
            ("HERDR_PANE_ID", "w1:p1"),
            ("HERDR_WORKSPACE_ID", "w1"),
            ("HERDR_TAB_ID", "w1:t1"),
        ]);
        let identity = resolve_self(&client, &e).unwrap();

        assert_eq!(identity.env.pane_id, "w1:p1");
        assert_eq!(identity.pane.pane_id, "w1:p1");
        assert_eq!(
            identity.agent.as_ref().unwrap().name.as_deref(),
            Some("builder")
        );
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
        methods.insert("pane.current".to_string(), pane_json(None));
        // agent.get returns agent_not_found via error envelope — our fake
        // server can't emit per-method error bodies, so omit agent.get:
        // the unknown-method error code is "method_not_found", which is
        // NOT is_agent_not_found → resolve_self would fail. Instead, give
        // agent.get a minimal agent_info with no cwd fields.
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

        let client = HerdrClient::from_socket_path(&sock);
        let e = env(&[
            ("HERDR_PANE_ID", "w1:p1"),
            ("HERDR_WORKSPACE_ID", "w1"),
            ("HERDR_TAB_ID", "w1:t1"),
        ]);
        let identity = resolve_self(&client, &e).unwrap();
        assert_eq!(
            identity.canonical_path,
            std::fs::canonicalize(&project).unwrap()
        );
    }

    #[test]
    #[cfg(unix)]
    fn resolve_self_without_env_vars_errors_before_socket() {
        let tmp = tempfile::tempdir().unwrap();
        // No listener at all: env parsing must fail first.
        let client = HerdrClient::from_socket_path(tmp.path().join("nope.sock"));
        let e = env(&[]);
        assert!(matches!(
            resolve_self(&client, &e),
            Err(CoreError::MissingEnv(_))
        ));
    }
}
