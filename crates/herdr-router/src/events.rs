//! `events.subscribe` wiring for the daemon.
//!
//! ## Subscription set (documented choice)
//!
//! Static lifecycle subscriptions:
//!
//! | subscription | why |
//! |---|---|
//! | `pane.agent_detected` | new/released agent detection — primary registry signal |
//! | `pane.created` / `pane.updated` | pane lifecycle + metadata changes (agent label, cwd) |
//! | `pane.moved` | `pane_id` re-key (agent_id must stay stable) |
//! | `pane.closed` / `pane.exited` | agent offline marking |
//! | `workspace.closed` / `tab.closed` | cascade closes → full `agent.list` reconcile |
//!
//! Plus one `pane.agent_status_changed` subscription **per watched agent
//! pane** — the server requires a concrete `pane_id` for this kind (there
//! is no global variant), so [`RegistrySync::watched_panes`] feeds them and
//! the event loop resubscribes when the set changes.
//!
//! Deliberately NOT subscribed: `pane.output_changed` /
//! `pane.output_matched` / `pane.scroll_changed` (high volume, irrelevant
//! to the registry), `workspace.*`/`tab.*` create/focus/rename/move
//! (registry only cares about closes; the 60s reconcile covers drift),
//! `layout.updated`, `worktree.*`.
//!
//! `[[events]]` plugin hooks are intentionally absent from the manifest:
//! the daemon's own subscribe loop is authoritative and reconnects with
//! backoff, so per-event hook processes would only add latency and
//! duplicate reconciles. `tick` remains for manual/diagnostic use.

use std::collections::BTreeSet;

use herdr_protocol::Subscription;

/// The static lifecycle subscriptions (see module docs for rationale).
pub fn static_subscriptions() -> Vec<Subscription> {
    vec![
        Subscription::PaneAgentDetected,
        Subscription::PaneCreated,
        Subscription::PaneUpdated,
        Subscription::PaneMoved,
        Subscription::PaneClosed,
        Subscription::PaneExited,
        Subscription::WorkspaceClosed,
        Subscription::TabClosed,
    ]
}

/// Full subscription list: static lifecycle kinds plus a
/// `pane.agent_status_changed` filter for every watched pane id.
pub fn subscriptions_for(watched_panes: &BTreeSet<String>) -> Vec<Subscription> {
    let mut subs = static_subscriptions();
    for pane_id in watched_panes {
        subs.push(Subscription::PaneAgentStatusChanged {
            pane_id: pane_id.clone(),
            agent_status: None,
        });
    }
    subs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_set_covers_registry_relevant_lifecycle() {
        let names: Vec<String> = static_subscriptions()
            .iter()
            .map(|s| serde_json::to_value(s).unwrap()["type"].as_str().unwrap().to_string())
            .collect();
        for expected in [
            "pane.agent_detected",
            "pane.created",
            "pane.updated",
            "pane.moved",
            "pane.closed",
            "pane.exited",
            "workspace.closed",
            "tab.closed",
        ] {
            assert!(names.iter().any(|n| n == expected), "missing {expected}");
        }
        // High-volume kinds must stay out.
        assert!(!names.iter().any(|n| n.contains("output")));
        assert!(!names.iter().any(|n| n.contains("scroll")));
        assert!(!names.iter().any(|n| n == "layout.updated"));
    }

    #[test]
    fn watched_panes_become_status_subscriptions() {
        let watched: BTreeSet<String> = ["w1:p1".to_string(), "w2:p3".to_string()]
            .into_iter()
            .collect();
        let subs = subscriptions_for(&watched);
        let status_subs: Vec<_> = subs
            .iter()
            .filter(|s| matches!(s, Subscription::PaneAgentStatusChanged { .. }))
            .collect();
        assert_eq!(status_subs.len(), 2);
        for s in status_subs {
            if let Subscription::PaneAgentStatusChanged { pane_id, agent_status } = s {
                assert!(watched.contains(pane_id));
                assert!(agent_status.is_none());
            }
        }
    }
}
