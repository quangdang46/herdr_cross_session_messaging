//! `herdr-pool inbox [--unread]` and `herdr-pool inbox read <id>`.
//!
//! `inbox` lists messages addressed to this agent (`pending`, `delivered`,
//! `acknowledged`; `--unread` restricts to pending+delivered).
//! `inbox read <id>` / `message read <id>` mark the message acknowledged
//! (when it is `delivered`) and print the full content.

use herdr_plugin::paths::EnvLookup;
use herdr_pool_store::Message;
use serde::Serialize;

use crate::cli::InboxAction;
use crate::context::{open_identity_pool, resolve_identity, upsert_self, CmdResult, Ctx, Failure};
use crate::output::{emit_json, fmt_time, print_table, snippet, MessageJson};

#[derive(Serialize)]
struct InboxJson {
    unread_only: bool,
    messages: Vec<MessageJson>,
}

pub fn run(
    ctx: &Ctx,
    env: &EnvLookup<'_>,
    unread: bool,
    action: Option<&InboxAction>,
) -> CmdResult<()> {
    match action {
        Some(InboxAction::Read { id }) => read_message(ctx, env, id),
        None => list_inbox(ctx, env, unread),
    }
}

fn list_inbox(ctx: &Ctx, env: &EnvLookup<'_>, unread: bool) -> CmdResult<()> {
    let identity = resolve_identity(ctx, env)?;
    let pool = open_identity_pool(ctx, &identity)?;
    let self_record = upsert_self(&pool, &identity)?;

    let messages = pool.pool.messages().inbox(&self_record.agent_id, unread)?;

    if ctx.json {
        emit_json(&InboxJson {
            unread_only: unread,
            messages: messages.iter().map(MessageJson::from).collect(),
        });
        return Ok(());
    }

    if messages.is_empty() {
        println!(
            "{}",
            if unread {
                "no unread messages"
            } else {
                "inbox is empty"
            }
        );
        return Ok(());
    }
    let rows: Vec<Vec<String>> = messages
        .iter()
        .map(|m| {
            vec![
                m.id.clone(),
                m.from_agent.clone(),
                m.kind.clone(),
                m.status.to_string(),
                fmt_time(m.created_at),
                snippet(&m.content, 60),
            ]
        })
        .collect();
    print_table(&["ID", "FROM", "KIND", "STATUS", "TIME", "SNIPPET"], &rows);
    Ok(())
}

/// Shared `read` path for `inbox read` and `message read`.
///
/// Marks the message `acknowledged` when it is `delivered` (the only legal
/// source state for that transition) and only when it is addressed to the
/// caller. `pending` mail cannot be acknowledged yet — the store has no
/// pending→acknowledged edge — so it is shown with a note instead.
pub fn read_message(ctx: &Ctx, env: &EnvLookup<'_>, id: &str) -> CmdResult<()> {
    let identity = resolve_identity(ctx, env)?;
    let pool = open_identity_pool(ctx, &identity)?;
    let self_record = upsert_self(&pool, &identity)?;

    let msg = pool
        .pool
        .messages()
        .message(id)?
        .ok_or_else(|| Failure::usage(format!("message '{id}' not found")))?;

    if msg.to_agent != self_record.agent_id {
        return Err(Failure::usage(format!(
            "message '{id}' is not addressed to you ({})",
            self_record.agent_id
        )));
    }

    // Acknowledge only from `delivered`; pending mail isn't readable-as-ack
    // yet and acknowledged mail is already terminal for this purpose.
    if msg.status == herdr_pool_store::MessageStatus::Delivered {
        pool.pool.messages().mark_acknowledged(id)?;
    }
    let msg = pool.pool.messages().message(id)?.unwrap_or(msg);

    if ctx.json {
        emit_json(&MessageJson::from(&msg));
    } else {
        println!("id:         {}", msg.id);
        println!("thread:     {}", msg.thread_id.as_deref().unwrap_or("-"));
        println!("from:       {}", msg.from_agent);
        println!("status:     {}", msg.status);
        println!("created:    {}", fmt_time(msg.created_at));
        if let Some(t) = msg.delivered_at {
            println!("delivered:  {}", fmt_time(t));
        }
        if let Some(t) = msg.acked_at {
            println!("acknowledged: {}", fmt_time(t));
        }
        println!("---");
        println!("{}", msg.content);
        if msg.status == herdr_pool_store::MessageStatus::Pending {
            eprintln!("note: message is still pending delivery; not acknowledged");
        }
    }
    Ok(())
}

/// Fetch a message for `message get` (no ack, no recipient check beyond scope).
pub fn get_message(ctx: &Ctx, env: &EnvLookup<'_>, id: &str) -> CmdResult<Message> {
    let identity = resolve_identity(ctx, env)?;
    let pool = open_identity_pool(ctx, &identity)?;
    let _ = upsert_self(&pool, &identity)?;
    pool.pool
        .messages()
        .message(id)?
        .ok_or_else(|| Failure::usage(format!("message '{id}' not found")))
}
