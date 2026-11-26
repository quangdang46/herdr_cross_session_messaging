//! Pool-wide user configuration.
//!
//! Settings live in `config.toml` inside the config dir resolved by
//! [`crate::paths::config_dir`] (`~/.config/herdr-cross-session-messaging/`):
//!
//! ```toml
//! [bootstrap]
//! enabled = false        # stop auto-sending the [herdr-pool] bootstrap prompt
//!
//! [panel]
//! autostart = true       # dock the inbox panel on new tabs/workspaces
//! width = 36             # preferred panel width in terminal columns
//! ```
//!
//! `HERDR_POOL_*` environment variables override the file, one per setting:
//! `HERDR_POOL_BOOTSTRAP`, `HERDR_POOL_PANEL_AUTOSTART`,
//! `HERDR_POOL_PANEL_WIDTH`. A missing or malformed file falls back to
//! defaults — configuration must never keep the pool from working.

use std::path::Path;

use serde::Deserialize;

use crate::paths::{config_dir, EnvLookup};

/// Panel width bounds (terminal columns), mirroring the dock-share clamps
/// used when computing the split ratio.
pub const PANEL_WIDTH_DEFAULT: u16 = 34;
pub const PANEL_WIDTH_MIN: u16 = 24;
pub const PANEL_WIDTH_MAX: u16 = 80;

/// Resolved pool configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolConfig {
    /// Auto-send the `[herdr-pool] bootstrap` instruction prompt to newly
    /// detected agents. Default `true`.
    pub bootstrap_enabled: bool,
    /// Auto-dock the messaging panel on `tab.created`/`workspace.created`
    /// (the `panel ensure` event hook no-ops while this is off). Default
    /// `false`.
    pub panel_autostart: bool,
    /// Preferred panel width in terminal columns. Default
    /// [`PANEL_WIDTH_DEFAULT`], clamped to `PANEL_WIDTH_MIN..=PANEL_WIDTH_MAX`.
    pub panel_width: u16,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            bootstrap_enabled: true,
            panel_autostart: false,
            panel_width: PANEL_WIDTH_DEFAULT,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    bootstrap: Option<BootstrapSection>,
    panel: Option<PanelSection>,
}

#[derive(Debug, Deserialize)]
struct BootstrapSection {
    enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct PanelSection {
    autostart: Option<bool>,
    width: Option<u16>,
}

impl PoolConfig {
    /// Load from the real process environment and config dir.
    pub fn load() -> Self {
        Self::from_parts(config_dir().ok().as_deref(), &|key| std::env::var_os(key))
    }

    /// Resolve from an explicit config dir + env lookup (pure; unit tests).
    ///
    /// `config_dir` is the directory containing `config.toml`; `None` or an
    /// unreadable/malformed file contributes nothing.
    pub fn from_parts(config_dir: Option<&Path>, env: &EnvLookup<'_>) -> Self {
        let mut config = Self::default();

        if let Some(dir) = config_dir {
            let path = dir.join("config.toml");
            match std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|text| toml::from_str::<FileConfig>(&text).map_err(|e| e.to_string()))
            {
                Ok(file) => {
                    if let Some(enabled) = file.bootstrap.and_then(|b| b.enabled) {
                        config.bootstrap_enabled = enabled;
                    }
                    if let Some(panel) = file.panel {
                        if let Some(autostart) = panel.autostart {
                            config.panel_autostart = autostart;
                        }
                        if let Some(width) = panel.width {
                            config.panel_width = width.clamp(PANEL_WIDTH_MIN, PANEL_WIDTH_MAX);
                        }
                    }
                }
                Err(_) => {
                    // Missing file is the normal case; a malformed one is
                    // logged once here rather than failing every command.
                    tracing::debug!(path = %path.display(), "pool config.toml unreadable; using defaults");
                }
            }
        }

        if let Some(v) = env_bool(env, "HERDR_POOL_BOOTSTRAP") {
            config.bootstrap_enabled = v;
        }
        if let Some(v) = env_bool(env, "HERDR_POOL_PANEL_AUTOSTART") {
            config.panel_autostart = v;
        }
        if let Some(width) = env("HERDR_POOL_PANEL_WIDTH")
            .and_then(|v| v.to_string_lossy().trim().parse::<u16>().ok())
        {
            config.panel_width = width.clamp(PANEL_WIDTH_MIN, PANEL_WIDTH_MAX);
        }
        config
    }
}

/// Parse a boolean env var: `1|true|yes|on` → true, `0|false|no|off` →
/// false, anything else → `None` (ignored).
fn env_bool(env: &EnvLookup<'_>, var: &str) -> Option<bool> {
    let value = env(var)?;
    match value.to_string_lossy().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::OsString;

    fn env<'a>(map: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        let owned: HashMap<String, OsString> = map
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        move |key| owned.get(key).cloned()
    }

    #[test]
    fn defaults_when_nothing_configured() {
        let e = env(&[]);
        assert_eq!(PoolConfig::from_parts(None, &e), PoolConfig::default());
    }

    #[test]
    fn file_values_apply() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[bootstrap]\nenabled = false\n\n[panel]\nautostart = true\nwidth = 48\n",
        )
        .unwrap();
        let e = env(&[]);
        let cfg = PoolConfig::from_parts(Some(dir.path()), &e);
        assert!(!cfg.bootstrap_enabled);
        assert!(cfg.panel_autostart);
        assert_eq!(cfg.panel_width, 48);
    }

    #[test]
    fn env_overrides_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[bootstrap]\nenabled = false\n[panel]\nwidth = 48\n",
        )
        .unwrap();
        let e = env(&[
            ("HERDR_POOL_BOOTSTRAP", "1"),
            ("HERDR_POOL_PANEL_WIDTH", "30"),
        ]);
        let cfg = PoolConfig::from_parts(Some(dir.path()), &e);
        assert!(cfg.bootstrap_enabled);
        assert_eq!(cfg.panel_width, 30);
    }

    #[test]
    fn malformed_file_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "not = [valid").unwrap();
        let e = env(&[]);
        assert_eq!(
            PoolConfig::from_parts(Some(dir.path()), &e),
            PoolConfig::default()
        );
    }

    #[test]
    fn width_is_clamped() {
        let e = env(&[("HERDR_POOL_PANEL_WIDTH", "500")]);
        assert_eq!(
            PoolConfig::from_parts(None, &e).panel_width,
            PANEL_WIDTH_MAX
        );
        let e = env(&[("HERDR_POOL_PANEL_WIDTH", "1")]);
        assert_eq!(
            PoolConfig::from_parts(None, &e).panel_width,
            PANEL_WIDTH_MIN
        );
    }

    #[test]
    fn unknown_bool_values_are_ignored() {
        let e = env(&[("HERDR_POOL_BOOTSTRAP", "maybe")]);
        assert!(PoolConfig::from_parts(None, &e).bootstrap_enabled);
    }
}
