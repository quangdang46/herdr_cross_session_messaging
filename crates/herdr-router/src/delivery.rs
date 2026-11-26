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
//! | `agent_blocked` | `defer(now + 15s)` — harness queues nothing; retry shortly, no attempt consumed; throttled pane snapshot + `notification.show` rescue notice |
//! | `agent_not_ready` / `agent_not_found` / `agent_target_ambiguous` / stalled | `record_failure(now + backoff(attempts))` |
//! | transport/timeout/other | `record_failure(now + backoff(attempts))` |
//! | recipient offline > [`OFFLINE_DEAD_AFTER`] | dead-letter immediately + throttled `notification.show` |
//!
//! `record_failure` dead-letters at [`MAX_DELIVERY_ATTEMPTS`] attempts.
//! Backoff is exponential: 5s × 2^attempts capped at 5 minutes, plus jitter.

use std::sync::Arc;
use std::time::Duration;

use herdr_pool_store::{Message, MessageStatus, MessageStore, PoolStore, STATUS_OFFLINE};
use herdr_protocol::{
    AgentReadParams, HerdrClient, HerdrError, NotificationShowParams, ReadOptions, ReadSource,
};

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
/// Minimum interval between blocked-agent rescue notices per recipient.
pub const BLOCKED_NOTIFY_INTERVAL: Duration = Duration::from_secs(120);
/// Lines of pane output captured for the blocked-agent snapshot.
pub const BLOCKED_SNAPSHOT_LINES: u32 = 30;
/// How long a recipient may stay `agent_blocked` before the daemon asks a
/// peer agent in the same scope to check on it (rescue escalation).
pub const BLOCKED_ESCALATE_AFTER: Duration = Duration::from_secs(5 * 60);
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
    /// Last rescue notice per recipient agent_id (throttle).
    last_blocked_notify: std::cell::RefCell<std::collections::HashMap<String, std::time::Instant>>,
    /// First `agent_blocked` defer seen per recipient (escalation timer).
    blocked_since: std::cell::RefCell<std::collections::HashMap<String, std::time::Instant>>,
    /// Recipients already escalated to a peer helper this blocked episode.
    blocked_escalated: std::cell::RefCell<std::collections::HashSet<String>>,
    /// Blocked duration that triggers peer-assist escalation.
    blocked_escalate_after: Duration,
    /// Toast the user on every successful delivery (`HERDR_POOL_NOTIFY_DELIVERY`).
    notify_on_delivery: bool,
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
            last_blocked_notify: std::cell::RefCell::new(std::collections::HashMap::new()),
            blocked_since: std::cell::RefCell::new(std::collections::HashMap::new()),
            blocked_escalated: std::cell::RefCell::new(std::collections::HashSet::new()),
            blocked_escalate_after: BLOCKED_ESCALATE_AFTER,
            notify_on_delivery: false,
        }
    }

    /// Override the blocked-escalation delay (tests).
    pub fn with_blocked_escalate_after(mut self, delay: Duration) -> Self {
        self.blocked_escalate_after = delay;
        self
    }

    /// Toast the user on each successful delivery (opt-in via env).
    pub fn with_notify_on_delivery(mut self, enabled: bool) -> Self {
        self.notify_on_delivery = enabled;
        self
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

        // Resolve the CURRENT target on every attempt. `agent.prompt`
        // accepts pane_id and agent name but NOT terminal_id (verified:
        // term_* → agent_not_found), so pane_id is preferred; it is kept
        // current by the registry's pane.moved rekey. Name is the fallback
        // for records captured before pane_id was known; terminal_id last.
        let target = recipient
            .pane_id
            .clone()
            .filter(|p| !p.is_empty())
            .or_else(|| recipient.name.clone().filter(|n| !n.is_empty()))
            .or_else(|| recipient.terminal_id.clone().filter(|t| !t.is_empty()));
        let Some(target) = target else {
            return self.record_retry(messages, msg, retry_delay(msg.attempt_count));
        };

        let sender = registry.get_agent(&msg.from_agent).ok().flatten();
        let text = message_envelope(msg, sender.as_ref());

        match self.client.agent_prompt(&target, &text, None) {
            Ok(_) => {
                messages.mark_delivered(&msg.id)?;
                self.clear_blocked_episode(&msg.to_agent);
                tracing::info!(message = %msg.id, to = %msg.to_agent, "message delivered");
                if self.notify_on_delivery {
                    self.notify_delivered(&recipient, sender.as_ref(), msg);
                }
            }
            Err(err) if err.is_agent_blocked() => {
                // Agent is alive but busy: nothing was sent. Defer without
                // consuming an attempt — blocking is not a delivery
                // failure, and dead-lettering busy agents' mail would lose
                // it invisibly (dead rows never reach the inbox).
                self.note_blocked(&msg.to_agent);
                self.defer_retry(messages, msg, BLOCKED_BACKOFF)?;
                self.maybe_rescue_blocked(&recipient, &target);
                self.maybe_escalate_blocked(pool, messages, &recipient, &target);
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

    /// Record the start of a blocked episode for escalation timing.
    fn note_blocked(&self, agent_id: &str) {
        self.blocked_since
            .borrow_mut()
            .entry(agent_id.to_string())
            .or_insert_with(std::time::Instant::now);
    }

    /// A blocked episode ends when mail delivers or the agent goes offline.
    fn clear_blocked_episode(&self, agent_id: &str) {
        self.blocked_since.borrow_mut().remove(agent_id);
        self.blocked_escalated.borrow_mut().remove(agent_id);
    }

    /// Peer-assist escalation: once a recipient has been `agent_blocked` for
    /// [`Self::blocked_escalate_after`], prompt another live agent in the
    /// same scope to check on it. Once per blocked episode; best-effort.
    fn maybe_escalate_blocked(
        &self,
        pool: &PoolStore,
        messages: &MessageStore<'_>,
        recipient: &herdr_pool_store::AgentRecord,
        target: &str,
    ) {
        if self
            .blocked_escalated
            .borrow()
            .contains(&recipient.agent_id)
        {
            return;
        }
        let blocked_for = self
            .blocked_since
            .borrow()
            .get(&recipient.agent_id)
            .map(|t| t.elapsed())
            .unwrap_or_default();
        if blocked_for < self.blocked_escalate_after {
            return;
        }

        // Any other live agent in this scope can assist.
        let helper = match pool.registry().list_agents(false) {
            Ok(agents) => agents
                .into_iter()
                .find(|a| a.agent_id != recipient.agent_id),
            Err(err) => {
                tracing::debug!(error = %err, "escalation: registry list failed");
                return;
            }
        };
        let Some(helper) = helper else {
            return; // no peer to ask — retry on the next blocked defer
        };
        let helper_target = helper
            .pane_id
            .clone()
            .filter(|p| !p.is_empty())
            .or_else(|| helper.name.clone().filter(|n| !n.is_empty()))
            .or_else(|| helper.terminal_id.clone().filter(|t| !t.is_empty()));
        let Some(helper_target) = helper_target else {
            return;
        };

        let queued = messages
            .inbox(&recipient.agent_id, true)
            .map(|m| m.len())
            .unwrap_or(0);
        let snapshot = self.pane_snapshot(target);
        let who = recipient
            .name
            .clone()
            .unwrap_or_else(|| recipient.agent_id.clone());
        let text = format!(
            "[herdr-pool] rescue assist: agent {who} ({}) has been blocked for \
             over {}s with {queued} message(s) queued in the pool.\n\
             Pane snapshot tail:\n{snapshot}\n\
             Please check on it (`herdr agent explain {}` or `herdr-pool peers`) \
             and if it needs the user's attention, surface that to the user. \
             Do not send keys or interrupt the agent yourself.",
            recipient.agent_id,
            blocked_for.as_secs(),
            recipient.pane_id.as_deref().unwrap_or("?"),
        );

        match self.client.agent_prompt(&helper_target, &text, None) {
            Ok(_) => {
                self.blocked_escalated
                    .borrow_mut()
                    .insert(recipient.agent_id.clone());
                tracing::info!(
                    agent = %recipient.agent_id,
                    helper = %helper.agent_id,
                    "blocked agent escalated to peer assist"
                );
            }
            Err(err) => {
                tracing::debug!(error = %err, "rescue escalation prompt failed");
            }
        }
    }

    /// Last lines of a pane's recent output for rescue context.
    fn pane_snapshot(&self, target: &str) -> String {
        match self.client.agent_read(&AgentReadParams {
            target: target.to_string(),
            source: ReadSource::Recent,
            options: ReadOptions {
                lines: Some(BLOCKED_SNAPSHOT_LINES),
                ..ReadOptions::default()
            },
        }) {
            Ok(snapshot) => {
                let tail: Vec<&str> = snapshot.text.lines().rev().take(15).collect();
                tail.into_iter().rev().collect::<Vec<_>>().join("\n")
            }
            Err(_) => "(snapshot unavailable)".to_string(),
        }
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
            self.clear_blocked_episode(&msg.to_agent);
            return Ok(());
        }
        // While offline, poll no faster than once a minute.
        let delay = retry_delay(msg.attempt_count).max(Duration::from_secs(60));
        self.record_retry(messages, msg, delay)
    }

    /// Defer a message without counting a delivery attempt (blocked path).
    fn defer_retry(
        &self,
        messages: &MessageStore<'_>,
        msg: &Message,
        delay: Duration,
    ) -> Result<()> {
        let next = now_millis() + delay.as_millis() as i64;
        messages.defer(&msg.id, Some(next))?;
        Ok(())
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

    /// Rescue notice for a blocked recipient, throttled per agent: capture a
    /// pane snapshot into the daemon log and toast the user that mail is
    /// queued behind the block. Best-effort — failures only log.
    fn maybe_rescue_blocked(&self, recipient: &herdr_pool_store::AgentRecord, target: &str) {
        let now = std::time::Instant::now();
        {
            let mut last = self.last_blocked_notify.borrow_mut();
            if last
                .get(&recipient.agent_id)
                .is_some_and(|t| now.duration_since(*t) < BLOCKED_NOTIFY_INTERVAL)
            {
                return;
            }
            last.insert(recipient.agent_id.clone(), now);
        }

        let waiting = recipient
            .name
            .clone()
            .unwrap_or_else(|| recipient.agent_id.clone());
        let snapshot = self.pane_snapshot(target);
        tracing::warn!(
            agent = %recipient.agent_id,
            "agent blocked with queued mail; pane snapshot tail:\n{snapshot}"
        );

        let title = format!("herdr-pool: agent {waiting} is blocked");
        let body = format!(
            "{} — messages are queued and will deliver when it unblocks",
            recipient.agent_id
        );
        if let Err(err) = self.client.notification_show(&NotificationShowParams {
            title,
            body: Some(body),
            position: None,
            sound: None,
        }) {
            tracing::debug!(error = %err, "blocked-agent notification failed");
        }
    }

    /// Opt-in `notification.show` toast for a delivered message.
    fn notify_delivered(
        &self,
        recipient: &herdr_pool_store::AgentRecord,
        sender: Option<&herdr_pool_store::AgentRecord>,
        msg: &Message,
    ) {
        let to = recipient
            .name
            .clone()
            .unwrap_or_else(|| recipient.agent_id.clone());
        let from = sender
            .and_then(|s| s.name.clone())
            .unwrap_or_else(|| msg.from_agent.clone());
        let snippet: String = msg.content.chars().take(80).collect();
        if let Err(err) = self.client.notification_show(&NotificationShowParams {
            title: format!("herdr-pool: new message for {to}"),
            body: Some(format!("from {from}: {snippet}")),
            position: None,
            sound: None,
        }) {
            tracing::debug!(error = %err, "delivery notification failed");
        }
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
