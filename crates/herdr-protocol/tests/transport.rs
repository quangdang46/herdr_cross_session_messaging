//! End-to-end transport tests against a fake Herdr server speaking NDJSON
//! over a Unix domain socket in a tempdir.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use herdr_protocol::events::{Subscription, SubscriptionEventData, SubscriptionFrame};
use herdr_protocol::types::{AgentStatus, ReadSource};
use herdr_protocol::{EventMatch, HerdrClient, HerdrError, OutputMatch};

/// Spawn a fake server. `handler` maps each request line to the response
/// lines to write back; the connection is closed after the responses.
/// Returns the socket path and a shared log of received request lines.
fn spawn_server(
    handler: impl Fn(&str) -> Vec<String> + Send + Sync + 'static,
) -> (PathBuf, Arc<Mutex<Vec<String>>>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("herdr.sock");
    // Leak the tempdir for the life of the process — tests are short-lived.
    let path: &'static PathBuf = Box::leak(Box::new(path));
    let _dir: &'static tempfile::TempDir = Box::leak(Box::new(dir));
    let listener = UnixListener::bind(path).expect("bind fake socket");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests2 = Arc::clone(&requests);
    let handler = Arc::new(handler);
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(stream) = conn else { break };
            let requests = Arc::clone(&requests2);
            let handler = Arc::clone(&handler);
            std::thread::spawn(move || {
                let mut reader = BufReader::new(&stream);
                let mut line = String::new();
                // The real server handles one request per connection; closing
                // after the response also gives SubscriptionStream its EOF.
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                let trimmed = line.trim_end().to_string();
                if trimmed.is_empty() {
                    return;
                }
                requests.lock().unwrap().push(trimmed.clone());
                for response in handler(&trimmed) {
                    let mut stream = stream.try_clone().expect("clone stream");
                    if stream
                        .write_all(response.as_bytes())
                        .and_then(|()| stream.write_all(b"\n"))
                        .and_then(|()| stream.flush())
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
    });
    (path.clone(), requests)
}

/// Extract `id` from a request line so canned responses can echo it.
fn request_id(line: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(line).expect("request is JSON");
    v["id"].as_str().expect("request has id").to_string()
}

fn pong_response(line: &str) -> String {
    let id = request_id(line);
    format!(
        r#"{{"id":"{id}","result":{{"type":"pong","version":"0.9.5","protocol":22,"capabilities":{{"live_handoff":true,"detached_server_daemon":false,"health_check":true,"ssh_agent_registration":true,"surface_interest":false,"endpoint_protocol_generation":1}}}}}}"#
    )
}

fn client_for(path: &PathBuf) -> HerdrClient {
    HerdrClient::from_socket_path(path).with_request_timeout(std::time::Duration::from_secs(5))
}

#[test]
fn ping_roundtrip() {
    let (path, requests) = spawn_server(|line| vec![pong_response(line)]);
    let client = client_for(&path);
    let pong = client.ping().expect("ping failed");
    assert_eq!(pong.version, "0.9.5");
    assert_eq!(pong.protocol, 22);

    let sent = requests.lock().unwrap();
    assert_eq!(sent.len(), 1);
    let req: serde_json::Value = serde_json::from_str(&sent[0]).unwrap();
    assert_eq!(req["method"], "ping");
    assert_eq!(req["params"], serde_json::json!({}));
    // Request ids are UUIDv7.
    assert!(uuid::Uuid::parse_str(req["id"].as_str().unwrap()).is_ok());
}

#[test]
fn agent_list_roundtrip() {
    let (path, requests) = spawn_server(|line| {
        let id = request_id(line);
        vec![format!(
            r#"{{"id":"{id}","result":{{"type":"agent_list","agents":[{{"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1","terminal_id":"t1","agent_status":"idle","revision":1,"focused":true}}]}}}}"#
        )]
    });
    let agents = client_for(&path).agent_list().expect("agent_list failed");
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].pane_id, "w1:p1");
    let sent = requests.lock().unwrap();
    let req: serde_json::Value = serde_json::from_str(&sent[0]).unwrap();
    assert_eq!(req["method"], "agent.list");
}

#[test]
fn api_error_envelope_maps_to_typed_error() {
    let (path, _) = spawn_server(|line| {
        let id = request_id(line);
        vec![format!(
            r#"{{"id":"{id}","error":{{"code":"agent_blocked","message":"agent is blocked"}}}}"#
        )]
    });
    let err = client_for(&path)
        .agent_prompt("reviewer", "hi", None)
        .expect_err("expected agent_blocked");
    assert!(err.is_agent_blocked(), "got {err:?}");
    match err {
        HerdrError::Api { code, message } => {
            assert_eq!(code, "agent_blocked");
            assert_eq!(message, "agent is blocked");
        }
        other => panic!("expected Api error, got {other:?}"),
    }
}

#[test]
fn events_subscribe_streams_frames_then_closed() {
    let (path, requests) = spawn_server(|line| {
        let id = request_id(line);
        let frames = [
            r#"{"event":"pane.output_matched","data":{"pane_id":"w1:p1","matched_line":"ok","read":{"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1","source":"recent_unwrapped","format":"text","text":"ok\n","revision":1,"truncated":false}}}"#,
            r#"{"event":"pane_agent_detected","data":{"type":"pane_agent_detected","pane_id":"w1:p1","workspace_id":"w1","agent":"claude","released":false}}"#,
        ];
        let mut out = vec![format!(
            r#"{{"id":"{id}","result":{{"type":"subscription_started"}}}}"#
        )];
        out.extend(frames.iter().map(|f| f.to_string()));
        out
    });
    let client = client_for(&path);
    let mut stream = client
        .events_subscribe(vec![
            Subscription::PaneAgentDetected,
            Subscription::PaneAgentStatusChanged {
                pane_id: "w1:p1".into(),
                agent_status: Some(AgentStatus::Working),
            },
        ])
        .expect("subscribe failed");

    match stream.next_frame().expect("frame 1") {
        SubscriptionFrame::SubscriptionEvent(env) => {
            assert_eq!(env.event_name, "pane.output_matched");
            assert!(matches!(
                env.data,
                SubscriptionEventData::PaneOutputMatched(_)
            ));
        }
        other => panic!("expected subscription event, got {other:?}"),
    }
    match stream.next_frame().expect("frame 2") {
        SubscriptionFrame::Event(env) => {
            assert_eq!(env.event.dot_name(), Some("pane.agent_detected"));
        }
        other => panic!("expected lifecycle event, got {other:?}"),
    }
    // Server closed the connection after the frames.
    assert!(matches!(stream.next_frame(), Err(HerdrError::Closed)));

    // Verify the subscribe request shape on the wire.
    let sent = requests.lock().unwrap();
    let req: serde_json::Value = serde_json::from_str(&sent[0]).unwrap();
    assert_eq!(req["method"], "events.subscribe");
    assert_eq!(
        req["params"]["subscriptions"][0]["type"],
        "pane.agent_detected"
    );
    assert_eq!(
        req["params"]["subscriptions"][1]["type"],
        "pane.agent_status_changed"
    );
}

#[test]
fn events_wait_returns_matched_envelope() {
    let (path, _) = spawn_server(|line| {
        let id = request_id(line);
        vec![format!(
            r#"{{"id":"{id}","result":{{"type":"wait_matched","event":{{"event":"pane_agent_status_changed","data":{{"type":"pane_agent_status_changed","pane_id":"w1:p1","workspace_id":"w1","agent_status":"idle"}}}}}}}}"#
        )]
    });
    let env = client_for(&path)
        .events_wait(
            EventMatch::PaneAgentStatusChanged {
                pane_id: "w1:p1".into(),
                agent_status: AgentStatus::Idle,
            },
            Some(5_000),
        )
        .expect("events_wait failed");
    assert_eq!(env.event.dot_name(), Some("pane.agent_status_changed"));
}

#[test]
fn silent_disconnect_surfaces_closed() {
    // Handler returns no lines: server accepts then closes immediately.
    let (path, _) = spawn_server(|_line| Vec::new());
    let err = client_for(&path).ping().expect_err("expected Closed");
    assert!(matches!(err, HerdrError::Closed), "got {err:?}");
}

#[test]
fn malformed_response_is_protocol_error() {
    let (path, _) = spawn_server(|line| {
        let id = request_id(line);
        vec![format!(r#"{{"id":"{id}","result":{{"type":"pong"}}"#)] // truncated JSON
    });
    let err = client_for(&path).ping().expect_err("expected Protocol");
    assert!(matches!(err, HerdrError::Protocol(_)), "got {err:?}");
}

#[test]
fn id_mismatch_is_protocol_error() {
    let (path, _) = spawn_server(|_line| {
        vec![
            r#"{"id":"not-the-request","result":{"type":"pong","version":"1","protocol":22}}"#
                .to_string(),
        ]
    });
    let err = client_for(&path).ping().expect_err("expected Protocol");
    assert!(matches!(err, HerdrError::Protocol(_)), "got {err:?}");
}

#[test]
fn oversized_response_line_is_protocol_error() {
    let (path, _) = spawn_server(|line| {
        let id = request_id(line);
        // MAX_LINE_BYTES is 8 MiB; send 8 MiB + 1 of padding.
        let padding = "x".repeat(herdr_protocol::MAX_LINE_BYTES + 1);
        vec![format!(
            r#"{{"id":"{id}","result":{{"type":"pane_read","read":{{"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1","source":"recent","format":"text","text":"{padding}","revision":1,"truncated":true}}}}}}"#
        )]
    });
    let err = client_for(&path)
        .pane_read(&herdr_protocol::PaneReadParams {
            pane_id: "w1:p1".into(),
            source: ReadSource::Recent,
            options: Default::default(),
        })
        .expect_err("expected Protocol");
    assert!(matches!(err, HerdrError::Protocol(_)), "got {err:?}");
}

#[test]
fn empty_response_line_is_protocol_error() {
    let (path, _) = spawn_server(|line| {
        let id = request_id(line);
        // A bare newline is written as an empty response line.
        let _ = id;
        vec![String::new()]
    });
    let err = client_for(&path).ping().expect_err("expected Protocol");
    assert!(matches!(err, HerdrError::Protocol(_)), "got {err:?}");
}

#[test]
fn missing_socket_is_transport_error() {
    let client = HerdrClient::from_socket_path("/nonexistent/herdr.sock");
    let err = client.ping().expect_err("expected Transport");
    assert!(matches!(err, HerdrError::Transport(_)), "got {err:?}");
}

#[test]
fn request_params_shape_on_wire() {
    // Golden check that `call` wraps params under the `params` key.
    let (path, requests) = spawn_server(|line| {
        let id = request_id(line);
        vec![format!(
            r#"{{"id":"{id}","result":{{"type":"output_matched","pane_id":"w1:p1","revision":9,"read":{{"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1","source":"recent_unwrapped","format":"text","text":"done\n","revision":9,"truncated":false}},"matched_line":"done"}}}}"#
        )]
    });
    let result = client_for(&path)
        .pane_wait_for_output(&herdr_protocol::PaneWaitForOutputParams {
            pane_id: "w1:p1".into(),
            source: ReadSource::Recent,
            r#match: OutputMatch::Substring {
                value: "done".into(),
            },
            lines: None,
            strip_ansi: true,
            timeout_ms: Some(1_000),
        })
        .expect("wait_for_output failed");
    assert_eq!(result.matched_line.as_deref(), Some("done"));

    let sent = requests.lock().unwrap();
    let req: serde_json::Value = serde_json::from_str(&sent[0]).unwrap();
    assert_eq!(req["method"], "pane.wait_for_output");
    assert_eq!(
        req["params"],
        serde_json::json!({
            "pane_id": "w1:p1",
            "source": "recent",
            "match": {"type": "substring", "value": "done"},
            "timeout_ms": 1000
        })
    );
}
