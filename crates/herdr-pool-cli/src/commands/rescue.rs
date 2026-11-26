//! `herdr-pool rescue <id> <done|needs-human>` — helper report-back.
//!
//! Called by the peer agent that received a `[HERDR RESCUE REQUEST]` once it
//! finishes (or gives up on) driving the source pane. `done` moves the job
//! to `verifying` — the router monitor still has to observe the source
//! leave `blocked` before it resolves; `needs-human` escalates immediately.

use herdr_plugin::paths::EnvLookup;
use herdr_pool_store::HelperReport;
use herdr_protocol::NotificationShowParams;

use crate::cli::RescueOutcome;
use crate::context::{open_identity_pool, resolve_identity, upsert_self, CmdResult, Ctx};
use crate::output::emit_json;

pub fn run(ctx: &Ctx, env: &EnvLookup<'_>, id: &str, outcome: &RescueOutcome) -> CmdResult<()> {
    let identity = resolve_identity(ctx, env)?;
    let pool = open_identity_pool(ctx, &identity)?;
    let record = upsert_self(&pool, &identity)?;

    let report = match outcome {
        RescueOutcome::Done => HelperReport::Done,
        RescueOutcome::NeedsHuman => HelperReport::NeedsHuman,
    };
    let job = pool
        .pool
        .rescues()
        .helper_report(id, &record.agent_id, report)
        .map_err(|err| crate::context::Failure::runtime(format!("rescue report failed: {err}")))?;

    if matches!(outcome, RescueOutcome::NeedsHuman) {
        // Surface the escalation immediately instead of waiting for the
        // daemon's next sweep (the job is already terminal).
        let _ = ctx.client.notification_show(&NotificationShowParams {
            title: "herdr-pool: rescue needs a human".to_string(),
            body: Some(format!(
                "{} for agent {} — please check the pane",
                job.rescue_id, job.source_agent_id
            )),
            position: None,
            sound: None,
        });
    }

    if ctx.json {
        emit_json(&serde_json::json!({
            "rescue_id": job.rescue_id,
            "status": format!("{:?}", job.status).to_lowercase(),
        }));
    } else {
        println!("rescue {} -> {:?}", job.rescue_id, job.status);
    }
    Ok(())
}
