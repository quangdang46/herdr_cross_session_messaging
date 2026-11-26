//! Registry synchronization: Herdr state → per-scope agent registries.
//!
//! [`RegistrySync`] is the single writer of registry state in the daemon.
//! It is fed by two sources:
//!
//! * **Bootstrap** — [`RegistrySync::sync_snapshot`] walks
//!   `session.snapshot().agents`, resolves each agent's project scope, and
//!   upserts an [`AgentRecord`] (reusing the stored `agent_id` keyed by
//!   `terminal_id`). [`RegistrySync::reconcile`] does the same via
//!   `agent.list` and additionally marks agents that vanished as offline.
//! * **Events** — [`RegistrySync::handle_frame`] maps subscription frames
//!   (`pane.agent_detected`, `pane.updated`, `pane.moved`, `pane.closed`,
//!   `pane.exited`, `pane.created`, `workspace.closed`, `tab.closed`, and
//!   per-pane `pane.agent_status_changed` subscription events) onto the
//!   same upsert/offline/rekey operations.
//!
//! ## Bootstrap notifier
//!
//! [`Bootstrapper`] sends the one-shot `[herdr-pool] bootstrap` prompt the
//! first time an agent needs it (`bootstrap_version < BOOTSTRAP_VERSION`).
//! Failures (`agent_blocked`, `agent_not_ready`, transport) are retried on
//! later sightings, capped at [`Bootstrapper::MAX_ATTEMPTS_PER_RUN`] prompt
//! attempts per agent per daemon run — the skill files remain the durable
//! channel. Success persists `bootstrap_version` in the agents table, so
//! restarts and `tick` runs do not re-send.
//!
//! ## Watched-pane subscriptions
//!
//! `pane.agent_status_changed` subscriptions *require* a concrete
//! `pane_id` server-side (verified against upstream `src/api/schema/events.rs`)
//! — there is no global status subscription. [`RegistrySync::watched_panes`]
//! therefore exposes the set of live agent pane ids; the event loop folds
//! them into `events.subscribe` and reconnects when the set changes
//! ([`RegistrySync::subscriptions_changed`]).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use herdr_pool_core::{resolve_scope_for_agent, ResolvedScope};
use herdr_pool_store::{new_agent_id, AgentRecord, PoolStore, STATUS_OFFLINE};
use herdr_protocol::{
    AgentInfo, AgentStatus, EventData, HerdrClient, PaneInfo, SubscriptionEventData,
    SubscriptionFrame,
};

use crate::envelope::{bootstrap_text, BOOTSTRAP_VERSION};
use crate::error::Result;
use crate::pools::Pools;

/// Unix epoch milliseconds (matches the store's timestamp convention).
pub(crate) fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Automatic pane labeling is on by default; `HERDR_POOL_PANE_LABEL` set to
/// `0`/`false`/`no`/`off` (any case) disables it. Resolved once per process.
fn pane_label_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("HERDR_POOL_PANE_LABEL")
            .map(|v| {
                !matches!(
                    v.to_ascii_lowercase().as_str(),
                    "0" | "false" | "no" | "off"
                )
            })
            .unwrap_or(true)
    })
}

/// Status-change nudges for the delivery worker.
///
/// Cheap `Arc`-shared condvar; the delivery thread waits on it with a poll
/// timeout, the event loop rings it when an agent transitions to a
/// promptable-ish state (`idle`/`working`) or after any registry mutation
/// that could unblock delivery.
#[derive(Debug, Clone, Default)]
pub struct Notify {
    inner: Arc<(std::sync::Mutex<()>, std::sync::Condvar)>,
}

impl Notify {
    /// Wake all current waiters (they re-check their predicate and go back
    /// to sleep if nothing changed).
    pub fn notify(&self) {
        let _guard = self.inner.0.lock().unwrap_or_else(|e| e.into_inner());
        self.inner.1.notify_all();
    }

    /// The raw pair for `wait_timeout` loops.
    pub(crate) fn pair(&self) -> Arc<(std::sync::Mutex<()>, std::sync::Condvar)> {
        Arc::clone(&self.inner)
    }
}

/// What a dispatched frame changed, for the event loop's bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Nothing the delivery worker or subscription set cares about.
    Quiet,
    /// Registry state changed; delivery should be nudged.
    RegistryChanged,
    /// The watched-pane set may have changed; resubscribe.
    Resubscribe,
}

/// Registry sync + bootstrap engine. Single-threaded by design: the daemon
/// event loop owns the only instance.
pub struct RegistrySync<'a> {
    client: &'a HerdrClient,
    pools: &'a Pools,
    bootstrap: Bootstrapper<'a>,
    /// Pane ids currently covered by `pane.agent_status_changed`
    /// subscriptions (rebuilt by [`Self::refresh_watched`]).
    watched: BTreeSet<String>,
    /// Set when `watched` differs from the last subscribed set.
    subs_dirty: bool,
    /// Whether any agent transitioned to a promptable state recently —
    /// surfaced so the event loop can nudge the delivery worker.
    delivery_nudge: bool,
    /// Pane ids already checked for auto-labeling this run — bounds the
    /// extra `pane.get` calls to once per pane per daemon run.
    label_checked: HashSet<String>,
}

impl<'a> RegistrySync<'a> {
    pub fn new(client: &'a HerdrClient, pools: &'a Pools, bootstrap_enabled: bool) -> Self {
        Self {
            client,
            pools,
            bootstrap: Bootstrapper::new(client, bootstrap_enabled),
            watched: BTreeSet::new(),
            subs_dirty: false,
            delivery_nudge: false,
            label_checked: HashSet::new(),
        }
    }

    /// Pane ids the event loop should subscribe `pane.agent_status_changed`
    /// for. Empty until the first [`Self::refresh_watched`].
    pub fn watched_panes(&self) -> &BTreeSet<String> {
        &self.watched
    }

    /// True when the watched-pane set changed since the last check — the
    /// event loop should tear down and re-issue `events.subscribe`.
    pub fn subscriptions_changed(&self) -> bool {
        self.subs_dirty
    }

    /// Clear the dirty flag after a successful (re)subscribe — the new
    /// subscription set already reflects `watched_panes`.
    pub fn acknowledge_subscriptions(&mut self) {
        self.subs_dirty = false;
    }

    /// Take (and clear) the delivery-nudge flag.
    pub fn take_delivery_nudge(&mut self) -> bool {
        std::mem::take(&mut self.delivery_nudge)
    }

    /// Rebuild the watched-pane set from every live registry record.
    /// Marks `subs_dirty` when the set actually changed.
    pub fn refresh_watched(&mut self) {
        let mut next = BTreeSet::new();
        for pool in self.pools.all() {
            let registry = pool.registry();
            match registry.list_agents(false) {
                Ok(agents) => {
                    for agent in agents {
                        if let Some(pane) = agent.pane_id {
                            if !pane.is_empty() {
                                next.insert(pane);
                            }
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(scope = %pool.scope_key(), error = %err, "list_agents failed");
                }
            }
        }
        if next != self.watched {
            tracing::debug!(
                before = self.watched.len(),
                after = next.len(),
                "watched pane set changed; resubscribing"
            );
            self.watched = next;
            self.subs_dirty = true;
        }
    }

    /// Full bootstrap pass: `session.snapshot` → upsert every reported agent.
    ///
    /// This is the daemon's Resync entry point and what `tick` runs.
    pub fn sync_snapshot(&mut self) -> Result<usize> {
        let snapshot = self.client.session_snapshot()?;
        let mut count = 0;
        for agent in &snapshot.agents {
            match self.upsert_agent(agent) {
                Ok(()) => count += 1,
                Err(err) => {
                    tracing::debug!(pane = %agent.pane_id, error = %err, "agent upsert skipped");
                }
            }
        }
        self.refresh_watched();
        Ok(count)
    }

    /// Safety reconcile: `agent.list` diff against every registry.
    ///
    /// Upserts every live agent (same path as snapshot) and marks registry
    /// records whose terminal/pane no longer appears in `agent.list` as
    /// offline. Runs every [`crate::daemon::RECONCILE_INTERVAL`] and after
    /// workspace/tab closes.
    pub fn reconcile(&mut self) -> Result<usize> {
        let agents = self.client.agent_list()?;
        let mut live_terminals = std::collections::HashSet::new();
        let mut live_panes = std::collections::HashSet::new();
        let mut count = 0;
        for agent in &agents {
            live_terminals.insert(agent.terminal_id.clone());
            live_panes.insert(agent.pane_id.clone());
            match self.upsert_agent(agent) {
                Ok(()) => count += 1,
                Err(err) => {
                    tracing::debug!(pane = %agent.pane_id, error = %err, "reconcile upsert skipped");
                }
            }
        }
        self.mark_vanished_offline(&live_terminals, &live_panes);
        self.refresh_watched();
        Ok(count)
    }

    /// Mark live records absent from `agent.list` offline. A record counts
    /// as live when either its terminal id or current pane id still appears
    /// (pane ids churn on `pane.move`; terminal ids are the stable anchor).
    fn mark_vanished_offline(
        &self,
        live_terminals: &std::collections::HashSet<String>,
        live_panes: &std::collections::HashSet<String>,
    ) {
        for pool in self.pools.all() {
            let registry = pool.registry();
            let agents = match registry.list_agents(false) {
                Ok(a) => a,
                Err(err) => {
                    tracing::warn!(scope = %pool.scope_key(), error = %err, "list_agents failed");
                    continue;
                }
            };
            for record in agents {
                let still_live = record
                    .terminal_id
                    .as_deref()
                    .is_some_and(|t| live_terminals.contains(t))
                    || record
                        .pane_id
                        .as_deref()
                        .is_some_and(|p| live_panes.contains(p));
                if !still_live {
                    if let Err(err) = registry.mark_offline(&record.agent_id) {
                        tracing::debug!(agent = %record.agent_id, error = %err, "mark_offline failed");
                    } else {
                        tracing::info!(agent = %record.agent_id, "agent vanished; marked offline");
                    }
                }
            }
        }
    }

    /// Dispatch one subscription frame. Returns the outcome for the event
    /// loop (delivery nudge / resubscribe bookkeeping happens inside).
    ///
    /// Any non-quiet outcome may have changed the watched-pane set, so
    /// [`Self::refresh_watched`] runs before returning — `subs_dirty` then
    /// tells the event loop to re-issue `events.subscribe`.
    pub fn handle_frame(&mut self, frame: &SubscriptionFrame) -> Result<SyncOutcome> {
        let outcome = match frame {
            SubscriptionFrame::Event(envelope) => {
                self.handle_event(envelope.event, &envelope.data)?
            }
            SubscriptionFrame::SubscriptionEvent(env) => {
                if let SubscriptionEventData::PaneAgentStatusChanged(ev) = &env.data {
                    self.handle_status_change(&ev.pane_id, ev.agent_status)?;
                    SyncOutcome::RegistryChanged
                } else {
                    // output_matched / scroll_changed / unknown: not subscribed.
                    SyncOutcome::Quiet
                }
            }
        };
        if outcome != SyncOutcome::Quiet {
            self.refresh_watched();
        }
        Ok(outcome)
    }

    fn handle_event(
        &mut self,
        kind: herdr_protocol::EventKind,
        data: &EventData,
    ) -> Result<SyncOutcome> {
        use herdr_protocol::EventKind as K;
        match (kind, data) {
            (
                K::PaneAgentDetected,
                EventData::PaneAgentDetected {
                    pane_id, released, ..
                },
            ) => {
                if *released {
                    self.mark_offline_pane(pane_id);
                    Ok(SyncOutcome::RegistryChanged)
                } else {
                    self.refresh_agent_by_pane(pane_id)?;
                    Ok(SyncOutcome::RegistryChanged)
                }
            }
            (K::PaneCreated, EventData::PaneCreated { pane })
            | (K::PaneUpdated, EventData::PaneUpdated { pane }) => {
                // PaneInfo may carry an agent; refresh via agent.get which
                // resolves the authoritative AgentInfo (or 404s for shells).
                self.refresh_agent_by_pane(&pane.pane_id)?;
                Ok(SyncOutcome::RegistryChanged)
            }
            (
                K::PaneAgentStatusChanged,
                EventData::PaneAgentStatusChanged {
                    pane_id,
                    agent_status,
                    ..
                },
            ) => {
                // Lifecycle variant of the same signal (defensive: we do not
                // subscribe to it, but a future server may push it anyway).
                self.handle_status_change(pane_id, *agent_status)?;
                Ok(SyncOutcome::RegistryChanged)
            }
            (
                K::PaneMoved,
                EventData::PaneMoved {
                    pane,
                    previous_pane_id,
                    ..
                },
            ) => {
                self.handle_pane_moved(previous_pane_id, pane)?;
                Ok(SyncOutcome::Resubscribe)
            }
            (K::PaneClosed, EventData::PaneClosed { pane_id, .. })
            | (K::PaneExited, EventData::PaneExited { pane_id, .. }) => {
                self.mark_offline_pane(pane_id);
                Ok(SyncOutcome::Resubscribe)
            }
            (K::WorkspaceClosed, EventData::WorkspaceClosed { .. })
            | (K::TabClosed, EventData::TabClosed { .. }) => {
                // Cascading closes are easiest to get right via the
                // authoritative agent.list diff.
                self.reconcile()?;
                Ok(SyncOutcome::Resubscribe)
            }
            _ => Ok(SyncOutcome::Quiet),
        }
    }

    /// `pane.moved`: keep `agent_id` stable across the pane-id change, then
    /// refresh the record (the move may also carry a new cwd/scope).
    fn handle_pane_moved(&mut self, previous_pane_id: &str, pane: &PaneInfo) -> Result<()> {
        // Rekey every live record pointing at the old pane id (or at this
        // terminal under a stale pane id) to the new pane id.
        let mut rekeyed = Vec::new();
        for pool in self.pools.all() {
            let registry = pool.registry();
            let mut candidates = registry.find_by_pane(previous_pane_id)?;
            if let Some(existing) = registry.get_by_terminal(&pane.terminal_id)? {
                if !candidates.iter().any(|c| c.agent_id == existing.agent_id) {
                    candidates.push(existing);
                }
            }
            for record in candidates {
                if record.status == STATUS_OFFLINE {
                    continue;
                }
                if record.pane_id.as_deref() == Some(pane.pane_id.as_str()) {
                    continue;
                }
                registry.rekey_pane(&record.agent_id, &pane.pane_id)?;
                rekeyed.push(record.agent_id.clone());
            }
        }
        if !rekeyed.is_empty() {
            tracing::info!(
                pane = %pane.pane_id,
                previous = %previous_pane_id,
                agents = ?rekeyed,
                "pane moved; rekeyed registry records"
            );
        }
        // Refresh authoritative state; a moved plain pane 404s and is a no-op.
        self.refresh_agent_by_pane(&pane.pane_id)?;
        Ok(())
    }

    /// Fetch `agent.get(pane_id)` and run the full upsert path. A
    /// `agent_not_found` on a pane we track marks the record offline —
    /// the agent was released or the pane became a plain shell.
    fn refresh_agent_by_pane(&mut self, pane_id: &str) -> Result<()> {
        match self.client.agent_get(pane_id) {
            Ok(agent) => self.upsert_agent(&agent),
            Err(err) if err.is_agent_not_found() => {
                self.mark_offline_pane(pane_id);
                Ok(())
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Mark every live record bound to `pane_id` offline, across all pools.
    fn mark_offline_pane(&self, pane_id: &str) {
        for pool in self.pools.all() {
            let registry = pool.registry();
            let records = match registry.find_by_pane(pane_id) {
                Ok(r) => r,
                Err(err) => {
                    tracing::debug!(scope = %pool.scope_key(), error = %err, "find_by_pane failed");
                    continue;
                }
            };
            for record in records {
                if record.status == STATUS_OFFLINE {
                    continue;
                }
                if let Err(err) = registry.mark_offline(&record.agent_id) {
                    tracing::debug!(agent = %record.agent_id, error = %err, "mark_offline failed");
                } else {
                    tracing::info!(agent = %record.agent_id, pane = %pane_id, "agent offline");
                }
            }
        }
    }

    /// Update only the stored status for the live agent on `pane_id`.
    /// Falls back to a full `agent.get` refresh when the pane is unknown —
    /// a status change can be the first signal we see for a new agent.
    fn handle_status_change(&mut self, pane_id: &str, status: AgentStatus) -> Result<()> {
        let mut found = false;
        for pool in self.pools.all() {
            let registry = pool.registry();
            let records = registry.find_by_pane(pane_id)?;
            for mut record in records {
                if record.status == STATUS_OFFLINE {
                    continue;
                }
                found = true;
                if record.status == status.as_str() {
                    continue;
                }
                let previous = record.status.clone();
                record.status = status.as_str().to_string();
                record.last_seen_at = now_millis();
                record.offline_at = None;
                if let Err(err) = registry.upsert_agent(&record) {
                    tracing::debug!(agent = %record.agent_id, error = %err, "status upsert failed");
                    continue;
                }
                tracing::debug!(
                    agent = %record.agent_id,
                    pane = %pane_id,
                    %previous,
                    %status,
                    "agent status updated"
                );
                if matches!(status, AgentStatus::Idle | AgentStatus::Working) {
                    self.delivery_nudge = true;
                    // A promptable transition is also the bootstrap retry
                    // point. Status events carry no readiness fields, so an
                    // agent still flagged not-ready gets a fresh agent.get
                    // — that is the only way to observe interactive_ready
                    // flipping true (e.g. `devin --resume` finishing).
                    let needs_bootstrap = record.bootstrap_version.unwrap_or(0) < BOOTSTRAP_VERSION;
                    let not_ready = !record.interactive_ready || record.launch_pending;
                    if needs_bootstrap && not_ready {
                        self.refresh_agent_by_pane(pane_id)?;
                    } else {
                        self.bootstrap.maybe_attempt(&pool, &record);
                    }
                }
            }
        }
        if !found {
            // Unknown pane: could be a brand-new agent whose detection event
            // we missed — resolve it fully.
            self.refresh_agent_by_pane(pane_id)?;
        }
        Ok(())
    }

    /// Upsert one `AgentInfo`: resolve scope → open pool → merge with any
    /// existing record (stable `agent_id` via `terminal_id`) → persist →
    /// maybe bootstrap.
    fn upsert_agent(&mut self, agent: &AgentInfo) -> Result<()> {
        let resolved = self.resolve_agent_scope(agent)?;
        let pool = self.pools.for_resolved(&resolved)?;
        let record = self.merge_record(&pool, agent, &resolved)?;
        self.retire_foreign_scope_records(agent, &pool);
        let registry = pool.registry();
        let previous = registry.get_agent(&record.agent_id)?;
        let pane_changed = previous
            .as_ref()
            .and_then(|p| p.pane_id.as_deref())
            .is_some_and(|old| Some(old) != record.pane_id.as_deref());
        registry.upsert_agent(&record)?;
        let is_new = previous.is_none();
        if is_new {
            tracing::info!(
                agent = %record.agent_id,
                kind = ?record.agent_kind,
                name = ?record.name,
                scope = %record.scope_key,
                "new agent registered"
            );
        }
        self.delivery_nudge = true;
        self.bootstrap.maybe_attempt(&pool, &record);
        if is_new || pane_changed {
            self.ensure_pane_label(agent, &record);
        }
        Ok(())
    }

    /// Label the agent's pane with its display name when the pane has no
    /// label yet — never overwrites a user- or agent-set label, and only
    /// runs once per pane per daemon run ([`Self::label_checked`]).
    /// `HERDR_POOL_PANE_LABEL=0|false|no|off` disables this entirely.
    fn ensure_pane_label(&mut self, agent: &AgentInfo, record: &AgentRecord) {
        if !pane_label_enabled() {
            return;
        }
        let Some(pane_id) = record.pane_id.clone().filter(|p| !p.is_empty()) else {
            return;
        };
        if !self.label_checked.insert(pane_id.clone()) {
            return;
        }
        let pane = match self.client.pane_get(&pane_id) {
            Ok(pane) => pane,
            Err(err) => {
                tracing::debug!(pane = %pane_id, error = %err, "pane.get for label check failed");
                return;
            }
        };
        if pane.label.as_deref().is_some_and(|l| !l.trim().is_empty()) {
            return;
        }
        let label = agent
            .name
            .clone()
            .or_else(|| agent.display_agent.clone())
            .or_else(|| agent.agent.clone())
            .filter(|l| !l.trim().is_empty());
        let Some(label) = label else {
            return;
        };
        match self.client.pane_rename(&pane_id, Some(&label)) {
            Ok(_) => {
                tracing::info!(pane = %pane_id, label = %label, "pane auto-labeled");
            }
            Err(err) => {
                tracing::debug!(pane = %pane_id, error = %err, "pane.rename failed");
            }
        }
    }

    /// An agent belongs to exactly one scope. When its resolved scope
    /// differs from where its `terminal_id` was previously registered
    /// (launch cwd changed, terminal reused by another project), retire
    /// those records — otherwise the agent stays a live ghost peer in the
    /// old scope forever (`mark_vanished_offline` diffs against the
    /// *global* live set, which still contains this terminal).
    fn retire_foreign_scope_records(&self, agent: &AgentInfo, pool: &Arc<PoolStore>) {
        for other in self.pools.all() {
            if other.scope_key() == pool.scope_key() {
                continue;
            }
            let registry = other.registry();
            let records = match registry.find_by_terminal(&agent.terminal_id) {
                Ok(r) => r,
                Err(err) => {
                    tracing::debug!(scope = %other.scope_key(), error = %err, "find_by_terminal failed");
                    continue;
                }
            };
            for rec in records {
                if rec.status == STATUS_OFFLINE {
                    continue;
                }
                if let Err(err) = registry.mark_offline(&rec.agent_id) {
                    tracing::debug!(agent = %rec.agent_id, error = %err, "cross-scope mark_offline failed");
                } else {
                    tracing::info!(
                        agent = %rec.agent_id,
                        from_scope = %other.scope_key(),
                        to_scope = %pool.scope_key(),
                        "agent moved scope; retired stale record"
                    );
                }
            }
        }
    }

    /// Resolve an agent's project scope, falling back to
    /// `pane.process_info` once when the record carries no cwd fields.
    fn resolve_agent_scope(&self, agent: &AgentInfo) -> Result<ResolvedScope> {
        match resolve_scope_for_agent(agent, None) {
            Ok(scope) => Ok(scope),
            Err(core_err) => {
                if !matches!(core_err, herdr_pool_core::CoreError::ScopeUnresolved(_)) {
                    return Err(core_err.into());
                }
                let process_info = self
                    .client
                    .pane_process_info(Some(agent.pane_id.as_str()))?;
                resolve_scope_for_agent(agent, Some(&process_info)).map_err(Into::into)
            }
        }
    }

    /// Build the `AgentRecord` for `agent`, preserving identity fields
    /// (`agent_id`, `first_seen_at`, `bootstrap_version`) from any existing
    /// record matched by `terminal_id` (then live `pane_id`).
    fn merge_record(
        &self,
        pool: &Arc<PoolStore>,
        agent: &AgentInfo,
        resolved: &ResolvedScope,
    ) -> Result<AgentRecord> {
        let registry = pool.registry();
        // Terminal match includes offline rows: a transient offline mark
        // (agent restart, resume handoff) must not mint a new agent_id.
        let existing = registry
            .get_by_terminal_any(&agent.terminal_id)?
            .or_else(|| {
                registry
                    .find_by_pane(&agent.pane_id)
                    .ok()?
                    .into_iter()
                    .find(|r| r.status != STATUS_OFFLINE)
            });

        let now = now_millis();
        Ok(AgentRecord {
            agent_id: existing
                .as_ref()
                .map(|e| e.agent_id.clone())
                .unwrap_or_else(new_agent_id),
            scope_key: resolved.scope_key.as_str().to_string(),
            terminal_id: Some(agent.terminal_id.clone()),
            pane_id: Some(agent.pane_id.clone()),
            agent_kind: agent.agent.clone().or_else(|| agent.display_agent.clone()),
            name: agent.name.clone(),
            cwd: Some(resolved.canonical_path.to_string_lossy().into_owned()),
            status: agent.agent_status.as_str().to_string(),
            interactive_ready: agent.interactive_ready,
            launch_pending: agent.launch_pending,
            bootstrap_version: existing.as_ref().and_then(|e| e.bootstrap_version),
            first_seen_at: existing.map(|e| e.first_seen_at).unwrap_or(now),
            last_seen_at: now,
            offline_at: None,
        })
    }
}

/// One-shot reconcile used by `herdr-routerd tick` (plugin `[[events]]`
/// hooks) — snapshot + registry sync + bootstrap attempts, no daemon.
pub fn run_tick(client: &HerdrClient, state_dir: &Path, bootstrap_enabled: bool) -> Result<usize> {
    let pools = Pools::open(state_dir)?;
    let mut sync = RegistrySync::new(client, &pools, bootstrap_enabled);
    let mut synced = sync.sync_snapshot()?;
    synced += sync.reconcile()?;
    sync.refresh_watched();
    // Rescue sweep: dispatch/monitor/expire jobs left over from before a
    // restart or accumulated while the daemon was down. Best-effort —
    // tick must not fail because a helper prompt was rejected.
    if let Err(err) =
        crate::rescue::RescueManager::new(client.clone(), Arc::new(pools)).sweep_once()
    {
        tracing::warn!(error = %err, "tick: rescue sweep failed");
    }
    Ok(synced)
}

// ---------------------------------------------------------------------------
// Bootstrapper
// ---------------------------------------------------------------------------

/// Sends the one-shot bootstrap prompt to newly registered agents.
///
/// Rate limiting: at most [`Self::MAX_ATTEMPTS_PER_RUN`] `agent.prompt`
/// calls per agent per daemon run (tracked in memory; `bootstrap_version`
/// in the DB is the durable record). `agent_blocked`/`agent_not_ready` and
/// transient transport errors leave the agent eligible for the next
/// sighting; after the cap the agent is silently skipped — the installed
/// skill remains the fallback channel.
pub struct Bootstrapper<'a> {
    client: &'a HerdrClient,
    /// `[bootstrap] enabled` / `HERDR_POOL_BOOTSTRAP` — when false the
    /// prompt is never sent (the installed skill remains the channel).
    enabled: bool,
    /// agent_id → prompt attempts used this run.
    attempts: HashMap<String, u8>,
}

impl<'a> Bootstrapper<'a> {
    /// Prompt attempts allowed per agent per daemon run.
    pub const MAX_ATTEMPTS_PER_RUN: u8 = 3;

    fn new(client: &'a HerdrClient, enabled: bool) -> Self {
        Self {
            client,
            enabled,
            attempts: HashMap::new(),
        }
    }

    /// Attempt the bootstrap prompt for `record` when it needs one
    /// (`bootstrap_version < BOOTSTRAP_VERSION`). Cheap no-op otherwise.
    /// Errors are logged and swallowed — bootstrap must never break the
    /// registry sync path.
    ///
    /// Readiness gate: an agent Herdr reports as `!interactive_ready` or
    /// `launch_pending` (still launching, or mid `--resume`) is skipped
    /// *without* consuming an attempt — `agent.prompt` there could paste
    /// the instruction into a half-restored TUI. The next AgentInfo
    /// sighting (agent.get/list refresh, status change) re-evaluates.
    pub fn maybe_attempt(&mut self, pool: &Arc<PoolStore>, record: &AgentRecord) {
        if !self.enabled {
            return;
        }
        if record.status == STATUS_OFFLINE {
            return;
        }
        let current = record.bootstrap_version.unwrap_or(0);
        if current >= BOOTSTRAP_VERSION {
            return;
        }
        if !record.interactive_ready || record.launch_pending {
            tracing::debug!(
                agent = %record.agent_id,
                interactive_ready = record.interactive_ready,
                launch_pending = record.launch_pending,
                "bootstrap deferred: agent not interactive-ready"
            );
            return;
        }
        let used = self.attempts.get(&record.agent_id).copied().unwrap_or(0);
        if used >= Self::MAX_ATTEMPTS_PER_RUN {
            return;
        }
        self.attempts.insert(record.agent_id.clone(), used + 1);

        // Same target policy as the delivery worker: `agent.prompt`
        // rejects terminal_id (agent_not_found), so pane_id is preferred,
        // then name, terminal_id last.
        let target = record
            .pane_id
            .clone()
            .filter(|p| !p.is_empty())
            .or_else(|| record.name.clone().filter(|n| !n.is_empty()))
            .or_else(|| record.terminal_id.clone().filter(|t| !t.is_empty()));
        let Some(target) = target else {
            tracing::debug!(agent = %record.agent_id, "bootstrap skipped: no target");
            return;
        };

        match self.client.agent_prompt(&target, bootstrap_text(), None) {
            Ok(_) => {
                self.attempts.remove(&record.agent_id);
                if let Err(err) = mark_bootstrapped(pool, record) {
                    tracing::warn!(agent = %record.agent_id, error = %err, "bootstrap_version persist failed");
                } else {
                    tracing::info!(agent = %record.agent_id, "bootstrap prompt delivered");
                }
            }
            Err(err) if err.is_agent_blocked() || err.is_agent_not_ready() => {
                // Rejected before anything was sent — do not burn the
                // per-run budget, or an agent first seen while blocked
                // would never get bootstrapped after it unblocks.
                if used == 0 {
                    self.attempts.remove(&record.agent_id);
                } else {
                    self.attempts.insert(record.agent_id.clone(), used);
                }
                tracing::debug!(
                    agent = %record.agent_id,
                    error = %err,
                    "bootstrap prompt deferred (agent not promptable)"
                );
            }
            Err(err) => {
                tracing::debug!(
                    agent = %record.agent_id,
                    attempt = used + 1,
                    error = %err,
                    "bootstrap prompt deferred"
                );
            }
        }
    }
}

/// Persist `BOOTSTRAP_VERSION` on the agent row (re-upserts the record so
/// no other field is lost).
fn mark_bootstrapped(pool: &Arc<PoolStore>, record: &AgentRecord) -> Result<()> {
    let mut updated = record.clone();
    updated.bootstrap_version = Some(BOOTSTRAP_VERSION);
    updated.last_seen_at = now_millis();
    pool.registry().upsert_agent(&updated)?;
    Ok(())
}
