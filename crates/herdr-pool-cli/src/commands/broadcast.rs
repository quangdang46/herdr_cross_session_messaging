//! `herdr-pool broadcast <text>` — send a message to every live peer in
//! this project scope.
//!
//! Each recipient gets their own message + thread (same as an individual
//! `send`), delivered through the shared [`deliver`] path — blocked or
//! unreachable recipients stay scheduled for the router daemon.
//! Offline agents are skipped entirely (they are not addressable peers).

use herdr_plugin::paths::EnvLookup;
use serde::Serialize;

use crate::commands::deliver::deliver;
use crate::context::{
    ensure_daemon, open_identity_pool, refresh_registry, resolve_identity, upsert_self, CmdResult,
    Ctx, Failure,
};
use crate::output::{emit_json, print_table, MessageJson};

#[derive(Serialize)]
struct BroadcastJson {
    /// Number of live peers the message was sent to.
    recipients: usize,
    /// Per-recipient delivery outcome (status reflects post-deliver state).
    deliveries: Vec<DeliveryJson>,
}

#[derive(Serialize)]
struct DeliveryJson {
    agent_id: String,
    name: Option<String>,
    message: MessageJson,
}

pub fn run(ctx: &Ctx, env: &EnvLookup<'_>, text: &str) -> CmdResult<()> {
    if text.trim().is_empty() {
        return Err(Failure::usage("message text must not be empty"));
    }
    ensure_daemon(ctx)?;

    let identity = resolve_identity(ctx, env)?;
    let pool = open_identity_pool(ctx, &identity)?;
    let self_record = upsert_self(&pool, &identity)?;
    refresh_registry(ctx, &pool)?;

    // Live, non-self agents in this scope — same set `peers` shows.
    let peers: Vec<_> = pool
        .pool
        .registry()
        .list_agents(false)?
        .into_iter()
        .filter(|a| a.agent_id != self_record.agent_id)
        .collect();

    if peers.is_empty() {
        if ctx.json {
            emit_json(&BroadcastJson {
                recipients: 0,
                deliveries: vec![],
            });
        } else {
            println!("no other agents in this project scope");
        }
        return Ok(());
    }

    let messages = pool.pool.messages();
    let mut deliveries = Vec::with_capacity(peers.len());
    for peer in &peers {
        let msg = messages.send(&self_record.agent_id, &peer.agent_id, text)?;
        let delivered = deliver(ctx, &pool, &msg, &self_record)?;
        deliveries.push(DeliveryJson {
            agent_id: peer.agent_id.clone(),
            name: peer.name.clone(),
            message: MessageJson::from(&delivered),
        });
    }

    if ctx.json {
        emit_json(&BroadcastJson {
            recipients: deliveries.len(),
            deliveries,
        });
    } else {
        let rows: Vec<Vec<String>> = deliveries
            .iter()
            .map(|d| {
                vec![
                    d.agent_id.clone(),
                    d.name.clone().unwrap_or_else(|| "-".into()),
                    d.message.id.clone(),
                    d.message.status.clone(),
                ]
            })
            .collect();
        print_table(&["AGENT", "NAME", "MSG", "STATUS"], &rows);
        println!("broadcast to {} peer(s)", deliveries.len());
    }
    Ok(())
}
