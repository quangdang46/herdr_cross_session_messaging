//! Request `params` payloads for Herdr API methods.
//!
//! Mirrors the request `$defs` in `herdr-api.schema.json`. All fields are
//! optional unless marked required; `skip_serializing_if` keeps the wire
//! payload minimal. Serialize-only in practice — the client never receives
//! params back.

use serde::Serialize;

use crate::events::{EventMatch, OutputMatch, Subscription};
use crate::types::{
    AgentStatus, EnvMap, IntegrationTarget, LayoutNode, NotificationShowSound, PaneAgentState,
    PaneDirection, PaneRightClickTarget, PaneTextPoint, PaneTextRange, PaneZoomMode, ReadFormat,
    ReadSource, SplitDirection, ToastPosition,
};

/// Empty params object (`{}`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct EmptyParams {}

/// `agent.prompt` wait options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct AgentPromptWait {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<Vec<AgentStatus>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// `agent.prompt` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentPromptParams {
    pub target: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait: Option<AgentPromptWait>,
}

/// `agent.wait` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct AgentWaitParams {
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<Vec<AgentStatus>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// `true` helper for `skip_serializing_if` on default-true booleans.
pub(crate) fn is_true(value: &bool) -> bool {
    *value
}

/// Shared read-options fields for `agent.read` / `pane.read` / output waits.
///
/// `strip_ansi` defaults to `true` (matching the server default); `false` is
/// always sent explicitly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<ReadFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
    #[serde(skip_serializing_if = "is_true")]
    pub strip_ansi: bool,
}

impl Default for ReadOptions {
    fn default() -> Self {
        Self {
            format: None,
            lines: None,
            strip_ansi: true,
        }
    }
}

/// `agent.read` params.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentReadParams {
    pub target: String,
    pub source: ReadSource,
    #[serde(flatten)]
    pub options: ReadOptions,
}

/// `pane.read` params.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PaneReadParams {
    pub pane_id: String,
    pub source: ReadSource,
    #[serde(flatten)]
    pub options: ReadOptions,
}

/// `agent.send_keys` / `pane.send_keys` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SendKeysParams {
    pub target: String,
    pub keys: Vec<String>,
}

/// `pane.send_keys` params (pane_id keyed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PaneSendKeysParams {
    pub pane_id: String,
    pub keys: Vec<String>,
}

/// `agent.rename` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentRenameParams {
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `agent.start` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct AgentStartParams {
    pub name: String,
    pub kind: String,
    pub pane_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// `agent.view.set` params.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(default)]
pub struct AgentViewSetParams {
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<Vec<serde_json::Value>>,
}

/// `agent.view.clear` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct AgentViewClearParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// `pane.process_info` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct PaneProcessInfoParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
}

/// `layout.export` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct LayoutExportParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
}

/// `layout.apply` params.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(default)]
pub struct LayoutApplyParams {
    pub root: LayoutNode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_label: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub focus: bool,
}

/// `layout.set_split_ratio` params.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(default)]
pub struct LayoutSetSplitRatioParams {
    pub path: Vec<bool>,
    pub ratio: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
}

/// `pane.wait_for_output` params.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(default)]
pub struct PaneWaitForOutputParams {
    pub pane_id: String,
    pub source: ReadSource,
    pub r#match: OutputMatch,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
    #[serde(skip_serializing_if = "is_true")]
    pub strip_ansi: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl Default for PaneWaitForOutputParams {
    fn default() -> Self {
        Self {
            pane_id: String::new(),
            source: ReadSource::Recent,
            r#match: OutputMatch::Substring {
                value: String::new(),
            },
            lines: None,
            strip_ansi: true,
            timeout_ms: None,
        }
    }
}

/// `notification.show` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct NotificationShowParams {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<ToastPosition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sound: Option<NotificationShowSound>,
}

/// `integration.install` / `integration.uninstall` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IntegrationTargetParams {
    pub target: IntegrationTarget,
}

/// `events.subscribe` params.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EventsSubscribeParams {
    pub subscriptions: Vec<Subscription>,
}

/// `events.wait` params.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(default)]
pub struct EventsWaitParams {
    pub match_event: EventMatch,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// `client.window_title.set` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClientWindowTitleSetParams {
    pub title: String,
}

/// `client_shell.surface.set` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClientShellSurfaceSetParams {
    pub active: bool,
}

/// `server.live_handoff` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct ServerLiveHandoffParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_protocol: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub import_exe: Option<String>,
}

/// `server.ssh_agent.register` params (unix only upstream).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServerSshAgentRegisterParams {
    pub socket_path: String,
}

/// `product_announcement.dismiss` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProductAnnouncementDismissParams {
    pub id: String,
    pub version: String,
}

/// `release_notes.dismiss` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleaseNotesDismissParams {
    pub version: String,
}

/// `command.invoke` params.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(default)]
pub struct CommandInvokeParams {
    pub command_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection: Option<serde_json::Value>,
}

/// `workspace.create` params.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(default)]
pub struct WorkspaceCreateParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "EnvMap::is_empty")]
    pub env: EnvMap,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub focus: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_workspace_id: Option<String>,
}

/// `workspace.rename` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceRenameParams {
    pub workspace_id: String,
    pub label: String,
}

/// `workspace.move` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceMoveParams {
    pub workspace_id: String,
    pub insert_index: u64,
}

/// `workspace.move_block` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct WorkspaceMoveBlockParams {
    pub workspace_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_workspace_id: Option<String>,
}

/// `workspace.report_metadata` params.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(default)]
pub struct WorkspaceReportMetadataParams {
    pub workspace_id: String,
    pub source: String,
    pub tokens: crate::types::MetadataMap,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
}

/// `workspace.close` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceCloseParams {
    pub workspace_id: String,
    pub close_group: bool,
}

/// `worktree.list` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct WorktreeListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub trust_repository: bool,
}

/// `worktree.create` params.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(default)]
pub struct WorktreeCreateParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub focus: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub trust_repository: bool,
}

/// `worktree.open` params.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(default)]
pub struct WorktreeOpenParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub focus: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub trust_repository: bool,
}

/// `worktree.remove` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct WorktreeRemoveParams {
    pub workspace_id: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub trust_repository: bool,
}

/// `tab.create` params.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(default)]
pub struct TabCreateParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "EnvMap::is_empty")]
    pub env: EnvMap,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub focus: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// `tab.list` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct TabListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

/// `tab.rename` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TabRenameParams {
    pub tab_id: String,
    pub label: String,
}

/// `tab.move` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TabMoveParams {
    pub tab_id: String,
    pub insert_index: u64,
}

/// `pane.split` params.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(default)]
pub struct PaneSplitParams {
    pub direction: SplitDirection,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_pane_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "EnvMap::is_empty")]
    pub env: EnvMap,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right_click: Option<PaneRightClickTarget>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub focus: bool,
}

/// `pane.swap` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct PaneSwapParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_pane_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_pane_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direction: Option<PaneDirection>,
}

/// Where a pane move lands.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PaneMoveDestination {
    Tab {
        tab_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        target_pane_id: Option<String>,
        split: SplitDirection,
        #[serde(skip_serializing_if = "Option::is_none")]
        ratio: Option<f64>,
    },
    NewTab {
        #[serde(skip_serializing_if = "Option::is_none")]
        workspace_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    NewWorkspace {
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tab_label: Option<String>,
    },
}

/// `pane.move` params.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(default)]
pub struct PaneMoveParams {
    pub pane_id: String,
    pub destination: PaneMoveDestination,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub focus: bool,
}

/// `pane.zoom` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct PaneZoomParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<PaneZoomMode>,
}

/// `pane.layout` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct PaneLayoutParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
}

/// `pane.neighbor` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct PaneNeighborParams {
    pub direction: PaneDirection,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
}

/// `pane.edges` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct PaneEdgesParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
}

/// `pane.focus_direction` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PaneFocusDirectionParams {
    pub direction: PaneDirection,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
}

/// `pane.resize` params.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(default)]
pub struct PaneResizeParams {
    pub direction: PaneDirection,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<f64>,
}

/// `pane.scroll` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PaneScrollParams {
    pub pane_id: String,
    pub offset_from_bottom: i64,
}

/// `pane.selection.read` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct PaneSelectionReadParams {
    pub pane_id: String,
    pub anchor: PaneTextPoint,
    pub cursor: PaneTextPoint,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_revision: Option<u64>,
}

/// `pane.copy_motion` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct PaneCopyMotionParams {
    pub pane_id: String,
    pub cursor: PaneTextPoint,
    pub motion: crate::types::PaneCopyMotion,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_revision: Option<u64>,
}

/// `pane.copy_search` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct PaneCopySearchParams {
    pub pane_id: String,
    pub cursor: PaneTextPoint,
    pub direction: crate::types::PaneCopySearchDirection,
    pub query: String,
    pub content_revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<PaneTextRange>,
}

/// `pane.list` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct PaneListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

/// `pane.current` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct PaneCurrentParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller_pane_id: Option<String>,
}

/// `pane.input.set` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PaneInputSetParams {
    pub pane_id: String,
    pub right_click: PaneRightClickTarget,
}

/// `pane.link.activate` / `pane.link.resolve` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct PaneLinkActivateParams {
    pub pane_id: String,
    pub viewport_row: u64,
    pub col: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset_from_bottom: Option<u64>,
}

/// `pane.rename` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PaneRenameParams {
    pub pane_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// `pane.send_text` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PaneSendTextParams {
    pub pane_id: String,
    pub text: String,
}

/// `pane.send_input` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct PaneSendInputParams {
    pub pane_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keys: Option<Vec<String>>,
}

/// `pane.report_agent` params.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(default)]
pub struct PaneReportAgentParams {
    pub pane_id: String,
    pub source: String,
    pub agent: String,
    pub state: PaneAgentState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_session_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_argv: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

/// `pane.report_agent_session` params.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(default)]
pub struct PaneReportAgentSessionParams {
    pub pane_id: String,
    pub source: String,
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_session_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_argv: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_start_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

/// `pane.report_metadata` params.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(default)]
pub struct PaneReportMetadataParams {
    pub pane_id: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applies_to_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_agent: Option<String>,
    #[serde(skip_serializing_if = "crate::types::MetadataMap::is_empty")]
    pub state_labels: crate::types::MetadataMap,
    #[serde(skip_serializing_if = "crate::types::MetadataMap::is_empty")]
    pub tokens: crate::types::MetadataMap,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub clear_title: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub clear_display_agent: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub clear_state_labels: bool,
}

/// `pane.clear_agent_authority` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct PaneClearAgentAuthorityParams {
    pub pane_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

/// `pane.release_agent` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct PaneReleaseAgentParams {
    pub pane_id: String,
    pub source: String,
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}
