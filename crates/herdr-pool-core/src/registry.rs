//! Peer-registry record construction, runtime-agnostic.
//!
//! [`AgentRecord`]s are built from [`RuntimeAgent`]s — the normalized
//! view every [`AgentRuntime`] backend produces — so the same upsert
//! logic serves herdr panes (`w1:p1` handles) and hc terminals (`t7`,
//! `machine/t7`). The `pane_id` column stores the runtime handle
//! verbatim and `ag_*` pool ids are minted by the store, so ids stay
//! stable regardless of backend.
//!
//! Shared by the CLI ([`crate::identity::resolve_self`] callers) and,
//! once ported, the router's registry sync — keep this module free of
//! CLI concerns.

use herdr_pool_store::{new_agent_id, AgentRecord, RegistryStore, STATUS_OFFLINE};
use herdr_runtime::{AgentRuntime, RuntimeAgent};

use crate::error::{CoreError, Result};
use crate::identity::SelfIdentity;
use crate::pool::PoolContext;
use crate::scope::{resolve_scope_for_runtime, ResolvedScope};

/// Build (or merge into) the [`AgentRecord`] for a [`RuntimeAgent`],
/// preserving `agent_id`/`first_seen_at`/`bootstrap_version` from any
/// existing record matched by `terminal_id` then live handle.
pub fn record_for_agent(
    registry: &RegistryStore<'_>,
    agent: &RuntimeAgent,
    resolved: &ResolvedScope,
) -> AgentRecord {
    let existing = agent
        .terminal_id
        .as_deref()
        .and_then(|t| registry.get_by_terminal_any(t).ok().flatten())
        .or_else(|| {
            registry
                .find_by_pane(&agent.handle)
                .ok()?
                .into_iter()
                .find(|r| r.status != STATUS_OFFLINE)
        });

    let now = now_millis();
    AgentRecord {
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
    }
}

/// Upsert *this* process into the pool registry and return its record.
///
/// The agent record is used when the self handle hosts a detected agent;
/// otherwise the pane/terminal record supplies ids, name, and status so
/// plain shells stay addressable. `agent_kind: None` marks shell records
/// — the offline sweep never touches them (same rule as before the port).
pub fn upsert_self(pool: &PoolContext, identity: &SelfIdentity) -> Result<AgentRecord> {
    let registry = pool.pool.registry();
    // The record carrying the most identity: agent when present, else the
    // pane/terminal record (shells).
    let primary = identity.agent.as_ref().unwrap_or(&identity.pane);
    let terminal_id = primary
        .terminal_id
        .clone()
        .or_else(|| identity.pane.terminal_id.clone());

    let existing = terminal_id
        .as_deref()
        .and_then(|t| registry.get_by_terminal_any(t).ok().flatten())
        .or_else(|| {
            registry
                .find_by_pane(&primary.handle)
                .ok()?
                .into_iter()
                .find(|r| r.status != STATUS_OFFLINE)
        });
    let had_existing = existing.is_some();

    let now = now_millis();
    // Plain panes carry no agent readiness — preserve any stored values,
    // defaulting to ready (no launch pending). Detected agents report
    // their own flags.
    let (interactive_ready, launch_pending) = match &identity.agent {
        Some(agent) => (agent.interactive_ready, agent.launch_pending),
        None => (
            existing
                .as_ref()
                .map(|e| e.interactive_ready)
                .unwrap_or(true),
            existing.as_ref().map(|e| e.launch_pending).unwrap_or(false),
        ),
    };

    let record = AgentRecord {
        agent_id: existing
            .as_ref()
            .map(|e| e.agent_id.clone())
            .unwrap_or_else(new_agent_id),
        scope_key: pool.scope_key.as_str().to_string(),
        terminal_id,
        pane_id: Some(primary.handle.clone()),
        agent_kind: primary.kind.clone(),
        name: primary.name.clone(),
        cwd: Some(pool.canonical_path.to_string_lossy().into_owned()),
        status: primary.status.as_str().to_string(),
        interactive_ready,
        launch_pending,
        bootstrap_version: existing.as_ref().and_then(|e| e.bootstrap_version),
        first_seen_at: existing.map(|e| e.first_seen_at).unwrap_or(now),
        last_seen_at: now,
        offline_at: None,
    };

    // Plugin surfaces (panel/popup) resolve identity through
    // HERDR_PLUGIN_CONTEXT_JSON to the *focused* pane. When that pane
    // hosts no agent and has no registry record yet, persisting one would
    // mint a ghost peer — visible in `peers`/`broadcast` but never
    // promptable. Skip the write; the returned record still gives callers
    // a self id for this invocation. An existing record is refreshed
    // as usual (the pane registered itself earlier), and a focused pane
    // WITH an agent upserts the real agent's record — just `last_seen`.
    // hc has no plugin panes, so `from_plugin_context` is never set there.
    let ghost = identity.env.from_plugin_context && identity.agent.is_none() && !had_existing;
    if !ghost {
        registry.upsert_agent(&record)?;
    }
    Ok(record)
}

/// Cheap registry reconcile: `AgentRuntime::list_agents` → upsert every
/// agent whose scope matches this pool, then mark stored live *agent*
/// records that vanished offline.
///
/// Records with `agent_kind: None` (plain shell panes/terminals) are
/// never marked offline here — authoritative offline marking belongs to
/// the router daemon's event-driven sync.
///
/// Returns the number of agent records upserted.
pub fn refresh_registry(runtime: &dyn AgentRuntime, pool: &PoolContext) -> Result<usize> {
    let agents = runtime.list_agents()?;
    let registry = pool.pool.registry();
    let mut synced = 0usize;
    // (terminal_id, handle) pairs seen live in *this* scope.
    let mut live_terms: Vec<String> = Vec::new();
    let mut live_handles: Vec<String> = Vec::new();

    for agent in &agents {
        let scope = match scope_for_runtime(runtime, agent) {
            Ok(scope) => scope,
            // Agent's cwd is gone/unresolvable — skip, don't fail the command.
            Err(CoreError::ScopeUnresolved(_)) => continue,
            Err(err) => return Err(err),
        };
        if scope.scope_key != pool.scope_key {
            continue; // never cross scopes
        }
        let record = record_for_agent(&registry, agent, &scope);
        registry.upsert_agent(&record)?;
        if let Some(term) = &agent.terminal_id {
            live_terms.push(term.clone());
        }
        live_handles.push(agent.handle.clone());
        synced += 1;
    }

    // Mark offline: stored live agent-kind records absent from the
    // backend's live list.
    for stored in registry.list_agents(false)? {
        if stored.agent_kind.is_none() {
            continue;
        }
        let term_live = stored
            .terminal_id
            .as_deref()
            .is_some_and(|t| live_terms.iter().any(|x| x == t));
        let handle_live = stored
            .pane_id
            .as_deref()
            .is_some_and(|p| live_handles.iter().any(|x| x == p));
        if !term_live && !handle_live {
            if let Err(err) = registry.mark_offline(&stored.agent_id) {
                tracing::debug!(agent = %stored.agent_id, error = %err, "mark_offline skipped");
            }
        }
    }
    Ok(synced)
}

/// Resolve a [`RuntimeAgent`]'s scope, falling back to the backend's
/// process-info cwds once when the record carries no cwd fields (herdr
/// `pane.process_info`; hc returns an empty list). [`CoreError::
/// ScopeUnresolved`] is preserved so callers can skip unresolvable
/// agents without failing.
fn scope_for_runtime(runtime: &dyn AgentRuntime, agent: &RuntimeAgent) -> Result<ResolvedScope> {
    match resolve_scope_for_runtime(agent, &[]) {
        Ok(scope) => Ok(scope),
        Err(CoreError::ScopeUnresolved(_)) => {
            let cwds = runtime.process_info_cwds(&agent.handle)?;
            resolve_scope_for_runtime(agent, &cwds)
        }
        Err(err) => Err(err),
    }
}

/// Unix epoch milliseconds — matches the store's timestamp convention.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
