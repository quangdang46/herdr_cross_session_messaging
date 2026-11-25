//! `herdr-pool reply <msg|thread-id> <text>` — reply in the same thread.
//!
//! - `msg_*` id → [`MessageStore::reply`]: same thread, recipient = the
//!   original sender, `kind = "reply"`.
//! - `th_*` id → post into that thread addressed to the most recent
//!   participant who isn't us (the "original sender" for a thread we
//!   joined by receiving mail).
//!
//! Delivery goes through the same [`deliver`] path as `send`.

use herdr_plugin::paths::EnvLookup;

use crate::commands::deliver::deliver;
use crate::context::{
    ensure_daemon, open_identity_pool, refresh_registry, resolve_identity, upsert_self, CmdResult,
    Ctx, Failure,
};
use crate::output::{emit_json, MessageJson};
use serde::Serialize;

#[derive(Serialize)]
struct ReplyJson {
    message: MessageJson,
}

pub fn run(ctx: &Ctx, env: &EnvLookup<'_>, target: &str, text: &str) -> CmdResult<()> {
    if text.trim().is_empty() {
        return Err(Failure::usage("reply text must not be empty"));
    }
    ensure_daemon(ctx)?;

    let identity = resolve_identity(ctx, env)?;
    let pool = open_identity_pool(ctx, &identity)?;
    let self_record = upsert_self(&pool, &identity)?;
    refresh_registry(ctx, &pool)?;

    let messages = pool.pool.messages();

    // Dispatch on id shape first (`msg_`/`th_` prefixes are stable), then
    // fall back to lookup so callers can paste bare ids.
    let msg = if target.starts_with("th_") {
        reply_in_thread(&messages, &self_record.agent_id, target, text)?
    } else if target.starts_with("msg_") {
        match messages.message(target)? {
            Some(_) => messages.reply(target, &self_record.agent_id, text)?,
            None => {
                return Err(Failure::usage(format!("message '{target}' not found")));
            }
        }
    } else if is_thread(&messages, target)? {
        reply_in_thread(&messages, &self_record.agent_id, target, text)?
    } else if messages.message(target)?.is_some() {
        messages.reply(target, &self_record.agent_id, text)?
    } else {
        return Err(Failure::usage(format!(
            "'{target}' is neither a known message nor thread"
        )));
    };

    // `deliver` resolves the recipient record and reports a clear error
    // when it is missing from the registry.
    let delivered = deliver(ctx, &pool, &msg, &self_record)?;
    let out = MessageJson::from(&delivered);

    if ctx.json {
        emit_json(&ReplyJson { message: out });
    } else {
        println!(
            "replied {} -> {} ({}): {}",
            out.id,
            out.to_agent,
            out.status,
            crate::output::snippet(&out.content, 60)
        );
    }
    Ok(())
}

fn is_thread(messages: &herdr_pool_store::MessageStore<'_>, id: &str) -> CmdResult<bool> {
    Ok(messages.thread(id)?.is_some())
}

/// Post `kind="reply"` into thread `thread_id`, addressed to the most recent
/// message author who isn't `self_agent`.
fn reply_in_thread(
    messages: &herdr_pool_store::MessageStore<'_>,
    self_agent: &str,
    thread_id: &str,
    text: &str,
) -> CmdResult<herdr_pool_store::Message> {
    let msgs = messages.thread_messages(thread_id)?;
    if msgs.is_empty() {
        return Err(Failure::usage(format!(
            "thread '{thread_id}' has no messages"
        )));
    }
    // Most recent inbound message determines the reply target.
    let target = msgs
        .iter()
        .rev()
        .find(|m| m.from_agent != self_agent)
        .map(|m| m.from_agent.clone())
        .ok_or_else(|| {
            Failure::usage(format!(
                "thread '{thread_id}' has no peer messages to reply to"
            ))
        })?;
    Ok(messages.send_in_thread(thread_id, self_agent, &target, "reply", text)?)
}
