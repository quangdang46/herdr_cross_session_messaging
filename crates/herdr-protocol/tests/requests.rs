//! Golden request-params fixtures: each `tests/fixtures/requests.ndjson` entry
//! pins the exact JSON `params` object a typed params struct serializes to.

use std::path::PathBuf;

use herdr_protocol::params::*;
use herdr_protocol::types::*;
use herdr_protocol::{EventMatch, OutputMatch, Subscription};
use serde::Serialize;

fn fixture() -> Vec<serde_json::Value> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("requests.ndjson");
    let text = std::fs::read_to_string(path).expect("read requests fixture");
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("fixture line is not valid JSON"))
        .collect()
}

fn assert_params(name: &str, params: &impl Serialize, expected: &serde_json::Value) {
    let actual = serde_json::to_value(params).expect("params must serialize");
    assert_eq!(
        &actual, expected,
        "{name}: serialized params do not match fixture"
    );
}

#[test]
fn request_fixtures_match_typed_params() {
    let cases = fixture();
    assert!(cases.len() >= 20);
    for case in &cases {
        let name = case["name"].as_str().unwrap().to_string();
        let method = case["method"].as_str().unwrap();
        let expected = &case["params"];

        match name.as_str() {
            "ping" => assert_params(&name, &EmptyParams {}, expected),
            "agent_prompt" => assert_params(
                &name,
                &AgentPromptParams {
                    target: "reviewer".into(),
                    text: "please rebase".into(),
                    wait: Some(AgentPromptWait {
                        until: Some(vec![AgentStatus::Idle, AgentStatus::Done]),
                        timeout_ms: Some(30_000),
                    }),
                },
                expected,
            ),
            "agent_prompt_no_wait" => assert_params(
                &name,
                &AgentPromptParams {
                    target: "w1:p1".into(),
                    text: "hi".into(),
                    wait: None,
                },
                expected,
            ),
            "agent_wait" => assert_params(
                &name,
                &AgentWaitParams {
                    target: "builder".into(),
                    until: Some(vec![AgentStatus::Idle]),
                    timeout_ms: Some(60_000),
                },
                expected,
            ),
            "agent_read" => assert_params(
                &name,
                &AgentReadParams {
                    target: "reviewer".into(),
                    source: ReadSource::Recent,
                    options: ReadOptions {
                        format: Some(ReadFormat::Text),
                        lines: Some(200),
                        strip_ansi: true,
                    },
                },
                expected,
            ),
            "agent_read_no_strip" => assert_params(
                &name,
                &AgentReadParams {
                    target: "reviewer".into(),
                    source: ReadSource::Visible,
                    options: ReadOptions {
                        format: None,
                        lines: None,
                        strip_ansi: false,
                    },
                },
                expected,
            ),
            "agent_send_keys" => assert_params(
                &name,
                &SendKeysParams {
                    target: "reviewer".into(),
                    keys: vec!["enter".into()],
                },
                expected,
            ),
            "agent_start" => assert_params(
                &name,
                &AgentStartParams {
                    name: "reviewer".into(),
                    kind: "claude".into(),
                    pane_id: "w1:p1".into(),
                    args: Some(vec!["--flag".into()]),
                    timeout_ms: Some(30_000),
                },
                expected,
            ),
            "agent_rename" => assert_params(
                &name,
                &AgentRenameParams {
                    target: "reviewer".into(),
                    name: Some("reviewer-2".into()),
                },
                expected,
            ),
            "pane_read" => assert_params(
                &name,
                &PaneReadParams {
                    pane_id: "w1:p1".into(),
                    source: ReadSource::Recent,
                    options: ReadOptions {
                        format: None,
                        lines: Some(50),
                        strip_ansi: true,
                    },
                },
                expected,
            ),
            "pane_send_text" => assert_params(
                &name,
                &PaneSendTextParams {
                    pane_id: "w1:p1".into(),
                    text: "ls -la\n".into(),
                },
                expected,
            ),
            "pane_send_keys" => assert_params(
                &name,
                &PaneSendKeysParams {
                    pane_id: "w1:p1".into(),
                    keys: vec!["ctrl+c".into()],
                },
                expected,
            ),
            "pane_wait_for_output" => assert_params(
                &name,
                &PaneWaitForOutputParams {
                    pane_id: "w1:p1".into(),
                    source: ReadSource::Recent,
                    r#match: OutputMatch::Substring {
                        value: "done".into(),
                    },
                    lines: None,
                    strip_ansi: true,
                    timeout_ms: Some(10_000),
                },
                expected,
            ),
            "events_subscribe" => assert_params(
                &name,
                &EventsSubscribeParams {
                    subscriptions: vec![
                        Subscription::PaneAgentDetected,
                        Subscription::PaneOutputMatched {
                            pane_id: "w1:p1".into(),
                            source: ReadSource::Recent,
                            r#match: OutputMatch::Regex {
                                value: "build.*".into(),
                            },
                            lines: Some(100),
                            strip_ansi: true,
                        },
                        Subscription::PaneAgentStatusChanged {
                            pane_id: "w1:p1".into(),
                            agent_status: Some(AgentStatus::Working),
                        },
                        Subscription::WorkspaceCreated,
                    ],
                },
                expected,
            ),
            "events_wait" => assert_params(
                &name,
                &EventsWaitParams {
                    match_event: EventMatch::PaneAgentStatusChanged {
                        pane_id: "w1:p1".into(),
                        agent_status: AgentStatus::Idle,
                    },
                    timeout_ms: Some(5_000),
                },
                expected,
            ),
            "notification_show" => assert_params(
                &name,
                &NotificationShowParams {
                    title: "Build done".into(),
                    body: Some("in 12s".into()),
                    position: Some(ToastPosition::BottomRight),
                    sound: Some(NotificationShowSound::Done),
                },
                expected,
            ),
            "layout_set_split_ratio" => assert_params(
                &name,
                &LayoutSetSplitRatioParams {
                    path: vec![true, false],
                    ratio: 0.3,
                    pane_id: Some("w1:p1".into()),
                    tab_id: None,
                },
                expected,
            ),
            "worktree_remove" => assert_params(
                &name,
                &WorktreeRemoveParams {
                    workspace_id: "w3".into(),
                    force: true,
                    trust_repository: true,
                },
                expected,
            ),
            "tab_move" => assert_params(
                &name,
                &TabMoveParams {
                    tab_id: "w1:t1".into(),
                    insert_index: 0,
                },
                expected,
            ),
            "workspace_close" => assert_params(
                &name,
                &WorkspaceCloseParams {
                    workspace_id: "w1".into(),
                    close_group: false,
                },
                expected,
            ),
            "pane_report_agent" => assert_params(
                &name,
                &PaneReportAgentParams {
                    pane_id: "w1:p1".into(),
                    source: "plugin".into(),
                    agent: "custom".into(),
                    state: PaneAgentState::Working,
                    agent_session_id: None,
                    agent_session_path: None,
                    resume_argv: None,
                    message: None,
                    seq: Some(12),
                },
                expected,
            ),
            other => panic!("no typed params coverage for fixture {other} ({method})"),
        }
    }
}
