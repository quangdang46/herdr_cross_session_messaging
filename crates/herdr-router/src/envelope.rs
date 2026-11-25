//! Message envelope text formats.
//!
//! These strings are the *wire contract* between the router daemon and the
//! receiving coding agent. The `herdr-pool` CLI (and the `herdr-messaging`
//! skill) parse the same format, so changes here must be mirrored there.
//!
//! ## Message envelope
//!
//! Every pool message is delivered as one `agent.prompt` whose text is:
//!
//! ```text
//! [herdr-pool] msg <id> from <kind> (<agent_id>) thread <tid>:
//! <content>
//! Reply: herdr-pool reply <id> "..."
//! ```
//!
//! - `<id>` — `msg_<uuidv7>` message id.
//! - `<kind>` — sender agent kind (`claude`, `codex`, ...) or display name;
//!   `unknown` when the sender is not (or no longer) registered.
//! - `<agent_id>` — sender's stable `ag_...` pool id.
//! - `<tid>` — `th_<uuidv7>` thread id, or `-` for legacy rows without one.
//!
//! ## Bootstrap prompt
//!
//! Sent once per agent per [`BOOTSTRAP_VERSION`] when the registry first
//! sees a live agent in a managed scope. Kept deliberately compact — the
//! skill files are the durable channel; this is only a pointer.

use herdr_pool_store::{AgentRecord, Message};

/// Prefix marking every router-produced prompt. Agents and the CLI key off
/// this exact string.
pub const ENVELOPE_PREFIX: &str = "[herdr-pool]";

/// Current bootstrap text version. Bump when the bootstrap wording changes;
/// agents whose stored `bootstrap_version` is older get re-bootstrapped once.
pub const BOOTSTRAP_VERSION: u32 = 1;

/// Human label for a sender record: agent kind first, then name.
fn sender_label(record: Option<&AgentRecord>) -> String {
    record
        .and_then(|r| r.agent_kind.clone().or_else(|| r.name.clone()))
        .unwrap_or_else(|| "unknown".to_string())
}

/// Format the delivery envelope for `msg`.
///
/// `sender` is the sender's registry record (used for the `<kind>` label);
/// pass `None` when the sender is unknown — the envelope still carries the
/// raw `from_agent` id.
pub fn message_envelope(msg: &Message, sender: Option<&AgentRecord>) -> String {
    let thread = msg.thread_id.as_deref().unwrap_or("-");
    format!(
        "{ENVELOPE_PREFIX} msg {} from {} ({}) thread {thread}:\n{}\n\
         Reply: herdr-pool reply {} \"...\"",
        msg.id,
        sender_label(sender),
        msg.from_agent,
        msg.content,
        msg.id
    )
}

/// Compact bootstrap prompt text for a newly detected agent.
pub fn bootstrap_text() -> &'static str {
    "[herdr-pool] bootstrap: peer messaging is enabled for this project.\n\
     Other coding agents in this folder can message you through the pool.\n\
     Quick start:\n\
     \x20 - herdr-pool whoami   (your agent id + scope)\n\
     \x20 - herdr-pool peers    (other agents in this project)\n\
     \x20 - herdr-pool inbox    (messages waiting for you)\n\
     Incoming mail arrives as \"[herdr-pool] msg ...\" prompts; reply with\n\
     \x20 herdr-pool reply <msg-id> \"...\"\n\
     Run `herdr-pool --help` for the full CLI. Skill: herdr-messaging."
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn msg() -> Message {
        Message {
            id: "msg_abc".into(),
            thread_id: Some("th_xyz".into()),
            from_agent: "ag_sender".into(),
            to_agent: "ag_me".into(),
            kind: "message".into(),
            content: "please review".into(),
            status: herdr_pool_store::MessageStatus::Pending,
            attempt_count: 0,
            next_retry_at: None,
            delivered_at: None,
            acked_at: None,
            created_at: 0,
        }
    }

    #[test]
    fn envelope_matches_documented_format() {
        let sender = AgentRecord {
            agent_id: "ag_sender".into(),
            scope_key: "s".into(),
            terminal_id: None,
            pane_id: None,
            agent_kind: Some("claude".into()),
            name: None,
            cwd: None,
            status: "idle".into(),
            bootstrap_version: None,
            first_seen_at: 0,
            last_seen_at: 0,
            offline_at: None,
        };
        let text = message_envelope(&msg(), Some(&sender));
        assert_eq!(
            text,
            "[herdr-pool] msg msg_abc from claude (ag_sender) thread th_xyz:\n\
             please review\n\
             Reply: herdr-pool reply msg_abc \"...\""
        );
    }

    #[test]
    fn envelope_without_thread_or_sender() {
        let mut m = msg();
        m.thread_id = None;
        let text = message_envelope(&m, None);
        assert!(text.contains("thread -:"));
        assert!(text.contains("from unknown (ag_sender)"));
    }

    #[test]
    fn bootstrap_mentions_pool_commands() {
        let text = bootstrap_text();
        assert!(text.starts_with("[herdr-pool]"));
        assert!(text.contains("herdr-pool whoami"));
        assert!(text.contains("herdr-pool peers"));
        assert!(text.contains("herdr-pool inbox"));
    }
}
