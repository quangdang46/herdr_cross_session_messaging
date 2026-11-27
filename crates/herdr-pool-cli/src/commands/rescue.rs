//! `herdr-pool rescue <id> <claim|done|needs-human>` — helper report-back.
//!
//! Called by the peer agent that received a `[HERDR RESCUE REQUEST]`.
//! `claim` is the fencing checkpoint: the helper must claim (with the
//! dispatch token from the prompt) *before* touching the source pane — a
//! failed claim means the rescue was reassigned and the helper must stop.
//! `done` moves the job to `verifying` — the router monitor still has to
//! observe the source leave `blocked` before it resolves; `needs-human`
//! escalates immediately.

use herdr_plugin::paths::EnvLookup;
use herdr_pool_store::HelperReport;

use crate::cli::RescueAction;
use crate::context::{open_identity_pool, resolve_identity, upsert_self, CmdResult, Ctx};
use crate::output::emit_json;

pub fn run(ctx: &Ctx, env: &EnvLookup<'_>, id: &str, action: &RescueAction) -> CmdResult<()> {
    let identity = resolve_identity(ctx, env)?;
    let pool = open_identity_pool(ctx, &identity)?;
    let record = upsert_self(&pool, &identity)?;
    let rescues = pool.pool.rescues();

    match action {
        RescueAction::Claim { token } => {
            let job = rescues.claim(id, &record.agent_id, token).map_err(|err| {
                crate::context::Failure::runtime(format!("rescue claim failed: {err}"))
            })?;
            if ctx.json {
                emit_json(&serde_json::json!({
                    "rescue_id": job.rescue_id,
                    "status": format!("{:?}", job.status).to_lowercase(),
                }));
            } else {
                println!("rescue {} claimed -> {:?}", job.rescue_id, job.status);
            }
            return Ok(());
        }
        RescueAction::Done { .. } | RescueAction::NeedsHuman { .. } => {}
    }

    let (report, token) = match action {
        RescueAction::Done { token } => (HelperReport::Done, token.as_deref()),
        RescueAction::NeedsHuman { token } => (HelperReport::NeedsHuman, token.as_deref()),
        RescueAction::Claim { .. } => unreachable!(),
    };
    let job = rescues
        .helper_report(id, &record.agent_id, token, report)
        .map_err(|err| crate::context::Failure::runtime(format!("rescue report failed: {err}")))?;

    if matches!(action, RescueAction::NeedsHuman { .. }) {
        // Surface the escalation immediately instead of waiting for the
        // daemon's next sweep (the job is already terminal). hc has no
        // toast surface; `notify` is a no-op there.
        if let Ok(runtime) = ctx.runtime() {
            let _ = runtime.notify(
                "herdr-pool: rescue needs a human",
                Some(&format!(
                    "{} for agent {} — please check the pane",
                    job.rescue_id, job.source_agent_id
                )),
            );
        }
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
