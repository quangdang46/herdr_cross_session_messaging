# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org/). All workspace crates share one version
(`workspace.package.version`).

## [Unreleased]

### Added

- `herdr-pool` agent CLI (`bins/herdr-pool-cli`): `whoami`, `peers`, `send`,
  `inbox` (+`inbox read`), `reply`, `threads`, `message read|get`, `daemon
  start|stop|status|logs`, `doctor`, `install`, `uninstall`, `version`.
  Global flags `--json`, `--socket-path`, `--state-dir`, `-v`.
- `skills/herdr-messaging/` agent skill (`SKILL.md` +
  `references/commands.md`), embedded into binaries and installed by
  `herdr-pool install [--global]`.
- `herdr-plugin.toml` plugin manifest with `[[startup]]` (`herdr-routerd
  start`) and `open-inbox` workspace action.
- crates.io metadata (keywords/categories) and `version` requirements on
  internal path deps, in preparation for publishing.

### Changed

- README rewritten for the implemented surface: install paths
  (`herdr plugin link` / `herdr plugin install` / `cargo install --git`),
  `herdr integration install <agent>` guidance, real CLI reference,
  configuration/state dirs, troubleshooting, and roadmap.
- `skills/herdr-messaging/references/commands.md` rewritten to match the
  shipped `--help` exactly (daemon subcommands, `message get`, `inbox read`,
  `--project`/`--agent` flags, `version`).

### Release prep decisions (ro3)

- **crates.io**: `herdr-protocol` and `herdr-pool-store` are publishable —
  `cargo publish --dry-run` passes for both. `herdr-protocol@0.1.0` already
  exists on crates.io under this name; publishing requires either a version
  bump negotiation or a rename — verify ownership before releasing.
- **`publish = false`** on `herdr-plugin`, `herdr-pool-core`, `herdr-router`,
  `herdr-pool-cli`, `herdr-routerd`, `herdr-testkit`. Root cause:
  `crates/herdr-plugin/src/skill.rs` embeds `skills/herdr-messaging/**` via
  `include_str!`, which lives outside the crate root and cannot be packaged.
  To publish the rest, move the skill payload under `crates/herdr-plugin/`
  (or fetch it at build time) and drop the `publish = false` markers.
- **Binary distribution**: keep it simple — no GitHub release workflow yet
  (deferred). Install binaries via `cargo install --git
  https://github.com/quangdang46/herdr_cross_session_messaging
  herdr-pool-cli herdr-routerd`, or `cargo build --release` from a checkout.
  Revisit prebuilt GitHub releases after the first tagged release.

## [0.1.0] — 2025-11-25

Initial pre-release: protocol client, SQLite pool store, scope/registry core,
router daemon (`herdr-routerd`), agent CLI (`herdr-pool`), plugin manifest,
and agent skill. MVP scope per `docs/IMPLEMENTATION_PLAN.md`.
