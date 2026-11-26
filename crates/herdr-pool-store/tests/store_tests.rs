//! Integration tests for herdr-pool-store: migrations, WAL concurrency,
//! message status transitions, inbox scoping, registry semantics, and
//! outbox/backoff behavior. All tests run against tempdir databases.

use std::path::{Path, PathBuf};

use herdr_pool_store::{
    new_agent_id, AgentRecord, MessageStatus, PoolStore, ScopeIndex, StoreError,
    MAX_DELIVERY_ATTEMPTS,
};
use rusqlite::Connection;

fn state_dir() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().to_path_buf();
    (tmp, dir)
}

fn open_store(scope: &str, dir: &Path) -> PoolStore {
    PoolStore::open(scope, dir).expect("open pool store")
}

fn agent(scope: &str, name: &str) -> AgentRecord {
    let now = 1_700_000_000_000i64;
    AgentRecord {
        agent_id: new_agent_id(),
        scope_key: scope.to_string(),
        terminal_id: Some(format!("term-{name}")),
        pane_id: Some(format!("pane-{name}")),
        agent_kind: Some("test-agent".to_string()),
        name: Some(name.to_string()),
        cwd: Some("/tmp/project".to_string()),
        status: "idle".to_string(),
        bootstrap_version: None,
        first_seen_at: now,
        last_seen_at: now,
        offline_at: None,
    }
}

// ---------------------------------------------------------------------------
// Migrations
// ---------------------------------------------------------------------------

#[test]
fn migrations_apply_once_and_reopen_is_idempotent() {
    let (_tmp, dir) = state_dir();
    let scope = "scope-a";

    {
        let store = open_store(scope, &dir);
        // Send a message so we can verify data survives reopen.
        store
            .messages()
            .send("ag_sender", "ag_rcpt", "hello")
            .expect("send");
    }

    // Reopen: migrations must be a no-op, data must persist.
    let store = open_store(scope, &dir);
    let inbox = store.messages().inbox("ag_rcpt", false).expect("inbox");
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].content, "hello");

    // Schema version is 3 (V1 schema + V2 rescue_jobs + V3 captured_at)
    // and core tables exist.
    let conn = Connection::open(store.path()).expect("raw conn");
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .expect("user_version");
    assert_eq!(version, 3);
    for table in ["agents", "threads", "messages", "rescue_jobs"] {
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [table],
                |r| r.get(0),
            )
            .expect("table check");
        assert!(exists, "missing table {table}");
    }
}

#[test]
fn pool_db_lives_under_pools_scope_key() {
    let (_tmp, dir) = state_dir();
    let store = open_store("abc123", &dir);
    assert_eq!(
        store.path(),
        dir.join("pools").join("abc123").join("pool.db")
    );
    assert!(store.path().exists());
}

// ---------------------------------------------------------------------------
// WAL / concurrent readers
// ---------------------------------------------------------------------------

#[test]
fn wal_mode_allows_concurrent_reader_connection() {
    let (_tmp, dir) = state_dir();
    let store = open_store("scope-wal", &dir);
    store
        .messages()
        .send("ag_a", "ag_b", "wal test")
        .expect("send");

    // journal_mode must be wal.
    let conn = Connection::open(store.path()).expect("raw conn");
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .expect("busy_timeout");
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .expect("journal_mode");
    assert_eq!(mode.to_lowercase(), "wal");

    // A second independent connection reads committed data.
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .expect("count");
    assert_eq!(count, 1);

    // A second PoolStore handle to the same path sees the same data.
    let store2 = open_store("scope-wal", &dir);
    assert_eq!(
        store2.messages().inbox("ag_b", false).expect("inbox").len(),
        1
    );
}

// ---------------------------------------------------------------------------
// MessageStore transitions
// ---------------------------------------------------------------------------

#[test]
fn happy_path_pending_delivered_acknowledged() {
    let (_tmp, dir) = state_dir();
    let store = open_store("scope-msg", &dir);
    let msgs = store.messages();

    let msg = msgs.send("ag_a", "ag_b", "hi").expect("send");
    assert!(msg.id.starts_with("msg_"));
    assert!(msg.thread_id.as_deref().unwrap_or("").starts_with("th_"));
    assert_eq!(msg.status, MessageStatus::Pending);

    msgs.mark_delivered(&msg.id).expect("mark_delivered");
    let m = msgs.message(&msg.id).expect("get").expect("some");
    assert_eq!(m.status, MessageStatus::Delivered);
    assert!(m.delivered_at.is_some());

    msgs.mark_acknowledged(&msg.id).expect("ack");
    let m = msgs.message(&msg.id).expect("get").expect("some");
    assert_eq!(m.status, MessageStatus::Acknowledged);
    assert!(m.acked_at.is_some());
}

#[test]
fn pending_can_be_acknowledged_directly() {
    let (_tmp, dir) = state_dir();
    let store = open_store("scope-pending-ack", &dir);
    let msgs = store.messages();

    // A recipient that reads pending mail via inbox consumes it: the
    // outbox must never deliver it again.
    let msg = msgs.send("ag_a", "ag_b", "early read").expect("send");
    msgs.mark_acknowledged(&msg.id).expect("ack pending");
    let m = msgs.message(&msg.id).expect("get").expect("some");
    assert_eq!(m.status, MessageStatus::Acknowledged);
    assert!(m.acked_at.is_some());
    assert!(msgs.outbox_due(i64::MAX).expect("due").is_empty());
}

#[test]
fn invalid_transitions_are_rejected() {
    let (_tmp, dir) = state_dir();
    let store = open_store("scope-trans", &dir);
    let msgs = store.messages();
    let msg = msgs.send("ag_a", "ag_b", "hi").expect("send");

    // retry on a plain pending message is allowed (clears any defer
    // schedule); it only fails from delivered/acknowledged.
    msgs.retry(&msg.id).expect("retry pending is a no-op");
    // failed -> acknowledged must fail (failed mail was never read).
    msgs.record_failure(&msg.id, None).expect("fail");
    assert!(matches!(
        msgs.mark_acknowledged(&msg.id),
        Err(StoreError::Invalid(_))
    ));
    msgs.retry(&msg.id).expect("retry");
    // record_failure on acknowledged must fail.
    msgs.mark_delivered(&msg.id).expect("delivered");
    msgs.mark_acknowledged(&msg.id).expect("ack");
    assert!(matches!(
        msgs.record_failure(&msg.id, None),
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(msgs.retry(&msg.id), Err(StoreError::Invalid(_))));
    // Double mark_delivered fails (delivered -> delivered not allowed).
    assert!(matches!(
        msgs.mark_delivered(&msg.id),
        Err(StoreError::Invalid(_))
    ));
    // Unknown id -> NotFound.
    assert!(matches!(
        msgs.mark_delivered("msg_nonexistent"),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn failure_retry_dead_letter_flow() {
    let (_tmp, dir) = state_dir();
    let store = open_store("scope-fail", &dir);
    let msgs = store.messages();

    let msg = msgs.send("ag_a", "ag_b", "flaky").expect("send");

    // First failure -> failed, scheduled retry.
    let st = msgs
        .record_failure(&msg.id, Some(4_000_000_000_000))
        .expect("failure");
    assert_eq!(st, MessageStatus::Failed);
    let m = msgs.message(&msg.id).expect("get").expect("some");
    assert_eq!(m.attempt_count, 1);
    assert_eq!(m.next_retry_at, Some(4_000_000_000_000));

    // failed -> delivered is allowed (retry succeeded).
    msgs.mark_delivered(&msg.id)
        .expect("delivered after failure");
    let m = msgs.message(&msg.id).expect("get").expect("some");
    assert_eq!(m.status, MessageStatus::Delivered);
    assert_eq!(m.next_retry_at, None, "retry schedule cleared on delivery");

    // failed -> pending via explicit retry.
    let msg2 = msgs.send("ag_a", "ag_b", "retry me").expect("send2");
    msgs.record_failure(&msg2.id, Some(1)).expect("fail");
    msgs.retry(&msg2.id).expect("retry");
    let m = msgs.message(&msg2.id).expect("get").expect("some");
    assert_eq!(m.status, MessageStatus::Pending);
    assert_eq!(m.next_retry_at, None);

    // Drive to dead-letter.
    let msg3 = msgs.send("ag_a", "ag_b", "doomed").expect("send3");
    for i in 1..=MAX_DELIVERY_ATTEMPTS {
        let st = msgs.record_failure(&msg3.id, Some(1)).expect("fail");
        if i < MAX_DELIVERY_ATTEMPTS {
            assert_eq!(st, MessageStatus::Failed);
        } else {
            assert_eq!(st, MessageStatus::Dead);
        }
    }
    let m = msgs.message(&msg3.id).expect("get").expect("some");
    assert_eq!(m.status, MessageStatus::Dead);
    assert_eq!(m.next_retry_at, None, "dead rows drop retry schedule");

    // dead -> pending via explicit retry (revive).
    msgs.retry(&msg3.id).expect("revive");
    assert_eq!(
        msgs.message(&msg3.id).expect("get").expect("some").status,
        MessageStatus::Pending
    );
}

#[test]
fn send_in_thread_and_reply() {
    let (_tmp, dir) = state_dir();
    let store = open_store("scope-thread", &dir);
    let msgs = store.messages();

    let t = msgs.create_thread(Some("project sync")).expect("thread");
    assert!(t.thread_id.starts_with("th_"));
    assert_eq!(t.subject.as_deref(), Some("project sync"));

    let m1 = msgs
        .send_in_thread(&t.thread_id, "ag_a", "ag_b", "message", "first")
        .expect("send1");
    let m2 = msgs
        .send_in_thread(&t.thread_id, "ag_b", "ag_a", "message", "second")
        .expect("send2");
    // Unknown thread -> NotFound.
    assert!(matches!(
        msgs.send_in_thread("th_nope", "ag_a", "ag_b", "message", "x"),
        Err(StoreError::NotFound(_))
    ));

    let history = msgs.thread_messages(&t.thread_id).expect("history");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].id, m1.id);
    assert_eq!(history[1].id, m2.id);

    // Reply resolves recipient = original sender, kind = reply.
    let reply = msgs.reply(&m1.id, "ag_b", "got it").expect("reply");
    assert_eq!(reply.to_agent, "ag_a");
    assert_eq!(reply.from_agent, "ag_b");
    assert_eq!(reply.kind, "reply");
    assert_eq!(reply.thread_id.as_deref(), Some(t.thread_id.as_str()));
    assert_eq!(msgs.thread_messages(&t.thread_id).expect("h").len(), 3);

    // threads() lists the thread with bumped activity.
    let threads = msgs.threads().expect("threads");
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].thread_id, t.thread_id);
}

// ---------------------------------------------------------------------------
// Inbox scoping
// ---------------------------------------------------------------------------

#[test]
fn inbox_is_scoped_per_agent() {
    let (_tmp, dir) = state_dir();
    let store = open_store("scope-inbox", &dir);
    let msgs = store.messages();

    msgs.send("ag_a", "ag_b", "for b").expect("s1");
    msgs.send("ag_a", "ag_c", "for c").expect("s2");
    msgs.send("ag_b", "ag_a", "for a").expect("s3");

    let b_all = msgs.inbox("ag_b", false).expect("b inbox");
    assert_eq!(b_all.len(), 1);
    assert_eq!(b_all[0].to_agent, "ag_b");

    let c_all = msgs.inbox("ag_c", false).expect("c inbox");
    assert_eq!(c_all.len(), 1);
    assert_eq!(c_all[0].content, "for c");

    let a_unread = msgs.inbox("ag_a", true).expect("a unread");
    assert_eq!(a_unread.len(), 1);
    assert_eq!(a_unread[0].content, "for a");

    // Ack the message -> drops out of unread, stays in full inbox.
    msgs.mark_delivered(&a_unread[0].id).expect("delivered");
    msgs.mark_acknowledged(&a_unread[0].id).expect("ack");
    assert!(msgs.inbox("ag_a", true).expect("unread").is_empty());
    assert_eq!(msgs.inbox("ag_a", false).expect("all").len(), 1);
}

// ---------------------------------------------------------------------------
// Outbox / backoff
// ---------------------------------------------------------------------------

#[test]
fn outbox_due_respects_next_retry_at() {
    let (_tmp, dir) = state_dir();
    let store = open_store("scope-outbox", &dir);
    let msgs = store.messages();
    let now = 1_000_000i64;

    let pending = msgs.send("ag_a", "ag_b", "new").expect("pending");
    let later = msgs.send("ag_a", "ag_b", "later").expect("later");
    let past = msgs.send("ag_a", "ag_b", "past").expect("past");
    let done = msgs.send("ag_a", "ag_b", "done").expect("done");

    // `later` fails with a future retry time; `past` fails with a past one.
    msgs.record_failure(&later.id, Some(now + 60_000))
        .expect("fail later");
    msgs.record_failure(&past.id, Some(now - 1))
        .expect("fail past");
    msgs.mark_delivered(&done.id).expect("delivered");

    let due = msgs.outbox_due(now).expect("outbox");
    let ids: Vec<&str> = due.iter().map(|m| m.id.as_str()).collect();
    assert!(ids.contains(&pending.id.as_str()), "pending always due");
    assert!(ids.contains(&past.id.as_str()), "past-due failure retried");
    assert!(!ids.contains(&later.id.as_str()), "future retry not due");
    assert!(!ids.contains(&done.id.as_str()), "delivered not in outbox");

    // After the retry time passes, `later` becomes due.
    let due_later = msgs.outbox_due(now + 60_001).expect("outbox later");
    assert!(due_later.iter().any(|m| m.id == later.id));

    // Oldest-first ordering.
    let positions: Vec<usize> = [pending.id.as_str(), past.id.as_str()]
        .iter()
        .map(|id| ids.iter().position(|x| x == id).expect("present"))
        .collect();
    assert!(positions[0] < positions[1], "created_at ordering");
}

// ---------------------------------------------------------------------------
// RegistryStore
// ---------------------------------------------------------------------------

#[test]
fn registry_upsert_is_idempotent() {
    let (_tmp, dir) = state_dir();
    let store = open_store("scope-reg", &dir);
    let reg = store.registry();

    let mut rec = agent("scope-reg", "alice");
    rec.agent_id = "ag_fixed1".to_string();
    reg.upsert_agent(&rec).expect("upsert1");
    reg.upsert_agent(&rec).expect("upsert2");

    let agents = reg.list_agents(false).expect("list");
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].first_seen_at, rec.first_seen_at);
    assert_eq!(agents[0].agent_id, "ag_fixed1");

    // Update fields; first_seen_at must be preserved.
    let mut rec2 = rec.clone();
    rec2.status = "working".to_string();
    rec2.last_seen_at = rec.last_seen_at + 5000;
    reg.upsert_agent(&rec2).expect("upsert3");
    let got = reg.get_agent("ag_fixed1").expect("get").expect("some");
    assert_eq!(got.status, "working");
    assert_eq!(got.first_seen_at, rec.first_seen_at);
    assert_eq!(got.last_seen_at, rec.last_seen_at + 5000);
}

#[test]
fn rekey_pane_preserves_agent_id() {
    let (_tmp, dir) = state_dir();
    let store = open_store("scope-rekey", &dir);
    let reg = store.registry();

    let mut rec = agent("scope-rekey", "bob");
    rec.agent_id = "ag_bob".to_string();
    reg.upsert_agent(&rec).expect("upsert");

    reg.rekey_pane("ag_bob", "pane-bob-moved").expect("rekey");
    let got = reg.get_agent("ag_bob").expect("get").expect("some");
    assert_eq!(got.agent_id, "ag_bob");
    assert_eq!(got.pane_id.as_deref(), Some("pane-bob-moved"));
    // terminal_id anchor unchanged.
    assert_eq!(got.terminal_id.as_deref(), Some("term-bob"));

    // Resolving by the NEW pane id works; old pane id does not.
    assert!(reg.resolve_peer("pane-bob-moved").is_ok());
    assert!(matches!(
        reg.resolve_peer("pane-bob"),
        Err(StoreError::NotFound(_))
    ));

    // Unknown agent -> NotFound.
    assert!(matches!(
        reg.rekey_pane("ag_ghost", "pane-x"),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn mark_offline_and_resolve_peer() {
    let (_tmp, dir) = state_dir();
    let store = open_store("scope-peer", &dir);
    let reg = store.registry();

    let mut a = agent("scope-peer", "alice");
    a.agent_id = "ag_alice".to_string();
    a.name = Some("alice".to_string());
    let mut b = agent("scope-peer", "bob");
    b.agent_id = "ag_bob".to_string();
    b.name = Some("bob".to_string());
    reg.upsert_agent(&a).expect("a");
    reg.upsert_agent(&b).expect("b");

    // Resolve by agent_id, pane_id, terminal_id, name.
    for target in ["ag_alice", "pane-alice", "term-alice", "alice"] {
        let r = reg.resolve_peer(target).expect("resolve");
        assert_eq!(r.agent_id, "ag_alice", "target {target}");
    }

    // Not found.
    assert!(matches!(
        reg.resolve_peer("nobody"),
        Err(StoreError::NotFound(_))
    ));

    // Ambiguity: two live agents sharing a name.
    let mut c = agent("scope-peer", "carol");
    c.agent_id = "ag_carol2".to_string();
    c.name = Some("alice".to_string()); // duplicate name with ag_alice
    reg.upsert_agent(&c).expect("c");
    // agent_id still resolves uniquely.
    assert_eq!(
        reg.resolve_peer("ag_alice").expect("id").agent_id,
        "ag_alice"
    );
    // name 'alice' now ambiguous.
    assert!(matches!(
        reg.resolve_peer("alice"),
        Err(StoreError::Ambiguous(_))
    ));

    // mark_offline removes from live resolution and default listing.
    reg.mark_offline("ag_carol2").expect("offline");
    let got = reg.get_agent("ag_carol2").expect("get").expect("some");
    assert_eq!(got.status, herdr_pool_store::STATUS_OFFLINE);
    assert!(got.offline_at.is_some());
    assert_eq!(reg.list_agents(false).expect("live").len(), 2);
    assert_eq!(reg.list_agents(true).expect("all").len(), 3);
    // Name 'alice' unambiguous again.
    assert_eq!(
        reg.resolve_peer("alice").expect("resolve").agent_id,
        "ag_alice"
    );
    assert!(reg.get_by_terminal("term-carol").expect("term").is_none());

    // Unknown agent offline -> NotFound.
    assert!(matches!(
        reg.mark_offline("ag_ghost"),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn cross_scope_isolation_by_construction() {
    let (_tmp, dir) = state_dir();
    let store_a = open_store("scope-x", &dir);
    let store_b = open_store("scope-y", &dir);

    let mut rec = agent("scope-x", "wally");
    rec.agent_id = "ag_wally".to_string();
    store_a.registry().upsert_agent(&rec).expect("upsert");
    store_a
        .messages()
        .send("ag_wally", "ag_other", "secret")
        .expect("send");

    // Scope B sees nothing.
    assert!(store_b
        .registry()
        .list_agents(true)
        .expect("list")
        .is_empty());
    assert!(matches!(
        store_b.registry().resolve_peer("ag_wally"),
        Err(StoreError::NotFound(_))
    ));
    assert!(store_b
        .messages()
        .inbox("ag_other", false)
        .expect("inbox")
        .is_empty());
    assert!(store_b.messages().threads().expect("threads").is_empty());

    // Distinct db files.
    assert_ne!(store_a.path(), store_b.path());
}

// ---------------------------------------------------------------------------
// ScopeIndex
// ---------------------------------------------------------------------------

#[test]
fn scope_index_register_resolve_list() {
    let (_tmp, dir) = state_dir();
    let index = ScopeIndex::open(&dir).expect("index");

    assert!(index.resolve("missing").expect("resolve").is_none());
    index.register("k1", "/proj/one").expect("reg1");
    index.register("k2", "/proj/two").expect("reg2");
    // Idempotent re-register keeps created_at and updates path.
    index.register("k1", "/proj/one").expect("reg1 again");

    assert_eq!(
        index.resolve("k1").expect("resolve").as_deref(),
        Some("/proj/one")
    );
    let list = index.list().expect("list");
    assert_eq!(list.len(), 2);
    assert!(list
        .iter()
        .any(|e| e.scope_key == "k1" && e.canonical_path == "/proj/one"));
}

#[test]
fn agent_ids_are_unique_within_same_millisecond() {
    // Regression: ids truncated to the uuidv7 timestamp prefix collided for
    // agents registered in the same ms, silently merging registry rows.
    let ids: std::collections::HashSet<String> = (0..1000).map(|_| new_agent_id()).collect();
    assert_eq!(ids.len(), 1000);
    assert!(ids.iter().all(|id| id.starts_with("ag_") && id.len() == 35));
}

#[test]
fn defer_reschedules_without_consuming_attempts() {
    let (_tmp, dir) = state_dir();
    let store = open_store("defer_scope", &dir);
    let msgs = store.messages();
    let msg = msgs.send("ag_a", "ag_b", "busy recipient").expect("send");

    msgs.defer(&msg.id, Some(1_999_999_999_999)).expect("defer");
    let m = msgs.message(&msg.id).expect("get").expect("some");
    assert_eq!(m.status, MessageStatus::Pending);
    assert_eq!(m.attempt_count, 0);
    assert_eq!(m.next_retry_at, Some(1_999_999_999_999));

    // Deferred pending is not due before next_retry_at; retry() clears
    // the schedule so it becomes due immediately.
    assert!(msgs.outbox_due(1_000_000_000_000).expect("due").is_empty());
    msgs.retry(&msg.id).expect("retry pending");
    let m = msgs.message(&msg.id).expect("get").expect("some");
    assert_eq!(m.status, MessageStatus::Pending);
    assert_eq!(m.next_retry_at, None);
    assert_eq!(msgs.outbox_due(0).expect("due").len(), 1);

    // Defer on terminal states is rejected.
    msgs.mark_delivered(&msg.id).expect("delivered");
    assert!(msgs.defer(&msg.id, None).is_err());
}

// ---------------------------------------------------------------------------
// V2: rescue_jobs
// ---------------------------------------------------------------------------

use herdr_pool_store::RescueStatus;

#[test]
fn rescue_create_pending_is_idempotent_per_source() {
    let (_tmp, dir) = state_dir();
    let store = open_store("rescue_scope", &dir);
    let rescues = store.rescues();

    assert!(rescues
        .create_pending(
            "ag_src",
            Some("w:p1"),
            Some("tui".into()),
            Some("visible".into())
        )
        .expect("create"));
    // Second open job for the same source is refused.
    assert!(!rescues
        .create_pending("ag_src", Some("w:p1"), None, None)
        .expect("create dup"));
    // A different source is fine.
    assert!(rescues
        .create_pending("ag_src2", Some("w:p2"), None, None)
        .expect("create src2"));

    let open = rescues.list_open().expect("list");
    assert_eq!(open.len(), 2);
    assert!(open.iter().all(|j| j.status == RescueStatus::Pending));
    assert_eq!(open[0].snapshot.as_deref(), Some("tui"));
    assert_eq!(open[0].snapshot_source.as_deref(), Some("visible"));
}

#[test]
fn rescue_assign_reserves_helper_and_unassign_releases() {
    let (_tmp, dir) = state_dir();
    let store = open_store("rescue_assign", &dir);
    let rescues = store.rescues();
    rescues
        .create_pending("ag_src", Some("w:p1"), None, None)
        .expect("create");
    let job = rescues
        .open_for_source("ag_src")
        .expect("open")
        .expect("some");

    // Claim for a helper.
    assert!(rescues
        .assign(&job.rescue_id, "ag_helper", Some("w:p9"))
        .expect("assign"));
    let assigned = rescues.get(&job.rescue_id).expect("get").expect("some");
    assert_eq!(assigned.status, RescueStatus::Assigned);
    assert_eq!(assigned.helper_agent_id.as_deref(), Some("ag_helper"));
    assert_eq!(assigned.attempts, 1);
    assert!(rescues
        .open_for_helper("ag_helper")
        .expect("helper")
        .is_some());

    // A second job cannot reserve the same helper (unique index).
    rescues
        .create_pending("ag_src2", Some("w:p2"), None, None)
        .expect("create src2");
    let job2 = rescues
        .open_for_source("ag_src2")
        .expect("open")
        .expect("some");
    let dup = rescues.assign(&job2.rescue_id, "ag_helper", None);
    assert!(dup.is_err(), "double-booking a helper must fail");

    // Running → prompt succeeded; unassign only applies to 'assigned'.
    rescues
        .set_status(&job.rescue_id, RescueStatus::Running)
        .expect("running");
    rescues.unassign(&job.rescue_id).expect("unassign noop");
    assert_eq!(
        rescues
            .get(&job.rescue_id)
            .expect("get")
            .expect("some")
            .status,
        RescueStatus::Running
    );
}

#[test]
fn rescue_terminal_status_and_open_for_source() {
    let (_tmp, dir) = state_dir();
    let store = open_store("rescue_terminal", &dir);
    let rescues = store.rescues();
    rescues
        .create_pending("ag_src", Some("w:p1"), None, None)
        .expect("create");
    let job = rescues
        .open_for_source("ag_src")
        .expect("open")
        .expect("some");

    rescues
        .set_status(&job.rescue_id, RescueStatus::Resolved)
        .expect("resolve");
    let done = rescues.get(&job.rescue_id).expect("get").expect("some");
    assert_eq!(done.status, RescueStatus::Resolved);
    assert!(done.resolved_at.is_some());
    // Terminal job frees the source for a future blocked episode.
    assert!(rescues.open_for_source("ag_src").expect("open").is_none());
    assert!(rescues
        .create_pending("ag_src", None, None, None)
        .expect("re-create"));
}

#[test]
fn rescue_survives_reopen() {
    let (_tmp, dir) = state_dir();
    let store = open_store("rescue_persist", &dir);
    store
        .rescues()
        .create_pending(
            "ag_src",
            Some("w:p1"),
            Some("snap".into()),
            Some("visible".into()),
        )
        .expect("create");
    drop(store);

    // Reopen the same db: the pending job is still there (daemon restart).
    let store = open_store("rescue_persist", &dir);
    let open = store.rescues().list_open().expect("list");
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].source_agent_id, "ag_src");
    assert_eq!(open[0].snapshot.as_deref(), Some("snap"));
}

#[test]
fn rescue_latest_for_source_returns_newest_regardless_of_status() {
    let (_tmp, dir) = state_dir();
    let store = open_store("rescue_latest", &dir);
    let rescues = store.rescues();
    assert!(rescues
        .latest_for_source("ag_src")
        .expect("latest")
        .is_none());

    rescues
        .create_pending("ag_src", Some("w:p1"), None, None)
        .expect("create");
    let first = rescues
        .latest_for_source("ag_src")
        .expect("latest")
        .unwrap();
    assert_eq!(first.status, RescueStatus::Pending);

    // A terminal job becomes the latest — open_for_source goes empty but
    // latest_for_source still reports it (the needs_human mute signal).
    rescues
        .set_status(&first.rescue_id, RescueStatus::NeedsHuman)
        .expect("needs_human");
    assert!(rescues.open_for_source("ag_src").expect("open").is_none());
    let latest = rescues
        .latest_for_source("ag_src")
        .expect("latest")
        .unwrap();
    assert_eq!(latest.rescue_id, first.rescue_id);
    assert_eq!(latest.status, RescueStatus::NeedsHuman);
    assert!(latest.resolved_at.is_some());
}

#[test]
fn rescue_update_source_refreshes_pane_and_snapshot() {
    let (_tmp, dir) = state_dir();
    let store = open_store("rescue_update", &dir);
    let rescues = store.rescues();
    rescues
        .create_pending("ag_src", Some("w:p1"), None, None)
        .expect("create");
    let job = rescues
        .open_for_source("ag_src")
        .expect("open")
        .expect("some");

    rescues
        .update_source(
            &job.rescue_id,
            Some("w:p2"),
            Some("new".into()),
            Some("visible".into()),
        )
        .expect("update");
    let updated = rescues.get(&job.rescue_id).expect("get").expect("some");
    assert_eq!(updated.source_pane_id.as_deref(), Some("w:p2"));
    assert_eq!(updated.snapshot.as_deref(), Some("new"));
    // COALESCE: None args keep existing values.
    rescues
        .update_source(&job.rescue_id, None, None, None)
        .expect("noop");
    let same = rescues.get(&job.rescue_id).expect("get").expect("some");
    assert_eq!(same.source_pane_id.as_deref(), Some("w:p2"));
}

#[test]
fn rescue_helper_report_transitions_and_validates_helper() {
    let (_tmp, dir) = state_dir();
    let store = open_store("rescue_report", &dir);
    let rescues = store.rescues();
    rescues
        .create_pending("ag_src", Some("w:p1"), None, None)
        .expect("create");
    let job = rescues
        .open_for_source("ag_src")
        .expect("open")
        .expect("some");
    rescues
        .assign(&job.rescue_id, "ag_helper", Some("w:p9"))
        .expect("assign");
    rescues
        .set_status(&job.rescue_id, RescueStatus::Running)
        .expect("running");

    // A non-helper cannot report.
    assert!(rescues
        .helper_report(
            &job.rescue_id,
            "ag_intruder",
            herdr_pool_store::HelperReport::Done
        )
        .is_err());

    // Helper reports done → verifying.
    let reported = rescues
        .helper_report(
            &job.rescue_id,
            "ag_helper",
            herdr_pool_store::HelperReport::Done,
        )
        .expect("report done");
    assert_eq!(reported.status, RescueStatus::Verifying);
    // Idempotent: reporting done again while verifying is fine.
    let again = rescues
        .helper_report(
            &job.rescue_id,
            "ag_helper",
            herdr_pool_store::HelperReport::Done,
        )
        .expect("report done again");
    assert_eq!(again.status, RescueStatus::Verifying);

    // needs-human from verifying → terminal.
    let nh = rescues
        .helper_report(
            &job.rescue_id,
            "ag_helper",
            herdr_pool_store::HelperReport::NeedsHuman,
        )
        .expect("report needs-human");
    assert_eq!(nh.status, RescueStatus::NeedsHuman);
    assert!(nh.resolved_at.is_some());
    // Terminal job rejects further reports.
    assert!(rescues
        .helper_report(
            &job.rescue_id,
            "ag_helper",
            herdr_pool_store::HelperReport::Done
        )
        .is_err());
}

#[test]
fn rescue_helper_report_needs_human_from_running() {
    let (_tmp, dir) = state_dir();
    let store = open_store("rescue_report_nh", &dir);
    let rescues = store.rescues();
    rescues
        .create_pending("ag_src", Some("w:p1"), None, None)
        .expect("create");
    let job = rescues
        .open_for_source("ag_src")
        .expect("open")
        .expect("some");
    rescues
        .assign(&job.rescue_id, "ag_helper", None)
        .expect("assign");
    // 'done' is not accepted while still 'assigned' (prompt may not have landed).
    assert!(rescues
        .helper_report(
            &job.rescue_id,
            "ag_helper",
            herdr_pool_store::HelperReport::Done
        )
        .is_err());
    // But needs-human is — the helper may fail before ever running.
    let nh = rescues
        .helper_report(
            &job.rescue_id,
            "ag_helper",
            herdr_pool_store::HelperReport::NeedsHuman,
        )
        .expect("report needs-human");
    assert_eq!(nh.status, RescueStatus::NeedsHuman);
}
