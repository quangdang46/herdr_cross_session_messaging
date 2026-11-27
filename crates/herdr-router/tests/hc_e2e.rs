//! hc-backend end-to-end tests: `Daemon` event loop + `DeliveryWorker`
//! against a fake `hc connect` — NDJSON JSON-RPC over stdio driven by
//! `crates/herdr-runtime/tests/fake-hc.sh` via `HC_CONNECT_CMD`.
//!
//! Mirrors `router_e2e.rs`/`e2e_lifecycle.rs` (FakeHerdr socket) for the
//! `HERDR_POOL_RUNTIME=hc` backend: snapshot/event discovery → registry →
//! bootstrap → `terminal.send` delivery, status/gone event sync, the
//! blocked-defer path (`terminal.get` pre-poll instead of `agent_blocked`),
//! `wait_agent` over `terminal.wait`, and env-based runtime selection.
//!
//! ## Fake-server notes
//!
//! `fake-hc.sh` answers every request from `resp/<method>.json` (re-read
//! per call, so tests may rewrite responses mid-flight) and appends every
//! request line to `seen.log`. After an `events.subscribe` result it also
//! emits `event.json` verbatim — once per connection. The daemon
//! resubscribes when the watched-handle set changes, and each resubscribe
//! is a fresh `hc connect` process that replays `event.json`, so event
//! fixtures must be idempotent (status re-sets, gone re-marks).
//!
//! `Daemon::new` reads `HC_CONNECT_CMD` from the process env, so tests
//! mutate env under a shared lock (the command embeds `FAKE_HC_DIR`, so
//! only `HC_CONNECT_CMD`/`HERDR_POOL_RUNTIME` need setting).

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use herdr_pool_store::{MessageStatus, PoolStore, STATUS_OFFLINE};
use herdr_router::daemon::{read_runtime_stamp, Daemon, DaemonConfig, RUNTIME_STAMP_FILE};
use herdr_runtime::{RuntimeKind, RuntimeStatus, WaitEnd, WaitRequest};
use serde_json::{json, Value};
use tempfile::TempDir;

const WAIT: Duration = Duration::from_secs(15);

fn wait_until(timeout: Duration, what: &str, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    eprintln!("wait_until timed out: {what}");
    false
}

// ---------------------------------------------------------------------------
// Env plumbing — HC_CONNECT_CMD / HERDR_POOL_RUNTIME are process-global.
// ---------------------------------------------------------------------------

/// Serializes every test in this file: they all mutate the same env vars.
/// Poison-tolerant: a panicking test must not cascade `PoisonError` into
/// the rest of the suite.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Saves/restores env vars on drop.
struct EnvGuard {
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl EnvGuard {
    fn set(vars: &[(&'static str, &str)]) -> Self {
        let saved = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var_os(k)))
            .collect();
        for (k, v) in vars {
            std::env::set_var(k, v);
        }
        Self { saved }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, v) in self.saved.drain(..) {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// fake-hc.sh fixture
// ---------------------------------------------------------------------------

const FAKE_HC_SH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../herdr-runtime/tests/fake-hc.sh"
);

/// `HC_CONNECT_CMD` value: run fake-hc.sh with `FAKE_HC_DIR` set inline
/// (the var is run through `sh -c`, so the env prefix exports it).
fn connect_cmd(dir: &Path) -> String {
    format!("FAKE_HC_DIR='{}' sh '{}'", dir.display(), FAKE_HC_SH)
}

fn resp_path(dir: &Path, method: &str) -> PathBuf {
    dir.join("resp").join(format!("{method}.json"))
}

fn write_resp(dir: &Path, method: &str, body: Value) {
    fs::create_dir_all(dir.join("resp")).unwrap();
    fs::write(resp_path(dir, method), body.to_string()).unwrap();
}

fn seen_requests(dir: &Path) -> String {
    fs::read_to_string(dir.join("seen.log")).unwrap_or_default()
}

/// Parsed request frames from `seen.log` (one JSON object per line).
fn seen_frames(dir: &Path) -> Vec<Value> {
    seen_requests(dir)
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// `terminal.send` request frames observed so far.
fn send_frames(dir: &Path) -> Vec<Value> {
    seen_frames(dir)
        .into_iter()
        .filter(|f| f["method"] == "terminal.send")
        .collect()
}

fn count_method(dir: &Path, method: &str) -> usize {
    seen_requests(dir)
        .lines()
        .filter(|l| l.contains(&format!("\"method\":\"{method}\"")))
        .count()
}

/// A live, 7501-reporting hc terminal. `state_word` is the 7501
/// `status.state` (`idle`/`working`/`blocked`/`done`).
fn terminal_json(address: &str, cwd: &Path, state_word: &str) -> Value {
    json!({
        "address": address,
        "name": "builder",
        "labels": {},
        "cwd": cwd.to_string_lossy(),
        "argv": ["/bin/claude"],
        "state": "live",
        "reported": true,
        "status": {"state": state_word, "app": "claude", "msg": "", "title": ""},
        "foreground": {"name": "claude", "argv": ["claude"], "cwd": cwd.to_string_lossy(), "pid": 42},
        "reason": null,
        "turn": 1,
        "created_ms": 1
    })
}

/// `events.subscribe` result snapshot carrying `terminals`.
fn subscribe_result(terminals: Vec<Value>) -> Value {
    json!({"seq": 1, "terminals": terminals})
}

/// Standard hc responses every daemon test needs: hello handshake,
/// terminal.list (reconcile safety), terminal.get/send/read defaults.
fn write_base_responses(dir: &Path, terminal: &Value) {
    write_resp(dir, "hello", json!({"version": "0.1.17", "methods": []}));
    write_resp(
        dir,
        "terminal.list",
        json!({"terminals": [terminal.clone()], "hidden_finished": 0, "machines": []}),
    );
    write_resp(dir, "terminal.get", terminal.clone());
    write_resp(
        dir,
        "terminal.send",
        json!({"started": true, "status": "working", "turn": 3}),
    );
    write_resp(
        dir,
        "terminal.read",
        json!({"text": "", "complete": true, "how": "screen"}),
    );
}

fn hc_config(state_dir: &Path) -> DaemonConfig {
    DaemonConfig {
        state_dir: state_dir.to_path_buf(),
        socket_path: None,
        reconcile_interval: Duration::from_secs(3600),
        delivery_poll: Duration::from_millis(50),
        event_wakeup: Duration::from_millis(200),
        notify_on_delivery: false,
        bootstrap_enabled: true,
        runtime_kind: RuntimeKind::Hc,
    }
}

/// The single pool store across all scopes (tests use one scope).
fn only_pool(daemon: &Daemon) -> Arc<PoolStore> {
    let pools = daemon.pools().all();
    assert_eq!(pools.len(), 1, "expected exactly one scope pool");
    Arc::clone(&pools[0])
}

fn live_agent(pool: &PoolStore) -> herdr_pool_store::AgentRecord {
    let agents = pool.registry().list_agents(true).unwrap();
    assert_eq!(agents.len(), 1, "expected exactly one live registry record");
    agents.into_iter().next().unwrap()
}

// ---------------------------------------------------------------------------
// discovery + bootstrap (events.subscribe snapshot path)
// ---------------------------------------------------------------------------

#[test]
fn hc_snapshot_discovers_agent_and_bootstraps() {
    let _lock = env_lock();
    let hc = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let cwd = std::fs::canonicalize(project.path()).unwrap();
    let terminal = terminal_json("t7", &cwd, "idle");

    write_base_responses(hc.path(), &terminal);
    write_resp(
        hc.path(),
        "events.subscribe",
        subscribe_result(vec![terminal.clone()]),
    );
    let _env = EnvGuard::set(&[("HC_CONNECT_CMD", &connect_cmd(hc.path()))]);

    let daemon = Arc::new(Daemon::new(hc_config(state.path())).unwrap());
    let handle = daemon.spawn().unwrap();

    // Discovery: the subscribe snapshot registers the terminal as an
    // agent — handle "t7" in pane_id, no terminal_id on hc.
    assert!(
        wait_until(WAIT, "agent registered", || {
            daemon.pools().all().iter().any(|p| {
                p.registry()
                    .list_agents(false)
                    .map(|a| !a.is_empty())
                    .unwrap_or(false)
            })
        }),
        "snapshot terminal was not registered"
    );
    let pool = only_pool(&daemon);
    let agent = live_agent(&pool);
    assert_eq!(agent.pane_id.as_deref(), Some("t7"));
    assert_eq!(agent.terminal_id, None, "hc has no terminal_id");
    assert_eq!(agent.status, "idle");
    assert_eq!(agent.agent_kind.as_deref(), Some("claude"));
    assert_eq!(agent.name.as_deref(), Some("builder"));
    assert!(agent.interactive_ready);
    assert!(!agent.launch_pending);

    // Bootstrap: one terminal.send carrying the bootstrap prompt.
    assert!(
        wait_until(WAIT, "bootstrap terminal.send", || {
            send_frames(hc.path()).iter().any(|f| {
                f["params"]["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("[herdr-pool] bootstrap"))
            })
        }),
        "expected a bootstrap prompt via terminal.send"
    );
    let boot = send_frames(hc.path())
        .into_iter()
        .find(|f| {
            f["params"]["text"]
                .as_str()
                .is_some_and(|t| t.contains("[herdr-pool] bootstrap"))
        })
        .unwrap();
    assert_eq!(boot["params"]["terminal"], "t7");
    // mark_bootstrapped persists after the send resolves — wait for it.
    assert!(
        wait_until(WAIT, "bootstrap_version persisted", || {
            pool.registry()
                .get_agent(&agent.agent_id)
                .ok()
                .flatten()
                .is_some_and(|r| r.bootstrap_version == Some(herdr_router::BOOTSTRAP_VERSION))
        }),
        "bootstrap_version was not persisted"
    );

    handle.shutdown();
    handle.join();
}

// ---------------------------------------------------------------------------
// send → deliver via terminal.send
// ---------------------------------------------------------------------------

#[test]
fn hc_send_delivers_message_via_terminal_send() {
    let _lock = env_lock();
    let hc = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let cwd = std::fs::canonicalize(project.path()).unwrap();
    let terminal = terminal_json("t7", &cwd, "idle");

    write_base_responses(hc.path(), &terminal);
    write_resp(
        hc.path(),
        "events.subscribe",
        subscribe_result(vec![terminal.clone()]),
    );
    let _env = EnvGuard::set(&[("HC_CONNECT_CMD", &connect_cmd(hc.path()))]);

    let daemon = Arc::new(Daemon::new(hc_config(state.path())).unwrap());
    let handle = daemon.spawn().unwrap();

    assert!(wait_until(WAIT, "agent registered", || {
        daemon.pools().all().iter().any(|p| {
            p.registry()
                .list_agents(false)
                .map(|a| !a.is_empty())
                .unwrap_or(false)
        })
    }));
    let pool = only_pool(&daemon);
    let agent = live_agent(&pool);
    // Wait out the bootstrap send so delivery assertions see only ours.
    assert!(wait_until(WAIT, "bootstrap sent", || {
        count_method(hc.path(), "terminal.send") >= 1
    }));

    let msg = pool
        .messages()
        .send("ag_peer", &agent.agent_id, "hello from hc peer")
        .unwrap();

    assert!(
        wait_until(WAIT, "message delivered", || {
            pool.messages()
                .message(&msg.id)
                .map(|m| {
                    m.map(|m| m.status == MessageStatus::Delivered)
                        .unwrap_or(false)
                })
                .unwrap_or(false)
        }),
        "message was not delivered"
    );

    // The delivery prompt went to the terminal address with the envelope.
    let delivery = send_frames(hc.path())
        .into_iter()
        .find(|f| {
            f["params"]["text"]
                .as_str()
                .is_some_and(|t| t.contains(&msg.id) && t.contains("hello from hc peer"))
        })
        .expect("delivery terminal.send recorded");
    assert_eq!(delivery["params"]["terminal"], "t7");
    let text = delivery["params"]["text"].as_str().unwrap();
    assert!(text.contains("[herdr-pool] msg "), "{text}");

    handle.shutdown();
    handle.join();
}

// ---------------------------------------------------------------------------
// wait_agent over terminal.wait (daemon-owned runtime)
// ---------------------------------------------------------------------------

#[test]
fn hc_wait_agent_uses_terminal_wait() {
    let _lock = env_lock();
    let hc = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let cwd = std::fs::canonicalize(project.path()).unwrap();
    let terminal = terminal_json("t7", &cwd, "idle");

    write_base_responses(hc.path(), &terminal);
    write_resp(
        hc.path(),
        "terminal.wait",
        json!({
            "terminal": "t7", "ended": true, "reason": "status",
            "turn": 2, "children": [], "status": "idle"
        }),
    );
    let _env = EnvGuard::set(&[("HC_CONNECT_CMD", &connect_cmd(hc.path()))]);

    // No spawn needed: wait_agent rides the RPC connection opened by
    // Daemon::new (hello handshake only).
    let daemon = Arc::new(Daemon::new(hc_config(state.path())).unwrap());

    let outcome = daemon
        .runtime()
        .wait_agent(
            "t7",
            WaitRequest {
                until: Some(vec![RuntimeStatus::Idle]),
                timeout_ms: Some(5_000),
                output_match: None,
            },
        )
        .unwrap();
    assert_eq!(outcome.reason, WaitEnd::Status);
    assert_eq!(
        outcome.agent.as_ref().map(|a| a.status),
        Some(RuntimeStatus::Idle)
    );

    let wait_line = seen_frames(hc.path())
        .into_iter()
        .find(|f| f["method"] == "terminal.wait")
        .expect("terminal.wait request recorded");
    assert_eq!(wait_line["params"]["terminal"], "t7");
    assert!(
        wait_line["params"]["caller"]
            .as_str()
            .is_some_and(|c| c.starts_with("herdr-pool:")),
        "caller correlation tag missing: {wait_line}"
    );
}

// ---------------------------------------------------------------------------
// registry sync via hc events (created + status)
// ---------------------------------------------------------------------------

#[test]
fn hc_created_and_status_events_sync_registry() {
    let _lock = env_lock();
    let hc = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let cwd = std::fs::canonicalize(project.path()).unwrap();
    let terminal = terminal_json("t7", &cwd, "idle");

    write_base_responses(hc.path(), &terminal);
    // Empty subscribe snapshot: discovery arrives via the `created`
    // event instead (AgentSeen → get_agent refresh path).
    write_resp(hc.path(), "events.subscribe", subscribe_result(vec![]));
    // Emitted after every events.subscribe result: t7 created, then a
    // status transition to working. Idempotent across resubscribes.
    fs::write(
        hc.path().join("event.json"),
        format!(
            "{}\n{}\n",
            json!({
                "jsonrpc": "2.0", "method": "event",
                "params": {"type": "created", "terminal": "t7", "data": terminal, "seq": 2}
            }),
            json!({
                "jsonrpc": "2.0", "method": "event",
                "params": {"type": "status", "terminal": "t7",
                    "data": {"state": "working", "app": "claude"}, "seq": 3}
            })
        ),
    )
    .unwrap();
    let _env = EnvGuard::set(&[("HC_CONNECT_CMD", &connect_cmd(hc.path()))]);

    let daemon = Arc::new(Daemon::new(hc_config(state.path())).unwrap());
    let handle = daemon.spawn().unwrap();

    assert!(
        wait_until(WAIT, "agent registered via created event", || {
            daemon.pools().all().iter().any(|p| {
                p.registry()
                    .list_agents(false)
                    .map(|a| !a.is_empty())
                    .unwrap_or(false)
            })
        }),
        "created event did not register the agent"
    );
    let pool = only_pool(&daemon);
    let agent = live_agent(&pool);
    assert_eq!(agent.pane_id.as_deref(), Some("t7"));

    // The status event transitioned the registry record to working.
    // (Resubscribes replay created→idle then status→working, so the
    // stable end state is working.)
    assert!(
        wait_until(WAIT, "status -> working", || {
            pool.registry()
                .get_agent(&agent.agent_id)
                .ok()
                .flatten()
                .is_some_and(|r| r.status == "working")
        }),
        "hc status event did not update the registry"
    );

    handle.shutdown();
    handle.join();
}

// ---------------------------------------------------------------------------
// registry sync via hc events (finished → offline)
// ---------------------------------------------------------------------------

#[test]
fn hc_finished_events_mark_agent_offline() {
    let _lock = env_lock();
    let hc = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let cwd = std::fs::canonicalize(project.path()).unwrap();

    // A finished terminal: state != "live" maps to RuntimeStatus::Offline,
    // interactive_ready=false. terminal.get agrees so AgentSeen refreshes
    // converge on offline (no live/offline flicker across event replays).
    let mut finished = terminal_json("t7", &cwd, "idle");
    finished["state"] = json!("finished");
    finished["status"] = Value::Null;
    finished["reported"] = json!(false);
    finished["foreground"] = Value::Null;

    write_base_responses(hc.path(), &finished);
    write_resp(hc.path(), "events.subscribe", subscribe_result(vec![]));
    fs::write(
        hc.path().join("event.json"),
        format!(
            "{}\n{}\n",
            json!({
                "jsonrpc": "2.0", "method": "event",
                "params": {"type": "created", "terminal": "t7", "data": finished, "seq": 2}
            }),
            json!({
                "jsonrpc": "2.0", "method": "event",
                "params": {"type": "finished", "terminal": "t7",
                    "data": {"address": "t7", "state": "finished"}, "seq": 3}
            })
        ),
    )
    .unwrap();
    let _env = EnvGuard::set(&[("HC_CONNECT_CMD", &connect_cmd(hc.path()))]);

    let daemon = Arc::new(Daemon::new(hc_config(state.path())).unwrap());
    let handle = daemon.spawn().unwrap();

    // The agent record exists (include_offline) and is offline; it must
    // never appear as live, and no bootstrap/delivery prompt is sent to
    // a finished terminal.
    assert!(
        wait_until(WAIT, "offline record", || {
            daemon.pools().all().iter().any(|p| {
                p.registry()
                    .list_agents(true)
                    .map(|a| {
                        a.iter().any(|r| {
                            r.pane_id.as_deref() == Some("t7") && r.status == STATUS_OFFLINE
                        })
                    })
                    .unwrap_or(false)
            })
        }),
        "finished terminal was not recorded offline"
    );
    let pool = only_pool(&daemon);
    let records = pool.registry().list_agents(true).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].status, STATUS_OFFLINE);
    assert!(
        send_frames(hc.path()).is_empty(),
        "no terminal.send expected for an offline agent"
    );

    handle.shutdown();
    handle.join();
}

// ---------------------------------------------------------------------------
// blocked → deferred (no attempt consumed) → delivered after unblock
// ---------------------------------------------------------------------------

#[test]
fn hc_blocked_defers_then_delivers_after_unblock() {
    let _lock = env_lock();
    let hc = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let cwd = std::fs::canonicalize(project.path()).unwrap();

    // Terminal reports blocked: send_prompt's terminal.get pre-poll maps
    // this to Deferred(Blocked) — the hc analogue of agent_blocked.
    let blocked = terminal_json("t7", &cwd, "blocked");
    write_base_responses(hc.path(), &blocked);
    write_resp(
        hc.path(),
        "events.subscribe",
        subscribe_result(vec![terminal_json("t7", &cwd, "idle")]),
    );
    let _env = EnvGuard::set(&[("HC_CONNECT_CMD", &connect_cmd(hc.path()))]);

    let daemon = Arc::new(Daemon::new(hc_config(state.path())).unwrap());
    let handle = daemon.spawn().unwrap();

    assert!(wait_until(WAIT, "agent registered", || {
        daemon.pools().all().iter().any(|p| {
            p.registry()
                .list_agents(false)
                .map(|a| !a.is_empty())
                .unwrap_or(false)
        })
    }));
    let pool = only_pool(&daemon);
    let agent = live_agent(&pool);

    let msg = pool
        .messages()
        .send("ag_peer", &agent.agent_id, "blocked hc delivery")
        .unwrap();

    // Deferred: still pending, scheduled, no attempt consumed, and —
    // crucially — terminal.send was never called for this message
    // (the pre-poll refused before typing anything).
    assert!(
        wait_until(WAIT, "blocked defer recorded", || {
            pool.messages()
                .message(&msg.id)
                .map(|m| {
                    m.map(|m| {
                        m.status == MessageStatus::Pending
                            && m.attempt_count == 0
                            && m.next_retry_at.is_some()
                            && m.delivered_at.is_none()
                    })
                    .unwrap_or(false)
                })
                .unwrap_or(false)
        }),
        "message should stay pending+scheduled while terminal is blocked"
    );
    assert!(
        !send_frames(hc.path()).iter().any(|f| f["params"]["text"]
            .as_str()
            .is_some_and(|t| t.contains(&msg.id))),
        "terminal.send must not fire while the terminal is blocked"
    );

    // Unblock: flip the pre-poll response to idle and make the deferred
    // message due now (production would wait out the 15s backoff).
    write_resp(hc.path(), "terminal.get", terminal_json("t7", &cwd, "idle"));
    pool.messages().retry(&msg.id).unwrap();

    assert!(
        wait_until(WAIT, "delivered after unblock", || {
            pool.messages()
                .message(&msg.id)
                .map(|m| {
                    m.map(|m| m.status == MessageStatus::Delivered)
                        .unwrap_or(false)
                })
                .unwrap_or(false)
        }),
        "message was not delivered after the terminal unblocked"
    );
    assert!(
        send_frames(hc.path()).iter().any(|f| {
            f["params"]["text"]
                .as_str()
                .is_some_and(|t| t.contains(&msg.id))
        }),
        "delivery terminal.send missing after unblock"
    );

    handle.shutdown();
    handle.join();
}

// ---------------------------------------------------------------------------
// HERDR_POOL_RUNTIME selection + runtime stamp
// ---------------------------------------------------------------------------

#[test]
fn hc_runtime_flag_selects_backend_and_stamps_state() {
    let _lock = env_lock();
    let hc = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let cwd = std::fs::canonicalize(project.path()).unwrap();
    let terminal = terminal_json("t7", &cwd, "idle");

    write_base_responses(hc.path(), &terminal);
    write_resp(
        hc.path(),
        "events.subscribe",
        subscribe_result(vec![terminal.clone()]),
    );
    let _env = EnvGuard::set(&[
        ("HC_CONNECT_CMD", &connect_cmd(hc.path())),
        ("HERDR_POOL_RUNTIME", "hc"),
    ]);

    // from_env_at parses the flag into DaemonConfig.
    let config = DaemonConfig::from_env_at(state.path().to_path_buf()).unwrap();
    assert_eq!(config.runtime_kind, RuntimeKind::Hc);

    // Daemon::new connects via HC_CONNECT_CMD and stamps <state>/runtime.
    let daemon = Arc::new(Daemon::new(config).unwrap());
    assert_eq!(read_runtime_stamp(state.path()).as_deref(), Some("hc"));
    assert_eq!(
        fs::read_to_string(state.path().join(RUNTIME_STAMP_FILE))
            .unwrap()
            .trim(),
        "hc"
    );
    drop(daemon);

    // Invalid values are rejected with a usage-ready message.
    std::env::set_var("HERDR_POOL_RUNTIME", "bogus");
    let err = DaemonConfig::from_env_at(state.path().to_path_buf()).unwrap_err();
    assert!(
        err.to_string().contains("invalid HERDR_POOL_RUNTIME"),
        "got {err}"
    );
}
