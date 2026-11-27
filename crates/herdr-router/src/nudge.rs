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
//! it cannot itself trigger further nudges or pollute threads. Per-message
//! reminders are throttled to one per [`NUDGE_REPEAT`] per daemon run;
//! delivery honors the same readiness gates as the outbox worker
//! (`interactive_ready`, `launch_pending`, offline, blocked).

use std::cell::{Cell, RefCell};
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
/// Nudged-message bookkeeping is pruned past this age so the map cannot
/// grow without bound over a long daemon run.
const NUDGE_BOOKKEEPING_TTL: Duration = Duration::from_secs(24 * 60 * 60);

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

/// Periodic unanswered-mail reminder sweeper. Single-threaded by design —
/// owned by the delivery worker thread.
pub struct NudgeManager {
    runtime: Arc<dyn AgentRuntime>,
    pools: Arc<Pools>,
    enabled: bool,
    after: Duration,
    repeat: Duration,
    /// Last scan start; `None` until the first sweep.
    last_scan: Cell<Option<Instant>>,
    /// `scope_key/msg_id` → last nudge time (per daemon run).
    nudged: RefCell<HashMap<String, Instant>>,
}

impl NudgeManager {
    pub fn new(runtime: Arc<dyn AgentRuntime>, pools: Arc<Pools>) -> Self {
        Self {
            runtime,
            pools,
            enabled: !env_flag_disabled("HERDR_POOL_NUDGE"),
            after: env_minutes("HERDR_POOL_NUDGE_MINUTES", NUDGE_AFTER),
            repeat: NUDGE_REPEAT,
            last_scan: Cell::new(None),
            nudged: RefCell::new(HashMap::new()),
        }
    }

    /// Override the unanswered threshold / repeat interval (tests).
    pub fn with_timings(mut self, after: Duration, repeat: Duration) -> Self {
        self.after = after;
        self.repeat = repeat;
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
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        if let Some(last) = self.last_scan.get() {
            if now.duration_since(last) < NUDGE_SCAN_INTERVAL {
                return;
            }
        }
        self.last_scan.set(Some(now));
        self.prune_bookkeeping(now);

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
            // is worth a reminder — older ones are covered by it.
            for msg in latest_per_thread(candidates) {
                self.maybe_nudge(pool.as_ref(), &msg, now);
            }
        }
    }

    /// Send the reminder for one candidate, honoring the per-message
    /// throttle and the recipient's current readiness.
    fn maybe_nudge(&self, pool: &herdr_pool_store::PoolStore, msg: &Message, now: Instant) {
        let key = format!("{}/{}", pool.scope_key(), msg.id);
        {
            let nudged = self.nudged.borrow();
            if nudged
                .get(&key)
                .is_some_and(|t| now.duration_since(*t) < self.repeat)
            {
                return;
            }
        }

        let registry = pool.registry();
        let recipient = match registry.get_agent(&msg.to_agent) {
            Ok(Some(r)) => r,
            _ => return,
        };
        if recipient.status == STATUS_OFFLINE
            || !recipient.interactive_ready
            || recipient.launch_pending
        {
            return;
        }
        let Some(target) = prompt_target(&recipient) else {
            return;
        };
        let sender = registry.get_agent(&msg.from_agent).ok().flatten();

        match self
            .runtime
            .send_prompt(&target, &nudge_envelope(msg, sender.as_ref()))
        {
            Ok(SendOutcome::Delivered { .. }) => {
                self.nudged.borrow_mut().insert(key, now);
                tracing::info!(
                    message = %msg.id,
                    to = %msg.to_agent,
                    "sent unanswered-message reminder"
                );
            }
            Ok(SendOutcome::Deferred { .. }) => {
                // Busy or mid-launch: skip quietly; the next scan retries.
                tracing::debug!(message = %msg.id, "nudge deferred");
            }
            Ok(SendOutcome::Failed { message, .. }) => {
                tracing::debug!(message = %msg.id, error = %message, "nudge prompt failed");
            }
            Err(err) => {
                tracing::debug!(message = %msg.id, error = %err, "nudge prompt failed");
            }
        }
    }

    /// Drop bookkeeping for messages older than [`NUDGE_BOOKKEEPING_TTL`].
    fn prune_bookkeeping(&self, now: Instant) {
        let mut nudged = self.nudged.borrow_mut();
        nudged.retain(|_, t| now.duration_since(*t) < NUDGE_BOOKKEEPING_TTL);
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
