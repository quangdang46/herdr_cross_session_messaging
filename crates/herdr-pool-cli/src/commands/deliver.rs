//! Shared direct-delivery path for `send`/`reply`.
//!
//! The CLI attempts `AgentRuntime::send_prompt` immediately after
//! inserting the pending message. On `Delivered` the message is
//! `delivered`; on `Deferred`/`Failed`/transport errors it follows the
//! same failure policy the delivery worker uses
//! (`herdr_router::delivery`).

use herdr_pool_core::PoolContext;
use herdr_pool_store::{AgentRecord, Message};
use herdr_router::delivery::{retry_delay, BLOCKED_BACKOFF, NOT_READY_BACKOFF};
use herdr_router::envelope::message_envelope;
use herdr_runtime::{DeferReason, SendOutcome};

use crate::context::{delivery_target, now_millis, CmdResult, Ctx, Failure};

/// Deliver `msg` (already inserted as `pending`) to its recipient.
///
/// `sender` is the sender's registry record used to render the envelope's
/// `<kind>` label. Returns the message re-read after bookkeeping so callers
/// print the final status.
pub fn deliver(
    ctx: &Ctx,
    pool: &PoolContext,
    msg: &Message,
    sender: &AgentRecord,
) -> CmdResult<Message> {
    let messages = pool.pool.messages();
    let registry = pool.pool.registry();

    let recipient = registry.get_agent(&msg.to_agent)?.ok_or_else(|| {
        Failure::usage(format!(
            "recipient agent '{}' not in registry",
            msg.to_agent
        ))
    })?;

    // The recipient's TUI is still launching or mid `--resume` — prompting
    // now could paste the envelope into a half-restored UI. Same policy as
    // the delivery worker: defer without consuming an attempt.
    if !recipient.interactive_ready || recipient.launch_pending {
        let next = now_millis() + NOT_READY_BACKOFF.as_millis() as i64;
        messages.defer(&msg.id, Some(next))?;
        tracing::debug!(message = %msg.id, "recipient not interactive-ready; deferred");
        return Ok(messages.message(&msg.id)?.unwrap_or_else(|| msg.clone()));
    }

    let Some(target) = delivery_target(&recipient) else {
        // No pane/terminal to prompt: leave pending for the daemon, which
        // applies the offline/backoff policy.
        messages.record_failure(&msg.id, Some(now_millis()))?;
        return Ok(messages.message(&msg.id)?.unwrap_or_else(|| msg.clone()));
    };

    // The envelope is the wire contract shared with the router daemon —
    // reuse `herdr_router::envelope::message_envelope` verbatim so CLI and
    // daemon deliveries are byte-identical.
    let text = message_envelope(msg, Some(sender));

    let runtime = ctx.runtime()?;
    match runtime.send_prompt(&target, &text) {
        Ok(SendOutcome::Delivered { .. }) => {
            // The recipient may have consumed pending mail via `inbox`
            // while the prompt was in flight; `acknowledged` is already
            // terminal, so only a genuine transition failure is an error.
            if let Err(err) = messages.mark_delivered(&msg.id) {
                let current = messages.message(&msg.id)?;
                if current.map(|m| m.status) != Some(herdr_pool_store::MessageStatus::Acknowledged)
                {
                    return Err(err.into());
                }
            }
            tracing::info!(message = %msg.id, to = %msg.to_agent, "message delivered");
        }
        Ok(SendOutcome::Deferred {
            reason: DeferReason::Blocked,
        }) => {
            // Agent alive but busy: defer without consuming an attempt —
            // same policy as the delivery worker.
            let next = now_millis() + BLOCKED_BACKOFF.as_millis() as i64;
            messages.defer(&msg.id, Some(next))?;
            tracing::debug!(message = %msg.id, "agent blocked; deferred");
        }
        Ok(SendOutcome::Deferred {
            reason: DeferReason::NotReady,
        }) => {
            let next = now_millis() + NOT_READY_BACKOFF.as_millis() as i64;
            messages.defer(&msg.id, Some(next))?;
            tracing::debug!(message = %msg.id, "recipient not ready; deferred");
        }
        Ok(SendOutcome::Failed { kind, message }) => {
            let delay = retry_delay(msg.attempt_count);
            let next = now_millis() + delay.as_millis() as i64;
            messages.record_failure(&msg.id, Some(next))?;
            tracing::debug!(message = %msg.id, kind = ?kind, error = %message, "prompt failed; scheduled retry");
        }
        Err(err) => {
            let delay = retry_delay(msg.attempt_count);
            let next = now_millis() + delay.as_millis() as i64;
            messages.record_failure(&msg.id, Some(next))?;
            tracing::debug!(message = %msg.id, error = %err, "prompt failed; scheduled retry");
        }
    }

    Ok(messages.message(&msg.id)?.unwrap_or_else(|| msg.clone()))
}
