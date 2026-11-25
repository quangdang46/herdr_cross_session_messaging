//! State/config directory resolution.
//!
//! Single source of truth for where the plugin keeps state (pool databases,
//! pidfiles, daemon locks) and config. Resolution is pure: callers inject an
//! env lookup plus an optional passwd-style home fallback, and the thin
//! [`state_dir`]/[`config_dir`] wrappers read the real process environment.
//!
//! State dir resolution order:
//! 1. `HERDR_PLUGIN_STATE_DIR` (injected by Herdr for plugin commands)
//! 2. `HERDR_POOL_STATE_DIR` (explicit user override)
//! 3. `$XDG_STATE_HOME/herdr-cross-session-messaging`
//! 4. `$HOME/.local/state/herdr-cross-session-messaging`
//! 5. `<home>/.herdr-cross-session-messaging` when `$HOME` is unset but a home
//!    directory is known (passwd fallback) — the "no home env" fallback.
//!
//! Config dir resolution mirrors this with `HERDR_PLUGIN_CONFIG_DIR`,
//! `HERDR_POOL_CONFIG_DIR`, `$XDG_CONFIG_HOME`, `$HOME/.config`, and
//! `<home>/.config` respectively.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::{PluginError, Result};

/// Directory name used under XDG/home state and config roots.
pub const APP_DIR_NAME: &str = "herdr-cross-session-messaging";

/// Env vars consulted during resolution, in priority order.
pub const ENV_PLUGIN_STATE_DIR: &str = "HERDR_PLUGIN_STATE_DIR";
pub const ENV_POOL_STATE_DIR: &str = "HERDR_POOL_STATE_DIR";
pub const ENV_PLUGIN_CONFIG_DIR: &str = "HERDR_PLUGIN_CONFIG_DIR";
pub const ENV_POOL_CONFIG_DIR: &str = "HERDR_POOL_CONFIG_DIR";
pub const ENV_XDG_STATE_HOME: &str = "XDG_STATE_HOME";
pub const ENV_XDG_CONFIG_HOME: &str = "XDG_CONFIG_HOME";
pub const ENV_HOME: &str = "HOME";
pub const ENV_USERPROFILE: &str = "USERPROFILE";

/// Env lookup used by the pure resolvers. Maps a variable name to its value.
pub type EnvLookup<'a> = dyn Fn(&str) -> Option<OsString> + 'a;

/// Resolve the plugin state directory from an injected environment.
///
/// `home_fallback` is a home directory obtained without consulting `HOME`
/// (e.g. `std::env::home_dir()`); it is only consulted when `HOME`/`USERPROFILE`
/// are absent, producing the `~/.herdr-cross-session-messaging` fallback.
pub fn state_dir_from(env: &EnvLookup<'_>, home_fallback: Option<&Path>) -> Result<PathBuf> {
    resolve(
        env,
        home_fallback,
        &[ENV_PLUGIN_STATE_DIR, ENV_POOL_STATE_DIR],
        ENV_XDG_STATE_HOME,
        XdgKind::State,
    )
}

/// Resolve the plugin config directory from an injected environment.
pub fn config_dir_from(env: &EnvLookup<'_>, home_fallback: Option<&Path>) -> Result<PathBuf> {
    resolve(
        env,
        home_fallback,
        &[ENV_PLUGIN_CONFIG_DIR, ENV_POOL_CONFIG_DIR],
        ENV_XDG_CONFIG_HOME,
        XdgKind::Config,
    )
}

/// Thin wrapper: resolve the state dir from the real process environment.
pub fn state_dir() -> Result<PathBuf> {
    state_dir_from(
        &|key| std::env::var_os(key),
        std::env::home_dir().as_deref(),
    )
}

/// Thin wrapper: resolve the config dir from the real process environment.
pub fn config_dir() -> Result<PathBuf> {
    config_dir_from(
        &|key| std::env::var_os(key),
        std::env::home_dir().as_deref(),
    )
}

/// Create the state directory (and parents) if needed; returns the path.
pub fn ensure_state_dir() -> Result<PathBuf> {
    let dir = state_dir()?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[derive(Clone, Copy)]
enum XdgKind {
    State,
    Config,
}

impl XdgKind {
    /// XDG-relative directory for the platform default when no XDG var is set.
    fn default_rel(self) -> &'static str {
        match self {
            XdgKind::State => ".local/state",
            XdgKind::Config => ".config",
        }
    }

    /// Fallback directory name inside home when only `home_fallback` is known.
    fn fallback_rel(self) -> PathBuf {
        match self {
            XdgKind::State => PathBuf::from(format!(".{APP_DIR_NAME}")),
            XdgKind::Config => PathBuf::from(".config").join(APP_DIR_NAME),
        }
    }
}

fn resolve(
    env: &EnvLookup<'_>,
    home_fallback: Option<&Path>,
    override_vars: &[&str],
    xdg_var: &str,
    kind: XdgKind,
) -> Result<PathBuf> {
    // 1-2. Explicit overrides (plugin-injected, then pool-level).
    for var in override_vars {
        if let Some(path) = env_path(env, var) {
            return Ok(path);
        }
    }

    // 3. XDG base directory.
    if let Some(base) = env_path(env, xdg_var) {
        return Ok(base.join(APP_DIR_NAME));
    }

    // 4. $HOME / $USERPROFILE conventional location.
    if let Some(home) = env_home(env) {
        return Ok(home.join(kind.default_rel()).join(APP_DIR_NAME));
    }

    // 5. No HOME env: fall back to a passwd-derived home with a hidden dir.
    if let Some(home) = home_fallback {
        return Ok(home.join(kind.fallback_rel()));
    }

    Err(PluginError::MissingEnv(ENV_HOME))
}

/// Read an env var as a path, treating unset/empty as absent.
fn env_path(env: &EnvLookup<'_>, var: &str) -> Option<PathBuf> {
    env(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Home directory from `HOME`, then `USERPROFILE` (Windows).
fn env_home(env: &EnvLookup<'_>) -> Option<PathBuf> {
    env_path(env, ENV_HOME).or_else(|| env_path(env, ENV_USERPROFILE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env<'a>(map: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        let owned: HashMap<String, OsString> = map
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        move |key| owned.get(key).cloned()
    }

    #[test]
    fn plugin_state_dir_env_wins() {
        let e = env(&[
            ("HERDR_PLUGIN_STATE_DIR", "/p/state"),
            ("HERDR_POOL_STATE_DIR", "/pool/state"),
            ("XDG_STATE_HOME", "/xdg"),
            ("HOME", "/home/u"),
        ]);
        assert_eq!(state_dir_from(&e, None).unwrap(), PathBuf::from("/p/state"));
    }

    #[test]
    fn pool_state_dir_env_second() {
        let e = env(&[
            ("HERDR_POOL_STATE_DIR", "/pool/state"),
            ("XDG_STATE_HOME", "/xdg"),
            ("HOME", "/home/u"),
        ]);
        assert_eq!(
            state_dir_from(&e, None).unwrap(),
            PathBuf::from("/pool/state")
        );
    }

    #[test]
    fn xdg_state_home_third() {
        let e = env(&[("XDG_STATE_HOME", "/xdg"), ("HOME", "/home/u")]);
        assert_eq!(
            state_dir_from(&e, None).unwrap(),
            PathBuf::from("/xdg/herdr-cross-session-messaging")
        );
    }

    #[test]
    fn home_local_state_fourth() {
        let e = env(&[("HOME", "/home/u")]);
        assert_eq!(
            state_dir_from(&e, None).unwrap(),
            PathBuf::from("/home/u/.local/state/herdr-cross-session-messaging")
        );
    }

    #[test]
    fn no_home_env_uses_fallback_hidden_dir() {
        let e = env(&[]);
        let home = Path::new("/passwd/home/u");
        assert_eq!(
            state_dir_from(&e, Some(home)).unwrap(),
            PathBuf::from("/passwd/home/u/.herdr-cross-session-messaging")
        );
    }

    #[test]
    fn no_home_at_all_errors() {
        let e = env(&[]);
        assert!(matches!(
            state_dir_from(&e, None),
            Err(PluginError::MissingEnv("HOME"))
        ));
    }

    #[test]
    fn empty_env_values_are_ignored() {
        let e = env(&[
            ("HERDR_PLUGIN_STATE_DIR", ""),
            ("XDG_STATE_HOME", ""),
            ("HOME", "/home/u"),
        ]);
        assert_eq!(
            state_dir_from(&e, None).unwrap(),
            PathBuf::from("/home/u/.local/state/herdr-cross-session-messaging")
        );
    }

    #[test]
    fn config_dir_chain() {
        let e = env(&[
            ("HERDR_PLUGIN_CONFIG_DIR", "/p/cfg"),
            ("XDG_CONFIG_HOME", "/xdg"),
            ("HOME", "/home/u"),
        ]);
        assert_eq!(config_dir_from(&e, None).unwrap(), PathBuf::from("/p/cfg"));

        let e = env(&[("XDG_CONFIG_HOME", "/xdg"), ("HOME", "/home/u")]);
        assert_eq!(
            config_dir_from(&e, None).unwrap(),
            PathBuf::from("/xdg/herdr-cross-session-messaging")
        );

        let e = env(&[("HOME", "/home/u")]);
        assert_eq!(
            config_dir_from(&e, None).unwrap(),
            PathBuf::from("/home/u/.config/herdr-cross-session-messaging")
        );

        let e = env(&[]);
        let home = Path::new("/passwd/home/u");
        assert_eq!(
            config_dir_from(&e, Some(home)).unwrap(),
            PathBuf::from("/passwd/home/u/.config/herdr-cross-session-messaging")
        );
    }
}
