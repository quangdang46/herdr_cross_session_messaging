# herdr-cross-session-messaging

Peer-to-peer messaging layer for coding agents running inside
[Herdr](https://herdr.dev): agents in the same project folder discover each
other, exchange messages in threads, and help each other — without the user
acting as a relay.

**Status: in development.** Design: `docs/IMPLEMENTATION_PLAN.md`.
Task tracking: `br list` (43 beads, milestones M0–M6).

## Architecture (planned)

```
herdr-plugin.toml  →  herdr-routerd (daemon)  →  Herdr socket API
        │                    │
        │              events.subscribe + agent.prompt delivery
        │
   herdr-pool CLI  ←  per-scope SQLite pools (project isolation)
        │
   skills/herdr-messaging/SKILL.md → auto-installed into agent skill dirs
```

- **Project-scoped**: each canonical project folder gets its own message pool;
  agents in different folders can never see or message each other.
- **Herdr is the transport**: discovery via `agent.list`/`session.snapshot`,
  delivery via `agent.prompt`, state via `events.subscribe`.
- **Sync Rust**: no async runtime; `std::os::unix::net::UnixStream` +
  `rusqlite`.

## Build

```bash
cargo check --workspace && cargo test --workspace
```

Rust ≥ 1.92 (workspace `rust-version`).

## Reference material

- `docs/IMPLEMENTATION_PLAN.md` — full design
- `herdr-api.schema.json` — Herdr socket protocol schema (herdr 0.9.5)
- `research/` — extracted schema digests used to write the plan
- `.tmp/herdr` — upstream Herdr source (local clone, gitignored)
