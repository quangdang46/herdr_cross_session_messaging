//! Peer TUI rescue: hand a blocked agent's pane context to a healthy peer.
//!
//! When an agent stays `blocked`, a [`RescueManager`] sweep creates a
//! durable [`RescueJob`] (snapshot of the source pane's visible TUI), picks
//! an eligible helper in the same scope, reserves it, and dispatches a
//! `[HERDR RESCUE REQUEST]` prompt via `agent.prompt`. The helper agent —
//! not the pool — reads the source pane through Herdr (`agent read`,
//! `agent send-keys`, `agent wait`) and unblocks it; the pool only detects,
//! snapshots, dispatches, reserves, and verifies.
//!
//! ## Safety properties
//!
//! - **No rescue loops:** blocked agents are never helper candidates, and
//!   the store's partial unique indexes allow one open job per source and
//!   one per helper — A can never be assigned to rescue B while B rescues A.
//! - **Scope isolation:** helpers are drawn only from the same pool db
//!   (same project scope).
//! - **Durable:** jobs live in SQLite, so a daemon restart resumes pending
//!   dispatches and keeps helper reservations instead of duplicating them.
//! - **Bounded:** open jobs older than [`Self::job_timeout`] (or helpers
//!   that fail dispatch too many times) escalate to `needs_human` with a
//!   user notification rather than retrying forever.
//!
//! The worker calls [`RescueManager::ensure_job`] the moment `agent.prompt`
//! reports `agent_blocked`, and [`RescueManager::sweep_once`] on every
//! delivery poll for detection of agents blocked without pending mail,
//! dispatch, monitoring, and expiry.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use herdr_pool_store::{AgentRecord, PoolStore, RescueJob, RescueStatus, STATUS_OFFLINE};
use herdr_protocol::{
    AgentReadParams, HerdrClient, HerdrError, NotificationShowParams, ReadOptions, ReadSource,
};

use crate::envelope::rescue_envelope;
use crate::error::Result;
use crate::pools::Pools;
use crate::registry_sync::now_millis;

/// Default maximum age of an open rescue job before it escalates to
/// `needs_human` (pending too long = no helper ever came; running too long
/// = the helper could not safely resolve it).
pub const RESCUE_JOB_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Lines requested for the `recent-unwrapped` snapshot fallback.
pub const RESCUE_FALLBACK_LINES: u32 = 120;
/// Dispatch attempts allowed before a pending job escalates to `needs_human`.
pub const RESCUE_MAX_ATTEMPTS: u32 = 5;
/// An `assigned` job older than this is treated as crashed-between-assign-
/// and-prompt and returned to `pending` for redispatch. The assign→prompt
/// window is synchronous, so a persisted `assigned` row only survives a
/// daemon crash; 60s is far longer than that window yet short enough to
/// recover quickly.
pub const RESCUE_ASSIGNED_STALE: Duration = Duration::from_secs(60);
/// Grace period after a helper reports `done` (`verifying`) during which
/// the monitor waits for the source to actually leave `blocked`. Past this,
/// the helper's claim didn't hold → `needs_human`.
pub const RESCUE_VERIFY_TIMEOUT: Duration = Duration::from_secs(120);

/// Rescue coordinator: detection, snapshotting, helper selection,
/// reservation, dispatch, monitoring, and expiry across all scope pools.
pub struct RescueManager {
    client: HerdrClient,
    pools: Arc<Pools>,
    /// Max age of an open job before `needs_human`.
    job_timeout: Duration,
    /// Max dispatch attempts before `needs_human`.
    max_attempts: u32,
    /// Helpers whose dispatch prompt was rejected, per rescue job. The
    /// registry status may lag a `agent_blocked` rejection by a status
    /// event, so without this the same helper would be deterministically
    /// re-picked until the attempt cap fires (R8). In-memory only: a
    /// daemon restart simply forgets the exclusions — the attempt cap
    /// still bounds retries.
    failed_helpers: Mutex<HashMap<String, HashSet<String>>>,
    /// agent_id → last sweep time the agent was seen NOT `blocked`
    /// (offline counts). This is the "new blocked episode" signal that
    /// lifts the needs_human mute in [`Self::ensure_job`]: an agent that
    /// escalated and stayed blocked must not churn a fresh rescue job
    /// every sweep, but one that recovered and blocked again deserves a
    /// new attempt. In-memory only — a restart simply forgets the mute,
    /// costing at most one extra job cycle per stuck agent.
    seen_unblocked_at: Mutex<HashMap<String, i64>>,
    /// Age after which a persisted `assigned` row is treated as
    /// crashed-between-assign-and-prompt (or an ambiguous dispatch whose
    /// helper never claimed) and returned to `pending`.
    assigned_stale: Duration,
}

impl RescueManager {
    pub fn new(client: HerdrClient, pools: Arc<Pools>) -> Self {
        Self {
            client,
            pools,
            job_timeout: RESCUE_JOB_TIMEOUT,
            max_attempts: RESCUE_MAX_ATTEMPTS,
            failed_helpers: Mutex::new(HashMap::new()),
            seen_unblocked_at: Mutex::new(HashMap::new()),
            assigned_stale: RESCUE_ASSIGNED_STALE,
        }
    }

    /// Override the open-job timeout (tests).
    pub fn set_job_timeout(&mut self, timeout: Duration) {
        self.job_timeout = timeout;
    }

    /// Override the dispatch-attempt cap (tests).
    pub fn set_max_attempts(&mut self, max: u32) {
        self.max_attempts = max;
    }

    /// Override the stale-`assigned` threshold (tests).
    pub fn set_assigned_stale(&mut self, stale: Duration) {
        self.assigned_stale = stale;
    }

    /// Ensure an open rescue job exists for `source` (a blocked agent).
    ///
    /// Called by the delivery worker on every `agent_blocked` outcome and
    /// by [`Self::sweep_once`] for agents the registry reports as blocked.
    /// Idempotent: an existing open job for the source is left untouched —
    /// one blocked episode, one job.
    pub fn ensure_job(&self, pool: &PoolStore, source: &AgentRecord) -> Result<()> {
        let rescues = pool.rescues();
        if rescues.open_for_source(&source.agent_id)?.is_some() {
            return Ok(());
        }
        // Same-episode mute: the latest job for this source ended
        // `needs_human` and the agent has not been observed un-blocked
        // since — starting another job now would just re-escalate (and
        // re-toast) every sweep. A `resolved`/`failed` latest job, or an
        // unblocked sighting after the escalation, means a new episode.
        if let Some(latest) = rescues.latest_for_source(&source.agent_id)? {
            if latest.status == RescueStatus::NeedsHuman {
                let muted_at = latest.resolved_at.unwrap_or(latest.updated_at);
                let seen_unblocked = self
                    .seen_unblocked_at
                    .lock()
                    .ok()
                    .and_then(|m| m.get(&source.agent_id).copied())
                    .unwrap_or(0);
                if seen_unblocked <= muted_at {
                    return Ok(());
                }
            }
        }
        // `agent.prompt` just reported `agent_blocked`, but the registry
        // status may still be stale (`idle`/`working`) until the next
        // status event lands. Mark it now so the monitor doesn't resolve
        // the job as "recovered" before it is ever dispatched; RegistrySync
        // corrects the record on the next real status sighting.
        if source.status != "blocked" {
            let mut blocked = source.clone();
            blocked.status = "blocked".to_string();
            blocked.last_seen_at = now_millis();
            if let Err(err) = pool.registry().upsert_agent(&blocked) {
                tracing::debug!(agent = %source.agent_id, error = %err, "rescue: status mark failed");
            }
        }
        let (snapshot, snapshot_source) = source
            .pane_id
            .as_deref()
            .or(source.name.as_deref())
            .or(source.terminal_id.as_deref())
            .map(|t| self.capture_snapshot(t))
            .unwrap_or((None, None));
        let created = rescues.create_pending(
            &source.agent_id,
            source.pane_id.as_deref(),
            snapshot,
            snapshot_source,
        )?;
        if created {
            tracing::info!(
                agent = %source.agent_id,
                "rescue job created for blocked agent"
            );
        }
        Ok(())
    }

    /// One rescue sweep over every pool: detect newly blocked agents,
    /// expire timed-out jobs, resolve recovered sources, and dispatch
    /// pending jobs to eligible helpers. Best-effort per pool/job — one
    /// failure never stalls the rest.
    pub fn sweep_once(&self) -> Result<()> {
        for pool in self.pools.all() {
            if let Err(err) = self.sweep_pool(&pool) {
                tracing::warn!(scope = %pool.scope_key(), error = %err, "rescue sweep failed");
            }
        }
        Ok(())
    }

    fn sweep_pool(&self, pool: &Arc<PoolStore>) -> Result<()> {
        // Include offline records: an offline agent counts as "seen
        // unblocked" so a returning agent starts a fresh episode.
        let all = match pool.registry().list_agents(true) {
            Ok(all) => all,
            Err(err) => {
                tracing::warn!(error = %err, "rescue: registry list failed");
                return Ok(());
            }
        };
        let agents: Vec<AgentRecord> = all
            .iter()
            .filter(|a| a.status != STATUS_OFFLINE)
            .cloned()
            .collect();

        // Record unblocked sightings (the mute-lift signal for ensure_job).
        // Prune to live registry ids so the map can't grow unboundedly.
        let now = now_millis();
        if let Ok(mut seen) = self.seen_unblocked_at.lock() {
            let live: HashSet<&str> = all.iter().map(|a| a.agent_id.as_str()).collect();
            seen.retain(|id, _| live.contains(id.as_str()));
            for agent in all.iter().filter(|a| a.status != "blocked") {
                seen.insert(agent.agent_id.clone(), now);
            }
        }

        // 1. Detect: registry says blocked → ensure a job exists.
        for agent in agents.iter().filter(|a| a.status == "blocked") {
            if let Err(err) = self.ensure_job(pool, agent) {
                tracing::debug!(agent = %agent.agent_id, error = %err, "rescue ensure_job failed");
            }
        }

        // 2. Drive every open job.
        let open = match pool.rescues().list_open() {
            Ok(open) => open,
            Err(err) => {
                tracing::warn!(error = %err, "rescue: list_open failed");
                return Ok(());
            }
        };
        // Drop failure memory for jobs that are no longer open.
        if let Ok(mut failed) = self.failed_helpers.lock() {
            let open_ids: HashSet<&str> = open.iter().map(|j| j.rescue_id.as_str()).collect();
            failed.retain(|id, _| open_ids.contains(id.as_str()));
        }
        // `reserved` is mutated as jobs dispatch within this sweep, so two
        // pending jobs never pick the same helper in one pass (R6).
        let mut reserved: HashSet<String> = open
            .iter()
            .filter_map(|j| j.helper_agent_id.clone())
            .collect();
        let by_id: std::collections::HashMap<&str, &AgentRecord> =
            agents.iter().map(|a| (a.agent_id.as_str(), a)).collect();

        for job in &open {
            if let Err(err) = self.drive_job(pool, job, &agents, &by_id, &mut reserved) {
                tracing::debug!(rescue = %job.rescue_id, error = %err, "rescue drive failed");
            }
        }
        Ok(())
    }

    /// Advance one open job: expire → monitor → dispatch.
    fn drive_job(
        &self,
        pool: &Arc<PoolStore>,
        job: &RescueJob,
        agents: &[AgentRecord],
        by_id: &std::collections::HashMap<&str, &AgentRecord>,
        reserved: &mut HashSet<String>,
    ) -> Result<()> {
        let rescues = pool.rescues();
        let now = now_millis();
        let age = now.saturating_sub(job.created_at);

        // Crash recovery: `assigned` is only persisted between the assign
        // UPDATE and the prompt call — a row still `assigned` after
        // RESCUE_ASSIGNED_STALE means the daemon died mid-dispatch (or the
        // prompt result was lost). Return it to pending for redispatch.
        if job.status == RescueStatus::Assigned
            && now.saturating_sub(job.updated_at) > self.assigned_stale.as_millis() as i64
        {
            tracing::info!(
                rescue = %job.rescue_id,
                "stale assigned rescue returned to pending"
            );
            let _ = rescues.unassign(&job.rescue_id);
            return Ok(());
        }

        // Expiry: open too long, or too many failed dispatches → human.
        if age > self.job_timeout.as_millis() as i64
            || (job.status == RescueStatus::Pending && job.attempts >= self.max_attempts)
        {
            self.finish(pool, job, RescueStatus::NeedsHuman, "rescue job timed out");
            return Ok(());
        }

        // Monitor: source no longer blocked → resolved (helper worked, or
        // the agent recovered on its own — R11).
        match by_id.get(job.source_agent_id.as_str()) {
            Some(source) => {
                if source.status != STATUS_OFFLINE && source.status != "blocked" {
                    // Refresh pane identity before resolving (R9): a moved
                    // pane must not leave a stale id in the record.
                    if source.pane_id.as_deref() != job.source_pane_id.as_deref() {
                        let _ = rescues.update_source(
                            &job.rescue_id,
                            source.pane_id.as_deref(),
                            None,
                            None,
                        );
                    }
                    self.finish(pool, job, RescueStatus::Resolved, "source unblocked");
                    return Ok(());
                }
                if source.pane_id.as_deref() != job.source_pane_id.as_deref() {
                    let _ = rescues.update_source(
                        &job.rescue_id,
                        source.pane_id.as_deref(),
                        None,
                        None,
                    );
                }
            }
            None => {
                // Source vanished from the registry entirely.
                self.finish(pool, job, RescueStatus::Failed, "source agent gone");
                return Ok(());
            }
        }

        // Verifying grace: the helper reported `done` but the source is
        // still blocked — its claim didn't hold.
        if job.status == RescueStatus::Verifying
            && now.saturating_sub(job.updated_at) > RESCUE_VERIFY_TIMEOUT.as_millis() as i64
        {
            self.finish(
                pool,
                job,
                RescueStatus::NeedsHuman,
                "helper reported done but source still blocked",
            );
            return Ok(());
        }

        if job.status != RescueStatus::Pending {
            return Ok(()); // assigned/running/verifying: helper owns it now
        }

        let failed = self
            .failed_helpers
            .lock()
            .ok()
            .and_then(|m| m.get(&job.rescue_id).cloned())
            .unwrap_or_default();
        self.dispatch(pool, job, agents, by_id, reserved, &failed)
    }

    /// Pick a helper and dispatch the rescue prompt. The job stays
    /// `pending` when no eligible helper exists (Case 2 with no free peer).
    fn dispatch(
        &self,
        pool: &Arc<PoolStore>,
        job: &RescueJob,
        agents: &[AgentRecord],
        by_id: &std::collections::HashMap<&str, &AgentRecord>,
        reserved: &mut HashSet<String>,
        failed: &HashSet<String>,
    ) -> Result<()> {
        let Some(source) = by_id.get(job.source_agent_id.as_str()).copied() else {
            return Ok(());
        };
        let Some(helper) = pick_helper(agents, &job.source_agent_id, reserved, failed) else {
            return Ok(()); // stays pending — retried next sweep
        };
        let Some(helper_target) = herdr_target(helper) else {
            return Ok(());
        };

        // Reserve first: the UPDATE is atomic on status='pending', and the
        // helper unique index rejects double-booking. A lost race just
        // means another dispatcher got there first. `assign` also mints
        // the dispatch fencing token carried by the prompt.
        let rescues = pool.rescues();
        let token =
            match rescues.assign(&job.rescue_id, &helper.agent_id, helper.pane_id.as_deref()) {
                Ok(Some(token)) => token,
                Ok(None) => return Ok(()),
                Err(err) => {
                    tracing::debug!(rescue = %job.rescue_id, error = %err, "rescue assign failed");
                    return Ok(());
                }
            };

        // Rebuild the job view with the reservation + token for the prompt.
        let mut prompt_job = job.clone();
        prompt_job.status = RescueStatus::Assigned;
        prompt_job.helper_agent_id = Some(helper.agent_id.clone());
        prompt_job.helper_pane_id = helper.pane_id.clone();
        prompt_job.dispatch_token = Some(token);
        let text = rescue_envelope(&prompt_job, source);

        match self.client.agent_prompt(&helper_target, &text, None) {
            Ok(_) => {
                let _ = rescues.set_status(&job.rescue_id, RescueStatus::Running);
                // Hold the reservation for the rest of this sweep so a
                // second pending job picks a different helper (R6).
                reserved.insert(helper.agent_id.clone());
                tracing::info!(
                    rescue = %job.rescue_id,
                    source = %job.source_agent_id,
                    helper = %helper.agent_id,
                    "rescue dispatched to peer helper"
                );
            }
            Err(err) if dispatch_outcome_ambiguous(&err) => {
                // At-least-once hazard: the prompt may have reached Herdr
                // (and the helper) even though we saw an error. Do NOT
                // unassign — that would let a second helper be dispatched
                // while the first may already be working the pane. Keep
                // the reservation; if the helper really got the prompt it
                // claims/reports with this token, otherwise the stale-
                // assigned recovery returns the job to pending and the
                // NEXT assign mints a fresh token that fences this helper
                // out of claim/report.
                tracing::warn!(
                    rescue = %job.rescue_id,
                    helper = %helper.agent_id,
                    error = %err,
                    "rescue dispatch outcome ambiguous; holding assignment"
                );
            }
            Err(err) => {
                // Definitive rejection (agent_blocked, not_found, ...):
                // the prompt certainly did not land. Release the
                // reservation so another helper (or a later sweep) can try,
                // and exclude this helper for the job — the registry status
                // may still look eligible until the blocked event lands.
                let _ = rescues.unassign(&job.rescue_id);
                if let Ok(mut failed_map) = self.failed_helpers.lock() {
                    failed_map
                        .entry(job.rescue_id.clone())
                        .or_default()
                        .insert(helper.agent_id.clone());
                }
                tracing::debug!(
                    rescue = %job.rescue_id,
                    helper = %helper.agent_id,
                    error = %err,
                    "rescue dispatch failed; released helper"
                );
            }
        }
        Ok(())
    }

    /// Terminal transition + user notification for `needs_human`.
    fn finish(&self, pool: &Arc<PoolStore>, job: &RescueJob, status: RescueStatus, why: &str) {
        if let Err(err) = pool.rescues().set_status(&job.rescue_id, status) {
            tracing::debug!(rescue = %job.rescue_id, error = %err, "rescue set_status failed");
            return;
        }
        tracing::warn!(
            rescue = %job.rescue_id,
            source = %job.source_agent_id,
            status = ?status,
            "{why}"
        );
        if status == RescueStatus::NeedsHuman {
            self.notify_needs_human(pool, job);
        }
    }

    fn notify_needs_human(&self, pool: &Arc<PoolStore>, job: &RescueJob) {
        let who = pool
            .registry()
            .get_agent(&job.source_agent_id)
            .ok()
            .flatten()
            .and_then(|a| a.name.or(Some(a.agent_id)))
            .unwrap_or_else(|| job.source_agent_id.clone());
        let _ = self.client.notification_show(&NotificationShowParams {
            title: format!("herdr-pool: agent {who} needs help"),
            body: Some(format!(
                "{} has been blocked and no peer helper could resolve it — please check the pane",
                job.source_agent_id
            )),
            position: None,
            sound: None,
        });
    }

    /// Capture the source pane's TUI for handoff. Tries `visible` first
    /// (full viewport, no line cap); falls back to `recent-unwrapped` tail
    /// history. Returns `(text, source_label)`; `(None, None)` when both
    /// reads fail — the job is still created, just without a snapshot.
    fn capture_snapshot(&self, target: &str) -> (Option<String>, Option<String>) {
        let visible = self.client.agent_read(&AgentReadParams {
            target: target.to_string(),
            source: ReadSource::Visible,
            options: ReadOptions::default(),
        });
        if let Ok(result) = visible {
            return (Some(result.text), Some("visible".to_string()));
        }
        let recent = self.client.agent_read(&AgentReadParams {
            target: target.to_string(),
            source: ReadSource::RecentUnwrapped,
            options: ReadOptions {
                lines: Some(RESCUE_FALLBACK_LINES),
                ..ReadOptions::default()
            },
        });
        match recent {
            Ok(result) => (Some(result.text), Some("recent-unwrapped".to_string())),
            Err(_) => (None, None),
        }
    }
}

/// Whether an `agent_prompt` failure leaves the delivery outcome
/// *ambiguous* — the request may have reached Herdr and the helper even
/// though no response came back (timeout, dropped connection, malformed
/// reply, daemon crash mid-flight). For these the job must keep its
/// assignment so the fencing token stays valid for a helper that did
/// receive the prompt; definitive API rejections (`agent_blocked`,
/// `agent_not_found`, ...) prove nothing was sent and release instead.
fn dispatch_outcome_ambiguous(err: &HerdrError) -> bool {
    err.is_timeout()
        || matches!(
            err,
            HerdrError::Closed | HerdrError::Transport(_) | HerdrError::Protocol(_)
        )
}

/// Herdr-facing target for a registry record: pane id first (kept current
/// by `pane.move` rekeys), then name, then terminal id — the same order the
/// delivery worker uses. Never the pool-internal `ag_...` id.
fn herdr_target(agent: &AgentRecord) -> Option<String> {
    agent
        .pane_id
        .clone()
        .filter(|p| !p.is_empty())
        .or_else(|| agent.name.clone().filter(|n| !n.is_empty()))
        .or_else(|| agent.terminal_id.clone().filter(|t| !t.is_empty()))
}

/// Rank a helper candidate: lower is better. `idle` first, `done` next,
/// `working` last (a busy agent can still take a prompt, but an idle peer
/// is the safer choice). Everything else is ineligible.
fn helper_rank(status: &str) -> Option<u8> {
    match status {
        "idle" => Some(0),
        "done" => Some(1),
        "working" => Some(2),
        _ => None,
    }
}

/// Choose the best eligible helper for `source_agent_id`.
///
/// Excludes: the source itself, offline agents (already filtered by
/// `list_agents(false)` but re-checked), blocked/unknown-status agents,
/// agents whose TUI is not interactive-ready or still launching (the
/// rescue prompt would land in a half-restored session), agents with no
/// Herdr target, helpers already holding a reservation (`reserved`), and
/// helpers whose dispatch already failed for this job (`failed`).
/// Deterministic: status rank, then agent_id — never the raw
/// `list_agents` order.
fn pick_helper<'a>(
    agents: &'a [AgentRecord],
    source_agent_id: &str,
    reserved: &HashSet<String>,
    failed: &HashSet<String>,
) -> Option<&'a AgentRecord> {
    agents
        .iter()
        .filter(|a| a.agent_id != source_agent_id)
        .filter(|a| a.status != STATUS_OFFLINE)
        .filter(|a| a.interactive_ready && !a.launch_pending)
        .filter(|a| !reserved.contains(&a.agent_id))
        .filter(|a| !failed.contains(&a.agent_id))
        .filter(|a| herdr_target(a).is_some())
        .filter_map(|a| helper_rank(&a.status).map(|rank| (rank, a)))
        .min_by(|(ra, a), (rb, b)| ra.cmp(rb).then_with(|| a.agent_id.cmp(&b.agent_id)))
        .map(|(_, a)| a)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn agent(id: &str, status: &str) -> AgentRecord {
        AgentRecord {
            agent_id: id.into(),
            scope_key: "s".into(),
            terminal_id: Some(format!("term_{id}")),
            pane_id: Some(format!("w:p_{id}")),
            agent_kind: Some("claude".into()),
            name: None,
            cwd: None,
            status: status.into(),
            interactive_ready: true,
            launch_pending: false,
            bootstrap_version: None,
            first_seen_at: 0,
            last_seen_at: 0,
            offline_at: None,
        }
    }

    #[test]
    fn pick_helper_prefers_idle_over_working() {
        let agents = vec![
            agent("ag_work", "working"),
            agent("ag_idle", "idle"),
            agent("ag_done", "done"),
        ];
        let reserved = HashSet::new();
        let failed = HashSet::new();
        let pick = pick_helper(&agents, "ag_src", &reserved, &failed).unwrap();
        assert_eq!(pick.agent_id, "ag_idle");
    }

    #[test]
    fn pick_helper_never_returns_blocked_or_source() {
        let agents = vec![
            agent("ag_src", "blocked"),
            agent("ag_b", "blocked"),
            agent("ag_c", "idle"),
        ];
        let reserved = HashSet::new();
        let failed = HashSet::new();
        let pick = pick_helper(&agents, "ag_src", &reserved, &failed).unwrap();
        assert_eq!(pick.agent_id, "ag_c");
    }

    #[test]
    fn pick_helper_skips_reserved_and_offline() {
        let agents = vec![
            agent("ag_r", "idle"),
            agent("ag_off", "idle"),
            agent("ag_free", "working"),
        ];
        let mut agents = agents;
        agents[1].status = STATUS_OFFLINE.into();
        let reserved: HashSet<String> = ["ag_r".to_string()].into_iter().collect();
        let failed = HashSet::new();
        let pick = pick_helper(&agents, "ag_src", &reserved, &failed).unwrap();
        assert_eq!(pick.agent_id, "ag_free");
    }

    #[test]
    fn pick_helper_skips_failed_helpers() {
        let agents = vec![agent("ag_bad", "idle"), agent("ag_good", "idle")];
        let reserved = HashSet::new();
        let failed: HashSet<String> = ["ag_bad".to_string()].into_iter().collect();
        let pick = pick_helper(&agents, "ag_src", &reserved, &failed).unwrap();
        assert_eq!(pick.agent_id, "ag_good");
        // With every candidate failed for this job, nothing qualifies.
        let failed: HashSet<String> = ["ag_bad".to_string(), "ag_good".to_string()]
            .into_iter()
            .collect();
        assert!(pick_helper(&agents, "ag_src", &reserved, &failed).is_none());
    }

    #[test]
    fn pick_helper_returns_none_when_all_blocked() {
        let agents = vec![agent("ag_a", "blocked"), agent("ag_b", "blocked")];
        let reserved = HashSet::new();
        let failed = HashSet::new();
        assert!(pick_helper(&agents, "ag_a", &reserved, &failed).is_none());
        assert!(pick_helper(&agents, "ag_b", &reserved, &failed).is_none());
    }

    #[test]
    fn pick_helper_skips_not_ready_agents() {
        // A helper whose TUI is still launching/resuming cannot receive the
        // rescue prompt — it is skipped even when idle.
        let agents = vec![
            agent("ag_resuming", "idle"),
            agent("ag_pending", "idle"),
            agent("ag_ready", "idle"),
        ];
        let mut agents = agents;
        agents[0].interactive_ready = false;
        agents[1].launch_pending = true;
        let reserved = HashSet::new();
        let failed = HashSet::new();
        let pick = pick_helper(&agents, "ag_src", &reserved, &failed).unwrap();
        assert_eq!(pick.agent_id, "ag_ready");
    }

    #[test]
    fn pick_helper_deterministic_on_ties() {
        let agents = vec![agent("ag_z", "idle"), agent("ag_a", "idle")];
        let reserved = HashSet::new();
        let failed = HashSet::new();
        let pick = pick_helper(&agents, "ag_src", &reserved, &failed).unwrap();
        assert_eq!(pick.agent_id, "ag_a");
    }
}
