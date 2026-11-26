# herdr-pool command reference

Per-subcommand reference for the `herdr-pool` CLI. This file mirrors
`herdr-pool --help`; **if the two ever disagree, `--help` is authoritative**.

## Global flags

These apply to every subcommand:

```sh
herdr-pool [--json] [--socket-path <PATH>] [--state-dir <DIR>] [-v|--verbose] <COMMAND>
```

- `--json` — machine-readable JSON output (stable schema per command).
- `--socket-path <PATH>` — override `HERDR_SOCKET_PATH`.
- `--state-dir <DIR>` — override `HERDR_POOL_STATE_DIR` / XDG state dir.
- `-v, --verbose` — debug-level logging on stderr.

Output contract: stdout = data, stderr = diagnostics, exit 0 = success,
exit 1 = runtime failure, exit 2 = usage/precondition error.

## whoami

```sh
herdr-pool whoami [--json]
```

Show this pane's agent identity, scope, and pool agent id. Run this first in
every session; cache the result.

JSON (`--json`): `agent_id` (`ag_…`), `pane_id`, `workspace_id`, `tab_id`,
`agent_kind`, `name`, `scope_key`, `canonical_path`, `status`.

## peers

```sh
herdr-pool peers [--json]
```

List other live agents in the same project scope (self excluded).

JSON: `{scope_key, peers: [AgentJson…]}` where each agent has `agent_id`,
`agent_kind`, `name`, `status`, `pane_id`, `terminal_id`, `cwd`, `scope_key`.
Only these peers are valid `send`/`reply` targets.

## send

```sh
herdr-pool send <peer> <text> [--thread <THREAD_ID>] [--json]
```

Send a message to one peer.

- `<peer>`: `ag_*` pool id, pane/terminal id, or an unambiguous agent name.
- `<text>`: one argument; quote multi-word text.
- `--thread <THREAD_ID>`: post into an existing `th_*` thread instead of
  starting a new one.

JSON: `{message: MessageJson}` — includes `id` (`msg_*`), `thread_id`,
`from_agent`, `to_agent`, `kind`, `content`, `status`, `attempt_count`,
`next_retry_at`, `delivered_at`, `acked_at`, `created_at`. Status is
`pending` when delivery is deferred to the router daemon.

Errors: unknown/ambiguous peer, offline peer, self-send, cross-scope target
(rejected — never silently delivered).

## broadcast

```sh
herdr-pool broadcast <text> [--json]
```

Send one message to every live peer in this scope — each recipient gets their
own message and thread, delivered like an individual `send`. Offline agents
are skipped. Use only when the user asks to notify all peers at once; prefer
`send`/`reply` for targeted messages.

JSON: `{recipients: <n>, deliveries: [{agent_id, name, message: MessageJson}…]}`.

## inbox

```sh
herdr-pool inbox [--unread] [--follow] [--json]
herdr-pool inbox read <msg_id> [--json]
```

List messages addressed to this agent.

- `--unread`: only show unread (pending/delivered, not acknowledged) messages.
- `--follow`: redraw the inbox every 2s until killed — used by the plugin's
  popup pane (`herdr plugin pane open --plugin herdr.cross-session-messaging
  --entrypoint inbox`); agents should not run it interactively.
- `inbox read <msg_id>`: mark one message acknowledged and print its full
  content.

JSON: `{messages: [MessageJson…]}`. Listing does not acknowledge; use
`inbox read` or `message read`.

## reply

```sh
herdr-pool reply <target> <text> [--json]
```

Reply within a thread — same thread, addressed to the original sender.

- `<target>`: a `msg_*` id (replies into that message's thread) or a `th_*`
  thread id.
- `<text>`: reply body.

JSON: `{message: MessageJson}`.

## threads

```sh
herdr-pool threads [--json]
```

Per-thread summaries for this scope.

JSON: `{threads: [ThreadSummaryJson…]}` with `thread_id`, `subject`,
`last_message`, `peer_agent_id`, `peer_name`, `peer_agent_kind`,
`unread_count`, `message_count`, `last_activity_at`.

## message

```sh
herdr-pool message read <msg_id> [--json]
herdr-pool message get <msg_id> [--json]
```

Inspect a single message.

- `message read <msg_id>`: mark acknowledged and print full content
  (equivalent to `inbox read`).
- `message get <msg_id>`: print the full `MessageJson` record as JSON
  without changing state.

## daemon

```sh
herdr-pool daemon start|stop|status|logs [--json]
```

Manage the `herdr-routerd` delivery daemon. Equivalent to running
`herdr-routerd <subcommand>` directly; prefer `herdr-routerd` for
`tick`/`run`.

- `start` — spawn the daemon detached (logs to `<state>/logs/routerd.log`).
- `stop` — graceful shutdown (sentinel + SIGTERM).
- `status` — pid / state / uptime.
- `logs` — print the daemon log file path and recent lines.

The daemon is normally spawned by the plugin `[[startup]]` hook; single
instance is enforced via pidfile/lock under the plugin state dir. Use
`herdr-pool doctor` to check liveness instead of starting a second copy.

## doctor

```sh
herdr-pool doctor [--project <DIR>] [--json]
```

Read-only health report. Checks include: `HERDR_*` pane env present, Herdr
socket reachable (`ping`), agent identity resolvable, scope resolved, pool
db migrated, router daemon alive, skill dirs status (project + global),
and version alignment (CLI vs plugin manifest vs skill payload).

- `--project <DIR>`: project root for skill-dir checks (default: agent cwd /
  cwd).

JSON: `{ok, checks: [{name, status, detail}…]}`; non-zero exit on any `fail`.
Relay failing checks to the user verbatim.

## install

```sh
herdr-pool install [--global] [--agent <KIND>] [--project <DIR>] [--json]
```

Install the `herdr-messaging` agent skill into agent skill directories.

- Default (project): `<root>/.agents/skills/herdr-messaging/` (Codex, Gemini,
  OpenCode, Cursor, and other `.agents`-compatible agents) and
  `<root>/.claude/skills/herdr-messaging/` (Claude).
- `--global`: same layout under `~` instead of the project root.
- `--project <DIR>`: project root (default: current directory).
- `--agent <KIND>`: accepted for forward compatibility; the installer covers
  all supported agent skill dirs regardless.

Never overwrites user-modified directories; conflicts are reported, not
clobbered. Marker file `.herdr-messaging-managed` tracks ownership.

## uninstall

```sh
herdr-pool uninstall [--global] [--agent <KIND>] [--project <DIR>] [--json]
```

Remove only skill directories still carrying the managed marker; user-owned
files are left untouched. Flags mirror `install`.

## version

```sh
herdr-pool version [--json]
```

Print version information.

JSON: `{name, version, skill_version}`.

## Notes for agents

- Prefer `--json` whenever you will parse output; the human format may change.
- All commands are scope-local by construction; there is no flag to escape
  the project scope.
- If a command fails with an environment error, run `herdr-pool doctor`
  before retrying.
