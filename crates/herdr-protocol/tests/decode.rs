//! Fixture-driven decode tests for response results, error envelopes, and
//! subscription/lifecycle frames. Fixtures live in `tests/fixtures/*.ndjson`.

use std::path::PathBuf;

use herdr_protocol::events::{SubscriptionEventData, SubscriptionFrame};
use herdr_protocol::results::{ResponseEnvelope, ResponseResult};
use herdr_protocol::{EventData, EventKind, HerdrError};

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn fixture_lines(name: &str) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(fixture_path(name))
        .unwrap_or_else(|e| panic!("failed to read fixture {name}: {e}"));
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("fixture line is not valid JSON"))
        .collect()
}

// ---------------------------------------------------------------------------
// Response results
// ---------------------------------------------------------------------------

#[test]
fn all_response_fixtures_decode() {
    let cases = fixture_lines("responses.ndjson");
    assert!(cases.len() > 40, "expected broad result coverage");
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        let result = &case["result"];
        let type_name = result["type"].as_str().unwrap();
        let decoded: ResponseResult = serde_json::from_value(result.clone())
            .unwrap_or_else(|e| panic!("{name}: failed to decode result: {e}"));
        assert_eq!(
            decoded.kind(),
            type_name,
            "{name}: kind() mismatch for decoded result"
        );
        if type_name == "quantum_flux_capacitor" {
            assert!(decoded.raw().is_some(), "{name}: expected raw payload");
        }
    }
}

#[test]
fn unknown_result_type_preserves_raw() {
    let result = serde_json::json!({
        "type": "future_widget",
        "nested": {"a": [1, 2, 3]},
        "extra": "keep me"
    });
    let decoded: ResponseResult = serde_json::from_value(result.clone()).unwrap();
    match &decoded {
        ResponseResult::Unknown { type_name, raw } => {
            assert_eq!(type_name, "future_widget");
            assert_eq!(raw, &result);
        }
        other => panic!("expected Unknown, got {:?}", other.kind()),
    }
    assert_eq!(decoded.kind(), "future_widget");
}

#[test]
fn result_without_type_is_unknown_empty() {
    let decoded: ResponseResult = serde_json::from_value(serde_json::json!({"data": 1})).unwrap();
    match &decoded {
        ResponseResult::Unknown { type_name, .. } => assert_eq!(type_name, ""),
        other => panic!("expected Unknown, got {:?}", other.kind()),
    }
}

#[test]
fn fixture_spot_checks() {
    let cases = fixture_lines("responses.ndjson");
    let get = |name: &str| -> serde_json::Value {
        cases
            .iter()
            .find(|c| c["name"] == name)
            .unwrap_or_else(|| panic!("missing fixture {name}"))["result"]
            .clone()
    };

    let pong: ResponseResult = serde_json::from_value(get("pong")).unwrap();
    match pong {
        ResponseResult::Pong(p) => {
            assert_eq!(p.version, "0.9.5");
            assert_eq!(p.protocol, 22);
        }
        other => panic!("expected pong, got {}", other.kind()),
    }

    let list: ResponseResult = serde_json::from_value(get("agent_list")).unwrap();
    match list {
        ResponseResult::AgentList(l) => {
            assert_eq!(l.agents.len(), 2);
            assert_eq!(l.agents[0].name.as_deref(), Some("reviewer"));
            assert_eq!(l.agents[0].agent_status.as_str(), "blocked");
        }
        other => panic!("expected agent_list, got {}", other.kind()),
    }

    let read: ResponseResult = serde_json::from_value(get("pane_read")).unwrap();
    match read {
        ResponseResult::PaneRead(r) => {
            assert_eq!(r.read.text, "hello world\n");
            assert!(!r.read.truncated);
        }
        other => panic!("expected pane_read, got {}", other.kind()),
    }

    // `agent.explain` returns a free-form payload.
    let explain: ResponseResult = serde_json::from_value(get("agent_explain")).unwrap();
    match explain {
        ResponseResult::AgentExplain(e) => {
            assert_eq!(e.explain["agent_status"], "blocked");
        }
        other => panic!("expected agent_explain, got {}", other.kind()),
    }

    // `events.wait` wraps an EventEnvelope in `result.event`.
    let wait: ResponseResult = serde_json::from_value(get("wait_matched")).unwrap();
    match wait {
        ResponseResult::WaitMatched(w) => {
            assert_eq!(w.event.event, EventKind::PaneAgentStatusChanged);
            match &w.event.data {
                EventData::PaneAgentStatusChanged { agent_status, .. } => {
                    assert_eq!(agent_status.as_str(), "idle");
                }
                other => panic!("unexpected event data: {other:?}"),
            }
        }
        other => panic!("expected wait_matched, got {}", other.kind()),
    }
}

// ---------------------------------------------------------------------------
// Error envelopes
// ---------------------------------------------------------------------------

#[test]
fn error_fixtures_decode_and_classify() {
    let cases = fixture_lines("errors.ndjson");
    assert!(cases.len() >= 10);
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        let code = case["code"].as_str().unwrap();
        let message = case["message"].as_str().unwrap();
        let envelope: ResponseEnvelope = serde_json::from_value(serde_json::json!({
            "id": "req-1",
            "error": {"code": code, "message": message},
        }))
        .unwrap();
        let err = envelope.error.expect("error body missing");
        assert_eq!(err.code, code, "{name}");
        assert_eq!(err.message, message, "{name}");

        // Exercise the HerdrError predicates the same way `transact` maps them.
        let api = HerdrError::Api {
            code: code.to_string(),
            message: message.to_string(),
        };
        match code {
            "agent_blocked" => assert!(api.is_agent_blocked(), "{name}"),
            "agent_not_found" => assert!(api.is_agent_not_found(), "{name}"),
            "agent_target_ambiguous" => assert!(api.is_agent_target_ambiguous(), "{name}"),
            "agent_not_ready" => assert!(api.is_agent_not_ready(), "{name}"),
            "timeout" => assert!(api.is_timeout(), "{name}"),
            _ => {}
        }
    }
}

#[test]
fn retryable_codes_classified() {
    // Mirrors the code list in `HerdrError::is_retryable`.
    for code in [
        "agent_blocked",
        "agent_not_ready",
        "agent_prompt_stalled",
        "agent_not_running",
        "timeout",
        "server_unavailable",
    ] {
        let err = HerdrError::Api {
            code: code.to_string(),
            message: String::new(),
        };
        assert!(err.is_retryable(), "{code} should be retryable");
    }
    for code in [
        "pane_not_found",
        "invalid_request",
        "invalid_params",
        "events_lost",
    ] {
        let err = HerdrError::Api {
            code: code.to_string(),
            message: String::new(),
        };
        assert!(!err.is_retryable(), "{code} should not be retryable");
    }
    assert!(HerdrError::Closed.is_retryable());
    assert!(HerdrError::Timeout.is_retryable());
}

#[test]
fn malformed_envelope_rejected() {
    // Not JSON at all.
    assert!(serde_json::from_str::<ResponseEnvelope>("not json").is_err());
    // Missing id.
    assert!(serde_json::from_str::<ResponseEnvelope>(r#"{"result":{"type":"pong"}}"#).is_err());
    // Neither result nor error decodes to a ResponseResult later, but the
    // envelope itself is structurally valid.
    let env: ResponseEnvelope =
        serde_json::from_str(r#"{"id":"x","result":{"type":"pong","version":"1","protocol":22}}"#)
            .unwrap();
    assert!(env.result.is_some() && env.error.is_none());
}

// ---------------------------------------------------------------------------
// Subscription / lifecycle frames
// ---------------------------------------------------------------------------

#[test]
fn subscription_frame_fixtures_decode() {
    let cases = fixture_lines("frames.ndjson");
    assert_eq!(cases.len(), 9);

    // 1. pane.output_matched
    let f = SubscriptionFrame::from_json_value(cases[0].clone()).unwrap();
    match f {
        SubscriptionFrame::SubscriptionEvent(env) => {
            assert_eq!(env.event_name, "pane.output_matched");
            match env.data {
                SubscriptionEventData::PaneOutputMatched(e) => {
                    assert_eq!(e.pane_id, "w1:p1");
                    assert_eq!(e.matched_line, "build finished in 12s");
                    assert_eq!(e.read.text, "compiling...\nbuild finished in 12s\n");
                }
                other => panic!("unexpected data: {other:?}"),
            }
        }
        other => panic!("expected subscription event, got {other:?}"),
    }

    // 2. pane.agent_status_changed (dot form, rich payload)
    let f = SubscriptionFrame::from_json_value(cases[1].clone()).unwrap();
    match f {
        SubscriptionFrame::SubscriptionEvent(env) => match env.data {
            SubscriptionEventData::PaneAgentStatusChanged(e) => {
                assert_eq!(e.agent_status.as_str(), "working");
                assert_eq!(e.agent.as_deref(), Some("claude"));
                assert_eq!(e.display_agent.as_deref(), Some("Claude"));
                assert_eq!(
                    e.state_labels.get("phase").map(String::as_str),
                    Some("edit")
                );
            }
            other => panic!("unexpected data: {other:?}"),
        },
        other => panic!("expected subscription event, got {other:?}"),
    }

    // 3. pane.scroll_changed
    let f = SubscriptionFrame::from_json_value(cases[2].clone()).unwrap();
    match f {
        SubscriptionFrame::SubscriptionEvent(env) => match env.data {
            SubscriptionEventData::ScrollChanged(e) => {
                assert_eq!(e.scroll.offset_from_bottom, 5);
                assert_eq!(e.scroll.max_offset_from_bottom, 40);
            }
            other => panic!("unexpected data: {other:?}"),
        },
        other => panic!("expected subscription event, got {other:?}"),
    }

    // 4-7. Lifecycle events (snake_case names).
    let expected_kinds = [
        EventKind::PaneAgentDetected,
        EventKind::PaneAgentStatusChanged,
        EventKind::PaneMoved,
        EventKind::WorkspaceCreated,
    ];
    for (i, kind) in expected_kinds.iter().enumerate() {
        let f = SubscriptionFrame::from_json_value(cases[3 + i].clone()).unwrap();
        match f {
            SubscriptionFrame::Event(env) => {
                assert_eq!(&env.event, kind, "frame {}", 3 + i);
                // Lifecycle payloads always carry a `type` discriminator,
                // preserved either way by EventData.
                let _ = &env.data;
            }
            other => panic!("frame {}: expected lifecycle event, got {other:?}", 3 + i),
        }
    }

    // 5 (index 4): pane_agent_status_changed lifecycle — check typed data.
    let f = SubscriptionFrame::from_json_value(cases[4].clone()).unwrap();
    match f {
        SubscriptionFrame::Event(env) => match env.data {
            EventData::PaneAgentStatusChanged { agent_status, .. } => {
                assert_eq!(agent_status.as_str(), "blocked");
            }
            other => panic!("unexpected data: {other:?}"),
        },
        other => panic!("expected lifecycle event, got {other:?}"),
    }

    // 8. Unknown dot-name subscription kind → Unknown payload, kind preserved.
    let f = SubscriptionFrame::from_json_value(cases[7].clone()).unwrap();
    match f {
        SubscriptionFrame::SubscriptionEvent(env) => {
            assert_eq!(env.event_name, "some.future_event");
            match env.data {
                SubscriptionEventData::Unknown { kind, raw } => {
                    assert_eq!(kind, "some.future_event");
                    assert_eq!(raw["experimental"], true);
                }
                other => panic!("unexpected data: {other:?}"),
            }
        }
        other => panic!("expected subscription event, got {other:?}"),
    }

    // 9. Error frame surfaces as Api error.
    let err = SubscriptionFrame::from_json_value(cases[8].clone()).unwrap_err();
    match err {
        HerdrError::Api { code, .. } => assert_eq!(code, "events_lost"),
        other => panic!("expected Api error, got {other:?}"),
    }
}

#[test]
fn frame_missing_event_field_is_protocol_error() {
    let err = SubscriptionFrame::from_json_value(serde_json::json!({"data": {}})).unwrap_err();
    assert!(matches!(err, HerdrError::Protocol(_)));
}

#[test]
fn frame_with_null_data_still_decodes() {
    // Defensive: `data` absent decodes as Null for unknown kinds.
    let f = SubscriptionFrame::from_json_value(serde_json::json!({
        "event": "brand.new_thing"
    }))
    .unwrap();
    match f {
        SubscriptionFrame::SubscriptionEvent(env) => {
            assert!(matches!(env.data, SubscriptionEventData::Unknown { .. }));
        }
        other => panic!("expected subscription event, got {other:?}"),
    }
}
