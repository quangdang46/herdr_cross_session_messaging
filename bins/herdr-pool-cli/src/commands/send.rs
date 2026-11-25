//! `herdr-pool send <peer> <text> [--thread ID]` — send a pool message.
//!
//! Flow (per research/beads-import.md §M3):
//! 1. Resolve self identity + open this scope's pool; upsert self.
//! 2. Refresh the registry from `agent.list` (cheap reconcile).
//! 3. `resolve_peer` — `ag_*` id / pane / terminal id / unambiguous name.
//! 4. Insert the message as `pending` (new thread, or `--thread` existing).
//! 5. Attempt immediate `agent.prompt` delivery ([`deliver`]); blocked or
//!    not-ready recipients stay scheduled for the router daemon.
//! 6. Print message id + final status.

use herdr_plugin::paths::EnvLookup;
use serde::Serialize;

use crate::commands::deliver::deliver;
use crate::context::{
    ensure_daemon, open_identity_pool, refresh_registry, resolve_identity, resolve_peer,
    upsert_self, CmdResult, Ctx,
};
use crate::output::{emit_json, MessageJson};

#[derive(Serialize)]
struct SendJson {
    message: MessageJson,
}

pub fn run(
    ctx: &Ctx,
    env: &EnvLookup<'_>,
    peer: &str,
    text: &str,
    thread: Option<&str>,
) -> CmdResult<()> {
    if text.trim().is_empty() {
        return Err(crate::context::Failure::usage(
            "message text must not be empty",
        ));
    }

    // Make sure a stopped daemon can't strand the message; best-effort.
    ensure_daemon(ctx)?;

    let identity = resolve_identity(ctx, env)?;
    let pool = open_identity_pool(ctx, &identity)?;
    let self_record = upsert_self(&pool, &identity)?;
    refresh_registry(ctx, &pool)?;

    let recipient = resolve_peer(&pool, peer)?;
    if recipient.agent_id == self_record.agent_id {
        return Err(crate::context::Failure::usage(
            "cannot send a message to yourself",
        ));
    }
    if recipient.status == herdr_pool_store::STATUS_OFFLINE {
        return Err(crate::context::Failure::usage(format!(
            "peer '{}' ({}) is offline",
            recipient.name.as_deref().unwrap_or(&recipient.agent_id),
            recipient.agent_id
        )));
    }

    let messages = pool.pool.messages();
    let msg = match thread {
        Some(tid) => messages.send_in_thread(
            tid,
            &self_record.agent_id,
            &recipient.agent_id,
            "message",
            text,
        )?,
        None => messages.send(&self_record.agent_id, &recipient.agent_id, text)?,
    };

    let delivered = deliver(ctx, &pool, &msg, &self_record)?;
    let out = MessageJson::from(&delivered);

    if ctx.json {
        emit_json(&SendJson { message: out });
    } else {
        println!(
            "sent {} -> {} ({}): {}",
            out.id,
            out.to_agent,
            out.status,
            crate::output::snippet(&out.content, 60)
        );
        if out.status == "pending" || out.status == "failed" {
            eprintln!(
                "note: delivery deferred to the router daemon (status: {})",
                out.status
            );
        }
    }
    Ok(())
}
