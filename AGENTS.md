# herdr-cross-session-messaging

Project-scoped peer-to-peer messaging layer for coding agents running inside
Herdr. See `docs/IMPLEMENTATION_PLAN.md` for the full design and `br list` /
`.beads/issues.jsonl` for the task graph.

## Layout

- `crates/herdr-protocol` — sync NDJSON client + types for the Herdr socket API
- `crates/herdr-runtime` — `AgentRuntime` trait + `HerdrRuntime`/`HcRuntime`
  backends + `HERDR_POOL_RUNTIME=herdr|hc` selection (`select.rs`)
- `crates/herdr-pool-store` — SQLite pools/messages/registry (per project scope)
- `crates/herdr-pool-core` — scope resolution + peer registry logic
- `crates/herdr-plugin` — herdr-plugin.toml model, skill installer, paths
- `crates/herdr-router` — daemon: event loop, registry sync, delivery worker
- `crates/herdr-pool-cli` — `herdr-pool` agent-facing CLI
- `crates/herdr-routerd` — router daemon binary
- `crates/herdr-testkit` — `FakeHerdr` scriptable socket server for tests
- `crates/hc-spike` — hc JSON-RPC client spike (superseded by `herdr-runtime/src/hc`)
- `skills/herdr-messaging/` — agent skill payload (SKILL.md)
- `.tmp/` — local reference clones (gitignored): herdr source, herdr-auto-title
- `herdr-api.schema.json`, `research/` — API contract artifacts (generated)

## Commands

```bash
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

## Conventions

- Sync only: `std::os::unix::net::UnixStream` + threads. No tokio/async.
- One socket connection per request; only `events.subscribe`/`events.wait`/
  `agent.wait`/`agent.prompt`+wait/`pane.wait_for_output` hold a connection.
- Crate errors: thiserror in libs; anyhow in binaries. `tracing` for logs
  (`RUST_LOG`/`HERDR_LOG`).
- Beads: `br ready`, `br dep cycles` must stay empty. Claim with
  `br update <id> --claim --actor <name>`; close with proof in `--reason`.
- Never overwrite user-owned skill dirs; marker file
  `.herdr-messaging-managed` tracks installer-owned copies.
- Herdr API quirks: `agent.prompt` fails `agent_blocked` on blocked agents;
  `pane.move` changes `pane_id`; popup panes have no `pane_id`; agent names
  match `[a-z][a-z0-9_-]{0,31}`; subscriptions use dot-names, events/waits
  use snake_case.
- Dual runtime: `HERDR_POOL_RUNTIME=herdr|hc` (default `herdr`) selects the
  backend for CLI + routerd; `HC_CONNECT_CMD` overrides the `hc connect`
  argv. CLI and daemon must agree — the daemon stamps `<state>/runtime` and
  the CLI warns on mismatch. On hc, `handle` == terminal address (`t7` /
  `machine/t7`), `terminal_id` is always `None`, and rescue/pane-labeling
  stay herdr-only (`herdr_client()` is `None`).
