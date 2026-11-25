//! Herdr API domain types.
//!
//! These mirror the `$defs` in `herdr-api.schema.json` (schema_version 1,
//! herdr 0.9.5). All structs use `#[serde(default)]` and do NOT deny unknown
//! fields so newer servers can add fields without breaking older clients.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Agent lifecycle state as reported by Herdr.
///
/// `idle` does NOT necessarily mean "safe to type" — unfamiliar prompts may
/// read `idle` instead of `blocked`. See `agent.explain` for debugging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    /// Idle and not yet seen by the user.
    Done,
    /// Catch-all for `unknown` and any status value added by a newer server.
    #[serde(other)]
    Unknown,
}

impl AgentStatus {
    /// The wire string for this status.
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentStatus::Idle => "idle",
            AgentStatus::Working => "working",
            AgentStatus::Blocked => "blocked",
            AgentStatus::Done => "done",
            AgentStatus::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How an agent session reference was recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionRefKind {
    Id,
    Path,
    #[serde(other)]
    Unknown,
}

/// Terminal read source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadSource {
    Visible,
    Recent,
    RecentUnwrapped,
    Detection,
    #[serde(other)]
    Unknown,
}

/// Terminal read output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadFormat {
    Text,
    Ansi,
    #[serde(other)]
    Unknown,
}

/// Split direction inside a tab layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    Right,
    Down,
    #[serde(other)]
    Unknown,
}

/// Cardinal direction for pane operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneDirection {
    Left,
    Right,
    Up,
    Down,
    #[serde(other)]
    Unknown,
}

/// Agent state a pane may self-report via `pane.report_agent`.
/// Note: this enum has no `done` variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneAgentState {
    Idle,
    Working,
    Blocked,
    Unknown,
    #[serde(other)]
    Other,
}

/// Zoom mode for `pane.zoom`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneZoomMode {
    Toggle,
    On,
    Off,
    #[serde(other)]
    Unknown,
}

/// Right-click target mode for pane input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneRightClickTarget {
    Herdr,
    Pane,
    #[serde(other)]
    Unknown,
}

/// Notification sound selection for `notification.show`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationShowSound {
    None,
    Done,
    Request,
    #[serde(other)]
    Unknown,
}

/// Corner of the screen for `notification.show` toasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToastPosition {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    #[serde(other)]
    Unknown,
}

/// Integration target agent for `integration.*` methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationTarget {
    Pi,
    Omp,
    Claude,
    Codex,
    Copilot,
    Devin,
    Droid,
    Kimi,
    Opencode,
    Kilo,
    Hermes,
    Qodercli,
    Qwen,
    Cursor,
    Mastracode,
    AntigravityCli,
    Grok,
    #[serde(other)]
    Unknown,
}

impl IntegrationTarget {
    /// The wire string for this target.
    pub fn as_str(&self) -> &'static str {
        match self {
            IntegrationTarget::Pi => "pi",
            IntegrationTarget::Omp => "omp",
            IntegrationTarget::Claude => "claude",
            IntegrationTarget::Codex => "codex",
            IntegrationTarget::Copilot => "copilot",
            IntegrationTarget::Devin => "devin",
            IntegrationTarget::Droid => "droid",
            IntegrationTarget::Kimi => "kimi",
            IntegrationTarget::Opencode => "opencode",
            IntegrationTarget::Kilo => "kilo",
            IntegrationTarget::Hermes => "hermes",
            IntegrationTarget::Qodercli => "qodercli",
            IntegrationTarget::Qwen => "qwen",
            IntegrationTarget::Cursor => "cursor",
            IntegrationTarget::Mastracode => "mastracode",
            IntegrationTarget::AntigravityCli => "antigravity_cli",
            IntegrationTarget::Grok => "grok",
            IntegrationTarget::Unknown => "unknown",
        }
    }
}

/// Why a client window title was (or was not) applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientWindowTitleReason {
    Set,
    Cleared,
    NoForegroundClient,
    #[serde(other)]
    Unknown,
}

/// Result of `notification.show`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationShowReason {
    Shown,
    Disabled,
    RateLimited,
    NoForegroundClient,
    Busy,
    #[serde(other)]
    Unknown,
}

/// Reason a `pane.move` did not move the pane as requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneMoveReason {
    SameTab,
    ZoomedTab,
    #[serde(other)]
    Unknown,
}

/// Reason a `pane.swap` did not swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneSwapReason {
    NoNeighbor,
    SamePane,
    NotFound,
    CrossTab,
    #[serde(other)]
    Unknown,
}

/// Reason a `pane.resize` did not resize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneResizeReason {
    Unchanged,
    #[serde(other)]
    Unknown,
}

/// Reason a `pane.zoom` did not change zoom state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneZoomReason {
    SinglePane,
    AlreadyZoomed,
    AlreadyUnzoomed,
    #[serde(other)]
    Unknown,
}

/// Reason `pane.focus_direction` did not move focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneFocusDirectionReason {
    NoNeighbor,
    #[serde(other)]
    Unknown,
}

/// Copy-mode motion for `pane.copy_motion`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneCopyMotion {
    LineEnd,
    FirstNonBlank,
    NextWordStart,
    PreviousWordStart,
    NextWordEnd,
    NextBigWordStart,
    PreviousBigWordStart,
    NextBigWordEnd,
    PreviousParagraph,
    NextParagraph,
    #[serde(other)]
    Unknown,
}

/// Search direction for `pane.copy_search`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneCopySearchDirection {
    Forward,
    Backward,
    #[serde(other)]
    Unknown,
}

/// Agent session identity recorded for a pane/agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSessionInfo {
    pub source: String,
    pub agent: String,
    pub kind: AgentSessionRefKind,
    pub value: String,
}

/// Token/state-label metadata maps shared by several info structs.
pub type MetadataMap = HashMap<String, String>;

/// Environment overrides for spawned panes/tabs/workspaces.
pub type EnvMap = HashMap<String, serde_json::Value>;

/// A live coding agent known to the Herdr server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub terminal_id: String,
    pub agent_status: AgentStatus,
    pub revision: u64,
    pub focused: bool,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub agent_session: Option<AgentSessionInfo>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub foreground_cwd: Option<String>,
    #[serde(default)]
    pub display_agent: Option<String>,
    #[serde(default)]
    pub interactive_ready: bool,
    #[serde(default)]
    pub launch_pending: bool,
    #[serde(default)]
    pub screen_detection_skipped: bool,
    #[serde(default)]
    pub state_change_seq: u64,
    #[serde(default)]
    pub completion_seq: Option<u64>,
    #[serde(default)]
    pub terminal_title: Option<String>,
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub tokens: MetadataMap,
    #[serde(default)]
    pub state_labels: MetadataMap,
}

/// Scroll position info for a pane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneScrollInfo {
    pub offset_from_bottom: u64,
    pub max_offset_from_bottom: u64,
    pub viewport_rows: u64,
}

/// A terminal pane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub terminal_id: String,
    pub agent_status: AgentStatus,
    pub revision: u64,
    pub focused: bool,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub agent_session: Option<AgentSessionInfo>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub foreground_cwd: Option<String>,
    #[serde(default)]
    pub display_agent: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub restore_error: Option<String>,
    #[serde(default)]
    pub scroll: Option<PaneScrollInfo>,
    #[serde(default)]
    pub terminal_title: Option<String>,
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub tokens: MetadataMap,
    #[serde(default)]
    pub state_labels: MetadataMap,
}

/// A tab inside a workspace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabInfo {
    pub tab_id: String,
    pub workspace_id: String,
    pub label: String,
    pub number: u64,
    pub pane_count: u64,
    pub agent_status: AgentStatus,
    pub focused: bool,
}

/// Worktree checkout info attached to a workspace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceWorktreeInfo {
    pub checkout_path: String,
    pub repo_key: String,
    pub repo_name: String,
    pub repo_root: String,
    pub is_linked_worktree: bool,
}

/// A workspace (top-level session container).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub workspace_id: String,
    pub label: String,
    pub number: u64,
    pub pane_count: u64,
    pub tab_count: u64,
    pub active_tab_id: String,
    pub agent_status: AgentStatus,
    pub focused: bool,
    #[serde(default)]
    pub tokens: MetadataMap,
    #[serde(default)]
    pub worktree: Option<WorkspaceWorktreeInfo>,
}

/// A git worktree known to `worktree.*` methods.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub path: String,
    pub label: String,
    pub is_bare: bool,
    pub is_detached: bool,
    pub is_linked_worktree: bool,
    pub is_prunable: bool,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub open_workspace_id: Option<String>,
}

/// Repo context returned by `worktree.list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorktreeSourceInfo {
    pub repo_key: String,
    pub repo_name: String,
    pub repo_root: String,
    pub source_checkout_path: String,
    #[serde(default)]
    pub source_workspace_id: Option<String>,
}

/// Rect in terminal cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneLayoutRect {
    pub x: u64,
    pub y: u64,
    pub width: u64,
    pub height: u64,
}

/// A pane placed in a layout snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneLayoutPane {
    pub pane_id: String,
    pub focused: bool,
    pub rect: PaneLayoutRect,
}

/// A split region in a layout snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneLayoutSplit {
    pub id: String,
    pub direction: SplitDirection,
    pub ratio: f64,
    pub rect: PaneLayoutRect,
}

/// Geometry snapshot of a tab's pane layout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneLayoutSnapshot {
    pub workspace_id: String,
    pub tab_id: String,
    pub area: PaneLayoutRect,
    pub focused_pane_id: String,
    pub zoomed: bool,
    pub panes: Vec<PaneLayoutPane>,
    pub splits: Vec<PaneLayoutSplit>,
}

/// A foreground process inside a pane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneProcessInfoProcess {
    pub pid: u64,
    pub name: String,
    #[serde(default)]
    pub argv: Option<Vec<String>>,
    #[serde(default)]
    pub argv0: Option<String>,
    #[serde(default)]
    pub cmdline: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

/// Process info for a pane (`pane.process_info`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneProcessInfo {
    pub pane_id: String,
    #[serde(default)]
    pub shell_pid: Option<u64>,
    #[serde(default)]
    pub foreground_process_group_id: Option<u64>,
    #[serde(default)]
    pub tty: Option<String>,
    #[serde(default)]
    pub foreground_processes: Vec<PaneProcessInfoProcess>,
}

/// Result of `pane.read` / `agent.read`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneReadResult {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub source: ReadSource,
    pub format: ReadFormat,
    pub text: String,
    pub revision: u64,
    pub truncated: bool,
}

/// A text position inside a pane's buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneTextPoint {
    pub row: u64,
    pub col: u64,
}

/// A text range inside a pane's buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneTextRange {
    pub start: PaneTextPoint,
    pub end: PaneTextPoint,
}

/// Result payload of `pane.swap`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneSwapResult {
    pub changed: bool,
    pub source_pane_id: String,
    #[serde(default)]
    pub target_pane_id: Option<String>,
    #[serde(default)]
    pub focused_pane_id: String,
    #[serde(default)]
    pub reason: Option<PaneSwapReason>,
    #[serde(default)]
    pub layout: Option<PaneLayoutSnapshot>,
}

/// Result payload of `pane.move`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneMoveResult {
    pub changed: bool,
    pub pane: PaneInfo,
    pub previous_pane_id: String,
    pub previous_workspace_id: String,
    pub previous_tab_id: String,
    pub focused_pane_id: String,
    #[serde(default)]
    pub reason: Option<PaneMoveReason>,
    #[serde(default)]
    pub created_workspace: Option<WorkspaceInfo>,
    #[serde(default)]
    pub created_tab: Option<TabInfo>,
    #[serde(default)]
    pub closed_workspace_id: Option<String>,
    #[serde(default)]
    pub closed_tab_id: Option<String>,
    #[serde(default)]
    pub source_layout: Option<PaneLayoutSnapshot>,
    pub target_layout: PaneLayoutSnapshot,
}

/// Result payload of `pane.neighbor`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneNeighborResult {
    pub pane_id: String,
    pub direction: PaneDirection,
    #[serde(default)]
    pub neighbor_pane_id: Option<String>,
    #[serde(default)]
    pub layout: Option<PaneLayoutSnapshot>,
}

/// Result payload of `pane.edges`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneEdgesResult {
    pub pane_id: String,
    pub left: bool,
    pub right: bool,
    pub up: bool,
    pub down: bool,
    #[serde(default)]
    pub layout: Option<PaneLayoutSnapshot>,
}

/// Result payload of `pane.focus_direction`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneFocusDirectionResult {
    pub source_pane_id: String,
    #[serde(default)]
    pub focused_pane_id: Option<String>,
    pub changed: bool,
    #[serde(default)]
    pub reason: Option<PaneFocusDirectionReason>,
    #[serde(default)]
    pub layout: Option<PaneLayoutSnapshot>,
}

/// Result payload of `pane.resize`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneResizeResult {
    pub pane_id: String,
    pub changed: bool,
    #[serde(default)]
    pub focused_pane_id: String,
    #[serde(default)]
    pub reason: Option<PaneResizeReason>,
    #[serde(default)]
    pub layout: Option<PaneLayoutSnapshot>,
}

/// Result payload of `pane.zoom`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneZoomResult {
    pub pane_id: String,
    pub zoomed: bool,
    pub changed: bool,
    pub zoom_changed: bool,
    pub focus_changed: bool,
    pub focused_pane_id: String,
    #[serde(default)]
    pub reason: Option<PaneZoomReason>,
    #[serde(default)]
    pub layout: Option<PaneLayoutSnapshot>,
}

/// Serializable layout tree node (`layout.*`, `pane.split`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LayoutNode {
    Pane {
        #[serde(default)]
        pane_id: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        command: Option<Vec<String>>,
        #[serde(default)]
        env: EnvMap,
    },
    Split {
        direction: SplitDirection,
        ratio: f64,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

/// Exported/applied layout description.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutDescription {
    pub workspace_id: String,
    pub tab_id: String,
    pub focused_pane_id: String,
    pub zoomed: bool,
    pub root: LayoutNode,
}

/// Server capabilities reported by `ping`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerCapabilities {
    pub live_handoff: bool,
    pub detached_server_daemon: bool,
    pub health_check: bool,
    pub ssh_agent_registration: bool,
    pub surface_interest: bool,
    #[serde(default)]
    pub endpoint_protocol_generation: Option<u64>,
}

/// Full session state (`session.snapshot`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub version: String,
    pub protocol: u64,
    pub workspaces: Vec<WorkspaceInfo>,
    pub tabs: Vec<TabInfo>,
    pub panes: Vec<PaneInfo>,
    pub layouts: Vec<PaneLayoutSnapshot>,
    pub agents: Vec<AgentInfo>,
    #[serde(default)]
    pub focused_workspace_id: Option<String>,
    #[serde(default)]
    pub focused_tab_id: Option<String>,
    #[serde(default)]
    pub focused_pane_id: Option<String>,
}

/// Result of `pane.copy_motion`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneCopyMotionResult {
    pub pane_id: String,
    pub content_revision: u64,
    pub cursor: PaneTextPoint,
}

/// Result of `pane.copy_search`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneCopySearchResult {
    pub pane_id: String,
    pub content_revision: u64,
    pub total: u64,
    #[serde(default)]
    pub current: Option<u64>,
    #[serde(default)]
    pub current_global: Option<u64>,
    pub matches: Vec<PaneTextRange>,
}

/// Result of `pane.selection.read`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneSelectionResult {
    pub pane_id: String,
    pub text: String,
}

/// Result of `pane.link.resolve`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneLinkRegion {
    pub row: u64,
    pub start_col: u64,
    pub end_col: u64,
}

/// Result of `pane.link.activate`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneLinkActivatedResult {
    pub handled: bool,
    #[serde(default)]
    pub url: Option<String>,
}

/// Result of `integration.list` entries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntegrationInfo {
    pub target: IntegrationTarget,
    pub label: String,
    pub command: String,
    pub available: bool,
    pub state: IntegrationState,
}

/// Installation state of an agent integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationState {
    NotInstalled,
    Current,
    Outdated,
    #[serde(other)]
    Unknown,
}

/// Result details of `integration.install` / `integration.uninstall`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntegrationMessages {
    pub messages: Vec<String>,
}

/// Result of `agent.view.set` / `agent.view.clear`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentViewResult {
    pub active: bool,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

/// Output matched by `pane.wait_for_output`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputMatchedResult {
    pub pane_id: String,
    pub revision: u64,
    pub read: PaneReadResult,
    #[serde(default)]
    pub matched_line: Option<String>,
}

/// Result of `client.window_title.set` / `clear`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientWindowTitleResult {
    pub changed: bool,
    pub reason: ClientWindowTitleReason,
}

/// Result of `notification.show`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotificationShowResult {
    pub shown: bool,
    pub reason: NotificationShowReason,
}

/// Result of `client_shell.surface.set`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientShellSurfaceSetResult {
    pub active: bool,
    pub projection_revision: u64,
}
