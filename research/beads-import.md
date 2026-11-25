# herdr-cross-session-messaging beads

## M0 · Scaffold Cargo workspace [task] [p1]

Create root `Cargo.toml` as a Cargo workspace for the `herdr-cross-session-messaging` project (a Herdr plugin giving coding agents peer-to-peer messaging inside Herdr panes).

Requirements:
- `[workspace]` members: `crates/herdr-protocol`, `crates/herdr-pool-core`, `crates/herdr-pool-store`, `crates/herdr-plugin`, `crates/herdr-router`, plus bin packages `bins/herdr-pool-cli` and `bins/herdr-routerd` (each a tiny package with its own Cargo.toml so they can be published independently, crates.io-style).
- `[workspace.package]`: edition = "2021", rust-version = "1.92" (required by refinery 0.10), license = "MIT OR Apache-2.0", repository/homepage placeholders.
- `[workspace.dependencies]` exactly per docs/IMPLEMENTATION_PLAN.md §4: serde 1.0.229+derive, serde_json 1.0.154, toml 1.2.3, thiserror 2.0.21, anyhow 1.0.105 (bins only), clap 4.6.10+derive, rusqlite 0.43.0+bundled, refinery 0.10.0+rusqlite-bundled, directories 6.0.0 (or etcetera 0.11.0 — decide and document), time 0.3.55 (serde/macros/formatting/parsing), uuid 1.29.3 (v7+serde), tracing 0.1.44, tracing-subscriber 0.3.23+env-filter, sha2 0.10.
- Create directory skeletons with minimal `src/lib.rs` / `src/main.rs` stubs so `cargo metadata`/`cargo check` passes.
- Sync design decision (from plan): NO tokio/async — sync `std::os::unix::net::UnixStream` + threads; rusqlite is sync anyway.

Acceptance: `cargo check --workspace` green; `cargo tree` shows no tokio.

## M0 · Lib crate skeletons [task] [p1]

Create the five library crates with correct manifests and stub modules:

- `crates/herdr-protocol`: NDJSON socket client + Herdr API types (depends only on serde/serde_json/thiserror/tracing/uuid/time).
- `crates/herdr-pool-core`: project scope resolution + agent registry logic (depends on herdr-protocol, herdr-pool-store).
- `crates/herdr-pool-store`: SQLite pool/message/registry storage (depends on rusqlite, refinery, time, uuid, thiserror).
- `crates/herdr-plugin`: herdr-plugin.toml manifest parsing + skill installer + state/config dir resolution (depends on toml, serde, thiserror; herdr-pool-store only if needed — avoid if it creates cycles).
- `crates/herdr-router`: daemon event loop + delivery engine library, testable without main (depends on all above).

Each lib gets: crate-level docs describing responsibility, `#![warn(missing_docs)]` optional, a typed `Error` enum via thiserror, and `pub mod` structure stubs. Bins depend on libs, never the reverse.

Acceptance: all crates compile; dependency direction is acyclic (protocol ← store ← core ← router; plugin standalone-ish).

## M0 · Bin crate skeletons [task] [p1]

Create two bin packages:

- `bins/herdr-pool-cli` → binary name `herdr-pool`. clap derive CLI with global flags `--json`, `--socket-path`, `--state-dir`, `-v/--verbose`; subcommand enum stub for: whoami, peers, send, inbox, reply, threads, message, daemon, doctor, install, uninstall. `main` returns anyhow::Result, init tracing_subscriber with env-filter (HERDR_LOG or RUST_LOG).
- `bins/herdr-routerd` → binary name `herdr-routerd`. Subcommands: `start` (spawn/detach daemon), `stop`, `status`, `tick` (one-shot reconcile used by plugin event hooks), `run` (foreground loop for debugging). Stub implementations returning "not implemented" errors are fine.

Acceptance: `cargo run -p herdr-pool-cli -- --help` and `herdr-routerd --help` print usage; `cargo install --path` dry-run works.

## M0 · Repo hygiene and docs skeleton [task] [p2]

- Update README.md: project one-liner, architecture overview (link docs/IMPLEMENTATION_PLAN.md), build/test commands, status MVP.
- `.gitignore`: `/target`, `*.db`, `.beads/*.db` keep policy per br defaults (check `br config list`), daemon pid/log files.
- `AGENTS.md`: build (`cargo test --workspace`), lint (`cargo clippy --workspace -- -D warnings`, `cargo fmt --check`), beads workflow notes (`br ready`, `br dep cycles` must stay empty).
- Decide and document state dir layout: prefer `etcetera` crate if chosen in scaffold bead, else `directories`; document resolution order: `HERDR_PLUGIN_STATE_DIR` (when plugin-launched) → `HERDR_POOL_STATE_DIR` env → XDG state → `~/.local/state/herdr-cross-session-messaging/`.
- Keep `herdr-api.schema.json` and `research/` in repo as API reference (they are generated/extracted artifacts — note provenance in research/README or file headers).

Acceptance: fresh clone → `cargo test` works; README renders; AGENTS.md accurate.

## M0 · CI workflow [task] [p2]

`.github/workflows/ci.yml`: on push/PR — rust toolchain (stable + MSRV 1.92 job optional), `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`. macOS + ubuntu matrix (Windows can be `continue-on-error` initially since MVP is unix-socket only; cfg-gate named pipe).

Acceptance: workflow file valid; clippy clean on current code.

## M1 · NDJSON transport core [feature] [p1]

In `crates/herdr-protocol`: synchronous client over `std::os::unix::net::UnixStream`.

- `HerdrClient::connect()` resolves socket path: `HERDR_SOCKET_PATH` env → `$XDG_CONFIG_HOME/herdr/herdr.sock` or `~/.config/herdr/herdr.sock` (named sessions: `sessions/<name>/herdr.sock` when `HERDR_SESSION`/`--session` set — support env for now).
- `request(method, params) -> Result<Response>`: serialize `{"id":<uuid or counter>,"method":...,"params":...}`, write line + flush, read one line, verify id echo, decode envelope. One connection per call (server closes after response).
- Long-lived `subscribe()`/`wait()` variants returning a streaming reader for `events.subscribe`, `events.wait`, `agent.wait`, `agent.prompt --wait`, `pane.wait_for_output`.
- Guards: max line length (e.g. 8 MiB) to avoid unbounded `lines()` allocation; reject empty/invalid-UTF8 before write; detect silent disconnect (EOF before response → `Transport(Closed)`).
- Request timeout wrapper (configurable, default ~30s) implemented via reader thread + channel or `set_read_timeout` on the stream (SO_RCVTIMEO works on unix sockets on unix platforms).

Acceptance: unit-testable against a `std::os::unix::net::UnixListener` in tempdir; request/response roundtrip works; oversized/malformed input handled.

## M1 · Protocol envelope and result types [feature] [p1]

Serde types for the wire protocol, derived from `herdr-api.schema.json` (keep `research/success-defs.json`, `research/all-request-defs.json` as reference):

- `Request { id: String, method: String, params: serde_json::Value }` (or typed params per method — prefer typed where cheap).
- `Response` envelope: `id`, `result: Option<ResponseResult>`, `error: Option<ApiError{code,message}>`.
- `ResponseResult`: internally/adjacently tagged enum on `type` (snake_case discriminators: `pong`, `session_snapshot`, `agent_list`, `agent_info`, `agent_prompted`, `agent_started`, `pane_read`, `pane_list`, `pane_info`, `pane_current`, `pane_process_info`, `ok`, `wait_matched`, `subscription_started`, `notification_show`, `workspace_list`, `workspace_info`, `integration_install`, `agent_explain`...). Include `#[serde(other)]`-style fallback or `Unknown { type: String, raw: Value }` variant so new server types don't break the client.
- All structs `#[serde(default)]` + ignore unknown fields for forward compat.

Acceptance: fixture JSON for each known result type round-trips; unknown `type` decodes to fallback variant.

## M1 · Herdr domain types [feature] [p1]

Port the schema `$defs` into Rust structs (snake_case fields matching wire):

- `AgentStatus` enum: idle|working|blocked|done|unknown (serde rename_all snake_case).
- `AgentInfo`: required terminal_id, agent_status, workspace_id, tab_id, pane_id, focused, revision; optional agent, agent_session{source,agent,kind:"id"|"path",value}, completion_seq, cwd, display_agent, foreground_cwd, interactive_ready, launch_pending, name, screen_detection_skipped, state_change_seq, state_labels(map), terminal_title, terminal_title_stripped, title, tokens(map ≤32).
- `PaneInfo`: pane_id, terminal_id, workspace_id, tab_id, focused, agent_status, revision + optional agent/agent_session/cwd/foreground_cwd/label/scroll/state_labels/terminal_title*/title/tokens/restore_error.
- `WorkspaceInfo`, `TabInfo`, `PaneLayoutSnapshot` (+Rect/Pane/Split), `SessionSnapshot` (version, protocol, workspaces, tabs, panes, layouts, agents, focused_*_id).
- `PaneReadResult` {pane_id, workspace_id, tab_id, source, format, text, revision, truncated}.
- `PaneProcessInfo`/`PaneProcessInfoProcess` {pid,name,argv?,argv0?,cmdline?,cwd?}, shell_pid, foreground_process_group_id, tty.
- Enums: `ReadSource` (visible|recent|recent_unwrapped|detection), `ReadFormat` (text|ansi), `AgentSessionRefKind` (id|path), `PaneAgentState` for report_agent (idle|working|blocked|unknown — NO done).

Acceptance: deserializing `research/success-defs.json`-shaped fixtures works; missing optional fields default cleanly.

## M1 · Client methods: server/workspace/agent-read batch [feature] [p1]

Typed client methods on `HerdrClient`:

- `ping()` → Pong {version, protocol, capabilities?}
- `session_snapshot()` → SessionSnapshot
- `workspace_list()`, `workspace_get(id)`
- `agent_list()` → Vec<AgentInfo>; `agent_get(target)`, `agent_explain(target)`, `agent_rename(target, Option<name>)`, `agent_focus(target)`
- `pane_list(workspace_id?)`, `pane_current(caller_pane_id?)`, `pane_get(pane_id)`, `pane_process_info(pane_id?)`, `pane_rename(pane_id,label?)`

Target resolution note: `target` strings resolve terminal_id → pane_id → agent name server-side; ambiguous → `agent_target_ambiguous`, none → `agent_not_found`. Document this in rustdoc.

Acceptance: each method has a fixture round-trip test; error codes surface as `HerdrError::Api`.

## M1 · Client methods: agent control batch [feature] [p1]

- `agent_prompt(target, text, wait: Option<AgentPromptWait{until,timeout_ms}>)` → AgentInfo. MUST surface `agent_blocked` distinctly (typed error variant or code check) — callers depend on it for queueing.
- `agent_wait(target, until, timeout_ms)` → AgentInfo (server-owned wait; timeout → `timeout`, vanished → `agent_not_running`).
- `agent_send_keys(target, keys: &[String])` → ok. Keys syntax: printable, `enter`, `esc`, `ctrl+h`, `alt+x`, `shift+tab`, `f1`, `minus`, `plus`; NO `prefix+` strings.
- `agent_start(name, kind, pane_id, args, timeout_ms)` → {agent, argv}. timeout_ms must be >3000 ≤300000.
- `agent_read(target, source, format, lines, strip_ansi)` → PaneReadResult.
- `pane_read(pane_id, ...)`, `pane_send_text(pane_id,text)`, `pane_send_keys(pane_id,keys)`, `pane_wait_for_output` (if schema supports — check all-request-defs.json).

Acceptance: prompt-with-wait compiles to single request with `wait` object; all error codes unit-tested via fixture error envelopes.

## M1 · Subscription and event clients [feature] [p1]

- `events_subscribe(subscriptions) -> SubscriptionStream`: send `events.subscribe`, expect `subscription_started`, then yield decoded `subscription_event` frames `{event: kind, data}` on the SAME connection. Kinds: `pane.output_matched`, `pane.agent_status_changed`, `pane.scroll_changed`.
- `Subscription` param type: dot-names — lifecycle (`workspace.created`, `pane.created/closed/moved/exited/updated/focused`, `pane.agent_detected`, `tab.*`, `worktree.*`, `layout.updated`) take `{type}` only; `pane.agent_status_changed` adds optional `pane_id`+`agent_status`; `pane.output_matched` adds `pane_id`,`source`,`match{type:substring|regex,value}`,`lines?`,`strip_ansi?`; `pane.scroll_changed` adds `pane_id`. NOTE: lifecycle subs have NO id filters → client-side filtering.
- `events_wait(match_event, timeout_ms)` one-shot; `EventMatch` uses SNAKE names (`pane_agent_status_changed` requires `agent_status`, `pane_moved` requires `pane_id`, etc.) — keep separate types from Subscription to avoid mixing conventions.
- Lifecycle `event` frames pushed on subscribed connections: decode `EventKind`/`EventData` snake_case (workspace_*, tab_*, pane_*, worktree_*, layout_updated, pane_output_changed...).
- Reconnect helper: stream wrapper that re-subscribes + emits a `Resync` marker so callers re-run `session.snapshot` (events are not replayable over snapshot).

Acceptance: fake-server test pushes frames; decoder handles all 3 subscription kinds + lifecycle envelopes; reconnect emits resync.

## M1 · Remaining client methods + error taxonomy [feature] [p1]

- `notification_show(title, body?, position?, sound?)` → {shown, reason}.
- `pane_report_agent(...)` and `pane_report_metadata(...)` — needed only if we later report custom presence; include typed methods anyway (cheap, part of public API surface we may use for heartbeat/status display in phase 2+).
- `integration_install(target)` enum (pi|omp|claude|codex|copilot|devin|droid|kimi|opencode|kilo|hermes|qodercli|qwen|cursor|mastracode|antigravity_cli|grok).
- `agent_wait`/`events_wait` timeouts.
- Central `HerdrError` (thiserror): `Transport(io)`, `Closed`, `Timeout`, `Protocol(serde)`, `Api{code,message}`, plus convenience predicates `is_agent_blocked()`, `is_agent_not_found()`, `is_timeout()`, `is_retryable()`.
- Socket-path resolution unit tests (env override, XDG, default, named session).

Acceptance: `cargo test -p herdr-protocol` covers envelopes, all method param shapes used, error classification.

## M1 · Protocol fixture tests [task] [p1]

Recorded-fixture test suite in `crates/herdr-protocol/tests/`:

- Golden NDJSON request lines for each method (assert exact JSON shape).
- Golden response lines → typed decode (pong, agent_list with 2 agents, agent_prompted, pane_read, session_snapshot minimal, wait_matched, subscription_started + 3 subscription_event kinds, error envelopes: agent_blocked, agent_not_ready, agent_target_ambiguous, agent_not_found, timeout, invalid_params).
- Malformed cases: empty line, invalid UTF-8 (transport-level, document behavior), unknown result `type` → fallback, missing optional fields.
- Property-ish test: serialize→deserialize roundtrip for all domain types.

Acceptance: fixtures live in `tests/fixtures/*.ndjson`; tests are deterministic, no network.

## M2 · Project scope resolution [feature] [p1]

In `crates/herdr-pool-core`:

- `canonicalize_path(path) -> PathBuf` (fs::canonicalize, fallback lexical normalize).
- `scope_key(canonical_path) -> String` = lowercase hex sha256 of canonical path (first 16 bytes → 32 hex chars is fine; document choice). Never put raw paths in DB filenames — pools dir uses scope_key only.
- `ScopeIndex` (global db at `<state>/index.db`): table `scopes(scope_key PK, canonical_path, created_at)`; `register_scope`, `lookup_path`.
- `resolve_scope_for_agent(AgentInfo|PaneInfo, process_info fallback) -> ScopeKey`: prefer `cwd`/`foreground_cwd` on the record → else `pane.process_info` foreground process `cwd` → else error `ScopeUnresolved` (do NOT guess from env vars; HERDR_* identifies pane not cwd).
- MVP scoping rule (from plan §3): scope = the pane's launch cwd (workspace/tab root cwd as reported by Herdr), NOT the agent process's transient subdirectory cwd — verify against real `agent.list` output which field is stable; document decision in code + tests with fixtures.
- Two panes same dir → same scope; worktrees (different dirs) → different scopes; same folder in two Herdr workspaces → same scope.

Acceptance: unit tests with fixture AgentInfo/PaneInfo/process_info cover all resolution branches + canonicalization edge cases (symlinks, trailing slash, `~`).

## M2 · SQLite migrations V1 [feature] [p1]

In `crates/herdr-pool-store/migrations/V1__init.sql` (refinery `embed_migrations!`):

```sql
CREATE TABLE agents(
  agent_id TEXT PRIMARY KEY,          -- ag_<uuidv7-short>
  scope_key TEXT NOT NULL,
  terminal_id TEXT NOT NULL,
  pane_id TEXT NOT NULL,              -- current public pane id (re-keyed on move)
  agent_kind TEXT,
  name TEXT,
  cwd TEXT,
  status TEXT NOT NULL,               -- last seen Herdr agent_status
  bootstrap_version INTEGER NOT NULL DEFAULT 0,
  first_seen_at INTEGER NOT NULL,
  last_seen_at INTEGER NOT NULL,
  offline_at INTEGER
);
CREATE INDEX idx_agents_scope ON agents(scope_key) WHERE offline_at IS NULL;
CREATE UNIQUE INDEX idx_agents_live_pane ON agents(scope_key, pane_id) WHERE offline_at IS NULL;

CREATE TABLE threads(
  thread_id TEXT PRIMARY KEY,         -- th_<uuidv7>
  scope_key TEXT NOT NULL,
  subject TEXT,
  created_at INTEGER NOT NULL,
  last_activity_at INTEGER NOT NULL
);
CREATE INDEX idx_threads_scope ON threads(scope_key, last_activity_at DESC);

CREATE TABLE messages(
  id TEXT PRIMARY KEY,                -- msg_<uuidv7>
  scope_key TEXT NOT NULL,
  thread_id TEXT NOT NULL REFERENCES threads(thread_id),
  from_agent TEXT NOT NULL,
  to_agent TEXT NOT NULL,             -- agent_id or 'broadcast-scope' reserved
  kind TEXT NOT NULL DEFAULT 'message',  -- message|notice|rescue-request(phase2)
  content TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'pending'
    CHECK(status IN ('pending','delivered','acknowledged','failed','dead')),
  attempts INTEGER NOT NULL DEFAULT 0,
  next_retry_at INTEGER,
  created_at INTEGER NOT NULL,
  delivered_at INTEGER,
  acked_at INTEGER
);
CREATE INDEX idx_messages_inbox ON messages(scope_key, to_agent, status, created_at);
CREATE INDEX idx_messages_outbox ON messages(status, next_retry_at) WHERE status='pending';
CREATE INDEX idx_messages_thread ON messages(thread_id, created_at);
```

Acceptance: `PoolStore::open` on tempdir applies migration; re-open is idempotent (refinery history); `PRAGMA journal_mode=WAL`, `busy_timeout=5000`, `foreign_keys=ON` set.

## M2 · PoolStore open/migrate [feature] [p1]

`PoolStore::open(scope_key, state_dir) -> Result<Self>`: resolve `<state>/pools/<scope_key>/pool.db`, `create_dir_all`, open rusqlite Connection, run refinery migrations (embedded), set pragmas (WAL, busy_timeout, foreign_keys, synchronous=NORMAL). Provide `PoolStore::for_scope(scope)` convenience + `ScopeIndex` global db open. `Mutex<Connection>` interior mutability for shared use; document one-connection-per-thread alternative.

Also `state_dir` resolution shared helper (used by daemon, CLI, installer): env overrides → etcetera/directories → XDG fallback. Put in herdr-plugin or a small `herdr-pool-core::paths` module — decide and keep single source.

Acceptance: tempdir open/migrate/reopen test; concurrent readers test (two connections, WAL).

## M2 · MessageStore API [feature] [p1]

Repository layer over messages/threads tables:

- `send_message(from, to, content, thread_id: Option) -> Message` — creates thread if none (subject = first 80 chars), inserts status='pending'.
- `inbox(agent_id, unread_only) -> Vec<Message>` — to_agent=me AND status IN ('pending','delivered') ordered created_at; unread = not acknowledged.
- `get_message(id)`, `get_thread(thread_id) -> {thread, messages}`.
- `list_threads(agent_id) -> Vec<ThreadSummary>` (last activity, peer, unread count).
- `mark_delivered(id)`, `mark_acknowledged(id)` (sets acked_at; thread-level "replied" derived, not stored per message), `mark_failed(id, attempts, next_retry_at)`, `dead_letter(id)`.
- `outbox_pending(now) -> Vec<Message>` for delivery worker; `outbox_due(now)` respecting next_retry_at backoff.
- All writes in transactions; return typed errors.

Acceptance: unit tests on tempdir DB for each transition incl. invalid transitions rejected; inbox scoping (two agents same scope see only own mail; other scope invisible).

## M2 · RegistryStore API [feature] [p1]

Agent registry per scope:

- `upsert_agent(scope, AgentInfo-ish record)` — keyed by agent_id; on conflict update pane_id/name/status/cwd/last_seen; set offline_at=NULL.
- `resolve_peer(scope, query)` — accept `ag_*` agent_id, exact `name`, or unambiguous `agent_kind`; return agent_id or typed errors `PeerNotFound`/`PeerAmbiguous`/`CrossScopeDenied` (query may include scope hint — ALWAYS verify scope match before returning).
- `rekey_pane(scope, old_pane_id, new_pane_id, terminal_id)` — for `pane.moved`; keep agent_id stable.
- `mark_offline(scope, pane_id|terminal_id)` on pane.closed/exited/agent gone.
- `list_peers(scope, exclude_agent_id)` — live agents only.
- `self_record(scope, terminal_id/pane_id)` for whoami.
- `set_bootstrap_version(agent_id, v)`, `needs_bootstrap(agent_id, current_version)`.

Acceptance: tests cover upsert idempotency, pane re-key preserving agent_id, offline marking, cross-scope query rejection.

## M2 · Crate error types + tracing pass [task] [p1]

Sweep all lib crates: consistent thiserror enums (`ProtocolError`, `StoreError`, `CoreError`, `PluginError`, `RouterError`) with `#[from]` conversions; `tracing` spans on socket calls (`herdr.request method=...`), DB ops (`store.query`), daemon loop iterations. No `unwrap`/`expect` in lib code outside tests; `#![deny(clippy::unwrap_used)]` optional per crate policy — document choice.

Acceptance: `cargo clippy --workspace -- -D warnings` clean.

## M3 · CLI skeleton and output layer [feature] [p1]

`herdr-pool` clap derive:

- Globals: `--json` (machine output, stable schema), `--socket-path`, `--state-dir`, `-v`.
- Subcommands: `whoami`, `peers`, `send <peer> <text> [--thread ID]`, `inbox [--unread]`, `reply <msg-or-thread> <text>`, `threads`, `message read <id>`, `message get <id>`, `daemon <start|stop|status|logs>`, `doctor`, `install [--global] [--agent ...]`, `uninstall`, `version`.
- Output helpers: human tables vs `--json` serde structs; exit codes (0 ok, 1 runtime, 2 usage — match herdcluster convention).
- Every command resolves: env context (HERDR_PANE_ID etc.) → scope → store. Commands that need fresh daemon state call `ensure_daemon()` first (see router beads).

Acceptance: `--help` for every subcommand; `--json` output documented; clap `value_parser`/`arg` conflicts handled.

## M3 · whoami [feature] [p1]

`herdr-pool whoami`: read `HERDR_PANE_ID`/`HERDR_WORKSPACE_ID`/`HERDR_TAB_ID` env (error clearly if absent — not inside Herdr); `pane.current`/`agent.get` to resolve AgentInfo; compute scope; upsert self into registry (agent_id stable per terminal_id+agent_session — define: agent_id = `ag_` + first 8 of uuidv5? NO — use stored agent_id keyed by terminal_id; create on first sight); print `{agent_id, pane_id, agent_kind, name, scope_key, status}`.

Also usable non-interactively by scripts (`--json`).

Acceptance: unit test with fake socket returning AgentInfo; integration: two fake panes get distinct agent_ids, same scope.

## M3 · peers [feature] [p1]

`herdr-pool peers [--json]`: resolve own scope → `list_peers` → print table (agent_id, kind, name, status, cwd-short). Refresh registry from `agent.list` first (cheap reconcile) so peers reflect live Herdr state. Never list cross-scope agents — by construction impossible, but assert in test.

Acceptance: fixture test; output stable for scripting.

## M3 · send [feature] [p1]

`herdr-pool send <peer> <text> [--thread ID] [--json]`:

1. Resolve self (whoami path) + scope.
2. `resolve_peer(scope, peer)` — agent_id/name/kind; `CrossScopeDenied` if peer arg looks like foreign scope (defensive; registry already scoped).
3. Insert message status='pending' (+thread).
4. Deliver: `agent.prompt(target=pane_id-or-name, text=formatted)` where formatted text includes a compact header the receiving agent understands, e.g. `[herdr-pool] msg <id> from <sender-kind> (<agent_id>) thread <tid>:\n<content>\nReply: herdr-pool reply <id> "..."`. Decide exact envelope in this bead and mirror it in SKILL.md.
5. On `agent_blocked`/`agent_not_ready` → leave pending, set next_retry_at (delivery worker picks up). On success → `delivered` + delivered_at.
6. Print message id + status.

Acceptance: fake-socket test asserts prompt text format; blocked agent → status stays pending; cross-scope send errors.

## M3 · inbox + message read [feature] [p1]

`herdr-pool inbox [--unread] [--json]`: list messages to self (pending+delivered, not acked if --unread), with from/thread/snippet/time. `herdr-pool message read <id>` / `inbox read <id>`: mark acknowledged + show full content. `message get <id>`: full JSON.

Acceptance: ack transitions persisted; unread filter correct.

## M3 · reply + threads [feature] [p1]

`herdr-pool reply <message-id|thread-id> <text>`: resolve original message → same thread_id, to_agent=original sender; same delivery path as send. `herdr-pool threads [--json]`: per-thread summary (peer, subject/first msg, last activity, unread count).

Acceptance: reply lands in same thread; threads list scoped correctly.

## M3 · doctor [task] [p2]

`herdr-pool doctor`: checks and reports — HERDR_ENV/pane env present, socket path + `ping` ok (version/protocol), scope resolved + canonical path, pool db migrated, daemon pid alive, skill dirs detected per agent, `herdr-pool` version vs plugin manifest version. `--fix` optional later; MVP is read-only report.

## M4 · Daemon lifecycle [feature] [p1]

`herdr-routerd` + `herdr-pool daemon` wrapper:

- Single instance: pidfile `<state>/routerd.pid` + `flock`-style lock (or atomic create + pid liveness check); `start` detaches (double-fork or `setsid`; on unix use `std::process::Command` spawn with stdin/out/err → log file `<state>/logs/routerd.log`); `stop` SIGTERM → graceful shutdown (finish in-flight, close socket); `status` prints pid/state/uptime; `tick` = one-shot reconcile (used by plugin `[[events]]` hooks).
- Daemon env detection: when spawned by plugin startup hook, `HERDR_PLUGIN_STATE_DIR`/`HERDR_PLUGIN_*` present; when spawned by CLI, resolve state dir normally. Document both.
- Graceful degradation: no Herdr socket → daemon idles with backoff, keeps retrying (server may start later); exits cleanly on SIGTERM/SIGINT.

Acceptance: start/status/stop cycle in tempdir; second `start` is no-op or reports running; log file written.

## M4 · Event loop [feature] [p1]

Daemon main loop in `herdr-router`:

- On start: `session.snapshot` → build/refresh registries for ALL scopes present (group agents by resolved scope); then `events.subscribe` for: `pane.agent_detected`, `pane.agent_status_changed` (no pane filter — client-side), `pane.moved`, `pane.closed`, `pane.exited`, `pane.created`, `workspace.closed`, `tab.closed` (choose minimal set; document).
- Frame dispatch: subscription_event kinds + lifecycle event frames → handler functions per event type.
- Reconnect: on socket error/EOF → backoff (1s→30s cap, jitter), re-subscribe, emit internal Resync → re-snapshot.
- Single writer to store: all DB access on daemon thread or Mutex<Connection>; CLI processes have own connections (WAL allows it).

Acceptance: fake-server integration test: connect, snapshot, subscribe, push events, assert handler calls; reconnect test.

## M4 · Registry sync handlers [feature] [p1]

Handlers mapping Herdr events → registry updates:

- `pane.agent_detected` / `pane.agent_status_changed` → resolve scope, upsert agent (status, pane_id, cwd), detect NEW agent (first_seen or agent_session change) → enqueue bootstrap.
- `pane.moved` → `rekey_pane` (old→new pane_id; payload has previous_pane_id + new pane).
- `pane.closed`/`pane.exited`/`workspace.closed`/`tab.closed` → mark_offline for affected agents; cascade workspace close.
- `agent_status` transitions → update stored status; on →idle/working for agents with pending messages, nudge delivery worker (condvar/notify or short poll).
- Periodic safety reconcile (every 60s): `agent.list` diff vs registry → catch missed events.

Acceptance: unit tests per handler with event fixtures; integration: move/exits update DB correctly.

## M4 · Delivery worker [feature] [p1]

Outbox drain inside daemon:

- Loop (tick ~500ms-2s or notify-driven): `outbox_due(now)` → for each message resolve recipient CURRENT pane target (never cache target strings; re-resolve via registry→agent.get if needed) → `agent.prompt` with envelope text.
- `agent_blocked` → keep pending, next_retry_at = now + backoff (e.g. 15s); do NOT send_keys in MVP.
- `agent_not_ready`/`agent_not_found`/`agent_target_ambiguous` → retry with backoff; after max_attempts (e.g. 20) or recipient offline >5min → status='dead' (+ optional notification.show to user? decide: yes, throttled).
- Success → status='delivered', delivered_at.
- `next_retry_at` exponential backoff: base 5s, ×2, cap 5min, jitter.

Acceptance: fake-server tests: happy delivery, blocked→pending→delivered-on-idle, not_found→dead after attempts, backoff schedule unit test.

## M4 · Bootstrap notifier [feature] [p1]

When registry sync detects a NEW live agent in a scope (new terminal_id or new agent_session value):

- Once per agent_session: `agent.prompt` a compact bootstrap message telling the agent messaging is available + first commands (`herdr-pool whoami`, `herdr-pool peers`) + pointer to skill. Store `bootstrap_version` on agent row; bump constant `BOOTSTRAP_VERSION` when text changes.
- If prompt fails (`agent_not_ready`/`agent_blocked`) → retry on next idle transition, max ~3 attempts, then give up silently (skill files are the durable channel).
- Never bootstrap agents outside managed scopes? Decision: bootstrap ANY detected agent that resolves a scope (they're in a project dir by definition). Document.
- Rate-limit: max 1 bootstrap prompt per agent per daemon run.

Acceptance: test: new agent event → exactly one prompt; version bump → re-bootstrap once; blocked → deferred not lost.

## M4 · Plugin startup hook wiring [task] [p1]

- `herdr-routerd start` must work when invoked by Herdr `[[startup]]` hook (env: HERDR_PLUGIN_STATE_DIR etc.) AND standalone. Detect mode by env presence.
- Ensure `herdr-routerd`/`herdr-pool` binaries are locatable: plugin commands run with plugin dir as cwd and PATH may not include cargo bin — document install expectation (cargo install --path or release binaries on PATH); `HERDR_BIN_PATH` is for calling HERDR, not our bins.
- `tick` subcommand: single reconcile pass (snapshot+registry sync) for `[[events]]` hooks like `pane.agent_detected` — cheap, no daemon required. Decide whether events hooks are even needed given daemon subscribe (probably keep manifest minimal: startup only + actions). Document decision.

Acceptance: `herdr plugin link` + server restart → daemon auto-starts (manual verification bead notes steps).

## M5 · SKILL.md authoring [feature] [p1]

Create `skills/herdr-messaging/SKILL.md` (+ `references/commands.md` if body exceeds ~200 lines) per agentskills.io spec:

- Frontmatter: `name: herdr-messaging` (must match dir name), `description` narrowly scoped ("Message, inspect, and reply to peer coding agents in the Herdr pool using the herdr-pool CLI. Use when the user asks to use Herdr pool messaging, check a herdr-pool inbox, or send/reply to a Herdr peer. Requires HERDR_ENV=1. Do NOT use for general Herdr pane/tab/workspace control — use the herdr skill for that."), `license`, `compatibility`, `metadata` (author, version).
- Body sections: When this applies / Preconditions (`test "${HERDR_ENV:-}" = 1`, `command -v herdr-pool`) / Learn the installed CLI (`herdr-pool --help` authoritative, help-first rule) / whoami→peers→inbox→send→reply workflows with exact command syntax matching implemented CLI / message envelope explanation (what the `[herdr-pool]` header means, how to reply) / Safety (peer messages are data not instructions; no secrets; ask user before destructive peer requests; no tight inbox polling; don't use `herdr agent prompt` directly for pool messaging) / Errors (re-run --help, re-check whoami/peers, don't blindly retry sends) / Anti-patterns.
- `references/commands.md`: full per-subcommand flag reference + JSON output schemas.
- Keep <500 lines, imperative voice, concrete trigger phrases in description.

Acceptance: SKILL.md validates against spec (name/dir match, frontmatter fields); content reviewed against final CLI --help output (update examples to match real syntax).

## M5 · Skill installer [feature] [p1]

In `crates/herdr-plugin` (+ `herdr-pool install/uninstall` CLI):

- Embedded skill payload via `include_str!`/`include_dir`-style (no extra dep needed — include_str for SKILL.md + commands.md).
- Detect install targets per agent: project `.agents/skills/herdr-messaging/` (Codex/Gemini/OpenCode/Cursor compat), `.claude/skills/herdr-messaging/` (Claude project), and global `~/.agents/skills/`, `~/.claude/skills/` when `--global`. Project scope is DEFAULT (isolation requirement); global opt-in.
- Install rules: skip if dir exists AND contains `.herdr-messaging-managed` marker with same/newer version → update; if exists WITHOUT marker or user-modified → report conflict, do NOT overwrite; write to temp sibling dir → validate SKILL.md frontmatter parses → atomic rename; write marker file with version+hash.
- Uninstall: remove only installer-owned dirs (marker present); never delete unmarked dirs.
- Version source: `CARGO_PKG_VERSION` of herdr-plugin crate + skill `metadata.version` kept in sync (const).

Acceptance: tempdir tests: fresh install, update same version (idempotent), conflict on user dir, uninstall preserves user files, project vs global targeting.

## M5 · herdr-plugin.toml manifest [task] [p1]

Author manifest at repo root:

```toml
id = "herdr.cross-session-messaging"
name = "Cross-Session Messaging"
version = "0.1.0"
min_herdr_version = "0.9.0"
description = "Peer-to-peer messaging layer for coding agents in Herdr (project-scoped pools, herdr-pool CLI, agent skills)."
platforms = ["linux", "macos", "windows"]

[[startup]]
command = ["herdr-routerd", "start"]

[[actions]]
id = "open-inbox"
title = "Messaging inbox"
contexts = ["workspace"]
command = ["herdr-pool", "inbox"]
```

Validate locally: `herdr plugin link <repo>` → check `plugin.list` warnings empty; `plugin.action.list` shows action. Document `herdr plugin install <github>` path for release. Note: `[[events]]` hooks optional — decide after daemon proves reliable (document choice).

Acceptance: `herdr plugin link` succeeds with zero warnings; action invoke runs `herdr-pool inbox` (needs binaries on PATH — document install step).

## M5 · Integration install guidance [task] [p2]

Document (README + doctor output) recommended `herdr integration install <agent>` per agent kind for session resume/identity (claude, codex, pi, opencode, etc.). Do NOT auto-run installs — user consent. Note which integrations also report state (pi, omp, kimi, opencode, kilo, mastracode) vs screen-detection-only.

## M6 · Fake Herdr server test harness [feature] [p1]

Test-support crate or `tests/common` module: unix `UnixListener` in tempdir speaking NDJSON protocol — scriptable: queue of canned responses per method, ability to push event/subscription frames, record received requests. Reusable across herdr-protocol/router/cli e2e tests. Must implement enough methods: ping, session.snapshot, agent.list/get/prompt/wait/send_keys/read, pane.*, events.subscribe streaming.

Acceptance: harness itself unit-tested; usable from integration tests via `HERDR_SOCKET_PATH` env override.

## M6 · E2E: messaging happy path [feature] [p1]

`tests/e2e/` with fake server + two simulated agents (different HERDR_PANE_ID/terminal_id, same cwd → same scope):

1. `whoami` both → distinct agent_ids, same scope_key.
2. `peers` each sees the other only.
3. `send` A→B → B `inbox` shows message; `message read` acks.
4. `reply` B→A in same thread; `threads` shows one thread two messages.
5. Delivery path: assert fake server received `agent.prompt` with correct target + envelope format.

Acceptance: `cargo test --test e2e` green; JSON outputs asserted.

## M6 · E2E: project isolation [feature] [p1]

Two scopes (different cwd): agents in scope A cannot `peers`/`send` to scope B — send returns CrossScopeDenied/PeerNotFound; inbox empty cross-scope; DB files physically separate (`pools/<key>/pool.db`). Same cwd in two different workspace_ids → SAME scope (verify).

Acceptance: isolation tests green; attempt to forge foreign pane_id target rejected.

## M6 · E2E: lifecycle and delivery edge cases [feature] [p1]

- `pane.moved` event → agent_id stable, new pane_id used for next prompt.
- `pane.agent_status_changed` → blocked: message stays pending; →idle: delivered.
- Agent exit (`pane.exited`) → offline; send to offline peer → pending until max attempts → dead (+notification.show called on fake server if implemented).
- Daemon restart: kill + start → snapshot reconcile rebuilds registry; pending outbox resumes.
- Bootstrap: new agent_detected → exactly one agent.prompt bootstrap; version bump → once more.

Acceptance: all scenarios green against fake server.

## M6 · README + user docs [task] [p1]

README.md final: what it is, architecture diagram (mermaid or ascii), install paths (`herdr plugin link` dev / `herdr plugin install` release / cargo install bins), quickstart (open 2 agents → they message), CLI reference, skill behavior, config/state dirs, troubleshooting (agent_blocked, scope unresolved, daemon logs), roadmap (rescue phase 2, inbox UI phase 3), license. Link IMPLEMENTATION_PLAN.md for internals.

## M6 · Release prep [task] [p2]

- `cargo publish --dry-run` per lib crate; fix metadata (description, keywords, categories).
- Decide bin distribution: crates.io `cargo install herdr-pool-cli` (bin name herdr-pool) + `herdr-routerd`; or GitHub release binaries via cargo-dist (evaluate — keep simple first).
- Version policy: workspace version sync (all crates same x.y.z); CHANGELOG.md started.
- Optional: GitHub release workflow building binaries ( defer if time — mark optional in bead description).

## M0-followup · beads graph hygiene [task] [p2]

After all beads created: run `br dep cycles` (must be empty), `br ready` sanity, `bv --robot-insights` if available; ensure every bead has description + acceptance criteria; group check that MVP beads (M0-M5 core) have no dependency on phase-2 work.
