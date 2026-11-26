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
- Peer TUI rescue (router): when an agent stays `agent_blocked`, a durable
  `rescue_jobs` row (SQLite, survives daemon restart) captures the source
  pane's visible TUI and dispatches a `[HERDR RESCUE REQUEST]` prompt to an
  eligible same-scope peer — idle first, then done, then working; blocked,
  offline, and already-reserved helpers are never chosen. The helper
  re-reads the source pane via Herdr (`agent read`/`send-keys`/`wait`) and
  unblocks it; the pool only detects, snapshots, reserves, dispatches, and
  verifies. Helper reservations (one rescue per helper) plus a partial
  unique index per source make rescue loops (A↔B) impossible; jobs with no
  eligible helper stay `pending`, and jobs open past `RESCUE_JOB_TIMEOUT`
  (15 min) or `RESCUE_MAX_ATTEMPTS` dispatches escalate to `needs_human`
  with a `notification.show` toast. On `agent_blocked` the delivery worker
  still defers without consuming attempts and shows the throttled blocked
  toast as before. Jobs record `captured_at` (snapshot freshness),
  reservations taken earlier in the same sweep exclude later picks (parallel
  rescues get distinct helpers), helpers whose dispatch prompt was rejected
  are excluded from re-picks for that job (R8 — no retrying a helper that
  just went blocked), and `assigned` rows stale past
  `RESCUE_ASSIGNED_STALE` (60s — the crash window between reserve and
  prompt) return to `pending` for redispatch. An agent whose latest job
  ended `needs_human` is muted until it is observed un-blocked, so a
  permanently stuck agent does not churn a fresh job (and toast) every
  sweep; a recovered-then-reblocked agent starts a new episode normally.
- `herdr-pool rescue <rs-id> done|needs-human` — the assigned helper reports
  its outcome back to the pool: `done` moves the job to `verifying` (the
  monitor still requires the source to actually leave `blocked`, with a
  120s `RESCUE_VERIFY_TIMEOUT` grace before `needs_human`), `needs-human`
  escalates immediately with a `notification.show` toast. Only the
  assigned helper may report; the rescue prompt now carries the `rs_*` id.
- Rescue dispatch fencing (migration V4): `assign` mints a `dt_*`
  `dispatch_token` carried in the rescue prompt; the helper must
  `herdr-pool rescue <rs-id> claim <token>` before touching the source
  pane, and `done`/`needs-human` now take the token too. When
  `agent.prompt` fails ambiguously (timeout, closed/transport/protocol
  errors — the prompt may still have reached the helper), the job keeps
  its assignment instead of being instantly re-picked; if the helper never
  claims, the stale-`assigned` recovery re-dispatches with a FRESH token,
  fencing the old helper out of `claim`/report so two helpers cannot both
  work one source pane. Definitive API rejections (`agent_blocked`,
  `agent_not_found`, ...) still release the helper immediately. The prompt
  also carries the stable source agent id and instructs the helper to
  re-resolve the source's current pane via `herdr-pool peers` before
  acting, since `pane.move` can invalidate the pane id captured at
  dispatch time.
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
- Agent identity churn on transient offline marks: identity merge looked up
  existing records with a live-only `terminal_id` query, so an agent marked
  offline by a sighting gap (agent restart, `pane.exited` during a
  `devin --resume` handoff) and then seen again minted a *new* `agent_id` —
  resetting `bootstrap_version` (duplicate bootstrap prompts) and orphaning
  threads/messages addressed to the old id. The merge now uses
  `get_by_terminal_any`, which prefers live rows but falls back to the most
  recent offline record for the same terminal.
- `inbox --follow` survives transient store errors instead of exiting.
- `herdr-pool install` now refreshes marker-owned skill installs whose
  payload content changed without a `SKILL_VERSION` bump (previously
  reported `already_installed` and left stale files). `doctor` reports such
  targets as `outdated`.
- Bootstrap prompt budget no longer burns on `agent_blocked`/
  `agent_not_ready` rejections — an agent first seen while blocked can
  still be bootstrapped after it unblocks, within the same daemon run.
- `send`/`reply`/`broadcast` now defer (no attempt consumed) when the
  recipient's TUI is not `interactive_ready` or `launch_pending` — the
  CLI direct-delivery path had the same paste-into-half-restored-UI hole
  the daemon worker just closed.
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
- `SKILL.md` gains a "Peer rescue" section: helpers receiving
  `[HERDR RESCUE REQUEST]` re-read the source pane via `herdr agent read`,
  interact only with the assigned pane via `herdr agent send-keys`/`wait`,
  verify the source resumed, and never approve destructive actions
  unprompted. Bootstrap prompt bumped to v2 mentioning rescue. Skill
  payload version 0.4.0.
- Bootstrap no longer fires the moment Herdr detects an agent: `agent.prompt`
  could paste the instruction into a TUI that is still launching or
  restoring a session (e.g. `devin --resume`). The router now stores Herdr's
  `interactive_ready`/`launch_pending` flags on the agent record (migration
  V5) and the bootstrapper waits until the agent reports interactive-ready —
  detection alone (`idle` status included) is not sufficient. Status-change
  events carry no readiness fields, so a promptable transition for a
  not-yet-ready agent triggers a fresh `agent.get`; readiness flips are also
  picked up by the periodic `agent.list` reconcile. Older Herdr servers that
  omit `interactive_ready` are treated as ready (previous behavior). Message
  delivery to a not-ready recipient likewise defers without consuming an
  attempt instead of prompting into a half-restored TUI, and rescue helper
  selection skips not-ready agents.
- Pool migrations raced on first open: `PRAGMA user_version` was read before
  the transaction began, so a CLI and the daemon opening a brand-new
  `pool.db` concurrently both saw v1 and both applied V2 — the loser failed
  with "table already exists". Migrations now run inside a `BEGIN
  IMMEDIATE` transaction (version read inside the tx) and retry on
  `SQLITE_BUSY`/`SQLITE_LOCKED` within the busy budget, so concurrent
  first-opens serialize cleanly. The same race existed one step earlier:
  `PRAGMA journal_mode=WAL` ran before `busy_timeout` was registered, so
  racing openers could fail instantly on the WAL switch — the busy timeout
  is now set via the API first and the pragma batch retries on BUSY too.

### Changed

- Workspace layout: `bins/herdr-pool-cli` and `bins/herdr-routerd` moved to
  `crates/` — `bins/` vs `crates/` was only a convention; all Cargo
  packages now live under `crates/`. Package/binary names, dependencies,
  and `herdr-plugin.toml` install paths unchanged in behavior.
- Bootstrap prompt bumped to `BOOTSTRAP_VERSION = 2` (adds a rescue
  pointer); agents bootstrapped at v1 get re-prompted once.
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
