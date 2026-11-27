//! Registry synchronization: runtime state → per-scope agent registries.
//!
//! [`RegistrySync`] is the single writer of registry state in the daemon.
//! It is fed by two sources:
//!
//! * **Bootstrap** — [`RegistrySync::sync_snapshot`] walks
//!   `runtime.list_agents()`, resolves each agent's project scope, and
//!   upserts an [`AgentRecord`] (reusing the stored `agent_id` keyed by
//!   `terminal_id`, then live handle). [`RegistrySync::reconcile`] does
//!   the same listing and additionally marks agents that vanished as
//!   offline.
//! * **Events** — [`RegistrySync::handle_event`] maps normalized
//!   [`RuntimeEvent`]s (`AgentSeen`, `AgentStatusChanged`, `AgentGone`,
//!   `HandleRekeyed`, `TopologyChanged`) onto the same
//!   upsert/offline/rekey/reconcile operations. Backend wire formats
//!   never reach this module.
//!
//! `AgentRecord.pane_id` stores the backend-opaque runtime *handle*
//! (`w1:p1` on herdr, `t7`/`machine/t7` on hc); `terminal_id` stays
//! `Option` (hc has none).
//!
//! ## Bootstrap notifier
//!
//! [`Bootstrapper`] sends the one-shot `[herdr-pool] bootstrap` prompt the
//! first time an agent needs it (`bootstrap_version < BOOTSTRAP_VERSION`).
//! Failures (`Deferred`/`Failed` outcomes, transport) are retried on
//! later sightings, capped at [`Bootstrapper::MAX_ATTEMPTS_PER_RUN`]
//! prompt attempts per agent per daemon run — the skill files remain the
//! durable channel. Success persists `bootstrap_version` in the agents
//! table, so restarts and `tick` runs do not re-send.
//!
//! ## Watched-handle status subscriptions
//!
//! Fine-grained status events need a concrete handle per subscription on
//! herdr (`pane.agent_status_changed` has no global variant — the
//! `AgentRuntime::subscribe_events` `watched` argument carries the set).
//! [`RegistrySync::watched_panes`] exposes the live agent handles; the
//! event loop folds them into `subscribe_events` and resubscribes when
//! the set changes ([`RegistrySync::subscriptions_changed`]). On hc the
//! parameter is ignored (the event feed is already global).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use herdr_pool_core::{resolve_scope, ResolvedScope};
use herdr_pool_store::{new_agent_id, AgentRecord, PoolStore, STATUS_OFFLINE};
use herdr_protocol::HerdrClient;
use herdr_runtime::{
    AgentRuntime, RuntimeAgent, RuntimeError, RuntimeEvent, RuntimeStatus, SendOutcome,
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

/// What a dispatched event changed, for the event loop's bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Nothing the delivery worker or subscription set cares about.
    Quiet,
    /// Registry state changed; delivery should be nudged.
    RegistryChanged,
    /// The watched-handle set may have changed; resubscribe.
    Resubscribe,
}

/// Registry sync + bootstrap engine. Single-threaded by design: the daemon
/// event loop owns the only instance.
pub struct RegistrySync<'a> {
    runtime: &'a dyn AgentRuntime,
    pools: &'a Pools,
    bootstrap: Bootstrapper<'a>,
    /// Handles currently covered by fine-grained status subscriptions
    /// (rebuilt by [`Self::refresh_watched`]).
    watched: BTreeSet<String>,
    /// Set when `watched` differs from the last subscribed set.
    subs_dirty: bool,
    /// Whether any agent transitioned to a promptable state recently —
    /// surfaced so the event loop can nudge the delivery worker.
    delivery_nudge: bool,
    /// Handles already checked for auto-labeling this run — bounds the
    /// extra `pane.get` calls to once per handle per daemon run.
    label_checked: HashSet<String>,
}

impl<'a> RegistrySync<'a> {
    pub fn new(runtime: &'a dyn AgentRuntime, pools: &'a Pools, bootstrap_enabled: bool) -> Self {
        Self {
            runtime,
            pools,
            bootstrap: Bootstrapper::new(runtime, bootstrap_enabled),
            watched: BTreeSet::new(),
            subs_dirty: false,
            delivery_nudge: false,
            label_checked: HashSet::new(),
        }
    }

    /// Handles the event loop should request fine-grained status events
    /// for. Empty until the first [`Self::refresh_watched`].
    pub fn watched_panes(&self) -> &BTreeSet<String> {
        &self.watched
    }

    /// True when the watched-handle set changed since the last check — the
    /// event loop should tear down and re-issue `subscribe_events`.
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

    /// Rebuild the watched-handle set from every live registry record.
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
                "watched handle set changed; resubscribing"
            );
            self.watched = next;
            self.subs_dirty = true;
        }
    }

    /// Full bootstrap pass: `runtime.list_agents()` → upsert every reported
    /// agent.
    ///
    /// This is the daemon's prime entry point and what `tick` runs.
    pub fn sync_snapshot(&mut self) -> Result<usize> {
        let agents = self.runtime.list_agents()?;
        self.sync_agents(&agents)
    }

    /// Upsert a batch of runtime agents (e.g. an [`EventStream`] snapshot)
    /// and rebuild the watched set. Same path as [`Self::sync_snapshot`]
    /// minus the listing call.
    pub fn sync_agents(&mut self, agents: &[RuntimeAgent]) -> Result<usize> {
        let mut count = 0;
        for agent in agents {
            match self.upsert_agent(agent) {
                Ok(()) => count += 1,
                Err(err) => {
                    tracing::debug!(handle = %agent.handle, error = %err, "agent upsert skipped");
                }
            }
        }
        self.refresh_watched();
        Ok(count)
    }

    /// Safety reconcile: `runtime.list_agents()` diff against every
    /// registry.
    ///
    /// Upserts every live agent (same path as snapshot) and marks registry
    /// records whose handle/terminal no longer appears in the listing as
    /// offline. Runs every [`crate::daemon::RECONCILE_INTERVAL`] and after
    /// topology changes.
    pub fn reconcile(&mut self) -> Result<usize> {
        let agents = self.runtime.list_agents()?;
        let mut live_handles = std::collections::HashSet::new();
        let mut live_terminals = std::collections::HashSet::new();
        let mut count = 0;
        for agent in &agents {
            live_handles.insert(agent.handle.clone());
            if let Some(t) = &agent.terminal_id {
                live_terminals.insert(t.clone());
            }
            match self.upsert_agent(agent) {
                Ok(()) => count += 1,
                Err(err) => {
                    tracing::debug!(handle = %agent.handle, error = %err, "reconcile upsert skipped");
                }
            }
        }
        self.mark_vanished_offline(&live_handles, &live_terminals);
        self.refresh_watched();
        Ok(count)
    }

    /// Mark live records absent from the agent listing offline. A record
    /// counts as live when either its terminal id or current handle still
    /// appears (handles churn on `pane.move`; terminal ids are the stable
    /// anchor where the backend has them).
    fn mark_vanished_offline(
        &self,
        live_handles: &std::collections::HashSet<String>,
        live_terminals: &std::collections::HashSet<String>,
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
                        .is_some_and(|p| live_handles.contains(p));
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

    /// Dispatch one normalized runtime event. Returns the outcome for the
    /// event loop (delivery nudge / resubscribe bookkeeping happens inside).
    ///
    /// Any non-quiet outcome may have changed the watched-handle set, so
    /// [`Self::refresh_watched`] runs before returning — `subs_dirty` then
    /// tells the event loop to re-issue `subscribe_events`.
    pub fn handle_event(&mut self, event: &RuntimeEvent) -> Result<SyncOutcome> {
        let outcome = match event {
            RuntimeEvent::AgentSeen { agent } => {
                // Event payloads are hints, not authoritative records
                // (e.g. herdr pane.created carries no readiness flags) —
                // always refresh via `get_agent`, matching the old
                // `agent.get`-per-event behavior. A `NotFound` means the
                // handle is a plain shell or already gone.
                self.refresh_agent_by_handle(&agent.handle)?;
                SyncOutcome::RegistryChanged
            }
            RuntimeEvent::AgentStatusChanged { handle, status } => {
                self.handle_status_change(handle, *status)?;
                SyncOutcome::RegistryChanged
            }
            RuntimeEvent::AgentGone { handle } => {
                self.mark_offline_handle(handle);
                SyncOutcome::Resubscribe
            }
            RuntimeEvent::HandleRekeyed { old, new } => {
                self.handle_rekeyed(old, new)?;
                SyncOutcome::Resubscribe
            }
            RuntimeEvent::TopologyChanged => {
                // Cascading workspace/tab closes: the event does not
                // itemize panes, so the authoritative listing diff is the
                // only correct answer (same as the old reconcile path).
                self.reconcile()?;
                SyncOutcome::Resubscribe
            }
        };
        if outcome != SyncOutcome::Quiet {
            self.refresh_watched();
        }
        Ok(outcome)
    }

    /// Handle re-keyed (herdr `pane.moved`; never fires on hc): keep
    /// `agent_id` stable across the handle change, then refresh the record
    /// (the move may also carry a new cwd/scope).
    fn handle_rekeyed(&mut self, old_handle: &str, new_handle: &str) -> Result<()> {
        // Rekey every live record pointing at the old handle.
        let mut rekeyed = Vec::new();
        for pool in self.pools.all() {
            let registry = pool.registry();
            for record in registry.find_by_pane(old_handle)? {
                if record.status == STATUS_OFFLINE {
                    continue;
                }
                if record.pane_id.as_deref() == Some(new_handle) {
                    continue;
                }
                registry.rekey_pane(&record.agent_id, new_handle)?;
                rekeyed.push(record.agent_id.clone());
            }
        }
        if !rekeyed.is_empty() {
            tracing::info!(
                handle = %new_handle,
                previous = %old_handle,
                agents = ?rekeyed,
                "handle rekeyed; updated registry records"
            );
        }
        // Refresh authoritative state; a moved plain pane 404s and is a
        // no-op. The upsert re-anchors identity by `terminal_id`, covering
        // records whose stored handle was already stale.
        self.refresh_agent_by_handle(new_handle)
    }

    /// Fetch `runtime.get_agent(handle)` and run the full upsert path. A
    /// `NotFound` on a handle we track marks the record offline — the
    /// agent was released or the pane became a plain shell.
    fn refresh_agent_by_handle(&mut self, handle: &str) -> Result<()> {
        match self.runtime.get_agent(handle) {
            Ok(agent) => self.upsert_agent(&agent),
            Err(RuntimeError::NotFound { .. }) => {
                self.mark_offline_handle(handle);
                Ok(())
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Mark every live record bound to `handle` offline, across all pools.
    fn mark_offline_handle(&self, handle: &str) {
        for pool in self.pools.all() {
            let registry = pool.registry();
            let records = match registry.find_by_pane(handle) {
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
                    tracing::info!(agent = %record.agent_id, handle = %handle, "agent offline");
                }
            }
        }
    }

    /// Evict a handle the backend reported gone (`pane_not_found` on
    /// subscribe): mark every live record bound to it offline and rebuild
    /// the watched set so `refresh_watched` cannot re-add it. The handle
    /// is also dropped from `watched` unconditionally — a dead handle
    /// must never stay subscribed even if the offline write failed.
    pub fn evict_dead_pane(&mut self, pane_id: &str) {
        self.mark_offline_handle(pane_id);
        self.refresh_watched();
        self.watched.remove(pane_id);
        tracing::info!(pane = %pane_id, "evicted dead watched handle");
    }

    /// Update only the stored status for the live agent on `handle`.
    /// Falls back to a full `get_agent` refresh when the handle is
    /// unknown — a status change can be the first signal we see for a
    /// new agent.
    fn handle_status_change(&mut self, handle: &str, status: RuntimeStatus) -> Result<()> {
        let mut found = false;
        for pool in self.pools.all() {
            let registry = pool.registry();
            let records = registry.find_by_pane(handle)?;
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
                    handle = %handle,
                    %previous,
                    %status,
                    "agent status updated"
                );
                if matches!(status, RuntimeStatus::Idle | RuntimeStatus::Working) {
                    self.delivery_nudge = true;
                    // A promptable transition is also the bootstrap retry
                    // point. Status events carry no readiness fields, so an
                    // agent still flagged not-ready gets a fresh get_agent
                    // — that is the only way to observe interactive_ready
                    // flipping true (e.g. `devin --resume` finishing).
                    let needs_bootstrap = record.bootstrap_version.unwrap_or(0) < BOOTSTRAP_VERSION;
                    let not_ready = !record.interactive_ready || record.launch_pending;
                    if needs_bootstrap && not_ready {
                        self.refresh_agent_by_handle(handle)?;
                    } else {
                        self.bootstrap.maybe_attempt(&pool, &record);
                    }
                }
            }
        }
        if !found {
            // Unknown handle: could be a brand-new agent whose detection
            // event we missed — resolve it fully.
            self.refresh_agent_by_handle(handle)?;
        }
        Ok(())
    }

    /// Upsert one [`RuntimeAgent`]: resolve scope → open pool → merge with
    /// any existing record (stable `agent_id` via `terminal_id`, then live
    /// handle) → persist → maybe bootstrap.
    fn upsert_agent(&mut self, agent: &RuntimeAgent) -> Result<()> {
        let resolved = self.resolve_agent_scope(agent)?;
        let pool = self.pools.for_resolved(&resolved)?;
        let record = self.merge_record(&pool, agent, &resolved)?;
        self.retire_foreign_scope_records(agent, &pool);
        let registry = pool.registry();
        let previous = registry.get_agent(&record.agent_id)?;
        let handle_changed = previous
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
        if is_new || handle_changed {
            self.ensure_pane_label(agent, &record);
        }
        Ok(())
    }

    /// Label the agent's pane with its display name when the pane has no
    /// label yet — never overwrites a user- or agent-set label, and only
    /// runs once per pane per daemon run ([`Self::label_checked`]).
    /// `HERDR_POOL_PANE_LABEL=0|false|no|off` disables this entirely.
    /// Herdr-only: skipped entirely on backends without `pane.rename`.
    fn ensure_pane_label(&mut self, agent: &RuntimeAgent, record: &AgentRecord) {
        if !pane_label_enabled() {
            return;
        }
        let Some(client) = self.runtime.herdr_client() else {
            return;
        };
        let Some(pane_id) = record.pane_id.clone().filter(|p| !p.is_empty()) else {
            return;
        };
        if !self.label_checked.insert(pane_id.clone()) {
            return;
        }
        let pane = match client.pane_get(&pane_id) {
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
            .or_else(|| agent.kind.clone())
            .filter(|l| !l.trim().is_empty());
        let Some(label) = label else {
            return;
        };
        match client.pane_rename(&pane_id, Some(&label)) {
            Ok(_) => {
                tracing::info!(pane = %pane_id, label = %label, "pane auto-labeled");
            }
            Err(err) => {
                tracing::debug!(pane = %pane_id, error = %err, "pane.rename failed");
            }
        }
    }

    /// An agent belongs to exactly one scope. When its resolved scope
    /// differs from where it was previously registered (launch cwd
    /// changed, terminal reused by another project), retire those records
    /// — otherwise the agent stays a live ghost peer in the old scope
    /// forever (`mark_vanished_offline` diffs against the *global* live
    /// set, which still contains this agent).
    fn retire_foreign_scope_records(&self, agent: &RuntimeAgent, pool: &Arc<PoolStore>) {
        for other in self.pools.all() {
            if other.scope_key() == pool.scope_key() {
                continue;
            }
            let registry = other.registry();
            // Terminal id is the stable cross-scope anchor where the
            // backend has one; the handle is the fallback (hc addresses
            // are stable for the terminal's lifetime).
            let records = match agent.terminal_id.as_deref() {
                Some(terminal_id) => match registry.find_by_terminal(terminal_id) {
                    Ok(r) => r,
                    Err(err) => {
                        tracing::debug!(scope = %other.scope_key(), error = %err, "find_by_terminal failed");
                        continue;
                    }
                },
                None => match registry.find_by_pane(&agent.handle) {
                    Ok(r) => r,
                    Err(err) => {
                        tracing::debug!(scope = %other.scope_key(), error = %err, "find_by_pane failed");
                        continue;
                    }
                },
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

    /// Resolve an agent's project scope, falling back to the runtime's
    /// foreground-process cwd table once when the record carries no cwd
    /// fields (herdr `pane.process_info`; empty on flat backends).
    fn resolve_agent_scope(&self, agent: &RuntimeAgent) -> Result<ResolvedScope> {
        match resolve_scope(agent.cwd.as_deref(), agent.foreground_cwd.as_deref(), None) {
            Ok(scope) => Ok(scope),
            Err(core_err) => {
                if !matches!(core_err, herdr_pool_core::CoreError::ScopeUnresolved(_)) {
                    return Err(core_err.into());
                }
                // Process-info cwds are lower priority than the record
                // fields — try each in reported order, first hit wins.
                for cwd in self
                    .runtime
                    .process_info_cwds(&agent.handle)
                    .unwrap_or_default()
                {
                    if let Ok(scope) = resolve_scope(Some(&cwd), None, None) {
                        return Ok(scope);
                    }
                }
                Err(core_err.into())
            }
        }
    }

    /// Build the `AgentRecord` for `agent`, preserving identity fields
    /// (`agent_id`, `first_seen_at`, `bootstrap_version`) from any existing
    /// record matched by `terminal_id` (then live `handle`).
    fn merge_record(
        &self,
        pool: &Arc<PoolStore>,
        agent: &RuntimeAgent,
        resolved: &ResolvedScope,
    ) -> Result<AgentRecord> {
        let registry = pool.registry();
        // Terminal match includes offline rows: a transient offline mark
        // (agent restart, resume handoff) must not mint a new agent_id.
        let existing = match agent.terminal_id.as_deref() {
            Some(terminal_id) => registry.get_by_terminal_any(terminal_id)?,
            None => None,
        }
        .or_else(|| {
            registry
                .find_by_pane(&agent.handle)
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
            terminal_id: agent.terminal_id.clone(),
            pane_id: Some(agent.handle.clone()),
            agent_kind: agent.kind.clone(),
            name: agent.name.clone(),
            cwd: Some(resolved.canonical_path.to_string_lossy().into_owned()),
            status: agent.status.as_str().to_string(),
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
/// hooks and manual diagnostics) — listing + registry sync + bootstrap
/// attempts + rescue sweep, no daemon. `runtime` is the backend selected
/// by `HERDR_POOL_RUNTIME`; `rescue_client` is the raw herdr client on the
/// herdr backend (`None` on hc — [`RescueManager`] still uses herdr-only
/// read sources the trait does not carry yet).
pub fn run_tick(
    runtime: &dyn AgentRuntime,
    rescue_client: Option<HerdrClient>,
    state_dir: &Path,
    bootstrap_enabled: bool,
) -> Result<usize> {
    let pools = Pools::open(state_dir)?;
    let mut sync = RegistrySync::new(runtime, &pools, bootstrap_enabled);
    let mut synced = sync.sync_snapshot()?;
    synced += sync.reconcile()?;
    sync.refresh_watched();
    // Rescue sweep: dispatch/monitor/expire jobs left over from before a
    // restart or accumulated while the daemon was down. Best-effort —
    // tick must not fail because a helper prompt was rejected.
    if let Some(client) = rescue_client {
        if let Err(err) = crate::rescue::RescueManager::new(client, Arc::new(pools)).sweep_once() {
            tracing::warn!(error = %err, "tick: rescue sweep failed");
        }
    }
    Ok(synced)
}

// ---------------------------------------------------------------------------
// Bootstrapper
// ---------------------------------------------------------------------------

/// Sends the one-shot bootstrap prompt to newly registered agents.
///
/// Rate limiting: at most [`Self::MAX_ATTEMPTS_PER_RUN`] prompt
/// attempts per agent per daemon run (tracked in memory;
/// `bootstrap_version` in the DB is the durable record). `Deferred`
/// outcomes (blocked / not-ready) and transient transport errors leave
/// the agent eligible for the next sighting; after the cap the agent is
/// silently skipped — the installed skill remains the fallback channel.
pub struct Bootstrapper<'a> {
    runtime: &'a dyn AgentRuntime,
    /// `[bootstrap] enabled` / `HERDR_POOL_BOOTSTRAP` — when false the
    /// prompt is never sent (the installed skill remains the channel).
    enabled: bool,
    /// agent_id → prompt attempts used this run.
    attempts: HashMap<String, u8>,
}

impl<'a> Bootstrapper<'a> {
    /// Prompt attempts allowed per agent per daemon run.
    pub const MAX_ATTEMPTS_PER_RUN: u8 = 3;

    fn new(runtime: &'a dyn AgentRuntime, enabled: bool) -> Self {
        Self {
            runtime,
            enabled,
            attempts: HashMap::new(),
        }
    }

    /// Attempt the bootstrap prompt for `record` when it needs one
    /// (`bootstrap_version < BOOTSTRAP_VERSION`). Cheap no-op otherwise.
    /// Errors are logged and swallowed — bootstrap must never break the
    /// registry sync path.
    ///
    /// Readiness gate: an agent the runtime reports as `!interactive_ready`
    /// or `launch_pending` (still launching, or mid `--resume`) is skipped
    /// *without* consuming an attempt — prompting there could paste the
    /// instruction into a half-restored TUI. The next record sighting
    /// (get_agent/list refresh, status change) re-evaluates.
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

        // Same target policy as the delivery worker: the runtime handle
        // (stored in `pane_id`) is preferred, then name, then terminal id.
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

        match self.runtime.send_prompt(&target, bootstrap_text()) {
            Ok(SendOutcome::Delivered { .. }) => {
                self.attempts.remove(&record.agent_id);
                if let Err(err) = mark_bootstrapped(pool, record) {
                    tracing::warn!(agent = %record.agent_id, error = %err, "bootstrap_version persist failed");
                } else {
                    tracing::info!(agent = %record.agent_id, "bootstrap prompt delivered");
                }
            }
            Ok(SendOutcome::Deferred { .. }) => {
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
                    "bootstrap prompt deferred (agent not promptable)"
                );
            }
            Ok(SendOutcome::Failed { message, .. }) => {
                tracing::debug!(
                    agent = %record.agent_id,
                    attempt = used + 1,
                    error = %message,
                    "bootstrap prompt deferred"
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
