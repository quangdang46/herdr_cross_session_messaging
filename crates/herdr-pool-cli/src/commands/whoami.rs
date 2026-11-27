//! `herdr-pool whoami` — resolve and register this pane's pool identity.

use herdr_plugin::paths::EnvLookup;

use crate::context::{open_identity_pool, resolve_identity, upsert_self, CmdResult, Ctx};
use crate::output::{emit_json, WhoamiJson};

pub fn run(ctx: &Ctx, env: &EnvLookup<'_>) -> CmdResult<()> {
    let identity = resolve_identity(ctx, env)?;
    let pool = open_identity_pool(ctx, &identity)?;
    let record = upsert_self(&pool, &identity)?;

    let out = WhoamiJson {
        agent_id: record.agent_id.clone(),
        pane_id: identity.env.handle.clone(),
        workspace_id: identity.workspace.clone(),
        tab_id: identity.tab.clone(),
        agent_kind: record.agent_kind.clone(),
        name: record.name.clone(),
        scope_key: pool.scope_key.as_str().to_string(),
        canonical_path: pool.canonical_path.to_string_lossy().into_owned(),
        status: record.status.clone(),
    };

    if ctx.json {
        emit_json(&out);
    } else {
        println!("agent_id:      {}", out.agent_id);
        println!("pane_id:       {}", out.pane_id);
        println!(
            "workspace_id:  {}",
            out.workspace_id.as_deref().unwrap_or("-")
        );
        println!("tab_id:        {}", out.tab_id.as_deref().unwrap_or("-"));
        println!(
            "agent_kind:    {}",
            out.agent_kind.as_deref().unwrap_or("-")
        );
        println!("name:          {}", out.name.as_deref().unwrap_or("-"));
        println!("scope_key:     {}", out.scope_key);
        println!("project:       {}", out.canonical_path);
        println!("status:        {}", out.status);
    }
    Ok(())
}
