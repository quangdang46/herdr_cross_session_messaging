//! Canned default responses for the fake Herdr server.
//!
//! Payloads are loaded from `herdr-protocol`'s wire fixtures
//! (`tests/fixtures/responses.ndjson`) so the fake server always speaks the
//! exact shapes real clients decode. Each fixture line is
//! `{"name": ..., "result": {...}}`; we keep the `result` object, which is
//! what goes inside the response envelope's `"result"` field.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::OnceLock;

use crate::Responder;

const RESPONSES_FIXTURE: &str =
    include_str!("../../herdr-protocol/tests/fixtures/responses.ndjson");

fn fixture_results() -> &'static HashMap<String, Value> {
    static MAP: OnceLock<HashMap<String, Value>> = OnceLock::new();
    MAP.get_or_init(|| {
        RESPONSES_FIXTURE
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|value| {
                let name = value.get("name")?.as_str()?.to_string();
                let result = value.get("result")?.clone();
                Some((name, result))
            })
            .collect()
    })
}

/// Look up a canned `result` payload by fixture name (e.g. `"agent_list"`,
/// `"pane_read"`, `"wait_matched"`). Returns `Value::Null` for unknown names.
pub fn canned_result(name: &str) -> Value {
    fixture_results().get(name).cloned().unwrap_or(Value::Null)
}

fn canned(name: &str) -> Value {
    canned_result(name)
}

/// Default responder for a method when the test has not scripted one.
///
/// Long-lived methods (`events.subscribe`, `events.wait`, `agent.wait`,
/// `agent.prompt` carrying a `wait` param, `pane.wait_for_output`) hold the
/// connection open so tests can push responses/frames later.
pub(crate) fn default_for(method: &str, params: &Value) -> Responder {
    match method {
        "ping" => Responder::Result(canned("pong")),
        "session.snapshot" => Responder::Result(canned("session_snapshot")),
        "server.stop" | "server.reload_config" | "server.reload_agent_manifests" => {
            Responder::Result(canned("ok"))
        }
        "agent.list" => Responder::Result(canned("agent_list")),
        "agent.get" | "agent.focus" | "agent.rename" => Responder::Result(canned("agent_info")),
        "agent.explain" => Responder::Result(canned("agent_explain")),
        "agent.start" => Responder::Result(canned("agent_started")),
        "agent.read" | "pane.read" => Responder::Result(canned("pane_read")),
        // `agent.prompt` without `wait` resolves immediately; with `wait` the
        // real server holds the connection until the agent reaches a status.
        "agent.prompt" => {
            if params.get("wait").is_some() {
                Responder::Hold
            } else {
                Responder::Result(canned("agent_prompted"))
            }
        }
        "agent.wait" => Responder::Hold,
        "agent.send_keys" => Responder::Result(canned("ok")),
        "agent.view.set" | "agent.view.clear" => Responder::Result(canned("agent_view")),
        "pane.list" => Responder::Result(canned("pane_list")),
        "pane.current" => Responder::Result(canned("pane_current")),
        "pane.get" | "pane.rename" => Responder::Result(canned("pane_info")),
        "pane.process_info" => Responder::Result(canned("pane_process_info")),
        "pane.send_text"
        | "pane.send_keys"
        | "pane.send_input"
        | "pane.close"
        | "pane.report_agent"
        | "pane.report_agent_session"
        | "pane.report_metadata"
        | "pane.clear_agent_authority"
        | "pane.release_agent" => Responder::Result(canned("ok")),
        "pane.wait_for_output" => Responder::Hold,
        "events.subscribe" => Responder::RespondAndHold(canned("subscription_started")),
        "events.wait" => Responder::Hold,
        "notification.show" => Responder::Result(canned("notification_show")),
        "workspace.list" => Responder::Result(canned("workspace_list")),
        "workspace.get" | "workspace.focus" | "workspace.rename" => {
            Responder::Result(canned("workspace_info"))
        }
        "integration.list" => Responder::Result(canned("integration_list")),
        "integration.install" => Responder::Result(canned("integration_install")),
        "integration.uninstall" => Responder::Result(canned("integration_uninstall")),
        _ => Responder::Error {
            code: "invalid_request".to_string(),
            message: format!("invalid request: unknown method {method:?}"),
        },
    }
}
