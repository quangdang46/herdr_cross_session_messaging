//! Outbox delivery worker.
//!
//! Drains `messages.outbox_due(now)` across every known scope pool and
//! submits each pending message via `agent.prompt` with the
//! [`crate::envelope`] text. The recipient's *current* target is resolved
//! from the registry on every attempt — target strings are never cached,
//! so `pane.move` re-keys and agent restarts are picked up automatically.
//!
//! ## Failure policy
//!
//! | `agent.prompt` outcome | action |
//! |---|---|
//! | success | `mark_delivered` |
//! | `agent_blocked` | `record_failure(now + 15s)` — harness queues nothing; retry shortly |
//! | `agent_not_ready` / `agent_not_found` / `agent_target_ambiguous` / stalled | `record_failure(now + backoff(attempts))` |
//! | transport/timeout/other | `record_failure(now + backoff(attempts))` |
//! | recipient offline > [`OFFLINE_DEAD_AFTER`] | dead-letter immediately + throttled `notification.show` |
//!
//! `record_failure` dead-letters at [`MAX_DELIVERY_ATTEMPTS`] attempts.
//! Backoff is exponential: 5s × 2^attempts capped at 5 minutes, plus jitter.

use std::sync::Arc;
use std::time::Duration;

use herdr_pool_store::{Message, MessageStatus, MessageStore, PoolStore, STATUS_OFFLINE};
use herdr_protocol::{HerdrClient, HerdrError, NotificationShowParams};

use crate::envelope::message_envelope;
use crate::error::Result;
use crate::pools::Pools;
use crate::registry_sync::{now_millis, Notify};

/// Base retry delay for the first failed attempt.
pub const BACKOFF_BASE: Duration = Duration::from_secs(5);
/// Maximum retry delay.
pub const BACKOFF_CAP: Duration = Duration::from_secs(300);
/// Delay applied after `agent_blocked` — shorter than the generic backoff
/// because the agent is alive and will unblock on its own.
pub const BLOCKED_BACKOFF: Duration = Duration::from_secs(15);
/// How long a recipient may stay offline before pending mail is
/// dead-lettered instead of retried forever.
pub const OFFLINE_DEAD_AFTER: Duration = Duration::from_secs(5 * 60);
/// Minimum interval between dead-letter `notification.show` toasts.
pub const DEAD_NOTIFY_INTERVAL: Duration = Duration::from_secs(60);
/// How often the worker sweeps the outbox when idle.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Exponential backoff schedule (pure, no jitter):
/// `min(BASE * 2^attempts, CAP)` where `attempts` is the count of failures
/// already recorded (`Message::attempt_count`).
pub fn retry_delay(attempts: i64) -> Duration {
    let shift = attempts.clamp(0, 20) as u32;
    let millis = BACKOFF_BASE
        .as_millis()
        .saturating_mul(1u128 << shift.min(16))
        .min(BACKOFF_CAP.as_millis());
    Duration::from_millis(millis as u64)
}

/// [`retry_delay`] plus up to 25% jitter derived from `nanos` (callers pass
/// `SystemTime`/`Instant` nanos or a counter — kept a parameter so the
/// schedule stays deterministic in tests).
pub fn retry_delay_jittered(attempts: i64, nanos: u128) -> Duration {
    let base = retry_delay(attempts);
    let jitter_millis = (nanos % (base.as_millis() / 4 + 1)) as u64;
    base + Duration::from_millis(jitter_millis)
}

fn jitter_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u128 + d.as_secs() as u128 * 1_000_000_000)
        .unwrap_or(0)
}

/// Delivery worker: drains outboxes until shutdown.
pub struct DeliveryWorker {
    client: HerdrClient,
    pools: Arc<Pools>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    notify: Notify,
    poll_interval: Duration,
    /// Last time a dead-letter notification was shown (throttle).
    last_dead_notify: std::cell::Cell<Option<std::time::Instant>>,
}

impl DeliveryWorker {
    pub fn new(
        client: HerdrClient,
        pools: Arc<Pools>,
        shutdown: Arc<std::sync::atomic::AtomicBool>,
        notify: Notify,
        poll_interval: Duration,
    ) -> Self {
        Self {
            client,
            pools,
            shutdown,
            notify,
            poll_interval,
            last_dead_notify: std::cell::Cell::new(None),
        }
    }

    /// Main loop: drain, then sleep until notified or `poll_interval`
    /// elapses. Checks the shutdown flag and the daemon's stop sentinel
    /// between sweeps.
    pub fn run(&self, stop_file: Option<&std::path::Path>) {
        let pair = self.notify.pair();
        let (lock, cvar) = &*pair;
        loop {
            if self.shutdown.load(std::sync::atomic::Ordering::SeqCst)
                || stop_file.is_some_and(|p| p.exists())
            {
                break;
            }
            if let Err(err) = self.drain_once() {
                tracing::warn!(error = %err, "outbox drain failed");
            }
            let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            let (guard, _) = cvar
                .wait_timeout(guard, self.poll_interval)
                .unwrap_or_else(|e| e.into_inner());
            drop(guard);
        }
    }

    /// One sweep over every pool's outbox. Returns messages attempted.
    /// Public so `tick` and tests can drive a single pass.
    pub fn drain_once(&self) -> Result<usize> {
        let now = now_millis();
        let mut attempted = 0;
        for pool in self.pools.all() {
            let messages = pool.messages();
            let due = match messages.outbox_due(now) {
                Ok(due) => due,
                Err(err) => {
                    tracing::warn!(scope = %pool.scope_key(), error = %err, "outbox_due failed");
                    continue;
                }
            };
            for msg in due {
                attempted += 1;
                if let Err(err) = self.deliver(pool.as_ref(), &messages, &msg) {
                    tracing::debug!(message = %msg.id, error = %err, "delivery bookkeeping failed");
                }
            }
        }
        Ok(attempted)
    }

    /// Deliver one message: resolve recipient → prompt → record outcome.
    fn deliver(&self, pool: &PoolStore, messages: &MessageStore<'_>, msg: &Message) -> Result<()> {
        let registry = pool.registry();
        let recipient = match registry.get_agent(&msg.to_agent)? {
            Some(recipient) => recipient,
            None => {
                // Unknown recipient id — count it as a failure so the
                // message eventually dead-letters instead of wedging.
                return self.record_retry(messages, msg, retry_delay(msg.attempt_count));
            }
        };

        if recipient.status == STATUS_OFFLINE {
            return self.handle_offline_recipient(messages, msg, &recipient);
        }

        // Resolve the CURRENT target on every attempt: terminal id is the
        // stable anchor, pane id the fallback. Never cache across attempts.
        let target = recipient
            .terminal_id
            .clone()
            .filter(|t| !t.is_empty())
            .or_else(|| recipient.pane_id.clone().filter(|p| !p.is_empty()));
        let Some(target) = target else {
            return self.record_retry(messages, msg, retry_delay(msg.attempt_count));
        };

        let sender = registry.get_agent(&msg.from_agent).ok().flatten();
        let text = message_envelope(msg, sender.as_ref());

        match self.client.agent_prompt(&target, &text, None) {
            Ok(_) => {
                messages.mark_delivered(&msg.id)?;
                tracing::info!(message = %msg.id, to = %msg.to_agent, "message delivered");
            }
            Err(err) if err.is_agent_blocked() => {
                // Agent is alive but busy: nothing was sent. Short backoff —
                // the next status transition or sweep retries it.
                self.record_retry(messages, msg, BLOCKED_BACKOFF)?;
            }
            Err(err) => {
                let delay = if err.is_agent_not_found()
                    || err.is_agent_target_ambiguous()
                    || err.is_agent_not_ready()
                    || err.is_timeout()
                    || matches!(err, HerdrError::Closed | HerdrError::Transport(_))
                {
                    retry_delay_jittered(msg.attempt_count, jitter_nanos())
                } else {
                    // agent_prompt_failed, empty_agent_prompt, protocol etc.:
                    // still retry with backoff; MAX_DELIVERY_ATTEMPTS bounds it.
                    retry_delay_jittered(msg.attempt_count, jitter_nanos())
                };
                tracing::debug!(message = %msg.id, error = %err, "prompt failed; backing off");
                self.record_retry(messages, msg, delay)?;
            }
        }
        Ok(())
    }

    /// Recipient is offline: dead-letter once it has been gone longer than
    /// [`OFFLINE_DEAD_AFTER`], otherwise keep retrying on the normal
    /// (capped) schedule so a quick restart does not lose mail.
    fn handle_offline_recipient(
        &self,
        messages: &MessageStore<'_>,
        msg: &Message,
        recipient: &herdr_pool_store::AgentRecord,
    ) -> Result<()> {
        let offline_for = recipient
            .offline_at
            .map(|at| now_millis().saturating_sub(at))
            .unwrap_or(0);
        if offline_for > OFFLINE_DEAD_AFTER.as_millis() as i64 {
            self.dead_letter(messages, msg)?;
            self.maybe_notify_dead(msg);
            return Ok(());
        }
        // While offline, poll no faster than once a minute.
        let delay = retry_delay(msg.attempt_count).max(Duration::from_secs(60));
        self.record_retry(messages, msg, delay)
    }

    /// Record a failed attempt with `next_retry_at = now + delay`.
    fn record_retry(
        &self,
        messages: &MessageStore<'_>,
        msg: &Message,
        delay: Duration,
    ) -> Result<()> {
        let next = now_millis() + delay.as_millis() as i64;
        let status = messages.record_failure(&msg.id, Some(next))?;
        if status == MessageStatus::Dead {
            tracing::warn!(message = %msg.id, "message dead-lettered");
            self.maybe_notify_dead(msg);
        }
        Ok(())
    }

    /// Force a message to `dead` regardless of remaining attempts (used for
    /// the offline-too-long path). `record_failure` is the only transition
    /// into `dead`, so call it until it reports `Dead` — bounded by
    /// `MAX_DELIVERY_ATTEMPTS`.
    fn dead_letter(&self, messages: &MessageStore<'_>, msg: &Message) -> Result<()> {
        for _ in 0..16 {
            match messages.record_failure(&msg.id, None)? {
                MessageStatus::Dead => return Ok(()),
                MessageStatus::Failed => continue,
                other => {
                    tracing::debug!(message = %msg.id, %other, "dead_letter: unexpected status");
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Throttled `notification.show` for dead-lettered mail.
    fn maybe_notify_dead(&self, msg: &Message) {
        let now = std::time::Instant::now();
        if self
            .last_dead_notify
            .get()
            .is_some_and(|t| now.duration_since(t) < DEAD_NOTIFY_INTERVAL)
        {
            return;
        }
        self.last_dead_notify.set(Some(now));
        let title = format!("herdr-pool: message {} could not be delivered", msg.id);
        let body = format!("to {} — marked dead after repeated failures", msg.to_agent);
        if let Err(err) = self.client.notification_show(&NotificationShowParams {
            title,
            body: Some(body),
            position: None,
            sound: None,
        }) {
            tracing::debug!(error = %err, "dead-letter notification failed");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn backoff_schedule_is_exponential_and_capped() {
        assert_eq!(retry_delay(0), Duration::from_secs(5));
        assert_eq!(retry_delay(1), Duration::from_secs(10));
        assert_eq!(retry_delay(2), Duration::from_secs(20));
        assert_eq!(retry_delay(3), Duration::from_secs(40));
        assert_eq!(retry_delay(6), Duration::from_secs(300));
        assert_eq!(retry_delay(20), Duration::from_secs(300));
        assert_eq!(retry_delay(-3), Duration::from_secs(5));
    }

    #[test]
    fn jittered_stays_within_quarter() {
        let base = retry_delay(0);
        for nanos in [0u128, 7, 12345, u128::MAX - 1] {
            let d = retry_delay_jittered(0, nanos);
            assert!(d >= base && d <= base + base / 4 + Duration::from_millis(1));
        }
    }
}
