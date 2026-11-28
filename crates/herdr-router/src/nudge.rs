//! Unanswered-message reminders ("nudges").
//!
//! Agents sometimes finish delegated work but forget to report back to the
//! peer that asked — the `herdr-pool` instructions live in the skill, and
//! nothing forces a closing message. [`NudgeManager`] closes that gap: on a
//! slow scan it looks for `delivered` messages whose recipient has posted
//! nothing newer in the same thread for [`NUDGE_AFTER`], and sends the
//! recipient a gentle `[herdr-pool] reminder` prompt suggesting they
//! proactively continue the conversation.
//!
//! A nudge is a router-generated prompt — never a stored pool message, so
//! it cannot itself trigger further nudges or pollute threads. Reminders
//! are bounded three ways so a busy recipient is never spammed:
//!
//! - at most [`NUDGE_MAX_PER_MESSAGE`] reminders per message, ever —
//!   persisted in `message_nudges`, so daemon restarts do not reset the
//!   count (ack/FYI mail that legitimately never gets a reply stops being
//!   nudged after the cap instead of every [`NUDGE_REPEAT`] forever);
//! - at most [`NUDGE_PER_SWEEP_PER_RECIPIENT`] reminders per recipient per
//!   sweep, so a large backlog drains gradually instead of arriving as one
//!   burst of prompts;
//! - repeat reminders for the same message are spaced by [`NUDGE_REPEAT`].
//!
//! Delivery honors the same readiness gates as the outbox worker
//! (`interactive_ready`, `launch_pending`, offline, blocked).

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use herdr_pool_store::{AgentRecord, Message, STATUS_OFFLINE};
use herdr_runtime::{AgentRuntime, SendOutcome};

use crate::envelope::nudge_envelope;
use crate::pools::Pools;
use crate::registry_sync::now_millis;

/// How long a delivered message may sit unanswered before the first
/// reminder. `HERDR_POOL_NUDGE_MINUTES` overrides (whole minutes);
/// `HERDR_POOL_NUDGE=0|false|no|off` disables the sweep entirely.
pub const NUDGE_AFTER: Duration = Duration::from_secs(10 * 60);
/// Minimum interval between reminders for the same message.
pub const NUDGE_REPEAT: Duration = Duration::from_secs(30 * 60);
/// How often the sweep scans pool tables (the delivery poll is ~1s; the
/// nudge scan is deliberately much slower — it is a background courtesy,
/// not part of the delivery path).
pub const NUDGE_SCAN_INTERVAL: Duration = Duration::from_secs(60);
/// Maximum reminders sent for one message, ever. `HERDR_POOL_NUDGE_MAX`
/// overrides (whole count; `0` disables nudging entirely).
pub const NUDGE_MAX_PER_MESSAGE: i64 = 3;
/// Maximum reminders delivered to one recipient per sweep — bounds the
/// burst when many threads become eligible at once. `HERDR_POOL_NUDGE_PER_SWEEP`
/// overrides.
pub const NUDGE_PER_SWEEP_PER_RECIPIENT: usize = 2;

fn env_flag_disabled(var: &str) -> bool {
    std::env::var(var)
        .map(|v| {
            matches!(
                v.to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
        })
        .unwrap_or(false)
}

fn env_minutes(var: &str, default: Duration) -> Duration {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|m| *m > 0)
        .map(|m| Duration::from_secs(m.saturating_mul(60)))
        .unwrap_or(default)
}

fn env_usize(var: &str, default: usize) -> usize {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
}

fn env_i64(var: &str, default: i64) -> i64 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(default)
}

/// Periodic unanswered-mail reminder sweeper. Single-threaded by design —
/// owned by the delivery worker thread. Per-message state lives in the
/// pool db (`message_nudges`), not in memory.
pub struct NudgeManager {
    runtime: Arc<dyn AgentRuntime>,
    pools: Arc<Pools>,
    enabled: bool,
    after: Duration,
    repeat: Duration,
    max_per_message: i64,
    per_sweep_per_recipient: usize,
    /// Last scan start; `None` until the first sweep.
    last_scan: Cell<Option<Instant>>,
}

impl NudgeManager {
    pub fn new(runtime: Arc<dyn AgentRuntime>, pools: Arc<Pools>) -> Self {
        Self {
            runtime,
            pools,
            enabled: !env_flag_disabled("HERDR_POOL_NUDGE"),
            after: env_minutes("HERDR_POOL_NUDGE_MINUTES", NUDGE_AFTER),
            repeat: NUDGE_REPEAT,
            max_per_message: env_i64("HERDR_POOL_NUDGE_MAX", NUDGE_MAX_PER_MESSAGE),
            per_sweep_per_recipient: env_usize(
                "HERDR_POOL_NUDGE_PER_SWEEP",
                NUDGE_PER_SWEEP_PER_RECIPIENT,
            ),
            last_scan: Cell::new(None),
        }
    }

    /// Override the unanswered threshold / repeat interval (tests).
    pub fn with_timings(mut self, after: Duration, repeat: Duration) -> Self {
        self.after = after;
        self.repeat = repeat;
        self
    }

    /// Override the per-message cap / per-sweep per-recipient cap (tests).
    pub fn with_limits(mut self, max_per_message: i64, per_sweep: usize) -> Self {
        self.max_per_message = max_per_message;
        self.per_sweep_per_recipient = per_sweep;
        self
    }

    /// Force the next [`Self::sweep`] to run regardless of
    /// [`NUDGE_SCAN_INTERVAL`] (tests).
    pub fn reset_scan_timer(&self) {
        self.last_scan.set(None);
    }

    /// One throttled sweep across every known pool. Cheap no-ops when
    /// disabled, inside the scan interval, or when nothing qualifies.
    pub fn sweep(&self) {
        if !self.enabled || self.max_per_message <= 0 {
            return;
        }
        let now = Instant::now();
        if let Some(last) = self.last_scan.get() {
            if now.duration_since(last) < NUDGE_SCAN_INTERVAL {
                return;
            }
        }
        self.last_scan.set(Some(now));

        let cutoff = now_millis().saturating_sub(self.after.as_millis() as i64);
        for pool in self.pools.all() {
            let messages = pool.messages();
            let candidates = match messages.unanswered_delivered(cutoff) {
                Ok(c) => c,
                Err(err) => {
                    tracing::warn!(scope = %pool.scope_key(), error = %err, "unanswered_delivered failed");
                    continue;
                }
            };
            // Only the newest unanswered message per (thread, recipient)
            // is worth a reminder — older ones are covered by it. Sort
            // oldest-first so the per-recipient sweep cap drains the
            // backlog in a stable order.
            let mut candidates = latest_per_thread(candidates);
            candidates.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));

            // Successful reminders sent per recipient this sweep.
            let mut sent: HashMap<String, usize> = HashMap::new();
            for msg in &candidates {
                if sent.get(&msg.to_agent).copied().unwrap_or(0) >= self.per_sweep_per_recipient {
                    continue;
                }
                if self.maybe_nudge(pool.as_ref(), &messages, msg) {
                    *sent.entry(msg.to_agent.clone()).or_insert(0) += 1;
                }
            }
        }
    }

    /// Send the reminder for one candidate if it is due: under the
    /// per-message cap, past the repeat interval, and the recipient is
    /// ready. Returns `true` when a reminder prompt was delivered.
    fn maybe_nudge(
        &self,
        pool: &herdr_pool_store::PoolStore,
        messages: &herdr_pool_store::MessageStore<'_>,
        msg: &Message,
    ) -> bool {
        let now_ms = now_millis();
        match messages.nudge_state(&msg.id) {
            Ok(Some(state)) => {
                if state.count >= self.max_per_message
                    || now_ms.saturating_sub(state.last_nudge_at) < self.repeat.as_millis() as i64
                {
                    return false;
                }
            }
            Ok(None) => {}
            Err(err) => {
                tracing::debug!(message = %msg.id, error = %err, "nudge_state failed");
                return false;
            }
        }

        let registry = pool.registry();
        let recipient = match registry.get_agent(&msg.to_agent) {
            Ok(Some(r)) => r,
            _ => return false,
        };
        if recipient.status == STATUS_OFFLINE
            || !recipient.interactive_ready
            || recipient.launch_pending
        {
            return false;
        }
        let Some(target) = prompt_target(&recipient) else {
            return false;
        };
        let sender = registry.get_agent(&msg.from_agent).ok().flatten();

        match self
            .runtime
            .send_prompt(&target, &nudge_envelope(msg, sender.as_ref()))
        {
            Ok(SendOutcome::Delivered { .. }) => {
                if let Err(err) = messages.record_nudge(&msg.id, now_ms) {
                    tracing::debug!(message = %msg.id, error = %err, "record_nudge failed");
                }
                tracing::info!(
                    message = %msg.id,
                    to = %msg.to_agent,
                    "sent unanswered-message reminder"
                );
                true
            }
            Ok(SendOutcome::Deferred { .. }) => {
                // Busy or mid-launch: skip quietly; the next scan retries.
                tracing::debug!(message = %msg.id, "nudge deferred");
                false
            }
            Ok(SendOutcome::Failed { message, .. }) => {
                tracing::debug!(message = %msg.id, error = %message, "nudge prompt failed");
                false
            }
            Err(err) => {
                tracing::debug!(message = %msg.id, error = %err, "nudge prompt failed");
                false
            }
        }
    }
}

/// Newest message per `(thread_id, to_agent)` — older unanswered mail in
/// the same thread is covered by the reminder for the latest one.
fn latest_per_thread(messages: Vec<Message>) -> Vec<Message> {
    let mut latest: HashMap<(Option<String>, String), Message> = HashMap::new();
    for msg in messages {
        let key = (msg.thread_id.clone(), msg.to_agent.clone());
        // `id` is uuidv7 — ordering by (created_at, id) breaks millisecond
        // ties the same way insertion order would.
        let older = |a: &Message, b: &Message| (a.created_at, &a.id) < (b.created_at, &b.id);
        match latest.get(&key) {
            Some(existing) if !older(existing, &msg) => {}
            _ => {
                latest.insert(key, msg);
            }
        }
    }
    latest.into_values().collect()
}

/// Same target policy as the delivery worker: `agent.prompt` accepts pane
/// id and agent name but not `terminal_id`, so pane id is preferred.
fn prompt_target(record: &AgentRecord) -> Option<String> {
    record
        .pane_id
        .clone()
        .filter(|p| !p.is_empty())
        .or_else(|| record.name.clone().filter(|n| !n.is_empty()))
        .or_else(|| record.terminal_id.clone().filter(|t| !t.is_empty()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn msg(id: &str, thread: Option<&str>, from: &str, to: &str, created: i64) -> Message {
        Message {
            id: id.into(),
            thread_id: thread.map(str::to_string),
            from_agent: from.into(),
            to_agent: to.into(),
            kind: "message".into(),
            content: "hi".into(),
            status: herdr_pool_store::MessageStatus::Delivered,
            attempt_count: 0,
            next_retry_at: None,
            delivered_at: Some(created),
            acked_at: None,
            created_at: created,
        }
    }

    #[test]
    fn latest_per_thread_keeps_newest_per_recipient() {
        let msgs = vec![
            msg("msg_1", Some("th_1"), "ag_a", "ag_b", 100),
            msg("msg_2", Some("th_1"), "ag_a", "ag_b", 200),
            msg("msg_3", Some("th_1"), "ag_c", "ag_d", 150),
            msg("msg_4", None, "ag_a", "ag_b", 50),
        ];
        let picked = latest_per_thread(msgs);
        assert_eq!(picked.len(), 3);
        assert!(picked.iter().any(|m| m.id == "msg_2"));
        assert!(picked.iter().any(|m| m.id == "msg_3"));
        assert!(picked.iter().any(|m| m.id == "msg_4"));
        assert!(!picked.iter().any(|m| m.id == "msg_1"));
    }
}
