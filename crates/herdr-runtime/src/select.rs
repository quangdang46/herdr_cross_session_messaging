//! Backend selection: `HERDR_POOL_RUNTIME=herdr|hc` (default `herdr`).
//!
//! One env var picks which [`AgentRuntime`] backend every pool component
//! (CLI context, router daemon, `tick`) talks to, so herdr and hc can
//! coexist during the migration (RFC `docs/hc-migration/00-rfc.md` §3).
//! [`runtime_kind_from_env`] parses the flag once; [`connect_runtime`]
//! builds the matching runtime eagerly — the daemon is long-lived, so a
//! blocking `hc connect` at startup is fine (the CLI keeps its own lazy
//! factory in `herdr-pool-cli`).
//!
//! CLI and daemon MUST agree on the backend within a scope: the daemon
//! stamps the kind into `<state>/runtime` at startup
//! ([`crate::herdr_router`](../herdr_router) owns the file helpers) and
//! the CLI warns on mismatch.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use herdr_protocol::HerdrClient;

use crate::error::RuntimeError;
use crate::types::EnvLookup;
use crate::{AgentRuntime, HcRuntime, HerdrRuntime};

/// Env var selecting the agent runtime backend (`herdr` default, `hc`
/// opt-in). Shared by the CLI and `herdr-routerd`.
pub const RUNTIME_ENV_VAR: &str = "HERDR_POOL_RUNTIME";
/// Optional override for the `hc connect` argv (JSON-RPC over stdio) —
/// e.g. `HC_CONNECT_CMD="ssh host hc connect"` for a remote core.
pub const HC_CONNECT_CMD_ENV_VAR: &str = "HC_CONNECT_CMD";

/// Which agent runtime backend pool components talk to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeKind {
    /// Herdr socket API (`HERDR_*` pane env, `HERDR_PANE_ID` handles).
    Herdr,
    /// hc terminal runtime (`HC_TERMINAL_ID`, `t7`/`machine/t7` handles).
    Hc,
}

impl RuntimeKind {
    /// Stable name for env propagation, stamps, and diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Herdr => "herdr",
            Self::Hc => "hc",
        }
    }
}

/// Parse [`RUNTIME_ENV_VAR`] from `env`. Missing/empty → [`RuntimeKind::Herdr`];
/// `herdr`/`hc` case-insensitive; anything else → `Err` with a usage-ready
/// message (`invalid HERDR_POOL_RUNTIME '<v>' (expected herdr|hc)`).
pub fn runtime_kind_from_env(env: &EnvLookup<'_>) -> Result<RuntimeKind, String> {
    let Some(value) = env(RUNTIME_ENV_VAR).filter(|v| !v.is_empty()) else {
        return Ok(RuntimeKind::Herdr);
    };
    let value = value.to_string_lossy();
    if value.eq_ignore_ascii_case("herdr") {
        Ok(RuntimeKind::Herdr)
    } else if value.eq_ignore_ascii_case("hc") {
        Ok(RuntimeKind::Hc)
    } else {
        Err(format!(
            "invalid {RUNTIME_ENV_VAR} '{value}' (expected herdr|hc)"
        ))
    }
}

/// The command that speaks hc JSON-RPC on stdio: `HC_CONNECT_CMD` (run via
/// `sh -c`, so remote cores work: `ssh host hc connect`) or the default
/// `hc connect --json`.
pub fn hc_connect_command(env: &EnvLookup<'_>) -> Command {
    if let Some(connect) = env(HC_CONNECT_CMD_ENV_VAR).filter(|v| !v.is_empty()) {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(connect);
        return cmd;
    }
    let mut cmd = Command::new("hc");
    cmd.args(["connect", "--json"]);
    cmd
}

/// A connected runtime plus the raw herdr client when the backend is herdr.
///
/// `herdr_client` stays `Some` only for [`RuntimeKind::Herdr`]: pool code
/// that still needs herdr-only RPCs (pane labels, `pane.process_info`,
/// rescue read sources) uses it as the documented escape hatch.
pub struct ConnectedRuntime {
    /// The selected backend, ready for trait calls.
    pub runtime: Arc<dyn AgentRuntime>,
    /// Raw [`HerdrClient`] on the herdr backend; `None` on hc.
    pub herdr_client: Option<HerdrClient>,
}

/// Eagerly connect the backend selected by `kind`.
///
/// * `Herdr` — resolves the socket (`socket_path` override or
///   `HERDR_SOCKET_PATH`/XDG env) and wraps it in [`HerdrRuntime`].
/// * `Hc` — spawns `hc connect --json` (or [`HC_CONNECT_CMD_ENV_VAR`]) and
///   performs the `hello` handshake.
pub fn connect_runtime(
    kind: RuntimeKind,
    socket_path: Option<&Path>,
    env: &EnvLookup<'_>,
) -> Result<ConnectedRuntime, RuntimeError> {
    match kind {
        RuntimeKind::Herdr => {
            let client = match socket_path {
                Some(path) => HerdrClient::from_socket_path(path),
                None => HerdrClient::new()?,
            };
            Ok(ConnectedRuntime {
                runtime: Arc::new(HerdrRuntime::new(client.clone())),
                herdr_client: Some(client),
            })
        }
        RuntimeKind::Hc => {
            let runtime = HcRuntime::connect(hc_connect_command(env))?;
            Ok(ConnectedRuntime {
                runtime: Arc::new(runtime),
                herdr_client: None,
            })
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::OsString;

    use super::*;

    fn env_from<'a>(map: &'a HashMap<String, OsString>) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |key| map.get(key).cloned()
    }

    #[test]
    fn runtime_kind_defaults_to_herdr() {
        let map = HashMap::new();
        assert_eq!(
            runtime_kind_from_env(&env_from(&map)).unwrap(),
            RuntimeKind::Herdr
        );
    }

    #[test]
    fn runtime_kind_parses_both_values_case_insensitive() {
        for (raw, want) in [
            ("herdr", RuntimeKind::Herdr),
            ("HERDR", RuntimeKind::Herdr),
            ("hc", RuntimeKind::Hc),
            ("HC", RuntimeKind::Hc),
        ] {
            let mut map = HashMap::new();
            map.insert(RUNTIME_ENV_VAR.to_string(), OsString::from(raw));
            assert_eq!(runtime_kind_from_env(&env_from(&map)).unwrap(), want);
        }
    }

    #[test]
    fn runtime_kind_rejects_unknown_value() {
        let mut map = HashMap::new();
        map.insert(RUNTIME_ENV_VAR.to_string(), OsString::from("bogus"));
        let err = runtime_kind_from_env(&env_from(&map)).unwrap_err();
        assert!(err.contains("invalid HERDR_POOL_RUNTIME"));
        assert!(err.contains("herdr|hc"));
    }

    #[test]
    fn runtime_kind_ignores_empty_value() {
        let mut map = HashMap::new();
        map.insert(RUNTIME_ENV_VAR.to_string(), OsString::new());
        assert_eq!(
            runtime_kind_from_env(&env_from(&map)).unwrap(),
            RuntimeKind::Herdr
        );
    }

    #[test]
    fn hc_connect_command_defaults_to_hc_connect_json() {
        let map = HashMap::new();
        let cmd = hc_connect_command(&env_from(&map));
        assert_eq!(cmd.get_program(), "hc");
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, ["connect", "--json"]);
    }

    #[test]
    fn hc_connect_command_honors_override_via_sh_c() {
        let mut map = HashMap::new();
        map.insert(
            HC_CONNECT_CMD_ENV_VAR.to_string(),
            OsString::from("ssh host hc connect"),
        );
        let cmd = hc_connect_command(&env_from(&map));
        assert_eq!(cmd.get_program(), "sh");
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, ["-c", "ssh host hc connect"]);
    }
}
