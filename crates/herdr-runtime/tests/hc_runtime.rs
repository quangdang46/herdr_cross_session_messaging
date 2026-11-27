//! Integration tests for [`HcRuntime`] against a fake `hc connect`
//! (NDJSON JSON-RPC over stdio, driven by files in a temp dir — see
//! `fake-hc.sh`).

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use herdr_runtime::hc::HcRuntime;
use herdr_runtime::{
    AgentRuntime, DeferReason, RuntimeError, RuntimeEvent, RuntimeStatus, SendFailureKind,
    SendOutcome, WaitEnd, WaitRequest,
};
use serde_json::{json, Value};
use tempfile::TempDir;

fn fake_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-hc.sh"));
    cmd.env("FAKE_HC_DIR", dir);
    cmd
}

fn resp_path(dir: &Path, method: &str) -> PathBuf {
    dir.join("resp").join(format!("{method}.json"))
}

fn err_path(dir: &Path, method: &str) -> PathBuf {
    dir.join("resp").join(format!("{method}.err"))
}

fn write_resp(dir: &Path, method: &str, body: Value) {
    fs::create_dir_all(dir.join("resp")).unwrap();
    fs::write(resp_path(dir, method), body.to_string()).unwrap();
}

fn write_err(dir: &Path, method: &str, kind: &str) {
    fs::create_dir_all(dir.join("resp")).unwrap();
    let err = json!({
        "code": -32000,
        "message": kind,
        "data": {"kind": kind},
    });
    fs::write(err_path(dir, method), err.to_string()).unwrap();
}

/// A live, 7501-reporting terminal fixture.
fn terminal_json(address: &str) -> Value {
    json!({
        "address": address,
        "name": null,
        "labels": {},
        "cwd": "/repo",
        "argv": ["/bin/claude"],
        "state": "live",
        "reported": true,
        "status": {"state": "idle", "app": "claude", "msg": "", "title": ""},
        "foreground": {"name": "claude", "argv": ["claude"], "cwd": "/repo/sub", "pid": 42},
        "reason": null,
        "turn": 1,
        "created_ms": 1
    })
}

fn fixture() -> (TempDir, PathBuf, HcRuntime) {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_path_buf();
    write_resp(&state, "hello", json!({"version": "0.1.17", "methods": []}));
    let rt = HcRuntime::connect(fake_cmd(&state)).unwrap();
    (dir, state, rt)
}

fn seen_requests(state: &Path) -> String {
    fs::read_to_string(state.join("seen.log")).unwrap_or_default()
}

fn count_method(state: &Path, method: &str) -> usize {
    seen_requests(state)
        .lines()
        .filter(|l| l.contains(&format!("\"method\":\"{method}\"")))
        .count()
}

// ----- list / get ----------------------------------------------------------

#[test]
fn list_agents_maps_terminal_records() {
    let (_t, state, rt) = fixture();
    let mut reviewer = terminal_json("t7");
    reviewer["labels"] = json!({"herdr-pool.name": "reviewer"});
    reviewer["status"]["state"] = json!("blocked");
    let mut remote = terminal_json("machine/t3");
    remote["reported"] = json!(false);
    remote["status"] = Value::Null;
    remote["foreground"] = Value::Null;
    remote["argv"] = json!(["/bin/sh"]);
    write_resp(
        &state,
        "terminal.list",
        json!({"terminals": [reviewer, remote], "hidden_finished": 0, "machines": []}),
    );

    let agents = rt.list_agents().unwrap();
    assert_eq!(agents.len(), 2);

    let first = &agents[0];
    assert_eq!(first.handle, "t7");
    assert_eq!(first.terminal_id, None);
    assert_eq!(first.name.as_deref(), Some("reviewer"));
    assert_eq!(first.kind.as_deref(), Some("claude"));
    assert_eq!(first.cwd.as_deref(), Some("/repo"));
    assert_eq!(first.foreground_cwd.as_deref(), Some("/repo/sub"));
    assert_eq!(first.status, RuntimeStatus::Blocked);
    assert!(first.interactive_ready);

    // Remote machine-qualified address passes through unchanged.
    let second = &agents[1];
    assert_eq!(second.handle, "machine/t3");
    assert_eq!(second.status, RuntimeStatus::Unknown);
    assert!(!second.interactive_ready);
    // Live + never reported + just observed → launch_pending heuristic.
    assert!(second.launch_pending);
    // kind falls back to argv[0] basename when no status/foreground name.
    assert_eq!(second.kind.as_deref(), Some("sh"));
}

#[test]
fn get_agent_returns_runtime_agent() {
    let (_t, state, rt) = fixture();
    write_resp(&state, "terminal.get", terminal_json("t7"));
    let agent = rt.get_agent("t7").unwrap();
    assert_eq!(agent.handle, "t7");
    assert_eq!(agent.status, RuntimeStatus::Idle);
    assert!(agent.interactive_ready);
    assert!(!agent.launch_pending);
}

#[test]
fn get_agent_not_found_maps_to_runtime_not_found() {
    let (_t, state, rt) = fixture();
    write_err(&state, "terminal.get", "not_found");
    let err = rt.get_agent("t9").unwrap_err();
    assert!(matches!(err, RuntimeError::NotFound { .. }), "got {err:?}");
}

#[test]
fn get_agent_finished_maps_to_offline() {
    let (_t, state, rt) = fixture();
    let mut t = terminal_json("t7");
    t["state"] = json!("finished");
    t["status"] = Value::Null;
    write_resp(&state, "terminal.get", t);
    let agent = rt.get_agent("t7").unwrap();
    assert_eq!(agent.status, RuntimeStatus::Offline);
    assert!(!agent.interactive_ready);
}

// ----- send_prompt ---------------------------------------------------------

#[test]
fn send_prompt_delivers_and_reports_started() {
    let (_t, state, rt) = fixture();
    write_resp(&state, "terminal.get", terminal_json("t7"));
    write_resp(
        &state,
        "terminal.send",
        json!({"started": true, "status": "working", "turn": 3}),
    );

    let outcome = rt.send_prompt("t7", "hello pool").unwrap();
    assert!(
        matches!(
            outcome,
            SendOutcome::Delivered {
                started: Some(true)
            }
        ),
        "got {outcome:?}"
    );
    let seen = seen_requests(&state);
    assert!(
        seen.contains("\"method\":\"terminal.send\""),
        "seen: {seen}"
    );
    assert!(seen.contains("\"terminal\":\"t7\""));
    assert!(seen.contains("\"text\":\"hello pool\""));
    assert!(seen.contains("\"caller\":\"herdr-pool:"));
}

#[test]
fn send_prompt_blocked_status_defers_without_sending() {
    let (_t, state, rt) = fixture();
    let mut t = terminal_json("t7");
    t["status"]["state"] = json!("blocked");
    write_resp(&state, "terminal.get", t);

    let outcome = rt.send_prompt("t7", "hi").unwrap();
    assert!(
        matches!(
            outcome,
            SendOutcome::Deferred {
                reason: DeferReason::Blocked
            }
        ),
        "got {outcome:?}"
    );
    assert_eq!(count_method(&state, "terminal.send"), 0);
}

#[test]
fn send_prompt_unreported_fresh_terminal_defers_not_ready() {
    let (_t, state, rt) = fixture();
    // Live shell that never reported 7501 → launch_pending window.
    let mut t = terminal_json("t7");
    t["reported"] = json!(false);
    t["status"] = Value::Null;
    write_resp(&state, "terminal.get", t);

    let outcome = rt.send_prompt("t7", "hi").unwrap();
    assert!(
        matches!(
            outcome,
            SendOutcome::Deferred {
                reason: DeferReason::NotReady
            }
        ),
        "got {outcome:?}"
    );
    assert_eq!(count_method(&state, "terminal.send"), 0);
}

#[test]
fn send_prompt_spawn_pending_defers_not_ready() {
    let (_t, state, rt) = fixture();
    let mut t = terminal_json("t7");
    t["reason"] = json!("spawn_pending");
    write_resp(&state, "terminal.get", t);

    let outcome = rt.send_prompt("t7", "hi").unwrap();
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
fn send_prompt_finished_terminal_fails_not_found() {
    let (_t, state, rt) = fixture();
    let mut t = terminal_json("t7");
    t["state"] = json!("finished");
    write_resp(&state, "terminal.get", t);

    let outcome = rt.send_prompt("t7", "hi").unwrap();
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
    assert_eq!(count_method(&state, "terminal.send"), 0);
}

#[test]
fn send_prompt_pre_poll_not_found_fails() {
    let (_t, state, rt) = fixture();
    write_err(&state, "terminal.get", "not_found");

    let outcome = rt.send_prompt("t9", "hi").unwrap();
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
fn send_prompt_blocked_api_error_defers() {
    let (_t, state, rt) = fixture();
    write_resp(&state, "terminal.get", terminal_json("t7"));
    // Status flipped to blocked between poll and send — the core refuses
    // with error kind `blocked` (no force flag sent).
    write_err(&state, "terminal.send", "blocked");

    let outcome = rt.send_prompt("t7", "hi").unwrap();
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
fn send_prompt_send_not_found_fails() {
    let (_t, state, rt) = fixture();
    write_resp(&state, "terminal.get", terminal_json("t7"));
    write_err(&state, "terminal.send", "not_found");

    let outcome = rt.send_prompt("t7", "hi").unwrap();
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
fn send_prompt_started_null_maps_to_unknown() {
    let (_t, state, rt) = fixture();
    write_resp(&state, "terminal.get", terminal_json("t7"));
    // Program doesn't report 7501 → started is null.
    write_resp(
        &state,
        "terminal.send",
        json!({"started": null, "status": null, "turn": 4}),
    );

    let outcome = rt.send_prompt("t7", "hi").unwrap();
    assert!(
        matches!(outcome, SendOutcome::Delivered { started: None }),
        "got {outcome:?}"
    );
}

// ----- wait ----------------------------------------------------------------

#[test]
fn wait_agent_status_match_returns_agent() {
    let (_t, state, rt) = fixture();
    write_resp(
        &state,
        "terminal.wait",
        json!({
            "terminal": "t7", "ended": true, "reason": "status",
            "turn": 2, "children": [], "status": "idle"
        }),
    );
    write_resp(&state, "terminal.get", terminal_json("t7"));

    let outcome = rt
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
    assert_eq!(outcome.agent.unwrap().status, RuntimeStatus::Idle);

    let seen = seen_requests(&state);
    let wait_line = seen
        .lines()
        .find(|l| l.contains("\"method\":\"terminal.wait\""))
        .expect("terminal.wait request");
    assert!(wait_line.contains("\"caller\":\"herdr-pool:"));
    assert!(wait_line.contains("\"timeout\":"));
}

#[test]
fn wait_agent_timeout_check_in_maps_to_timeout() {
    let (_t, state, rt) = fixture();
    write_resp(
        &state,
        "terminal.wait",
        json!({
            "terminal": "t7", "ended": false, "reason": "timeout",
            "turn": 2, "children": [], "status": "working"
        }),
    );

    let outcome = rt
        .wait_agent(
            "t7",
            WaitRequest {
                until: Some(vec![RuntimeStatus::Idle]),
                timeout_ms: Some(50),
                output_match: None,
            },
        )
        .unwrap();
    assert_eq!(outcome.reason, WaitEnd::Timeout);
}

#[test]
fn wait_agent_output_match_passes_output_param() {
    let (_t, state, rt) = fixture();
    write_resp(
        &state,
        "terminal.wait",
        json!({
            "terminal": "t7", "ended": true, "reason": "output",
            "turn": 2, "children": []
        }),
    );
    write_resp(&state, "terminal.get", terminal_json("t7"));

    let outcome = rt
        .wait_agent(
            "t7",
            WaitRequest {
                until: None,
                timeout_ms: Some(5_000),
                output_match: Some("build finished".to_string()),
            },
        )
        .unwrap();
    assert_eq!(outcome.reason, WaitEnd::Output);

    let seen = seen_requests(&state);
    let wait_line = seen
        .lines()
        .find(|l| l.contains("\"method\":\"terminal.wait\""))
        .expect("terminal.wait request");
    assert!(wait_line.contains("\"output\":\"build finished\""));
}

#[test]
fn wait_agent_remismatched_status_rewaits_until_deadline() {
    let (_t, state, rt) = fixture();
    // Wait keeps ending on `done` while we asked for `idle`.
    write_resp(
        &state,
        "terminal.wait",
        json!({
            "terminal": "t7", "ended": true, "reason": "status",
            "turn": 2, "children": [], "status": "done"
        }),
    );
    let mut done_term = terminal_json("t7");
    done_term["status"]["state"] = json!("done");
    write_resp(&state, "terminal.get", done_term);

    let outcome = rt
        .wait_agent(
            "t7",
            WaitRequest {
                until: Some(vec![RuntimeStatus::Idle]),
                // Long enough for several wait→check→backoff rounds.
                timeout_ms: Some(1_300),
                output_match: None,
            },
        )
        .unwrap();
    assert_eq!(outcome.reason, WaitEnd::Timeout);
    // At least two wait calls happened (initial + re-wait).
    assert!(count_method(&state, "terminal.wait") >= 2);
}

// ----- read / keys ---------------------------------------------------------

#[test]
fn read_agent_returns_text() {
    let (_t, state, rt) = fixture();
    write_resp(
        &state,
        "terminal.read",
        json!({"text": "hello world\n", "complete": true, "how": "screen"}),
    );
    assert_eq!(rt.read_agent("t7", 50).unwrap(), "hello world\n");
}

#[test]
fn send_keys_forwards_keys() {
    let (_t, state, rt) = fixture();
    write_resp(&state, "terminal.keys", json!({"turn": 3}));
    rt.send_keys("t7", &["esc".to_string(), "enter".to_string()])
        .unwrap();
    let seen = seen_requests(&state);
    assert!(seen.contains("\"method\":\"terminal.keys\""));
    assert!(seen.contains("\"keys\":[\"esc\",\"enter\"]"));
}

// ----- self_identity -------------------------------------------------------

#[test]
fn self_identity_reads_hc_terminal_id_env() {
    let (_t, state, rt) = fixture();
    write_resp(&state, "terminal.get", terminal_json("t7"));
    let env =
        |key: &str| -> Option<OsString> { (key == "HC_TERMINAL_ID").then(|| OsString::from("t7")) };
    let identity = rt.self_identity(&env).unwrap();
    assert_eq!(identity.handle.as_deref(), Some("t7"));
    assert_eq!(identity.agent.unwrap().handle, "t7");
}

#[test]
fn self_identity_without_env_has_no_handle() {
    let (_t, _state, rt) = fixture();
    let env = |_: &str| -> Option<OsString> { None };
    let identity = rt.self_identity(&env).unwrap();
    assert!(identity.handle.is_none());
    assert!(identity.agent.is_none());
}

// ----- capabilities / subscribe_events -------------------------------------

#[test]
fn capabilities_report_hc_features() {
    let (_t, _state, rt) = fixture();
    let caps = rt.capabilities();
    assert!(!caps.notify);
    assert!(!caps.plugin_panels);
    assert!(!caps.workspaces);
    assert!(caps.cross_machine);
}

#[test]
fn subscribe_events_returns_snapshot_then_normalized_events() {
    let (_t, state, rt) = fixture();
    write_resp(
        &state,
        "events.subscribe",
        json!({"seq": 5, "terminals": [terminal_json("t9")]}),
    );
    // status event: t9 went idle→working; then finished → AgentGone.
    fs::write(
        state.join("event.json"),
        concat!(
            "{\"jsonrpc\":\"2.0\",\"method\":\"event\",\"params\":{\"type\":\"status\",\"terminal\":\"t9\",\"data\":{\"state\":\"working\",\"app\":\"claude\"},\"seq\":6}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"event\",\"params\":{\"type\":\"finished\",\"terminal\":\"t9\",\"data\":{\"address\":\"t9\",\"state\":\"finished\"},\"seq\":7}}\n"
        ),
    )
    .unwrap();

    let stream = rt.subscribe_events(&[]).unwrap();
    assert_eq!(stream.snapshot.len(), 1);
    assert_eq!(stream.snapshot[0].handle, "t9");
    assert_eq!(stream.snapshot[0].status, RuntimeStatus::Idle);

    let event = stream
        .events
        .recv_timeout(Duration::from_secs(3))
        .expect("status event");
    assert!(
        matches!(
            event,
            RuntimeEvent::AgentStatusChanged { ref handle, status: RuntimeStatus::Working }
                if handle == "t9"
        ),
        "got {event:?}"
    );

    let event = stream
        .events
        .recv_timeout(Duration::from_secs(3))
        .expect("finished event");
    assert!(
        matches!(event, RuntimeEvent::AgentGone { ref handle } if handle == "t9"),
        "got {event:?}"
    );
}

#[test]
fn subscribe_events_created_event_maps_to_agent_seen() {
    let (_t, state, rt) = fixture();
    write_resp(
        &state,
        "events.subscribe",
        json!({"seq": 1, "terminals": []}),
    );
    let mut t = terminal_json("t4");
    t["status"]["state"] = json!("working");
    fs::write(
        state.join("event.json"),
        format!(
            "{}\n",
            json!({
                "jsonrpc": "2.0",
                "method": "event",
                "params": {
                    "type": "created",
                    "terminal": "t4",
                    "data": t,
                    "seq": 2
                }
            })
        ),
    )
    .unwrap();

    let stream = rt.subscribe_events(&[]).unwrap();
    assert!(stream.snapshot.is_empty());
    let event = stream
        .events
        .recv_timeout(Duration::from_secs(3))
        .expect("created event");
    match event {
        RuntimeEvent::AgentSeen { agent } => {
            assert_eq!(agent.handle, "t4");
            assert_eq!(agent.status, RuntimeStatus::Working);
        }
        other => panic!("got {other:?}"),
    }
}
