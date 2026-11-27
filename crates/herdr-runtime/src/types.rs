//! Pool-domain types for [`AgentRuntime`] — the normalized view every
//! backend maps its native records into (RFC §2).

use std::ffi::OsString;
use std::sync::mpsc;

use herdr_protocol::{HerdrClient, MetadataMap};

/// Environment lookup used by [`AgentRuntime::self_identity`].
///
/// Same shape as `herdr_plugin::paths::EnvLookup`; defined locally so this
/// crate does not depend on the plugin crate. Type aliases are transparent,
/// so callers can pass either.
pub type EnvLookup<'a> = dyn Fn(&str) -> Option<OsString> + 'a;

/// Normalized agent lifecycle status stored verbatim in `AgentRecord.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeStatus {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
    /// Backend knows the record is gone (hc finished terminal); herdr never
    /// produces this — dead panes simply leave `agent.list`.
    Offline,
}

impl RuntimeStatus {
    /// Stable lowercase word for logs/tests.
    pub fn as_str(&self) -> &'static str {
        match self {
            RuntimeStatus::Idle => "idle",
            RuntimeStatus::Working => "working",
            RuntimeStatus::Blocked => "blocked",
            RuntimeStatus::Done => "done",
            RuntimeStatus::Unknown => "unknown",
            RuntimeStatus::Offline => "offline",
        }
    }
}

impl std::fmt::Display for RuntimeStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Canonical peer/agent view — the pool's `AgentRecord` source of truth.
/// Each backend maps native records into this.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeAgent {
    /// Pane-equivalent handle: `"w1:p1"` (herdr) or `"t7"`/`"machine/t7"`
    /// (hc). Stable for the record's lifetime; re-resolved every delivery.
    pub handle: String,
    /// Secondary stable id where the backend has one (herdr `terminal_id`).
    /// `None` on hc — adapters must not invent one.
    pub terminal_id: Option<String>,
    /// Detected agent kind (`"claude"`, `"devin"`, ...). Heuristic on hc.
    pub kind: Option<String>,
    /// Pool-facing name (herdr agent name / hc label / terminal name).
    pub name: Option<String>,
    /// Launch cwd (authoritative for scope) and live foreground cwd.
    pub cwd: Option<String>,
    pub foreground_cwd: Option<String>,
    /// Mapped status word — pool stores it verbatim in `AgentRecord.status`.
    pub status: RuntimeStatus,
    /// Backend readiness flags; hc collapses both into `status`/`state`.
    pub interactive_ready: bool,
    pub launch_pending: bool,
    /// Program-reported detail (tokens, titles) — `MetadataMap` passthrough.
    pub extra: MetadataMap,
}

/// Why a delivery was deferred rather than attempted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferReason {
    /// Recipient is alive but blocked on a prompt (herdr `agent_blocked` /
    /// hc `status.state=blocked`).
    Blocked,
    /// Recipient exists but is not promptable yet (launching, resuming).
    NotReady,
}

/// Why a prompt could not be delivered at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendFailureKind {
    /// Target does not resolve (agent/pane gone).
    NotFound,
    /// Target resolves to 2+ candidates.
    Ambiguous,
    /// Backend rejected the prompt for a non-deferrable reason
    /// (stalled, failed, empty).
    Rejected,
}

/// Outcome of a prompt/delivery attempt — replaces herdr's typed errors.
///
/// Folds herdr's `agent_blocked`/`agent_not_ready`/`agent_prompt_stalled`
/// error taxonomy AND hc's `{started,status,turn}` result into one enum the
/// delivery worker switches on once.
#[derive(Debug)]
pub enum SendOutcome {
    /// Text accepted; `started` = program went working within ~seconds
    /// (hc) / prompt submitted (herdr). `None` = backend can't tell.
    Delivered { started: Option<bool> },
    /// Recipient alive but must not be typed into right now. Caller defers,
    /// does NOT consume an attempt.
    Deferred { reason: DeferReason },
    /// Recipient not promptable: not found, ambiguous, not ready, gone.
    /// Carries a stable machine string for logs/tests.
    Failed {
        kind: SendFailureKind,
        message: String,
    },
}

/// Reason-agnostic wait request (RFC §6.3). `handle` is the separate first
/// argument of [`AgentRuntime::wait_agent`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WaitRequest {
    /// Statuses that satisfy the wait. `None` waits for any terminal
    /// condition the backend defines.
    pub until: Option<Vec<RuntimeStatus>>,
    pub timeout_ms: Option<u64>,
    /// Optional output substring; backends without output waits may return
    /// [`RuntimeError::Unsupported`](crate::RuntimeError::Unsupported).
    pub output_match: Option<String>,
}

/// Which condition ended a [`AgentRuntime::wait_agent`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitEnd {
    /// An `until` status (or backend default terminal status) was reached.
    Status,
    /// The `output_match` substring appeared.
    Output,
    /// `timeout_ms` elapsed first.
    Timeout,
}

/// Result of [`AgentRuntime::wait_agent`].
#[derive(Debug, Clone)]
pub struct WaitOutcome {
    /// Freshest agent record available after the wait (`None` when the
    /// backend cannot produce one, e.g. after timeout on a gone handle).
    pub agent: Option<RuntimeAgent>,
    /// Which condition ended the wait.
    pub reason: WaitEnd,
}

/// Normalized runtime event — `registry_sync` never parses backend JSON.
#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeEvent {
    /// A new agent/pane record appeared (or an existing one needs a full
    /// refresh — payload carries the freshest record the backend has).
    AgentSeen { agent: RuntimeAgent },
    /// Status transition for a known handle.
    AgentStatusChanged {
        handle: String,
        status: RuntimeStatus,
    },
    /// The handle's record is gone.
    AgentGone { handle: String },
    /// Handle was re-keyed (herdr `pane.moved`; never fires on hc).
    HandleRekeyed { old: String, new: String },
    /// Workspace/tab-level topology change (herdr `workspace.closed` /
    /// `tab.closed`) — cascading closes the event does not itemize.
    /// Consumers should run a full reconcile. Never fires on hc (flat
    /// terminal model).
    TopologyChanged,
}

/// Snapshot + live stream returned by [`AgentRuntime::subscribe_events`].
///
/// `snapshot` MUST be delivered before any streamed event so callers can
/// bootstrap the registry; reconnect/backoff is caller-owned.
pub struct EventStream {
    pub snapshot: Vec<RuntimeAgent>,
    pub events: mpsc::Receiver<RuntimeEvent>,
}

/// Capability query — lets callers degrade features (toasts, workspaces)
/// without `if backend ==` checks. All flags default to `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RuntimeCaps {
    /// `notification.show`-style user toasts work.
    pub notify: bool,
    /// Plugin panes/popups exist (inbox UI); hc has none.
    pub plugin_panels: bool,
    /// Workspace/tab topology exists (herdr only; hc is flat terminals).
    pub workspaces: bool,
    /// Peers on other machines are addressable (`machine/t7`).
    pub cross_machine: bool,
}

/// This process's own identity as seen by the backend: the env-provided
/// handle plus the agent record when this pane/terminal hosts a known agent.
#[derive(Debug, Clone)]
pub struct SelfIdentity {
    /// Backend handle of self (`HERDR_PANE_ID` on herdr, `HC_TERMINAL_ID`
    /// on hc). `None` when the env exposes no handle.
    pub handle: Option<String>,
    /// Agent record for self, `None` for plain shells or unknown agents.
    pub agent: Option<RuntimeAgent>,
}

/// Backend-agnostic agent runtime. Sync, `Send + Sync`, object-safe
/// (`Box<dyn AgentRuntime>` / `Arc<dyn AgentRuntime>`).
///
/// `handle` arguments are the opaque pane-equivalent handles stored in
/// [`RuntimeAgent::handle`] — callers must not parse them.
pub trait AgentRuntime: Send + Sync {
    /// All live agents/peers visible to this backend.
    fn list_agents(&self) -> Result<Vec<RuntimeAgent>, crate::RuntimeError>;

    /// Single record refresh. `NotFound` when the handle is stale.
    fn get_agent(&self, handle: &str) -> Result<RuntimeAgent, crate::RuntimeError>;

    /// Pane/terminal-level record for `handle`, including agent-less
    /// panes — the self-identity/scope fallback path. herdr: `pane.get`
    /// (a shell pane still carries `cwd`/`terminal_id`); hc: identical to
    /// [`AgentRuntime::get_agent`] since terminals are flat. The default
    /// delegates to `get_agent`, which is correct for flat backends.
    fn get_pane(&self, handle: &str) -> Result<RuntimeAgent, crate::RuntimeError> {
        self.get_agent(handle)
    }

    /// Extra cwd candidates for scope resolution — the foreground-process
    /// table behind `cwd`/`foreground_cwd`. herdr: `pane.process_info`
    /// cwds; hc has no process table, so the default empty vec applies.
    /// Callers consult this only after record cwds fail to resolve.
    fn process_info_cwds(&self, _handle: &str) -> Result<Vec<String>, crate::RuntimeError> {
        Ok(Vec::new())
    }

    /// Deliver a prompt/envelope. The ONLY write path — never raw keys.
    fn send_prompt(&self, handle: &str, text: &str) -> Result<SendOutcome, crate::RuntimeError>;

    /// Block until a status/exit/output condition or timeout.
    /// Used by rescue verify and optional CLI waits.
    fn wait_agent(
        &self,
        handle: &str,
        req: WaitRequest,
    ) -> Result<WaitOutcome, crate::RuntimeError>;

    /// Screen/history text for snapshots and liveness checks.
    fn read_agent(&self, handle: &str, lines: u32) -> Result<String, crate::RuntimeError>;

    /// Raw keys — rescue unblock path only (esc/enter/ctrl+c).
    fn send_keys(&self, handle: &str, keys: &[String]) -> Result<(), crate::RuntimeError>;

    /// This process's own identity (env detection + record lookup).
    fn self_identity(&self, env: &EnvLookup<'_>) -> Result<SelfIdentity, crate::RuntimeError>;

    /// Subscribe to backend events → normalized [`RuntimeEvent`]s.
    /// MUST return a snapshot first (registry bootstrap), then a stream.
    /// Caller owns reconnect/backoff.
    ///
    /// `watched` lists handles the caller wants fine-grained status
    /// events for. On herdr these become per-pane
    /// `pane.agent_status_changed` subscriptions (the server has no
    /// global variant); on hc the feed is already global and the set is
    /// ignored. Backends may also ignore it entirely.
    fn subscribe_events(&self, watched: &[String]) -> Result<EventStream, crate::RuntimeError>;

    /// Escape hatch: the underlying [`HerdrClient`] when this runtime
    /// wraps the herdr backend, `None` on other backends. Temporary
    /// bridge for pool code that still needs herdr-only RPCs (pane
    /// labels, `pane.process_info`) until those features are ported or
    /// dropped — new code should not depend on it.
    fn herdr_client(&self) -> Option<&HerdrClient> {
        None
    }

    /// User-facing toast. herdr: `notification.show`; hc: no-op v1.
    fn notify(&self, title: &str, body: Option<&str>) -> Result<(), crate::RuntimeError> {
        let _ = (title, body);
        Ok(())
    }

    /// Optional capability query — see [`RuntimeCaps`].
    fn capabilities(&self) -> RuntimeCaps {
        RuntimeCaps::default()
    }
}
