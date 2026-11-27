//! M6 end-to-end tests: daemon lifecycle + delivery edge cases against
//! `FakeHerdr` (scriptable NDJSON unix-socket server).
//!
//! Sibling of `router_e2e.rs`; covers the bead-od7 scenarios that need the
//! real `Daemon` event loop and `DeliveryWorker`:
//!
//! * `pane.moved` → `agent_id` stable, registry `pane_id` re-keyed, next
//!   prompt still resolves (terminal anchor; pane-id fallback covered at
//!   worker level).
//! * `pane.agent_status_changed` blocked → message stays scheduled;
//!   →idle → delivered.
//! * `pane.exited` → offline; mail to a long-offline peer dead-letters and
//!   triggers `notification.show`.
//! * Daemon restart → snapshot reconcile rebuilds the registry (stable
//!   `agent_id`) and the pending outbox resumes; no duplicate bootstrap.
//! * `pane.agent_detected` → exactly one bootstrap prompt; a
//!   `bootstrap_version` bump re-bootstraps exactly once more.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use herdr_pool_store::{AgentRecord, MessageStatus, PoolStore, STATUS_OFFLINE};
use herdr_router::daemon::{clear_stop_sentinel, Daemon, DaemonConfig};
use herdr_router::delivery::DeliveryWorker;
use herdr_router::registry_sync::Notify;
use herdr_router::Pools;
use herdr_testkit::{FakeHerdr, HandlerReply};
use serde_json::{json, Value};

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

/// Wait until an `events.subscribe` carrying a per-pane
/// `pane.agent_status_changed` subscription for `pane` has been
/// recorded. The first subscribe goes out before the snapshot sync
/// populates the watched set, and a resubscribe follows; frames pushed
/// while only the stale subscription is live are dropped, so tests must
/// wait for this before pushing pane-scoped events.
fn wait_for_status_subscription(server: &FakeHerdr, pane: &str) -> bool {
    wait_until(WAIT, "per-pane status subscription", || {
        server.requests_for("events.subscribe").iter().any(|req| {
            req.params["subscriptions"].as_array().is_some_and(|subs| {
                subs.iter()
                    .any(|s| s["type"] == "pane.agent_status_changed" && s["pane_id"] == pane)
            })
        })
    })
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn agent_json(pane: &str, term: &str, cwd: &std::path::Path, status: &str) -> Value {
    let ws = pane.split(':').next().unwrap_or("w1");
    json!({
        "pane_id": pane,
        "workspace_id": ws,
        "tab_id": format!("{ws}:t1"),
        "terminal_id": term,
        "agent_status": status,
        "revision": 1,
        "focused": true,
        "agent": "claude",
        "name": "builder",
        "cwd": cwd.to_string_lossy(),
        "foreground_cwd": cwd.to_string_lossy(),
        "interactive_ready": true,
    })
}

fn agent_info(agent: Value) -> Value {
    json!({ "type": "agent_info", "agent": agent })
}

fn snapshot_result(agents: Vec<Value>) -> Value {
    json!({
        "type": "session_snapshot",
        "snapshot": {
            "version": "0.9.5",
            "protocol": 22,
            "workspaces": [],
            "tabs": [],
            "panes": [],
            "layouts": [],
            "agents": agents,
            "focused_workspace_id": "w1",
            "focused_tab_id": "w1:t1",
            "focused_pane_id": "w1:p1",
        }
    })
}

fn test_config(state_dir: &std::path::Path, socket: &std::path::Path) -> DaemonConfig {
    DaemonConfig {
        state_dir: state_dir.to_path_buf(),
        socket_path: Some(socket.to_path_buf()),
        reconcile_interval: Duration::from_secs(3600),
        delivery_poll: Duration::from_millis(50),
        event_wakeup: Duration::from_millis(200),
        notify_on_delivery: false,
        bootstrap_enabled: true,
    }
}

/// Script `agent.get` to resolve `target` (pane or terminal id) against a
/// fixed map of `{"<target>": <AgentInfo json>}`; unknown targets get
/// `agent_not_found`. Queue depth covers every lookup the daemon makes.
fn script_agent_get(server: &FakeHerdr, agents: Vec<(String, Value)>) {
    for _ in 0..24 {
        let agents = agents.clone();
        server.on("agent.get").then(move |req| {
            let target = req
                .params
                .get("target")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match agents.iter().find(|(t, _)| t == target) {
                Some((_, info)) => HandlerReply::Result(info.clone()),
                None => HandlerReply::Error {
                    code: "agent_not_found".into(),
                    message: format!("no agent for target {target}"),
                },
            }
        });
    }
}

fn prompt_texts(server: &FakeHerdr) -> Vec<String> {
    server
        .requests_for("agent.prompt")
        .iter()
        .filter_map(|r| {
            r.params
                .get("text")
                .and_then(|t| t.as_str())
                .map(str::to_string)
        })
        .collect()
}

fn bootstrap_prompts(server: &FakeHerdr) -> usize {
    prompt_texts(server)
        .iter()
        .filter(|t| t.contains("[herdr-pool] bootstrap"))
        .count()
}

/// The single pool store across all scopes (tests use one scope).
fn only_pool(daemon: &Daemon) -> Arc<PoolStore> {
    let pools = daemon.pools().all();
    assert_eq!(pools.len(), 1, "expected exactly one scope pool");
    Arc::clone(&pools[0])
}

fn live_agent(pool: &PoolStore) -> AgentRecord {
    let agents = pool.registry().list_agents(true).unwrap();
    assert_eq!(agents.len(), 1, "expected exactly one registry record");
    agents.into_iter().next().unwrap()
}

fn pane_moved_event(new_pane: &str, term: &str, cwd: &std::path::Path) -> Value {
    let ws = new_pane.split(':').next().unwrap_or("w1");
    json!({
        "event": "pane_moved",
        "data": {
            "type": "pane_moved",
            "pane": {
                "pane_id": new_pane,
                "workspace_id": ws,
                "tab_id": format!("{ws}:t1"),
                "terminal_id": term,
                "agent_status": "idle",
                "revision": 2,
                "focused": false,
                "cwd": cwd.to_string_lossy(),
                "foreground_cwd": cwd.to_string_lossy(),
            },
            "previous_pane_id": "w1:p1",
            "previous_workspace_id": "w1",
            "previous_tab_id": "w1:t1",
        }
    })
}

fn agent_detected_event(pane: &str) -> Value {
    json!({
        "event": "pane_agent_detected",
        "data": {
            "type": "pane_agent_detected",
            "pane_id": pane,
            "workspace_id": "w1",
            "agent": "claude",
            "final_status": "idle",
            "released": false,
        }
    })
}

fn status_event(pane: &str, status: &str) -> Value {
    json!({
        "event": "pane.agent_status_changed",
        "data": {
            "pane_id": pane,
            "workspace_id": "w1",
            "agent_status": status,
            "agent": "claude",
        }
    })
}

fn pane_updated_event(pane: &str, term: &str, cwd: &std::path::Path) -> Value {
    let ws = pane.split(':').next().unwrap_or("w1");
    json!({
        "event": "pane_updated",
        "data": {
            "type": "pane_updated",
            "pane": {
                "pane_id": pane,
                "workspace_id": ws,
                "tab_id": format!("{ws}:t1"),
                "terminal_id": term,
                "agent_status": "idle",
                "revision": 2,
                "focused": false,
                "cwd": cwd.to_string_lossy(),
                "foreground_cwd": cwd.to_string_lossy(),
            },
        }
    })
}

// ---------------------------------------------------------------------------
// pane.moved → stable agent_id, re-keyed pane_id, delivery still works
// ---------------------------------------------------------------------------

#[test]
fn pane_moved_keeps_agent_id_and_rekeys_pane() {
    let server = FakeHerdr::start().unwrap();
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cwd = project.path();

    server
        .on("session.snapshot")
        .respond(snapshot_result(vec![agent_json(
            "w1:p1", "term_a", cwd, "idle",
        )]));
    // After the move the pane lives at w1:p9; agent.get resolves it there.
    script_agent_get(
        &server,
        vec![
            (
                "w1:p1".into(),
                agent_info(agent_json("w1:p1", "term_a", cwd, "idle")),
            ),
            (
                "w1:p9".into(),
                agent_info(agent_json("w1:p9", "term_a", cwd, "idle")),
            ),
            (
                "term_a".into(),
                agent_info(agent_json("w1:p9", "term_a", cwd, "idle")),
            ),
        ],
    );

    let daemon = Arc::new(Daemon::new(test_config(state.path(), server.socket_path())).unwrap());
    let handle = daemon.spawn().unwrap();

    assert!(
        wait_until(WAIT, "bootstrap prompt", || bootstrap_prompts(&server) == 1),
        "expected exactly one bootstrap prompt"
    );
    let pool = only_pool(&daemon);
    let before = live_agent(&pool);
    assert_eq!(before.pane_id.as_deref(), Some("w1:p1"));
    let agent_id = before.agent_id.clone();

    // pane.move w1:p1 → w1:p9 (same terminal). Wait for the resubscribe
    // carrying the per-pane filters so the frame is not dropped.
    assert!(wait_for_status_subscription(&server, "w1:p1"));
    server.push_event(pane_moved_event("w1:p9", "term_a", cwd));

    assert!(
        wait_until(WAIT, "pane re-keyed", || {
            pool.registry()
                .get_agent(&agent_id)
                .ok()
                .flatten()
                .is_some_and(|r| {
                    r.pane_id.as_deref() == Some("w1:p9") && r.status != STATUS_OFFLINE
                })
        }),
        "pane.moved did not re-key the registry record"
    );
    let after = pool.registry().get_agent(&agent_id).unwrap().unwrap();
    assert_eq!(after.agent_id, agent_id, "agent_id must survive pane.moved");
    assert_eq!(after.terminal_id.as_deref(), Some("term_a"));

    // Next delivery resolves against the *current* record: pane_id is the
    // preferred prompt target (agent.prompt rejects terminal_id), and the
    // registry re-keyed it to w1:p9 on pane.moved.
    let msg = pool
        .messages()
        .send("ag_peer", &agent_id, "after the move")
        .unwrap();
    assert!(
        wait_until(WAIT, "post-move delivery", || {
            prompt_texts(&server)
                .iter()
                .any(|t| t.contains(&msg.id) && t.contains("after the move"))
        }),
        "message was not delivered after pane.moved"
    );
    let req = server
        .requests_for("agent.prompt")
        .into_iter()
        .find(|r| {
            r.params
                .get("text")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t.contains(&msg.id))
        })
        .unwrap();
    assert_eq!(req.params["target"], "w1:p9");
    assert_eq!(
        pool.messages().message(&msg.id).unwrap().unwrap().status,
        MessageStatus::Delivered
    );

    handle.shutdown();
    handle.join();
}

/// Worker-level check: when a record has no `terminal_id`, the *current*
/// `pane_id` is the delivery target (the "new pane id used for next
/// prompt" half of the bead, which the terminal-anchored path above
/// cannot exercise).
#[test]
fn delivery_falls_back_to_current_pane_id() {
    let server = FakeHerdr::start().unwrap();
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();

    let pools = Arc::new(Pools::open(state.path()).unwrap());
    let pool = pools
        .for_scope_key("0123456789abcdef0123456789abcdef", project.path())
        .unwrap();
    pool.registry()
        .upsert_agent(&AgentRecord {
            agent_id: "ag_pane_only".into(),
            scope_key: "0123456789abcdef0123456789abcdef".into(),
            terminal_id: None,
            pane_id: Some("w7:p7".into()),
            agent_kind: Some("claude".into()),
            name: None,
            cwd: None,
            status: "idle".into(),
            interactive_ready: true,
            launch_pending: false,
            bootstrap_version: None,
            first_seen_at: 0,
            last_seen_at: 0,
            offline_at: None,
        })
        .unwrap();
    let msg = pool
        .messages()
        .send("ag_sender", "ag_pane_only", "pane-id delivery")
        .unwrap();

    let client = herdr_protocol::HerdrClient::from_socket_path(server.socket_path());
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker = DeliveryWorker::new(
        Arc::new(herdr_runtime::HerdrRuntime::new(client.clone())),
        client,
        Arc::clone(&pools),
        shutdown,
        Notify::default(),
        Duration::from_millis(10),
    );
    assert_eq!(worker.drain_once().unwrap(), 1);

    let req = server
        .requests_for("agent.prompt")
        .into_iter()
        .find(|r| {
            r.params
                .get("text")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t.contains(&msg.id))
        })
        .expect("delivery prompt recorded");
    assert_eq!(req.params["target"], "w7:p7");
    assert_eq!(
        pool.messages().message(&msg.id).unwrap().unwrap().status,
        MessageStatus::Delivered
    );
}

// ---------------------------------------------------------------------------
// blocked → scheduled; idle → delivered
// ---------------------------------------------------------------------------

#[test]
fn blocked_status_schedules_message_and_idle_delivers() {
    let server = FakeHerdr::start().unwrap();
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cwd = project.path();

    server
        .on("session.snapshot")
        .respond(snapshot_result(vec![agent_json(
            "w1:p1", "term_a", cwd, "idle",
        )]));
    script_agent_get(
        &server,
        vec![(
            "w1:p1".into(),
            agent_info(agent_json("w1:p1", "term_a", cwd, "idle")),
        )],
    );

    let daemon = Arc::new(Daemon::new(test_config(state.path(), server.socket_path())).unwrap());
    let handle = daemon.spawn().unwrap();
    assert!(wait_until(WAIT, "bootstrap", || {
        bootstrap_prompts(&server) == 1
    }));
    // Pushed subscription frames are broadcast-only: an event sent before
    // `events.subscribe` connects is lost. Wait for the subscription itself
    // (it is issued after the snapshot sync that produced the bootstrap).
    assert!(wait_until(WAIT, "events.subscribe", || {
        !server.requests_for("events.subscribe").is_empty()
    }));
    let pool = only_pool(&daemon);
    let agent = live_agent(&pool);

    // Agent reports blocked via the per-pane status subscription. Wait
    // for the resubscribe that carries the per-pane filter — a frame
    // pushed while only the initial (unfiltered) subscription is live is
    // dropped.
    assert!(
        wait_for_status_subscription(&server, "w1:p1"),
        "expected events.subscribe with per-pane status subscription"
    );
    server.push_subscription_event(status_event("w1:p1", "blocked"));
    assert!(wait_until(WAIT, "status -> blocked", || {
        pool.registry()
            .get_agent(&agent.agent_id)
            .map(|r| r.map(|r| r.status == "blocked").unwrap_or(false))
            .unwrap_or(false)
    }));

    // The next (and only, thanks to the +15s blocked backoff) prompt
    // attempt fails with agent_blocked → deferred + scheduled retry, with
    // no delivery attempt consumed.
    server
        .on("agent.prompt")
        .error("agent_blocked", "agent is busy");
    let msg = pool
        .messages()
        .send("ag_peer", &agent.agent_id, "blocked delivery")
        .unwrap();

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
        "message should stay pending+scheduled after agent_blocked"
    );

    // Agent goes idle → delivery nudge; make the retry due now (the
    // production path would wait out the 15s backoff) and let the worker
    // deliver on the next sweep.
    server.push_subscription_event(status_event("w1:p1", "idle"));
    assert!(wait_until(WAIT, "status -> idle", || {
        pool.registry()
            .get_agent(&agent.agent_id)
            .map(|r| r.map(|r| r.status == "idle").unwrap_or(false))
            .unwrap_or(false)
    }));
    pool.messages().retry(&msg.id).unwrap();

    assert!(
        wait_until(WAIT, "delivered after idle", || {
            pool.messages()
                .message(&msg.id)
                .map(|m| {
                    m.map(|m| m.status == MessageStatus::Delivered)
                        .unwrap_or(false)
                })
                .unwrap_or(false)
        }),
        "message was not delivered after idle transition"
    );
    // One blocked attempt + one successful retry.
    let attempts = server
        .requests_for("agent.prompt")
        .iter()
        .filter(|r| {
            r.params
                .get("text")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t.contains(&msg.id))
        })
        .count();
    assert_eq!(attempts, 2, "expected one blocked + one successful attempt");

    handle.shutdown();
    handle.join();
}

// ---------------------------------------------------------------------------
// pane.exited → offline → dead-letter + notification.show
// ---------------------------------------------------------------------------

#[test]
fn pane_exited_offline_dead_letters_with_notification() {
    let server = FakeHerdr::start().unwrap();
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cwd = project.path();

    server
        .on("session.snapshot")
        .respond(snapshot_result(vec![agent_json(
            "w1:p1", "term_a", cwd, "idle",
        )]));
    script_agent_get(
        &server,
        vec![(
            "w1:p1".into(),
            agent_info(agent_json("w1:p1", "term_a", cwd, "idle")),
        )],
    );

    let daemon = Arc::new(Daemon::new(test_config(state.path(), server.socket_path())).unwrap());
    let handle = daemon.spawn().unwrap();
    assert!(wait_until(WAIT, "events.subscribe", || {
        !server.requests_for("events.subscribe").is_empty()
    }));
    let pool = only_pool(&daemon);
    let agent = live_agent(&pool);

    // pane.exited → offline. Wait for the resubscribe carrying the
    // per-pane filter so the frame is not dropped.
    assert!(wait_for_status_subscription(&server, "w1:p1"));
    server.push_event(json!({
        "event": "pane_exited",
        "data": {
            "type": "pane_exited",
            "pane_id": "w1:p1",
            "workspace_id": "w1",
        }
    }));
    assert!(
        wait_until(WAIT, "agent offline", || {
            pool.registry()
                .get_agent(&agent.agent_id)
                .map(|r| {
                    r.map(|r| r.status == STATUS_OFFLINE && r.offline_at.is_some())
                        .unwrap_or(false)
                })
                .unwrap_or(false)
        }),
        "pane.exited did not mark the agent offline"
    );

    // Backdate offline_at beyond OFFLINE_DEAD_AFTER so the next outbox
    // sweep takes the dead-letter path instead of the keep-retrying path.
    let mut record = pool.registry().get_agent(&agent.agent_id).unwrap().unwrap();
    record.offline_at =
        Some(now_millis() - (herdr_router::OFFLINE_DEAD_AFTER.as_millis() as i64) - 60_000);
    pool.registry().upsert_agent(&record).unwrap();

    // Mail to the offline peer dead-letters; no agent.prompt is attempted.
    let msg = pool
        .messages()
        .send("ag_sender", &agent.agent_id, "never delivered")
        .unwrap();
    assert!(
        wait_until(WAIT, "message dead-lettered", || {
            pool.messages()
                .message(&msg.id)
                .map(|m| m.map(|m| m.status == MessageStatus::Dead).unwrap_or(false))
                .unwrap_or(false)
        }),
        "offline-too-long message was not dead-lettered"
    );
    assert!(
        !server
            .requests_for("agent.prompt")
            .iter()
            .any(|r| r.params["text"]
                .as_str()
                .is_some_and(|t| t.contains(&msg.id))),
        "offline recipient must not be prompted"
    );
    assert!(
        wait_until(WAIT, "dead-letter notification", || {
            !server.requests_for("notification.show").is_empty()
        }),
        "expected notification.show for the dead-lettered message"
    );
    let notif = server.last_request_for("notification.show").unwrap();
    assert!(
        notif.params["title"]
            .as_str()
            .unwrap_or("")
            .contains("herdr-pool"),
        "unexpected notification params: {:?}",
        notif.params
    );

    handle.shutdown();
    handle.join();
}

// ---------------------------------------------------------------------------
// daemon restart → snapshot reconcile + outbox resume, no re-bootstrap
// ---------------------------------------------------------------------------

#[test]
fn daemon_restart_reconciles_registry_and_resumes_outbox() {
    let server = FakeHerdr::start().unwrap();
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cwd = project.path();

    // Enough snapshot responses for both daemon incarnations.
    for _ in 0..4 {
        server
            .on("session.snapshot")
            .respond(snapshot_result(vec![agent_json(
                "w1:p1", "term_a", cwd, "idle",
            )]));
    }
    script_agent_get(
        &server,
        vec![(
            "w1:p1".into(),
            agent_info(agent_json("w1:p1", "term_a", cwd, "idle")),
        )],
    );

    let config = test_config(state.path(), server.socket_path());
    let daemon1 = Arc::new(Daemon::new(config.clone()).unwrap());
    let handle1 = daemon1.spawn().unwrap();
    assert!(wait_until(WAIT, "first bootstrap", || {
        bootstrap_prompts(&server) == 1
    }));
    let pool = only_pool(&daemon1);
    let agent_id = live_agent(&pool).agent_id.clone();
    handle1.shutdown();
    handle1.join();

    // Queue mail while the daemon is down — it must survive in pool.db.
    let msg = pool
        .messages()
        .send("ag_peer", &agent_id, "queued while daemon was down")
        .unwrap();

    // Restart: clear the stop sentinel the graceful shutdown wrote, then
    // run a fresh Daemon on the same state dir.
    clear_stop_sentinel(state.path());
    let daemon2 = Arc::new(Daemon::new(config).unwrap());
    let handle2 = daemon2.spawn().unwrap();

    // Snapshot reconcile rebuilds the registry with the SAME agent_id
    // (terminal_id is the stable anchor across restarts).
    assert!(
        wait_until(WAIT, "registry rebuilt", || {
            daemon2.pools().all().iter().any(|p| {
                p.registry()
                    .get_agent(&agent_id)
                    .map(|r| r.map(|r| r.status != STATUS_OFFLINE).unwrap_or(false))
                    .unwrap_or(false)
            })
        }),
        "registry was not rebuilt after restart"
    );

    // The pending outbox resumes: the queued message is delivered.
    assert!(
        wait_until(WAIT, "outbox resumed", || {
            pool.messages()
                .message(&msg.id)
                .map(|m| {
                    m.map(|m| m.status == MessageStatus::Delivered)
                        .unwrap_or(false)
                })
                .unwrap_or(false)
        }),
        "pending message was not delivered after restart"
    );
    assert!(
        prompt_texts(&server)
            .iter()
            .any(|t| t.contains(&msg.id) && t.contains("queued while daemon was down")),
        "expected delivery prompt for the queued message"
    );

    // bootstrap_version persisted → no second bootstrap prompt.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        bootstrap_prompts(&server),
        1,
        "restart must not re-send the bootstrap prompt"
    );

    handle2.shutdown();
    handle2.join();
}

// ---------------------------------------------------------------------------
// bootstrap: agent_detected → one prompt; version bump → one more
// ---------------------------------------------------------------------------

#[test]
fn agent_detected_bootstraps_once_and_version_bump_rebootstraps() {
    let server = FakeHerdr::start().unwrap();
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cwd = project.path();

    // Snapshot starts empty — the agent appears via pane.agent_detected.
    for _ in 0..2 {
        server
            .on("session.snapshot")
            .respond(snapshot_result(vec![]));
    }
    script_agent_get(
        &server,
        vec![(
            "w1:p1".into(),
            agent_info(agent_json("w1:p1", "term_a", cwd, "idle")),
        )],
    );

    let daemon = Arc::new(Daemon::new(test_config(state.path(), server.socket_path())).unwrap());
    let handle = daemon.spawn().unwrap();
    assert!(wait_until(WAIT, "events.subscribe", || {
        !server.requests_for("events.subscribe").is_empty()
    }));
    let pool = || only_pool(&daemon);

    // First detection → exactly one bootstrap prompt.
    server.push_event(agent_detected_event("w1:p1"));
    assert!(
        wait_until(WAIT, "first bootstrap", || bootstrap_prompts(&server) == 1),
        "expected one bootstrap prompt after agent_detected"
    );
    let agent_id = live_agent(&pool()).agent_id.clone();

    // The detection grew the watched set → the daemon resubscribes with a
    // per-pane status filter; further pushed frames must wait for it.
    assert!(wait_for_status_subscription(&server, "w1:p1"));

    // A repeated detection is a no-op for bootstrap (version persisted).
    // Wait for the agent.get lookup proving the event was processed, then
    // assert no additional bootstrap prompt appeared.
    let gets_before = server.requests_for("agent.get").len();
    server.push_event(agent_detected_event("w1:p1"));
    assert!(
        wait_until(WAIT, "second detection processed", || {
            server.requests_for("agent.get").len() > gets_before
        }),
        "second agent_detected was not processed"
    );
    // Give any (unexpected) prompt a moment, then assert still exactly one.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        bootstrap_prompts(&server),
        1,
        "duplicate agent_detected must not re-bootstrap"
    );

    // Simulate a bootstrap-version bump: mark the record as needing an
    // older bootstrap, then detect again → exactly one more prompt.
    let mut record = pool().registry().get_agent(&agent_id).unwrap().unwrap();
    // BOOTSTRAP_VERSION is 1 → 0 marks the agent as needing a re-bootstrap.
    record.bootstrap_version = Some(0);
    pool().registry().upsert_agent(&record).unwrap();

    let gets_before = server.requests_for("agent.get").len();
    server.push_event(agent_detected_event("w1:p1"));
    assert!(
        wait_until(WAIT, "re-bootstrap after version bump", || {
            bootstrap_prompts(&server) == 2
        }),
        "expected a second bootstrap prompt after version bump"
    );
    assert!(
        server.requests_for("agent.get").len() > gets_before,
        "version-bump detection was not processed"
    );

    // And it settles again — no third prompt.
    let gets_before = server.requests_for("agent.get").len();
    server.push_event(agent_detected_event("w1:p1"));
    assert!(
        wait_until(WAIT, "third detection processed", || {
            server.requests_for("agent.get").len() > gets_before
        }),
        "third agent_detected was not processed"
    );
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(bootstrap_prompts(&server), 2);

    handle.shutdown();
    handle.join();
}

// ---------------------------------------------------------------------------
// agent_blocked bootstrap rejections must not exhaust the attempt budget
// ---------------------------------------------------------------------------

#[test]
fn bootstrap_blocked_rejections_do_not_exhaust_attempt_budget() {
    let server = FakeHerdr::start().unwrap();
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cwd = project.path();

    server
        .on("session.snapshot")
        .respond(snapshot_result(vec![]));
    // The agent only ever reports `blocked` until we flip it to idle.
    script_agent_get(
        &server,
        vec![(
            "w1:p1".into(),
            agent_info(agent_json("w1:p1", "term_a", cwd, "blocked")),
        )],
    );
    // Every bootstrap prompt is rejected while the agent is blocked; once it
    // goes idle the next prompt falls through to the canned success.
    for _ in 0..3 {
        server.fail_next("agent.prompt", "agent_blocked", "agent is busy");
    }

    let daemon = Arc::new(Daemon::new(test_config(state.path(), server.socket_path())).unwrap());
    let handle = daemon.spawn().unwrap();
    assert!(wait_until(WAIT, "events.subscribe", || {
        !server.requests_for("events.subscribe").is_empty()
    }));

    // Three sightings while blocked → three rejected bootstrap attempts.
    // Under the old accounting these burned the whole per-run budget.
    // The first detection grows the watched set and triggers a
    // resubscribe; later pushes must wait for the per-pane filter.
    for i in 0..3 {
        if i > 0 {
            assert!(wait_for_status_subscription(&server, "w1:p1"));
        }
        let prompts_before = server.requests_for("agent.prompt").len();
        server.push_event(agent_detected_event("w1:p1"));
        assert!(
            wait_until(WAIT, "blocked bootstrap attempt", || {
                server.requests_for("agent.prompt").len() > prompts_before
            }),
            "blocked agent detection did not produce a bootstrap attempt"
        );
    }
    let pool = || only_pool(&daemon);
    let agent_id = live_agent(&pool()).agent_id.clone();
    assert_eq!(
        pool()
            .registry()
            .get_agent(&agent_id)
            .unwrap()
            .map(|r| r.bootstrap_version)
            .unwrap_or(None),
        None,
        "blocked rejections must not mark the agent bootstrapped"
    );

    // Agent unblocks → the next bootstrap attempt must still be allowed.
    assert!(wait_for_status_subscription(&server, "w1:p1"));
    server.push_subscription_event(status_event("w1:p1", "idle"));
    assert!(
        wait_until(WAIT, "bootstrap after unblock", || {
            pool()
                .registry()
                .get_agent(&agent_id)
                .ok()
                .flatten()
                .and_then(|r| r.bootstrap_version)
                .is_some()
        }),
        "bootstrap was not retried after the agent went idle"
    );

    handle.shutdown();
    handle.join();
}

// ---------------------------------------------------------------------------
// bootstrap waits for interactive_ready (e.g. `devin --resume`)
// ---------------------------------------------------------------------------

#[test]
fn bootstrap_waits_for_interactive_ready() {
    // An agent detected while its TUI is still resuming reports
    // `interactive_ready: false` (status may already be `idle` — idle does
    // NOT imply promptable). The bootstrap prompt must be deferred until a
    // later AgentInfo sighting reports readiness, then sent exactly once.
    let server = FakeHerdr::start().unwrap();
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cwd = project.path().to_path_buf();

    server
        .on("session.snapshot")
        .respond(snapshot_result(vec![]));

    // agent.get resolves readiness from a live flag the test flips.
    let ready = Arc::new(AtomicBool::new(false));
    for _ in 0..24 {
        let ready = Arc::clone(&ready);
        let cwd = cwd.clone();
        server.on("agent.get").then(move |req| {
            let target = req
                .params
                .get("target")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if target != "w1:p1" && target != "term_a" {
                return HandlerReply::Error {
                    code: "agent_not_found".into(),
                    message: format!("no agent for target {target}"),
                };
            }
            let mut agent = agent_json("w1:p1", "term_a", &cwd, "idle");
            agent["interactive_ready"] = json!(ready.load(Ordering::SeqCst));
            agent["launch_pending"] = json!(!ready.load(Ordering::SeqCst));
            HandlerReply::Result(agent_info(agent))
        });
    }

    let daemon = Arc::new(Daemon::new(test_config(state.path(), server.socket_path())).unwrap());
    let handle = daemon.spawn().unwrap();
    assert!(wait_until(WAIT, "events.subscribe", || {
        !server.requests_for("events.subscribe").is_empty()
    }));

    // Detection while still resuming: agent.get is consulted, the record
    // is registered as not-ready, and NO bootstrap prompt is sent.
    server.push_event(agent_detected_event("w1:p1"));
    assert!(
        wait_until(WAIT, "not-ready detection processed", || {
            !server.requests_for("agent.get").is_empty()
        }),
        "agent_detected did not trigger agent.get"
    );
    // Let any (incorrect) prompt arrive, then assert none did.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        bootstrap_prompts(&server),
        0,
        "bootstrap must not fire while interactive_ready=false"
    );
    let pool = || only_pool(&daemon);
    let record = live_agent(&pool());
    assert!(!record.interactive_ready, "readiness must persist");
    assert!(record.launch_pending);

    // The new pane joined the watched set → the daemon resubscribes with
    // a per-pane status filter; pushing before that settles can drop the
    // frame on the floor.
    assert!(
        wait_for_status_subscription(&server, "w1:p1"),
        "daemon did not resubscribe after agent detection"
    );

    // Resume finishes → pane.updated refreshes AgentInfo → ready → the
    // bootstrap prompt goes out exactly once.
    ready.store(true, Ordering::SeqCst);
    server.push_event(pane_updated_event("w1:p1", "term_a", &cwd));
    assert!(
        wait_until(WAIT, "bootstrap after ready", || {
            bootstrap_prompts(&server) == 1
        }),
        "bootstrap was not sent once interactive_ready flipped true"
    );

    // Further sightings do not re-bootstrap (version persisted).
    server.push_event(pane_updated_event("w1:p1", "term_a", &cwd));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(bootstrap_prompts(&server), 1);

    handle.shutdown();
    handle.join();
}
