//! End-to-end-ish unit tests for the pool commands against `FakeHerdr`.
//!
//! Each test scripts the socket responses a real Herdr server would send
//! (`pane.current`, `agent.get`, `agent.list`, `agent.prompt`), runs the
//! command against a temp state dir, then asserts on the store and the
//! recorded `agent.prompt` payloads.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::ffi::OsString;

use herdr_pool_core::open_pool;
use serde_json::json;

use crate::commands::{inbox, peers, reply, send, threads, whoami};
use crate::context::{env_from_map, Failure};
use crate::testutil::{agent_value, Fixture};

/// Run `whoami`-style commands need `&EnvLookup`; build map + closure at the
/// call site to keep lifetimes simple.
macro_rules! with_env {
    ($f:expr, $pane:expr, $body:expr) => {{
        let map = $f.env_map($pane);
        let env = env_from_map(&map);
        $body(&env)
    }};
}

fn peer_agent_id(pool: &herdr_pool_core::PoolContext) -> String {
    pool.pool
        .registry()
        .list_agents(false)
        .unwrap()
        .into_iter()
        .find(|a| a.name.as_deref() == Some("peer"))
        .unwrap()
        .agent_id
}

fn self_agent_id(pool: &herdr_pool_core::PoolContext) -> String {
    pool.pool
        .registry()
        .list_agents(false)
        .unwrap()
        .into_iter()
        .find(|a| a.name.as_deref() == Some("self"))
        .unwrap()
        .agent_id
}

// ---------------------------------------------------------------------------
// whoami
// ---------------------------------------------------------------------------

#[test]
fn whoami_registers_self_and_reports_identity() {
    let f = Fixture::new();
    let cwd = f.project_cwd();
    f.script_self("w1:p1", "term_self", &cwd);
    f.script_agent_list(vec![agent_value(
        "w1:p1",
        "term_self",
        "claude",
        Some("self"),
        &cwd,
        "idle",
    )]);

    let ctx = f.ctx();
    with_env!(f, "w1:p1", |env| whoami::run(&ctx, env)).unwrap();

    // Self is now in the registry keyed by terminal_id.
    let pool = open_pool(f.state.path(), f.project.path()).unwrap();
    let record = pool
        .pool
        .registry()
        .get_by_terminal("term_self")
        .unwrap()
        .expect("self record");
    assert!(record.agent_id.starts_with("ag_"));
    assert_eq!(record.agent_kind.as_deref(), Some("claude"));
    assert_eq!(record.name.as_deref(), Some("self"));
    assert_eq!(record.scope_key, pool.scope_key.as_str());
}

#[test]
fn whoami_without_herdr_env_is_usage_error() {
    let f = Fixture::new();
    let ctx = f.ctx();
    let map = HashMap::<String, OsString>::new();
    let env = env_from_map(&map);
    let err = whoami::run(&ctx, &env).unwrap_err();
    assert_eq!(err.code, 2);
    assert_eq!(err.kind, "usage");
}

// ---------------------------------------------------------------------------
// peers
// ---------------------------------------------------------------------------

#[test]
fn peers_lists_scope_agents_excluding_self() {
    let f = Fixture::new();
    f.script_two_agents();

    let ctx = f.ctx();
    with_env!(f, "w1:p1", |env| peers::run(&ctx, env)).unwrap();

    let pool = open_pool(f.state.path(), f.project.path()).unwrap();
    let agents = pool.pool.registry().list_agents(false).unwrap();
    assert_eq!(agents.len(), 2);
    assert!(agents.iter().any(|a| a.name.as_deref() == Some("peer")));
    assert!(agents.iter().any(|a| a.name.as_deref() == Some("self")));
}

#[test]
fn peers_never_crosses_scopes() {
    let f = Fixture::new();
    let cwd_a = f.project_cwd();
    // A second project dir = second scope.
    let other = tempfile::tempdir().unwrap();
    let cwd_b = std::fs::canonicalize(other.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();

    // Self lives in project B; agent.list reports agents in BOTH scopes.
    f.script_self("w2:p1", "term_b", &cwd_b);
    f.script_agent_list(vec![
        agent_value("w1:p1", "term_a", "claude", Some("alice"), &cwd_a, "idle"),
        agent_value("w2:p1", "term_b", "codex", Some("self"), &cwd_b, "idle"),
        agent_value("w2:p2", "term_c", "claude", Some("carol"), &cwd_b, "idle"),
    ]);

    let ctx = f.ctx();
    // env for pane w2:p1 → workspace w2.
    let map = [
        ("HERDR_PANE_ID", "w2:p1"),
        ("HERDR_WORKSPACE_ID", "w2"),
        ("HERDR_TAB_ID", "w2:t1"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), OsString::from(v)))
    .collect::<HashMap<_, _>>();
    let env = env_from_map(&map);
    peers::run(&ctx, &env).unwrap();

    // Scope B pool contains self + carol only — alice (scope A) excluded.
    let pool_b = open_pool(f.state.path(), other.path()).unwrap();
    let names: Vec<_> = pool_b
        .pool
        .registry()
        .list_agents(false)
        .unwrap()
        .iter()
        .filter_map(|a| a.name.clone())
        .collect();
    assert!(names.contains(&"self".to_string()));
    assert!(names.contains(&"carol".to_string()));
    assert!(!names.contains(&"alice".to_string()));

    // And scope A's pool was never created by this command.
    let pool_a = open_pool(f.state.path(), f.project.path()).unwrap();
    assert!(pool_a
        .pool
        .registry()
        .list_agents(false)
        .unwrap()
        .is_empty());
}

// ---------------------------------------------------------------------------
// send
// ---------------------------------------------------------------------------

#[test]
fn send_delivers_envelope_and_marks_delivered() {
    let f = Fixture::new();
    f.script_two_agents();
    let ctx = f.ctx();

    with_env!(f, "w1:p1", |env| send::run(
        &ctx,
        env,
        "peer",
        "please review",
        None
    ))
    .unwrap();

    // Exactly one agent.prompt went out, addressed at the peer's pane_id
    // (agent.prompt rejects terminal_id targets).
    let prompts = f.server.requests_for("agent.prompt");
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].params["target"], json!("w1:p2"));
    let text = prompts[0].params["text"].as_str().unwrap();
    assert!(
        text.starts_with("[herdr-pool] msg msg_"),
        "envelope prefix: {text}"
    );
    assert!(text.contains(" from claude (ag_"), "sender label: {text}");
    assert!(text.contains(" thread th_"), "thread id: {text}");
    assert!(text.contains("please review"), "content: {text}");
    assert!(
        text.contains("Reply: herdr-pool reply msg_"),
        "reply hint: {text}"
    );

    // Message is delivered in the store.
    let pool = open_pool(f.state.path(), f.project.path()).unwrap();
    let peer_id = peer_agent_id(&pool);
    let inbox = pool.pool.messages().inbox(&peer_id, false).unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].status, herdr_pool_store::MessageStatus::Delivered);
    assert!(inbox[0].delivered_at.is_some());
}

#[test]
fn send_blocked_agent_leaves_retry_scheduled() {
    let f = Fixture::new();
    f.script_two_agents();
    f.server
        .fail_next("agent.prompt", "agent_blocked", "agent is blocked");
    let ctx = f.ctx();

    with_env!(f, "w1:p1", |env| send::run(
        &ctx, env, "peer", "hello?", None
    ))
    .unwrap();

    let pool = open_pool(f.state.path(), f.project.path()).unwrap();
    let peer_id = peer_agent_id(&pool);
    // Deferred (still `pending`) rows ARE visible in the inbox — the
    // recipient can read mail the daemon has not prompted yet.
    let inbox = pool.pool.messages().inbox(&peer_id, false).unwrap();
    assert_eq!(inbox.len(), 1);
    // Store records the defer schedule for the daemon without consuming
    // a delivery attempt.
    assert_eq!(inbox[0].status, herdr_pool_store::MessageStatus::Pending);
    assert!(inbox[0].next_retry_at.is_some());
    assert_eq!(inbox[0].attempt_count, 0);
    assert_eq!(inbox[0].to_agent, peer_id);
}

#[test]
fn send_unknown_peer_is_usage_error() {
    let f = Fixture::new();
    f.script_two_agents();
    let ctx = f.ctx();

    let err = with_env!(f, "w1:p1", |env| {
        send::run(&ctx, env, "nobody", "hi", None)
    })
    .unwrap_err();
    assert_eq!(err.code, 2);
}

#[test]
fn send_cross_scope_peer_not_found() {
    let f = Fixture::new();
    // Self in project; peer agent lives in a DIFFERENT cwd → different scope.
    let cwd = f.project_cwd();
    let other = tempfile::tempdir().unwrap();
    let cwd_b = std::fs::canonicalize(other.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    f.script_self("w1:p1", "term_self", &cwd);
    f.script_agent_list(vec![
        agent_value("w1:p1", "term_self", "claude", Some("self"), &cwd, "idle"),
        agent_value("w1:p2", "term_peer", "codex", Some("peer"), &cwd_b, "idle"),
    ]);

    let ctx = f.ctx();
    let err = with_env!(f, "w1:p1", |env| send::run(&ctx, env, "peer", "hi", None)).unwrap_err();
    assert_eq!(err.code, 2);
    assert!(f.server.requests_for("agent.prompt").is_empty());
}

// ---------------------------------------------------------------------------
// inbox + message read
// ---------------------------------------------------------------------------

#[test]
fn inbox_lists_and_read_acknowledges() {
    let f = Fixture::new();
    f.script_two_agents();
    let ctx = f.ctx();

    // Deliver a message peer→self directly through the store.
    with_env!(f, "w1:p1", |env| {
        // First populate the registry via peers.
        peers::run(&ctx, env)?;
        let pool = open_pool(f.state.path(), f.project.path()).unwrap();
        let self_id = self_agent_id(&pool);
        let peer_id = peer_agent_id(&pool);
        let msg = pool
            .pool
            .messages()
            .send(&peer_id, &self_id, "inbound hello")
            .unwrap();
        // Simulate delivery so ack is legal.
        pool.pool.messages().mark_delivered(&msg.id).unwrap();
        Ok::<_, Failure>(msg.id)
    })
    .unwrap();

    // unread inbox shows it
    let unread_count = with_env!(f, "w1:p1", |env| { inbox::run(&ctx, env, true, None) });
    unread_count.unwrap();

    // find the message id and read it
    let pool = open_pool(f.state.path(), f.project.path()).unwrap();
    let self_id = self_agent_id(&pool);
    let msgs = pool.pool.messages().inbox(&self_id, true).unwrap();
    assert_eq!(msgs.len(), 1);
    let msg_id = msgs[0].id.clone();

    with_env!(f, "w1:p1", |env| inbox::run(
        &ctx,
        env,
        false,
        Some(&crate::cli::InboxAction::Read { id: msg_id.clone() })
    ))
    .unwrap();

    let after = pool.pool.messages().message(&msg_id).unwrap().unwrap();
    assert_eq!(after.status, herdr_pool_store::MessageStatus::Acknowledged);
    assert!(after.acked_at.is_some());
}

// ---------------------------------------------------------------------------
// reply + threads
// ---------------------------------------------------------------------------

#[test]
fn reply_lands_in_same_thread_to_original_sender() {
    let f = Fixture::new();
    f.script_two_agents();
    let ctx = f.ctx();

    // peer→self message via store
    let msg_id = with_env!(f, "w1:p1", |env| {
        peers::run(&ctx, env)?;
        let pool = open_pool(f.state.path(), f.project.path()).unwrap();
        let self_id = self_agent_id(&pool);
        let peer_id = peer_agent_id(&pool);
        let msg = pool
            .pool
            .messages()
            .send(&peer_id, &self_id, "question for you")
            .unwrap();
        pool.pool.messages().mark_delivered(&msg.id).unwrap();
        Ok::<_, Failure>(msg.id)
    })
    .unwrap();

    with_env!(f, "w1:p1", |env| reply::run(
        &ctx,
        env,
        &msg_id,
        "here is the answer"
    ))
    .unwrap();

    let prompts = f.server.requests_for("agent.prompt");
    assert_eq!(prompts.len(), 1);
    let text = prompts[0].params["text"].as_str().unwrap();
    assert!(text.contains("thread th_"));

    let pool = open_pool(f.state.path(), f.project.path()).unwrap();
    let peer_id = peer_agent_id(&pool);
    let inbox = pool.pool.messages().inbox(&peer_id, false).unwrap();
    let reply_msg = inbox
        .iter()
        .find(|m| m.kind == "reply")
        .expect("reply message");
    // Same thread as the original.
    let original = pool.pool.messages().message(&msg_id).unwrap().unwrap();
    assert_eq!(reply_msg.thread_id, original.thread_id);
    assert_eq!(reply_msg.status, herdr_pool_store::MessageStatus::Delivered);
}

#[test]
fn threads_summarizes_conversations() {
    let f = Fixture::new();
    f.script_two_agents();
    let ctx = f.ctx();

    with_env!(f, "w1:p1", |env| send::run(
        &ctx, env, "peer", "first", None
    ))
    .unwrap();
    with_env!(f, "w1:p1", |env| threads::run(&ctx, env)).unwrap();

    let pool = open_pool(f.state.path(), f.project.path()).unwrap();
    let threads = pool.pool.messages().threads().unwrap();
    assert_eq!(threads.len(), 1);
}
