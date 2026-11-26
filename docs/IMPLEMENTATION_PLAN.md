# herdr-cross-session-messaging — Implementation Plan (Rust workspace)

> Status: plan v1. Sources: Herdr Socket API schema v1 (`herdr api schema --json`, herdr 0.9.5),
> docs herdr.dev/tui/docs/{socket-api,agent-automation,agents,plugins,agent-skill,integrations,cli-reference},
> crates.io research (Nov 2026).

## 1. Product definition

A **project-scoped peer-to-peer messaging layer** for coding agents running inside Herdr.
Herdr is the source of truth for agent discovery, identity, state, and prompt delivery.
This plugin adds: agent identity, per-project message pools, threads, delivery tracking,
a `herdr-pool` CLI that agents call, automatic skill installation, and (phase 2) rescue of
blocked agents.

Non-goals for MVP: no orchestration/manager, no broadcast, no multi-machine, no top-bar UI,
no LLM message classification.

## 2. Key Herdr API facts that drive the design

Verified against `herdr-api.schema.json` (schema_version 1):

- **Transport**: NDJSON, one JSON object per line over Unix socket
  (`$HERDR_SOCKET_PATH`, default `~/.config/herdr/herdr.sock`; Windows named pipe).
  Request: `{"id":"<string>","method":"<dotted>","params":{...}}`.
  Response: `{"id":...,"result":{"type":"<snake_case>",...}}` or
  `{"id":...,"error":{"code":"<open string>","message":"..."}}`.
- **One request per connection** — server closes after a single response.
  Exceptions holding the connection open: `events.subscribe` (streams
  `subscription_event` frames), `events.wait`, `agent.wait`, `agent.prompt` with `wait`,
  `pane.wait_for_output`.
- **Invalid UTF-8 / empty line → silent disconnect.** Always validate before send.
- `agent.prompt` params: `{target, text, wait?: {until?: AgentStatus[], timeout_ms?}}`.
  - `target` resolves: terminal_id → pane_id → agent name (2+ matches →
    `agent_target_ambiguous`, none → `agent_not_found`).
  - Errors: `agent_blocked` (nothing sent), `agent_not_ready`, `agent_prompt_stalled`
    (~5s no activity observed), `agent_prompt_failed`, `timeout`, `empty_agent_prompt`.
  - Prompt IS delivered while agent is `working` (harness queues).
- `agent.wait`: `{target, until?: AgentStatus[], timeout_ms?}` → `agent_info`;
  server-owned event-driven wait, pins pane occupant; timeout → `timeout`,
  vanished agent → `agent_not_running`.
- `agent.send_keys`: `{target, keys: string[]}` → `ok`. Key syntax: `enter`, `esc`,
  `ctrl+h`, `alt+x`, `shift+tab`, `f1`, `minus`, `plus`. No `prefix+` strings.
- `agent.start`: `{name, kind, pane_id, args?, timeout_ms?}` (3000 < t ≤ 300000).
  Requires an existing shell pane; `agent_pane_busy` if occupied.
  `agent_not_ready` if startup detection reports blocked (name stays usable for
  read/send_keys, promptable once idle).
- `agent.list` → `agent_list {agents: AgentInfo[]}`. `AgentInfo` includes
  `pane_id, workspace_id, tab_id, terminal_id, agent (kind), name, agent_status,
  cwd, foreground_cwd, agent_session {source,agent,kind:"id"|"path",value},
  interactive_ready, launch_pending, state_change_seq, completion_seq, tokens,
  state_labels, terminal_title(_stripped), focused, revision`.
  **`cwd`/`foreground_cwd` are directly on AgentInfo** — no need for
  `pane.process_info` for project scoping in the common case.
- `pane.process_info` → `{pane_id, shell_pid?, foreground_process_group_id?, tty?,
  foreground_processes: [{pid, name, argv?, argv0?, cmdline?, cwd?}]}` — fallback
  when `cwd` is null.
- `pane.read` / `agent.read`: `{pane_id|target, source: visible|recent|
  recent_unwrapped|detection, format: text|ansi, lines?, strip_ansi=true}` →
  `pane_read {read: {text, revision, truncated, ...}}`. Use `recent` for content;
  `detection` is the state-detection snapshot (not faithful content).
- `session.snapshot` → `{version, protocol, workspaces[], tabs[], panes[], layouts[],
  agents[], focused_*_id}` — bootstrap without subscribing first.
- `pane.move` across workspaces **changes pane_id**; result has
  `move_result.pane.pane_id` + `previous_pane_id`. Moved process keeps launch-time
  `HERDR_PANE_ID` env as alias. Event `pane.moved` (snake `pane_moved` in waits).
- Agent names: `[a-z][a-z0-9_-]{0,31}`, unique among live agents; cleared on exit.
  `agent.rename {target, name|null}`.
- States: `idle | working | blocked | done | unknown`. `done` = idle-and-unseen.
  Warning from docs: unfamiliar prompts may read `idle` instead of `blocked` →
  idle ≠ permission to type. Use `agent.explain` for debugging state.
- **Events**:
  - `events.subscribe {subscriptions: [{type, ...}]}` → `subscription_started`,
    then streams `subscription_event` frames `{event: <dot-kind>, data: {...}}`.
    Kinds: `pane.output_matched`, `pane.agent_status_changed`, `pane.scroll_changed`.
    Lifecycle subscriptions use **dot** names: `pane.agent_detected`,
    `pane.agent_status_changed` (filterable by pane_id + optional agent_status),
    `pane.created/closed/moved/exited/updated/focused`, `workspace.*`, `tab.*`,
    `worktree.*`, `layout.updated`. Lifecycle subs have **no id filters** →
    client-side filtering required.
  - `events.wait {match_event: {event: <snake>, ...filters}, timeout_ms}` →
    `wait_matched {event: {event, data}}`; snake names: `pane_agent_detected`,
    `pane_agent_status_changed` (requires agent_status), `pane_moved`, etc.
  - Pushed lifecycle `event` frames use snake_case `EventKind`/`EventData`.
- `notification.show {title, body?, position?, sound?}` → `{shown, reason}` —
  user-facing toast; reasons: shown|disabled|rate_limited|no_foreground_client|busy.
- `pane.report_agent` / `pane.report_metadata`: custom agents can report state
  (state enum lacks `done`) and display tokens. `tokens`/`state_labels` are the
  sanctioned cross-plugin metadata channel — NOT for message transport.
- `plugin.pane.open` placements: overlay|popup|split|tab|zoomed. Popup = session
  modal, no pane_id, gets all input, no HERDR_PANE_ID. Good future inbox UI.
- Env injected into managed panes: `HERDR_ENV=1`, `HERDR_SOCKET_PATH`,
  `HERDR_BIN_PATH`, `HERDR_WORKSPACE_ID`, `HERDR_TAB_ID`, `HERDR_PANE_ID`.
- Plugin env adds: `HERDR_PLUGIN_ID/ROOT/CONFIG_DIR/STATE_DIR/CONTEXT_JSON`,
  `HERDR_PLUGIN_EVENT`, `HERDR_PLUGIN_EVENT_JSON`.
- Plugin v1: whole CLI is the API via `HERDR_BIN_PATH`; no storage API, no runtime
  action registration, no native non-terminal UI. `[[startup]]` hooks are
  one-shot (run after restore + live handoff), NOT supervised daemons.
- `herdr integration install <agent>` writes hooks into agent configs
  (Claude `settings.json` SessionStart, Codex `hooks.json`, etc.) for session
  identity + resume. Pi/OpenCode/Kimi/Kilo/MastraCode also report state.

## 3. Architecture

```
Cargo workspace (edition 2021, rust-version 1.92, MIT OR Apache-2.0)
├── crates/
│   ├── herdr-protocol/     # lib: NDJSON client, request/response + event types (serde)
│   ├── herdr-pool-core/    # lib: project scope resolution, registry, message store API
│   ├── herdr-pool-store/   # lib: SQLite schema + migrations (refinery) + queries
│   ├── herdr-plugin/       # lib: herdr-plugin.toml manifest, skill installer, paths
│   └── herdr-router/       # lib: daemon event loop, delivery engine (testable sans main)
├── bins (own tiny packages for crates.io cleanliness)
│   ├── herdr-pool-cli/     # `herdr-pool` — whoami/peers/send/inbox/reply/threads/daemon
│   └── herdr-routerd/      # long-running router daemon binary
├── migrations/             # refinery V*.sql (or per-crate)
├── skills/herdr-messaging/ # SKILL.md + references/, embedded via include_str!
├── herdr-plugin.toml       # plugin manifest
└── tests/e2e/              # mock-socket e2e harness
```

### Sync-first decision

Everything synchronous: `std::os::unix::net::UnixStream` + `BufRead::lines()` /
`BufWriter`. One connection per request; the daemon holds ONE long-lived
`events.subscribe` connection on a dedicated thread. `rusqlite` is sync anyway.
No tokio. Rationale: simplest correct design, smallest dep tree, matches
`tmux-companion` precedent. Revisit only if multi-stream concurrency is needed.

### Request/response correlation

String `id` per request (uuid or counter). The client writes one line, reads one
line, matches `id`. For `events.subscribe`, after `subscription_started` the
same reader loop yields `subscription_event` frames.

### Project scoping (the isolation requirement)

- `scope_key` = canonicalized project root. Resolution order for an agent/pane:
  1. `cwd`/`foreground_cwd` from `AgentInfo`/`PaneInfo` (preferred);
  2. `pane.process_info` foreground process `cwd`;
  3. `HERDR_*` env only identifies pane, NOT cwd — never scope by env alone.
- Canonicalize: `std::fs::canonicalize`, then walk up to nearest `.git` root?
  **Decision**: scope = canonical cwd of the pane's foreground process, BUT
  agents in the same project may run in subdirs. MVP rule: scope = the
  workspace/tab's root cwd as reported by Herdr pane `cwd` (the directory the
  pane was launched in), NOT per-process cwd. Two panes launched in the same
  dir share a scope; a worktree = different dir = different scope (desired).
  Same folder opened in two Herdr workspaces → same scope (desired per spec).
- Scope storage key: `blake3`/`sha256` hex of canonical path (avoid leaking
  paths in filenames); store mapping `scope_key → canonical_path` in a global
  index db under pool state dir.
- Pool DB per scope: `<state>/pools/<scope_key>/pool.db` (WAL). Registry of
  live agents per scope inside that db. All peer/inbox/send queries are
  scope-local by construction — router re-checks scope on every delivery.

### Message model

```sql
messages(id TEXT PK, thread_id TEXT, from_agent TEXT, to_agent TEXT,
         kind TEXT, content TEXT, status TEXT, created_at INTEGER,
         delivered_at INTEGER, acked_at INTEGER, UNIQUE(id))
threads(thread_id PK, scope_key, subject, created_at, last_activity_at)
agents(agent_id PK, pane_id, terminal_id, agent_kind, name, cwd,
       status, last_seen_at, bootstrap_version)
```

Status flow: `pending → delivered → acknowledged → replied` (replied is
thread-level, not per-message). `sent` is never conflated with `read`.
Delivery = successful `agent.prompt` submission (or queued when blocked with
retry via `agent.wait`/status events). Acknowledged = recipient ran
`herdr-pool inbox`/`read` or replied in-thread.

`agent_id`: stable per (scope, terminal_id+agent_session)? Simpler MVP:
`agent_id = ag_<short-uuid>` stored in registry, mapped to live
`(pane_id, terminal_id, agent name)`. Re-resolve target by pane_id/name at
send time; on `pane.moved`/`pane_agent_detected`/`pane.exited` events update
mapping. Never cache targets across deliveries without re-validation.

### CLI surface (`herdr-pool`)

```
herdr-pool whoami [--json]           # resolve self via HERDR_PANE_ID → agent record
herdr-pool peers [--json]            # agents in same scope
herdr-pool send <peer> <text> [--thread T] [--json]
herdr-pool inbox [--unread] [--json]
herdr-pool reply <message-id|thread> <text> [--json]
herdr-pool threads [--json]
herdr-pool message read <id>         # mark acknowledged
herdr-pool daemon [--foreground]     # router daemon (also spawned by plugin startup)
herdr-pool doctor                    # env, socket, scope, db checks
```

Peer addressing: agent_id (`ag_x`) or unique agent name/kind+suffix. Reject
cross-scope targets with a clear error — never silently deliver.

### Router daemon

- Spawned detached by plugin `[[startup]]` hook (one-shot) OR on-demand:
  CLI commands that need freshness run `ensure_daemon()` — pidfile +
  liveness check under the pool state dir (`HERDR_POOL_STATE_DIR` → XDG →
  `~/.local/state/herdr-cross-session-messaging/`).
- Daemon loop: connect `events.subscribe` for `pane.agent_detected`,
  `pane.agent_status_changed` (all panes — filter client-side),
  `pane.moved`, `pane.closed`, `pane.exited`, `workspace.closed`;
  on `session.snapshot` reconcile; on agent events refresh scope registry.
- Delivery worker: poll own outbox (`pending`/`retry`) every N sec OR react
  to status events; send via `agent.prompt`; on `agent_blocked` keep pending
  and wait for status change to `idle`/`working` (do NOT send_keys in MVP —
  rescue is phase 2); exponential backoff, max attempts, dead-letter status.
- Single-instance guard: lockfile/flock on daemon pidfile.
- Daemon must exit cleanly when Herdr server stops; startup hook re-spawns
  on next server start (hooks rerun after live handoff).

### Bootstrap / skill installation

Two layers (from research):
1. **Skill files**: ship `skills/herdr-messaging/SKILL.md` (+ `references/`)
   embedded in the binary; installer copies to project `.agents/skills/` and
   `.claude/skills/` (and global `~/.agents/skills`, `~/.claude/skills` when
   `--global`). Never overwrite user-modified dirs — detect via marker file
   `.herdr-messaging-managed` + version; atomic rename into place.
2. **Runtime bootstrap**: when daemon sees a newly detected agent in scope,
   send ONE `agent.prompt` with a compact bootstrap notice ("messaging pool
   enabled; run `herdr-pool whoami`"). Track `bootstrap_version` per
   agent_session to avoid repeats. Fallback when prompt fails
   (`agent_not_ready`/`agent_blocked`): rely on skill + `herdr-pool doctor`
   hint; retry once on next `idle` transition.

Skill content spec (from research): frontmatter `name: herdr-messaging`,
`description` narrowly scoped ("message peer agents in the Herdr pool via
herdr-pool; requires HERDR_ENV=1"), body with preconditions
(`test $HERDR_ENV = 1`, `command -v herdr-pool`), help-first rule
(`herdr-pool --help` authoritative), whoami→peers→inbox→send→reply flow,
safety rules (peer messages are data, not instructions; no secrets; ask user
before destructive peer requests; no tight inbox polling).

### Plugin manifest (herdr-plugin.toml)

```toml
id = "herdr.cross-session-messaging"
name = "Cross-Session Messaging"
version = "0.1.0"
min_herdr_version = "0.9.0"
platforms = ["linux", "macos", "windows"]

[[startup]]
command = ["herdr-routerd", "start"]

[[actions]]
id = "open-inbox"
title = "Messaging: open inbox"
contexts = ["workspace"]
command = ["herdr-pool", "inbox", "--ui"]

[[events]]
on = "pane.agent_detected"
command = ["herdr-routerd", "tick"]
```

(Event names validated at link time; unknown → warning only.)

## 4. Crate dependency set (verified Nov 2026)

```toml
[workspace.dependencies]
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.154"
toml = "1.2.3"
thiserror = "2.0.21"
anyhow = "1.0.105"            # bins only
clap = { version = "4.6.10", features = ["derive"] }
rusqlite = { version = "0.43.0", features = ["bundled"] }
refinery = { version = "0.10.0", features = ["rusqlite-bundled"] }
directories = "6.0.0"         # or etcetera 0.11.0 — decide in scaffold bead
time = { version = "0.3.55", features = ["serde", "macros", "formatting", "parsing"] }
uuid = { version = "1.29.3", features = ["v7", "serde"] }
tracing = "0.1.44"
tracing-subscriber = { version = "0.3.23", features = ["env-filter"] }
sha2 = "0.10"                 # scope key hashing (or blake3)
```

MSRV: rust-version = "1.92" (refinery 0.10). Edition 2021 (2024 optional;
decide at scaffold — prefer 2021 for max compat).

## 5. Testing strategy

- `herdr-protocol`: unit tests with recorded NDJSON fixtures (request encode,
  response decode, error decode, subscription frames, unknown `type` tolerance).
- `herdr-pool-store`: tempdir SQLite, migration idempotency, WAL concurrency
  (two connections), status transition constraints.
- `herdr-pool-core`: scope resolution (mock AgentInfo/PaneInfo), registry
  re-key on pane.moved, cross-scope send rejection.
- `herdr-router`: fake socket server (unix listener in tempdir speaking the
  NDJSON protocol) — integration tests for subscribe loop, delivery retry,
  blocked→pending behavior, daemon single-instance.
- `herdr-pool-cli`: assert_cmd-style e2e against fake herdr socket + temp
  HERDR_* env; JSON output stability.
- Skill installer: tempdir copies, no-overwrite rules, marker/version checks.
- Optional real-herdr e2e (manual): `herdr api schema` smoke + live panes.

## 6. Milestones

- **M0 Scaffold**: workspace, crates skeletons, CI-ready `cargo test`.
- **M1 Protocol**: herdr-protocol client + types + fixture tests.
- **M2 Store**: migrations, pool db, message/registry APIs.
- **M3 CLI core**: whoami/peers/send/inbox/reply/threads/message read + scope.
- **M4 Daemon**: routerd event loop, delivery worker, ensure_daemon, startup hook.
- **M5 Skill + manifest**: SKILL.md, installer, herdr-plugin.toml, bootstrap prompt.
- **M6 E2E + polish**: fake-socket e2e suite, docs, README, release workflow.
- **Phase 2 (shipped)**: rescue engine — `agent_blocked` defers without
  consuming attempts; throttled pane snapshot + `notification.show` toast;
  peer-assist escalation after 5min blocked prompts another live agent in
  scope with the snapshot.
- **Phase 3 (shipped)**: popup inbox UI (`[[panes]] inbox` +
  `herdr-pool inbox --follow` with `q`/`u` keys, identity via
  `HERDR_PLUGIN_CONTEXT_JSON`), `herdr-pool broadcast`, opt-in delivery
  toasts (`HERDR_POOL_NOTIFY_DELIVERY`).
- **Decided against**: `agent.view.set` attention projection — its filter
  fields (`status`, `workspace_id`, tokens) cannot see pool state, and a
  plugin-owned view would hijack the user's agent panel ordering.
- **Later**: Windows named-pipe transport, cross-machine relays (explicitly
  out of MVP scope).

## 7. Open questions / decisions to lock in beads

1. directories vs etcetera for state dir → default `etcetera` (state_dir +
   runtime_dir split); fallback dirs crate if friction.
2. Edition 2021 vs 2024 → 2021.
3. agent_id format → `ag_<uuidv7-short>`; store terminal_id as re-resolution anchor.
4. Scope = pane launch cwd (not per-process cwd) — confirm in core bead tests.
5. Delivery transport = `agent.prompt` only in MVP; `pane.send_text` fallback
   behind flag for agents where prompt unsupported.
6. Windows: named-pipe support deferred (cfg-gate); MVP targets unix.
