//! `herdr-plugin.toml` manifest model.
//!
//! Herdr discovers plugins via a TOML manifest at the plugin root. Parsing is
//! tolerant: unknown fields are ignored so newer manifests still load on older
//! clients. [`Manifest::validate`] enforces the invariants Herdr relies on
//! (id charset, local action/pane/link-handler ids without dots).

use std::path::Path;

use serde::Deserialize;

use crate::error::{PluginError, Result};

/// Canonical manifest file name at the plugin root.
pub const MANIFEST_FILE_NAME: &str = "herdr-plugin.toml";

/// Parsed `herdr-plugin.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    /// Plugin id, e.g. `herdr.cross-session-messaging`.
    pub id: String,
    /// Human-readable plugin name.
    pub name: String,
    /// Plugin version (semver string).
    pub version: String,
    /// Minimum Herdr version this plugin requires.
    #[serde(default)]
    pub min_herdr_version: Option<String>,
    /// Short description shown in plugin listings.
    #[serde(default)]
    pub description: Option<String>,
    /// Platforms this plugin supports (`linux`, `macos`, `windows`, ...).
    #[serde(default)]
    pub platforms: Vec<String>,
    /// Build hooks run at plugin build/install time.
    #[serde(default)]
    pub build: Vec<Build>,
    /// One-shot startup hooks run after session restore/live handoff.
    #[serde(default)]
    pub startup: Vec<Startup>,
    /// User-invocable actions.
    #[serde(default)]
    pub actions: Vec<Action>,
    /// Event hooks fired on Herdr runtime events.
    #[serde(default)]
    pub events: Vec<EventHook>,
    /// Pane templates opened by the plugin.
    #[serde(default)]
    pub panes: Vec<Pane>,
    /// Link handlers for `plugin://` or custom URL patterns.
    #[serde(default, rename = "link_handlers")]
    pub link_handlers: Vec<LinkHandler>,
}

/// A `[[build]]` entry: a command run at build time.
#[derive(Debug, Clone, Deserialize)]
pub struct Build {
    /// Optional label for this build step.
    #[serde(default)]
    pub name: Option<String>,
    /// Argv to execute.
    pub command: Vec<String>,
    /// Platforms this step applies to; empty means all.
    #[serde(default)]
    pub platforms: Vec<String>,
    /// Herdr contexts this step applies to; empty means all.
    #[serde(default)]
    pub contexts: Vec<String>,
}

/// A `[[startup]]` entry: a one-shot command run on plugin startup.
#[derive(Debug, Clone, Deserialize)]
pub struct Startup {
    /// Argv to execute.
    pub command: Vec<String>,
    /// Platforms this hook applies to; empty means all.
    #[serde(default)]
    pub platforms: Vec<String>,
    /// Herdr contexts this hook applies to; empty means all.
    #[serde(default)]
    pub contexts: Vec<String>,
}

/// An `[[actions]]` entry: a named, user-invocable command.
#[derive(Debug, Clone, Deserialize)]
pub struct Action {
    /// Local action id (no dots; unique within the manifest).
    pub id: String,
    /// Title shown in command palettes.
    #[serde(default)]
    pub title: Option<String>,
    /// Argv to execute.
    pub command: Vec<String>,
    /// Platforms this action applies to; empty means all.
    #[serde(default)]
    pub platforms: Vec<String>,
    /// Herdr contexts (e.g. `workspace`) this action applies to.
    #[serde(default)]
    pub contexts: Vec<String>,
    /// Optional pane placement hint (`overlay`, `popup`, `split`, `tab`,
    /// `zoomed`) when the action opens UI.
    #[serde(default)]
    pub placement: Option<String>,
}

/// An `[[events]]` entry: a command run when a Herdr event fires.
#[derive(Debug, Clone, Deserialize)]
pub struct EventHook {
    /// Event name, e.g. `pane.agent_detected`. Validated by Herdr at link
    /// time; unknown names produce warnings, not failures.
    pub on: String,
    /// Argv to execute.
    pub command: Vec<String>,
    /// Platforms this hook applies to; empty means all.
    #[serde(default)]
    pub platforms: Vec<String>,
    /// Herdr contexts this hook applies to; empty means all.
    #[serde(default)]
    pub contexts: Vec<String>,
}

/// A `[[panes]]` entry: a pane template the plugin can open.
#[derive(Debug, Clone, Deserialize)]
pub struct Pane {
    /// Local pane id (no dots; unique within the manifest).
    pub id: String,
    /// Title shown for the pane.
    #[serde(default)]
    pub title: Option<String>,
    /// Argv to run inside the pane.
    pub command: Vec<String>,
    /// Placement (`overlay`, `popup`, `split`, `tab`, `zoomed`).
    #[serde(default)]
    pub placement: Option<String>,
    /// Optional link/output pattern associated with the pane.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Platforms this pane applies to; empty means all.
    #[serde(default)]
    pub platforms: Vec<String>,
    /// Herdr contexts this pane applies to.
    #[serde(default)]
    pub contexts: Vec<String>,
}

/// A `[[link_handlers]]` entry: routes matching links to an action or command.
#[derive(Debug, Clone, Deserialize)]
pub struct LinkHandler {
    /// Optional local id (no dots when present).
    #[serde(default)]
    pub id: Option<String>,
    /// Pattern the link must match.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Id of an `[[actions]]` entry to invoke.
    #[serde(default)]
    pub action: Option<String>,
    /// Argv to execute directly (alternative to `action`).
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// Platforms this handler applies to; empty means all.
    #[serde(default)]
    pub platforms: Vec<String>,
}

impl Manifest {
    /// Parse a manifest from TOML text and validate it.
    pub fn parse(text: &str) -> Result<Self> {
        let manifest: Manifest =
            toml::from_str(text).map_err(|err| PluginError::Manifest(format!("parse: {err}")))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Load and parse `herdr-plugin.toml` (or `path` if given) from disk.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|err| PluginError::Manifest(format!("read {}: {err}", path.display())))?;
        Self::parse(&text)
    }

    /// Load the manifest from a plugin root directory.
    pub fn load_from_dir(root: &Path) -> Result<Self> {
        Self::load(&root.join(MANIFEST_FILE_NAME))
    }

    /// Validate cross-field invariants. Unknown TOML fields are already
    /// ignored by serde; this checks required values and id rules.
    pub fn validate(&self) -> Result<()> {
        validate_plugin_id(&self.id)?;
        require_nonempty("name", &self.name)?;
        require_nonempty("version", &self.version)?;
        if let Some(v) = &self.min_herdr_version {
            require_nonempty("min_herdr_version", v)?;
        }

        for (i, build) in self.build.iter().enumerate() {
            require_argv(&format!("build[{i}].command"), &build.command)?;
        }
        for (i, hook) in self.startup.iter().enumerate() {
            require_argv(&format!("startup[{i}].command"), &hook.command)?;
        }
        for (i, action) in self.actions.iter().enumerate() {
            validate_local_id(&format!("actions[{i}].id"), &action.id)?;
            require_argv(&format!("actions[{i}].command"), &action.command)?;
        }
        for (i, event) in self.events.iter().enumerate() {
            require_nonempty(&format!("events[{i}].on"), &event.on)?;
            require_argv(&format!("events[{i}].command"), &event.command)?;
        }
        for (i, pane) in self.panes.iter().enumerate() {
            validate_local_id(&format!("panes[{i}].id"), &pane.id)?;
            require_argv(&format!("panes[{i}].command"), &pane.command)?;
        }
        for (i, handler) in self.link_handlers.iter().enumerate() {
            if let Some(id) = &handler.id {
                validate_local_id(&format!("link_handlers[{i}].id"), id)?;
            }
            if handler.action.is_none() && handler.command.is_none() {
                return Err(PluginError::Manifest(format!(
                    "link_handlers[{i}] needs `action` or `command`"
                )));
            }
            if let Some(cmd) = &handler.command {
                require_argv(&format!("link_handlers[{i}].command"), cmd)?;
            }
        }
        Ok(())
    }
}

/// Plugin id charset: `[a-zA-Z0-9:._-]+` (dots allowed, e.g. `herdr.foo`).
fn validate_plugin_id(id: &str) -> Result<()> {
    if id.is_empty() {
        return Err(PluginError::Manifest("id must not be empty".into()));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '_' | '-'))
    {
        return Err(PluginError::Manifest(format!(
            "id {id:?} contains invalid characters (allowed: a-zA-Z0-9:._-)"
        )));
    }
    Ok(())
}

/// Local ids (actions, panes, link handlers) must not contain dots — they are
/// referenced as bare names inside the plugin, not as dotted paths.
fn validate_local_id(field: &str, id: &str) -> Result<()> {
    if id.is_empty() {
        return Err(PluginError::Manifest(format!("{field} must not be empty")));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    {
        return Err(PluginError::Manifest(format!(
            "{field} {id:?} is invalid (allowed: a-zA-Z0-9_-, no dots)"
        )));
    }
    Ok(())
}

fn require_nonempty(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(PluginError::Manifest(format!("{field} must not be empty")));
    }
    Ok(())
}

fn require_argv(field: &str, argv: &[String]) -> Result<()> {
    if argv.is_empty() || argv.iter().any(|a| a.is_empty()) {
        return Err(PluginError::Manifest(format!(
            "{field} must be a non-empty argv array"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
id = "herdr.cross-session-messaging"
name = "Cross-Session Messaging"
version = "0.1.0"
min_herdr_version = "0.9.0"
description = "Peer messaging for Herdr agents"
platforms = ["linux", "macos", "windows"]
unknown_top_level_field = "ignored"

[[build]]
command = ["cargo", "build", "--release"]
platforms = ["linux"]

[[startup]]
command = ["herdr-routerd", "start"]
contexts = ["workspace"]
extra_hook_field = 42

[[actions]]
id = "open-inbox"
title = "Messaging: open inbox"
contexts = ["workspace"]
command = ["herdr-pool", "inbox", "--ui"]
placement = "popup"

[[events]]
on = "pane.agent_detected"
command = ["herdr-routerd", "tick"]

[[panes]]
id = "inbox-view"
title = "Inbox"
placement = "split"
command = ["herdr-pool", "inbox", "--ui"]

[[link_handlers]]
id = "pool-link"
pattern = "herdr-pool://message/*"
action = "open-inbox"
"#;

    #[test]
    fn parses_full_manifest() {
        let m = Manifest::parse(VALID).unwrap();
        assert_eq!(m.id, "herdr.cross-session-messaging");
        assert_eq!(m.min_herdr_version.as_deref(), Some("0.9.0"));
        assert_eq!(m.platforms.len(), 3);
        assert_eq!(m.build.len(), 1);
        assert_eq!(m.startup.len(), 1);
        assert_eq!(m.actions[0].id, "open-inbox");
        assert_eq!(m.actions[0].placement.as_deref(), Some("popup"));
        assert_eq!(m.events[0].on, "pane.agent_detected");
        assert_eq!(m.panes[0].id, "inbox-view");
        assert_eq!(m.link_handlers[0].action.as_deref(), Some("open-inbox"));
    }

    #[test]
    fn minimal_manifest_ok() {
        let m = Manifest::parse(
            r#"
id = "a.b"
name = "N"
version = "0.0.1"
"#,
        )
        .unwrap();
        assert!(m.actions.is_empty() && m.events.is_empty());
    }

    #[test]
    fn rejects_bad_plugin_id_charset() {
        let err = Manifest::parse(r#"id = "bad id!" name = "N" version = "1""#).unwrap_err();
        assert!(matches!(err, PluginError::Manifest(_)));
    }

    #[test]
    fn rejects_dot_in_action_id() {
        let err = Manifest::parse(
            r#"
id = "a.b"
name = "N"
version = "1"
[[actions]]
id = "has.dot"
command = ["x"]
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no dots"));
    }

    #[test]
    fn rejects_missing_required_fields() {
        assert!(Manifest::parse("name = \"N\"").is_err());
        assert!(Manifest::parse(
            "id = \"a\" name = \"N\" version = \"1\"\n[[events]]\non = \"x\"\n"
        )
        .is_err());
    }

    #[test]
    fn link_handler_requires_action_or_command() {
        let err = Manifest::parse(
            r#"
id = "a.b"
name = "N"
version = "1"
[[link_handlers]]
pattern = "foo://*"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("action"));
    }

    #[test]
    fn link_handler_with_command_ok() {
        let m = Manifest::parse(
            r#"
id = "a.b"
name = "N"
version = "1"
[[link_handlers]]
pattern = "foo://*"
command = ["herdr-pool", "open"]
"#,
        )
        .unwrap();
        assert_eq!(
            m.link_handlers[0].command.as_ref().unwrap()[0],
            "herdr-pool"
        );
    }
}
