# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org/). All workspace crates share one version
(`workspace.package.version`).

## [Unreleased]

### Added

- `herdr-pool broadcast <text>` — send one message to every live peer in the
  scope (own thread per recipient), with per-recipient delivery results.
- Plugin popup inbox: `[[panes]] inbox` (`placement = "popup"`, 80%×20)
  running `herdr-pool inbox --follow`, which redraws every 2s and supports
  `q`/`Esc` quit and `u` unread-toggle (raw mode on ttys). Identity inside
  plugin panes resolves via `HERDR_PLUGIN_CONTEXT_JSON`
  (`workspace_id`/`tab_id`/`focused_pane_id`) when `HERDR_*_ID` vars are
  absent.
- Blocked-agent rescue (router): on `agent_blocked` the delivery worker
  defers without consuming attempts, captures a pane snapshot tail into the
  daemon log, and shows a throttled `notification.show` toast. After
  `BLOCKED_ESCALATE_AFTER` (5 min) of continuous blocking it prompts another
  live agent in the scope with the snapshot and queued-mail count — once per
  blocked episode.
- Opt-in delivery toast: `HERDR_POOL_NOTIFY_DELIVERY=1|true|yes` makes the
  daemon `notification.show` on every successful delivery.
- `MessageStore::defer` — reschedule pending/failed mail without consuming
  delivery attempts (used for `agent_blocked`).
- `herdr-pool` agent CLI (`crates/herdr-pool-cli`): `whoami`, `peers`, `send`,
  `broadcast`, `inbox` (+`inbox read`, `--follow`), `reply`, `threads`,
  `message read|get`, `daemon start|stop|status|logs`, `doctor`, `install`,
  `uninstall`, `version`. Global flags `--json`, `--socket-path`,
  `--state-dir`, `-v`.
- `skills/herdr-messaging/` agent skill (`SKILL.md` +
  `references/commands.md`), embedded into binaries and installed by
  `herdr-pool install [--global]`.
- `herdr-plugin.toml` plugin manifest with `[[build]]` (`cargo install` of
  both binaries), `[[startup]]` (`herdr-routerd start`), `open-inbox`
  workspace action, and the `inbox` popup pane.
- crates.io metadata (keywords/categories) and `version` requirements on
  internal path deps, in preparation for publishing.

### Fixed

- `agent_id` collision: ids were derived from the timestamp-only prefix of
  uuidv7, so two agents registered in the same millisecond shared an id and
  overwrote each other's registry row. Now the full uuidv7 is used.
- Bootstrap and delivery prompts targeted `terminal_id`, which `agent.prompt`
  rejects (`agent_not_found`); target resolution now prefers `pane_id`, then
  agent name, then `terminal_id`.
- `daemon start` ignored `--socket-path` and could corrupt `--json` output;
  the override now propagates via `HERDR_SOCKET_PATH` and daemon stdio is
  silenced under `--json`.
- `inbox`/`message read` now acknowledge `pending` mail too, so explicitly
  read messages are not prompted again by the delivery worker.
- State dir split-brain: `HERDR_PLUGIN_STATE_DIR` is no longer consulted for
  pool state. Agent panes never receive plugin env vars, so plugin-spawned
  daemons and agent CLIs resolved different state dirs and kept separate
  pools. `HERDR_POOL_STATE_DIR` → XDG → home is now the single resolution
  chain everywhere.
- `reply` marks the original `msg_*` message acknowledged (best-effort).
- Agents whose resolved scope changes (terminal reused in another project)
  no longer leave a live ghost record in the old scope's registry.
- `inbox --follow` survives transient store errors instead of exiting.
- `herdr-pool install` now refreshes marker-owned skill installs whose
  payload content changed without a `SKILL_VERSION` bump (previously
  reported `already_installed` and left stale files). `doctor` reports such
  targets as `outdated`.
- Bootstrap prompt budget no longer burns on `agent_blocked`/
  `agent_not_ready` rejections — an agent first seen while blocked can
  still be bootstrapped after it unblocks, within the same daemon run.
- `herdr-routerd run` now auto-installs the `herdr-messaging` skill into the
  global agent skill dirs (`~/.agents/skills`, `~/.claude/skills`) on every
  start — plugin install is now truly one-step; `herdr-pool install` remains
  for manual/project-local installs. Conflicts are logged, never clobbered.
- `HERDR_POOL_SCOPE_MODE=git` scopes agents by the nearest git toplevel
  (`.git` dir or file) instead of the pane launch cwd, so agents launched in
  different subdirectories of one repo share a pool. Default stays `cwd`.
- `skills/herdr-messaging/` is now the single committed skill source; the
  `.agents/` and `.claude/` copies are installer output, gitignored. A test
  asserts the embedded payload covers every file in the source dir.
- `SKILL.md` anti-pattern no longer claims broadcast does not exist (the CLI
  ships `herdr-pool broadcast`); it now restricts broadcast to explicit
  user-requested all-peer notifications. Skill payload version 0.3.0.
- Automatic pane labeling: when a newly registered agent's (or moved pane's)
  pane has no label, the router sets it to the agent's display name via
  `pane.rename`. User/agent-set labels are never overwritten; once per pane
  per daemon run. `HERDR_POOL_PANE_LABEL=0|false|no|off` disables.

### Changed

- README rewritten for the implemented surface: install paths
  (`herdr plugin link` / `herdr plugin install` / `cargo install --git`),
  `herdr integration install <agent>` guidance, real CLI reference,
  configuration/state dirs, troubleshooting, and roadmap.
- `skills/herdr-messaging/references/commands.md` rewritten to match the
  shipped `--help` exactly (daemon subcommands, `message get`, `inbox read`,
  `broadcast`, `--follow`, `--project`/`--agent` flags, `version`). Skill
  payload version bumped to 0.2.0.
- Plugin manifest `platforms` is now `["linux", "macos"]` — the Herdr socket
  transport on Windows is a named pipe, which `herdr-protocol` does not
  implement yet.

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
- **Binary distribution**: `herdr plugin install` works out of the box — the
  manifest's `[[build]]` commands run `cargo install --path` for both
  binaries (requires a Rust toolchain). No GitHub release workflow yet
  (deferred). Alternatives: `cargo install --git
  https://github.com/quangdang46/herdr_cross_session_messaging
  herdr-pool-cli herdr-routerd`, or `cargo build --release` from a checkout.
  Revisit prebuilt GitHub releases after the first tagged release.

## [0.1.0] — 2025-11-25

Initial pre-release: protocol client, SQLite pool store, scope/registry core,
router daemon (`herdr-routerd`), agent CLI (`herdr-pool`), plugin manifest,
and agent skill. MVP scope per `docs/IMPLEMENTATION_PLAN.md`.
