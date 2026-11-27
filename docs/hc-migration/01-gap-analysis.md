# hc / herdr-core capability gap analysis

> Bead: `herdr_cross_session_messaging-hc-gap-analysis-jo3`
> Verified against live `hc 0.1.17` (`hc schema`, `hc connect --json`) on 2026-11-27,
> and the herdr TUI socket API schema v1 used by this repo (herdr 0.9.5).

## 1. Scope

This document inventories every Herdr TUI socket API call the pool actually
makes in production code (not the full ~100-method schema — only call sites in
`crates/herdr-protocol/src/client.rs` consumers across `herdr-pool-cli`,
`herdr-router`, `herdr-pool-core`), and maps each to the hc OpenRPC surface.

hc API facts verified live:

- Transport: `hc connect --json` = newline-delimited JSON-RPC over
  stdin/stdout (`hello` → `framed:false`, `generation:1`). Also reachable over
  `ssh <box> hc connect`. A persistent subprocess gives one connection for all
  calls — same shape as our per-request Unix socket, but multiplexed.
- Methods: `hello`, `server.status|stop|restart`, `terminal.run|list|get|
  kill|rm|keep|label|read|attach|send|wait|keys|interrupt|put`,
  `machine.exec|list|add|connect|rename|rm`, `file.read|write|edit`,
  `doc.get|set`, `events.subscribe`, `rpc.discover`.
- Terminal identity: `address` (`t7`, stable per core), `name`, `labels{k=v}`,
  `parent` (spawning terminal), `cwd`, `argv`, `pid`, `state`
  (`live|finished`), `status` (program-reported 7501 record:
  `state: idle|working|done|blocked|error`, `kind`, `msg`, `progress`),
  `turn`, `foreground{name,argv,pid,cwd}`.
- Remote addressing is native: `machine/t7` works in `terminal.*` params and
  `events.subscribe` returns terminals "here and on connected machines".
- `terminal.send` params: `terminal`, `text`, `caller`, `if` (idle|working
  gate), `force`, `wait`. Result: `{started: bool|null, status, turn, wait?}`
  — `started` reports whether the program went `working` within a few seconds.
- `terminal.wait` reasons: `status`, `exit`, `output`, `timeout`,
  `not_started`, `message`, `children`. Supports `children: any|all`.
- Events stream as JSON-RPC notifications `{"method":"event","params":{...}}`
  with per-terminal `seq`; `events.subscribe` returns a snapshot
  `{seq, terminals[]}` first. Raw subscribe appears to require completing the
  `hello` handshake first (verify in spike bead).

## 2. Request-method mapping

| Herdr method (used by) | hc equivalent | Verdict | Semantic delta |
|---|---|---|---|
| `agent.list` (registry_sync, peers, send resolve) | `terminal.list` (+ `labels` filter) | **NATIVE** | Agent kind must be derived: herdr has detection manifests → `agent` kind; hc gives `foreground.name`/`argv` + program-reported `status.app`. Kind detection is heuristic on hc side. |
| `agent.get` (refresh single record) | `terminal.get` | **NATIVE** | Field names differ (`agent_status`→`status.state`, `interactive_ready`/`launch_pending` have no direct equivalent — see §3). |
| `agent.prompt` (delivery worker, send/reply/broadcast, rescue) | `terminal.send` | **WORKAROUND** | Biggest semantic gap. herdr `agent.prompt` returns typed errors `agent_blocked` (nothing sent), `agent_not_ready`, `agent_prompt_stalled`; hc `terminal.send` pastes text + enter and reports `{started,status,turn}` — a blocked agent may still receive text. Blocked/readiness gating must be done by polling `terminal.get`/`status.state` before send, or by `terminal.send(if: "idle")`. `wait` param gives inline turn-end. |
| `agent.wait` (rescue verify, CLI waits) | `terminal.wait` | **NATIVE** | herdr waits on `agent_status` set; hc waits on program status word / exit / output / timeout / `not_started` after send / `message`. Reason strings differ; map carefully. `children` mode is a bonus (parent→child waits). |
| `agent.read` (rescue capture, delivery liveness) | `terminal.read` | **NATIVE** | herdr has `source: visible|recent|detection` + `format: text|ansi`; hc `terminal.read` returns screen+history text (`lines` param, `complete`, `how`). No `detection` source — state detection is program-reported `status`, not screen scraping. Simpler but less faithful for agents that don't report 7501 status. |
| `agent.send_keys` (rescue unblock: esc/enter/ctrl+c) | `terminal.keys` | **NATIVE** | Key names differ slightly (`enter`, `esc`, `ctrl+c` — verify exact vocabulary in spike). `terminal.interrupt` is a nicer "press stop key" primitive herdr lacks. |
| `pane.current` (self identity, identity.rs) | — | **MISSING → WORKAROUND** | hc has no "current pane". Inside a terminal, identity = `HC_TERMINAL_ID` env (per `terminal.send` docs) or `caller` param inferred from parent process. Self-resolution must switch to env-based lookup (`HC_TERMINAL_ID` → `terminal.get`) instead of `HERDR_PANE_ID` → `pane.current`. |
| `pane.process_info` (cwd fallback for scope) | `terminal.get` → `cwd`, `foreground.cwd`, `argv` | **NATIVE** | Simpler: `cwd` is directly on Terminal; `foreground.cwd` covers cd-within-shell. `pane.process_info`'s process-tree detail has no equivalent (not needed — we only used it for cwd). |
| `pane.read` / `pane.send_text` / `pane.send_keys` (rescue/plugin extras) | `terminal.read` / `terminal.send` / `terminal.keys` | **NATIVE** | Same mapping as agent.* — hc has one terminal primitive, no pane/agent split. |
| `pane.wait_for_output` | `terminal.wait` (reason `output`) | **WORKAROUND** | hc wait `output` reason exists; confirm param shape (`--output X` in CLI) supports substring matching in spike. |
| `session.snapshot` (registry_sync bootstrap) | `terminal.list` (+ `events.subscribe` snapshot) | **WORKAROUND** | No single snapshot RPC; `events.subscribe` returns `{seq, terminals[]}` which IS a consistent snapshot+stream — arguably better (seq gives resume semantics). |
| `events.subscribe` (daemon event loop) | `events.subscribe` | **NATIVE** | Different event model: hc emits terminal-scoped notifications (`created/updated/finished`, status changes) with `machine/t7` addresses — flat, no workspace/tab tree. Registry sync gets simpler; pane.moved/tab.*/workspace.* events have no equivalent (see §3). |
| `events.wait` | `terminal.wait` / next event on subscribe connection | **WORKAROUND** | No generic event-wait RPC; per-terminal `terminal.wait` covers the use case (rescue verify). |
| `notification.show` (toasts: delivery notice, rescue needs-human, CLI) | — | **MISSING** | No notification/toast API in hc. Options: drop toasts on hc backend, write to `herdr-pool` stdout/log, or keep a minimal herdr-TUI dependency just for toast (ugly). Decide in RFC — likely accept degradation v1. |
| `workspace.list` (1 call site) | — | **MISSING** | No workspace concept. Scope must come from `cwd` + labels only. |

Unused-by-pool methods (`agent.start`, `agent.explain`, `agent.rename`,
`agent.focus`, `server.*`, `worktree.*`, `tab.*`, `layout.*`, `command.invoke`,
client window/title APIs, plugin/integration APIs) need no mapping — the pool
does not call them. If future features need agent spawn, `terminal.run` +
`terminal.label` covers it (and is arguably better: no pane-first dance).

## 3. Model-level differences (drive the runtime trait)

1. **Hierarchy vs flat.** Herdr: `workspace > tab > pane > agent`.
   hc: flat `terminal` with `address`, `labels`, `parent`, `cwd`.
   → Pool `scope_key` today derives from cwd (already runtime-agnostic).
   Workspace/tab identity was only used for paneId handles and events; on hc
   the peer handle becomes `t7`/`machine/t7` or a label/name.
2. **Agent identity.** herdr `AgentRecord` fields → hc sources:
   - `pane_id` → `address` (`t7`) — stable, but **per-core**: two machines can
     both have `t7`; always qualify with machine for remote (`mac/t7`).
   - `terminal_id` (`term_*`) → no equivalent; drop or map to `pid`.
   - `agent_id` (`ag_*`) → stays ours (pool registry), unchanged.
   - `name` → `name` or a `herdr-pool.name` label (labels are the clean
     mechanism — recommend reserving `herdr-pool.*` label namespace).
   - `agent_kind` → heuristic from `foreground.name`/`argv`/`status.app`
     (hc has no manifest-based detection; programs *may* self-report via
     7501 status records — coverage unknown per CLI).
   - `agent_status` → `status.state` (`idle|working|done|blocked|error`) +
     `state` (`live|finished`). **Blocked is only visible if the agent
     program reports it** — claude/codex 7501 support must be verified
     per-kind; fallback = screen heuristics (out of scope v1).
   - `interactive_ready` / `launch_pending` → **no equivalent**. hc
     `terminal.run` returns once spawned; "ready" ≈ first `status` report or
     `state=live` + `reported=true`. Delivery gating must be redefined
     (probably: send anyway + rely on `started`/`turn` + wait reason
     `not_started`).
   - `agent_session` (resume identity) → no equivalent; `terminal.keep`
     replaces-lost-programs is the closest lifecycle feature.
3. **Caller identity.** herdr: `HERDR_PANE_ID`/`HERDR_WORKSPACE_ID`/
   `HERDR_TAB_ID` env → `pane.current`. hc: `HC_TERMINAL_ID` env inside
   terminals + `caller` param on send/wait. → `context.rs` env detection
   needs an hc branch.
4. **Multi-machine.** herdr API is single-machine. hc `machine/t7` +
   `machine.*` methods + `events.subscribe` spanning machines = cross-machine
   messaging almost for free (scope must then include machine identity —
   design decision for RFC; HerdCluster remains the fleet layer).
5. **Connection model.** herdr: one TCP/unix connection per request.
   hc `connect`: one persistent stdio process, multiplexed requests +
   notifications. Sync client maps cleanly (request/response by `id`,
   reader thread demuxes notifications) — see spike bead.
6. **Events granularity.** herdr events are pane/tab/workspace-scoped;
   hc events are terminal-scoped with `seq` resume. Registry sync simplifies
   (no moved/rekey across tab/workspace), but `pane.moved`-style pane_id
   churn disappears too — `t7` is stable, `machine/t7` globally qualified.

## 4. Feature-level gaps & recommendations

| Feature | Status on hc | Recommendation |
|---|---|---|
| Toasts (`notification.show`) | MISSING | Degrade gracefully: log + optional stderr note; revisit if hc adds notify API. |
| Workspace/tab scoping | MISSING | Scope = cwd (+ optional `herdr-pool.scope` label for overrides). Already the primary mechanism. |
| Blocked-agent rescue flow | PARTIAL | `status.state=blocked` if agent reports 7501; else detect via `terminal.read` heuristics or `terminal.wait` timeouts. Rescue dispatch itself (`terminal.send`/`keys`) works. |
| Peer handles in envelopes | WORKAROUND | Use `address` (`t7`, or `machine/t7` remote) — arguably better than pane_id (globally unique). Update `agent_handle()` on hc backend. |
| Skill/plugin install | OK | herdr-plugin skill installer is filesystem-based; hc has no plugin API but doesn't need one — `herdr-pool` CLI + skill files work identically. Bootstrap prompt delivery goes through `terminal.send`. |
| `herdr-pool update` self-update | OK | Unchanged (binary self-update). |
| Cross-machine pools | NEW CAPABILITY | `machine/t7` addressing makes scope=`cwd+machine` feasible; defer to post-MVP, coordinate with HerdCluster assessment. |

## 5. What the runtime trait must abstract

For the RFC (`hc-migration-rfc`), the trait surface implied by this table:

```text
list_agents()            -> Vec<AgentRecord>     (terminal.list)
get_agent(handle)        -> AgentRecord          (terminal.get)
send_prompt(handle,text) -> SendOutcome{started,status,turn}  (terminal.send)
wait_agent(handle,...)   -> WaitOutcome          (terminal.wait)
read_agent(handle,...)   -> String               (terminal.read)
send_keys(handle,keys)   -> ()                    (terminal.keys)
current_identity()       -> SelfIdentity          (env: HC_TERMINAL_ID vs HERDR_*)
subscribe_events()       -> EventStream           (events.subscribe; terminal-scoped)
notify(title,body)       -> ()                    (herdr: notification.show; hc: no-op v1)
process_cwd(handle)      -> Option<PathBuf>       (terminal.get.cwd / foreground.cwd)
```

`AgentRecord` stays the pool's canonical identity; each backend maps its
native fields into it. `pane_id`-equivalent = `address`; `agent_status`
mapping table lives in the adapter.

## 6. Open questions for the RFC

1. Does `terminal.send` refuse/queue when `status.state=blocked`, or does it
   type anyway? (Affects whether delivery worker needs a pre-send status
   poll — spike should test against a blocked agent.)
2. 7501 status coverage: which agent CLIs (claude, codex, opencode, devin,
   droid, hermes) actually report `status.state`? Without it, `blocked`
   detection and `interactive_ready` gating degrade to heuristics.
3. `events.subscribe` hello-handshake requirement + reconnect/`seq` resume
   semantics (spike bead verifies).
4. Notification strategy on hc backend (drop vs. minimal desktop notify via
   `osascript`/platform tool — probably out of scope).
5. Scope key format when machine-qualified handles exist (`cwd` alone is
   ambiguous across machines with same path).
