//! M6 end-to-end tests: messaging happy path + project isolation, driven
//! through the real `herdr-pool` binary against a `FakeHerdr` socket.
//!
//! Two (or more) simulated agents share one `--state-dir` (same pool
//! storage) but carry different `HERDR_PANE_ID`/`terminal_id` identities,
//! exactly like two panes in one Herdr session. Each test uses a fresh
//! tempdir state dir so pools never leak between tests.
//!
//! The fixture seeds a fake *running* daemon (pidfile + instance lock
//! pointing at the test process) so `ensure_daemon` inside `send`/`reply`
//! sees `DaemonStatus::Running` and never spawns a real `herdr-routerd`
//! against the fake socket.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Output};

use herdr_testkit::{FakeHerdr, HandlerReply};
use serde_json::{json, Value};
use tempfile::TempDir;

/// One simulated agent pane.
struct Agent {
    /// `wN:pM` pane id.
    pane: &'static str,
    /// `term_*` terminal id (stable delivery anchor).
    term: &'static str,
    kind: &'static str,
    name: &'static str,
    /// Launch cwd — determines the pool scope.
    cwd: PathBuf,
    status: &'static str,
}

struct Fx {
    server: FakeHerdr,
    state: TempDir,
    project_a: TempDir,
    project_b: TempDir,
}

impl Fx {
    fn new() -> Self {
        let state = TempDir::new().unwrap();
        // Fake a live daemon so `ensure_daemon` short-circuits: pidfile +
        // lock dir both naming this (alive) test process.
        let pid = std::process::id().to_string();
        std::fs::write(state.path().join("routerd.pid"), format!("{pid}\n0\n")).unwrap();
        std::fs::create_dir(state.path().join("routerd.lock")).unwrap();
        std::fs::write(state.path().join("routerd.lock").join("pid"), &pid).unwrap();
        Self {
            server: FakeHerdr::start().unwrap(),
            state,
            project_a: TempDir::new().unwrap(),
            project_b: TempDir::new().unwrap(),
        }
    }

    fn cwd(&self, project: &TempDir) -> PathBuf {
        std::fs::canonicalize(project.path()).unwrap()
    }

    /// `HERDR_*` env for a pane id like `w1:p2` (workspace = prefix).
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
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("stdout is JSON")
    }

    /// `pools/<scope_key>/pool.db` must exist under the state dir.
    fn pool_db_exists(&self, scope_key: &str) -> bool {
        self.state
            .path()
            .join("pools")
            .join(scope_key)
            .join("pool.db")
            .is_file()
    }
}

fn ws_of(pane: &str) -> &str {
    pane.split(':').next().unwrap_or("w1")
}

fn agent_json(a: &Agent) -> Value {
    json!({
        "pane_id": a.pane, "workspace_id": ws_of(a.pane), "tab_id": format!("{}:t1", ws_of(a.pane)),
        "terminal_id": a.term, "agent_status": a.status, "revision": 1,
        "focused": false, "agent": a.kind, "name": a.name,
        "cwd": a.cwd.to_string_lossy(), "foreground_cwd": a.cwd.to_string_lossy(),
        "interactive_ready": true,
    })
}

fn pane_json(a: &Agent) -> Value {
    json!({
        "pane_id": a.pane, "workspace_id": ws_of(a.pane), "tab_id": format!("{}:t1", ws_of(a.pane)),
        "terminal_id": a.term, "agent_status": a.status, "revision": 1,
        "focused": true, "cwd": a.cwd.to_string_lossy(), "foreground_cwd": a.cwd.to_string_lossy(),
    })
}

/// Script `pane.current` + `agent.get` for every agent in `agents`.
///
/// Responders are consumed FIFO, so queue enough closures; each closure
/// dispatches on the request's target/caller pane id, so ordering across
/// commands does not matter.
fn script_agents(server: &FakeHerdr, agents: &[Agent]) {
    let by_pane: HashMap<String, usize> = agents
        .iter()
        .enumerate()
        .map(|(i, a)| (a.pane.to_string(), i))
        .collect();
    // agent.get may be called with a terminal id as well as a pane id.
    let by_term: HashMap<String, usize> = agents
        .iter()
        .enumerate()
        .map(|(i, a)| (a.term.to_string(), i))
        .collect();
    let panes: Vec<Value> = agents.iter().map(pane_json).collect();
    let infos: Vec<Value> = agents
        .iter()
        .map(|a| json!({"type": "agent_info", "agent": agent_json(a)}))
        .collect();
    let first_pane = panes[0].clone();

    for _ in 0..32 {
        let by_pane = by_pane.clone();
        let panes = panes.clone();
        let first = first_pane.clone();
        server.on("pane.current").then(move |req| {
            let caller = req
                .params
                .get("caller_pane_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let pane = by_pane
                .get(caller)
                .map(|&i| panes[i].clone())
                .unwrap_or(first.clone());
            HandlerReply::Result(json!({"type": "pane_current", "pane": pane}))
        });
    }
    for _ in 0..32 {
        let by_pane = by_pane.clone();
        let by_term = by_term.clone();
        let infos = infos.clone();
        server.on("agent.get").then(move |req| {
            let target = req
                .params
                .get("target")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let idx = by_pane.get(target).or_else(|| by_term.get(target)).copied();
            match idx {
                Some(i) => HandlerReply::Result(infos[i].clone()),
                None => HandlerReply::Error {
                    code: "agent_not_found".into(),
                    message: format!("no agent for target {target}"),
                },
            }
        });
    }
}

/// Queue `n` identical `agent.list` responses listing every agent.
fn script_agent_list(server: &FakeHerdr, agents: &[Agent], n: usize) {
    let list = json!({
        "type": "agent_list",
        "agents": agents.iter().map(agent_json).collect::<Vec<_>>(),
    });
    for _ in 0..n {
        server.on("agent.list").respond(list.clone());
    }
}

// ---------------------------------------------------------------------------
// bead 279 — messaging happy path
// ---------------------------------------------------------------------------

#[test]
fn messaging_happy_path_two_agents_one_scope() {
    let f = Fx::new();
    let cwd = f.cwd(&f.project_a);
    let agents = vec![
        Agent {
            pane: "w1:p1",
            term: "term_a",
            kind: "claude",
            name: "alpha",
            cwd: cwd.clone(),
            status: "idle",
        },
        Agent {
            pane: "w1:p2",
            term: "term_b",
            kind: "codex",
            name: "beta",
            cwd: cwd.clone(),
            status: "idle",
        },
    ];
    script_agents(&f.server, &agents);
    script_agent_list(&f.server, &agents, 16);

    let env_a = f.env("w1:p1");
    let env_b = f.env("w1:p2");

    // 1. whoami both → distinct agent_ids, same scope_key.
    let out = f.run(&env_a, &["--json", "whoami"]);
    let wa = Fx::stdout_json(&out);
    let out = f.run(&env_b, &["--json", "whoami"]);
    let wb = Fx::stdout_json(&out);
    let id_a = wa["agent_id"].as_str().unwrap().to_string();
    let id_b = wb["agent_id"].as_str().unwrap().to_string();
    assert!(id_a.starts_with("ag_") && id_b.starts_with("ag_"));
    assert_ne!(id_a, id_b, "each pane must get a distinct agent_id");
    assert_eq!(wa["scope_key"], wb["scope_key"], "same cwd → same scope");

    // 2. peers each sees the other only.
    let out = f.run(&env_a, &["--json", "peers"]);
    let peers_a = Fx::stdout_json(&out);
    let list_a = peers_a["peers"].as_array().unwrap();
    assert_eq!(list_a.len(), 1);
    assert_eq!(list_a[0]["agent_id"], id_b);
    assert_eq!(list_a[0]["name"], "beta");

    let out = f.run(&env_b, &["--json", "peers"]);
    let peers_b = Fx::stdout_json(&out);
    let list_b = peers_b["peers"].as_array().unwrap();
    assert_eq!(list_b.len(), 1);
    assert_eq!(list_b[0]["agent_id"], id_a);

    // 3. send A→B → delivered; B inbox shows it; message read acks.
    let out = f.run(
        &env_a,
        &["--json", "send", "beta", "please review the diff"],
    );
    let sent = Fx::stdout_json(&out);
    let msg = &sent["message"];
    assert_eq!(msg["status"], "delivered");
    let msg_id = msg["id"].as_str().unwrap().to_string();
    let thread_id = msg["thread_id"].as_str().unwrap().to_string();
    assert!(msg_id.starts_with("msg_") && thread_id.starts_with("th_"));
    assert_eq!(msg["to_agent"], id_b);

    let out = f.run(&env_b, &["--json", "inbox"]);
    let inbox = Fx::stdout_json(&out);
    let msgs = inbox["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["id"], msg_id);
    assert_eq!(msgs[0]["from_agent"], id_a);
    assert_eq!(msgs[0]["status"], "delivered");

    let out = f.run(&env_b, &["--json", "message", "read", &msg_id]);
    let read = Fx::stdout_json(&out);
    assert_eq!(read["id"], msg_id);
    assert_eq!(read["status"], "acknowledged");
    assert_eq!(read["content"], "please review the diff");

    // 4. reply B→A in the same thread.
    let out = f.run(&env_b, &["--json", "reply", &msg_id, "looks good, merging"]);
    let reply = Fx::stdout_json(&out);
    let rmsg = &reply["message"];
    assert_eq!(rmsg["status"], "delivered");
    assert_eq!(rmsg["kind"], "reply");
    assert_eq!(rmsg["thread_id"], thread_id);
    assert_eq!(rmsg["to_agent"], id_a);
    let reply_id = rmsg["id"].as_str().unwrap().to_string();

    // threads (from A's view): exactly one thread, two messages.
    let out = f.run(&env_a, &["--json", "threads"]);
    let threads = Fx::stdout_json(&out);
    let tlist = threads["threads"].as_array().unwrap();
    assert_eq!(tlist.len(), 1);
    assert_eq!(tlist[0]["thread_id"], thread_id);
    assert_eq!(tlist[0]["message_count"], 2);

    // 5. Delivery path: agent.prompt hit the right terminal with the envelope.
    let prompts = f.server.requests_for("agent.prompt");
    assert_eq!(prompts.len(), 2, "expected send + reply prompts");

    let p1 = prompts[0].params["text"].as_str().unwrap();
    assert_eq!(prompts[0].params["target"], "term_b");
    assert!(p1.starts_with("[herdr-pool] msg "), "{p1}");
    assert!(p1.contains(&msg_id));
    assert!(p1.contains(&thread_id));
    assert!(p1.contains("please review the diff"));
    assert!(p1.contains(&format!("({id_a})")));
    assert!(p1.contains(&format!("Reply: herdr-pool reply {msg_id}")));

    let p2 = prompts[1].params["text"].as_str().unwrap();
    assert_eq!(prompts[1].params["target"], "term_a");
    assert!(p2.starts_with("[herdr-pool] msg "));
    assert!(p2.contains(&reply_id));
    assert!(p2.contains(&thread_id));
    assert!(p2.contains("looks good, merging"));
}

// ---------------------------------------------------------------------------
// bead x6o — project isolation
// ---------------------------------------------------------------------------

#[test]
fn cross_scope_agents_cannot_see_or_reach_each_other() {
    let f = Fx::new();
    let cwd_a = f.cwd(&f.project_a);
    let cwd_b = f.cwd(&f.project_b);
    let agents = vec![
        Agent {
            pane: "w1:p1",
            term: "term_a1",
            kind: "claude",
            name: "alice",
            cwd: cwd_a.clone(),
            status: "idle",
        },
        Agent {
            pane: "w1:p2",
            term: "term_a2",
            kind: "codex",
            name: "alan",
            cwd: cwd_a.clone(),
            status: "idle",
        },
        Agent {
            pane: "w2:p1",
            term: "term_b1",
            kind: "claude",
            name: "bob",
            cwd: cwd_b.clone(),
            status: "idle",
        },
    ];
    script_agents(&f.server, &agents);
    script_agent_list(&f.server, &agents, 16);

    let env_a = f.env("w1:p1");
    let env_b = f.env("w2:p1");

    let out = f.run(&env_a, &["--json", "whoami"]);
    let wa = Fx::stdout_json(&out);
    let key_a = wa["scope_key"].as_str().unwrap().to_string();
    let out = f.run(&env_b, &["--json", "whoami"]);
    let wb = Fx::stdout_json(&out);
    let key_b = wb["scope_key"].as_str().unwrap().to_string();
    let id_b = wb["agent_id"].as_str().unwrap().to_string();
    assert_ne!(key_a, key_b, "different cwds must produce different scopes");

    // Scope A sees only its own second agent.
    let out = f.run(&env_a, &["--json", "peers"]);
    let peers = Fx::stdout_json(&out);
    let list = peers["peers"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["name"], "alan");

    // Scope B sees nobody.
    let out = f.run(&env_b, &["--json", "peers"]);
    let peers = Fx::stdout_json(&out);
    assert_eq!(peers["peers"].as_array().unwrap().len(), 0);

    // Deliver one in-scope message so scope A's db is non-empty.
    let out = f.run(&env_a, &["--json", "send", "alan", "scope-a-only"]);
    Fx::stdout_json(&out);

    // Cross-scope sends all fail with exit 2 (PeerNotFound): by name,
    // by foreign pane_id, by foreign terminal_id, and by foreign agent_id.
    for target in ["bob", "w2:p1", "term_b1", id_b.as_str()] {
        let out = f.run(&env_a, &["--json", "send", target, "hello foreign scope"]);
        assert_eq!(
            out.status.code(),
            Some(2),
            "send to {target} should fail as unknown peer"
        );
        let err: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(err["error"]["kind"], "usage", "target {target}: {err}");
    }

    // Scope B's inbox is empty even though scope A has delivered mail.
    let out = f.run(&env_b, &["--json", "inbox"]);
    let inbox = Fx::stdout_json(&out);
    assert_eq!(inbox["messages"].as_array().unwrap().len(), 0);

    // Pool databases are physically separate per scope key.
    assert!(f.pool_db_exists(&key_a), "missing pool db for scope A");
    assert!(f.pool_db_exists(&key_b), "missing pool db for scope B");
    let pools_dir = f.state.path().join("pools");
    let dirs: Vec<_> = std::fs::read_dir(&pools_dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().join("pool.db").exists())
        .collect();
    assert_eq!(dirs.len(), 2, "expected exactly two scope pools");
}

#[test]
fn same_cwd_different_workspace_shares_scope() {
    let f = Fx::new();
    let cwd = f.cwd(&f.project_a);
    // Same launch cwd, different Herdr workspace/tab/pane ids → same scope.
    let agents = vec![
        Agent {
            pane: "w1:p1",
            term: "term_e1",
            kind: "claude",
            name: "eclair",
            cwd: cwd.clone(),
            status: "idle",
        },
        Agent {
            pane: "w7:p1",
            term: "term_e2",
            kind: "codex",
            name: "eclair2",
            cwd: cwd.clone(),
            status: "idle",
        },
    ];
    script_agents(&f.server, &agents);
    script_agent_list(&f.server, &agents, 16);

    let out = f.run(&f.env("w1:p1"), &["--json", "whoami"]);
    let w1 = Fx::stdout_json(&out);
    let out = f.run(&f.env("w7:p1"), &["--json", "whoami"]);
    let w7 = Fx::stdout_json(&out);
    assert_eq!(
        w1["scope_key"], w7["scope_key"],
        "workspace id must not influence scope"
    );

    // And they really do see each other as peers.
    let out = f.run(&f.env("w1:p1"), &["--json", "peers"]);
    let peers = Fx::stdout_json(&out);
    let list = peers["peers"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["agent_id"], w7["agent_id"]);

    // Only one pool db exists for the shared scope.
    let key = w1["scope_key"].as_str().unwrap();
    assert!(f.pool_db_exists(key));
    let dirs: Vec<_> = std::fs::read_dir(f.state.path().join("pools"))
        .unwrap()
        .flatten()
        .filter(|e| e.path().join("pool.db").exists())
        .collect();
    assert_eq!(dirs.len(), 1);
}
