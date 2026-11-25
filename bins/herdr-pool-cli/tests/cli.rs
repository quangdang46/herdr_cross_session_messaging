//! End-to-end CLI tests: spawn the real `herdr-pool` binary against a
//! `FakeHerdr` socket and assert on stdout JSON, exit codes, and the
//! recorded `agent.prompt` envelope.
//!
//! Each test runs one CLI invocation per process, so a single scripted
//! responder per method suffices (FakeHerdr consumes responders FIFO).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::ffi::OsString;
use std::process::{Command, Output};

use herdr_testkit::FakeHerdr;
use serde_json::{json, Value};
use tempfile::TempDir;

struct Fx {
    server: FakeHerdr,
    state: TempDir,
    project: TempDir,
}

impl Fx {
    fn new() -> Self {
        Self {
            server: FakeHerdr::start().unwrap(),
            state: TempDir::new().unwrap(),
            project: TempDir::new().unwrap(),
        }
    }

    /// Fake a *running* router daemon (pidfile + instance lock naming this
    /// live test process) so `ensure_daemon` inside `send`/`reply`
    /// short-circuits instead of spawning a real `herdr-routerd` — whose
    /// `start` banner would pollute the CLI's captured stdout and whose
    /// `run` child could connect to a real Herdr socket.
    fn seed_running_daemon(&self) {
        let pid = std::process::id().to_string();
        std::fs::write(self.state.path().join("routerd.pid"), format!("{pid}\n0\n")).unwrap();
        std::fs::create_dir(self.state.path().join("routerd.lock")).unwrap();
        std::fs::write(self.state.path().join("routerd.lock").join("pid"), &pid).unwrap();
    }

    fn project_cwd(&self) -> String {
        std::fs::canonicalize(self.project.path())
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    fn env(&self, pane: &str) -> HashMap<String, OsString> {
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

    /// Base command with socket/state-dir wired to the fixture. Herdr env
    /// vars are cleared first so the child never inherits the test runner's
    /// own pane identity.
    fn cmd(&self, env: &HashMap<String, OsString>) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_herdr-pool"));
        cmd.arg("--socket-path")
            .arg(self.server.socket_path())
            .arg("--state-dir")
            .arg(self.state.path());
        for var in [
            "HERDR_PANE_ID",
            "HERDR_WORKSPACE_ID",
            "HERDR_TAB_ID",
            "HERDR_ENV",
            "HERDR_SOCKET_PATH",
            "HERDR_SESSION",
            "HERDR_PLUGIN_STATE_DIR",
            "HERDR_POOL_STATE_DIR",
        ] {
            cmd.env_remove(var);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd
    }

    fn run(&self, env: &HashMap<String, OsString>, args: &[&str]) -> Output {
        self.cmd(env).args(args).output().unwrap()
    }

    fn stdout_json(out: &Output) -> Value {
        assert!(
            out.status.success(),
            "status: {:?}\nstdout: {}\nstderr: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("stdout is JSON")
    }
}

fn agent_json(
    pane: &str,
    term: &str,
    kind: &str,
    name: Option<&str>,
    cwd: &str,
    status: &str,
) -> Value {
    let ws = pane.split(':').next().unwrap_or("w1");
    json!({
        "pane_id": pane, "workspace_id": ws, "tab_id": format!("{ws}:t1"),
        "terminal_id": term, "agent_status": status, "revision": 1,
        "focused": false, "agent": kind, "name": name, "cwd": cwd,
        "foreground_cwd": cwd, "interactive_ready": true,
    })
}

/// Scripted responders are consumed FIFO by FakeHerdr; a test may run
/// several CLI invocations against the same server, so queue extras.
const SCRIPT_DEPTH: usize = 16;

fn script_self(server: &FakeHerdr, pane: &str, term: &str, cwd: &str) {
    let ws = pane.split(':').next().unwrap_or("w1");
    let pane_result = json!({
        "type": "pane_current",
        "pane": {
            "pane_id": pane, "workspace_id": ws, "tab_id": format!("{ws}:t1"),
            "terminal_id": term, "agent_status": "idle", "revision": 1,
            "focused": true, "cwd": cwd, "foreground_cwd": cwd,
        }
    });
    for _ in 0..SCRIPT_DEPTH {
        server.on("pane.current").respond(pane_result.clone());
    }
    let agent = agent_json(pane, term, "claude", Some("self"), cwd, "idle");
    let pane = pane.to_string();
    for _ in 0..SCRIPT_DEPTH {
        let pane = pane.clone();
        let agent = agent.clone();
        server.on("agent.get").then(move |req| {
            let target = req
                .params
                .get("target")
                .and_then(|t| t.as_str())
                .unwrap_or("");
            if target == pane {
                herdr_testkit::HandlerReply::Result(
                    json!({"type": "agent_info", "agent": agent.clone()}),
                )
            } else {
                herdr_testkit::HandlerReply::Error {
                    code: "agent_not_found".into(),
                    message: "no agent".into(),
                }
            }
        });
    }
}

fn script_two_agents(f: &Fx) {
    let cwd = f.project_cwd();
    script_self(&f.server, "w1:p1", "term_self", &cwd);
    let list = json!({
        "type": "agent_list",
        "agents": [
            agent_json("w1:p1", "term_self", "claude", Some("self"), &cwd, "idle"),
            agent_json("w1:p2", "term_peer", "codex", Some("peer"), &cwd, "idle"),
        ]
    });
    for _ in 0..SCRIPT_DEPTH {
        f.server.on("agent.list").respond(list.clone());
    }
}

// ---------------------------------------------------------------------------
// clap surface
// ---------------------------------------------------------------------------

#[test]
fn every_subcommand_help_works() {
    for args in [
        &["whoami", "--help"][..],
        &["peers", "--help"][..],
        &["send", "--help"][..],
        &["inbox", "--help"][..],
        &["inbox", "read", "--help"][..],
        &["reply", "--help"][..],
        &["threads", "--help"][..],
        &["message", "--help"][..],
        &["message", "read", "--help"][..],
        &["message", "get", "--help"][..],
        &["daemon", "--help"][..],
        &["daemon", "start", "--help"][..],
        &["doctor", "--help"][..],
        &["install", "--help"][..],
        &["uninstall", "--help"][..],
        &["version", "--help"][..],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_herdr-pool"))
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn clap_rejects_missing_args() {
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-pool"))
        .args(["send"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn clap_rejects_unknown_subcommand() {
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-pool"))
        .args(["bogus"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

#[test]
fn whoami_json() {
    let f = Fx::new();
    let cwd = f.project_cwd();
    script_self(&f.server, "w1:p1", "term_self", &cwd);
    f.server
        .on("agent.list")
        .respond(json!({"type":"agent_list","agents":[]}));

    let env = f.env("w1:p1");
    let out = f.run(&env, &["--json", "whoami"]);
    let v = Fx::stdout_json(&out);
    assert!(v["agent_id"].as_str().unwrap().starts_with("ag_"));
    assert_eq!(v["pane_id"], "w1:p1");
    assert_eq!(v["agent_kind"], "claude");
    assert_eq!(v["name"], "self");
    assert_eq!(v["status"], "idle");
    assert_eq!(v["scope_key"].as_str().unwrap().len(), 32);
}

#[test]
fn whoami_outside_herdr_exits_2() {
    let f = Fx::new();
    let out = f.run(&HashMap::new(), &["whoami"]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("HERDR_PANE_ID"), "{err}");
}

#[test]
fn peers_json_lists_peer() {
    let f = Fx::new();
    script_two_agents(&f);
    let env = f.env("w1:p1");
    let out = f.run(&env, &["--json", "peers"]);
    let v = Fx::stdout_json(&out);
    let peers = v["peers"].as_array().unwrap();
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0]["name"], "peer");
    assert_eq!(peers[0]["agent_kind"], "codex");
}

#[test]
fn send_json_delivers_and_records_prompt() {
    let f = Fx::new();
    f.seed_running_daemon();
    script_two_agents(&f);
    let env = f.env("w1:p1");
    let out = f.run(&env, &["--json", "send", "peer", "review please"]);
    let v = Fx::stdout_json(&out);
    assert_eq!(v["message"]["status"], "delivered");
    assert!(v["message"]["id"].as_str().unwrap().starts_with("msg_"));

    let prompts = f.server.requests_for("agent.prompt");
    assert_eq!(prompts.len(), 1);
    let text = prompts[0].params["text"].as_str().unwrap();
    assert!(text.starts_with("[herdr-pool] msg msg_"));
    assert!(text.contains("thread th_"));
    assert!(text.contains("review please"));
    assert!(text.contains("Reply: herdr-pool reply msg_"));
}

#[test]
fn send_blocked_exits_0_with_failed_status() {
    let f = Fx::new();
    f.seed_running_daemon();
    script_two_agents(&f);
    f.server.fail_next("agent.prompt", "agent_blocked", "busy");
    let env = f.env("w1:p1");
    let out = f.run(&env, &["--json", "send", "peer", "hi"]);
    let v = Fx::stdout_json(&out);
    assert_eq!(v["message"]["status"], "failed");
    assert!(v["message"]["next_retry_at"].is_i64());
}

#[test]
fn send_unknown_peer_exits_2_json_error() {
    let f = Fx::new();
    f.seed_running_daemon();
    script_two_agents(&f);
    let env = f.env("w1:p1");
    let out = f.run(&env, &["--json", "send", "ghost", "hi"]);
    assert_eq!(out.status.code(), Some(2));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["error"]["kind"], "usage");
}

#[test]
fn inbox_and_message_get_json() {
    let f = Fx::new();
    f.seed_running_daemon();
    script_two_agents(&f);
    let env = f.env("w1:p1");

    // peer → self message via send from the peer's perspective is awkward;
    // use send self→peer then read it back as the peer would. Simpler:
    // send self→peer, then `message get` the id from the JSON output.
    let out = f.run(&env, &["--json", "send", "peer", "ping"]);
    let v = Fx::stdout_json(&out);
    let msg_id = v["message"]["id"].as_str().unwrap().to_string();

    let out = f.run(&env, &["--json", "message", "get", &msg_id]);
    let v = Fx::stdout_json(&out);
    assert_eq!(v["id"], msg_id);
    assert_eq!(v["content"], "ping");
    assert_eq!(v["status"], "delivered");
}

#[test]
fn version_json() {
    let f = Fx::new();
    let out = f.run(&HashMap::new(), &["--json", "version"]);
    let v = Fx::stdout_json(&out);
    assert_eq!(v["name"], "herdr-pool");
    assert!(v["version"].as_str().unwrap().contains('.'));
}

#[test]
fn daemon_status_json_when_stopped() {
    let f = Fx::new();
    let out = f.run(&HashMap::new(), &["--json", "daemon", "status"]);
    let v = Fx::stdout_json(&out);
    assert_eq!(v["running"], false);
}

#[test]
fn doctor_json_has_checks() {
    let f = Fx::new();
    let cwd = f.project_cwd();
    script_self(&f.server, "w1:p1", "term_self", &cwd);
    let env = f.env("w1:p1");
    let out = f.run(&env, &["--json", "doctor"]);
    // doctor may exit non-zero only on `fail` checks; socket+env are ok here.
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let checks = v["checks"].as_array().unwrap();
    assert!(checks.len() >= 6);
    let names: Vec<_> = checks.iter().filter_map(|c| c["name"].as_str()).collect();
    for expected in [
        "env.pane",
        "socket.ping",
        "scope",
        "pool.db",
        "daemon",
        "versions",
    ] {
        assert!(names.contains(&expected), "missing check {expected}");
    }
    let _ = out.status;
}

#[test]
fn install_then_uninstall_project_scope() {
    let f = Fx::new();
    let project = TempDir::new().unwrap();
    let out = f.run(
        &HashMap::new(),
        &[
            "--json",
            "install",
            "--project",
            project.path().to_str().unwrap(),
        ],
    );
    let v = Fx::stdout_json(&out);
    assert_eq!(v["scope"], "project");
    assert_eq!(v["all_ok"], true);
    assert!(project
        .path()
        .join(".agents/skills/herdr-messaging/SKILL.md")
        .is_file());
    assert!(project
        .path()
        .join(".claude/skills/herdr-messaging/SKILL.md")
        .is_file());

    let out = f.run(
        &HashMap::new(),
        &[
            "--json",
            "uninstall",
            "--project",
            project.path().to_str().unwrap(),
        ],
    );
    Fx::stdout_json(&out);
    assert!(!project
        .path()
        .join(".agents/skills/herdr-messaging")
        .exists());
}

#[test]
fn cross_scope_isolation_end_to_end() {
    let f = Fx::new();
    f.seed_running_daemon();
    // Self in project A; the only other agent lives in project B.
    let cwd_a = f.project_cwd();
    let other = TempDir::new().unwrap();
    let cwd_b = std::fs::canonicalize(other.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    script_self(&f.server, "w1:p1", "term_self", &cwd_a);
    let list = json!({
        "type": "agent_list",
        "agents": [
            agent_json("w1:p1", "term_self", "claude", Some("self"), &cwd_a, "idle"),
            agent_json("w1:p9", "term_b", "codex", Some("peer"), &cwd_b, "idle"),
        ]
    });
    for _ in 0..SCRIPT_DEPTH {
        f.server.on("agent.list").respond(list.clone());
    }

    let env = f.env("w1:p1");
    let out = f.run(&env, &["--json", "peers"]);
    let v = Fx::stdout_json(&out);
    assert_eq!(v["peers"].as_array().unwrap().len(), 0);

    let out = f.run(&env, &["--json", "send", "peer", "hi"]);
    assert_eq!(out.status.code(), Some(2));
}
