# herdr-pool command reference

Per-subcommand reference for the `herdr-pool` CLI. This file reflects the
planned interface; **`herdr-pool <cmd> --help` is authoritative** — if the two
disagree, follow `--help`.

All commands print human-readable output by default and JSON when passed
`--json`. JSON shapes are stable within a minor version; parse defensively.

## whoami

```sh
herdr-pool whoami [--json]
```

Resolve your own identity via `HERDR_PANE_ID` → agent record.

- Output (JSON): `agent_id` (`ag_…`), `name`, `agent_kind`, `pane_id`,
  `scope_key`, `cwd`, `status`.
- Run this first in every session; cache the result.

## peers

```sh
herdr-pool peers [--json]
```

List agents registered in the same project scope.

- Output (JSON): array of agent records (same fields as `whoami`, minus self
  unless `--include-self` is supported — check `--help`).
- Only these peers are valid `send`/`reply` targets.

## send

```sh
herdr-pool send <peer> <text> [--thread <id>] [--json]
```

Send a message to one peer.

- `<peer>`: `agent_id` (`ag_…`) or a unique agent name/kind within the scope.
- `--thread`: attach to an existing thread id (from `threads` or a previous
  `send` result). Omit to start a new thread.
- Output (JSON): `message_id`, `thread_id`, `status` (`pending`).
- Errors: unknown peer, ambiguous name, cross-scope target (rejected — never
  silently delivered).

## inbox

```sh
herdr-pool inbox [--unread] [--json]
```

List messages addressed to you.

- `--unread`: only unacknowledged messages.
- Output (JSON): array of messages with `id`, `thread_id`, `from`, `kind`,
  `content`, `status`, `created_at`.
- Reading via `inbox` does not acknowledge; use `message read` or `reply`.

## reply

```sh
herdr-pool reply <message-id|thread> <text> [--json]
```

Reply within a thread.

- First arg: a `message_id` (reply to that message's thread) or a `thread_id`.
- Output (JSON): `message_id`, `thread_id`, `status`.

## threads

```sh
herdr-pool threads [--json]
```

List threads in your scope with subjects and last-activity timestamps.

- Output (JSON): array of `{thread_id, subject, created_at, last_activity_at,
  message_count}` (exact fields per `--help`).

## message read

```sh
herdr-pool message read <id> [--json]
```

Mark a message as acknowledged/read.

- `<id>`: message id from `inbox`/`send`/`reply`.
- Output (JSON): the updated message record with `status: acknowledged`.

## daemon

```sh
herdr-pool daemon [--foreground] [--json]
```

Run the router daemon (`herdr-routerd`) in the foreground, or manage the
background instance.

- Normally spawned automatically by the plugin `[[startup]]` hook or on demand
  via `ensure_daemon()`; you rarely need this directly.
- Single-instance enforced via pidfile/lock under the plugin state dir.
- Use `herdr-pool doctor` to check daemon liveness instead of starting a
  second copy.

## doctor

```sh
herdr-pool doctor [--json]
```

Read-only health report: `HERDR_ENV`/pane env present, Herdr socket reachable
(`ping`), project scope resolved, pool db migrated, daemon alive, skill dirs
detected, CLI/manifest version match.

- Output (JSON): per-check `{name, status, detail}` entries; non-zero exit on
  failure. Relay failing checks to the user verbatim.

## install

```sh
herdr-pool install [--global] [--json]
```

Install the `herdr-messaging` agent skill into per-agent skill directories.

- Default (project): `.agents/skills/herdr-messaging/` (Codex, Gemini,
  OpenCode, Cursor) and `.claude/skills/herdr-messaging/` (Claude) under the
  project root.
- `--global`: `~/.agents/skills/…` and `~/.claude/skills/…` instead.
- Never overwrites user-modified directories; conflicts are reported, not
  clobbered. Marker file `.herdr-messaging-managed` tracks ownership.

## uninstall

```sh
herdr-pool uninstall [--global] [--json]
```

Remove only skill directories still carrying the managed marker; user-owned
files are left untouched.

## Notes for agents

- Prefer `--json` whenever you will parse output; the human format may change.
- All commands are scope-local by construction; there is no flag to escape the
  project scope.
- If a command fails with an environment error, run `herdr-pool doctor` before
  retrying.
