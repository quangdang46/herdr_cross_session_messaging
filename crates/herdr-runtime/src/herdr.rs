//! `HerdrRuntime` — [`AgentRuntime`] impl #1 over the Herdr TUI socket API.
//!
//! Thin mapping layer: every method is a direct call into
//! [`HerdrClient`] plus record/error translation. The only real logic is
//! `agent.prompt` error taxonomy → [`SendOutcome`] and subscription-frame
//! normalization → [`RuntimeEvent`].
//!
//! Known limitations (accepted for the dark landing; the port beads own
//! the fixes):
//!
//! * `subscribe_events` uses only the *static* lifecycle subscription set
//!   (the same kinds `herdr-router` subscribes to minus per-pane
//!   `pane.agent_status_changed`, which needs a watched-pane set the trait
//!   signature does not carry yet).
//! * `self_identity` reads `HERDR_PANE_ID` directly; plugin-context
//!   (`HERDR_PLUGIN_CONTEXT_JSON`) resolution stays in `herdr-pool-core`
//!   until the identity port bead.

use std::sync::mpsc;

use herdr_protocol::{
    AgentInfo, AgentReadParams, AgentStatus, EventData, HerdrClient, HerdrError, OutputMatch,
    PaneInfo, PaneWaitForOutputParams, ReadOptions, ReadSource, Subscription,
    SubscriptionEventData, SubscriptionFrame,
};

use crate::error::RuntimeError;
use crate::types::{
    AgentRuntime, DeferReason, EnvLookup, EventStream, RuntimeAgent, RuntimeCaps, RuntimeEvent,
    RuntimeStatus, SelfIdentity, SendFailureKind, SendOutcome, WaitEnd, WaitOutcome, WaitRequest,
};

/// `AgentRuntime` backed by a Herdr server (`HerdrClient`).
///
/// `handle` == `pane_id` (`w1:p1`); `terminal_id` is populated.
#[derive(Debug, Clone)]
pub struct HerdrRuntime {
    client: HerdrClient,
}

impl HerdrRuntime {
    /// Wrap an existing client.
    pub fn new(client: HerdrClient) -> Self {
        Self { client }
    }

    /// The underlying wire client (escape hatch for pool code that has not
    /// been ported to the trait yet).
    pub fn client(&self) -> &HerdrClient {
        &self.client
    }

    /// Best-effort agent refresh; `None` on any failure.
    fn agent_opt(&self, handle: &str) -> Option<RuntimeAgent> {
        self.client
            .agent_get(handle)
            .ok()
            .map(|a| runtime_agent(&a))
    }
}

impl AgentRuntime for HerdrRuntime {
    fn list_agents(&self) -> Result<Vec<RuntimeAgent>, RuntimeError> {
        Ok(self
            .client
            .agent_list()?
            .iter()
            .map(runtime_agent)
            .collect())
    }

    fn get_agent(&self, handle: &str) -> Result<RuntimeAgent, RuntimeError> {
        match self.client.agent_get(handle) {
            Ok(agent) => Ok(runtime_agent(&agent)),
            Err(err) if is_not_found(&err) => Err(RuntimeError::NotFound {
                handle: handle.to_string(),
            }),
            Err(err) => Err(err.into()),
        }
    }

    fn get_pane(&self, handle: &str) -> Result<RuntimeAgent, RuntimeError> {
        match self.client.pane_get(handle) {
            Ok(pane) => Ok(runtime_agent_from_pane(&pane)),
            Err(err) if is_not_found(&err) => Err(RuntimeError::NotFound {
                handle: handle.to_string(),
            }),
            Err(err) => Err(err.into()),
        }
    }

    fn process_info_cwds(&self, handle: &str) -> Result<Vec<String>, RuntimeError> {
        let info = self.client.pane_process_info(Some(handle))?;
        Ok(info
            .foreground_processes
            .iter()
            .filter_map(|p| p.cwd.clone())
            .collect())
    }

    fn send_prompt(&self, handle: &str, text: &str) -> Result<SendOutcome, RuntimeError> {
        match self.client.agent_prompt(handle, text, None) {
            // herdr `agent.prompt` has no "started" signal — submission is
            // the delivery proof.
            Ok(_) => Ok(SendOutcome::Delivered { started: None }),
            Err(err) => send_outcome_from_error(err),
        }
    }

    fn wait_agent(&self, handle: &str, req: WaitRequest) -> Result<WaitOutcome, RuntimeError> {
        if let Some(needle) = &req.output_match {
            // `agent.wait` cannot match output; `pane.wait_for_output` can.
            // On herdr the handle IS the pane id.
            let params = PaneWaitForOutputParams {
                pane_id: handle.to_string(),
                source: ReadSource::Recent,
                r#match: OutputMatch::Substring {
                    value: needle.clone(),
                },
                timeout_ms: req.timeout_ms,
                ..Default::default()
            };
            return match self.client.pane_wait_for_output(&params) {
                Ok(_) => Ok(WaitOutcome {
                    agent: self.agent_opt(handle),
                    reason: WaitEnd::Output,
                }),
                Err(err) if err.is_timeout() => Ok(WaitOutcome {
                    agent: self.agent_opt(handle),
                    reason: WaitEnd::Timeout,
                }),
                Err(err) if is_not_found(&err) => Err(RuntimeError::NotFound {
                    handle: handle.to_string(),
                }),
                Err(err) => Err(err.into()),
            };
        }

        let until = req.until.map(|statuses| {
            statuses
                .iter()
                .map(|s| runtime_status_to_agent(*s))
                .collect()
        });
        match self.client.agent_wait(handle, until, req.timeout_ms) {
            Ok(agent) => Ok(WaitOutcome {
                agent: Some(runtime_agent(&agent)),
                reason: WaitEnd::Status,
            }),
            Err(err) if err.is_timeout() => Ok(WaitOutcome {
                agent: self.agent_opt(handle),
                reason: WaitEnd::Timeout,
            }),
            Err(err) if is_not_found(&err) => Err(RuntimeError::NotFound {
                handle: handle.to_string(),
            }),
            Err(err) => Err(err.into()),
        }
    }

    fn read_agent(&self, handle: &str, lines: u32) -> Result<String, RuntimeError> {
        let read = self.client.agent_read(&AgentReadParams {
            target: handle.to_string(),
            source: ReadSource::Recent,
            options: ReadOptions {
                lines: Some(lines),
                ..Default::default()
            },
        })?;
        Ok(read.text)
    }

    fn send_keys(&self, handle: &str, keys: &[String]) -> Result<(), RuntimeError> {
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        Ok(self.client.agent_send_keys(handle, &keys)?)
    }

    fn self_identity(&self, env: &EnvLookup<'_>) -> Result<SelfIdentity, RuntimeError> {
        /// Herdr injects this into every pane/agent process.
        const ENV_PANE_ID: &str = "HERDR_PANE_ID";
        let handle = env(ENV_PANE_ID)
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string_lossy().into_owned());
        let agent = match &handle {
            Some(h) => match self.client.agent_get(h) {
                Ok(agent) => Some(runtime_agent(&agent)),
                Err(err) if is_not_found(&err) => None,
                Err(err) => return Err(err.into()),
            },
            None => None,
        };
        Ok(SelfIdentity { handle, agent })
    }

    fn subscribe_events(&self, watched: &[String]) -> Result<EventStream, RuntimeError> {
        // Snapshot first (registry bootstrap), then the live stream.
        let snapshot = self
            .client
            .session_snapshot()?
            .agents
            .iter()
            .map(runtime_agent)
            .collect();
        let stream = self
            .client
            .events_subscribe(lifecycle_subscriptions(watched))?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut stream = stream;
            loop {
                match stream.next_frame() {
                    Ok(frame) => {
                        if let Some(event) = frame_to_runtime_event(&frame) {
                            if tx.send(event).is_err() {
                                break; // caller dropped the receiver
                            }
                        }
                    }
                    // Any read error ends the stream — reconnect/backoff is
                    // the caller's job (RFC §2).
                    Err(err) => {
                        tracing::debug!(error = %err, "herdr event stream ended");
                        break;
                    }
                }
            }
        });
        Ok(EventStream {
            snapshot,
            events: rx,
        })
    }

    fn notify(&self, title: &str, body: Option<&str>) -> Result<(), RuntimeError> {
        self.client
            .notification_show(&herdr_protocol::NotificationShowParams {
                title: title.to_string(),
                body: body.map(str::to_string),
                position: None,
                sound: None,
            })?;
        Ok(())
    }

    fn capabilities(&self) -> RuntimeCaps {
        RuntimeCaps {
            notify: true,
            plugin_panels: true,
            workspaces: true,
            cross_machine: false,
        }
    }

    fn herdr_client(&self) -> Option<&HerdrClient> {
        Some(&self.client)
    }
}

/// Static lifecycle subscriptions plus a per-handle
/// `pane.agent_status_changed` for every watched handle (the server
/// requires a concrete `pane_id`; on herdr the handle IS the pane id).
fn lifecycle_subscriptions(watched: &[String]) -> Vec<Subscription> {
    let mut subs = vec![
        Subscription::PaneAgentDetected,
        Subscription::PaneCreated,
        Subscription::PaneUpdated,
        Subscription::PaneMoved,
        Subscription::PaneClosed,
        Subscription::PaneExited,
        Subscription::WorkspaceClosed,
        Subscription::TabClosed,
    ];
    for handle in watched {
        subs.push(Subscription::PaneAgentStatusChanged {
            pane_id: handle.clone(),
            agent_status: None,
        });
    }
    subs
}

/// Normalize one subscription frame into a [`RuntimeEvent`].
/// Frames that carry no agent-relevant signal return `None`.
fn frame_to_runtime_event(frame: &SubscriptionFrame) -> Option<RuntimeEvent> {
    match frame {
        SubscriptionFrame::Event(envelope) => match &envelope.data {
            EventData::PaneCreated { pane } | EventData::PaneUpdated { pane } => {
                Some(RuntimeEvent::AgentSeen {
                    agent: runtime_agent_from_pane(pane),
                })
            }
            EventData::PaneAgentDetected {
                pane_id,
                agent,
                final_status,
                released,
                ..
            } => {
                if *released {
                    // Agent released from the pane — the record is gone.
                    Some(RuntimeEvent::AgentGone {
                        handle: pane_id.clone(),
                    })
                } else {
                    Some(RuntimeEvent::AgentSeen {
                        agent: RuntimeAgent {
                            handle: pane_id.clone(),
                            terminal_id: None,
                            kind: agent.clone(),
                            name: None,
                            cwd: None,
                            foreground_cwd: None,
                            status: final_status
                                .map(runtime_status)
                                .unwrap_or(RuntimeStatus::Unknown),
                            interactive_ready: true,
                            launch_pending: false,
                            extra: Default::default(),
                        },
                    })
                }
            }
            EventData::PaneAgentStatusChanged {
                pane_id,
                agent_status,
                ..
            } => Some(RuntimeEvent::AgentStatusChanged {
                handle: pane_id.clone(),
                status: runtime_status(*agent_status),
            }),
            EventData::PaneClosed { pane_id, .. } | EventData::PaneExited { pane_id, .. } => {
                Some(RuntimeEvent::AgentGone {
                    handle: pane_id.clone(),
                })
            }
            EventData::PaneMoved {
                pane,
                previous_pane_id,
                ..
            } => Some(RuntimeEvent::HandleRekeyed {
                old: previous_pane_id.clone(),
                new: pane.pane_id.clone(),
            }),
            EventData::WorkspaceClosed { .. } | EventData::TabClosed { .. } => {
                Some(RuntimeEvent::TopologyChanged)
            }
            _ => None,
        },
        SubscriptionFrame::SubscriptionEvent(envelope) => match &envelope.data {
            SubscriptionEventData::PaneAgentStatusChanged(event) => {
                Some(RuntimeEvent::AgentStatusChanged {
                    handle: event.pane_id.clone(),
                    status: runtime_status(event.agent_status),
                })
            }
            _ => None,
        },
    }
}

fn runtime_agent(agent: &AgentInfo) -> RuntimeAgent {
    let mut extra = agent.tokens.clone();
    extra.extend(agent.state_labels.clone());
    RuntimeAgent {
        handle: agent.pane_id.clone(),
        terminal_id: Some(agent.terminal_id.clone()),
        kind: agent.agent.clone().or_else(|| agent.display_agent.clone()),
        name: agent.name.clone(),
        cwd: agent.cwd.clone(),
        foreground_cwd: agent.foreground_cwd.clone(),
        status: runtime_status(agent.agent_status),
        interactive_ready: agent.interactive_ready,
        launch_pending: agent.launch_pending,
        extra,
    }
}

fn runtime_agent_from_pane(pane: &PaneInfo) -> RuntimeAgent {
    let mut extra = pane.tokens.clone();
    extra.extend(pane.state_labels.clone());
    RuntimeAgent {
        handle: pane.pane_id.clone(),
        terminal_id: Some(pane.terminal_id.clone()),
        kind: pane.agent.clone().or_else(|| pane.display_agent.clone()),
        name: pane.label.clone(),
        cwd: pane.cwd.clone(),
        foreground_cwd: pane.foreground_cwd.clone(),
        status: runtime_status(pane.agent_status),
        // PaneInfo carries no readiness flags; match AgentInfo's serde
        // defaults (ready, not pending).
        interactive_ready: true,
        launch_pending: false,
        extra,
    }
}

fn runtime_status(status: AgentStatus) -> RuntimeStatus {
    match status {
        AgentStatus::Idle => RuntimeStatus::Idle,
        AgentStatus::Working => RuntimeStatus::Working,
        AgentStatus::Blocked => RuntimeStatus::Blocked,
        AgentStatus::Done => RuntimeStatus::Done,
        AgentStatus::Unknown => RuntimeStatus::Unknown,
    }
}

/// Inverse mapping for `wait_agent`'s `until` list. `Offline` has no herdr
/// equivalent and degrades to `Unknown` — herdr `agent.wait` cannot wait on
/// a status it never reports anyway.
fn runtime_status_to_agent(status: RuntimeStatus) -> AgentStatus {
    match status {
        RuntimeStatus::Idle => AgentStatus::Idle,
        RuntimeStatus::Working => AgentStatus::Working,
        RuntimeStatus::Blocked => AgentStatus::Blocked,
        RuntimeStatus::Done => AgentStatus::Done,
        RuntimeStatus::Unknown | RuntimeStatus::Offline => AgentStatus::Unknown,
    }
}

fn is_not_found(err: &HerdrError) -> bool {
    err.is_agent_not_found() || err.is_pane_not_found()
}

/// Map `agent.prompt` errors onto [`SendOutcome`]. Transport/protocol
/// failures stay `RuntimeError`s so the delivery worker's retry path is
/// unchanged.
fn send_outcome_from_error(err: HerdrError) -> Result<SendOutcome, RuntimeError> {
    if err.is_agent_blocked() {
        return Ok(SendOutcome::Deferred {
            reason: DeferReason::Blocked,
        });
    }
    if err.is_agent_not_ready() {
        return Ok(SendOutcome::Deferred {
            reason: DeferReason::NotReady,
        });
    }
    if is_not_found(&err) {
        return Ok(SendOutcome::Failed {
            kind: SendFailureKind::NotFound,
            message: err.to_string(),
        });
    }
    if err.is_agent_target_ambiguous() {
        return Ok(SendOutcome::Failed {
            kind: SendFailureKind::Ambiguous,
            message: err.to_string(),
        });
    }
    if let HerdrError::Api { code, .. } = &err {
        if matches!(
            code.as_str(),
            "agent_prompt_stalled" | "agent_prompt_failed" | "empty_agent_prompt"
        ) {
            return Ok(SendOutcome::Failed {
                kind: SendFailureKind::Rejected,
                message: err.to_string(),
            });
        }
    }
    Err(RuntimeError::from(err))
}
