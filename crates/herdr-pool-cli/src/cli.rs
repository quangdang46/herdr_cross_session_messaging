//! clap derive definitions for the `herdr-pool` command surface.
//!
//! Globals (`--json`, `--socket-path`, `--state-dir`, `-v`) apply to every
//! subcommand. Exit codes: 0 ok, 1 runtime failure, 2 usage error
//! (clap parse errors and precondition failures such as missing
//! `HERDR_*` env vars).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Peer-to-peer messaging for coding agents inside Herdr.
#[derive(Parser)]
#[command(
    name = "herdr-pool",
    version,
    about = "Peer-to-peer messaging for coding agents inside Herdr",
    long_about = "Project-scoped peer messaging for agents running in Herdr panes.\n\
                  Messages are stored in per-project SQLite pools under the plugin\n\
                  state dir; delivery goes through `agent.prompt` with a\n\
                  `[herdr-pool] msg ...` envelope the receiving agent understands.\n\n\
                  Backend: HERDR_POOL_RUNTIME=herdr|hc (default herdr) selects the\n\
                  agent runtime; the router daemon reads the same flag."
)]
pub struct Cli {
    #[command(flatten)]
    pub globals: Globals,

    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    /// Borrow the global flags (also reachable as `cli.globals`).
    pub fn globals(&self) -> &Globals {
        &self.globals
    }
}

/// Flags shared by every subcommand.
#[derive(Args, Clone, Debug)]
pub struct Globals {
    /// Emit machine-readable JSON output (stable schema per command).
    #[arg(long, global = true)]
    pub json: bool,

    /// Path to the Herdr socket (overrides HERDR_SOCKET_PATH).
    #[arg(long, global = true, value_name = "PATH")]
    pub socket_path: Option<PathBuf>,

    /// Plugin state directory (overrides HERDR_POOL_STATE_DIR / XDG).
    #[arg(long, global = true, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,

    /// Verbose logging (debug level on stderr).
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Show this pane's agent identity, scope, and pool agent id.
    Whoami,

    /// List other live agents in the same project scope.
    Peers,

    /// Send a message to a peer agent.
    Send {
        /// Peer target: `ag_*` id, pane/terminal id, or unambiguous name.
        peer: String,
        /// Message body (one argument; quote multi-word text).
        text: String,
        /// Post into an existing thread instead of starting a new one.
        #[arg(long, value_name = "THREAD_ID")]
        thread: Option<String>,
    },

    /// Send a message to every live peer in this project scope.
    Broadcast {
        /// Message body (one argument; quote multi-word text).
        text: String,
    },

    /// List messages addressed to this agent.
    Inbox {
        /// Only show unread (pending/delivered, not acknowledged) messages.
        #[arg(long)]
        unread: bool,
        /// Keep redrawing the inbox every 2s (for the plugin popup pane).
        #[arg(long)]
        follow: bool,
        /// Optional action on one message.
        #[command(subcommand)]
        action: Option<InboxAction>,
    },

    /// Reply to a message or thread (same thread, to the original sender).
    Reply {
        /// `msg_*` id or `th_*` thread id to reply into.
        target: String,
        /// Reply body.
        text: String,
    },

    /// Per-thread summaries for this scope.
    Threads,

    /// Inspect a single message.
    Message {
        #[command(subcommand)]
        action: MessageAction,
    },

    /// Manage the herdr-routerd delivery daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },

    /// Manage the left-docked messaging panel (inbox split pane).
    Panel {
        #[command(subcommand)]
        action: PanelAction,
    },

    /// Read-only environment/socket/scope/store diagnostics.
    Doctor {
        /// Project root for skill-dir checks (default: agent cwd / cwd).
        #[arg(long, value_name = "DIR")]
        project: Option<PathBuf>,
    },

    /// Install the herdr-messaging agent skill.
    Install {
        /// Install into per-user (~) skill dirs instead of the project.
        #[arg(long)]
        global: bool,
        /// Agent kinds to install for (accepted for forward compatibility;
        /// the installer covers all supported agent skill dirs).
        #[arg(long = "agent", value_name = "KIND")]
        agents: Vec<String>,
        /// Project root (default: current directory).
        #[arg(long, value_name = "DIR")]
        project: Option<PathBuf>,
    },

    /// Remove the herdr-messaging agent skill (marker-owned dirs only).
    Uninstall {
        /// Remove from per-user (~) skill dirs instead of the project.
        #[arg(long)]
        global: bool,
        /// Agent kinds to uninstall for (see `install --agent`).
        #[arg(long = "agent", value_name = "KIND")]
        agents: Vec<String>,
        /// Project root (default: current directory).
        #[arg(long, value_name = "DIR")]
        project: Option<PathBuf>,
    },

    /// Report the outcome of an assigned peer rescue (`rs_*` id).
    Rescue {
        /// Rescue job id from the `[HERDR RESCUE REQUEST]` prompt.
        id: String,
        /// What to report for this rescue job.
        #[command(subcommand)]
        action: RescueAction,
    },

    /// Prune old terminal rows across all project pools (bounds db growth).
    Clean {
        /// Only delete rows older than this many days (default 30).
        #[arg(long, value_name = "DAYS", default_value_t = 30)]
        older_than: u64,
        /// Report what would be deleted without changing anything.
        #[arg(long)]
        dry_run: bool,
    },

    /// Self-update: `herdr plugin update` + skill reinstall + daemon restart.
    Update {
        /// Plugin id or GitHub source to update.
        #[arg(
            long,
            value_name = "ID",
            default_value = "herdr.cross-session-messaging"
        )]
        plugin: String,
        /// Also refresh the global (~) skill install.
        #[arg(long)]
        global: bool,
    },

    /// Print version information.
    Version,
}

/// Action a rescue helper reports via `herdr-pool rescue <id> <action>`.
#[derive(clap::Subcommand, Clone, Debug)]
pub enum RescueAction {
    /// Confirm you are still the assigned helper before touching the
    /// source pane. Fails when the rescue was reassigned — then STOP.
    Claim {
        /// Dispatch token (`dt_*`) from the rescue prompt.
        token: String,
    },
    /// Source agent resumed — job moves to `verifying`.
    Done {
        /// Dispatch token (`dt_*`) from the rescue prompt.
        token: Option<String>,
    },
    /// Cannot resolve safely — escalate to a human now.
    NeedsHuman {
        /// Dispatch token (`dt_*`) from the rescue prompt.
        token: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum InboxAction {
    /// Mark a message acknowledged and print its full content.
    Read {
        /// `msg_*` id.
        id: String,
    },
}

#[derive(Subcommand)]
pub enum MessageAction {
    /// Mark a message acknowledged and print its full content.
    Read {
        /// `msg_*` id.
        id: String,
    },
    /// Print the full message record as JSON.
    Get {
        /// `msg_*` id.
        id: String,
    },
}

#[derive(Subcommand)]
pub enum DaemonAction {
    /// Start the router daemon (detached).
    Start,
    /// Stop the router daemon (graceful).
    Stop,
    /// Show daemon status.
    Status,
    /// Show the daemon log file path and recent lines.
    Logs,
}

/// Action for `herdr-pool panel`.
#[derive(Subcommand)]
pub enum PanelAction {
    /// Open the panel in this tab (focus it if already open).
    Open,
    /// Close the panel in this tab.
    Close,
    /// Close it when focused, focus it when open, open it when absent.
    Toggle,
    /// Auto-dock hook for `[[events]]`; no-ops unless `[panel] autostart`.
    Ensure,
    /// Show panel state for this tab.
    Status,
}
