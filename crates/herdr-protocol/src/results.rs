//! Wire envelope and response `result` types.
//!
//! Request:  `{"id":"<string>","method":"<dotted>","params":{...}}`
//! Response: `{"id":"<string>","result":{"type":"<snake>",...}}`
//!       or  `{"id":"<string>","error":{"code":"...","message":"..."}}`
//!
//! [`ResponseResult`] decodes the `result` object by its `type`
//! discriminator. Unknown `type` values decode to
//! [`ResponseResult::Unknown`] preserving the raw JSON, so newer servers
//! never break older clients.

use serde::{Deserialize, Serialize};

use crate::types::*;

/// Error body inside an error response envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

/// A request envelope written to the socket (one JSON object per line).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Request {
    pub id: String,
    pub method: String,
    pub params: serde_json::Value,
}

/// A response envelope read from the socket.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ResponseEnvelope {
    pub id: String,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<ErrorBody>,
}

// ---------------------------------------------------------------------------
// Result payload structs (the `result` object minus its `type` tag).
// ---------------------------------------------------------------------------

/// `pong` result (`ping`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PongResult {
    pub version: String,
    pub protocol: u64,
    #[serde(default)]
    pub capabilities: Option<ServerCapabilities>,
}

/// `session_snapshot` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshotResult {
    pub snapshot: SessionSnapshot,
}

/// Single-workspace results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceResult {
    pub workspace: WorkspaceInfo,
}

/// `workspace_created` / `worktree_created`-style results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceCreatedResult {
    pub workspace: WorkspaceInfo,
    pub tab: TabInfo,
    pub root_pane: PaneInfo,
}

/// `workspace_list` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceListResult {
    pub workspaces: Vec<WorkspaceInfo>,
}

/// `worktree_list` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorktreeListResult {
    pub source: WorktreeSourceInfo,
    pub worktrees: Vec<WorktreeInfo>,
}

/// `worktree_created` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorktreeCreatedResult {
    pub workspace: WorkspaceInfo,
    pub tab: TabInfo,
    pub root_pane: PaneInfo,
    pub worktree: WorktreeInfo,
}

/// `worktree_opened` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorktreeOpenedResult {
    pub workspace: WorkspaceInfo,
    pub tab: TabInfo,
    pub root_pane: PaneInfo,
    pub worktree: WorktreeInfo,
    pub already_open: bool,
}

/// `worktree_removed` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorktreeRemovedResult {
    pub workspace_id: String,
    pub path: String,
    pub forced: bool,
}

/// Single-tab results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabResult {
    pub tab: TabInfo,
}

/// `tab_created` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabCreatedResult {
    pub tab: TabInfo,
    pub root_pane: PaneInfo,
}

/// `tab_list` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabListResult {
    pub tabs: Vec<TabInfo>,
}

/// Single-agent results (`agent_info`, `agent_prompted`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentResult {
    pub agent: AgentInfo,
}

/// `agent_started` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentStartedResult {
    pub agent: AgentInfo,
    pub argv: Vec<String>,
}

/// `agent_list` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentListResult {
    pub agents: Vec<AgentInfo>,
}

/// `agent_view` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentViewResultPayload {
    pub active: bool,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

/// `agent_explain` result — free-form explanation payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentExplainResult {
    pub explain: serde_json::Value,
}

/// Single-pane results (`pane_info`, `pane_current`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneResult {
    pub pane: PaneInfo,
}

/// `pane_list` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneListResult {
    pub panes: Vec<PaneInfo>,
}

/// `pane_swap` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneSwapResultPayload {
    pub swap: PaneSwapResult,
}

/// `pane_move` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneMoveResultPayload {
    pub move_result: PaneMoveResult,
}

/// `pane_zoom` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneZoomResultPayload {
    pub zoom: PaneZoomResult,
}

/// `pane_layout` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneLayoutResultPayload {
    pub layout: PaneLayoutSnapshot,
}

/// `pane_process_info` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneProcessInfoResultPayload {
    pub process_info: PaneProcessInfo,
}

/// `layout_export` / `layout_apply` / `layout_split_ratio_set` results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutResult {
    pub layout: LayoutDescription,
}

/// `pane_neighbor` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneNeighborResultPayload {
    pub neighbor: PaneNeighborResult,
}

/// `pane_edges` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneEdgesResultPayload {
    pub edges: PaneEdgesResult,
}

/// `pane_focus_direction` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneFocusDirectionResultPayload {
    pub focus: PaneFocusDirectionResult,
}

/// `pane_resize` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneResizeResultPayload {
    pub resize: PaneResizeResult,
}

/// `pane_read` result (also returned by `agent.read`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneReadResultPayload {
    pub read: PaneReadResult,
}

/// `pane_selection` result (`pane.selection.read`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneSelectionResultPayload {
    pub pane_id: String,
    pub text: String,
}

/// `pane_copy_motion` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneCopyMotionResultPayload {
    pub pane_id: String,
    pub content_revision: u64,
    pub cursor: PaneTextPoint,
}

/// `pane_copy_search` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneCopySearchResultPayload {
    pub pane_id: String,
    pub content_revision: u64,
    pub total: u64,
    #[serde(default)]
    pub current: Option<u64>,
    #[serde(default)]
    pub current_global: Option<u64>,
    pub matches: Vec<PaneTextRange>,
}

/// `wait_matched` result (`events.wait`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WaitMatchedResult {
    pub event: crate::events::EventEnvelope,
}

/// `output_matched` result (`pane.wait_for_output`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputMatchedResultPayload {
    pub pane_id: String,
    pub revision: u64,
    pub read: PaneReadResult,
    #[serde(default)]
    pub matched_line: Option<String>,
}

/// `notification_show` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotificationShowResultPayload {
    pub shown: bool,
    pub reason: NotificationShowReason,
}

/// `client_window_title` result (`client.window_title.set`/`clear`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientWindowTitleResultPayload {
    pub changed: bool,
    pub reason: ClientWindowTitleReason,
}

/// `integration_list` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntegrationListResult {
    pub integrations: Vec<IntegrationInfo>,
}

/// `integration_install` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntegrationInstallResultPayload {
    pub target: IntegrationTarget,
    pub details: IntegrationMessages,
}

/// `integration_uninstall` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntegrationUninstallResultPayload {
    pub target: IntegrationTarget,
    pub details: IntegrationMessages,
}

/// `pane_link_resolved` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneLinkResolvedResultPayload {
    pub regions: Vec<PaneLinkRegion>,
}

/// `pane_link_activated` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneLinkActivatedResultPayload {
    pub handled: bool,
    #[serde(default)]
    pub url: Option<String>,
}

/// `client_shell_surface_set` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientShellSurfaceSetResultPayload {
    pub active: bool,
    pub projection_revision: u64,
}

/// Unit result for `ok` and `subscription_started`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmptyResult {}

// ---------------------------------------------------------------------------
// ResponseResult — tagged dispatch on `type`.
// ---------------------------------------------------------------------------

/// Decoded `result` object from a Herdr response.
///
/// Known `type` discriminators decode into typed payloads; anything else —
/// including result types added by newer servers — decodes to
/// [`ResponseResult::Unknown`] with the raw JSON preserved.
// Decode-heavy result enum matched in ~100 client methods; boxing large
// variants would hurt ergonomics for transient values.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum ResponseResult {
    /// `{"type":"pong",...}`
    Pong(PongResult),
    /// `{"type":"session_snapshot",...}`
    SessionSnapshot(SessionSnapshotResult),
    /// `{"type":"workspace_info",...}`
    WorkspaceInfo(WorkspaceResult),
    /// `{"type":"workspace_created",...}`
    WorkspaceCreated(WorkspaceCreatedResult),
    /// `{"type":"workspace_list",...}`
    WorkspaceList(WorkspaceListResult),
    /// `{"type":"worktree_list",...}`
    WorktreeList(WorktreeListResult),
    /// `{"type":"worktree_created",...}`
    WorktreeCreated(WorktreeCreatedResult),
    /// `{"type":"worktree_opened",...}`
    WorktreeOpened(WorktreeOpenedResult),
    /// `{"type":"worktree_removed",...}`
    WorktreeRemoved(WorktreeRemovedResult),
    /// `{"type":"tab_info",...}`
    TabInfo(TabResult),
    /// `{"type":"tab_created",...}`
    TabCreated(TabCreatedResult),
    /// `{"type":"tab_list",...}`
    TabList(TabListResult),
    /// `{"type":"agent_info",...}`
    AgentInfo(AgentResult),
    /// `{"type":"agent_prompted",...}`
    AgentPrompted(AgentResult),
    /// `{"type":"agent_started",...}`
    AgentStarted(AgentStartedResult),
    /// `{"type":"agent_list",...}`
    AgentList(AgentListResult),
    /// `{"type":"agent_view",...}`
    AgentView(AgentViewResultPayload),
    /// `{"type":"agent_explain",...}`
    AgentExplain(AgentExplainResult),
    /// `{"type":"pane_info",...}`
    PaneInfo(PaneResult),
    /// `{"type":"pane_current",...}`
    PaneCurrent(PaneResult),
    /// `{"type":"pane_list",...}`
    PaneList(PaneListResult),
    /// `{"type":"pane_swap",...}`
    PaneSwap(PaneSwapResultPayload),
    /// `{"type":"pane_move",...}`
    PaneMove(PaneMoveResultPayload),
    /// `{"type":"pane_zoom",...}`
    PaneZoom(PaneZoomResultPayload),
    /// `{"type":"pane_layout",...}`
    PaneLayout(PaneLayoutResultPayload),
    /// `{"type":"pane_process_info",...}`
    PaneProcessInfo(PaneProcessInfoResultPayload),
    /// `{"type":"layout_export",...}`
    LayoutExport(LayoutResult),
    /// `{"type":"layout_apply",...}`
    LayoutApply(LayoutResult),
    /// `{"type":"layout_split_ratio_set",...}`
    LayoutSplitRatioSet(LayoutResult),
    /// `{"type":"pane_neighbor",...}`
    PaneNeighbor(PaneNeighborResultPayload),
    /// `{"type":"pane_edges",...}`
    PaneEdges(PaneEdgesResultPayload),
    /// `{"type":"pane_focus_direction",...}`
    PaneFocusDirection(PaneFocusDirectionResultPayload),
    /// `{"type":"pane_resize",...}`
    PaneResize(PaneResizeResultPayload),
    /// `{"type":"pane_read",...}` (also `agent.read`)
    PaneRead(PaneReadResultPayload),
    /// `{"type":"pane_selection",...}`
    PaneSelection(PaneSelectionResultPayload),
    /// `{"type":"pane_copy_motion",...}`
    PaneCopyMotion(PaneCopyMotionResultPayload),
    /// `{"type":"pane_copy_search",...}`
    PaneCopySearch(PaneCopySearchResultPayload),
    /// `{"type":"subscription_started"}`
    SubscriptionStarted,
    /// `{"type":"wait_matched",...}` (`events.wait`)
    WaitMatched(WaitMatchedResult),
    /// `{"type":"output_matched",...}` (`pane.wait_for_output` push)
    OutputMatched(OutputMatchedResultPayload),
    /// `{"type":"notification_show",...}`
    NotificationShow(NotificationShowResultPayload),
    /// `{"type":"client_window_title",...}`
    ClientWindowTitle(ClientWindowTitleResultPayload),
    /// `{"type":"integration_list",...}`
    IntegrationList(IntegrationListResult),
    /// `{"type":"integration_install",...}`
    IntegrationInstall(IntegrationInstallResultPayload),
    /// `{"type":"integration_uninstall",...}`
    IntegrationUninstall(IntegrationUninstallResultPayload),
    /// `{"type":"pane_link_resolved",...}`
    PaneLinkResolved(PaneLinkResolvedResultPayload),
    /// `{"type":"pane_link_activated",...}`
    PaneLinkActivated(PaneLinkActivatedResultPayload),
    /// `{"type":"client_shell_surface_set",...}`
    ClientShellSurfaceSet(ClientShellSurfaceSetResultPayload),
    /// `{"type":"ok"}` — generic success acknowledgement.
    Ok,
    /// Any result type not known to this client version.
    Unknown {
        /// The unrecognized `type` string.
        type_name: String,
        /// The full raw `result` object.
        raw: serde_json::Value,
    },
}

impl ResponseResult {
    /// The `type` discriminator of this result.
    pub fn kind(&self) -> &str {
        match self {
            ResponseResult::Pong(_) => "pong",
            ResponseResult::SessionSnapshot(_) => "session_snapshot",
            ResponseResult::WorkspaceInfo(_) => "workspace_info",
            ResponseResult::WorkspaceCreated(_) => "workspace_created",
            ResponseResult::WorkspaceList(_) => "workspace_list",
            ResponseResult::WorktreeList(_) => "worktree_list",
            ResponseResult::WorktreeCreated(_) => "worktree_created",
            ResponseResult::WorktreeOpened(_) => "worktree_opened",
            ResponseResult::WorktreeRemoved(_) => "worktree_removed",
            ResponseResult::TabInfo(_) => "tab_info",
            ResponseResult::TabCreated(_) => "tab_created",
            ResponseResult::TabList(_) => "tab_list",
            ResponseResult::AgentInfo(_) => "agent_info",
            ResponseResult::AgentPrompted(_) => "agent_prompted",
            ResponseResult::AgentStarted(_) => "agent_started",
            ResponseResult::AgentList(_) => "agent_list",
            ResponseResult::AgentView(_) => "agent_view",
            ResponseResult::AgentExplain(_) => "agent_explain",
            ResponseResult::PaneInfo(_) => "pane_info",
            ResponseResult::PaneCurrent(_) => "pane_current",
            ResponseResult::PaneList(_) => "pane_list",
            ResponseResult::PaneSwap(_) => "pane_swap",
            ResponseResult::PaneMove(_) => "pane_move",
            ResponseResult::PaneZoom(_) => "pane_zoom",
            ResponseResult::PaneLayout(_) => "pane_layout",
            ResponseResult::PaneProcessInfo(_) => "pane_process_info",
            ResponseResult::LayoutExport(_) => "layout_export",
            ResponseResult::LayoutApply(_) => "layout_apply",
            ResponseResult::LayoutSplitRatioSet(_) => "layout_split_ratio_set",
            ResponseResult::PaneNeighbor(_) => "pane_neighbor",
            ResponseResult::PaneEdges(_) => "pane_edges",
            ResponseResult::PaneFocusDirection(_) => "pane_focus_direction",
            ResponseResult::PaneResize(_) => "pane_resize",
            ResponseResult::PaneRead(_) => "pane_read",
            ResponseResult::PaneSelection(_) => "pane_selection",
            ResponseResult::PaneCopyMotion(_) => "pane_copy_motion",
            ResponseResult::PaneCopySearch(_) => "pane_copy_search",
            ResponseResult::SubscriptionStarted => "subscription_started",
            ResponseResult::WaitMatched(_) => "wait_matched",
            ResponseResult::OutputMatched(_) => "output_matched",
            ResponseResult::NotificationShow(_) => "notification_show",
            ResponseResult::ClientWindowTitle(_) => "client_window_title",
            ResponseResult::IntegrationList(_) => "integration_list",
            ResponseResult::IntegrationInstall(_) => "integration_install",
            ResponseResult::IntegrationUninstall(_) => "integration_uninstall",
            ResponseResult::PaneLinkResolved(_) => "pane_link_resolved",
            ResponseResult::PaneLinkActivated(_) => "pane_link_activated",
            ResponseResult::ClientShellSurfaceSet(_) => "client_shell_surface_set",
            ResponseResult::Ok => "ok",
            ResponseResult::Unknown { type_name, .. } => type_name.as_str(),
        }
    }

    /// Raw JSON for `Unknown` results; `None` for typed variants.
    pub fn raw(&self) -> Option<&serde_json::Value> {
        match self {
            ResponseResult::Unknown { raw, .. } => Some(raw),
            _ => None,
        }
    }
}

macro_rules! decode_result {
    ($value:expr, $ty:ty, $variant:ident) => {
        serde_json::from_value::<$ty>($value).map(ResponseResult::$variant)
    };
}

impl<'de> Deserialize<'de> for ResponseResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let type_name = value
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let result = match type_name.as_str() {
            "pong" => decode_result!(value, PongResult, Pong),
            "session_snapshot" => decode_result!(value, SessionSnapshotResult, SessionSnapshot),
            "workspace_info" => decode_result!(value, WorkspaceResult, WorkspaceInfo),
            "workspace_created" => decode_result!(value, WorkspaceCreatedResult, WorkspaceCreated),
            "workspace_list" => decode_result!(value, WorkspaceListResult, WorkspaceList),
            "worktree_list" => decode_result!(value, WorktreeListResult, WorktreeList),
            "worktree_created" => decode_result!(value, WorktreeCreatedResult, WorktreeCreated),
            "worktree_opened" => decode_result!(value, WorktreeOpenedResult, WorktreeOpened),
            "worktree_removed" => decode_result!(value, WorktreeRemovedResult, WorktreeRemoved),
            "tab_info" => decode_result!(value, TabResult, TabInfo),
            "tab_created" => decode_result!(value, TabCreatedResult, TabCreated),
            "tab_list" => decode_result!(value, TabListResult, TabList),
            "agent_info" => decode_result!(value, AgentResult, AgentInfo),
            "agent_prompted" => decode_result!(value, AgentResult, AgentPrompted),
            "agent_started" => decode_result!(value, AgentStartedResult, AgentStarted),
            "agent_list" => decode_result!(value, AgentListResult, AgentList),
            "agent_view" => decode_result!(value, AgentViewResultPayload, AgentView),
            "agent_explain" => decode_result!(value, AgentExplainResult, AgentExplain),
            "pane_info" => decode_result!(value, PaneResult, PaneInfo),
            "pane_current" => decode_result!(value, PaneResult, PaneCurrent),
            "pane_list" => decode_result!(value, PaneListResult, PaneList),
            "pane_swap" => decode_result!(value, PaneSwapResultPayload, PaneSwap),
            "pane_move" => decode_result!(value, PaneMoveResultPayload, PaneMove),
            "pane_zoom" => decode_result!(value, PaneZoomResultPayload, PaneZoom),
            "pane_layout" => decode_result!(value, PaneLayoutResultPayload, PaneLayout),
            "pane_process_info" => {
                decode_result!(value, PaneProcessInfoResultPayload, PaneProcessInfo)
            }
            "layout_export" => decode_result!(value, LayoutResult, LayoutExport),
            "layout_apply" => decode_result!(value, LayoutResult, LayoutApply),
            "layout_split_ratio_set" => {
                decode_result!(value, LayoutResult, LayoutSplitRatioSet)
            }
            "pane_neighbor" => decode_result!(value, PaneNeighborResultPayload, PaneNeighbor),
            "pane_edges" => decode_result!(value, PaneEdgesResultPayload, PaneEdges),
            "pane_focus_direction" => {
                decode_result!(value, PaneFocusDirectionResultPayload, PaneFocusDirection)
            }
            "pane_resize" => decode_result!(value, PaneResizeResultPayload, PaneResize),
            "pane_read" => decode_result!(value, PaneReadResultPayload, PaneRead),
            "pane_selection" => decode_result!(value, PaneSelectionResultPayload, PaneSelection),
            "pane_copy_motion" => {
                decode_result!(value, PaneCopyMotionResultPayload, PaneCopyMotion)
            }
            "pane_copy_search" => {
                decode_result!(value, PaneCopySearchResultPayload, PaneCopySearch)
            }
            "subscription_started" => Ok(ResponseResult::SubscriptionStarted),
            "wait_matched" => decode_result!(value, WaitMatchedResult, WaitMatched),
            "output_matched" => decode_result!(value, OutputMatchedResultPayload, OutputMatched),
            "notification_show" => {
                decode_result!(value, NotificationShowResultPayload, NotificationShow)
            }
            "client_window_title" => {
                decode_result!(value, ClientWindowTitleResultPayload, ClientWindowTitle)
            }
            "integration_list" => decode_result!(value, IntegrationListResult, IntegrationList),
            "integration_install" => {
                decode_result!(value, IntegrationInstallResultPayload, IntegrationInstall)
            }
            "integration_uninstall" => {
                decode_result!(
                    value,
                    IntegrationUninstallResultPayload,
                    IntegrationUninstall
                )
            }
            "pane_link_resolved" => {
                decode_result!(value, PaneLinkResolvedResultPayload, PaneLinkResolved)
            }
            "pane_link_activated" => {
                decode_result!(value, PaneLinkActivatedResultPayload, PaneLinkActivated)
            }
            "client_shell_surface_set" => {
                decode_result!(
                    value,
                    ClientShellSurfaceSetResultPayload,
                    ClientShellSurfaceSet
                )
            }
            "ok" => Ok(ResponseResult::Ok),
            _ => Ok(ResponseResult::Unknown {
                type_name,
                raw: value,
            }),
        };
        result.map_err(serde::de::Error::custom)
    }
}
