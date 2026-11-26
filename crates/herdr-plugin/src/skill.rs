//! Agent skill installer for the `herdr-messaging` skill.
//!
//! The skill payload (`SKILL.md` + `references/commands.md`) is embedded in the
//! binary via `include_str!`. Installation copies it into per-agent skill
//! directories, guarded by a `.herdr-messaging-managed` marker file containing
//! the installed version and a SHA-256 of the payload. Directories without the
//! marker — or whose contents diverge from what the marker recorded — are
//! treated as user-owned and never overwritten.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::{PluginError, Result};

/// Skill name; must equal the skill directory name and SKILL.md `name:`.
pub const SKILL_NAME: &str = "herdr-messaging";

/// Skill payload version. Keep in sync with the crate version.
pub const SKILL_VERSION: &str = "0.4.0";

/// Marker file written into installed skill dirs to record ownership.
pub const MANAGED_MARKER: &str = ".herdr-messaging-managed";

const SKILL_MD: &str = include_str!("../../../skills/herdr-messaging/SKILL.md");
const COMMANDS_MD: &str = include_str!("../../../skills/herdr-messaging/references/commands.md");

/// One file in the embedded payload, relative to the skill dir.
struct PayloadFile {
    rel_path: &'static str,
    contents: &'static str,
}

const PAYLOAD: &[PayloadFile] = &[
    PayloadFile {
        rel_path: "SKILL.md",
        contents: SKILL_MD,
    },
    PayloadFile {
        rel_path: "references/commands.md",
        contents: COMMANDS_MD,
    },
];

/// Where the skill is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    /// Project-local: `<root>/.agents/skills/…` and `<root>/.claude/skills/…`.
    Project,
    /// Per-user: `~/.agents/skills/…` and `~/.claude/skills/…`.
    Global,
}

/// Per-target outcome of an install run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetOutcome {
    /// Fresh install into a previously absent directory.
    Installed { path: PathBuf },
    /// Updated an intact marker-owned install to the current version.
    Updated { path: PathBuf },
    /// Already at the current version with unmodified contents.
    AlreadyInstalled { path: PathBuf },
    /// Directory exists but is user-owned or modified; left untouched.
    Conflict { path: PathBuf, reason: String },
}

impl TargetOutcome {
    /// Path of the target directory this outcome refers to.
    pub fn path(&self) -> &Path {
        match self {
            TargetOutcome::Installed { path }
            | TargetOutcome::Updated { path }
            | TargetOutcome::AlreadyInstalled { path }
            | TargetOutcome::Conflict { path, .. } => path,
        }
    }

    /// Whether this outcome left the skill correctly installed.
    pub fn is_installed(&self) -> bool {
        !matches!(self, TargetOutcome::Conflict { .. })
    }
}

/// Aggregate result of [`install_skill`].
#[derive(Debug, Clone)]
pub struct InstallOutcome {
    /// Scope the operation ran against.
    pub scope: SkillScope,
    /// One entry per target directory (`.agents` and `.claude` variants).
    pub targets: Vec<TargetOutcome>,
}

impl InstallOutcome {
    /// True when every target ended up installed at the current version.
    pub fn all_installed(&self) -> bool {
        self.targets.iter().all(TargetOutcome::is_installed)
    }

    /// Targets that hit a conflict (user-owned or modified directories).
    pub fn conflicts(&self) -> impl Iterator<Item = &TargetOutcome> {
        self.targets
            .iter()
            .filter(|t| matches!(t, TargetOutcome::Conflict { .. }))
    }
}

/// Per-target state reported by [`skill_status`], for `doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetStatus {
    /// Marker present, contents match, version recorded.
    Installed { path: PathBuf, version: String },
    /// Marker present, contents match the marker, but the embedded payload
    /// is newer (installed before the last payload change).
    Outdated { path: PathBuf, version: String },
    /// Marker present but contents were modified after install.
    Modified { path: PathBuf, version: String },
    /// Directory exists without our marker — user-owned.
    Unmanaged { path: PathBuf },
    /// No skill directory at this target.
    Missing { path: PathBuf },
}

impl TargetStatus {
    /// Path of the target directory this status refers to.
    pub fn path(&self) -> &Path {
        match self {
            TargetStatus::Installed { path, .. }
            | TargetStatus::Outdated { path, .. }
            | TargetStatus::Modified { path, .. }
            | TargetStatus::Unmanaged { path }
            | TargetStatus::Missing { path } => path,
        }
    }
}

/// Outcome of removing skill dirs; only marker-owned dirs are removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UninstallOutcome {
    /// Marker-owned dir removed.
    Removed { path: PathBuf },
    /// Dir exists but is not marker-owned; left in place.
    Skipped { path: PathBuf },
    /// Nothing to remove.
    Missing { path: PathBuf },
}

impl UninstallOutcome {
    /// Path of the target directory this outcome refers to.
    pub fn path(&self) -> &Path {
        match self {
            UninstallOutcome::Removed { path }
            | UninstallOutcome::Skipped { path }
            | UninstallOutcome::Missing { path } => path,
        }
    }
}

/// Contents of the `.herdr-messaging-managed` marker file.
#[derive(Debug, Clone, Deserialize)]
struct Marker {
    version: String,
    sha256: String,
}

/// SHA-256 (hex) of the embedded payload — recorded in the marker and used to
/// detect user modifications of installed files.
pub fn payload_sha256() -> String {
    let mut hasher = Sha256::new();
    for file in PAYLOAD {
        hasher.update(file.rel_path.as_bytes());
        hasher.update([0u8]);
        hasher.update(file.contents.as_bytes());
        hasher.update([0u8]);
    }
    hex_lower(&hasher.finalize())
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// Hash the payload files currently on disk under `dir`, using the same
/// scheme as [`payload_sha256`]. Missing files hash as absent.
fn installed_payload_sha256(dir: &Path) -> String {
    let mut hasher = Sha256::new();
    for file in PAYLOAD {
        hasher.update(file.rel_path.as_bytes());
        hasher.update([0u8]);
        match fs::read(dir.join(file.rel_path)) {
            Ok(bytes) => hasher.update(&bytes),
            Err(_) => hasher.update(b"<missing>"),
        }
        hasher.update([0u8]);
    }
    hex_lower(&hasher.finalize())
}

/// Target skill directories for a scope.
///
/// Project: `<root>/.agents/skills/herdr-messaging` (Codex, Gemini, OpenCode,
/// Cursor) and `<root>/.claude/skills/herdr-messaging` (Claude).
/// Global: same layout under the user's home directory.
fn target_dirs(scope: SkillScope, root: &Path, home: &Path) -> Vec<PathBuf> {
    let base = match scope {
        SkillScope::Project => root.to_path_buf(),
        SkillScope::Global => home.to_path_buf(),
    };
    vec![
        base.join(".agents").join("skills").join(SKILL_NAME),
        base.join(".claude").join("skills").join(SKILL_NAME),
    ]
}

fn resolve_home() -> Result<PathBuf> {
    std::env::home_dir().ok_or(PluginError::MissingEnv("HOME"))
}

/// Install the skill for `scope`.
///
/// `target_root` is the project root for [`SkillScope::Project`] and ignored
/// for [`SkillScope::Global`] (the user's home is used instead).
pub fn install_skill(target_root: &Path, scope: SkillScope) -> Result<InstallOutcome> {
    let home = if scope == SkillScope::Global {
        resolve_home()?
    } else {
        PathBuf::new()
    };
    install_skill_inner(target_root, scope, &home)
}

/// Testable variant of [`install_skill`] with an explicit home directory.
pub fn install_skill_with_home(
    target_root: &Path,
    scope: SkillScope,
    home: &Path,
) -> Result<InstallOutcome> {
    install_skill_inner(target_root, scope, home)
}

#[tracing::instrument(skip_all, name = "skill.install", level = "debug", fields(scope = ?scope))]
fn install_skill_inner(
    target_root: &Path,
    scope: SkillScope,
    home: &Path,
) -> Result<InstallOutcome> {
    let mut targets = Vec::new();
    for dir in target_dirs(scope, target_root, home) {
        targets.push(install_one(&dir)?);
    }
    Ok(InstallOutcome { scope, targets })
}

/// Uninstall the skill for `scope`; only marker-owned directories are removed.
pub fn uninstall_skill(target_root: &Path, scope: SkillScope) -> Result<Vec<UninstallOutcome>> {
    let home = if scope == SkillScope::Global {
        resolve_home()?
    } else {
        PathBuf::new()
    };
    uninstall_skill_inner(target_root, scope, &home)
}

/// Testable variant of [`uninstall_skill`] with an explicit home directory.
pub fn uninstall_skill_with_home(
    target_root: &Path,
    scope: SkillScope,
    home: &Path,
) -> Result<Vec<UninstallOutcome>> {
    uninstall_skill_inner(target_root, scope, home)
}

#[tracing::instrument(skip_all, name = "skill.uninstall", level = "debug", fields(scope = ?scope))]
fn uninstall_skill_inner(
    target_root: &Path,
    scope: SkillScope,
    home: &Path,
) -> Result<Vec<UninstallOutcome>> {
    let mut outcomes = Vec::new();
    for dir in target_dirs(scope, target_root, home) {
        outcomes.push(uninstall_one(&dir)?);
    }
    Ok(outcomes)
}

/// Report per-target install state for `doctor`.
pub fn skill_status(target_root: &Path, scope: SkillScope) -> Result<Vec<TargetStatus>> {
    let home = if scope == SkillScope::Global {
        resolve_home()?
    } else {
        PathBuf::new()
    };
    Ok(target_dirs(scope, target_root, &home)
        .into_iter()
        .map(|dir| status_one(&dir))
        .collect())
}

/// Testable variant of [`skill_status`] with an explicit home directory.
pub fn skill_status_with_home(
    target_root: &Path,
    scope: SkillScope,
    home: &Path,
) -> Result<Vec<TargetStatus>> {
    Ok(target_dirs(scope, target_root, home)
        .into_iter()
        .map(|dir| status_one(&dir))
        .collect())
}

fn read_marker(dir: &Path) -> Result<Option<Marker>> {
    let path = dir.join(MANAGED_MARKER);
    match fs::read_to_string(&path) {
        Ok(text) => toml::from_str::<Marker>(&text)
            .map(Some)
            .map_err(|err| PluginError::Manifest(format!("marker {}: {err}", path.display()))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn status_one(dir: &Path) -> TargetStatus {
    if !dir.is_dir() {
        return TargetStatus::Missing {
            path: dir.to_path_buf(),
        };
    }
    let marker = match read_marker(dir) {
        Ok(Some(m)) => m,
        Ok(None) => {
            return TargetStatus::Unmanaged {
                path: dir.to_path_buf(),
            }
        }
        Err(_) => {
            return TargetStatus::Unmanaged {
                path: dir.to_path_buf(),
            }
        }
    };
    if installed_payload_sha256(dir) == marker.sha256 {
        if marker.sha256 == payload_sha256() {
            TargetStatus::Installed {
                path: dir.to_path_buf(),
                version: marker.version,
            }
        } else {
            TargetStatus::Outdated {
                path: dir.to_path_buf(),
                version: marker.version,
            }
        }
    } else {
        TargetStatus::Modified {
            path: dir.to_path_buf(),
            version: marker.version,
        }
    }
}

fn install_one(dir: &Path) -> Result<TargetOutcome> {
    if !dir.exists() {
        write_payload(dir)?;
        return Ok(TargetOutcome::Installed {
            path: dir.to_path_buf(),
        });
    }

    // Directory exists: consult the marker to decide ownership.
    let marker = match read_marker(dir)? {
        Some(m) => m,
        None => {
            return Ok(TargetOutcome::Conflict {
                path: dir.to_path_buf(),
                reason: "directory exists without .herdr-messaging-managed marker".into(),
            })
        }
    };

    let current_hash = installed_payload_sha256(dir);
    if current_hash != marker.sha256 {
        return Ok(TargetOutcome::Conflict {
            path: dir.to_path_buf(),
            reason: "installed files were modified after install".into(),
        });
    }

    // Payload content can change without a SKILL_VERSION bump; refresh whenever
    // the recorded hash or version differs from what this binary embeds.
    if marker.version == SKILL_VERSION && marker.sha256 == payload_sha256() {
        return Ok(TargetOutcome::AlreadyInstalled {
            path: dir.to_path_buf(),
        });
    }

    // Intact marker-owned install at an older version or payload: safe to update.
    write_payload(dir)?;
    Ok(TargetOutcome::Updated {
        path: dir.to_path_buf(),
    })
}

fn uninstall_one(dir: &Path) -> Result<UninstallOutcome> {
    if !dir.exists() {
        return Ok(UninstallOutcome::Missing {
            path: dir.to_path_buf(),
        });
    }
    match read_marker(dir)? {
        Some(_) => {
            fs::remove_dir_all(dir)?;
            Ok(UninstallOutcome::Removed {
                path: dir.to_path_buf(),
            })
        }
        None => Ok(UninstallOutcome::Skipped {
            path: dir.to_path_buf(),
        }),
    }
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write the payload into `dir` via a temp sibling + atomic rename.
///
/// The existing directory (if any) is removed only after the temp copy is
/// fully written and validated; callers must have verified ownership first.
#[tracing::instrument(skip_all, name = "skill.write", level = "debug", fields(path = %dir.display()))]
fn write_payload(dir: &Path) -> Result<()> {
    let parent = dir
        .parent()
        .ok_or_else(|| PluginError::Manifest(format!("{} has no parent", dir.display())))?;
    fs::create_dir_all(parent)?;

    let seq = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(
        ".{}.tmp-{}-{}",
        dir.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(SKILL_NAME),
        std::process::id(),
        seq
    ));
    if tmp.exists() {
        fs::remove_dir_all(&tmp)?;
    }

    let result = write_payload_into(&tmp).and_then(|()| {
        validate_skill_dir(&tmp, SKILL_NAME)?;
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
        fs::rename(&tmp, dir)?;
        Ok(())
    });

    if result.is_err() {
        let _ = fs::remove_dir_all(&tmp);
    }
    result
}

fn write_payload_into(dir: &Path) -> Result<()> {
    for file in PAYLOAD {
        let dest = dir.join(file.rel_path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dest, file.contents)?;
    }
    let marker = format!(
        "version = {:?}\nsha256 = {:?}\n",
        SKILL_VERSION,
        payload_sha256()
    );
    fs::write(dir.join(MANAGED_MARKER), marker)?;
    Ok(())
}

/// Extract the `name:` value from SKILL.md YAML-ish frontmatter.
fn frontmatter_name(markdown: &str) -> Option<&str> {
    let mut lines = markdown.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            return None;
        }
        if let Some(rest) = trimmed.strip_prefix("name:") {
            let value = rest.trim().trim_matches('"').trim_matches('\'');
            return Some(value);
        }
    }
    None
}

/// Verify the staged skill dir: SKILL.md exists and its frontmatter `name`
/// equals the final directory name (agentskills.io requirement). Validated
/// against `expected_name` because the temp staging dir has a different name.
fn validate_skill_dir(dir: &Path, expected_name: &str) -> Result<()> {
    let skill_md = dir.join("SKILL.md");
    let text = fs::read_to_string(&skill_md)
        .map_err(|err| PluginError::Manifest(format!("read {}: {err}", skill_md.display())))?;
    match frontmatter_name(&text) {
        Some(name) if name == expected_name => Ok(()),
        Some(name) => Err(PluginError::Manifest(format!(
            "SKILL.md frontmatter name {name:?} does not match directory {expected_name:?}"
        ))),
        None => Err(PluginError::Manifest(
            "SKILL.md has no frontmatter name field".into(),
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn embedded_skill_frontmatter_is_valid() {
        assert_eq!(frontmatter_name(SKILL_MD), Some(SKILL_NAME));
        // The install dir name must match the frontmatter name.
        assert!(PAYLOAD.iter().any(|f| f.rel_path == "SKILL.md"));
    }

    #[test]
    fn payload_hash_is_stable() {
        assert_eq!(payload_sha256(), payload_sha256());
        assert_eq!(payload_sha256().len(), 64);
    }

    #[test]
    fn install_fresh_project_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let outcome =
            install_skill_with_home(root, SkillScope::Project, Path::new("/unused")).unwrap();
        assert!(outcome.all_installed());
        assert_eq!(outcome.targets.len(), 2);
        for t in &outcome.targets {
            assert!(matches!(t, TargetOutcome::Installed { .. }));
            let dir = t.path();
            assert!(dir.join("SKILL.md").is_file());
            assert!(dir.join("references/commands.md").is_file());
            assert!(dir.join(MANAGED_MARKER).is_file());
            assert!(dir.ends_with("herdr-messaging"));
        }
        assert!(root.join(".agents/skills/herdr-messaging").is_dir());
        assert!(root.join(".claude/skills/herdr-messaging").is_dir());
    }

    #[test]
    fn install_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        install_skill_with_home(root, SkillScope::Project, Path::new("/u")).unwrap();
        let outcome = install_skill_with_home(root, SkillScope::Project, Path::new("/u")).unwrap();
        assert!(outcome
            .targets
            .iter()
            .all(|t| matches!(t, TargetOutcome::AlreadyInstalled { .. })));
    }

    #[test]
    fn install_conflicts_on_unmanaged_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let user_dir = root.join(".agents/skills/herdr-messaging");
        fs::create_dir_all(&user_dir).unwrap();
        fs::write(user_dir.join("SKILL.md"), "user content").unwrap();

        let outcome = install_skill_with_home(root, SkillScope::Project, Path::new("/u")).unwrap();
        // .agents target conflicts; .claude target installs fresh.
        let agents = outcome
            .targets
            .iter()
            .find(|t| t.path().starts_with(root.join(".agents")))
            .unwrap();
        assert!(matches!(agents, TargetOutcome::Conflict { .. }));
        // User file untouched.
        assert_eq!(
            fs::read_to_string(user_dir.join("SKILL.md")).unwrap(),
            "user content"
        );
        assert!(!outcome.all_installed());
        assert_eq!(outcome.conflicts().count(), 1);
    }

    #[test]
    fn install_conflicts_on_modified_managed_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        install_skill_with_home(root, SkillScope::Project, Path::new("/u")).unwrap();
        let skill_md = root.join(".agents/skills/herdr-messaging/SKILL.md");
        fs::write(&skill_md, "user edited").unwrap();

        let outcome = install_skill_with_home(root, SkillScope::Project, Path::new("/u")).unwrap();
        let agents = outcome
            .targets
            .iter()
            .find(|t| t.path().starts_with(root.join(".agents")))
            .unwrap();
        assert!(matches!(agents, TargetOutcome::Conflict { .. }));
        assert_eq!(fs::read_to_string(&skill_md).unwrap(), "user edited");
    }

    #[test]
    fn uninstall_removes_only_managed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Managed install.
        install_skill_with_home(root, SkillScope::Project, Path::new("/u")).unwrap();
        // User-owned dir at a third location is irrelevant; check .claude removed.
        let outcomes =
            uninstall_skill_with_home(root, SkillScope::Project, Path::new("/u")).unwrap();
        assert!(outcomes
            .iter()
            .all(|o| matches!(o, UninstallOutcome::Removed { .. })));
        assert!(!root.join(".agents/skills/herdr-messaging").exists());
        assert!(!root.join(".claude/skills/herdr-messaging").exists());
    }

    #[test]
    fn uninstall_skips_unmanaged_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let user_dir = root.join(".claude/skills/herdr-messaging");
        fs::create_dir_all(&user_dir).unwrap();
        fs::write(user_dir.join("SKILL.md"), "mine").unwrap();

        let outcomes =
            uninstall_skill_with_home(root, SkillScope::Project, Path::new("/u")).unwrap();
        assert!(outcomes
            .iter()
            .any(|o| matches!(o, UninstallOutcome::Skipped { .. })));
        assert_eq!(
            fs::read_to_string(user_dir.join("SKILL.md")).unwrap(),
            "mine"
        );
    }

    #[test]
    fn status_reports_matrix() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Nothing installed.
        let statuses = skill_status_with_home(root, SkillScope::Project, Path::new("/u")).unwrap();
        assert!(statuses
            .iter()
            .all(|s| matches!(s, TargetStatus::Missing { .. })));

        // Install, then corrupt one target.
        install_skill_with_home(root, SkillScope::Project, Path::new("/u")).unwrap();
        fs::write(
            root.join(".agents/skills/herdr-messaging/SKILL.md"),
            "tampered",
        )
        .unwrap();

        let statuses = skill_status_with_home(root, SkillScope::Project, Path::new("/u")).unwrap();
        let agents = statuses
            .iter()
            .find(|s| s.path().starts_with(root.join(".agents")))
            .unwrap();
        assert!(matches!(agents, TargetStatus::Modified { .. }));
        let claude = statuses
            .iter()
            .find(|s| s.path().starts_with(root.join(".claude")))
            .unwrap();
        assert!(matches!(claude, TargetStatus::Installed { .. }));
    }

    #[test]
    fn stale_payload_is_outdated_and_reinstall_refreshes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        install_skill_with_home(root, SkillScope::Project, Path::new("/u")).unwrap();

        // Simulate an install made before the embedded payload last changed:
        // on-disk files match the marker, but the marker hash is not the
        // current payload hash.
        let dir = root.join(".agents/skills/herdr-messaging");
        fs::write(dir.join("SKILL.md"), "older payload").unwrap();
        let stale_hash = installed_payload_sha256(&dir);
        assert_ne!(stale_hash, payload_sha256());
        fs::write(
            dir.join(MANAGED_MARKER),
            format!("version = {SKILL_VERSION:?}\nsha256 = {stale_hash:?}\n"),
        )
        .unwrap();

        let statuses = skill_status_with_home(root, SkillScope::Project, Path::new("/u")).unwrap();
        let agents = statuses
            .iter()
            .find(|s| s.path().starts_with(root.join(".agents")))
            .unwrap();
        assert!(matches!(agents, TargetStatus::Outdated { .. }));

        // Reinstall refreshes the stale target even at the same SKILL_VERSION.
        let outcome = install_skill_with_home(root, SkillScope::Project, Path::new("/u")).unwrap();
        let agents = outcome
            .targets
            .iter()
            .find(|t| t.path().starts_with(root.join(".agents")))
            .unwrap();
        assert!(matches!(agents, TargetOutcome::Updated { .. }));
        assert_eq!(fs::read_to_string(dir.join("SKILL.md")).unwrap(), SKILL_MD);
    }

    #[test]
    fn global_scope_uses_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let outcome =
            install_skill_with_home(Path::new("/project"), SkillScope::Global, &home).unwrap();
        assert!(outcome.all_installed());
        assert!(home
            .join(".agents/skills/herdr-messaging/SKILL.md")
            .is_file());
        assert!(home
            .join(".claude/skills/herdr-messaging/SKILL.md")
            .is_file());
    }

    #[test]
    fn payload_covers_every_source_file() {
        // skills/herdr-messaging/ is the single source of truth; PAYLOAD must
        // embed every file in it (and nothing else) so installs can't drift.
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills/herdr-messaging");
        let mut on_disk = Vec::new();
        collect_rel_files(&src, &src, &mut on_disk);
        on_disk.sort();

        let mut embedded: Vec<&str> = PAYLOAD.iter().map(|f| f.rel_path).collect();
        embedded.sort_unstable();
        assert_eq!(on_disk, embedded);

        for file in PAYLOAD {
            let disk = fs::read_to_string(src.join(file.rel_path)).unwrap();
            assert_eq!(disk, file.contents, "payload drift for {}", file.rel_path);
        }
    }

    fn collect_rel_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect_rel_files(root, &path, out);
            } else {
                out.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }

    #[test]
    fn marker_records_version_and_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        install_skill_with_home(root, SkillScope::Project, Path::new("/u")).unwrap();
        let marker = fs::read_to_string(
            root.join(".agents/skills/herdr-messaging")
                .join(MANAGED_MARKER),
        )
        .unwrap();
        let parsed: Marker = toml::from_str(&marker).unwrap();
        assert_eq!(parsed.version, SKILL_VERSION);
        assert_eq!(parsed.sha256, payload_sha256());
    }
}
