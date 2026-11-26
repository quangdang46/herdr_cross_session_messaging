# herdr-pool — Cross-Session Messaging for Herdr Agents

<div align="center">
  <img src="assets/herdr_pool_illustration.webp" alt="herdr-pool — peer-to-peer messaging for coding agents inside Herdr">
</div>

<div align="center">

![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS-blue.svg)
![Rust](https://img.shields.io/badge/Rust-1.92%2B-orange.svg)
![License](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)
![Status](https://img.shields.io/badge/status-pre--release-yellow.svg)

</div>

**Peer-to-peer messaging for coding agents running inside [Herdr](https://herdr.dev).**  
Agents in the same project folder discover each other, exchange threaded
messages, and help each other — without the user acting as a relay.

> **Status: pre-release (0.1.0).** The protocol, store, core, plugin manifest,
> `herdr-routerd` daemon, `herdr-pool` agent CLI, and `herdr-messaging` skill
> are all implemented and tested. Design:
> [`docs/IMPLEMENTATION_PLAN.md`](docs/IMPLEMENTATION_PLAN.md).

> **Why this exists:** I really enjoyed using Claude Code's
> [cross-session messaging](https://code.claude.com/docs/en/cross-session-messaging)
> — sessions warning each other about breaking changes mid-task is genuinely
> useful. But it only works between Claude Code sessions, shares one global
> message space across every folder on the machine, and agents don't know the
> feature exists until you remind them. So I built the same idea for *any*
> agent inside Herdr, scoped per project folder.

## TL;DR

### The problem

You run several coding agents in Herdr panes — a reviewer, an implementer, a
researcher. Today the only way they talk is **through you**: you read one
agent's output, retype it into another pane, and carry the reply back. Agents
can't see each other, can't ask each other questions, and can't hand off work.
Raw `herdr agent prompt` technically reaches another pane, but it has no
project isolation, no threading, no delivery tracking, and no way for the
*recipient agent* to pull its own inbox.

### The solution

`herdr-pool` gives every Herdr project scope a private message pool backed by a
local router daemon. Agents call a simple CLI — `whoami` → `peers` → `inbox` →
`send`/`reply` — and the daemon handles discovery (via Herdr's socket API),
delivery (via `agent.prompt`), retry while peers are busy, and per-scope
SQLite persistence. Everything is sync Rust: Unix sockets + threads, no async
runtime.

### Why herdr-pool?

| Capability | herdr-pool | `herdr agent prompt` ad hoc | User-as-relay |
|---|---|---|---|
| Project-scope isolation | ✅ enforced by daemon | ❌ any pane reachable | manual |
| Threaded conversations | ✅ `threads`/`reply` | ❌ flat prompts | ❌ |
| Delivery tracking | ✅ `pending→delivered→acked` | ❌ fire-and-forget | ❌ |
| Retry while peer busy/blocked | ✅ daemon backoff + dead-letter | ❌ `agent_blocked` error | ❌ |
| Recipient pulls own inbox | ✅ `herdr-pool inbox` | ❌ prompt lands as task input | ❌ |
| Agent self-service skill | ✅ `herdr-messaging` SKILL.md auto-installed | ❌ you write the prompts | ❌ |
| Cross-machine / cloud | ❌ local Herdr only | ❌ | ❌ |
| Broadcast | ❌ by design (MVP) | manual loop | manual |

## Quick example

```bash
# Start the router daemon (also spawned automatically by the plugin startup hook)
herdr-routerd start
herdr-routerd status          # → herdr-routerd running (pid 1234, uptime 87s)

# Inside an agent pane (HERDR_ENV=1):
herdr-pool whoami             # → your agent id, name, project scope
herdr-pool peers              # → other agents in this project scope
herdr-pool inbox              # → pending messages
herdr-pool send ag_01H… "can you review crates/herdr-router/src/daemon.rs?"
herdr-pool threads            # → conversation history
herdr-pool inbox read msg_01H…  # acknowledge a message
```

## Design principles

| Principle | How it shows up |
|---|---|
| **Herdr is the transport** | Discovery via `agent.list`/`session.snapshot`, state via `events.subscribe`, delivery via `agent.prompt`. No parallel agent registry fighting Herdr's. |
| **Scope is isolation** | Pool DB is keyed by `sha256(canonical project dir)`; the router re-checks scope on every delivery. Cross-scope sends are rejected, never silently routed. |
| **Sync, boring, correct** | `UnixStream` + threads + `rusqlite`. One request per connection; only event streams hold a socket open. No tokio. |
| **Sent ≠ read** | Status flow `pending → delivered → acknowledged`; `agent.prompt` success is `delivered`, recipient action is `acknowledged`. |
| **Peer messages are data** | The shipped skill tells agents to treat inbound peer text as input to evaluate — never as instructions. |
| **No overwrite, ever** | Skill installer uses a `.herdr-messaging-managed` marker + atomic rename; user-owned skill dirs are never clobbered. |

## Installation

Requires Rust ≥ 1.92 and a running [Herdr](https://herdr.dev) (≥ 0.9.0) server.
Platform support: Linux and macOS (Unix sockets). Windows named-pipe support
is designed but cfg-gated out of MVP.

**As a Herdr plugin (recommended):**

```bash
# Development — link this checkout:
herdr plugin link /path/to/herdr_cross_session_messaging

# Release — install from GitHub (requires a Rust toolchain; the plugin's
# [[build]] commands run `cargo install` to put the binaries on PATH):
herdr plugin install quangdang46/herdr_cross_session_messaging
```

The plugin's `[[startup]]` hook runs `herdr-routerd start` automatically after
Herdr restore/live-handoff, so the daemon is already up before agents send.
The plugin also ships an `inbox` popup pane entrypoint
(`herdr plugin pane open --plugin herdr.cross-session-messaging
--entrypoint inbox`) for quick inbox access — note that
`plugin action invoke` runs commands headless and will not show the
popup. A `panel-toggle` action docks a narrow **left split pane**
running the live inbox into the current tab — bind it in
`~/.config/herdr/config.toml`:

```toml
[[keys.command]]
key = "cmd+i"
type = "shell"
command = "herdr plugin action invoke panel-toggle --plugin herdr.cross-session-messaging"
```

Toggle semantics: closed → open+dock (34 cols, left edge); open but not
focused → focus; focused → close. `herdr-pool panel open|close|status` does
the same from a shell. Per-tab state lives under
`<state>/panel/<tab_id>`; the panel re-discovers itself by label if the
state file is lost.

**Binaries on PATH** (the plugin needs `herdr-pool` and `herdr-routerd`
findable — `herdr plugin install`'s `[[build]]` commands handle this via
`cargo install`; a Rust toolchain is required). You can also install
straight from git:

```bash
cargo install --git https://github.com/quangdang46/herdr_cross_session_messaging \
    herdr-pool-cli herdr-routerd
```

Or build from a checkout:

```bash
git clone https://github.com/quangdang46/herdr_cross_session_messaging
cd herdr_cross_session_messaging
cargo build --release --workspace
export PATH="$PWD/target/release:$PATH"
```

`herdr-pool` and `herdr-routerd` land in `target/release/`. Check
`herdr plugin list` / `herdr-pool doctor` afterwards. (Only the
`herdr-protocol`/`herdr-pool-store` libraries are crates.io-publishable today
— the rest are `publish = false` until the embedded skill payload moves into
`herdr-plugin`; see CHANGELOG.md.)

**Agent skill:** installed automatically — `herdr-routerd` (spawned by the
plugin `[[startup]]` hook) installs the `herdr-messaging` skill into the
global agent skill dirs (`~/.agents/skills`, `~/.claude/skills`) on every
start, refreshing it when the payload changes and never overwriting
user-modified dirs (marker file `.herdr-messaging-managed`). To install or
refresh manually — e.g. project-local copies or a specific root:

```bash
herdr-pool install            # project-local (.agents/skills, .claude/skills)
herdr-pool install --global   # per-user (~/.agents/skills, ~/.claude/skills)
```

`herdr-pool uninstall` removes only what the installer owns.

### Updating

`herdr-pool update` self-updates the whole install in one step:

```bash
herdr-pool update           # GitHub install: `herdr plugin update` + rebuild
herdr-pool update --global  # also refresh the per-user skill install
```

For GitHub-installed plugins it delegates to `herdr plugin update --yes`
(which refetches the repo and reruns the manifest `[[build]]` commands —
`cargo install` fresh binaries). For a locally linked checkout it runs
`git pull --ff-only` in the plugin root plus the same `cargo install
--path` builds instead. Either way it then reinstalls the skill payload
with the new binary and restarts `herdr-routerd` if it was running.
`--json` reports `{plugin, source_kind, version_before, version_after,
skills_project, skills_global, daemon}`.

### Cleaning up

Pool databases accumulate per project scope. `herdr-pool clean` prunes
terminal rows older than a cutoff across **all** pools:

```bash
herdr-pool clean                  # delete acked/dead mail + offline agents >30d
herdr-pool clean --older-than 7   # tighter window
herdr-pool clean --dry-run        # preview counts without deleting
```

It removes `acknowledged`/`dead` messages, `offline`/`unknown` agent
records that no surviving message references (peer names stay resolvable
for kept history), and emptied threads — then `VACUUM`s each touched db.
Live `pending`/`delivered`/`failed` mail is never touched.

### Agent integrations (`herdr integration install`)

For the best experience — session resume/identity and live agent state —
install Herdr's built-in integration for each agent kind you use:

```bash
herdr integration install claude     # Claude Code
herdr integration install codex      # Codex
herdr integration install pi         # Pi
herdr integration install opencode   # OpenCode
# …also: omp, kimi, kilo, mastracode, copilot, devin, droid, hermes,
#        qodercli, qwen, cursor, antigravity-cli, grok, letta
```

Run `herdr integration status` to see what's installed. **This is always a
user decision — herdr-pool never runs integration installs itself.**

Two tiers of integration quality:

- **Report state directly** (richer status for `peers`/delivery decisions):
  `pi`, `omp`, `kimi`, `opencode`, `kilo`, `mastracode`.
- **Session identity/resume via hooks, screen-detection for state**:
  `claude`, `codex`, `copilot`, `devin`, `droid`, `hermes`, `qodercli`,
  `qwen`, `cursor`, `antigravity-cli`, `grok`, `letta` — these still get
  session identity so `whoami`/scoping works, but agent status is inferred
  from the pane screen rather than reported.

Agents without any integration still work — scope falls back to pane launch
cwd — but `whoami`/`peers` metadata is thinner.

## Commands

### `herdr-routerd` — router daemon (implemented)

| Command | What it does |
|---|---|
| `herdr-routerd start` | Spawn the daemon detached; logs to `<state>/logs/routerd.log`. No-op if already running. |
| `herdr-routerd stop` | Write shutdown sentinel + SIGTERM; graceful exit after in-flight work. |
| `herdr-routerd status` | Print pid / state / uptime. |
| `herdr-routerd tick` | One-shot `session.snapshot` + registry reconcile (used by plugin `[[events]]` hooks). |
| `herdr-routerd run` | Foreground event loop (what `start` detaches into). |

State dir resolution: `HERDR_POOL_STATE_DIR` → XDG
state → `~/.local/state/herdr-cross-session-messaging`. Single-instance enforced by lockfile.

### `herdr-pool` — agent-facing CLI

Global flags on every subcommand: `--json`, `--socket-path <PATH>`,
`--state-dir <DIR>`, `-v/--verbose`. Exit codes: 0 ok, 1 runtime failure,
2 usage/precondition error.

| Command | Purpose |
|---|---|
| `whoami [--json]` | Resolve self via `HERDR_PANE_ID` → agent record + scope |
| `peers [--json]` | List live agents in the same project scope |
| `send <peer> <text> [--thread <th_id>]` | Send a message; peer = `ag_*` id, pane/terminal id, or unambiguous name |
| `broadcast <text>` | Send one message to every live peer in the scope (own thread each) |
| `inbox [--unread] [--follow] [--json]` | List messages addressed to this agent (`--follow` redraws live for the popup pane) |
| `inbox read <msg_id>` | Mark a message acknowledged, print full content |
| `reply <msg_id\|th_id> <text>` | Reply inside a thread (to the original sender) |
| `threads [--json]` | Per-thread summaries for the scope |
| `message read <msg_id>` / `message get <msg_id>` | Acknowledge+print, or fetch the full record as JSON |
| `daemon start\|stop\|status\|logs` | Manage the `herdr-routerd` delivery daemon |
| `panel open\|close\|toggle\|status` | Manage the left-docked inbox panel (split pane, per tab) |
| `panel ensure` | `[[events]]` autostart hook — docks the panel on new tabs when `[panel] autostart` is on |
| `doctor [--project <dir>] [--json]` | Env / socket / scope / DB / daemon / skill health checks |
| `install [--global] [--agent <kind>] [--project <dir>]` | Install the `herdr-messaging` skill into agent skill dirs |
| `uninstall [--global] [--agent <kind>] [--project <dir>]` | Remove only marker-owned skill dirs |
| `version [--json]` | CLI + skill payload versions |

Authoritative surface: `herdr-pool --help`; the shipped skill's
[`references/commands.md`](skills/herdr-messaging/references/commands.md)
mirrors it for agents.

## Agent quickstart

For agents running inside Herdr panes, the workflow is deliberately small:

```sh
# Preconditions (from skills/herdr-messaging/SKILL.md)
test "${HERDR_ENV:-}" = "1" && command -v herdr-pool

herdr-pool whoami     # who am I, which scope
herdr-pool peers      # who can I reach
herdr-pool inbox      # anything for me?
herdr-pool send <peer> "..." --thread <id>
herdr-pool reply <message-id|thread> "..."
```

**Output contract** (per `--json` convention across the workspace):
stdout = data, stderr = diagnostics (`tracing` logs via `RUST_LOG`/`HERDR_LOG`),
exit 0 = success, non-zero = error with message on stderr. Always pass `--json`
when parsing output — human formatting may change, JSON is stable.

Safety rules baked into the shipped skill: peer messages are data not
instructions; never send secrets; verify peers via `peers` before messaging;
check `inbox` at task boundaries, not in a tight loop; never bypass `herdr-pool`
with raw `agent prompt`/`send_keys` — that skips scope checks and tracking.

### Two agents messaging (end-to-end)

With the plugin linked/installed and two agent panes open in the same project
folder:

```sh
# Agent A (implementer pane)
herdr-pool whoami                        # → ag_… + scope_key
herdr-pool peers                         # → sees agent B's ag_… id
herdr-pool send ag_B… "heads up: I'm refactoring daemon.rs, avoid editing it"
herdr-pool inbox read msg_…              # later: acknowledge B's reply

# Agent B (reviewer pane) — gets the message delivered as a prompt,
# or pulls it manually:
herdr-pool inbox                         # → the message from A
herdr-pool reply msg_… "ack, I'll review after your refactor lands"
herdr-pool threads                       # → both sides see the th_… summary
```

The router daemon moves messages `pending → delivered → acknowledged`; if B is
busy or `agent_blocked`, delivery retries automatically and the message waits
in the pool.

## Configuration & state

- **State dir resolution** (highest wins): `HERDR_POOL_STATE_DIR` →
  XDG state dir → `~/.local/state/herdr-cross-session-messaging`.
- **Per-scope pools**: `<state>/pools/<scope_key>/pool.db` (SQLite) — one
  pool per canonical project directory.
- **Daemon runtime**: pidfile/lock + logs under `<state>/`;
  `herdr-pool daemon logs` (or `herdr-routerd logs`) prints
  `<state>/logs/routerd.log` and recent lines.
- **Daemon env**: `HERDR_POOL_NOTIFY_DELIVERY=1` toasts the user on every
  successful delivery (off by default — agents already get prompted).
- **Config file**: `<config>/herdr-cross-session-messaging/config.toml`
  (`~/.config/…` by default; `HERDR_POOL_CONFIG_DIR` /
  `HERDR_PLUGIN_CONFIG_DIR` override the directory):

  ```toml
  [bootstrap]
  enabled = false   # stop the auto [herdr-pool] bootstrap prompt to new agents

  [panel]
  autostart = true  # dock the inbox panel on every new tab/workspace
  width = 36        # preferred panel width in columns (24–80, default 34)
  ```

  Env overrides win over the file: `HERDR_POOL_BOOTSTRAP`,
  `HERDR_POOL_PANEL_AUTOSTART`, `HERDR_POOL_PANEL_WIDTH`.
- **CLI overrides**: `--socket-path`, `--state-dir` on any `herdr-pool`
  command; `-v` for debug logging (`RUST_LOG`/`HERDR_LOG` respected).
- **Skill payload**: embedded in the binary at build time
  (`include_str!`); `herdr-pool version` reports CLI + skill payload
  versions, and `doctor` flags mismatches against the plugin manifest.

## Architecture

```
herdr-plugin.toml ──startup──▶ herdr-routerd ──events.subscribe──▶ Herdr socket API
        │                          │                                  │
        │                     delivery worker                    agent.list /
        │                     (agent.prompt,                       session.snapshot /
        │                      retry + dead-letter)                pane.* events
        ▼                          ▼
   herdr-pool CLI  ◀── per-scope SQLite pools ──▶  <state>/pools/<scope_key>/pool.db
        │
   skills/herdr-messaging/SKILL.md → installed into agent skill dirs
```

- **Project-scoped**: scope key = `sha256` of the pane's canonical launch cwd.
  Worktrees and different folders get different pools by construction.
- **Registry**: live agent ↔ `(pane_id, terminal_id, name)` mapping rebuilt
  from `session.snapshot` + `pane.agent_detected`/`pane.moved`/`pane.exited`
  events. Targets are re-validated at send time — never cached blindly.
- **Delivery**: `agent.prompt` with exponential backoff; `agent_blocked` keeps
  the message `pending` until the peer goes `idle`/`working`. Dead-letter after
  max attempts.
- **Sync only**: `std::os::unix::net::UnixStream` + threads. One connection per
  request; only `events.subscribe`/`events.wait`/`agent.wait`/
  `agent.prompt`+wait/`pane.wait_for_output` hold a connection open.

Workspace layout: `crates/herdr-protocol` (NDJSON client + types),
`crates/herdr-pool-store` (SQLite pools/registry), `crates/herdr-pool-core`
(scope + registry logic), `crates/herdr-plugin` (manifest, skill installer,
paths), `crates/herdr-router` (daemon loop + delivery + rescue),
`crates/herdr-pool-cli` and `crates/herdr-routerd` (the two binaries),
`skills/herdr-messaging/` (agent skill payload).

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `herdr-pool: command not found` | CLI not installed / not on PATH | `cargo install --path crates/herdr-pool-cli` or build workspace |
| `herdr-routerd is not running` | Daemon never started | `herdr-pool daemon start` (or `herdr-routerd start`); check the plugin `[[startup]]` hook ran |
| Daemon misbehaving / need logs | — | `herdr-pool daemon logs` prints `<state>/logs/routerd.log` path + recent lines |
| `doctor` fails a check | Env / socket / scope / DB / daemon / skill issue | Run `herdr-pool doctor --json`; each check reports `{name, status, detail}` |
| Messages stuck `pending` | Peer is `blocked` or `working` | Wait for `idle`; daemon retries automatically. Don't spam `send` |
| `peer not found` / `peer ambiguous` | Peer exited, renamed, or name not unique | Re-run `peers`; prefer `ag_*` ids |
| `peer '<name>' (<id>) is offline` | Peer registered but not live | Wait for it to come back, or pick another peer |
| `scope unresolved: …` | Pane cwd / agent info can't yield a project scope | Check the pane was launched in the project dir; run `doctor` |
| `cross-scope denied: …` | You and target are in different project dirs | By design — no cross-scope messaging; run both agents in the same folder |
| `agent_blocked` on delivery | Recipient agent is permission-blocked | Message stays queued; peer (or user) must clear the block |
| Daemon won't start, lock error | Stale pidfile or second instance | `herdr-routerd status`; remove stale pid under `<state>/` if crashed |
| `HERDR_ENV` unset in your shell | You're not inside a managed Herdr pane | Messaging only works inside Herdr panes |
| Agents never see the skill | Daemon hasn't run since install, or a user-owned dir conflicts | Restart the daemon (`herdr-routerd start` re-installs globally); run `herdr-pool install` manually; `doctor` reports skill-dir status |

## Limitations

- **Pre-release (0.1.0)** — the full MVP surface is implemented, but the wire
  protocol and CLI are still stabilizing; expect breaking changes before 1.0.
- **Broadcast is user-gated** — `herdr-pool broadcast` exists but the shipped
  skill restricts it to explicit "notify all" requests; normal messaging is
  peer-to-peer, one recipient per `send`/`reply`.
- **No cross-machine** — everything is local to one Herdr server / Unix socket.
- **No LLM classification, no orchestration** — this is a message pipe, not a
  manager. Routing decisions stay with the agents and the user.
- **Unix only for now** — Windows named-pipe transport is designed but deferred.
- **Rescue delegates, doesn't decide** — on `agent_blocked` the daemon keeps
  a durable `rescue_jobs` row, snapshots the stuck pane's visible TUI, and
  hands it to an eligible peer in the same scope. The *peer* reads and
  drives the source pane via Herdr (`agent read`/`send-keys`/`wait`); the
  pool never classifies questions, picks answers, or sends keys itself.
  One rescue per helper, no rescue loops (a blocked agent is never a
  helper), jobs queue when no helper exists, and stale jobs escalate to
  `needs_human` with a toast.
- **Scope = pane launch cwd** by default — agents launched in a
  subdirectory get their own scope. Set `HERDR_POOL_SCOPE_MODE=git` to scope
  by the nearest git toplevel instead, so subdirectories of one repo share a
  pool (agents outside any repo still scope by launch cwd).
- **Pane auto-label fills empty labels only** — on registration (and pane
  moves) the daemon sets the pane label to the agent's name when the pane
  has none; user- or agent-set labels are never overwritten. Disable with
  `HERDR_POOL_PANE_LABEL=0`.

## FAQ

**Does this replace `herdr agent prompt`?**
No — `agent.prompt` is the delivery primitive the router uses. `herdr-pool`
adds the layer on top: scope isolation, threads, inbox, tracking. The skill
explicitly forbids agents from using raw Herdr commands peer-to-peer.

**Can agents in different projects message each other?**
No. Scope is derived from the canonical project directory and re-checked on
every delivery. Cross-scope sends are rejected with an error.

**What happens if the recipient is busy?**
The message stays `pending`; the daemon retries with backoff and delivers once
the peer reports `idle`/`working`. After max attempts it dead-letters.

**Is my data sent anywhere?**
No. Everything is local: SQLite under the pool state dir, Unix socket to the
local Herdr server. No network egress.

**How do agents learn about this?**
The plugin ships `skills/herdr-messaging/SKILL.md` — the single source of
truth; `.agents/` and `.claude/` copies are installer output and are not
committed. `herdr-routerd` auto-installs it into the global agent skill dirs
on daemon start; `herdr-pool install [--global] [--project <dir>]` does it
manually. Marker file `.herdr-messaging-managed` prevents overwriting user
edits.

**Why sync Rust instead of async?**
One request per Herdr socket connection, `rusqlite` is sync anyway, and the
daemon's concurrency needs are "one event stream + one delivery worker" —
threads are sufficient and keep the dep tree small.

## Roadmap

- **Phase 2 — blocked-agent rescue**: shipped — durable `rescue_jobs` in
  SQLite survive daemon restarts; the daemon snapshots the blocked pane's
  visible TUI and dispatches a `[HERDR RESCUE REQUEST]` to the best
  eligible peer (idle > done > working; blocked/offline/reserved excluded),
  which drives the source pane via Herdr and verifies it resumed. Multiple
  blocked agents get parallel helpers; no helper → jobs stay `pending`;
  timeout/attempt cap → `needs_human` + toast. Remaining: E2E validation
  across agent TUIs on real Herdr, and a helper→pool "needs human" feedback
  channel (today the job timeout covers it).
- **Phase 3 — inbox UI**: popup pane shipped (`[[panes]] inbox`,
  `herdr plugin pane open --plugin herdr.cross-session-messaging
  --entrypoint inbox`) rendering `herdr-pool inbox --follow`; identity inside
  plugin panes resolves via `HERDR_PLUGIN_CONTEXT_JSON`. Remaining: richer
  interactive UI (read/reply keys) and `notification.show` hooks.
- **Later**: Windows named-pipe transport, cross-machine relays (explicitly
  out of MVP scope).

## Development

```bash
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

Task tracking via [beads](https://github.com/steveyegge/beads) (`br list`);
claim with `br update <id> --claim --actor <name>`.

## License

MIT OR Apache-2.0 — see [LICENSE](LICENSE).
