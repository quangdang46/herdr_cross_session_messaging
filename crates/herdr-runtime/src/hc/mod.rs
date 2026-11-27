//! hc/herdr-core backend — JSON-RPC client transport plus [`HcRuntime`],
//! the [`AgentRuntime`] impl over it (RFC `docs/hc-migration/00-rfc.md` §6).
//!
//! Transport: one `hc connect --json` subprocess per connection (or any
//! argv — `ssh host hc connect` for remote cores). `events.subscribe`
//! hijacks its connection into a notification-only stream, so the runtime
//! owns **two** connections: `rpc` for calls and a dedicated event
//! connection spawned per `subscribe_events` (spike rule, RFC §6.4).
//!
//! Method mapping (verified against `hc schema`, hc 0.1.17):
//!
//! | trait method     | hc RPC                                     |
//! |------------------|--------------------------------------------|
//! | `list_agents`    | `terminal.list`                            |
//! | `get_agent`      | `terminal.get`                             |
//! | `send_prompt`    | `terminal.get` poll + `terminal.send`      |
//! | `wait_agent`     | `terminal.wait` (+ `terminal.get` check)   |
//! | `read_agent`     | `terminal.read`                            |
//! | `send_keys`      | `terminal.keys`                            |
//! | `self_identity`  | `HC_TERMINAL_ID` env + `terminal.get`      |
//! | `subscribe_events`| `hello` + `events.subscribe` (dedicated conn)|
//!
//! Known semantic gaps vs herdr (accepted v1, RFC §6.5/§8):
//!
//! * **No `agent_blocked` send error.** hc refuses blocked sends itself
//!   (`terminal.send` error kind `blocked`, unless `force`), and the
//!   adapter also pre-polls `status.state` — both map to
//!   [`SendOutcome::Deferred`]. Programs that never report 7501 status
//!   are invisible to this check; text is typed anyway (accepted).
//! * **`status.state=error` has no [`RuntimeStatus`] variant** — maps to
//!   `Unknown`.
//! * **`terminal.wait` cannot filter by status word.** It waits for the
//!   next "news" (turn end, exit, output, message, timeout check-in);
//!   `wait_agent` loops on it and re-checks `terminal.get` until the
//!   `until` set matches or the deadline passes.
//! * **`terminal.wait` has no per-status wait** and `timeout` is seconds
//!   (float), not ms.
//! * **`machine_unknown`** (remote core unreachable) surfaces as
//!   [`RuntimeError::Api`], not `NotFound` — the machine may return.
//! * **`missed`/`heartbeat` event notifications are dropped**; a `missed`
//!   means the subscriber fell behind and should re-snapshot (caller-owned
//!   reconnect handles this in v1).
//! * `notify` stays the trait default (no-op): hc has no toast API.

pub mod client;
pub mod error;
pub mod types;

use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::error::RuntimeError;
use crate::types::{
    AgentRuntime, DeferReason, EnvLookup, EventStream, RuntimeAgent, RuntimeCaps, RuntimeEvent,
    RuntimeStatus, SelfIdentity, SendFailureKind, SendOutcome, WaitEnd, WaitOutcome, WaitRequest,
};

pub use client::HcClient;
pub use error::{HcError, Result as HcResult};

use self::types::Terminal;

/// `caller` tag sent on `terminal.send`/`wait`/`get`/`keys`. hc correlates
/// a bare `terminal.wait` with the send that started the turn **per
/// caller**, so the value must be stable per process and unique across
/// pool processes (CLI vs daemon).
const CALLER_PREFIX: &str = "herdr-pool";

/// A live terminal that has never reported 7501 status counts as
/// launch-pending for this long after first observation (RFC §6.2).
const LAUNCH_PENDING_WINDOW: Duration = Duration::from_secs(10);

/// Backoff between `terminal.wait` check-ins when the ended status does
/// not satisfy `WaitRequest::until` (hc waits can't filter by status, so
/// mismatches re-poll).
const WAIT_MISMATCH_BACKOFF: Duration = Duration::from_millis(500);

/// `AgentRuntime` backed by an hc/herdr-core (`HcClient` + a connect
/// template for the dedicated event connection).
///
/// `handle` == terminal `address` (`t7` or `machine/t7`); `terminal_id`
/// is always `None` (hc has no equivalent — adapters must not invent one).
pub struct HcRuntime {
    /// RPC connection — `events.subscribe` is never called on it.
    rpc: HcClient,
    /// Command template for the dedicated event connection
    /// (`hc connect --json`, or e.g. `ssh host hc connect`).
    connect: std::process::Command,
    /// Per-process caller tag for send/wait correlation.
    caller: String,
    /// First-observation timestamps per address, for `launch_pending`.
    first_seen: Arc<Mutex<HashMap<String, Instant>>>,
}

impl HcRuntime {
    /// Connect to the local core: `hc connect --json`.
    pub fn local() -> Result<Self, RuntimeError> {
        Self::connect(hc_connect_command())
    }

    /// Connect with an arbitrary command that speaks hc on stdio —
    /// e.g. `ssh user@host hc connect` for a remote core. Performs the
    /// `hello` handshake on the RPC connection. The command is
    /// templated (program/args/env/cwd) for the dedicated event
    /// connection; `env_clear` is not representable and is not used.
    pub fn connect(cmd: std::process::Command) -> Result<Self, RuntimeError> {
        let template = clone_command(&cmd);
        let rpc = HcClient::connect(cmd)?;
        // `hello` is required before the core streams notifications and
        // doubles as a liveness/version check.
        rpc.hello()?;
        Ok(Self::new(rpc, template))
    }

    /// Wrap an already-connected RPC client plus the command template for
    /// event connections. Performs **no** handshake — callers (tests)
    /// that need one call [`HcClient::hello`] themselves.
    pub fn new(rpc: HcClient, connect: std::process::Command) -> Self {
        Self {
            rpc,
            connect,
            caller: format!("{CALLER_PREFIX}:{}", std::process::id()),
            first_seen: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The underlying JSON-RPC client (escape hatch).
    pub fn rpc(&self) -> &HcClient {
        &self.rpc
    }

    /// `terminal.get` → parsed [`Terminal`]. `RuntimeError::NotFound` when
    /// the address does not resolve (`not_found` kind); `machine_unknown`
    /// stays an [`RuntimeError::Api`] — the remote core may return.
    fn terminal_get(&self, handle: &str) -> Result<Terminal, RuntimeError> {
        let value = match self.rpc.call(
            "terminal.get",
            json!({"terminal": handle, "caller": self.caller}),
        ) {
            Ok(v) => v,
            Err(HcError::Api {
                kind: Some(kind), ..
            }) if kind == "not_found" => {
                return Err(RuntimeError::NotFound {
                    handle: handle.to_string(),
                })
            }
            Err(e) => return Err(e.into()),
        };
        parse_terminal(&value)
            .map_err(|e| RuntimeError::Protocol(format!("terminal.get {handle}: {e}")))
    }

    /// Freshest agent record for `handle`, `None` when it does not
    /// resolve. Used by `wait_agent`/`self_identity`.
    fn agent_opt(&self, handle: &str) -> Option<RuntimeAgent> {
        match self.terminal_get(handle) {
            Ok(t) => Some(terminal_agent(&t, &self.first_seen)),
            Err(_) => None,
        }
    }
}

/// `std::process::Command` is not `Clone`; rebuild an equivalent one from
/// its program/args/env/cwd accessors for the dedicated event
/// connection. `env_clear` has no getter and is not representable —
/// callers must not rely on it (we never set it).
fn clone_command(cmd: &std::process::Command) -> std::process::Command {
    let mut c = std::process::Command::new(cmd.get_program());
    c.args(cmd.get_args());
    if let Some(dir) = cmd.get_current_dir() {
        c.current_dir(dir);
    }
    for (key, value) in cmd.get_envs() {
        match value {
            Some(v) => {
                c.env(key, v);
            }
            None => {
                c.env_remove(key);
            }
        }
    }
    c
}

/// Default connect command: `hc connect --json`.
fn hc_connect_command() -> std::process::Command {
    let mut cmd = std::process::Command::new("hc");
    cmd.args(["connect", "--json"]);
    cmd
}

/// Parse a `Terminal` wire object.
fn parse_terminal(value: &Value) -> Result<Terminal, serde_json::Error> {
    serde_json::from_value(value.clone())
}

/// Map an hc `error.data.kind` string to a [`SendFailureKind`], or `None`
/// when the kind is not a delivery failure (caller maps to `RuntimeError`).
fn send_failure_kind(kind: Option<&str>) -> Option<SendFailureKind> {
    match kind {
        Some("not_found") | Some("finished") | Some("machine_unknown") => {
            Some(SendFailureKind::NotFound)
        }
        Some("unavailable") | Some("status_mismatch") => Some(SendFailureKind::Rejected),
        _ => None,
    }
}

/// `status.state` word → [`RuntimeStatus`].
///
/// `error` maps to `Unknown` (no `RuntimeStatus::Error` variant — gap vs
/// herdr, which reports `done`/`blocked` only anyway).
fn status_word(word: Option<&str>) -> RuntimeStatus {
    match word {
        Some("idle") => RuntimeStatus::Idle,
        Some("working") => RuntimeStatus::Working,
        Some("blocked") => RuntimeStatus::Blocked,
        Some("done") => RuntimeStatus::Done,
        _ => RuntimeStatus::Unknown,
    }
}

/// The terminal's current 7501 status word, if any.
fn current_status_word(t: &Terminal) -> Option<&str> {
    t.status.as_ref().map(|s| s.state.as_str())
}

/// Whether the terminal's program has ever reported status (7501) —
/// `reported` flag or a live status record.
fn has_reported(t: &Terminal) -> bool {
    t.reported || t.status.is_some()
}

/// Record first observation and return elapsed-since-seen.
fn note_seen(first_seen: &Mutex<HashMap<String, Instant>>, address: &str) -> Option<Duration> {
    let mut map = first_seen.lock().unwrap_or_else(|e| e.into_inner());
    let entry = map.entry(address.to_string()).or_insert_with(Instant::now);
    Some(entry.elapsed())
}

/// Map a wire [`Terminal`] into [`RuntimeAgent`]. `first_seen` feeds the
/// `launch_pending` heuristic (live + never-reported + < 10 s old).
fn terminal_agent(t: &Terminal, first_seen: &Mutex<HashMap<String, Instant>>) -> RuntimeAgent {
    let live = t.state == "live";
    let reported = has_reported(t);
    let launch_pending = live
        && !reported
        && note_seen(first_seen, &t.address)
            .map(|age| age < LAUNCH_PENDING_WINDOW)
            .unwrap_or(false);

    // Pool-facing name: herdr-pool.name label > terminal name.
    let name = t
        .labels
        .get("herdr-pool.name")
        .cloned()
        .or_else(|| t.name.clone());

    // Kind: herdr-pool.agent-kind label > status.app > foreground.name >
    // argv[0] basename. Heuristic only — 7501 coverage per CLI is an
    // open RFC question (§8 Q2).
    let kind = t
        .labels
        .get("herdr-pool.agent-kind")
        .cloned()
        .or_else(|| t.status.as_ref().and_then(|s| s.app.clone()))
        .or_else(|| {
            t.foreground
                .as_ref()
                .and_then(|f| f.name.clone())
                .filter(|n| !n.is_empty())
        })
        .or_else(|| {
            t.argv
                .first()
                .and_then(|a| std::path::Path::new(a).file_name())
                .map(|n| n.to_string_lossy().into_owned())
        });

    let mut extra = t.labels.clone();
    if let Some(s) = &t.status {
        extra.insert("status.state".to_string(), s.state.clone());
        for (key, val) in [
            ("status.app", &s.app),
            ("status.kind", &s.kind),
            ("status.msg", &s.msg),
            ("status.title", &s.title),
        ] {
            if let Some(v) = val {
                if !v.is_empty() {
                    extra.insert(key.to_string(), v.clone());
                }
            }
        }
        if let Some(p) = s.progress {
            extra.insert("status.progress".to_string(), p.to_string());
        }
    }

    RuntimeAgent {
        handle: t.address.clone(),
        terminal_id: None, // hc has no terminal_id equivalent — do not invent.
        kind,
        name,
        cwd: t.cwd.clone(),
        foreground_cwd: t.foreground.as_ref().and_then(|f| f.cwd.clone()),
        status: if live {
            status_word(current_status_word(t))
        } else {
            RuntimeStatus::Offline
        },
        interactive_ready: live && reported,
        launch_pending,
        extra,
    }
}

/// Normalize one `{"method":"event"}` notification frame into a
/// [`RuntimeEvent`]. hc event `params` = `{type, terminal, data, seq}`;
/// `data` is a full [`Terminal`] for `created`/`lost`/`restarted`/`keep`
/// and lifecycle notices for `status`/`finished`/`removed`.
///
/// `heartbeat`, `missed` (subscriber fell behind — caller reconnects),
/// `title`, `labels`, `size`, `doc`, `turn`, `foreground` carry no
/// pool-relevant transition and return `None`.
fn event_to_runtime(
    frame: &Value,
    first_seen: &Mutex<HashMap<String, Instant>>,
) -> Option<RuntimeEvent> {
    if frame.get("method").and_then(Value::as_str) != Some("event") {
        return None;
    }
    let params = frame.get("params")?;
    let kind = params.get("type").and_then(Value::as_str)?;
    let handle = params.get("terminal").and_then(Value::as_str)?.to_string();
    let data = params.get("data").cloned().unwrap_or(Value::Null);

    match kind {
        "created" | "lost" | "restarted" | "keep" => {
            let t = parse_terminal(&data).ok()?;
            Some(RuntimeEvent::AgentSeen {
                agent: terminal_agent(&t, first_seen),
            })
        }
        "status" => {
            // `data` is the new 7501 record (or a small change object).
            // Prefer an explicit status word; fall back to a full
            // Terminal payload.
            let word = data.get("state").and_then(Value::as_str).or_else(|| {
                data.get("status")
                    .and_then(|s| s.get("state"))
                    .and_then(Value::as_str)
            });
            if let Some(word) = word {
                Some(RuntimeEvent::AgentStatusChanged {
                    handle,
                    status: status_word(Some(word)),
                })
            } else if let Ok(t) = parse_terminal(&data) {
                Some(RuntimeEvent::AgentSeen {
                    agent: terminal_agent(&t, first_seen),
                })
            } else {
                None
            }
        }
        "finished" | "removed" => Some(RuntimeEvent::AgentGone { handle }),
        _ => None,
    }
}

impl AgentRuntime for HcRuntime {
    fn list_agents(&self) -> Result<Vec<RuntimeAgent>, RuntimeError> {
        let value = self.rpc.call("terminal.list", json!({}))?;
        let terminals = value
            .get("terminals")
            .and_then(Value::as_array)
            .ok_or_else(|| RuntimeError::Protocol("terminal.list: no terminals".into()))?;
        terminals
            .iter()
            .map(|v| {
                let t = parse_terminal(v)
                    .map_err(|e| RuntimeError::Protocol(format!("terminal.list: {e}")))?;
                Ok(terminal_agent(&t, &self.first_seen))
            })
            .collect()
    }

    fn get_agent(&self, handle: &str) -> Result<RuntimeAgent, RuntimeError> {
        let t = self.terminal_get(handle)?;
        Ok(terminal_agent(&t, &self.first_seen))
    }

    fn send_prompt(&self, handle: &str, text: &str) -> Result<SendOutcome, RuntimeError> {
        // Pre-send poll (RFC §6.1): hc has no agent_blocked send error —
        // the core refuses blocked sends (kind `blocked`) but only AFTER
        // deciding to type; polling first avoids the roundtrip and gives
        // precise defer reasons. `status.state` absent → proceed.
        let term = match self.terminal_get(handle) {
            Ok(t) => t,
            Err(RuntimeError::NotFound { .. }) => {
                return Ok(SendOutcome::Failed {
                    kind: SendFailureKind::NotFound,
                    message: format!("hc terminal not found: {handle}"),
                })
            }
            Err(e) => return Err(e),
        };

        if term.state == "finished" {
            return Ok(SendOutcome::Failed {
                kind: SendFailureKind::NotFound,
                message: format!("hc terminal finished: {handle}"),
            });
        }
        if current_status_word(&term) == Some("blocked") {
            return Ok(SendOutcome::Deferred {
                reason: DeferReason::Blocked,
            });
        }
        // `spawn_pending` reason or the first-seen launch window → not
        // promptable yet. Mirrors herdr's `agent_not_ready` defer.
        if term.reason.as_deref() == Some("spawn_pending")
            || terminal_agent(&term, &self.first_seen).launch_pending
        {
            return Ok(SendOutcome::Deferred {
                reason: DeferReason::NotReady,
            });
        }

        // No `if` gate: hc's `if` only matches exactly "idle"|"working"
        // and cannot express "not blocked"; the core already refuses
        // blocked/finished sends, and the pre-poll covers the common case.
        let result = self.rpc.call(
            "terminal.send",
            json!({
                "terminal": handle,
                "text": text,
                "caller": self.caller,
            }),
        );
        match result {
            Ok(value) => {
                // SendResult.started: bool|null — null when the program
                // doesn't report status (nobody can tell).
                let started = value.get("started").and_then(Value::as_bool);
                Ok(SendOutcome::Delivered { started })
            }
            Err(HcError::Api { kind, message, .. }) => {
                if kind.as_deref() == Some("blocked") {
                    return Ok(SendOutcome::Deferred {
                        reason: DeferReason::Blocked,
                    });
                }
                if let Some(fk) = send_failure_kind(kind.as_deref()) {
                    return Ok(SendOutcome::Failed {
                        kind: fk,
                        message: format!("hc {kind:?}: {message}"),
                    });
                }
                Err(RuntimeError::Api {
                    code: kind.unwrap_or_else(|| "hc".to_string()),
                    message,
                })
            }
            Err(e) => Err(e.into()),
        }
    }

    fn wait_agent(&self, handle: &str, req: WaitRequest) -> Result<WaitOutcome, RuntimeError> {
        // hc `terminal.wait` blocks until the next "news" (turn end,
        // exit, output match, message, timeout check-in) — it cannot
        // filter by status word, so loop: wait → terminal.get → check
        // `until`. `caller` correlates the wait with sends from this
        // process (a bare wait follows the caller's latest send turn).
        let deadline = req
            .timeout_ms
            .map(|ms| Instant::now() + Duration::from_millis(ms));

        loop {
            if let Some(d) = deadline {
                let remaining = d.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Ok(WaitOutcome {
                        agent: self.agent_opt(handle),
                        reason: WaitEnd::Timeout,
                    });
                }
            }

            let mut params = json!({
                "terminal": handle,
                "caller": self.caller,
            });
            if let Some(d) = deadline {
                let remaining = d.saturating_duration_since(Instant::now());
                // timeout is seconds (float); floor at 1ms so a nearly-
                // expired deadline still makes one check-in call.
                params["timeout"] = json!(remaining.as_secs_f64().max(0.001));
            }
            if let Some(needle) = &req.output_match {
                params["output"] = json!(needle);
            }

            let result = self.rpc.call("terminal.wait", params);
            let waited = match result {
                Ok(v) => v,
                Err(HcError::Api { kind, .. }) if kind.as_deref() == Some("not_found") => {
                    return Err(RuntimeError::NotFound {
                        handle: handle.to_string(),
                    })
                }
                Err(e) => return Err(e.into()),
            };

            let ended = waited
                .get("ended")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let reason = waited.get("reason").and_then(Value::as_str).unwrap_or("");

            // Timeout check-in (ended=false) or explicit timeout reason:
            // the wait's own timeout elapsed — a deadline always exists
            // here because we only pass `timeout` when one does, but a
            // server-side default check-in can still fire.
            if !ended || reason == "timeout" {
                match deadline {
                    Some(d) if Instant::now() < d => {
                        // Check-in before our deadline: keep waiting.
                        std::thread::sleep(
                            WAIT_MISMATCH_BACKOFF.min(d.saturating_duration_since(Instant::now())),
                        );
                        continue;
                    }
                    _ => {
                        return Ok(WaitOutcome {
                            agent: self.agent_opt(handle),
                            reason: WaitEnd::Timeout,
                        })
                    }
                }
            }

            if reason == "output" && req.output_match.is_some() {
                return Ok(WaitOutcome {
                    agent: self.agent_opt(handle),
                    reason: WaitEnd::Output,
                });
            }

            // Wait ended on news: fetch the fresh record and check `until`.
            // A gone handle counts as `Offline` for matching purposes.
            let agent = match self.get_agent(handle) {
                Ok(a) => Some(a),
                Err(RuntimeError::NotFound { .. }) => None,
                Err(e) => return Err(e),
            };
            let status = agent
                .as_ref()
                .map(|a| a.status)
                .unwrap_or(RuntimeStatus::Offline);
            let matched = match &req.until {
                None => true,
                Some(until) => until.contains(&status),
            };
            if matched {
                return Ok(WaitOutcome {
                    agent,
                    reason: WaitEnd::Status,
                });
            }

            // Ended but status not in `until` — re-wait. Without a
            // deadline this could poll forever; that is the caller's
            // contract (same as herdr agent.wait semantics).
            if let Some(d) = deadline {
                let remaining = d.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Ok(WaitOutcome {
                        agent,
                        reason: WaitEnd::Timeout,
                    });
                }
                std::thread::sleep(WAIT_MISMATCH_BACKOFF.min(remaining));
            } else {
                std::thread::sleep(WAIT_MISMATCH_BACKOFF);
            }
        }
    }

    fn read_agent(&self, handle: &str, lines: u32) -> Result<String, RuntimeError> {
        let value = self.rpc.call(
            "terminal.read",
            json!({
                "terminal": handle,
                "lines": lines,
                "caller": self.caller,
            }),
        )?;
        value
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| RuntimeError::Protocol("terminal.read: no text".into()))
    }

    fn send_keys(&self, handle: &str, keys: &[String]) -> Result<(), RuntimeError> {
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        self.rpc.call(
            "terminal.keys",
            json!({
                "terminal": handle,
                "keys": keys,
                "caller": self.caller,
            }),
        )?;
        Ok(())
    }

    fn self_identity(&self, env: &EnvLookup<'_>) -> Result<SelfIdentity, RuntimeError> {
        /// hc injects the terminal's own address into terminal processes.
        const ENV_TERMINAL_ID: &str = "HC_TERMINAL_ID";
        let handle = env(ENV_TERMINAL_ID)
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string_lossy().into_owned());
        let agent = match &handle {
            Some(h) => self.agent_opt(h),
            None => None,
        };
        Ok(SelfIdentity { handle, agent })
    }

    fn subscribe_events(&self, _watched: &[String]) -> Result<EventStream, RuntimeError> {
        // Dedicated connection: `events.subscribe` turns it into a
        // notification-only stream, so RPC keeps using `self.rpc`.
        let event_client = HcClient::connect(clone_command(&self.connect))?;
        event_client.hello()?;
        // Receiver must exist before subscribing or early notifications
        // are dropped (they fan out to subscribers registered at emit
        // time).
        let raw = event_client.subscribe_events();
        let snapshot_value = event_client.events_subscribe(&[])?;
        let snapshot = snapshot_value
            .get("terminals")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| parse_terminal(v).ok())
                    .map(|t| terminal_agent(&t, &self.first_seen))
                    .collect()
            })
            .unwrap_or_default();

        let (tx, rx) = mpsc::channel();
        let first_seen = Arc::clone(&self.first_seen);
        std::thread::spawn(move || {
            // Keep the client alive for the stream's lifetime; when the
            // caller drops `rx`, sends fail and we exit, dropping the
            // client (kills `hc connect`).
            let _event_client = event_client;
            // Channel closed: core gone or client dropped. Reconnect/
            // backoff is the caller's job (RFC §2).
            while let Ok(frame) = raw.recv() {
                if let Some(event) = event_to_runtime(&frame, &first_seen) {
                    if tx.send(event).is_err() {
                        break; // caller dropped the receiver
                    }
                }
            }
        });

        Ok(EventStream {
            snapshot,
            events: rx,
        })
    }

    fn capabilities(&self) -> RuntimeCaps {
        RuntimeCaps {
            // hc has no notification.show — trait default no-op applies.
            notify: false,
            plugin_panels: false,
            workspaces: false,
            // `machine/t7` addresses are natively supported by every
            // terminal.* call and events.subscribe spans machines.
            cross_machine: true,
        }
    }
}
