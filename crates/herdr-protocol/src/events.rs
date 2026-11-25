//! Event and subscription types for the Herdr socket API.
//!
//! Two distinct naming conventions exist on the wire:
//!
//! * **Subscriptions** (`events.subscribe` params and the 3 high-frequency
//!   pushed frames) use *dot* names: `pane.agent_status_changed`,
//!   `pane.output_matched`, `pane.scroll_changed`.
//! * **Lifecycle events** (`events.wait` `match_event`, `wait_matched`
//!   payloads, and lifecycle subscription frames) use *snake* names:
//!   `pane_agent_status_changed`, `workspace_created`, ...
//!
//! Keep the two type families separate — do not mix them.

use serde::{Deserialize, Serialize};

use crate::types::{
    AgentStatus, PaneInfo, PaneLayoutSnapshot, PaneReadResult, PaneScrollInfo, TabInfo,
    WorkspaceInfo, WorktreeInfo,
};

/// Output matcher for `pane.output_matched` subscriptions and
/// `pane.wait_for_output`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputMatch {
    Substring { value: String },
    Regex { value: String },
}

/// A subscription request entry for `events.subscribe`.
///
/// Serialize-only: subscriptions are sent to the server, never received.
/// For subscription types added by newer servers, use
/// [`HerdrClient::call_raw`](crate::HerdrClient::call_raw) with a raw params
/// object.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Subscription {
    #[serde(rename = "workspace.created")]
    WorkspaceCreated,
    #[serde(rename = "workspace.updated")]
    WorkspaceUpdated,
    #[serde(rename = "workspace.metadata_updated")]
    WorkspaceMetadataUpdated,
    #[serde(rename = "workspace.renamed")]
    WorkspaceRenamed,
    #[serde(rename = "workspace.moved")]
    WorkspaceMoved,
    #[serde(rename = "workspace.reordered")]
    WorkspaceReordered,
    #[serde(rename = "workspace.closed")]
    WorkspaceClosed,
    #[serde(rename = "workspace.focused")]
    WorkspaceFocused,
    #[serde(rename = "worktree.created")]
    WorktreeCreated,
    #[serde(rename = "worktree.opened")]
    WorktreeOpened,
    #[serde(rename = "worktree.removed")]
    WorktreeRemoved,
    #[serde(rename = "tab.created")]
    TabCreated,
    #[serde(rename = "tab.closed")]
    TabClosed,
    #[serde(rename = "tab.focused")]
    TabFocused,
    #[serde(rename = "tab.renamed")]
    TabRenamed,
    #[serde(rename = "tab.moved")]
    TabMoved,
    #[serde(rename = "pane.created")]
    PaneCreated,
    #[serde(rename = "pane.closed")]
    PaneClosed,
    #[serde(rename = "pane.updated")]
    PaneUpdated,
    #[serde(rename = "pane.focused")]
    PaneFocused,
    #[serde(rename = "pane.moved")]
    PaneMoved,
    #[serde(rename = "pane.exited")]
    PaneExited,
    #[serde(rename = "pane.agent_detected")]
    PaneAgentDetected,
    #[serde(rename = "pane.output_matched")]
    PaneOutputMatched {
        pane_id: String,
        source: crate::types::ReadSource,
        r#match: OutputMatch,
        #[serde(skip_serializing_if = "Option::is_none")]
        lines: Option<u32>,
        #[serde(skip_serializing_if = "crate::params::is_true")]
        strip_ansi: bool,
    },
    #[serde(rename = "pane.agent_status_changed")]
    PaneAgentStatusChanged {
        pane_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_status: Option<AgentStatus>,
    },
    #[serde(rename = "pane.scroll_changed")]
    PaneScrollChanged { pane_id: String },
    #[serde(rename = "layout.updated")]
    LayoutUpdated,
}

/// `events.wait` `match_event` — lifecycle event matcher.
///
/// Serialize-only. Snake_case event names.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventMatch {
    WorkspaceCreated {
        #[serde(skip_serializing_if = "Option::is_none")]
        workspace_id: Option<String>,
    },
    WorkspaceUpdated {
        workspace_id: String,
    },
    WorkspaceClosed {
        workspace_id: String,
    },
    WorkspaceRenamed {
        workspace_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    WorkspaceMoved {
        workspace_id: String,
    },
    WorkspaceFocused {
        workspace_id: String,
    },
    TabCreated {
        #[serde(skip_serializing_if = "Option::is_none")]
        tab_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        workspace_id: Option<String>,
    },
    TabClosed {
        tab_id: String,
    },
    TabRenamed {
        tab_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    TabMoved {
        tab_id: String,
    },
    TabFocused {
        tab_id: String,
    },
    PaneCreated {
        #[serde(skip_serializing_if = "Option::is_none")]
        pane_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        workspace_id: Option<String>,
    },
    PaneClosed {
        pane_id: String,
    },
    PaneFocused {
        pane_id: String,
    },
    PaneMoved {
        pane_id: String,
    },
    PaneOutputChanged {
        pane_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        min_revision: Option<u64>,
    },
    PaneExited {
        pane_id: String,
    },
    PaneAgentDetected {
        pane_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
    },
    PaneAgentStatusChanged {
        pane_id: String,
        agent_status: AgentStatus,
    },
}

/// Lifecycle event kind — snake_case names on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    WorkspaceCreated,
    WorkspaceUpdated,
    WorkspaceMetadataUpdated,
    WorkspaceClosed,
    WorkspaceRenamed,
    WorkspaceMoved,
    WorkspaceReordered,
    WorkspaceFocused,
    WorktreeCreated,
    WorktreeOpened,
    WorktreeRemoved,
    TabCreated,
    TabClosed,
    TabRenamed,
    TabMoved,
    TabFocused,
    PaneCreated,
    PaneClosed,
    PaneUpdated,
    PaneFocused,
    PaneMoved,
    PaneOutputChanged,
    PaneExited,
    PaneAgentDetected,
    PaneAgentStatusChanged,
    LayoutUpdated,
    #[serde(other)]
    Unknown,
}

impl EventKind {
    /// The dot-separated subscription name for this kind, if it has one.
    pub fn dot_name(&self) -> Option<&'static str> {
        Some(match self {
            EventKind::WorkspaceCreated => "workspace.created",
            EventKind::WorkspaceUpdated => "workspace.updated",
            EventKind::WorkspaceMetadataUpdated => "workspace.metadata_updated",
            EventKind::WorkspaceClosed => "workspace.closed",
            EventKind::WorkspaceRenamed => "workspace.renamed",
            EventKind::WorkspaceMoved => "workspace.moved",
            EventKind::WorkspaceReordered => "workspace.reordered",
            EventKind::WorkspaceFocused => "workspace.focused",
            EventKind::WorktreeCreated => "worktree.created",
            EventKind::WorktreeOpened => "worktree.opened",
            EventKind::WorktreeRemoved => "worktree.removed",
            EventKind::TabCreated => "tab.created",
            EventKind::TabClosed => "tab.closed",
            EventKind::TabRenamed => "tab.renamed",
            EventKind::TabMoved => "tab.moved",
            EventKind::TabFocused => "tab.focused",
            EventKind::PaneCreated => "pane.created",
            EventKind::PaneClosed => "pane.closed",
            EventKind::PaneUpdated => "pane.updated",
            EventKind::PaneFocused => "pane.focused",
            EventKind::PaneMoved => "pane.moved",
            EventKind::PaneOutputChanged => "pane.output_changed",
            EventKind::PaneExited => "pane.exited",
            EventKind::PaneAgentDetected => "pane.agent_detected",
            EventKind::PaneAgentStatusChanged => "pane.agent_status_changed",
            EventKind::LayoutUpdated => "layout.updated",
            EventKind::Unknown => return None,
        })
    }
}

/// Lifecycle event payload, tagged by `type` (snake_case).
///
/// Unknown `type` values decode to [`EventData::Unknown`] which preserves the
/// raw JSON payload for forward compatibility.
// Decode-heavy enum: boxing every variant would complicate the ~30 match
// sites in the client for negligible gain (values are transient).
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum EventData {
    WorkspaceCreated {
        workspace: WorkspaceInfo,
    },
    WorkspaceUpdated {
        workspace: WorkspaceInfo,
    },
    WorkspaceMetadataUpdated {
        workspace: WorkspaceInfo,
    },
    WorkspaceClosed {
        workspace_id: String,
        #[allow(dead_code)]
        workspace: Option<WorkspaceInfo>,
    },
    WorkspaceRenamed {
        workspace_id: String,
        label: String,
    },
    WorkspaceMoved {
        workspace_id: String,
        insert_index: u64,
        workspaces: Vec<WorkspaceInfo>,
    },
    WorkspaceReordered {
        before_workspace_id: Option<String>,
        workspace_ids: Vec<String>,
        workspaces: Vec<WorkspaceInfo>,
    },
    WorkspaceFocused {
        workspace_id: String,
    },
    WorktreeCreated {
        workspace: WorkspaceInfo,
        worktree: WorktreeInfo,
    },
    WorktreeOpened {
        workspace: WorkspaceInfo,
        worktree: WorktreeInfo,
        already_open: bool,
    },
    WorktreeRemoved {
        workspace_id: String,
        forced: bool,
        worktree: WorktreeInfo,
        workspace: Option<WorkspaceInfo>,
    },
    TabCreated {
        tab: TabInfo,
    },
    TabClosed {
        tab_id: String,
        workspace_id: String,
    },
    TabRenamed {
        tab_id: String,
        workspace_id: String,
        label: String,
    },
    TabMoved {
        tab_id: String,
        workspace_id: String,
        insert_index: u64,
        tabs: Vec<TabInfo>,
    },
    TabFocused {
        tab_id: String,
        workspace_id: String,
    },
    PaneCreated {
        pane: PaneInfo,
    },
    PaneClosed {
        pane_id: String,
        workspace_id: String,
    },
    PaneUpdated {
        pane: PaneInfo,
    },
    PaneFocused {
        pane_id: String,
        workspace_id: String,
    },
    PaneMoved {
        pane: PaneInfo,
        previous_pane_id: String,
        previous_workspace_id: String,
        previous_tab_id: String,
        created_workspace: Option<WorkspaceInfo>,
        created_tab: Option<TabInfo>,
        closed_workspace_id: Option<String>,
        closed_tab_id: Option<String>,
    },
    PaneOutputChanged {
        pane_id: String,
        workspace_id: String,
        revision: u64,
    },
    PaneExited {
        pane_id: String,
        workspace_id: String,
    },
    PaneAgentDetected {
        pane_id: String,
        workspace_id: String,
        agent: Option<String>,
        final_status: Option<AgentStatus>,
        released: bool,
    },
    PaneAgentStatusChanged {
        pane_id: String,
        workspace_id: String,
        agent_status: AgentStatus,
        agent: Option<String>,
        title: Option<String>,
        display_agent: Option<String>,
        state_labels: std::collections::HashMap<String, String>,
    },
    LayoutUpdated {
        layout: PaneLayoutSnapshot,
    },
    /// Unknown `type` — preserves the raw payload for forward compat.
    Unknown {
        /// The unrecognized `type` string.
        type_name: String,
        /// Raw event payload including the `type` field.
        raw: serde_json::Value,
    },
}

impl EventData {
    /// The `type` discriminator of this event payload.
    pub fn type_name(&self) -> &str {
        match self {
            EventData::WorkspaceCreated { .. } => "workspace_created",
            EventData::WorkspaceUpdated { .. } => "workspace_updated",
            EventData::WorkspaceMetadataUpdated { .. } => "workspace_metadata_updated",
            EventData::WorkspaceClosed { .. } => "workspace_closed",
            EventData::WorkspaceRenamed { .. } => "workspace_renamed",
            EventData::WorkspaceMoved { .. } => "workspace_moved",
            EventData::WorkspaceReordered { .. } => "workspace_reordered",
            EventData::WorkspaceFocused { .. } => "workspace_focused",
            EventData::WorktreeCreated { .. } => "worktree_created",
            EventData::WorktreeOpened { .. } => "worktree_opened",
            EventData::WorktreeRemoved { .. } => "worktree_removed",
            EventData::TabCreated { .. } => "tab_created",
            EventData::TabClosed { .. } => "tab_closed",
            EventData::TabRenamed { .. } => "tab_renamed",
            EventData::TabMoved { .. } => "tab_moved",
            EventData::TabFocused { .. } => "tab_focused",
            EventData::PaneCreated { .. } => "pane_created",
            EventData::PaneClosed { .. } => "pane_closed",
            EventData::PaneUpdated { .. } => "pane_updated",
            EventData::PaneFocused { .. } => "pane_focused",
            EventData::PaneMoved { .. } => "pane_moved",
            EventData::PaneOutputChanged { .. } => "pane_output_changed",
            EventData::PaneExited { .. } => "pane_exited",
            EventData::PaneAgentDetected { .. } => "pane_agent_detected",
            EventData::PaneAgentStatusChanged { .. } => "pane_agent_status_changed",
            EventData::LayoutUpdated { .. } => "layout_updated",
            EventData::Unknown { type_name, .. } => type_name.as_str(),
        }
    }
}

// Serde mirror used for both directions: internally tagged, known variants.
#[derive(Deserialize)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "type", rename_all = "snake_case")]
enum EventDataSerde {
    WorkspaceCreated {
        workspace: WorkspaceInfo,
    },
    WorkspaceUpdated {
        workspace: WorkspaceInfo,
    },
    WorkspaceMetadataUpdated {
        workspace: WorkspaceInfo,
    },
    WorkspaceClosed {
        workspace_id: String,
        #[serde(default)]
        workspace: Option<WorkspaceInfo>,
    },
    WorkspaceRenamed {
        workspace_id: String,
        label: String,
    },
    WorkspaceMoved {
        workspace_id: String,
        insert_index: u64,
        workspaces: Vec<WorkspaceInfo>,
    },
    WorkspaceReordered {
        #[serde(default)]
        before_workspace_id: Option<String>,
        workspace_ids: Vec<String>,
        workspaces: Vec<WorkspaceInfo>,
    },
    WorkspaceFocused {
        workspace_id: String,
    },
    WorktreeCreated {
        workspace: WorkspaceInfo,
        worktree: WorktreeInfo,
    },
    WorktreeOpened {
        workspace: WorkspaceInfo,
        worktree: WorktreeInfo,
        already_open: bool,
    },
    WorktreeRemoved {
        workspace_id: String,
        forced: bool,
        worktree: WorktreeInfo,
        #[serde(default)]
        workspace: Option<WorkspaceInfo>,
    },
    TabCreated {
        tab: TabInfo,
    },
    TabClosed {
        tab_id: String,
        workspace_id: String,
    },
    TabRenamed {
        tab_id: String,
        workspace_id: String,
        label: String,
    },
    TabMoved {
        tab_id: String,
        workspace_id: String,
        insert_index: u64,
        tabs: Vec<TabInfo>,
    },
    TabFocused {
        tab_id: String,
        workspace_id: String,
    },
    PaneCreated {
        pane: PaneInfo,
    },
    PaneClosed {
        pane_id: String,
        workspace_id: String,
    },
    PaneUpdated {
        pane: PaneInfo,
    },
    PaneFocused {
        pane_id: String,
        workspace_id: String,
    },
    PaneMoved {
        pane: PaneInfo,
        previous_pane_id: String,
        previous_workspace_id: String,
        previous_tab_id: String,
        #[serde(default)]
        created_workspace: Option<WorkspaceInfo>,
        #[serde(default)]
        created_tab: Option<TabInfo>,
        #[serde(default)]
        closed_workspace_id: Option<String>,
        #[serde(default)]
        closed_tab_id: Option<String>,
    },
    PaneOutputChanged {
        pane_id: String,
        workspace_id: String,
        revision: u64,
    },
    PaneExited {
        pane_id: String,
        workspace_id: String,
    },
    PaneAgentDetected {
        pane_id: String,
        workspace_id: String,
        #[serde(default)]
        agent: Option<String>,
        #[serde(default)]
        final_status: Option<AgentStatus>,
        released: bool,
    },
    PaneAgentStatusChanged {
        pane_id: String,
        workspace_id: String,
        agent_status: AgentStatus,
        #[serde(default)]
        agent: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        display_agent: Option<String>,
        #[serde(default)]
        state_labels: std::collections::HashMap<String, String>,
    },
    LayoutUpdated {
        layout: PaneLayoutSnapshot,
    },
}

impl From<EventDataSerde> for EventData {
    fn from(value: EventDataSerde) -> Self {
        match value {
            EventDataSerde::WorkspaceCreated { workspace } => {
                EventData::WorkspaceCreated { workspace }
            }
            EventDataSerde::WorkspaceUpdated { workspace } => {
                EventData::WorkspaceUpdated { workspace }
            }
            EventDataSerde::WorkspaceMetadataUpdated { workspace } => {
                EventData::WorkspaceMetadataUpdated { workspace }
            }
            EventDataSerde::WorkspaceClosed {
                workspace_id,
                workspace,
            } => EventData::WorkspaceClosed {
                workspace_id,
                workspace,
            },
            EventDataSerde::WorkspaceRenamed {
                workspace_id,
                label,
            } => EventData::WorkspaceRenamed {
                workspace_id,
                label,
            },
            EventDataSerde::WorkspaceMoved {
                workspace_id,
                insert_index,
                workspaces,
            } => EventData::WorkspaceMoved {
                workspace_id,
                insert_index,
                workspaces,
            },
            EventDataSerde::WorkspaceReordered {
                before_workspace_id,
                workspace_ids,
                workspaces,
            } => EventData::WorkspaceReordered {
                before_workspace_id,
                workspace_ids,
                workspaces,
            },
            EventDataSerde::WorkspaceFocused { workspace_id } => {
                EventData::WorkspaceFocused { workspace_id }
            }
            EventDataSerde::WorktreeCreated {
                workspace,
                worktree,
            } => EventData::WorktreeCreated {
                workspace,
                worktree,
            },
            EventDataSerde::WorktreeOpened {
                workspace,
                worktree,
                already_open,
            } => EventData::WorktreeOpened {
                workspace,
                worktree,
                already_open,
            },
            EventDataSerde::WorktreeRemoved {
                workspace_id,
                forced,
                worktree,
                workspace,
            } => EventData::WorktreeRemoved {
                workspace_id,
                forced,
                worktree,
                workspace,
            },
            EventDataSerde::TabCreated { tab } => EventData::TabCreated { tab },
            EventDataSerde::TabClosed {
                tab_id,
                workspace_id,
            } => EventData::TabClosed {
                tab_id,
                workspace_id,
            },
            EventDataSerde::TabRenamed {
                tab_id,
                workspace_id,
                label,
            } => EventData::TabRenamed {
                tab_id,
                workspace_id,
                label,
            },
            EventDataSerde::TabMoved {
                tab_id,
                workspace_id,
                insert_index,
                tabs,
            } => EventData::TabMoved {
                tab_id,
                workspace_id,
                insert_index,
                tabs,
            },
            EventDataSerde::TabFocused {
                tab_id,
                workspace_id,
            } => EventData::TabFocused {
                tab_id,
                workspace_id,
            },
            EventDataSerde::PaneCreated { pane } => EventData::PaneCreated { pane },
            EventDataSerde::PaneClosed {
                pane_id,
                workspace_id,
            } => EventData::PaneClosed {
                pane_id,
                workspace_id,
            },
            EventDataSerde::PaneUpdated { pane } => EventData::PaneUpdated { pane },
            EventDataSerde::PaneFocused {
                pane_id,
                workspace_id,
            } => EventData::PaneFocused {
                pane_id,
                workspace_id,
            },
            EventDataSerde::PaneMoved {
                pane,
                previous_pane_id,
                previous_workspace_id,
                previous_tab_id,
                created_workspace,
                created_tab,
                closed_workspace_id,
                closed_tab_id,
            } => EventData::PaneMoved {
                pane,
                previous_pane_id,
                previous_workspace_id,
                previous_tab_id,
                created_workspace,
                created_tab,
                closed_workspace_id,
                closed_tab_id,
            },
            EventDataSerde::PaneOutputChanged {
                pane_id,
                workspace_id,
                revision,
            } => EventData::PaneOutputChanged {
                pane_id,
                workspace_id,
                revision,
            },
            EventDataSerde::PaneExited {
                pane_id,
                workspace_id,
            } => EventData::PaneExited {
                pane_id,
                workspace_id,
            },
            EventDataSerde::PaneAgentDetected {
                pane_id,
                workspace_id,
                agent,
                final_status,
                released,
            } => EventData::PaneAgentDetected {
                pane_id,
                workspace_id,
                agent,
                final_status,
                released,
            },
            EventDataSerde::PaneAgentStatusChanged {
                pane_id,
                workspace_id,
                agent_status,
                agent,
                title,
                display_agent,
                state_labels,
            } => EventData::PaneAgentStatusChanged {
                pane_id,
                workspace_id,
                agent_status,
                agent,
                title,
                display_agent,
                state_labels,
            },
            EventDataSerde::LayoutUpdated { layout } => EventData::LayoutUpdated { layout },
        }
    }
}

impl<'de> Deserialize<'de> for EventData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match serde_json::from_value::<EventDataSerde>(value.clone()) {
            Ok(known) => Ok(EventData::from(known)),
            Err(_) => {
                let type_name = value
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                Ok(EventData::Unknown {
                    type_name,
                    raw: value,
                })
            }
        }
    }
}

/// Lifecycle event envelope: `{ "event": "<snake_kind>", "data": { "type": ..., ... } }`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EventEnvelope {
    pub event: EventKind,
    pub data: EventData,
}

/// Kind of a high-frequency subscription frame (dot names).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SubscriptionEventKind {
    #[serde(rename = "pane.output_matched")]
    PaneOutputMatched,
    #[serde(rename = "pane.agent_status_changed")]
    PaneAgentStatusChanged,
    #[serde(rename = "pane.scroll_changed")]
    ScrollChanged,
    #[serde(other)]
    Unknown,
}

/// `pane.output_matched` subscription frame payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneOutputMatchedEvent {
    pub pane_id: String,
    pub matched_line: String,
    pub read: PaneReadResult,
}

/// `pane.agent_status_changed` subscription frame payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneAgentStatusChangedEvent {
    pub pane_id: String,
    pub workspace_id: String,
    pub agent_status: AgentStatus,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub display_agent: Option<String>,
    #[serde(default)]
    pub state_labels: std::collections::HashMap<String, String>,
}

/// `pane.scroll_changed` subscription frame payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneScrollChangedEvent {
    pub pane_id: String,
    pub workspace_id: String,
    pub scroll: PaneScrollInfo,
}

/// Decoded payload of a subscription frame.
#[derive(Debug, Clone, PartialEq)]
pub enum SubscriptionEventData {
    PaneOutputMatched(PaneOutputMatchedEvent),
    PaneAgentStatusChanged(PaneAgentStatusChangedEvent),
    ScrollChanged(PaneScrollChangedEvent),
    /// Unknown dot-name kind — preserves raw JSON.
    Unknown {
        /// The unrecognized event name.
        kind: String,
        /// Raw `data` payload.
        raw: serde_json::Value,
    },
}

/// A `subscription_event` frame: `{ "event": "<dot_kind>", "data": {...} }`.
#[derive(Debug, Clone, PartialEq)]
pub struct SubscriptionEventEnvelope {
    /// Coarse kind classification.
    pub event: SubscriptionEventKind,
    /// The raw `event` string (dot name), kept for `Unknown` kinds.
    pub event_name: String,
    pub data: SubscriptionEventData,
}

impl SubscriptionEventEnvelope {
    /// The dot-name of this frame's event.
    pub fn kind_name(&self) -> &str {
        &self.event_name
    }
}

impl<'de> Deserialize<'de> for SubscriptionEventEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let event_name = value
            .get("event")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let data_value = value
            .get("data")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let (kind, data) = match event_name.as_str() {
            "pane.output_matched" => (
                SubscriptionEventKind::PaneOutputMatched,
                SubscriptionEventData::PaneOutputMatched(
                    serde_json::from_value(data_value).map_err(serde::de::Error::custom)?,
                ),
            ),
            "pane.agent_status_changed" => (
                SubscriptionEventKind::PaneAgentStatusChanged,
                SubscriptionEventData::PaneAgentStatusChanged(
                    serde_json::from_value(data_value).map_err(serde::de::Error::custom)?,
                ),
            ),
            "pane.scroll_changed" => (
                SubscriptionEventKind::ScrollChanged,
                SubscriptionEventData::ScrollChanged(
                    serde_json::from_value(data_value).map_err(serde::de::Error::custom)?,
                ),
            ),
            other => (
                SubscriptionEventKind::Unknown,
                SubscriptionEventData::Unknown {
                    kind: other.to_string(),
                    raw: data_value,
                },
            ),
        };
        Ok(SubscriptionEventEnvelope {
            event: kind,
            event_name,
            data,
        })
    }
}

impl Serialize for SubscriptionEventEnvelope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("event", &self.event_name)?;
        match &self.data {
            SubscriptionEventData::PaneOutputMatched(d) => {
                map.serialize_entry("data", d)?;
            }
            SubscriptionEventData::PaneAgentStatusChanged(d) => {
                map.serialize_entry("data", d)?;
            }
            SubscriptionEventData::ScrollChanged(d) => {
                map.serialize_entry("data", d)?;
            }
            SubscriptionEventData::Unknown { raw, .. } => {
                map.serialize_entry("data", raw)?;
            }
        }
        map.end()
    }
}

/// One frame read from an `events.subscribe` stream.
///
/// Lifecycle subscriptions push [`SubscriptionFrame::Event`] frames with
/// snake_case kinds; the three high-frequency subscriptions push
/// [`SubscriptionFrame::SubscriptionEvent`] frames with dot names.
// One variant is much larger than the other; frames are transient decode
// results, so boxing is not worth the ergonomics cost.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum SubscriptionFrame {
    /// Lifecycle event (`{"event": "<snake>", "data": {...}}`).
    Event(EventEnvelope),
    /// High-frequency subscription event (`{"event": "<dot>", "data": {...}}`).
    SubscriptionEvent(SubscriptionEventEnvelope),
}

impl SubscriptionFrame {
    /// Decode one subscription-stream frame from its JSON value.
    ///
    /// Frames carrying an `error` object surface as
    /// [`HerdrError::Api`](crate::HerdrError::Api). Dot-named `event`
    /// fields decode as [`SubscriptionFrame::SubscriptionEvent`]; snake-named
    /// fields decode as [`SubscriptionFrame::Event`].
    pub fn from_json_value(value: serde_json::Value) -> crate::error::Result<Self> {
        use crate::error::HerdrError;
        if let Some(error) = value.get("error") {
            let code = error
                .get("code")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let message = error
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            return Err(HerdrError::Api { code, message });
        }
        let event_name = value.get("event").and_then(|v| v.as_str()).ok_or_else(|| {
            HerdrError::Protocol("subscription frame missing `event` field".to_string())
        })?;
        // Dot names (`pane.output_matched` etc.) are subscription events;
        // snake names are lifecycle events.
        if event_name.contains('.') {
            let envelope: SubscriptionEventEnvelope =
                serde_json::from_value(value).map_err(|e| {
                    HerdrError::Protocol(format!("failed to decode subscription event: {e}"))
                })?;
            Ok(SubscriptionFrame::SubscriptionEvent(envelope))
        } else {
            let envelope: EventEnvelope = serde_json::from_value(value).map_err(|e| {
                HerdrError::Protocol(format!("failed to decode lifecycle event: {e}"))
            })?;
            Ok(SubscriptionFrame::Event(envelope))
        }
    }
}
