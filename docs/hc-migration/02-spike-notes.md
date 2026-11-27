# hc client spike notes — `crates/hc-spike`

> Bead: `herdr_cross_session_messaging-hc-client-spike-q9r`
> Spike binary: `cargo run -p hc-spike --bin hc-spike` — **ALL PASS** against
> live `hc 0.1.17` / herdr-core on 2026-11-27.

## What was proven

A **synchronous** hc/herdr-core client is feasible with `std::process` +
threads only — no tokio, matching this workspace's convention.

`HcClient` (`crates/hc-spike/src/client.rs`, ~330 lines):

- Spawns `hc connect --json` (or any argv — `ssh host hc connect` works for
  remote cores) as a child process; JSON-RPC over stdin/stdout,
  newline-delimited (`hello` → `framed:false`).
- One reader thread demuxes stdout: frames with `id` → pending-request map;
  `{"method":"event"}` notifications → subscriber channels.
- Typed helpers: `hello`, `terminal_list(_labeled)`, `terminal_run`,
  `terminal_send`, `terminal_read`, `terminal_wait`, `terminal_keys`,
  `terminal_kill_rm`, `events_subscribe`, `subscribe_events`.

Verified end-to-end by the spike binary: hello (version+32 methods) →
terminal.list → events.subscribe → terminal.run (`t12`, label
`probe=hc-spike2`) → terminal.read sees `READY-SPIKE` → send/read echo
roundtrip → live `{"method":"event"}` notification for the new terminal →
kill+rm cleanup.

## Protocol findings (correct the earlier assumptions)

1. **`events.subscribe` hijacks the connection.** In plain (non-framed)
   mode the subscribe result says the connection "now carries only
   notifications" — subsequent RPC calls on that connection time out.
   **Rule: dedicate one `hc connect` process to the event stream; keep a
   second for RPC.** (Framed mode with channels/credit may allow multiplexing
   — not explored; plain mode is sufficient.)
2. **`hello` is required before notifications flow.** `hc events` CLI gets
   events; a bare `connect` + `events.subscribe` without `hello` delivered
   none in early probing. The spike always calls `hello` first.
3. **Param names:** `terminal.send`/`read`/`wait`/`keys` take `terminal`
   (not `address`); `terminal.run` takes `argv` (array), `cwd`, `labels`
   (map). Errors are JSON-RPC `error` envelopes (e.g. `-32602 invalid_params`
   with `data.kind`, `-32000` with `data.kind` like `machine_unknown`,
   `live` for rm-on-running-terminal).
4. **Events** arrive as `{"jsonrpc":"2.0","method":"event","params":{type,
   terminal, data, seq}}`; `events.subscribe` first returns a snapshot
   `{seq, terminals[]}` — snapshot+stream in one call, with `seq` resume
   semantics.
5. **Status model:** programs self-report via 7501 records →
   `Terminal.status.state ∈ idle|working|done|blocked|error`;
   `Terminal.state ∈ live|finished`; `Terminal.turn` increments per turn.
   `terminal.send` returns `{started, status, turn}` — `started` = went
   working within a few seconds (null if program doesn't report status).
6. **Identity:** `address` (`t7`) is the stable per-core handle;
   `machine/t7` qualifies remote cores natively. `labels` is the right
   mechanism for pool metadata (recommend `herdr-pool.*` namespace).
   `parent` links spawned terminals; `HC_TERMINAL_ID` env exists inside
   terminals for caller identity.

## Gaps vs herdr socket API (feeds RFC)

- No `notification.show`, no workspace/tab/worktree model, no
  `interactive_ready`/`launch_pending`, no `agent_blocked` typed error on
  send (blocked is a status word, not a send rejection — delivery worker
  must poll status or use `terminal.send(if=...)`).
- Agent "kind" is heuristic (`foreground.name`/`argv`/`status.app`) — no
  manifest detection. 7501 status coverage per agent CLI is an open
  question for the RFC.

## Files

```
crates/hc-spike/
├── Cargo.toml            # serde, serde_json, thiserror only
└── src/
    ├── lib.rs            # HcClient re-export
    ├── client.rs         # process transport + JSON-RPC mux + helpers
    ├── error.rs          # HcError (Transport/Closed/Timeout/Protocol/Api)
    └── bin/hc-spike.rs   # e2e spike scenario (ALL PASS)
```

Note: `crates/hc-spike` is a workspace member (added to root `Cargo.toml`).
It is a spike — the production client belongs behind the runtime trait in
the `hc-adapter` bead, possibly promoted from this code.
