//! End-to-end tests: `Daemon` event loop + `DeliveryWorker` against
//! `FakeHerdr` (scriptable NDJSON unix-socket server).
//!
//! Covers the bead-M4 acceptance paths that need a real socket:
//! snapshot → subscribe (incl. per-pane status subs) → status events →
//! registry updates → outbox delivery with the `[herdr-pool]` envelope,
//! and `agent_blocked` keeping mail pending.

#![cfg(unix)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use herdr_router::daemon::{Daemon, DaemonConfig};
use herdr_router::delivery::DeliveryWorker;
use herdr_router::registry_sync::Notify;
use herdr_router::Pools;
use herdr_testkit::FakeHerdr;
use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(10);

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

fn agent_json(cwd: &std::path::Path, status: &str) -> Value {
    json!({
        "pane_id": "w1:p1",
        "workspace_id": "w1",
        "tab_id": "w1:t1",
        "terminal_id": "term_a",
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

/// The single live agent record across all pools (tests use one scope).
fn only_agent(daemon: &Daemon) -> herdr_pool_store::AgentRecord {
    let pools = daemon.pools().all();
    assert_eq!(pools.len(), 1, "expected exactly one scope pool");
    let agents = pools[0].registry().list_agents(false).unwrap();
    assert_eq!(agents.len(), 1);
    agents.into_iter().next().unwrap()
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

#[test]
fn daemon_snapshot_subscribes_status_events_and_delivers() {
    let server = FakeHerdr::start().unwrap();
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();

    server
        .on("session.snapshot")
        .respond(snapshot_result(vec![agent_json(project.path(), "idle")]));

    let daemon = Arc::new(Daemon::new(test_config(state.path(), server.socket_path())).unwrap());
    let handle = daemon.spawn().unwrap();

    // 1. Snapshot synced → agent registered + bootstrap prompt sent.
    assert!(
        wait_until(WAIT, "bootstrap agent.prompt", || {
            prompt_texts(&server)
                .iter()
                .any(|t| t.contains("[herdr-pool] bootstrap"))
        }),
        "expected a bootstrap prompt"
    );

    // 2. events.subscribe issued with a per-pane status subscription.
    assert!(
        wait_until(WAIT, "events.subscribe", || {
            !server.requests_for("events.subscribe").is_empty()
        }),
        "expected events.subscribe"
    );
    let subs_req = server.last_request_for("events.subscribe").unwrap();
    let subscriptions = subs_req.params["subscriptions"].as_array().unwrap();
    assert!(
        subscriptions
            .iter()
            .any(|s| { s["type"] == "pane.agent_status_changed" && s["pane_id"] == "w1:p1" }),
        "missing per-pane status subscription: {subscriptions:?}"
    );
    for expected in [
        "pane.agent_detected",
        "pane.moved",
        "pane.closed",
        "pane.exited",
        "workspace.closed",
        "tab.closed",
    ] {
        assert!(
            subscriptions.iter().any(|s| s["type"] == expected),
            "missing subscription {expected}"
        );
    }

    let agent = only_agent(&daemon);
    assert_eq!(agent.status, "idle");
    assert_eq!(agent.pane_id.as_deref(), Some("w1:p1"));
    assert_eq!(agent.terminal_id.as_deref(), Some("term_a"));
    assert_eq!(
        agent.bootstrap_version,
        Some(herdr_router::BOOTSTRAP_VERSION)
    );

    // 3. Subscription status event updates the registry.
    server.push_subscription_event(json!({
        "event": "pane.agent_status_changed",
        "data": {
            "pane_id": "w1:p1",
            "workspace_id": "w1",
            "agent_status": "working",
            "agent": "claude",
        }
    }));
    assert!(
        wait_until(WAIT, "status -> working", || {
            only_agent(&daemon).status == "working"
        }),
        "status change not applied"
    );

    // 4. A pending message is delivered via agent.prompt with the envelope.
    let pool = daemon.pools().all().remove(0);
    let msg = pool
        .messages()
        .send("ag_other", &agent.agent_id, "hello from peer")
        .unwrap();
    assert!(
        wait_until(WAIT, "message delivery prompt", || {
            prompt_texts(&server).iter().any(|t| {
                t.contains("[herdr-pool] msg ")
                    && t.contains(&msg.id)
                    && t.contains("hello from peer")
            })
        }),
        "expected delivery prompt with envelope"
    );
    let delivered = pool.messages().message(&msg.id).unwrap().unwrap();
    assert_eq!(delivered.status, herdr_pool_store::MessageStatus::Delivered);

    // The prompt targeted the agent's terminal id (stable anchor).
    let delivery_req = server
        .requests_for("agent.prompt")
        .into_iter()
        .find(|r| {
            r.params
                .get("text")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t.contains(&msg.id))
        })
        .unwrap();
    assert_eq!(delivery_req.params["target"], "w1:p1");

    handle.shutdown();
    handle.join();
}

#[test]
fn agent_blocked_keeps_message_pending() {
    let server = FakeHerdr::start().unwrap();
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();

    server
        .on("session.snapshot")
        .respond(snapshot_result(vec![agent_json(project.path(), "idle")]));

    let daemon = Arc::new(Daemon::new(test_config(state.path(), server.socket_path())).unwrap());
    let handle = daemon.spawn().unwrap();

    // Wait for the bootstrap prompt, then make the NEXT agent.prompt fail
    // with agent_blocked (the delivery attempt for our message).
    assert!(wait_until(WAIT, "bootstrap prompt", || {
        !server.requests_for("agent.prompt").is_empty()
    }));
    server
        .on("agent.prompt")
        .error("agent_blocked", "agent is busy");

    assert!(
        wait_until(WAIT, "agent registered", || {
            daemon.pools().all().iter().any(|p| {
                p.registry()
                    .list_agents(false)
                    .map(|a| !a.is_empty())
                    .unwrap_or(false)
            })
        }),
        "agent never registered"
    );
    let agent = only_agent(&daemon);
    let pool = daemon.pools().all().remove(0);
    let msg = pool
        .messages()
        .send("ag_other", &agent.agent_id, "blocked delivery")
        .unwrap();

    // The blocked attempt defers the message: still pending, scheduled,
    // and — crucially — no delivery attempt was consumed.
    assert!(
        wait_until(WAIT, "blocked defer recorded", || {
            pool.messages()
                .message(&msg.id)
                .map(|m| {
                    m.map(|m| {
                        m.status == herdr_pool_store::MessageStatus::Pending
                            && m.attempt_count == 0
                            && m.next_retry_at.is_some()
                    })
                    .unwrap_or(false)
                })
                .unwrap_or(false)
        }),
        "message should stay pending+scheduled after agent_blocked"
    );
    let after = pool.messages().message(&msg.id).unwrap().unwrap();
    assert_eq!(after.status, herdr_pool_store::MessageStatus::Pending);
    assert_eq!(after.attempt_count, 0);
    assert!(
        after.next_retry_at.is_some(),
        "blocked defer should be scheduled"
    );
    assert!(after.delivered_at.is_none());

    handle.shutdown();
    handle.join();
}

#[test]
fn pane_exited_marks_agent_offline_and_resubscribes() {
    let server = FakeHerdr::start().unwrap();
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();

    server
        .on("session.snapshot")
        .respond(snapshot_result(vec![agent_json(project.path(), "idle")]));

    let daemon = Arc::new(Daemon::new(test_config(state.path(), server.socket_path())).unwrap());
    let handle = daemon.spawn().unwrap();

    assert!(wait_until(WAIT, "events.subscribe", || {
        !server.requests_for("events.subscribe").is_empty()
    }));
    let _ = only_agent(&daemon);
    let subscribes_before = server.requests_for("events.subscribe").len();

    // Lifecycle event (snake_case) broadcast on the subscription stream.
    server.push_event(json!({
        "event": "pane_exited",
        "data": {
            "type": "pane_exited",
            "pane_id": "w1:p1",
            "workspace_id": "w1",
        }
    }));

    assert!(
        wait_until(WAIT, "agent marked offline", || {
            daemon.pools().all().iter().any(|p| {
                p.registry()
                    .list_agents(true)
                    .map(|a| a.iter().any(|r| r.status == "offline"))
                    .unwrap_or(false)
            })
        }),
        "pane.exited did not mark the agent offline"
    );

    // The watched-pane set shrank → the daemon re-issued events.subscribe
    // without the per-pane status subscription for w1:p1.
    assert!(
        wait_until(WAIT, "resubscribe after pane exit", || {
            server.requests_for("events.subscribe").len() > subscribes_before
        }),
        "expected a second events.subscribe after watched set changed"
    );
    let last = server
        .requests_for("events.subscribe")
        .last()
        .unwrap()
        .clone();
    let subscriptions = last.params["subscriptions"].as_array().unwrap();
    assert!(
        !subscriptions
            .iter()
            .any(|s| { s["type"] == "pane.agent_status_changed" && s["pane_id"] == "w1:p1" }),
        "stale status subscription still present: {subscriptions:?}"
    );

    handle.shutdown();
    handle.join();
}

#[test]
fn delivery_worker_blocked_backoff_is_recorded() {
    // Unit-level check of the worker against the fake server, without the
    // daemon event loop: one drain pass, agent_blocked → deferred + retry_at.
    let server = FakeHerdr::start().unwrap();
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();

    let pools = Arc::new(Pools::open(state.path()).unwrap());
    // Register a scope pool directly (scope key content is irrelevant here —
    // the worker only needs a store with a registry row + outbox row).
    let pool = pools
        .for_scope_key("0123456789abcdef0123456789abcdef", project.path())
        .unwrap();
    pool.registry()
        .upsert_agent(&herdr_pool_store::AgentRecord {
            agent_id: "ag_target".into(),
            scope_key: "0123456789abcdef0123456789abcdef".into(),
            terminal_id: Some("term_target".into()),
            pane_id: Some("w9:p9".into()),
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
        .send("ag_sender", "ag_target", "direct worker test")
        .unwrap();

    server.on("agent.prompt").error("agent_blocked", "busy");

    let client = herdr_protocol::HerdrClient::from_socket_path(server.socket_path());
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = DeliveryWorker::new(
        client,
        pools,
        shutdown,
        Notify::default(),
        Duration::from_millis(10),
    );
    assert_eq!(worker.drain_once().unwrap(), 1);

    let after = pool.messages().message(&msg.id).unwrap().unwrap();
    assert_eq!(after.status, herdr_pool_store::MessageStatus::Pending);
    assert_eq!(after.attempt_count, 0);
    assert!(after.next_retry_at.is_some());
    // 15s blocked backoff — a second immediate drain must not retry.
    assert_eq!(worker.drain_once().unwrap(), 0);
}

#[test]
fn delivery_worker_defers_not_ready_recipient_without_prompting() {
    // A recipient whose TUI is still launching/resuming
    // (`interactive_ready=false` / `launch_pending=true`) must not receive
    // `agent.prompt` — the envelope would be pasted into a half-restored
    // session. The message defers without consuming an attempt.
    let server = FakeHerdr::start().unwrap();
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();

    let pools = Arc::new(Pools::open(state.path()).unwrap());
    let pool = pools
        .for_scope_key("0123456789abcdef0123456789abcdef", project.path())
        .unwrap();
    pool.registry()
        .upsert_agent(&herdr_pool_store::AgentRecord {
            agent_id: "ag_target".into(),
            scope_key: "0123456789abcdef0123456789abcdef".into(),
            terminal_id: Some("term_target".into()),
            pane_id: Some("w9:p9".into()),
            agent_kind: Some("claude".into()),
            name: None,
            cwd: None,
            status: "idle".into(),
            interactive_ready: false,
            launch_pending: true,
            bootstrap_version: None,
            first_seen_at: 0,
            last_seen_at: 0,
            offline_at: None,
        })
        .unwrap();
    let msg = pool
        .messages()
        .send("ag_sender", "ag_target", "resume in progress")
        .unwrap();

    // No agent.prompt responder is registered — any prompt attempt would
    // surface as a request (and likely an error), which we assert absent.
    let client = herdr_protocol::HerdrClient::from_socket_path(server.socket_path());
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = DeliveryWorker::new(
        client,
        pools,
        shutdown,
        Notify::default(),
        Duration::from_millis(10),
    );
    assert_eq!(worker.drain_once().unwrap(), 1);

    let after = pool.messages().message(&msg.id).unwrap().unwrap();
    assert_eq!(after.status, herdr_pool_store::MessageStatus::Pending);
    assert_eq!(after.attempt_count, 0);
    assert!(after.next_retry_at.is_some());
    assert!(
        server.requests_for("agent.prompt").is_empty(),
        "agent.prompt must not be called for a not-ready recipient"
    );
    // Deferred ~15s — an immediate second drain skips it.
    assert_eq!(worker.drain_once().unwrap(), 0);
}

#[test]
fn delivery_worker_blocked_creates_rescue_job_and_dispatches_helper() {
    // agent_blocked on delivery → durable rescue job → rescue sweep picks
    // an eligible peer and sends a [HERDR RESCUE REQUEST] prompt.
    let server = FakeHerdr::start().unwrap();
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();

    let pools = Arc::new(Pools::open(state.path()).unwrap());
    let pool = pools
        .for_scope_key("0123456789abcdef0123456789abcdef", project.path())
        .unwrap();
    let record =
        |agent_id: &str, pane_id: &str, name: Option<&str>| herdr_pool_store::AgentRecord {
            agent_id: agent_id.into(),
            scope_key: "0123456789abcdef0123456789abcdef".into(),
            terminal_id: Some(format!("term_{agent_id}")),
            pane_id: Some(pane_id.into()),
            agent_kind: Some("claude".into()),
            name: name.map(str::to_string),
            cwd: None,
            status: "idle".into(),
            interactive_ready: true,
            launch_pending: false,
            bootstrap_version: None,
            first_seen_at: 0,
            last_seen_at: 0,
            offline_at: None,
        };
    pool.registry()
        .upsert_agent(&record("ag_target", "w9:p9", Some("target")))
        .unwrap();
    pool.registry()
        .upsert_agent(&record("ag_helper", "w9:p8", Some("helper")))
        .unwrap();
    pool.messages()
        .send("ag_sender", "ag_target", "rescue test")
        .unwrap();

    server.on("agent.prompt").then(|req| {
        if req.params["target"] == "w9:p9" {
            herdr_testkit::HandlerReply::Error {
                code: "agent_blocked".into(),
                message: "busy".into(),
            }
        } else {
            herdr_testkit::HandlerReply::Result(herdr_testkit::canned_result("agent_prompted"))
        }
    });

    let client = herdr_protocol::HerdrClient::from_socket_path(server.socket_path());
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = DeliveryWorker::new(
        client,
        pools,
        shutdown,
        Notify::default(),
        Duration::from_millis(10),
    );
    assert_eq!(worker.drain_once().unwrap(), 1);

    // The blocked delivery ensured a pending rescue job for ag_target.
    let open = pool.rescues().open_for_source("ag_target").unwrap();
    assert!(
        open.is_some(),
        "rescue job should exist after agent_blocked"
    );
    assert_eq!(
        open.as_ref().unwrap().status,
        herdr_pool_store::RescueStatus::Pending
    );

    // Rescue sweep: snapshot (agent.read canned) → pick ag_helper →
    // reserve → dispatch the rescue prompt.
    worker.sweep_rescue().unwrap();

    let job = pool
        .rescues()
        .open_for_source("ag_target")
        .unwrap()
        .unwrap();
    assert_eq!(job.status, herdr_pool_store::RescueStatus::Running);
    assert_eq!(job.helper_agent_id.as_deref(), Some("ag_helper"));

    let prompts = server.requests_for("agent.prompt");
    assert_eq!(prompts.len(), 2, "delivery + rescue prompts: {prompts:?}");
    assert_eq!(prompts[0].params["target"], "w9:p9");
    assert_eq!(prompts[1].params["target"], "w9:p8");
    let rescue = prompts[1].params["text"].as_str().unwrap();
    assert!(rescue.contains("[HERDR RESCUE REQUEST]"), "{rescue}");
    assert!(rescue.contains("ag_target"), "{rescue}");
    assert!(rescue.contains("w9:p9"), "{rescue}");

    // Second sweep while the source is still blocked: no duplicate job,
    // no second dispatch (helper reservation held by the running job).
    worker.sweep_rescue().unwrap();
    assert_eq!(server.requests_for("agent.prompt").len(), 2);

    // Source recovers → next sweep resolves the job.
    let mut recovered = record("ag_target", "w9:p9", Some("target"));
    recovered.status = "idle".into();
    pool.registry().upsert_agent(&recovered).unwrap();
    worker.sweep_rescue().unwrap();
    let done = pool.rescues().get(&job.rescue_id).unwrap().unwrap();
    assert_eq!(done.status, herdr_pool_store::RescueStatus::Resolved);
}

#[test]
fn rescue_dispatches_multiple_blocked_agents_to_distinct_helpers() {
    // Case 2: two blocked sources, two idle helpers → both rescues dispatch
    // in parallel, each to a different helper (R5/R6).
    let server = FakeHerdr::start().unwrap();
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();

    let pools = Arc::new(Pools::open(state.path()).unwrap());
    let pool = pools
        .for_scope_key("0123456789abcdef0123456789abcdef", project.path())
        .unwrap();
    let record = |agent_id: &str, pane_id: &str, status: &str| herdr_pool_store::AgentRecord {
        agent_id: agent_id.into(),
        scope_key: "0123456789abcdef0123456789abcdef".into(),
        terminal_id: Some(format!("term_{agent_id}")),
        pane_id: Some(pane_id.into()),
        agent_kind: Some("claude".into()),
        name: None,
        cwd: None,
        status: status.into(),
        interactive_ready: true,
        launch_pending: false,
        bootstrap_version: None,
        first_seen_at: 0,
        last_seen_at: 0,
        offline_at: None,
    };
    for (id, pane, status) in [
        ("ag_src1", "w9:p11", "blocked"),
        ("ag_src2", "w9:p12", "blocked"),
        ("ag_helper1", "w9:p21", "idle"),
        ("ag_helper2", "w9:p22", "idle"),
    ] {
        pool.registry()
            .upsert_agent(&record(id, pane, status))
            .unwrap();
    }
    let rescues = pool.rescues();
    rescues
        .create_pending(
            "ag_src1",
            Some("w9:p11"),
            Some("tui-1".into()),
            Some("visible".into()),
        )
        .unwrap();
    rescues
        .create_pending(
            "ag_src2",
            Some("w9:p12"),
            Some("tui-2".into()),
            Some("visible".into()),
        )
        .unwrap();

    server
        .on("agent.prompt")
        .respond(herdr_testkit::canned_result("agent_prompted"));

    let client = herdr_protocol::HerdrClient::from_socket_path(server.socket_path());
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = DeliveryWorker::new(
        client,
        pools,
        shutdown,
        Notify::default(),
        Duration::from_millis(10),
    );
    worker.sweep_rescue().unwrap();

    let j1 = rescues.open_for_source("ag_src1").unwrap().unwrap();
    let j2 = rescues.open_for_source("ag_src2").unwrap().unwrap();
    assert_eq!(j1.status, herdr_pool_store::RescueStatus::Running);
    assert_eq!(j2.status, herdr_pool_store::RescueStatus::Running);
    assert_ne!(
        j1.helper_agent_id, j2.helper_agent_id,
        "each rescue must hold a distinct helper reservation"
    );

    let prompts = server.requests_for("agent.prompt");
    assert_eq!(
        prompts.len(),
        2,
        "one rescue prompt per source: {prompts:?}"
    );
    let targets: Vec<&str> = prompts
        .iter()
        .filter_map(|r| r.params["target"].as_str())
        .collect();
    assert!(
        targets.contains(&"w9:p21") && targets.contains(&"w9:p22"),
        "{targets:?}"
    );
    // Snapshot handoff: each helper prompt carries its own source's TUI.
    let texts: Vec<&str> = prompts
        .iter()
        .filter_map(|r| r.params["text"].as_str())
        .collect();
    assert!(texts
        .iter()
        .any(|t| t.contains("tui-1") && t.contains("ag_src1")));
    assert!(texts
        .iter()
        .any(|t| t.contains("tui-2") && t.contains("ag_src2")));
}

#[test]
fn rescue_without_helper_stays_pending() {
    // R7: only blocked agents in scope → jobs remain pending, no prompts.
    let server = FakeHerdr::start().unwrap();
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();

    let pools = Arc::new(Pools::open(state.path()).unwrap());
    let pool = pools
        .for_scope_key("0123456789abcdef0123456789abcdef", project.path())
        .unwrap();
    for (id, pane) in [("ag_a", "w9:p1"), ("ag_b", "w9:p2")] {
        pool.registry()
            .upsert_agent(&herdr_pool_store::AgentRecord {
                agent_id: id.into(),
                scope_key: "0123456789abcdef0123456789abcdef".into(),
                terminal_id: Some(format!("term_{id}")),
                pane_id: Some(pane.into()),
                agent_kind: Some("claude".into()),
                name: None,
                cwd: None,
                status: "blocked".into(),
                interactive_ready: true,
                launch_pending: false,
                bootstrap_version: None,
                first_seen_at: 0,
                last_seen_at: 0,
                offline_at: None,
            })
            .unwrap();
    }
    let rescues = pool.rescues();
    rescues
        .create_pending("ag_a", Some("w9:p1"), None, None)
        .unwrap();
    rescues
        .create_pending("ag_b", Some("w9:p2"), None, None)
        .unwrap();

    let client = herdr_protocol::HerdrClient::from_socket_path(server.socket_path());
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = DeliveryWorker::new(
        client,
        pools,
        shutdown,
        Notify::default(),
        Duration::from_millis(10),
    );
    // Several sweeps: still no eligible helper, jobs stay pending.
    for _ in 0..3 {
        worker.sweep_rescue().unwrap();
    }

    assert!(server.requests_for("agent.prompt").is_empty());
    for src in ["ag_a", "ag_b"] {
        let job = rescues.open_for_source(src).unwrap().unwrap();
        assert_eq!(job.status, herdr_pool_store::RescueStatus::Pending);
        assert!(job.helper_agent_id.is_none());
    }
}

#[test]
fn rescue_ambiguous_dispatch_holds_assignment_then_redispatches_fresh_token() {
    // The dispatch prompt's outcome is lost (connection dropped after
    // Herdr may have delivered it). The job must NOT be unassigned
    // immediately — that would let a second helper be dispatched while
    // the first may already hold the prompt. Instead it stays `assigned`
    // until the stale window, then re-dispatches with a NEW fencing token
    // that locks the first helper out of claim/report.
    let server = FakeHerdr::start().unwrap();
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();

    let pools = Arc::new(Pools::open(state.path()).unwrap());
    let pool = pools
        .for_scope_key("0123456789abcdef0123456789abcdef", project.path())
        .unwrap();
    let record = |agent_id: &str, pane_id: &str, status: &str| herdr_pool_store::AgentRecord {
        agent_id: agent_id.into(),
        scope_key: "0123456789abcdef0123456789abcdef".into(),
        terminal_id: Some(format!("term_{agent_id}")),
        pane_id: Some(pane_id.into()),
        agent_kind: Some("claude".into()),
        name: None,
        cwd: None,
        status: status.into(),
        interactive_ready: true,
        launch_pending: false,
        bootstrap_version: None,
        first_seen_at: 0,
        last_seen_at: 0,
        offline_at: None,
    };
    pool.registry()
        .upsert_agent(&record("ag_src", "w9:p10", "blocked"))
        .unwrap();
    pool.registry()
        .upsert_agent(&record("ag_helper", "w9:p20", "idle"))
        .unwrap();
    pool.rescues()
        .create_pending(
            "ag_src",
            Some("w9:p10"),
            Some("tui".into()),
            Some("visible".into()),
        )
        .unwrap();

    // The dispatch prompt fails with a server-side `timeout` API error —
    // ambiguous: Herdr may have delivered the prompt to the helper even
    // though no success response came back. (A definitive rejection like
    // `agent_blocked` would release the assignment instead.)
    server
        .on("agent.prompt")
        .error("timeout", "prompt timed out");

    let client = herdr_protocol::HerdrClient::from_socket_path(server.socket_path());
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = DeliveryWorker::new(
        client,
        pools,
        shutdown,
        Notify::default(),
        Duration::from_millis(10),
    )
    .with_rescue_timeouts(Duration::from_secs(600), 5, Duration::from_millis(100));
    worker.sweep_rescue().unwrap();

    // Ambiguous outcome → assignment HELD (not released to pending),
    // helper reservation kept, dispatch token persisted.
    let job = pool.rescues().open_for_source("ag_src").unwrap().unwrap();
    assert_eq!(job.status, herdr_pool_store::RescueStatus::Assigned);
    assert_eq!(job.helper_agent_id.as_deref(), Some("ag_helper"));
    let token_v1 = job.dispatch_token.clone().expect("dispatch token");
    assert!(token_v1.starts_with("dt_"));
    assert_eq!(server.requests_for("agent.prompt").len(), 1);
    // The prompt carried the fencing token + claim instructions.
    let text = server.requests_for("agent.prompt")[0].params["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(text.contains(&token_v1), "{text}");
    assert!(text.contains("claim"), "{text}");

    // Within the stale window: another sweep must not re-dispatch.
    worker.sweep_rescue().unwrap();
    assert_eq!(server.requests_for("agent.prompt").len(), 1);

    // Past the stale window: first sweep returns the job to pending,
    // second sweep re-dispatches — the new prompt carries a DIFFERENT
    // fencing token.
    std::thread::sleep(Duration::from_millis(150));
    worker.sweep_rescue().unwrap();
    let job = pool.rescues().open_for_source("ag_src").unwrap().unwrap();
    assert_eq!(job.status, herdr_pool_store::RescueStatus::Pending);
    worker.sweep_rescue().unwrap();
    let job = pool.rescues().open_for_source("ag_src").unwrap().unwrap();
    assert_eq!(job.status, herdr_pool_store::RescueStatus::Running);
    let token_v2 = job.dispatch_token.clone().expect("token v2");
    assert_ne!(token_v1, token_v2, "re-dispatch must mint a fresh token");

    let prompts = server.requests_for("agent.prompt");
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    let text2 = prompts[1].params["text"].as_str().unwrap();
    assert!(text2.contains(&token_v2), "{text2}");
    assert!(!text2.contains(&token_v1), "{text2}");
}

#[test]
fn delivery_worker_notifies_on_delivery_when_enabled() {
    let server = FakeHerdr::start().unwrap();
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();

    let pools = Arc::new(Pools::open(state.path()).unwrap());
    let pool = pools
        .for_scope_key("0123456789abcdef0123456789abcdef", project.path())
        .unwrap();
    pool.registry()
        .upsert_agent(&herdr_pool_store::AgentRecord {
            agent_id: "ag_target".into(),
            scope_key: "0123456789abcdef0123456789abcdef".into(),
            terminal_id: Some("term_target".into()),
            pane_id: Some("w9:p9".into()),
            agent_kind: Some("claude".into()),
            name: Some("target".into()),
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
    pool.messages()
        .send("ag_sender", "ag_target", "notify me")
        .unwrap();

    let client = herdr_protocol::HerdrClient::from_socket_path(server.socket_path());
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = DeliveryWorker::new(
        client,
        pools,
        shutdown,
        Notify::default(),
        Duration::from_millis(10),
    )
    .with_notify_on_delivery(true);
    assert_eq!(worker.drain_once().unwrap(), 1);

    let toasts = server.requests_for("notification.show");
    assert_eq!(toasts.len(), 1, "expected one delivery toast");
    let title = toasts[0].params["title"].as_str().unwrap();
    assert!(title.contains("target"), "{title}");
}

#[test]
fn agent_scope_move_retires_stale_record() {
    // Same terminal_id re-appearing under a different project cwd must not
    // leave a live ghost record in the old scope's registry.
    let server = FakeHerdr::start().unwrap();
    let state = tempfile::tempdir().unwrap();
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();

    let client = herdr_protocol::HerdrClient::from_socket_path(server.socket_path());
    let pools = Arc::new(Pools::open(state.path()).unwrap());
    let mut sync = herdr_router::RegistrySync::new(&client, pools.as_ref(), true);

    // Bootstrap: agent lives in scope A.
    server
        .on("session.snapshot")
        .respond(snapshot_result(vec![agent_json(dir_a.path(), "idle")]));
    assert_eq!(sync.sync_snapshot().unwrap(), 1);

    let live_in = |pools: &Pools| -> Vec<(String, String)> {
        pools
            .all()
            .iter()
            .flat_map(|p| {
                p.registry()
                    .list_agents(true)
                    .unwrap()
                    .into_iter()
                    .map(move |a| (p.scope_key().to_string(), a.status))
            })
            .collect()
    };
    let before = live_in(&pools);
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].1, "idle");

    // Reconcile: the same terminal now reports cwd B (terminal reused by
    // another project). agent.get is not needed — reconcile upserts the
    // AgentInfo directly.
    server.on("agent.list").respond(json!({
        "type": "agent_list",
        "agents": [agent_json(dir_b.path(), "idle")],
    }));
    assert!(sync.reconcile().unwrap() >= 1);

    let after = live_in(&pools);
    assert_eq!(after.len(), 2, "expected records in both scopes: {after:?}");
    let mut statuses: Vec<&str> = after.iter().map(|(_, s)| s.as_str()).collect();
    statuses.sort_unstable();
    assert_eq!(statuses, ["idle", "offline"], "records: {after:?}");
}
