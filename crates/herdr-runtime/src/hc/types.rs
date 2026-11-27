//! hc/herdr-core wire records — serde view of the `Terminal`, `Status`,
//! and `Foreground` schemas (`hc schema`, hc 0.1.17). Only the fields the
//! pool reads are modeled; everything else is ignored.

use std::collections::HashMap;

use serde::Deserialize;

/// One hc terminal (`terminal.list`/`terminal.get`/event `data`).
///
/// `address` (`t7`, or `machine/t7` for connected machines) is the stable
/// pane-equivalent handle. `state` is `"live"` or `"finished"`; `reported`
/// is whether the program ever sent an OSC 7501 status record.
#[derive(Debug, Clone, Deserialize)]
pub struct Terminal {
    /// Stable per-core address; remote terminals arrive qualified
    /// (`machine/t7`).
    pub address: String,
    #[serde(default)]
    pub name: Option<String>,
    /// Free-form `k=v` labels; the pool reserves `herdr-pool.*`.
    #[serde(default)]
    pub labels: HashMap<String, String>,
    /// Launch working directory — authoritative for scope resolution.
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub argv: Vec<String>,
    /// `"live"` or `"finished"`.
    #[serde(default)]
    pub state: String,
    /// Whether the program ever sent a 7501 report.
    #[serde(default)]
    pub reported: bool,
    /// Current 7501 root record; `None` if the program never reported or
    /// already exited.
    #[serde(default)]
    pub status: Option<Status>,
    /// What runs in the terminal right now (`null` once finished).
    #[serde(default)]
    pub foreground: Option<Foreground>,
    /// Why the program ended or is pending (`spawn_pending`, `exited`, ...).
    #[serde(default)]
    pub reason: Option<String>,
    /// Bumped each time the status leaves `working`.
    #[serde(default)]
    pub turn: u64,
}

/// One OSC 7501 status record. `state` is the program's own word:
/// `idle | working | done | blocked | error` (unverified words pass
/// through and map to [`crate::RuntimeStatus::Unknown`]).
#[derive(Debug, Clone, Deserialize)]
pub struct Status {
    #[serde(default)]
    pub state: String,
    /// Program-reported app name — best `kind` heuristic on hc.
    #[serde(default)]
    pub app: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub msg: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub progress: Option<u64>,
}

/// The foreground process group leader.
#[derive(Debug, Clone, Deserialize)]
pub struct Foreground {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub pid: Option<i64>,
}
