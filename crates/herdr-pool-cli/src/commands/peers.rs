//! `herdr-pool peers` — live agents in this pane's project scope.

use herdr_plugin::paths::EnvLookup;

use crate::context::{
    open_identity_pool, refresh_registry, resolve_identity, upsert_self, CmdResult, Ctx,
};
use crate::output::{emit_json, print_table, AgentJson};
use serde::Serialize;

#[derive(Serialize)]
struct PeersJson {
    scope_key: String,
    canonical_path: String,
    peers: Vec<AgentJson>,
}

pub fn run(ctx: &Ctx, env: &EnvLookup<'_>) -> CmdResult<()> {
    let identity = resolve_identity(ctx, env)?;
    let pool = open_identity_pool(ctx, &identity)?;
    let self_record = upsert_self(&pool, &identity)?;

    // Cheap reconcile so peers reflect live Herdr state even when the
    // daemon is not running.
    refresh_registry(ctx, &pool)?;

    let peers: Vec<AgentJson> = pool
        .pool
        .registry()
        .list_agents(false)?
        .iter()
        .filter(|a| a.agent_id != self_record.agent_id)
        .map(AgentJson::from)
        .collect();

    if ctx.json {
        emit_json(&PeersJson {
            scope_key: pool.scope_key.as_str().to_string(),
            canonical_path: pool.canonical_path.to_string_lossy().into_owned(),
            peers,
        });
        return Ok(());
    }

    if peers.is_empty() {
        println!("no other agents in this project scope");
        return Ok(());
    }
    let rows: Vec<Vec<String>> = peers
        .iter()
        .map(|p| {
            vec![
                p.agent_id.clone(),
                p.agent_kind.clone().unwrap_or_else(|| "-".into()),
                p.name.clone().unwrap_or_else(|| "-".into()),
                p.status.clone(),
                p.cwd.clone().unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    print_table(&["AGENT_ID", "KIND", "NAME", "STATUS", "CWD"], &rows);
    Ok(())
}
