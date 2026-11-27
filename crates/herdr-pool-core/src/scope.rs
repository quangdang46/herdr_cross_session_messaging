//! Project scope resolution.
//!
//! A **scope** is the canonical launch cwd of a Herdr pane, hashed into a
//! [`ScopeKey`]. All pool data lives under `<state>/pools/<scope_key>/`, so
//! the scope key is the *only* path-derived value that may appear in
//! filenames or database rows — raw paths are stored only inside
//! `index.db`'s `scopes` table for human inspection.
//!
//! ## Which cwd field is authoritative
//!
//! Herdr reports two directory fields on `AgentInfo`/`PaneInfo`
//! (see the socket API docs):
//!
//! - `cwd` — "the pane/workspace cwd used for labels and follow-cwd
//!   behavior", i.e. **where the pane was launched**. This is stable for the
//!   lifetime of the pane and is the field the MVP scoping rule
//!   (research/beads-import.md §M2, plan §3) designates: *scope = the pane's
//!   launch cwd, NOT the agent process's transient subdirectory cwd*.
//! - `foreground_cwd` — the OS cwd of the process currently controlling the
//!   pane PTY. It tracks live `chdir`s and agent subprocesses, so it can
//!   drift away from the launch directory. It is used only as a fallback
//!   when `cwd` is absent.
//!
//! When both record fields are missing we fall back to
//! [`PaneProcessInfo::foreground_processes`] (the first foreground process
//! reporting a cwd), and finally to [`CoreError::ScopeUnresolved`]. We
//! deliberately do **not** guess from `HERDR_*` env vars: those identify
//! the pane, not a directory.
//!
//! Consequences (covered by tests):
//! - Two panes launched in the same directory resolve to the same scope,
//!   regardless of which Herdr workspace/tab hosts them.
//! - Git worktrees are different directories, so they get different scopes.
//! - An agent that `cd`s into a subdirectory keeps its pane's launch-cwd
//!   scope as long as `cwd` is populated.

use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use herdr_protocol::{AgentInfo, PaneInfo, PaneProcessInfo};
use herdr_runtime::RuntimeAgent;
use sha2::{Digest, Sha256};

use crate::error::{CoreError, Result};

/// Number of sha256 digest bytes encoded into a [`ScopeKey`].
///
/// 16 bytes → 32 lowercase hex chars. Truncation is safe here: scope keys
/// are derived from a small set of local project paths (birthday-bound
/// collision risk is negligible at this cardinality) and the full canonical
/// path remains recorded in `index.db` for audit.
const SCOPE_KEY_DIGEST_BYTES: usize = 16;

/// Hashed project scope identifier.
///
/// Wraps the 32-char lowercase hex string. Used as the `pools/<scope_key>/`
/// directory name and as the `scope_key` column in every pool table, so it
/// must never contain path separators or raw path text.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ScopeKey(String);

impl ScopeKey {
    /// The hex string form (also usable directly as a path component).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the inner string.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for ScopeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<ScopeKey> for String {
    fn from(key: ScopeKey) -> Self {
        key.0
    }
}

impl AsRef<str> for ScopeKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// A resolved project scope: canonical directory plus its key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedScope {
    /// Canonical (symlink-free, `.`/`..`-free) absolute project path.
    pub canonical_path: PathBuf,
    /// `sha256(canonical_path)[..16]` as hex — safe for filenames/db rows.
    pub scope_key: ScopeKey,
}

/// Canonicalize `path` for scope derivation.
///
/// Primary path is [`std::fs::canonicalize`], which resolves symlinks and
/// requires the path to exist. When canonicalization fails (nonexistent
/// path, permission error, dangling symlink), falls back to a purely
/// lexical normalization: `.`/`..` components are folded without touching
/// the filesystem, and relative paths are anchored at the process cwd so
/// the result is still absolute and deterministic.
///
/// The fallback exists so scope resolution degrades gracefully for panes
/// whose recorded cwd was deleted after launch; the resulting key is still
/// stable for a given spelling of the path.
pub fn canonicalize_path(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    match std::fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(_) => {
            let normalized = lexical_normalize(path);
            if normalized.is_absolute() {
                normalized
            } else {
                // Anchor relative paths so equivalent spellings from
                // different cwds cannot collide on a bare relative string.
                match std::env::current_dir() {
                    Ok(cwd) => lexical_normalize(&cwd.join(&normalized)),
                    Err(_) => normalized,
                }
            }
        }
    }
}

/// Lexically fold `.` and `..` components without filesystem access.
///
/// `..` at the root (or above a relative path's start) is preserved as a
/// literal `..` component only when the path is relative and nothing
/// precedes it; at filesystem root it is a no-op.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !out.has_root() {
                    // Relative path with more `..` than preceding components:
                    // keep the leading `..` so `../a` stays `../a`.
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Derive the [`ScopeKey`] for an already-canonical path.
///
/// `canonical_path` should come from [`canonicalize_path`]; this function
/// does not re-canonicalize (callers that only have a raw path should use
/// [`resolve_scope`] or [`PoolContext::open`](crate::pool::PoolContext::open)
/// which canonicalize first). The key is the lowercase hex encoding of the
/// first [`SCOPE_KEY_DIGEST_BYTES`] bytes of the SHA-256 digest of the
/// path's raw bytes — 32 hex chars, documented above.
pub fn scope_key(canonical_path: impl AsRef<Path>) -> ScopeKey {
    let bytes = canonical_path.as_ref().as_os_str().as_encoded_bytes();
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(SCOPE_KEY_DIGEST_BYTES * 2);
    for byte in &digest[..SCOPE_KEY_DIGEST_BYTES] {
        // `write!` into a `String` is infallible; the `fmt::Result` is
        // intentionally discarded rather than unwrapped.
        let _ = write!(hex, "{byte:02x}");
    }
    ScopeKey(hex)
}

/// How a candidate cwd maps to the scope root.
///
/// * [`ScopeMode::LaunchCwd`] — the pane's launch directory itself (default;
///   the historical MVP rule).
/// * [`ScopeMode::GitRoot`] — the nearest ancestor directory containing a
///   `.git` entry (directory *or* file, so worktrees and submodules count),
///   falling back to the launch directory when no marker exists. This lets
///   agents launched in different subdirectories of one repository share a
///   pool while unrelated folders stay isolated.
///
/// Selected once per process via `HERDR_POOL_SCOPE_MODE` (`cwd` | `git`;
/// anything else falls back to the default). The default is `cwd` because
/// git-root promotion has a footgun: if `$HOME` itself sits inside a git
/// repo (e.g. a dotfiles repo), every non-repo folder under home would
/// collapse into a single scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeMode {
    /// Scope = canonical launch cwd.
    LaunchCwd,
    /// Scope = nearest `.git` ancestor of the launch cwd (or cwd itself).
    GitRoot,
}

/// Env var selecting [`ScopeMode`].
pub const SCOPE_MODE_ENV: &str = "HERDR_POOL_SCOPE_MODE";

/// The process-wide scope mode, resolved lazily from the environment.
pub fn scope_mode() -> ScopeMode {
    static MODE: OnceLock<ScopeMode> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var(SCOPE_MODE_ENV) {
        Ok(v) if v.eq_ignore_ascii_case("git") => ScopeMode::GitRoot,
        _ => ScopeMode::LaunchCwd,
    })
}

/// Resolve the scope using the process-wide [`scope_mode`].
///
/// Candidate order (first non-empty wins):
/// 1. `cwd` — pane launch cwd (authoritative, see module docs).
/// 2. `foreground_cwd` — live foreground-process cwd (fallback only).
/// 3. `process_info.foreground_processes[*].cwd` — first reported cwd.
///
/// Each candidate is canonicalized via [`canonicalize_path`] before keying.
/// Empty/whitespace strings are ignored. Returns
/// [`CoreError::ScopeUnresolved`] when no candidate yields a path.
pub fn resolve_scope(
    cwd: Option<&str>,
    foreground_cwd: Option<&str>,
    process_info: Option<&PaneProcessInfo>,
) -> Result<ResolvedScope> {
    resolve_scope_with_mode(cwd, foreground_cwd, process_info, scope_mode())
}

/// Resolve the scope for an explicit [`ScopeMode`] (testable variant of
/// [`resolve_scope`]).
pub fn resolve_scope_with_mode(
    cwd: Option<&str>,
    foreground_cwd: Option<&str>,
    process_info: Option<&PaneProcessInfo>,
    mode: ScopeMode,
) -> Result<ResolvedScope> {
    resolve_scope_iter(
        [cwd, foreground_cwd]
            .into_iter()
            .flatten()
            .chain(process_info_cwds(process_info)),
        mode,
    )
}

/// Resolve the scope for a runtime-normalized [`RuntimeAgent`]
/// ([`herdr_runtime::AgentRuntime`] backends: herdr `AgentInfo`/`PaneInfo`
/// or hc `Terminal` records mapped into one shape).
///
/// Candidate order matches [`resolve_scope`]: `cwd` (launch directory,
/// authoritative) → `foreground_cwd` → `extra_cwds`. `extra_cwds` carries
/// backend-specific fallbacks such as herdr `pane.process_info` foreground
/// cwds (`AgentRuntime::process_info_cwds`); hc has no process table and
/// passes an empty slice.
pub fn resolve_scope_for_runtime(
    agent: &RuntimeAgent,
    extra_cwds: &[String],
) -> Result<ResolvedScope> {
    resolve_scope_for_runtime_with_mode(agent, extra_cwds, scope_mode())
}

/// [`resolve_scope_for_runtime`] with an explicit [`ScopeMode`]
/// (testable variant).
pub fn resolve_scope_for_runtime_with_mode(
    agent: &RuntimeAgent,
    extra_cwds: &[String],
    mode: ScopeMode,
) -> Result<ResolvedScope> {
    resolve_scope_iter(
        [agent.cwd.as_deref(), agent.foreground_cwd.as_deref()]
            .into_iter()
            .flatten()
            .chain(extra_cwds.iter().map(String::as_str)),
        mode,
    )
}

/// Shared candidate walk: first non-empty cwd wins, canonicalized and
/// keyed under `mode`.
fn resolve_scope_iter<'a>(
    candidates: impl Iterator<Item = &'a str>,
    mode: ScopeMode,
) -> Result<ResolvedScope> {
    for candidate in candidates {
        let trimmed = candidate.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut canonical_path = canonicalize_path(trimmed);
        if mode == ScopeMode::GitRoot {
            if let Some(root) = git_toplevel(&canonical_path) {
                canonical_path = root;
            }
        }
        let key = scope_key(&canonical_path);
        return Ok(ResolvedScope {
            canonical_path,
            scope_key: key,
        });
    }

    Err(CoreError::ScopeUnresolved(
        "no cwd on agent/pane record or process info".to_string(),
    ))
}

/// Nearest ancestor of `dir` (including `dir` itself) containing a `.git`
/// entry — a directory for normal repos, a file for worktrees/submodules.
/// Returns `None` outside any git work tree.
fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Foreground-process cwds from a [`PaneProcessInfo`], in reported order.
fn process_info_cwds<'a>(
    process_info: Option<&'a PaneProcessInfo>,
) -> impl Iterator<Item = &'a str> + 'a {
    process_info
        .into_iter()
        .flat_map(|info| info.foreground_processes.iter())
        .filter_map(|proc| proc.cwd.as_deref())
}

/// Resolve the scope for an [`AgentInfo`] record.
///
/// `process_info` should be the [`PaneProcessInfo`] for the agent's pane
/// (`client.pane_process_info(Some(pane_id))`); pass `None` when it is
/// unavailable or unnecessary.
pub fn resolve_scope_for_agent(
    agent: &AgentInfo,
    process_info: Option<&PaneProcessInfo>,
) -> Result<ResolvedScope> {
    resolve_scope(
        agent.cwd.as_deref(),
        agent.foreground_cwd.as_deref(),
        process_info,
    )
}

/// Resolve the scope for a [`PaneInfo`] record.
pub fn resolve_scope_for_pane(
    pane: &PaneInfo,
    process_info: Option<&PaneProcessInfo>,
) -> Result<ResolvedScope> {
    resolve_scope(
        pane.cwd.as_deref(),
        pane.foreground_cwd.as_deref(),
        process_info,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use herdr_protocol::{AgentStatus, MetadataMap};

    // ----- fixtures --------------------------------------------------------

    fn agent_info(cwd: Option<&str>, foreground_cwd: Option<&str>) -> AgentInfo {
        AgentInfo {
            pane_id: "w1:p1".into(),
            workspace_id: "w1".into(),
            tab_id: "w1:t1".into(),
            terminal_id: "term-1".into(),
            agent_status: AgentStatus::Working,
            revision: 1,
            focused: true,
            agent: Some("claude".into()),
            agent_session: None,
            name: Some("builder".into()),
            cwd: cwd.map(str::to_string),
            foreground_cwd: foreground_cwd.map(str::to_string),
            display_agent: None,
            interactive_ready: true,
            launch_pending: false,
            screen_detection_skipped: false,
            state_change_seq: 0,
            completion_seq: None,
            terminal_title: None,
            terminal_title_stripped: None,
            title: None,
            tokens: MetadataMap::new(),
            state_labels: MetadataMap::new(),
        }
    }

    fn pane_info(cwd: Option<&str>, foreground_cwd: Option<&str>) -> PaneInfo {
        PaneInfo {
            pane_id: "w1:p1".into(),
            workspace_id: "w1".into(),
            tab_id: "w1:t1".into(),
            terminal_id: "term-1".into(),
            agent_status: AgentStatus::Idle,
            revision: 1,
            focused: true,
            agent: None,
            agent_session: None,
            cwd: cwd.map(str::to_string),
            foreground_cwd: foreground_cwd.map(str::to_string),
            display_agent: None,
            label: None,
            restore_error: None,
            scroll: None,
            terminal_title: None,
            terminal_title_stripped: None,
            title: None,
            tokens: MetadataMap::new(),
            state_labels: MetadataMap::new(),
        }
    }

    fn process_info(cwds: &[Option<&str>]) -> PaneProcessInfo {
        PaneProcessInfo {
            pane_id: "w1:p1".into(),
            shell_pid: Some(100),
            foreground_process_group_id: Some(200),
            tty: Some("/dev/ttys001".into()),
            foreground_processes: cwds
                .iter()
                .enumerate()
                .map(|(i, cwd)| herdr_protocol::PaneProcessInfoProcess {
                    pid: 200 + i as u64,
                    name: "proc".into(),
                    argv: None,
                    argv0: None,
                    cmdline: None,
                    cwd: cwd.map(str::to_string),
                })
                .collect(),
        }
    }

    // ----- canonicalize_path ----------------------------------------------

    #[test]
    fn canonicalize_existing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = canonicalize_path(tmp.path());
        // tempdir itself may live under a symlinked /var on macOS; compare
        // against fs::canonicalize directly.
        assert_eq!(canonical, std::fs::canonicalize(tmp.path()).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn canonicalize_resolves_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(
            canonicalize_path(&link),
            std::fs::canonicalize(&real).unwrap()
        );
    }

    #[test]
    fn canonicalize_strips_trailing_slash_and_dot_components() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let spelled = format!("{}/./sub/", tmp.path().display());
        assert_eq!(
            canonicalize_path(&spelled),
            std::fs::canonicalize(&sub).unwrap()
        );
    }

    #[test]
    fn canonicalize_nonexistent_falls_back_to_lexical() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp
            .path()
            .join("does-not-exist")
            .join("..")
            .join("also-missing");
        // `..` folds lexically even though the path does not exist.
        let out = canonicalize_path(&missing);
        assert!(out.is_absolute());
        assert!(!out.to_string_lossy().contains("does-not-exist"));
        assert!(out.ends_with("also-missing"));
    }

    #[test]
    fn lexical_normalize_relative_parent_dir() {
        // Relative fallback keeps leading `..` rather than dropping it.
        let cwd = std::env::current_dir().unwrap();
        let out = canonicalize_path(Path::new("../definitely-not-here-xyz"));
        assert!(out.is_absolute());
        // Anchored at cwd/.. — still absolute and deterministic.
        let _ = cwd;
        assert!(out.ends_with("definitely-not-here-xyz"));
    }

    // ----- scope_key --------------------------------------------------------

    #[test]
    fn scope_key_is_32_lowercase_hex() {
        let key = scope_key("/tmp/some-project");
        assert_eq!(key.as_str().len(), 32);
        assert!(key
            .as_str()
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn scope_key_deterministic_and_sensitive() {
        let a = scope_key("/tmp/proj");
        let b = scope_key("/tmp/proj");
        let c = scope_key("/tmp/proj2");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn same_dir_different_spellings_same_key() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("proj");
        std::fs::create_dir(&sub).unwrap();

        let direct = scope_key(canonicalize_path(&sub));
        let dotted = scope_key(canonicalize_path(format!(
            "{}/./proj/",
            tmp.path().display()
        )));
        assert_eq!(direct, dotted);
    }

    #[test]
    fn different_dirs_different_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        assert_ne!(
            scope_key(canonicalize_path(&a)),
            scope_key(canonicalize_path(&b))
        );
    }

    #[test]
    fn worktrees_get_different_scopes_same_folder_same_scope() {
        // Two "workspaces" pointing at the same canonical dir share a scope;
        // two worktree dirs do not. (Scope ignores workspace/tab identity.)
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        let wt = tmp.path().join("wt-feature");
        std::fs::create_dir_all(&main).unwrap();
        std::fs::create_dir_all(&wt).unwrap();

        let scope_main =
            resolve_scope_for_pane(&pane_info(Some(&main.to_string_lossy()), None), None).unwrap();
        // Same folder via a second pane record (different workspace id is
        // irrelevant — scope only sees cwd).
        let mut pane2 = pane_info(Some(&main.to_string_lossy()), None);
        pane2.workspace_id = "w2".into();
        pane2.tab_id = "w2:t9".into();
        pane2.pane_id = "w2:p1".into();
        let scope_main2 = resolve_scope_for_pane(&pane2, None).unwrap();
        let scope_wt =
            resolve_scope_for_pane(&pane_info(Some(&wt.to_string_lossy()), None), None).unwrap();

        assert_eq!(scope_main.scope_key, scope_main2.scope_key);
        assert_ne!(scope_main.scope_key, scope_wt.scope_key);
    }

    // ----- resolve_scope branch coverage ------------------------------------

    #[test]
    fn resolve_prefers_record_cwd_over_foreground_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let launch = tmp.path().join("launch");
        let transient = tmp.path().join("transient");
        std::fs::create_dir_all(&launch).unwrap();
        std::fs::create_dir_all(&transient).unwrap();

        // cwd = launch dir (stable), foreground_cwd = agent subprocess dir.
        let scope = resolve_scope(
            Some(&launch.to_string_lossy()),
            Some(&transient.to_string_lossy()),
            None,
        )
        .unwrap();
        assert_eq!(
            scope.canonical_path,
            std::fs::canonicalize(&launch).unwrap()
        );
        assert_eq!(
            scope.scope_key,
            scope_key(std::fs::canonicalize(&launch).unwrap())
        );
    }

    #[test]
    fn resolve_falls_back_to_foreground_cwd_when_cwd_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dir");
        std::fs::create_dir(&dir).unwrap();
        let scope = resolve_scope(None, Some(&dir.to_string_lossy()), None).unwrap();
        assert_eq!(scope.canonical_path, std::fs::canonicalize(&dir).unwrap());
    }

    #[test]
    fn resolve_falls_back_to_process_info() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("procdir");
        std::fs::create_dir(&dir).unwrap();

        // First process has no cwd; second does — first reported wins.
        let info = process_info(&[None, Some(&dir.to_string_lossy())]);
        let scope = resolve_scope(None, None, Some(&info)).unwrap();
        assert_eq!(scope.canonical_path, std::fs::canonicalize(&dir).unwrap());
    }

    #[test]
    fn resolve_process_info_first_cwd_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let info = process_info(&[Some(&a.to_string_lossy()), Some(&b.to_string_lossy())]);
        let scope = resolve_scope(None, None, Some(&info)).unwrap();
        assert_eq!(scope.canonical_path, std::fs::canonicalize(&a).unwrap());
    }

    #[test]
    fn resolve_unresolved_when_nothing_reports_cwd() {
        let info = process_info(&[None, Some("")]);
        let err = resolve_scope(None, None, Some(&info)).unwrap_err();
        assert!(matches!(err, CoreError::ScopeUnresolved(_)));
        assert!(resolve_scope(None, None, None).is_err());
    }

    #[test]
    fn resolve_ignores_empty_and_whitespace_cwds() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("real");
        std::fs::create_dir(&dir).unwrap();
        // Whitespace/empty candidates are skipped; foreground_cwd wins over
        // nothing else being present.
        let scope = resolve_scope(Some("   "), Some(&dir.to_string_lossy()), None).unwrap();
        assert_eq!(scope.canonical_path, std::fs::canonicalize(&dir).unwrap());
        // All-empty candidates are unresolved.
        assert!(matches!(
            resolve_scope(Some(""), Some("  "), None),
            Err(CoreError::ScopeUnresolved(_))
        ));
    }

    #[test]
    fn agent_record_uses_same_branch_order() {
        let tmp = tempfile::tempdir().unwrap();
        let launch = tmp.path().join("launch");
        let procdir = tmp.path().join("procdir");
        std::fs::create_dir_all(&launch).unwrap();
        std::fs::create_dir_all(&procdir).unwrap();

        // Agent cwd beats process_info fallback.
        let agent = agent_info(Some(&launch.to_string_lossy()), None);
        let info = process_info(&[Some(&procdir.to_string_lossy())]);
        let scope = resolve_scope_for_agent(&agent, Some(&info)).unwrap();
        assert_eq!(
            scope.canonical_path,
            std::fs::canonicalize(&launch).unwrap()
        );

        // Agent without cwd falls through to process_info.
        let agent = agent_info(None, None);
        let scope = resolve_scope_for_agent(&agent, Some(&info)).unwrap();
        assert_eq!(
            scope.canonical_path,
            std::fs::canonicalize(&procdir).unwrap()
        );

        // Pane record path works identically.
        let pane = pane_info(
            Some(&launch.to_string_lossy()),
            Some(&procdir.to_string_lossy()),
        );
        let scope = resolve_scope_for_pane(&pane, None).unwrap();
        assert_eq!(
            scope.canonical_path,
            std::fs::canonicalize(&launch).unwrap()
        );
    }

    #[test]
    fn scope_key_uses_encoded_path_bytes() {
        // Non-UTF8-ish safety is handled by as_encoded_bytes; smoke-test that
        // two Path spellings of the same file hash identically.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("uni");
        std::fs::create_dir(&dir).unwrap();
        let k1 = scope_key(canonicalize_path(&dir));
        let k2 = scope_key(canonicalize_path(dir.as_os_str()));
        assert_eq!(k1, k2);
    }

    // ----- ScopeMode::GitRoot ----------------------------------------------

    #[test]
    fn git_root_mode_unifies_subdirs_of_one_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let backend = repo.join("backend");
        let frontend = repo.join("frontend");
        std::fs::create_dir_all(&backend).unwrap();
        std::fs::create_dir_all(&frontend).unwrap();
        std::fs::create_dir(repo.join(".git")).unwrap();

        let root_scope = resolve_scope_with_mode(
            Some(&repo.to_string_lossy()),
            None,
            None,
            ScopeMode::GitRoot,
        )
        .unwrap();
        let back_scope = resolve_scope_with_mode(
            Some(&backend.to_string_lossy()),
            None,
            None,
            ScopeMode::GitRoot,
        )
        .unwrap();
        let front_scope = resolve_scope_with_mode(
            Some(&frontend.to_string_lossy()),
            None,
            None,
            ScopeMode::GitRoot,
        )
        .unwrap();

        assert_eq!(root_scope.scope_key, back_scope.scope_key);
        assert_eq!(root_scope.scope_key, front_scope.scope_key);
        assert_eq!(
            back_scope.canonical_path,
            std::fs::canonicalize(&repo).unwrap()
        );
    }

    #[test]
    fn git_root_mode_keeps_unrelated_dirs_separate() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_a = tmp.path().join("a");
        let repo_b = tmp.path().join("b");
        std::fs::create_dir_all(repo_a.join(".git")).unwrap();
        std::fs::create_dir_all(repo_b.join(".git")).unwrap();

        let a = resolve_scope_with_mode(
            Some(&repo_a.to_string_lossy()),
            None,
            None,
            ScopeMode::GitRoot,
        )
        .unwrap();
        let b = resolve_scope_with_mode(
            Some(&repo_b.to_string_lossy()),
            None,
            None,
            ScopeMode::GitRoot,
        )
        .unwrap();
        assert_ne!(a.scope_key, b.scope_key);
    }

    #[test]
    fn git_root_mode_falls_back_to_cwd_outside_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plain");
        std::fs::create_dir(&dir).unwrap();
        let scope =
            resolve_scope_with_mode(Some(&dir.to_string_lossy()), None, None, ScopeMode::GitRoot)
                .unwrap();
        assert_eq!(scope.canonical_path, std::fs::canonicalize(&dir).unwrap());
    }

    #[test]
    fn git_root_mode_accepts_git_file_marker() {
        // Worktrees/submodules carry `.git` as a *file*, not a directory.
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("wt");
        let sub = wt.join("pkg");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: /elsewhere\n").unwrap();

        let scope =
            resolve_scope_with_mode(Some(&sub.to_string_lossy()), None, None, ScopeMode::GitRoot)
                .unwrap();
        assert_eq!(scope.canonical_path, std::fs::canonicalize(&wt).unwrap());
    }

    #[test]
    fn launch_cwd_mode_ignores_git_root() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let sub = repo.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir(repo.join(".git")).unwrap();

        let repo_scope = resolve_scope_with_mode(
            Some(&repo.to_string_lossy()),
            None,
            None,
            ScopeMode::LaunchCwd,
        )
        .unwrap();
        let sub_scope = resolve_scope_with_mode(
            Some(&sub.to_string_lossy()),
            None,
            None,
            ScopeMode::LaunchCwd,
        )
        .unwrap();
        assert_ne!(repo_scope.scope_key, sub_scope.scope_key);
        assert_eq!(
            sub_scope.canonical_path,
            std::fs::canonicalize(&sub).unwrap()
        );
    }
}
