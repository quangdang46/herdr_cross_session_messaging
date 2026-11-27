//! `herdr-pool threads [--json]` — per-thread summaries for this scope.
//!
//! Each row shows the thread id, subject/first-message snippet, the peer
//! (the other party relative to the caller), unread count (messages to the
//! caller still pending/delivered), message count, and last activity.

use herdr_plugin::paths::EnvLookup;
use serde::Serialize;

use crate::context::{open_identity_pool, resolve_identity, upsert_self, CmdResult, Ctx};
use crate::output::{emit_json, fmt_time, print_table, snippet, ThreadSummaryJson};

#[derive(Serialize)]
struct ThreadsJson {
    threads: Vec<ThreadSummaryJson>,
}

pub fn run(ctx: &Ctx, env: &EnvLookup<'_>) -> CmdResult<()> {
    let identity = resolve_identity(ctx, env)?;
    let pool = open_identity_pool(ctx, &identity)?;
    let self_record = upsert_self(&pool, &identity)?;

    let messages = pool.pool.messages();
    let registry = pool.pool.registry();
    let mut summaries = Vec::new();

    for thread in messages.threads()? {
        let msgs = messages.thread_messages(&thread.thread_id)?;
        let mut unread = 0i64;
        let mut peer_agent: Option<String> = None;
        let mut peer_name: Option<String> = None;
        let mut peer_pane: Option<String> = None;
        let mut peer_kind: Option<String> = None;

        for m in &msgs {
            if m.to_agent == self_record.agent_id
                && matches!(
                    m.status,
                    herdr_pool_store::MessageStatus::Pending
                        | herdr_pool_store::MessageStatus::Delivered
                )
            {
                unread += 1;
            }
            // Peer = the most recent participant who isn't us.
            let other = if m.from_agent == self_record.agent_id {
                Some(m.to_agent.as_str())
            } else {
                Some(m.from_agent.as_str())
            };
            if let Some(other_id) = other {
                if let Ok(Some(rec)) = registry.get_agent(other_id) {
                    peer_agent = Some(rec.agent_id.clone());
                    peer_name = rec.name.clone();
                    peer_pane = rec.pane_id.clone();
                    peer_kind = rec.agent_kind.clone();
                } else {
                    peer_agent = Some(other_id.to_string());
                }
            }
        }

        let last = msgs.last();
        summaries.push(ThreadSummaryJson {
            thread_id: thread.thread_id.clone(),
            subject: thread.subject.clone(),
            last_message: last.map(|m| snippet(&m.content, 60)),
            peer_agent_id: peer_agent,
            peer_name,
            peer_pane_id: peer_pane,
            peer_agent_kind: peer_kind,
            unread_count: unread,
            message_count: msgs.len() as i64,
            last_activity_at: thread.last_activity_at,
        });
    }

    if ctx.json {
        emit_json(&ThreadsJson { threads: summaries });
        return Ok(());
    }

    if summaries.is_empty() {
        println!("no threads in this project scope");
        return Ok(());
    }
    let rows: Vec<Vec<String>> = summaries
        .iter()
        .map(|t| {
            vec![
                t.thread_id.clone(),
                peer_label(t),
                t.unread_count.to_string(),
                t.message_count.to_string(),
                fmt_time(t.last_activity_at),
                t.subject
                    .clone()
                    .or(t.last_message.clone())
                    .unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    print_table(
        &[
            "THREAD",
            "PEER",
            "UNREAD",
            "MSGS",
            "LAST_ACTIVITY",
            "SUBJECT",
        ],
        &rows,
    );
    Ok(())
}

/// Human peer label: `name (pane_id)` when both are known — the pane id is
/// the handle agents recognize from the Herdr UI.
fn peer_label(t: &ThreadSummaryJson) -> String {
    match (&t.peer_name, &t.peer_pane_id) {
        (Some(name), Some(pane)) => format!("{name} ({pane})"),
        (Some(name), None) => name.clone(),
        (None, Some(pane)) => pane.clone(),
        (None, None) => t.peer_agent_id.clone().unwrap_or_else(|| "-".into()),
    }
}
