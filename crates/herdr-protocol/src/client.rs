//! Synchronous NDJSON client for the Herdr TUI socket API.
//!
//! Transport: one JSON object per line over a Unix domain socket
//! (`$HERDR_SOCKET_PATH`, default `~/.config/herdr/herdr.sock`, or
//! `$XDG_CONFIG_HOME/herdr/...`; `HERDR_SESSION=<name>` selects
//! `sessions/<name>/herdr.sock`). Windows named pipes are not implemented —
//! connect attempts return [`HerdrError::Unsupported`].
//!
//! One request per connection: the server closes after a single response,
//! except long-lived calls (`events.subscribe`, `events.wait`, `agent.wait`,
//! `agent.prompt` with `wait`, `pane.wait_for_output`) which hold the
//! connection until their server-side condition resolves.

use serde::Serialize;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{HerdrError, Result};
use crate::events::{EventEnvelope, EventMatch, Subscription, SubscriptionFrame};
use crate::params::*;
use crate::results::{PongResult, Request, ResponseEnvelope, ResponseResult};
use crate::types::*;

/// Maximum accepted NDJSON line length (~8 MiB). Longer lines are rejected
/// as [`HerdrError::Protocol`] to bound memory use on hostile/broken peers.
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// Environment variable overriding the socket path.
pub const SOCKET_PATH_ENV_VAR: &str = "HERDR_SOCKET_PATH";
/// Environment variable selecting a named session.
pub const SESSION_ENV_VAR: &str = "HERDR_SESSION";
/// Named session treated as the default (no `sessions/` subdir).
pub const DEFAULT_SESSION_NAME: &str = "default";

/// A bidirectional byte stream to the Herdr server.
///
/// `Send` is required so `SubscriptionStream` can move to a reader thread
/// (the runtime abstraction's `subscribe_events` does exactly that).
trait Conn: Read + Write + Send {}
impl<T: Read + Write + Send> Conn for T {}
type BoxConn = Box<dyn Conn>;

/// Open a connection to `path`, applying the client's configured timeouts.
#[cfg(unix)]
fn open_connection(path: &Path, read_timeout: Option<Duration>) -> Result<BoxConn> {
    let stream = std::os::unix::net::UnixStream::connect(path)?;
    if let Some(t) = read_timeout {
        stream.set_read_timeout(Some(t))?;
    }
    Ok(Box::new(stream))
}

/// Windows named-pipe transport is not implemented yet.
#[cfg(not(unix))]
fn open_connection(_path: &Path, _read_timeout: Option<Duration>) -> Result<BoxConn> {
    Err(HerdrError::Unsupported(
        "Herdr socket transport on this platform uses a named pipe, which is not implemented"
            .to_string(),
    ))
}

/// Read one NDJSON line, enforcing [`MAX_LINE_BYTES`].
///
/// EOF before any byte yields [`HerdrError::Closed`] — the server closes the
/// connection silently on empty lines and invalid UTF-8, so a clean EOF here
/// always means the peer went away.
fn read_line(reader: &mut impl BufRead) -> Result<String> {
    let mut buf = Vec::new();
    {
        // `Take` implements `BufRead` when the inner reader does, capping the
        // bytes we will buffer for one line.
        let mut limited = reader.by_ref().take((MAX_LINE_BYTES + 1) as u64);
        limited.read_until(b'\n', &mut buf)?;
    }
    if buf.is_empty() {
        return Err(HerdrError::Closed);
    }
    if buf.len() > MAX_LINE_BYTES {
        return Err(HerdrError::Protocol(format!(
            "response line exceeded {MAX_LINE_BYTES} bytes"
        )));
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
    }
    if buf.is_empty() {
        return Err(HerdrError::Protocol("empty response line".to_string()));
    }
    String::from_utf8(buf)
        .map_err(|_| HerdrError::Protocol("response line is not UTF-8".to_string()))
}

/// Synchronous client for the Herdr socket API.
///
/// `HerdrClient` is cheap to construct; each call opens a fresh connection.
/// Use [`HerdrClient::events_subscribe`] for long-lived event streams.
#[derive(Debug, Clone)]
pub struct HerdrClient {
    socket_path: PathBuf,
    request_timeout: Option<Duration>,
}

impl HerdrClient {
    /// Client using the socket path resolved from the environment.
    ///
    /// Resolution order:
    /// 1. `$HERDR_SOCKET_PATH`
    /// 2. `$XDG_CONFIG_HOME/herdr/sessions/<HERDR_SESSION>/herdr.sock` when
    ///    `HERDR_SESSION` names a valid non-default session
    /// 3. `$XDG_CONFIG_HOME/herdr/herdr.sock` (or `~/.config/herdr/herdr.sock`)
    pub fn new() -> Result<Self> {
        Ok(Self {
            socket_path: resolve_socket_path(),
            request_timeout: None,
        })
    }

    /// Client bound to an explicit socket path.
    pub fn from_socket_path<P: Into<PathBuf>>(path: P) -> Self {
        Self {
            socket_path: path.into(),
            request_timeout: None,
        }
    }

    /// Builder: set a per-request read timeout. Long waits should rely on the
    /// server-side `timeout_ms` params instead; leave `None` (default) to
    /// block until the server responds or closes.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = Some(timeout);
        self
    }

    /// The socket path this client connects to.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    // ------------------------------------------------------------------
    // Transport core
    // ------------------------------------------------------------------

    fn next_request_id(&self) -> String {
        uuid::Uuid::now_v7().to_string()
    }

    fn write_request(
        conn: &mut BoxConn,
        id: &str,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<()> {
        let request = Request {
            id: id.to_string(),
            method: method.to_string(),
            params: params.clone(),
        };
        let line = serde_json::to_string(&request)
            .map_err(|e| HerdrError::Protocol(format!("failed to encode request: {e}")))?;
        debug_assert!(!line.contains('\n'));
        let mut writer = BufWriter::new(&mut *conn);
        writer
            .write_all(line.as_bytes())
            .and_then(|()| writer.write_all(b"\n"))
            .and_then(|()| writer.flush())
            .map_err(HerdrError::Transport)
    }

    fn read_response_line(reader: &mut BufReader<BoxConn>) -> Result<String> {
        read_line(reader)
    }

    /// Perform a single request/response exchange on a fresh connection.
    #[tracing::instrument(skip_all, name = "herdr.request", level = "debug", fields(method = %method))]
    fn transact(&self, method: &str, params: &serde_json::Value) -> Result<ResponseResult> {
        let mut conn = open_connection(&self.socket_path, self.request_timeout)?;
        let id = self.next_request_id();
        Self::write_request(&mut conn, &id, method, params)?;
        let mut reader = BufReader::new(conn);
        let line = Self::read_response_line(&mut reader)?;
        let envelope: ResponseEnvelope = serde_json::from_str(&line)
            .map_err(|e| HerdrError::Protocol(format!("malformed response envelope: {e}")))?;
        if envelope.id != id {
            return Err(HerdrError::Protocol(format!(
                "response id {:?} does not match request id {id:?}",
                envelope.id
            )));
        }
        if let Some(error) = envelope.error {
            return Err(HerdrError::Api {
                code: error.code,
                message: error.message,
            });
        }
        let result = envelope
            .result
            .ok_or_else(|| HerdrError::Protocol("response has neither result nor error".into()))?;
        serde_json::from_value::<ResponseResult>(result)
            .map_err(|e| HerdrError::Protocol(format!("failed to decode result: {e}")))
    }

    /// Escape hatch: call any method with a raw JSON params object.
    ///
    /// Returns the decoded [`ResponseResult`]; unknown `type` discriminators
    /// surface as [`ResponseResult::Unknown`].
    pub fn call_raw(&self, method: &str, params: &serde_json::Value) -> Result<ResponseResult> {
        if !params.is_object() {
            return Err(HerdrError::Protocol(
                "params must serialize to a JSON object".to_string(),
            ));
        }
        self.transact(method, params)
    }

    /// Typed variant of [`HerdrClient::call_raw`]: serializes `params` and
    /// returns the decoded [`ResponseResult`].
    pub fn call<P: Serialize>(&self, method: &str, params: &P) -> Result<ResponseResult> {
        let value = serde_json::to_value(params)
            .map_err(|e| HerdrError::Protocol(format!("failed to encode params: {e}")))?;
        self.call_raw(method, &value)
    }

    // ------------------------------------------------------------------
    // Result extraction helpers
    // ------------------------------------------------------------------

    fn unexpected(result: &ResponseResult, expected: &str) -> HerdrError {
        HerdrError::Protocol(format!(
            "unexpected result type {:?} (expected {expected})",
            result.kind()
        ))
    }

    fn expect_ok(result: ResponseResult) -> Result<()> {
        match result {
            ResponseResult::Ok | ResponseResult::SubscriptionStarted => Ok(()),
            other => Err(Self::unexpected(&other, "ok")),
        }
    }

    // ------------------------------------------------------------------
    // ping / server / session / notification / client / misc
    // ------------------------------------------------------------------

    /// `ping` — server version, protocol, capabilities.
    pub fn ping(&self) -> Result<PongResult> {
        match self.call("ping", &EmptyParams {})? {
            ResponseResult::Pong(p) => Ok(p),
            other => Err(Self::unexpected(&other, "pong")),
        }
    }

    /// `server.stop` — ask the server to shut down.
    pub fn server_stop(&self) -> Result<()> {
        Self::expect_ok(self.call("server.stop", &EmptyParams {})?)
    }

    /// `server.live_handoff` — hand the session to a new server binary.
    /// Returns the raw result (usually `ok`).
    pub fn server_live_handoff(&self, params: &ServerLiveHandoffParams) -> Result<ResponseResult> {
        self.call("server.live_handoff", params)
    }

    /// `server.reload_config` — reload server configuration.
    /// The result type is `config_reload`, currently surfaced as
    /// [`ResponseResult::Unknown`]; inspect `raw()` for diagnostics.
    pub fn server_reload_config(&self) -> Result<ResponseResult> {
        self.call("server.reload_config", &EmptyParams {})
    }

    /// `server.ssh_agent.register` (unix only upstream).
    #[cfg(unix)]
    pub fn server_ssh_agent_register(&self, socket_path: &str) -> Result<()> {
        Self::expect_ok(self.call(
            "server.ssh_agent.register",
            &ServerSshAgentRegisterParams {
                socket_path: socket_path.to_string(),
            },
        )?)
    }

    /// `server.agent_manifests` — detection manifest status.
    /// Result type `agent_manifest_status` is not yet typed; returns raw.
    pub fn server_agent_manifests(&self) -> Result<ResponseResult> {
        self.call("server.agent_manifests", &EmptyParams {})
    }

    /// `server.reload_agent_manifests`.
    /// Result type `agent_manifest_reload` is not yet typed; returns raw.
    pub fn server_reload_agent_manifests(&self) -> Result<ResponseResult> {
        self.call("server.reload_agent_manifests", &EmptyParams {})
    }

    /// `session.snapshot` — full session state bootstrap.
    pub fn session_snapshot(&self) -> Result<SessionSnapshot> {
        match self.call("session.snapshot", &EmptyParams {})? {
            ResponseResult::SessionSnapshot(r) => Ok(r.snapshot),
            other => Err(Self::unexpected(&other, "session_snapshot")),
        }
    }

    /// `notification.show` — user-facing toast.
    pub fn notification_show(
        &self,
        params: &NotificationShowParams,
    ) -> Result<NotificationShowResult> {
        match self.call("notification.show", params)? {
            ResponseResult::NotificationShow(r) => Ok(NotificationShowResult {
                shown: r.shown,
                reason: r.reason,
            }),
            other => Err(Self::unexpected(&other, "notification_show")),
        }
    }

    /// Convenience: `notification.show` with just a title.
    pub fn notify(&self, title: &str) -> Result<NotificationShowResult> {
        self.notification_show(&NotificationShowParams {
            title: title.to_string(),
            body: None,
            position: None,
            sound: None,
        })
    }

    /// `client.window_title.set`.
    pub fn client_window_title_set(&self, title: &str) -> Result<ClientWindowTitleResult> {
        match self.call(
            "client.window_title.set",
            &ClientWindowTitleSetParams {
                title: title.to_string(),
            },
        )? {
            ResponseResult::ClientWindowTitle(r) => Ok(ClientWindowTitleResult {
                changed: r.changed,
                reason: r.reason,
            }),
            other => Err(Self::unexpected(&other, "client_window_title")),
        }
    }

    /// `client.window_title.clear`.
    pub fn client_window_title_clear(&self) -> Result<ClientWindowTitleResult> {
        match self.call("client.window_title.clear", &EmptyParams {})? {
            ResponseResult::ClientWindowTitle(r) => Ok(ClientWindowTitleResult {
                changed: r.changed,
                reason: r.reason,
            }),
            ResponseResult::Ok => Ok(ClientWindowTitleResult {
                changed: false,
                reason: ClientWindowTitleReason::Cleared,
            }),
            other => Err(Self::unexpected(&other, "client_window_title")),
        }
    }

    /// `client_shell.surface.set`.
    pub fn client_shell_surface_set(&self, active: bool) -> Result<ClientShellSurfaceSetResult> {
        match self.call(
            "client_shell.surface.set",
            &ClientShellSurfaceSetParams { active },
        )? {
            ResponseResult::ClientShellSurfaceSet(r) => Ok(ClientShellSurfaceSetResult {
                active: r.active,
                projection_revision: r.projection_revision,
            }),
            other => Err(Self::unexpected(&other, "client_shell_surface_set")),
        }
    }

    /// `product_announcement.dismiss`.
    pub fn product_announcement_dismiss(&self, id: &str, version: &str) -> Result<()> {
        Self::expect_ok(self.call(
            "product_announcement.dismiss",
            &ProductAnnouncementDismissParams {
                id: id.to_string(),
                version: version.to_string(),
            },
        )?)
    }

    /// `release_notes.dismiss`.
    pub fn release_notes_dismiss(&self, version: &str) -> Result<()> {
        Self::expect_ok(self.call(
            "release_notes.dismiss",
            &ReleaseNotesDismissParams {
                version: version.to_string(),
            },
        )?)
    }

    /// `command.invoke` — invoke a registered command.
    pub fn command_invoke(&self, params: &CommandInvokeParams) -> Result<ResponseResult> {
        self.call("command.invoke", params)
    }

    // ------------------------------------------------------------------
    // workspace.*
    // ------------------------------------------------------------------

    /// `workspace.create`.
    pub fn workspace_create(&self, params: &WorkspaceCreateParams) -> Result<WorkspaceCreated> {
        match self.call("workspace.create", params)? {
            ResponseResult::WorkspaceCreated(r) => Ok(WorkspaceCreated {
                workspace: r.workspace,
                tab: r.tab,
                root_pane: r.root_pane,
            }),
            other => Err(Self::unexpected(&other, "workspace_created")),
        }
    }

    /// `workspace.list`.
    pub fn workspace_list(&self) -> Result<Vec<WorkspaceInfo>> {
        match self.call("workspace.list", &EmptyParams {})? {
            ResponseResult::WorkspaceList(r) => Ok(r.workspaces),
            other => Err(Self::unexpected(&other, "workspace_list")),
        }
    }

    /// `workspace.get`.
    pub fn workspace_get(&self, workspace_id: &str) -> Result<WorkspaceInfo> {
        match self.call(
            "workspace.get",
            &serde_json::json!({ "workspace_id": workspace_id }),
        )? {
            ResponseResult::WorkspaceInfo(r) => Ok(r.workspace),
            other => Err(Self::unexpected(&other, "workspace_info")),
        }
    }

    /// `workspace.focus`.
    pub fn workspace_focus(&self, workspace_id: &str) -> Result<WorkspaceInfo> {
        match self.call(
            "workspace.focus",
            &serde_json::json!({ "workspace_id": workspace_id }),
        )? {
            ResponseResult::WorkspaceInfo(r) => Ok(r.workspace),
            other => Err(Self::unexpected(&other, "workspace_info")),
        }
    }

    /// `workspace.rename`.
    pub fn workspace_rename(&self, workspace_id: &str, label: &str) -> Result<WorkspaceInfo> {
        match self.call(
            "workspace.rename",
            &WorkspaceRenameParams {
                workspace_id: workspace_id.to_string(),
                label: label.to_string(),
            },
        )? {
            ResponseResult::WorkspaceInfo(r) => Ok(r.workspace),
            other => Err(Self::unexpected(&other, "workspace_info")),
        }
    }

    /// `workspace.move`.
    pub fn workspace_move(
        &self,
        workspace_id: &str,
        insert_index: u64,
    ) -> Result<Vec<WorkspaceInfo>> {
        match self.call(
            "workspace.move",
            &WorkspaceMoveParams {
                workspace_id: workspace_id.to_string(),
                insert_index,
            },
        )? {
            ResponseResult::WorkspaceList(r) => Ok(r.workspaces),
            other => Err(Self::unexpected(&other, "workspace_list")),
        }
    }

    /// `workspace.move_block`.
    pub fn workspace_move_block(
        &self,
        workspace_ids: &[String],
        before_workspace_id: Option<&str>,
    ) -> Result<Vec<WorkspaceInfo>> {
        match self.call(
            "workspace.move_block",
            &WorkspaceMoveBlockParams {
                workspace_ids: workspace_ids.to_vec(),
                before_workspace_id: before_workspace_id.map(str::to_string),
            },
        )? {
            ResponseResult::WorkspaceList(r) => Ok(r.workspaces),
            other => Err(Self::unexpected(&other, "workspace_list")),
        }
    }

    /// `workspace.report_metadata`.
    pub fn workspace_report_metadata(&self, params: &WorkspaceReportMetadataParams) -> Result<()> {
        Self::expect_ok(self.call("workspace.report_metadata", params)?)
    }

    /// `workspace.close`.
    pub fn workspace_close(&self, workspace_id: &str, close_group: bool) -> Result<()> {
        Self::expect_ok(self.call(
            "workspace.close",
            &WorkspaceCloseParams {
                workspace_id: workspace_id.to_string(),
                close_group,
            },
        )?)
    }

    // ------------------------------------------------------------------
    // worktree.*
    // ------------------------------------------------------------------

    /// `worktree.list`.
    pub fn worktree_list(&self, params: &WorktreeListParams) -> Result<WorktreeList> {
        match self.call("worktree.list", params)? {
            ResponseResult::WorktreeList(r) => Ok(WorktreeList {
                source: r.source,
                worktrees: r.worktrees,
            }),
            other => Err(Self::unexpected(&other, "worktree_list")),
        }
    }

    /// `worktree.create`.
    pub fn worktree_create(&self, params: &WorktreeCreateParams) -> Result<WorktreeCreated> {
        match self.call("worktree.create", params)? {
            ResponseResult::WorktreeCreated(r) => Ok(WorktreeCreated {
                workspace: r.workspace,
                tab: r.tab,
                root_pane: r.root_pane,
                worktree: r.worktree,
            }),
            other => Err(Self::unexpected(&other, "worktree_created")),
        }
    }

    /// `worktree.open`.
    pub fn worktree_open(&self, params: &WorktreeOpenParams) -> Result<WorktreeOpened> {
        match self.call("worktree.open", params)? {
            ResponseResult::WorktreeOpened(r) => Ok(WorktreeOpened {
                workspace: r.workspace,
                tab: r.tab,
                root_pane: r.root_pane,
                worktree: r.worktree,
                already_open: r.already_open,
            }),
            other => Err(Self::unexpected(&other, "worktree_opened")),
        }
    }

    /// `worktree.remove`.
    pub fn worktree_remove(&self, params: &WorktreeRemoveParams) -> Result<WorktreeRemoved> {
        match self.call("worktree.remove", params)? {
            ResponseResult::WorktreeRemoved(r) => Ok(WorktreeRemoved {
                workspace_id: r.workspace_id,
                path: r.path,
                forced: r.forced,
            }),
            other => Err(Self::unexpected(&other, "worktree_removed")),
        }
    }

    // ------------------------------------------------------------------
    // tab.*
    // ------------------------------------------------------------------

    /// `tab.create`.
    pub fn tab_create(&self, params: &TabCreateParams) -> Result<TabCreated> {
        match self.call("tab.create", params)? {
            ResponseResult::TabCreated(r) => Ok(TabCreated {
                tab: r.tab,
                root_pane: r.root_pane,
            }),
            other => Err(Self::unexpected(&other, "tab_created")),
        }
    }

    /// `tab.list`.
    pub fn tab_list(&self, workspace_id: Option<&str>) -> Result<Vec<TabInfo>> {
        match self.call(
            "tab.list",
            &TabListParams {
                workspace_id: workspace_id.map(str::to_string),
            },
        )? {
            ResponseResult::TabList(r) => Ok(r.tabs),
            other => Err(Self::unexpected(&other, "tab_list")),
        }
    }

    /// `tab.get`.
    pub fn tab_get(&self, tab_id: &str) -> Result<TabInfo> {
        match self.call("tab.get", &serde_json::json!({ "tab_id": tab_id }))? {
            ResponseResult::TabInfo(r) => Ok(r.tab),
            other => Err(Self::unexpected(&other, "tab_info")),
        }
    }

    /// `tab.focus`.
    pub fn tab_focus(&self, tab_id: &str) -> Result<TabInfo> {
        match self.call("tab.focus", &serde_json::json!({ "tab_id": tab_id }))? {
            ResponseResult::TabInfo(r) => Ok(r.tab),
            other => Err(Self::unexpected(&other, "tab_info")),
        }
    }

    /// `tab.rename`.
    pub fn tab_rename(&self, tab_id: &str, label: &str) -> Result<TabInfo> {
        match self.call(
            "tab.rename",
            &TabRenameParams {
                tab_id: tab_id.to_string(),
                label: label.to_string(),
            },
        )? {
            ResponseResult::TabInfo(r) => Ok(r.tab),
            other => Err(Self::unexpected(&other, "tab_info")),
        }
    }

    /// `tab.move`.
    pub fn tab_move(&self, tab_id: &str, insert_index: u64) -> Result<Vec<TabInfo>> {
        match self.call(
            "tab.move",
            &TabMoveParams {
                tab_id: tab_id.to_string(),
                insert_index,
            },
        )? {
            ResponseResult::TabList(r) => Ok(r.tabs),
            other => Err(Self::unexpected(&other, "tab_list")),
        }
    }

    /// `tab.close`.
    pub fn tab_close(&self, tab_id: &str) -> Result<()> {
        Self::expect_ok(self.call("tab.close", &serde_json::json!({ "tab_id": tab_id }))?)
    }

    // ------------------------------------------------------------------
    // agent.*
    // ------------------------------------------------------------------

    /// `agent.list`.
    pub fn agent_list(&self) -> Result<Vec<AgentInfo>> {
        match self.call("agent.list", &EmptyParams {})? {
            ResponseResult::AgentList(r) => Ok(r.agents),
            other => Err(Self::unexpected(&other, "agent_list")),
        }
    }

    /// `agent.get` — resolve a target (terminal_id | pane_id | agent name).
    pub fn agent_get(&self, target: &str) -> Result<AgentInfo> {
        match self.call("agent.get", &serde_json::json!({ "target": target }))? {
            ResponseResult::AgentInfo(r) => Ok(r.agent),
            other => Err(Self::unexpected(&other, "agent_info")),
        }
    }

    /// `agent.explain` — debug why an agent reports its current state.
    /// Returns the raw explanation payload.
    pub fn agent_explain(&self, target: &str) -> Result<serde_json::Value> {
        match self.call("agent.explain", &serde_json::json!({ "target": target }))? {
            ResponseResult::AgentExplain(r) => Ok(r.explain),
            other => Err(Self::unexpected(&other, "agent_explain")),
        }
    }

    /// `agent.rename` — set (`Some`) or clear (`None`) an agent's name.
    pub fn agent_rename(&self, target: &str, name: Option<&str>) -> Result<AgentInfo> {
        match self.call(
            "agent.rename",
            &AgentRenameParams {
                target: target.to_string(),
                name: name.map(str::to_string),
            },
        )? {
            ResponseResult::AgentInfo(r) => Ok(r.agent),
            other => Err(Self::unexpected(&other, "agent_info")),
        }
    }

    /// `agent.focus`.
    pub fn agent_focus(&self, target: &str) -> Result<AgentInfo> {
        match self.call("agent.focus", &serde_json::json!({ "target": target }))? {
            ResponseResult::AgentInfo(r) => Ok(r.agent),
            other => Err(Self::unexpected(&other, "agent_info")),
        }
    }

    /// `agent.read` — read the agent's terminal output.
    pub fn agent_read(&self, params: &AgentReadParams) -> Result<PaneReadResult> {
        match self.call("agent.read", params)? {
            ResponseResult::PaneRead(r) => Ok(r.read),
            other => Err(Self::unexpected(&other, "pane_read")),
        }
    }

    /// `agent.send_keys`.
    pub fn agent_send_keys(&self, target: &str, keys: &[&str]) -> Result<()> {
        Self::expect_ok(self.call(
            "agent.send_keys",
            &SendKeysParams {
                target: target.to_string(),
                keys: keys.iter().map(|s| s.to_string()).collect(),
            },
        )?)
    }

    /// `agent.start` — launch an agent in an existing shell pane.
    pub fn agent_start(&self, params: &AgentStartParams) -> Result<AgentStarted> {
        match self.call("agent.start", params)? {
            ResponseResult::AgentStarted(r) => Ok(AgentStarted {
                agent: r.agent,
                argv: r.argv,
            }),
            other => Err(Self::unexpected(&other, "agent_started")),
        }
    }

    /// `agent.prompt` — submit a prompt to an agent.
    ///
    /// Errors of note (see [`HerdrError`]):
    /// - `agent_blocked` → [`HerdrError::is_agent_blocked`]
    /// - `agent_not_ready`, `agent_prompt_stalled`, `agent_prompt_failed`
    /// - `agent_not_found`, `agent_target_ambiguous`, `timeout`,
    ///   `empty_agent_prompt`
    ///
    /// When `wait` is provided the server holds the connection until the
    /// agent reaches one of `until` statuses or `timeout_ms` elapses.
    pub fn agent_prompt(
        &self,
        target: &str,
        text: &str,
        wait: Option<AgentPromptWait>,
    ) -> Result<AgentInfo> {
        match self.call(
            "agent.prompt",
            &AgentPromptParams {
                target: target.to_string(),
                text: text.to_string(),
                wait,
            },
        )? {
            ResponseResult::AgentPrompted(r) => Ok(r.agent),
            ResponseResult::AgentInfo(r) => Ok(r.agent),
            other => Err(Self::unexpected(&other, "agent_prompted")),
        }
    }

    /// `agent.wait` — server-owned wait until the agent reaches a status.
    /// Blocks until then or `timeout_ms` (server-side `timeout` error).
    pub fn agent_wait(
        &self,
        target: &str,
        until: Option<Vec<AgentStatus>>,
        timeout_ms: Option<u64>,
    ) -> Result<AgentInfo> {
        match self.call(
            "agent.wait",
            &AgentWaitParams {
                target: target.to_string(),
                until,
                timeout_ms,
            },
        )? {
            ResponseResult::AgentInfo(r) => Ok(r.agent),
            other => Err(Self::unexpected(&other, "agent_info")),
        }
    }

    /// `agent.view.set`.
    pub fn agent_view_set(&self, params: &AgentViewSetParams) -> Result<AgentViewResult> {
        match self.call("agent.view.set", params)? {
            ResponseResult::AgentView(r) => Ok(AgentViewResult {
                active: r.active,
                label: r.label,
                source: r.source,
            }),
            other => Err(Self::unexpected(&other, "agent_view")),
        }
    }

    /// `agent.view.clear`.
    pub fn agent_view_clear(&self, source: Option<&str>) -> Result<AgentViewResult> {
        match self.call(
            "agent.view.clear",
            &AgentViewClearParams {
                source: source.map(str::to_string),
            },
        )? {
            ResponseResult::AgentView(r) => Ok(AgentViewResult {
                active: r.active,
                label: r.label,
                source: r.source,
            }),
            other => Err(Self::unexpected(&other, "agent_view")),
        }
    }

    // ------------------------------------------------------------------
    // pane.*
    // ------------------------------------------------------------------

    /// `pane.list`.
    pub fn pane_list(&self, workspace_id: Option<&str>) -> Result<Vec<PaneInfo>> {
        match self.call(
            "pane.list",
            &PaneListParams {
                workspace_id: workspace_id.map(str::to_string),
            },
        )? {
            ResponseResult::PaneList(r) => Ok(r.panes),
            other => Err(Self::unexpected(&other, "pane_list")),
        }
    }

    /// `pane.current`.
    pub fn pane_current(&self, caller_pane_id: Option<&str>) -> Result<PaneInfo> {
        match self.call(
            "pane.current",
            &PaneCurrentParams {
                caller_pane_id: caller_pane_id.map(str::to_string),
            },
        )? {
            ResponseResult::PaneCurrent(r) => Ok(r.pane),
            other => Err(Self::unexpected(&other, "pane_current")),
        }
    }

    /// `pane.get`.
    pub fn pane_get(&self, pane_id: &str) -> Result<PaneInfo> {
        match self.call("pane.get", &serde_json::json!({ "pane_id": pane_id }))? {
            ResponseResult::PaneInfo(r) => Ok(r.pane),
            other => Err(Self::unexpected(&other, "pane_info")),
        }
    }

    /// `pane.split`.
    pub fn pane_split(&self, params: &PaneSplitParams) -> Result<PaneInfo> {
        match self.call("pane.split", params)? {
            ResponseResult::PaneInfo(r) => Ok(r.pane),
            other => Err(Self::unexpected(&other, "pane_info")),
        }
    }

    /// `pane.swap`.
    pub fn pane_swap(&self, params: &PaneSwapParams) -> Result<PaneSwapResult> {
        match self.call("pane.swap", params)? {
            ResponseResult::PaneSwap(r) => Ok(r.swap),
            other => Err(Self::unexpected(&other, "pane_swap")),
        }
    }

    /// `pane.move`.
    pub fn pane_move(&self, params: &PaneMoveParams) -> Result<PaneMoveResult> {
        match self.call("pane.move", params)? {
            ResponseResult::PaneMove(r) => Ok(r.move_result),
            other => Err(Self::unexpected(&other, "pane_move")),
        }
    }

    /// `pane.zoom`.
    pub fn pane_zoom(&self, params: &PaneZoomParams) -> Result<PaneZoomResult> {
        match self.call("pane.zoom", params)? {
            ResponseResult::PaneZoom(r) => Ok(r.zoom),
            other => Err(Self::unexpected(&other, "pane_zoom")),
        }
    }

    /// `pane.layout`.
    pub fn pane_layout(&self, pane_id: Option<&str>) -> Result<PaneLayoutSnapshot> {
        match self.call(
            "pane.layout",
            &PaneLayoutParams {
                pane_id: pane_id.map(str::to_string),
            },
        )? {
            ResponseResult::PaneLayout(r) => Ok(r.layout),
            other => Err(Self::unexpected(&other, "pane_layout")),
        }
    }

    /// `pane.process_info` — foreground processes, shell pid, cwd fallback.
    pub fn pane_process_info(&self, pane_id: Option<&str>) -> Result<PaneProcessInfo> {
        match self.call(
            "pane.process_info",
            &PaneProcessInfoParams {
                pane_id: pane_id.map(str::to_string),
            },
        )? {
            ResponseResult::PaneProcessInfo(r) => Ok(r.process_info),
            other => Err(Self::unexpected(&other, "pane_process_info")),
        }
    }

    /// `pane.neighbor`.
    pub fn pane_neighbor(&self, params: &PaneNeighborParams) -> Result<PaneNeighborResult> {
        match self.call("pane.neighbor", params)? {
            ResponseResult::PaneNeighbor(r) => Ok(r.neighbor),
            other => Err(Self::unexpected(&other, "pane_neighbor")),
        }
    }

    /// `pane.edges`.
    pub fn pane_edges(&self, pane_id: Option<&str>) -> Result<PaneEdgesResult> {
        match self.call(
            "pane.edges",
            &PaneEdgesParams {
                pane_id: pane_id.map(str::to_string),
            },
        )? {
            ResponseResult::PaneEdges(r) => Ok(r.edges),
            other => Err(Self::unexpected(&other, "pane_edges")),
        }
    }

    /// `pane.focus_direction`.
    pub fn pane_focus_direction(
        &self,
        direction: PaneDirection,
        pane_id: Option<&str>,
    ) -> Result<PaneFocusDirectionResult> {
        match self.call(
            "pane.focus_direction",
            &PaneFocusDirectionParams {
                direction,
                pane_id: pane_id.map(str::to_string),
            },
        )? {
            ResponseResult::PaneFocusDirection(r) => Ok(r.focus),
            other => Err(Self::unexpected(&other, "pane_focus_direction")),
        }
    }

    /// `pane.resize`.
    pub fn pane_resize(&self, params: &PaneResizeParams) -> Result<PaneResizeResult> {
        match self.call("pane.resize", params)? {
            ResponseResult::PaneResize(r) => Ok(r.resize),
            other => Err(Self::unexpected(&other, "pane_resize")),
        }
    }

    /// `pane.scroll`.
    pub fn pane_scroll(&self, pane_id: &str, offset_from_bottom: i64) -> Result<PaneInfo> {
        match self.call(
            "pane.scroll",
            &PaneScrollParams {
                pane_id: pane_id.to_string(),
                offset_from_bottom,
            },
        )? {
            ResponseResult::PaneInfo(r) => Ok(r.pane),
            other => Err(Self::unexpected(&other, "pane_info")),
        }
    }

    /// `pane.clear`.
    pub fn pane_clear(&self, pane_id: &str) -> Result<()> {
        Self::expect_ok(self.call("pane.clear", &serde_json::json!({ "pane_id": pane_id }))?)
    }

    /// `pane.edit_scrollback`.
    pub fn pane_edit_scrollback(&self, pane_id: &str) -> Result<()> {
        Self::expect_ok(self.call(
            "pane.edit_scrollback",
            &serde_json::json!({ "pane_id": pane_id }),
        )?)
    }

    /// `pane.selection.read`.
    pub fn pane_selection_read(
        &self,
        params: &PaneSelectionReadParams,
    ) -> Result<PaneSelectionResult> {
        match self.call("pane.selection.read", params)? {
            ResponseResult::PaneSelection(r) => Ok(PaneSelectionResult {
                pane_id: r.pane_id,
                text: r.text,
            }),
            other => Err(Self::unexpected(&other, "pane_selection")),
        }
    }

    /// `pane.copy_motion`.
    pub fn pane_copy_motion(&self, params: &PaneCopyMotionParams) -> Result<PaneCopyMotionResult> {
        match self.call("pane.copy_motion", params)? {
            ResponseResult::PaneCopyMotion(r) => Ok(PaneCopyMotionResult {
                pane_id: r.pane_id,
                content_revision: r.content_revision,
                cursor: r.cursor,
            }),
            other => Err(Self::unexpected(&other, "pane_copy_motion")),
        }
    }

    /// `pane.copy_search`.
    pub fn pane_copy_search(&self, params: &PaneCopySearchParams) -> Result<PaneCopySearchResult> {
        match self.call("pane.copy_search", params)? {
            ResponseResult::PaneCopySearch(r) => Ok(PaneCopySearchResult {
                pane_id: r.pane_id,
                content_revision: r.content_revision,
                total: r.total,
                current: r.current,
                current_global: r.current_global,
                matches: r.matches,
            }),
            other => Err(Self::unexpected(&other, "pane_copy_search")),
        }
    }

    /// `pane.focus`.
    pub fn pane_focus(&self, pane_id: &str) -> Result<PaneInfo> {
        match self.call("pane.focus", &serde_json::json!({ "pane_id": pane_id }))? {
            ResponseResult::PaneInfo(r) => Ok(r.pane),
            other => Err(Self::unexpected(&other, "pane_info")),
        }
    }

    /// `pane.input.set`.
    pub fn pane_input_set(&self, pane_id: &str, right_click: PaneRightClickTarget) -> Result<()> {
        Self::expect_ok(self.call(
            "pane.input.set",
            &PaneInputSetParams {
                pane_id: pane_id.to_string(),
                right_click,
            },
        )?)
    }

    /// `pane.link.resolve` — find links in a viewport region.
    pub fn pane_link_resolve(
        &self,
        params: &PaneLinkActivateParams,
    ) -> Result<Vec<PaneLinkRegion>> {
        match self.call("pane.link.resolve", params)? {
            ResponseResult::PaneLinkResolved(r) => Ok(r.regions),
            other => Err(Self::unexpected(&other, "pane_link_resolved")),
        }
    }

    /// `pane.link.activate`.
    pub fn pane_link_activate(
        &self,
        params: &PaneLinkActivateParams,
    ) -> Result<PaneLinkActivatedResult> {
        match self.call("pane.link.activate", params)? {
            ResponseResult::PaneLinkActivated(r) => Ok(PaneLinkActivatedResult {
                handled: r.handled,
                url: r.url,
            }),
            other => Err(Self::unexpected(&other, "pane_link_activated")),
        }
    }

    /// `pane.rename`.
    pub fn pane_rename(&self, pane_id: &str, label: Option<&str>) -> Result<PaneInfo> {
        match self.call(
            "pane.rename",
            &PaneRenameParams {
                pane_id: pane_id.to_string(),
                label: label.map(str::to_string),
            },
        )? {
            ResponseResult::PaneInfo(r) => Ok(r.pane),
            other => Err(Self::unexpected(&other, "pane_info")),
        }
    }

    /// `pane.send_text` — type literal text into a pane.
    pub fn pane_send_text(&self, pane_id: &str, text: &str) -> Result<()> {
        Self::expect_ok(self.call(
            "pane.send_text",
            &PaneSendTextParams {
                pane_id: pane_id.to_string(),
                text: text.to_string(),
            },
        )?)
    }

    /// `pane.send_keys`.
    pub fn pane_send_keys(&self, pane_id: &str, keys: &[&str]) -> Result<()> {
        Self::expect_ok(self.call(
            "pane.send_keys",
            &PaneSendKeysParams {
                pane_id: pane_id.to_string(),
                keys: keys.iter().map(|s| s.to_string()).collect(),
            },
        )?)
    }

    /// `pane.send_input` — text and/or key events.
    pub fn pane_send_input(&self, params: &PaneSendInputParams) -> Result<()> {
        Self::expect_ok(self.call("pane.send_input", params)?)
    }

    /// `pane.read` — read terminal output.
    pub fn pane_read(&self, params: &PaneReadParams) -> Result<PaneReadResult> {
        match self.call("pane.read", params)? {
            ResponseResult::PaneRead(r) => Ok(r.read),
            other => Err(Self::unexpected(&other, "pane_read")),
        }
    }

    /// `pane.report_agent` — custom agent state report.
    pub fn pane_report_agent(&self, params: &PaneReportAgentParams) -> Result<()> {
        Self::expect_ok(self.call("pane.report_agent", params)?)
    }

    /// `pane.report_agent_session` — report agent session identity.
    pub fn pane_report_agent_session(&self, params: &PaneReportAgentSessionParams) -> Result<()> {
        Self::expect_ok(self.call("pane.report_agent_session", params)?)
    }

    /// `pane.report_metadata` — display tokens/state labels/title.
    pub fn pane_report_metadata(&self, params: &PaneReportMetadataParams) -> Result<()> {
        Self::expect_ok(self.call("pane.report_metadata", params)?)
    }

    /// `pane.clear_agent_authority`.
    pub fn pane_clear_agent_authority(&self, params: &PaneClearAgentAuthorityParams) -> Result<()> {
        Self::expect_ok(self.call("pane.clear_agent_authority", params)?)
    }

    /// `pane.release_agent`.
    pub fn pane_release_agent(&self, params: &PaneReleaseAgentParams) -> Result<()> {
        Self::expect_ok(self.call("pane.release_agent", params)?)
    }

    /// `pane.close`.
    pub fn pane_close(&self, pane_id: &str) -> Result<()> {
        Self::expect_ok(self.call("pane.close", &serde_json::json!({ "pane_id": pane_id }))?)
    }

    /// `pane.wait_for_output` — block until the pane output matches.
    ///
    /// Long-lived: the server holds the connection until a match or
    /// `timeout_ms`. Returns the [`OutputMatchedResult`] with the read
    /// snapshot and matched line.
    pub fn pane_wait_for_output(
        &self,
        params: &PaneWaitForOutputParams,
    ) -> Result<OutputMatchedResult> {
        match self.call("pane.wait_for_output", params)? {
            ResponseResult::OutputMatched(r) => Ok(OutputMatchedResult {
                pane_id: r.pane_id,
                revision: r.revision,
                read: r.read,
                matched_line: r.matched_line,
            }),
            other => Err(Self::unexpected(&other, "output_matched")),
        }
    }

    /// `popup.close`.
    pub fn popup_close(&self) -> Result<()> {
        Self::expect_ok(self.call("popup.close", &EmptyParams {})?)
    }

    // ------------------------------------------------------------------
    // layout.*
    // ------------------------------------------------------------------

    /// `layout.export`.
    pub fn layout_export(&self, params: &LayoutExportParams) -> Result<LayoutDescription> {
        match self.call("layout.export", params)? {
            ResponseResult::LayoutExport(r) => Ok(r.layout),
            other => Err(Self::unexpected(&other, "layout_export")),
        }
    }

    /// `layout.apply`.
    pub fn layout_apply(&self, params: &LayoutApplyParams) -> Result<LayoutDescription> {
        match self.call("layout.apply", params)? {
            ResponseResult::LayoutApply(r) => Ok(r.layout),
            other => Err(Self::unexpected(&other, "layout_apply")),
        }
    }

    /// `layout.set_split_ratio`.
    pub fn layout_set_split_ratio(
        &self,
        params: &LayoutSetSplitRatioParams,
    ) -> Result<LayoutDescription> {
        match self.call("layout.set_split_ratio", params)? {
            ResponseResult::LayoutSplitRatioSet(r) => Ok(r.layout),
            other => Err(Self::unexpected(&other, "layout_split_ratio_set")),
        }
    }

    // ------------------------------------------------------------------
    // events.*
    // ------------------------------------------------------------------

    /// `events.subscribe` — open a long-lived subscription stream.
    ///
    /// The returned [`SubscriptionStream`] yields [`SubscriptionFrame`]s until
    /// the server closes the connection (surfaced as [`HerdrError::Closed`]).
    #[tracing::instrument(
        skip_all,
        name = "herdr.request",
        level = "debug",
        fields(method = "events.subscribe")
    )]
    pub fn events_subscribe(&self, subscriptions: Vec<Subscription>) -> Result<SubscriptionStream> {
        let params = EventsSubscribeParams { subscriptions };
        let value = serde_json::to_value(&params)
            .map_err(|e| HerdrError::Protocol(format!("failed to encode params: {e}")))?;
        let mut conn = open_connection(&self.socket_path, self.request_timeout)?;
        let id = self.next_request_id();
        Self::write_request(&mut conn, &id, "events.subscribe", &value)?;
        let mut reader = BufReader::new(conn);
        let line = Self::read_response_line(&mut reader)?;
        let envelope: ResponseEnvelope = serde_json::from_str(&line)
            .map_err(|e| HerdrError::Protocol(format!("malformed subscribe ack: {e}")))?;
        if envelope.id != id {
            return Err(HerdrError::Protocol(format!(
                "subscribe ack id {:?} does not match request id {id:?}",
                envelope.id
            )));
        }
        if let Some(error) = envelope.error {
            return Err(HerdrError::Api {
                code: error.code,
                message: error.message,
            });
        }
        let result = envelope
            .result
            .ok_or_else(|| HerdrError::Protocol("subscribe ack has no result".into()))?;
        match serde_json::from_value::<ResponseResult>(result)
            .map_err(|e| HerdrError::Protocol(format!("failed to decode subscribe ack: {e}")))?
        {
            ResponseResult::SubscriptionStarted => Ok(SubscriptionStream {
                reader,
                _request_id: id,
            }),
            other => Err(Self::unexpected(&other, "subscription_started")),
        }
    }

    /// `events.wait` — block until a lifecycle event matches.
    ///
    /// Long-lived: the server holds the connection until the match or
    /// `timeout_ms`. Returns the matched [`EventEnvelope`].
    pub fn events_wait(
        &self,
        match_event: EventMatch,
        timeout_ms: Option<u64>,
    ) -> Result<EventEnvelope> {
        match self.call(
            "events.wait",
            &EventsWaitParams {
                match_event,
                timeout_ms,
            },
        )? {
            ResponseResult::WaitMatched(r) => Ok(r.event),
            other => Err(Self::unexpected(&other, "wait_matched")),
        }
    }

    // ------------------------------------------------------------------
    // integration.*
    // ------------------------------------------------------------------

    /// `integration.list`.
    pub fn integration_list(&self) -> Result<Vec<IntegrationInfo>> {
        match self.call("integration.list", &EmptyParams {})? {
            ResponseResult::IntegrationList(r) => Ok(r.integrations),
            other => Err(Self::unexpected(&other, "integration_list")),
        }
    }

    /// `integration.install`.
    pub fn integration_install(
        &self,
        target: IntegrationTarget,
    ) -> Result<IntegrationInstallResult> {
        match self.call("integration.install", &IntegrationTargetParams { target })? {
            ResponseResult::IntegrationInstall(r) => Ok(IntegrationInstallResult {
                target: r.target,
                messages: r.details.messages,
            }),
            other => Err(Self::unexpected(&other, "integration_install")),
        }
    }

    /// `integration.uninstall`.
    pub fn integration_uninstall(
        &self,
        target: IntegrationTarget,
    ) -> Result<IntegrationUninstallResult> {
        match self.call("integration.uninstall", &IntegrationTargetParams { target })? {
            ResponseResult::IntegrationUninstall(r) => Ok(IntegrationUninstallResult {
                target: r.target,
                messages: r.details.messages,
            }),
            other => Err(Self::unexpected(&other, "integration_uninstall")),
        }
    }
}

impl Default for HerdrClient {
    fn default() -> Self {
        Self {
            socket_path: resolve_socket_path(),
            request_timeout: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Public result structs returned by typed wrappers (stable API surface).
// ---------------------------------------------------------------------------

/// `workspace.create` result.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceCreated {
    pub workspace: WorkspaceInfo,
    pub tab: TabInfo,
    pub root_pane: PaneInfo,
}

/// `worktree.list` result.
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeList {
    pub source: WorktreeSourceInfo,
    pub worktrees: Vec<WorktreeInfo>,
}

/// `worktree.create` result.
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeCreated {
    pub workspace: WorkspaceInfo,
    pub tab: TabInfo,
    pub root_pane: PaneInfo,
    pub worktree: WorktreeInfo,
}

/// `worktree.open` result.
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeOpened {
    pub workspace: WorkspaceInfo,
    pub tab: TabInfo,
    pub root_pane: PaneInfo,
    pub worktree: WorktreeInfo,
    pub already_open: bool,
}

/// `worktree.remove` result.
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeRemoved {
    pub workspace_id: String,
    pub path: String,
    pub forced: bool,
}

/// `tab.create` result.
#[derive(Debug, Clone, PartialEq)]
pub struct TabCreated {
    pub tab: TabInfo,
    pub root_pane: PaneInfo,
}

/// `agent.start` result.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentStarted {
    pub agent: AgentInfo,
    pub argv: Vec<String>,
}

/// `integration.install` result.
#[derive(Debug, Clone, PartialEq)]
pub struct IntegrationInstallResult {
    pub target: IntegrationTarget,
    pub messages: Vec<String>,
}

/// `integration.uninstall` result.
#[derive(Debug, Clone, PartialEq)]
pub struct IntegrationUninstallResult {
    pub target: IntegrationTarget,
    pub messages: Vec<String>,
}

// ---------------------------------------------------------------------------
// Subscription stream
// ---------------------------------------------------------------------------

/// Long-lived stream of frames from `events.subscribe`.
///
/// Yields [`SubscriptionFrame::Event`] for lifecycle subscriptions
/// (snake_case kinds) and [`SubscriptionFrame::SubscriptionEvent`] for the
/// three high-frequency kinds (dot names). Server-side errors arrive as
/// error envelopes and surface as [`HerdrError::Api`]; EOF surfaces as
/// [`HerdrError::Closed`].
pub struct SubscriptionStream {
    reader: BufReader<BoxConn>,
    _request_id: String,
}

impl std::fmt::Debug for SubscriptionStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubscriptionStream").finish_non_exhaustive()
    }
}

impl SubscriptionStream {
    /// Read the next frame. Blocks until a frame arrives, the server closes
    /// the connection ([`HerdrError::Closed`]), or an error envelope arrives
    /// ([`HerdrError::Api`]).
    pub fn next_frame(&mut self) -> Result<SubscriptionFrame> {
        let line = read_line(&mut self.reader)?;
        let value: serde_json::Value = serde_json::from_str(&line)
            .map_err(|e| HerdrError::Protocol(format!("malformed subscription frame: {e}")))?;
        SubscriptionFrame::from_json_value(value)
    }
}

// ---------------------------------------------------------------------------
// Socket path resolution
// ---------------------------------------------------------------------------

/// Resolve the Herdr API socket path from the environment.
///
/// 1. `$HERDR_SOCKET_PATH` wins when set.
/// 2. `$HERDR_SESSION=<name>` (valid, non-`default`) selects
///    `<config>/herdr/sessions/<name>/herdr.sock`.
/// 3. Default: `<config>/herdr/herdr.sock` where `<config>` is
///    `$XDG_CONFIG_HOME` or `~/.config`.
pub fn resolve_socket_path() -> PathBuf {
    if let Ok(path) = std::env::var(SOCKET_PATH_ENV_VAR) {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    let config_dir = herdr_config_dir();
    if let Some(session) = session_name_from_env() {
        return config_dir.join("sessions").join(session).join("herdr.sock");
    }
    config_dir.join("herdr.sock")
}

fn herdr_config_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("herdr");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".config").join("herdr");
    }
    PathBuf::from("herdr")
}

fn session_name_from_env() -> Option<String> {
    let name = std::env::var(SESSION_ENV_VAR).ok()?;
    if name == DEFAULT_SESSION_NAME || name.is_empty() {
        return None;
    }
    let valid = !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    valid.then_some(name)
}

/// Validate an agent name per the Herdr rule `[a-z][a-z0-9_-]{0,31}`.
pub fn is_valid_agent_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 32
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}
