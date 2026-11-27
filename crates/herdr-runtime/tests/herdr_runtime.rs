//! Integration tests for [`HerdrRuntime`] against `herdr-testkit`'s
//! scriptable fake Herdr server.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::ffi::OsString;
use std::time::Duration;

use herdr_protocol::HerdrClient;
use herdr_runtime::{
    AgentRuntime, DeferReason, HerdrRuntime, RuntimeError, RuntimeEvent, RuntimeStatus,
    SendFailureKind, SendOutcome, WaitEnd, WaitRequest,
};
use herdr_testkit::{canned_result, FakeHerdr};
use serde_json::json;

fn runtime() -> (FakeHerdr, HerdrRuntime) {
    let server = FakeHerdr::start().unwrap();
    let client = HerdrClient::from_socket_path(server.socket_path());
    (server, HerdrRuntime::new(client))
}

// ----- list/get ------------------------------------------------------------

#[test]
fn list_agents_maps_agent_info_records() {
    let (_server, rt) = runtime();
    let agents = rt.list_agents().unwrap();
    assert_eq!(agents.len(), 2);

    let first = &agents[0];
    assert_eq!(first.handle, "w1:p1");
    assert_eq!(first.terminal_id.as_deref(), Some("term_a"));
    assert_eq!(first.kind.as_deref(), Some("claude"));
    assert_eq!(first.name.as_deref(), Some("reviewer"));
    assert_eq!(first.cwd.as_deref(), Some("/repo"));
    assert_eq!(first.status, RuntimeStatus::Blocked);
    assert!(first.interactive_ready);

    let second = &agents[1];
    assert_eq!(second.handle, "w1:p2");
    assert_eq!(second.status, RuntimeStatus::Idle);
}

#[test]
fn get_agent_returns_runtime_agent() {
    let (_server, rt) = runtime();
    let agent = rt.get_agent("w1:p1").unwrap();
    assert_eq!(agent.handle, "w1:p1");
    assert_eq!(agent.name.as_deref(), Some("reviewer"));
    // tokens + state_labels merge into `extra`.
    assert_eq!(
        agent.extra.get("role").map(String::as_str),
        Some("reviewer")
    );
}

#[test]
fn get_agent_not_found_maps_to_runtime_not_found() {
    let (server, rt) = runtime();
    server
        .on("agent.get")
        .error("agent_not_found", "no such agent");
    let err = rt.get_agent("w9:p9").unwrap_err();
    assert!(matches!(err, RuntimeError::NotFound { .. }), "got {err:?}");
}

// ----- send_prompt ---------------------------------------------------------

#[test]
fn send_prompt_success_is_delivered() {
    let (server, rt) = runtime();
    let outcome = rt.send_prompt("w1:p1", "hello").unwrap();
    assert!(
        matches!(outcome, SendOutcome::Delivered { started: None }),
        "got {outcome:?}"
    );
    // The wire call carried target+text and no wait.
    let req = server.last_request_for("agent.prompt").unwrap();
    assert_eq!(req.params["target"], "w1:p1");
    assert_eq!(req.params["text"], "hello");
    assert!(req.params.get("wait").is_none());
}

#[test]
fn send_prompt_blocked_defers() {
    let (server, rt) = runtime();
    server
        .on("agent.prompt")
        .error("agent_blocked", "agent is blocked");
    let outcome = rt.send_prompt("w1:p1", "hi").unwrap();
    assert!(
        matches!(
            outcome,
            SendOutcome::Deferred {
                reason: DeferReason::Blocked
            }
        ),
        "got {outcome:?}"
    );
}

#[test]
fn send_prompt_not_ready_defers() {
    let (server, rt) = runtime();
    server
        .on("agent.prompt")
        .error("agent_not_ready", "still launching");
    let outcome = rt.send_prompt("w1:p1", "hi").unwrap();
    assert!(
        matches!(
            outcome,
            SendOutcome::Deferred {
                reason: DeferReason::NotReady
            }
        ),
        "got {outcome:?}"
    );
}

#[test]
fn send_prompt_not_found_fails() {
    let (server, rt) = runtime();
    server.on("agent.prompt").error("agent_not_found", "gone");
    let outcome = rt.send_prompt("w1:p9", "hi").unwrap();
    assert!(
        matches!(
            outcome,
            SendOutcome::Failed {
                kind: SendFailureKind::NotFound,
                ..
            }
        ),
        "got {outcome:?}"
    );
}

#[test]
fn send_prompt_ambiguous_fails() {
    let (server, rt) = runtime();
    server
        .on("agent.prompt")
        .error("agent_target_ambiguous", "2 matches");
    let outcome = rt.send_prompt("claude", "hi").unwrap();
    assert!(
        matches!(
            outcome,
            SendOutcome::Failed {
                kind: SendFailureKind::Ambiguous,
                ..
            }
        ),
        "got {outcome:?}"
    );
}

#[test]
fn send_prompt_stalled_is_rejected_failure() {
    let (server, rt) = runtime();
    server
        .on("agent.prompt")
        .error("agent_prompt_stalled", "no response");
    let outcome = rt.send_prompt("w1:p1", "hi").unwrap();
    assert!(
        matches!(
            outcome,
            SendOutcome::Failed {
                kind: SendFailureKind::Rejected,
                ..
            }
        ),
        "got {outcome:?}"
    );
}

#[test]
fn send_prompt_transport_error_stays_runtime_error() {
    let (server, rt) = runtime();
    server
        .on("agent.prompt")
        .error("server_unavailable", "backend down");
    let err = rt.send_prompt("w1:p1", "hi").unwrap_err();
    assert!(matches!(err, RuntimeError::Api { .. }), "got {err:?}");
}

// ----- read / keys / notify -----------------------------------------------

#[test]
fn read_agent_returns_recent_text() {
    let (_server, rt) = runtime();
    let text = rt.read_agent("w1:p1", 50).unwrap();
    assert_eq!(text, "hello world\n");
}

#[test]
fn send_keys_forwards_key_list() {
    let (server, rt) = runtime();
    rt.send_keys("w1:p1", &["esc".to_string(), "enter".to_string()])
        .unwrap();
    let req = server.last_request_for("agent.send_keys").unwrap();
    assert_eq!(req.params["target"], "w1:p1");
    assert_eq!(req.params["keys"], json!(["esc", "enter"]));
}

#[test]
fn notify_calls_notification_show() {
    let (server, rt) = runtime();
    rt.notify("pool", Some("new mail")).unwrap();
    let req = server.last_request_for("notification.show").unwrap();
    assert_eq!(req.params["title"], "pool");
    assert_eq!(req.params["body"], "new mail");
}

// ----- wait ----------------------------------------------------------------

#[test]
fn wait_agent_status_returns_agent_on_match() {
    let (server, rt) = runtime();
    server.on("agent.wait").hold();

    let handle = std::thread::spawn({
        // HerdrRuntime is Clone; move a clone into the blocking call.
        // (rt itself is not Sync-shared here — spawn needs 'static.)
        let rt = rt.clone();
        move || {
            rt.wait_agent(
                "w1:p1",
                WaitRequest {
                    until: Some(vec![RuntimeStatus::Idle]),
                    timeout_ms: Some(5_000),
                    output_match: None,
                },
            )
        }
    });

    let req = server
        .wait_for_request("agent.wait", Duration::from_secs(2))
        .expect("agent.wait request");
    assert_eq!(req.params["target"], "w1:p1");
    server.push_response(&req.id, canned_result("agent_info"));

    let outcome = handle.join().unwrap().unwrap();
    assert_eq!(outcome.reason, WaitEnd::Status);
    assert_eq!(outcome.agent.unwrap().handle, "w1:p1");
}

#[test]
fn wait_agent_timeout_maps_to_timed_out() {
    let (server, rt) = runtime();
    server.on("agent.wait").hold();

    let handle = std::thread::spawn({
        let rt = rt.clone();
        move || {
            rt.wait_agent(
                "w1:p1",
                WaitRequest {
                    until: Some(vec![RuntimeStatus::Done]),
                    timeout_ms: Some(5_000),
                    output_match: None,
                },
            )
        }
    });

    let req = server
        .wait_for_request("agent.wait", Duration::from_secs(2))
        .expect("agent.wait request");
    server.push_error(&req.id, "timeout", "wait timed out");

    let outcome = handle.join().unwrap().unwrap();
    assert_eq!(outcome.reason, WaitEnd::Timeout);
}

#[test]
fn wait_agent_output_match_uses_pane_wait_for_output() {
    let (server, rt) = runtime();
    // Default responder for pane.wait_for_output is Hold.
    let handle = std::thread::spawn({
        let rt = rt.clone();
        move || {
            rt.wait_agent(
                "w1:p1",
                WaitRequest {
                    until: None,
                    timeout_ms: Some(5_000),
                    output_match: Some("build finished".to_string()),
                },
            )
        }
    });

    let req = server
        .wait_for_request("pane.wait_for_output", Duration::from_secs(2))
        .expect("pane.wait_for_output request");
    assert_eq!(req.params["pane_id"], "w1:p1");
    assert_eq!(req.params["match"]["type"], "substring");
    assert_eq!(req.params["match"]["value"], "build finished");
    server.push_response(&req.id, canned_result("output_matched"));

    let outcome = handle.join().unwrap().unwrap();
    assert_eq!(outcome.reason, WaitEnd::Output);
}

// ----- self_identity -------------------------------------------------------

#[test]
fn self_identity_reads_herdr_pane_id_env() {
    let (_server, rt) = runtime();
    let env = |key: &str| -> Option<OsString> {
        (key == "HERDR_PANE_ID").then(|| OsString::from("w1:p1"))
    };
    let identity = rt.self_identity(&env).unwrap();
    assert_eq!(identity.handle.as_deref(), Some("w1:p1"));
    // agent.get resolves against the canned agent_info.
    assert_eq!(identity.agent.unwrap().name.as_deref(), Some("reviewer"));
}

#[test]
fn self_identity_without_env_has_no_handle() {
    let (_server, rt) = runtime();
    let env = |_: &str| -> Option<OsString> { None };
    let identity = rt.self_identity(&env).unwrap();
    assert!(identity.handle.is_none());
    assert!(identity.agent.is_none());
    // No socket traffic needed for the no-env path.
}

// ----- capabilities / subscribe_events -------------------------------------

#[test]
fn capabilities_report_herdr_features() {
    let (_server, rt) = runtime();
    let caps = rt.capabilities();
    assert!(caps.notify);
    assert!(caps.plugin_panels);
    assert!(caps.workspaces);
    assert!(!caps.cross_machine);
}

#[test]
fn subscribe_events_returns_snapshot_then_normalized_events() {
    let (server, rt) = runtime();
    let stream = rt.subscribe_events().unwrap();
    // Canned session.snapshot has zero agents.
    assert!(stream.snapshot.is_empty());

    // Lifecycle frame (snake_case) → AgentGone.
    server.push_subscription_event(json!({
        "event": "pane_closed",
        "data": { "type": "pane_closed", "pane_id": "w1:p1", "workspace_id": "w1" }
    }));
    let event = stream
        .events
        .recv_timeout(Duration::from_secs(2))
        .expect("lifecycle event");
    assert!(
        matches!(event, RuntimeEvent::AgentGone { ref handle } if handle == "w1:p1"),
        "got {event:?}"
    );

    // High-frequency frame (dot name) → AgentStatusChanged.
    server.push_subscription_event(json!({
        "event": "pane.agent_status_changed",
        "data": {
            "pane_id": "w1:p2",
            "workspace_id": "w1",
            "agent_status": "working",
            "agent": "codex"
        }
    }));
    let event = stream
        .events
        .recv_timeout(Duration::from_secs(2))
        .expect("subscription event");
    assert!(
        matches!(
            event,
            RuntimeEvent::AgentStatusChanged { ref handle, status: RuntimeStatus::Working }
                if handle == "w1:p2"
        ),
        "got {event:?}"
    );

    // pane.moved → HandleRekeyed.
    server.push_subscription_event(json!({
        "event": "pane_moved",
        "data": {
            "type": "pane_moved",
            "pane": {
                "pane_id": "w1:p9",
                "workspace_id": "w1",
                "tab_id": "w1:t1",
                "terminal_id": "term_z",
                "agent_status": "idle",
                "revision": 1,
                "focused": false
            },
            "previous_pane_id": "w1:p1",
            "previous_workspace_id": "w1",
            "previous_tab_id": "w1:t1"
        }
    }));
    let event = stream
        .events
        .recv_timeout(Duration::from_secs(2))
        .expect("rekey event");
    assert!(
        matches!(
            event,
            RuntimeEvent::HandleRekeyed { ref old, ref new }
                if old == "w1:p1" && new == "w1:p9"
        ),
        "got {event:?}"
    );
}

#[test]
fn subscribe_events_snapshot_lists_agents() {
    let (server, rt) = runtime();
    server.on("session.snapshot").respond(json!({
        "type": "session_snapshot",
        "snapshot": {
            "version": "0.9.5",
            "protocol": 22,
            "workspaces": [],
            "tabs": [],
            "panes": [],
            "layouts": [],
            "agents": [{
                "pane_id": "w1:p1",
                "workspace_id": "w1",
                "tab_id": "w1:t1",
                "terminal_id": "term_a",
                "agent_status": "idle",
                "revision": 1,
                "focused": true,
                "agent": "claude",
                "name": "builder",
                "cwd": "/repo"
            }]
        }
    }));
    let stream = rt.subscribe_events().unwrap();
    assert_eq!(stream.snapshot.len(), 1);
    assert_eq!(stream.snapshot[0].handle, "w1:p1");
    assert_eq!(stream.snapshot[0].name.as_deref(), Some("builder"));
}
