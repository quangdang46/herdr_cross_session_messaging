//! Output layer: stable `--json` DTOs plus human-readable tables.
//!
//! ## JSON schema notes
//!
//! Every `--json` payload is a single JSON object printed to stdout. Schemas
//! are stable within a minor version; timestamps are unix epoch **milliseconds**
//! (`i64`), ids are the store's `ag_`/`msg_`/`th_` prefixed strings.
//!
//! - `whoami`: `{agent_id, pane_id, workspace_id, tab_id, agent_kind, name,
//!   scope_key, canonical_path, status}`
//! - `peers`: `{scope_key, peers: [AgentJson]}`
//! - `send`/`reply`: `{id, thread_id, kind, status, to_agent, from_agent,
//!   delivered_at, next_retry_at}`
//! - `inbox`/`inbox read`/`message read`: `{messages: [MessageJson]}` or a
//!   single `MessageJson` for `read`
//! - `message get`: `MessageJson`
//! - `threads`: `{threads: [ThreadSummaryJson]}`
//! - `daemon status`: `{running, pid, uptime_secs}`; `start`/`stop`:
//!   `{ok, pid?, message}`; `logs`: `{path, lines}`
//! - `doctor`: `{ok, checks: [{name, status, detail}]}`
//! - `install`/`uninstall`: `{scope, targets: [{path, outcome, reason?}]}`
//! - `update`: `{plugin, source_kind, version_before, version_after,
//!   skills_project, skills_global, daemon}`
//! - `version`: `{name, version, skill_version}`
//! - errors (any command, `--json`): `{"error": {"kind", "message"}}`

use serde::Serialize;

use herdr_pool_store::{AgentRecord, Message};

/// Print a JSON payload to stdout (pretty-printed for readability).
pub fn emit_json<T: Serialize>(value: &T) {
    match serde_json::to_string_pretty(value) {
        Ok(text) => println!("{text}"),
        Err(err) => {
            // Serialization of our DTOs is infallible in practice; degrade
            // to a compact fallback rather than panicking.
            eprintln!("herdr-pool: failed to encode JSON output: {err}");
        }
    }
}

/// Format an epoch-millis timestamp for human output (`YYYY-MM-DD HH:MM:SS UTC`).
pub fn fmt_time(millis: i64) -> String {
    let nanos = (millis as i128) * 1_000_000;
    match time::OffsetDateTime::from_unix_timestamp_nanos(nanos) {
        Ok(dt) => dt
            .format(&time::macros::format_description!(
                "[year]-[month]-[day] [hour]:[minute]:[second] UTC"
            ))
            .unwrap_or_else(|_| millis.to_string()),
        Err(_) => millis.to_string(),
    }
}

/// Short human snippet of message content (first line, ≤60 chars).
pub fn snippet(content: &str, max: usize) -> String {
    let first = content.lines().next().unwrap_or("").trim();
    if first.chars().count() > max {
        let mut s: String = first.chars().take(max).collect();
        s.push('…');
        s
    } else {
        first.to_string()
    }
}

// ---------------------------------------------------------------------------
// JSON DTOs
// ---------------------------------------------------------------------------

/// Serializable view of an [`AgentRecord`].
#[derive(Debug, Clone, Serialize)]
pub struct AgentJson {
    pub agent_id: String,
    pub agent_kind: Option<String>,
    pub name: Option<String>,
    pub status: String,
    pub pane_id: Option<String>,
    pub terminal_id: Option<String>,
    pub cwd: Option<String>,
    pub scope_key: String,
}

impl From<&AgentRecord> for AgentJson {
    fn from(r: &AgentRecord) -> Self {
        Self {
            agent_id: r.agent_id.clone(),
            agent_kind: r.agent_kind.clone(),
            name: r.name.clone(),
            status: r.status.clone(),
            pane_id: r.pane_id.clone(),
            terminal_id: r.terminal_id.clone(),
            cwd: r.cwd.clone(),
            scope_key: r.scope_key.clone(),
        }
    }
}

/// Serializable view of a [`Message`].
#[derive(Debug, Clone, Serialize)]
pub struct MessageJson {
    pub id: String,
    pub thread_id: Option<String>,
    pub from_agent: String,
    pub to_agent: String,
    pub kind: String,
    pub content: String,
    pub status: String,
    pub attempt_count: i64,
    pub next_retry_at: Option<i64>,
    pub delivered_at: Option<i64>,
    pub acked_at: Option<i64>,
    pub created_at: i64,
}

impl From<&Message> for MessageJson {
    fn from(m: &Message) -> Self {
        Self {
            id: m.id.clone(),
            thread_id: m.thread_id.clone(),
            from_agent: m.from_agent.clone(),
            to_agent: m.to_agent.clone(),
            kind: m.kind.clone(),
            content: m.content.clone(),
            status: m.status.to_string(),
            attempt_count: m.attempt_count,
            next_retry_at: m.next_retry_at,
            delivered_at: m.delivered_at,
            acked_at: m.acked_at,
            created_at: m.created_at,
        }
    }
}

/// Per-thread summary row for `threads`.
#[derive(Debug, Clone, Serialize)]
pub struct ThreadSummaryJson {
    pub thread_id: String,
    pub subject: Option<String>,
    /// Snippet of the most recent message.
    pub last_message: Option<String>,
    /// The other party's `agent_id` (peer relative to the caller).
    pub peer_agent_id: Option<String>,
    pub peer_name: Option<String>,
    pub peer_agent_kind: Option<String>,
    /// Messages addressed to the caller not yet acknowledged.
    pub unread_count: i64,
    pub message_count: i64,
    pub last_activity_at: i64,
}

/// `whoami` output.
#[derive(Debug, Clone, Serialize)]
pub struct WhoamiJson {
    pub agent_id: String,
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub agent_kind: Option<String>,
    pub name: Option<String>,
    pub scope_key: String,
    pub canonical_path: String,
    pub status: String,
}

// ---------------------------------------------------------------------------
// Human rendering helpers
// ---------------------------------------------------------------------------

/// Render a fixed-width table as a single string (no trailing newline per
/// row beyond the joins). Shared by [`print_table`] and the `inbox --follow`
/// redraw loop.
pub fn table_string(headers: &[&str], rows: &[Vec<String>]) -> String {
    if headers.is_empty() {
        return String::new();
    }
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
    }
    let render = |cells: &[String]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let w = widths.get(i).copied().unwrap_or(0);
                format!("{c:<w$}")
            })
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    let header_cells: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    let mut out = render(&header_cells);
    for row in rows {
        out.push('\n');
        out.push_str(&render(row));
    }
    out
}

/// A fixed-width table printer: header + rows of string cells.
pub fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let table = table_string(headers, rows);
    if !table.is_empty() {
        println!("{table}");
    }
}
