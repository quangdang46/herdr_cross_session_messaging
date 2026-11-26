//! Shared fixtures for unit tests: a [`FakeHerdr`] server plus helpers that
//! script the pane/agent records the CLI resolves against.
//!
//! Wire shapes mirror `herdr-protocol`'s response fixtures (`type`-tagged
//! result objects), so `HerdrClient` decodes them exactly like a real server.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::{Arc, Mutex};

use herdr_protocol::HerdrClient;
use herdr_testkit::{FakeHerdr, HandlerReply};
use serde_json::{json, Value};
use tempfile::TempDir;

use crate::context::Ctx;

/// How many times each scripted responder is queued. FakeHerdr consumes one
/// responder per request; tests run several commands, each of which calls
/// `pane.current`/`agent.get`/`agent.list` once.
const SCRIPTED_DEPTH: usize = 32;

/// A fake Herdr server + isolated state/project dirs.
pub struct Fixture {
    pub server: FakeHerdr,
    pub state: TempDir,
    pub project: TempDir,
    /// `agent.get` target (pane id) → agent record, consulted live by the
    /// scripted responder so tests can register agents in any order.
    agent_gets: Arc<Mutex<HashMap<String, Value>>>,
}

impl Fixture {
    pub fn new() -> Self {
        let agent_gets: Arc<Mutex<HashMap<String, Value>>> = Arc::new(Mutex::new(HashMap::new()));
        let server = FakeHerdr::start().expect("fake herdr");
        // One responder shape for every pane: look the target up in the
        // shared map at request time. Queueing per-pane closures instead
        // would make answers depend on FIFO consumption order.
        for _ in 0..SCRIPTED_DEPTH {
            let agent_gets = Arc::clone(&agent_gets);
            server.on("agent.get").then(move |req| {
                let target = req
                    .params
                    .get("target")
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                let agent = agent_gets.lock().unwrap().get(target).cloned();
                match agent {
                    Some(agent) => {
                        HandlerReply::Result(json!({"type": "agent_info", "agent": agent}))
                    }
                    None => HandlerReply::Error {
                        code: "agent_not_found".into(),
                        message: format!("no agent for {target}"),
                    },
                }
            });
        }
        Self {
            server,
            state: TempDir::new().expect("state dir"),
            project: TempDir::new().expect("project dir"),
            agent_gets,
        }
    }

    /// Canonical project path string (what `cwd` fields must contain for
    /// scope resolution to land in this fixture's pool).
    pub fn project_cwd(&self) -> String {
        std::fs::canonicalize(self.project.path())
            .expect("canonicalize project")
            .to_string_lossy()
            .into_owned()
    }

    /// CLI context bound to the fake socket and temp state dir.
    pub fn ctx(&self) -> Ctx {
        Ctx::for_test(
            false,
            HerdrClient::from_socket_path(self.server.socket_path()),
            self.state.path().to_path_buf(),
        )
    }

    /// Env map for a pane identity (HERDR_PANE_ID/WORKSPACE_ID/TAB_ID).
    pub fn env_map(&self, pane_id: &str) -> HashMap<String, OsString> {
        let ws = pane_id.split(':').next().unwrap_or("w1");
        [
            ("HERDR_PANE_ID", pane_id),
            ("HERDR_WORKSPACE_ID", ws),
            ("HERDR_TAB_ID", &format!("{ws}:t1")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), OsString::from(v)))
        .collect()
    }

    /// Script `pane.current` so `resolve_self` sees `self_pane`, and
    /// register an agent record for `agent.get` on that pane.
    ///
    /// FakeHerdr consumes one scripted responder per request, so each
    /// responder is queued [`SCRIPTED_DEPTH`] times — enough for the
    /// several socket calls a single test makes across commands.
    pub fn script_self(&self, self_pane: &str, self_terminal: &str, cwd: &str) {
        let pane_result = pane_current_result(self_pane, self_terminal, cwd);
        for _ in 0..SCRIPTED_DEPTH {
            self.server.on("pane.current").respond(pane_result.clone());
        }
        self.script_agent_get(
            self_pane,
            agent_value(
                self_pane,
                self_terminal,
                "claude",
                Some("self"),
                cwd,
                "idle",
            ),
        );
    }

    /// Register `agent` (an [`agent_value`]) as the `agent.get` answer for
    /// `pane_id`. Safe to call for any number of panes — the responder
    /// resolves targets from a shared map at request time.
    pub fn script_agent_get(&self, pane_id: &str, agent: Value) {
        self.agent_gets
            .lock()
            .unwrap()
            .insert(pane_id.to_string(), agent);
    }

    /// Script `agent.list` with the given agent objects (see [`agent_value`]).
    pub fn script_agent_list(&self, agents: Vec<Value>) {
        for _ in 0..SCRIPTED_DEPTH {
            self.server.on("agent.list").respond(json!({
                "type": "agent_list",
                "agents": agents
            }));
        }
    }

    /// Convenience: script self + a two-agent list (self + one peer) in
    /// `project_cwd()`, with `agent.get` answers for both panes.
    pub fn script_two_agents(&self) {
        let cwd = self.project_cwd();
        self.script_self("w1:p1", "term_self", &cwd);
        self.script_agent_get(
            "w1:p2",
            agent_value("w1:p2", "term_peer", "codex", Some("peer"), &cwd, "idle"),
        );
        self.script_agent_list(vec![
            agent_value("w1:p1", "term_self", "claude", Some("self"), &cwd, "idle"),
            agent_value("w1:p2", "term_peer", "codex", Some("peer"), &cwd, "idle"),
        ]);
    }
}

/// An `AgentInfo` wire object (as embedded in `agent_info`/`agent_list`).
pub fn agent_value(
    pane_id: &str,
    terminal_id: &str,
    kind: &str,
    name: Option<&str>,
    cwd: &str,
    status: &str,
) -> Value {
    let ws = pane_id.split(':').next().unwrap_or("w1");
    json!({
        "pane_id": pane_id,
        "workspace_id": ws,
        "tab_id": format!("{ws}:t1"),
        "terminal_id": terminal_id,
        "agent_status": status,
        "revision": 1,
        "focused": false,
        "agent": kind,
        "name": name,
        "cwd": cwd,
        "foreground_cwd": cwd,
        "interactive_ready": true,
    })
}

/// `pane.current` result for a pane launched in `cwd`.
pub fn pane_current_result(pane_id: &str, terminal_id: &str, cwd: &str) -> Value {
    let ws = pane_id.split(':').next().unwrap_or("w1");
    json!({
        "type": "pane_current",
        "pane": {
            "pane_id": pane_id,
            "workspace_id": ws,
            "tab_id": format!("{ws}:t1"),
            "terminal_id": terminal_id,
            "agent_status": "idle",
            "revision": 1,
            "focused": true,
            "cwd": cwd,
            "foreground_cwd": cwd,
        }
    })
}
