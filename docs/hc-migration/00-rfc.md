# RFC: hc / herdr-core runtime migration

> Bead: `herdr_cross_session_messaging-hc-migration-rfc-3lx`
> Status: proposal — decisions locked for the beads listed in §7.
> Inputs: `01-gap-analysis.md`, `02-spike-notes.md`, `crates/hc-spike/src/client.rs`,
> `docs/IMPLEMENTATION_PLAN.md`, and the live call sites in
> `herdr-pool-core` (`identity.rs`, `scope.rs`), `herdr-pool-cli` (`context.rs`),
> `herdr-router` (`delivery.rs`, `registry_sync.rs`, `rescue.rs`, `nudge.rs`).

## 0. TL;DR — the decisions

| # | Decision | Choice |
|---|----------|--------|
| a | Runtime abstraction | `AgentRuntime` trait in a new `herdr-runtime` crate; `HerdrRuntime` (wraps `HerdrClient`) is impl #1 |
| b | Backend selection | **Dual-backend via `HERDR_POOL_RUNTIME=herdr|hc`** (default `herdr`) — not a hard switch |
| c | Identity | hc `address` (`t7`, remote `machine/t7`) is the pane-equivalent handle; `ag_*` pool ids unchanged; pool metadata lives in `herdr-pool.*` terminal labels |
| d | Scope | Unchanged: canonical launch `cwd` → `ScopeKey`. Machine-qualified only in scope *membership checks*, not the key (v1: single machine) |
| e | Delivery | Pre-send status poll (`terminal.get` → `status.state`) + `terminal.send(if=…)` gate; `agent_blocked` has no hc equivalent — `blocked` becomes a *deferred* outcome detected from status, not an error |
| f | Rollout | runtime-trait → hc-adapter → identity/scope port → events/registry port → delivery/nudge port → cli/context port → flag → e2e/docs (existing beads, reordered below) |
| g | Open questions | §8 — 7501 coverage per CLI, `terminal.send` blocked behavior, scope across machines |

Rationale in one line each: the trait is cheap because the spike already
proved the transport; dual-backend is required because Herdr TUI is still
the primary host and `notification.show`/rescue UX has no hc equivalent;
`address` is a *better* handle than `pane_id` (stable, globally qualified);
cwd scoping already works unchanged; delivery must compensate for hc having
no typed `agent_blocked`.

---

## 1. Context

The pool today is hard-wired to the Herdr TUI socket API: `HerdrClient`
(NDJSON, one request per unix-socket connection) is constructed in
`Ctx::from_globals`, threaded into `DeliveryWorker`, `RescueManager`,
`NudgeManager`, `RegistrySync`, and every CLI command resolves identity via
`HERDR_*` env → `pane.current` → `agent.get` → scope. The hc spike
(`crates/hc-spike`) proved a synchronous `hc connect --json` JSON-RPC client
works with the same std-only constraint.

This RFC defines the seam that lets the pool run on **either** backend.

## 2. (a) `AgentRuntime` trait surface

New crate `crates/herdr-runtime` (lib only). Sync, `Send + Sync`, object-safe
where practical (`Box<dyn AgentRuntime>` for the CLI/daemon). All methods
return pool-domain types, never backend wire types.

```rust
/// Canonical peer/agent view — the pool's AgentRecord source of truth.
/// Each backend maps native records into this.
pub struct RuntimeAgent {
    /// Pane-equivalent handle: "w1:p1" (herdr) or "t7"/"machine/t7" (hc).
    /// Stable for the record's lifetime; re-resolved every delivery.
    pub handle: String,
    /// Secondary stable id where the backend has one (herdr terminal_id).
    /// None on hc — adapters must not invent one.
    pub terminal_id: Option<String>,
    /// Detected agent kind ("claude", "devin", ...). Heuristic on hc.
    pub kind: Option<String>,
    /// Pool-facing name (herdr agent name / hc label / terminal name).
    pub name: Option<String>,
    /// Launch cwd (authoritative for scope) and live foreground cwd.
    pub cwd: Option<String>,
    pub foreground_cwd: Option<String>,
    /// Mapped status word — pool stores it verbatim in AgentRecord.status.
    pub status: RuntimeStatus,
    /// Backend readiness flags; hc collapses both into `status`/`state`.
    pub interactive_ready: bool,
    pub launch_pending: bool,
    /// Program-reported detail (tokens, titles) — MetadataMap passthrough.
    pub extra: MetadataMap,
}

pub enum RuntimeStatus {
    Idle, Working, Blocked, Done, Unknown, Offline,
}

/// Outcome of a prompt/delivery attempt — replaces herdr's typed errors.
#[derive(Debug)]
pub enum SendOutcome {
    /// Text accepted; `started` = program went working within ~seconds
    /// (hc) / prompt submitted (herdr). `None` = backend can't tell.
    Delivered { started: Option<bool> },
    /// Recipient alive but must not be typed into right now
    /// (herdr agent_blocked / hc status.state=blocked). Caller defers,
    /// does NOT consume an attempt.
    Deferred { reason: DeferReason },
    /// Recipient not promptable: not found, ambiguous, not ready, gone.
    /// Carries a stable machine string for logs/tests.
    Failed { kind: SendFailureKind, message: String },
}

pub enum DeferReason { Blocked, NotReady }

pub trait AgentRuntime {
    /// All live agents/peers visible to this backend (herdr: agent.list;
    /// hc: terminal.list filtered to labeled/status-bearing terminals).
    fn list_agents(&self) -> Result<Vec<RuntimeAgent>, RuntimeError>;

    /// Single record refresh (herdr: agent.get; hc: terminal.get).
    /// `NotFound` when the handle is stale.
    fn get_agent(&self, handle: &str) -> Result<RuntimeAgent, RuntimeError>;

    /// Deliver a prompt/envelope. The ONLY write path — never raw keys.
    fn send_prompt(&self, handle: &str, text: &str) -> Result<SendOutcome, RuntimeError>;

    /// Block until a status/exit/output condition or timeout.
    /// Used by rescue verify and optional CLI waits.
    fn wait_agent(&self, handle: &str, req: WaitRequest) -> Result<WaitOutcome, RuntimeError>;

    /// Screen/history text for snapshots and liveness checks.
    fn read_agent(&self, handle: &str, lines: u32) -> Result<String, RuntimeError>;

    /// Raw keys — rescue unblock path only (esc/enter/ctrl+c).
    fn send_keys(&self, handle: &str, keys: &[String]) -> Result<(), RuntimeError>;

    /// This process's own identity (env detection + record lookup).
    fn self_identity(&self, env: &EnvLookup<'_>) -> Result<SelfIdentity, RuntimeError>;

    /// Subscribe to backend events → normalized `RuntimeEvent`s.
    /// MUST return a snapshot first (registry bootstrap), then a stream.
    /// Caller owns reconnect/backoff.
    fn subscribe_events(&self) -> Result<EventStream, RuntimeError>;

    /// User-facing toast. herdr: notification.show; hc: no-op v1.
    fn notify(&self, title: &str, body: Option<&str>) -> Result<(), RuntimeError> {
        let _ = (title, body);
        Ok(())
    }

    /// Optional capability query — lets callers degrade features
    /// (toasts, workspaces) without `if backend ==` checks.
    fn capabilities(&self) -> RuntimeCaps {
        RuntimeCaps::default()
    }
}
```

Supporting types (`WaitRequest`, `WaitOutcome`, `RuntimeEvent`,
`RuntimeCaps`, `RuntimeError`) live in the same crate. `RuntimeEvent` is a
normalized enum — `AgentSeen`, `AgentStatusChanged{handle,status}`,
`AgentGone`, `HandleRekeyed{old,new}` — so `registry_sync` never parses
backend JSON.

**Why this shape:**

- `SendOutcome` (not `Result<(), HerdrError>`) is the key abstraction: it
  folds herdr's `agent_blocked`/`agent_not_ready`/`agent_prompt_stalled`
  error taxonomy AND hc's `{started,status,turn}` result into one enum the
  delivery worker switches on once.
- `handle` is deliberately backend-opaque. herdr stores `pane_id`
  (`w1:p1`); hc stores `t7`/`machine/t7`. The registry's `pane_id` column
  becomes "runtime handle" — same SQLite schema, new semantics. No column
  rename in v1 (see §5.3).
- `terminal_id` stays `Option` because hc has no equivalent; the store's
  `get_by_terminal_any` lookup simply never matches on hc. Keeping the
  column avoids a store migration for zero benefit.
- `interactive_ready`/`launch_pending` collapse to a single hc notion
  (`state=live` + first `status` report) — the adapter synthesizes them so
  `delivery.rs`'s existing gates keep working untouched.
- `subscribe_events` returns `EventStream { snapshot: Vec<RuntimeAgent>,
  events: mpsc::Receiver<RuntimeEvent> }` — both backends can produce a
  consistent snapshot+stream (herdr `session.snapshot`+
  `events.subscribe`; hc `events.subscribe` `{seq,terminals[]}`+notifications).

**Crate placement:** `herdr-runtime` depends on `herdr-protocol` (for
`HerdrClient`, `AgentInfo`, `MetadataMap`) and defines the trait + types.
`herdr-pool-core`/`herdr-router`/`herdr-pool-cli` depend on
`herdr-runtime`, not `herdr-protocol` directly (except tests). This keeps
`herdr-protocol` purely the herdr wire client.

## 3. (b) Dual-backend flag vs hard switch — **dual-backend**

**Decision: `HERDR_POOL_RUNTIME=herdr|hc`, default `herdr`.**

Why not a hard switch:

1. **Herdr TUI remains the primary host.** hc/herdr-core is a separate
   runtime (different terminal multiplexer model — flat terminals vs
   workspace/tab/pane). Users on herdr 0.9.x without hc installed must keep
   working; defaulting to `herdr` preserves every existing install.
2. **Feature asymmetry is real.** `notification.show` (delivery toasts,
   blocked/rescue notices, dead-letter alerts) and the popup inbox UI have
   no hc equivalent. A hard switch would silently degrade those; a flag
   makes degradation explicit and per-installation.
3. **Migration safety.** The pool DB, `ag_*` ids, threads, and message
   history must survive a user flipping the flag. Dual-backend lets us ship
   the trait + both adapters, dogfood hc on one machine, and flip back on
   regression without data migration.
4. **Cost is bounded.** The flag is one `match` at three construction
   sites (`Ctx::from_globals`, `herdr-routerd` startup, `ensure_daemon`
   propagation). Everything downstream already goes through the trait.

Mechanics:

- `HERDR_POOL_RUNTIME` parsed once in `herdr-runtime::select()` →
  `Arc<dyn AgentRuntime>`. Invalid value → usage error listing valid values.
- `herdr-routerd` reads the same env; `ensure_daemon` propagates it like
  `HERDR_SOCKET_PATH`/`HERDR_POOL_STATE_DIR`.
- CLI and daemon MUST agree on runtime within a scope — a `send` on `hc`
  while the daemon drains on `herdr` would double-deliver. Guard: stamp
  `runtime` into each pool's `meta` table (or `agents` row); on mismatch,
  CLI refuses pool commands with a clear error rather than corrupting
  delivery state. v1 keeps it simple: daemon startup writes
  `runtime=<name>` to `<state>/runtime`; CLI warns (not refuses) on
  mismatch — refusal can tighten later.
- `hc` backend additionally requires `hc` on PATH (or `HC_CONNECT_CMD`
  override, e.g. `ssh host hc connect` for remote cores — future).

## 4. (c) Identity model

### 4.1 Handles

| Concept | herdr backend | hc backend |
|---|---|---|
| Pane-equivalent handle (stored in `agents.pane_id`) | `pane_id` `w1:p1` | `address` `t7`; remote `machine/t7` |
| Stable secondary id | `terminal_id` `term_*` | none (`pid` is ephemeral — do NOT store) |
| Pool agent id | `ag_<uuidv7-short>` — **unchanged, runtime-agnostic** | same |
| Peer display handle (`agent_handle()`) | pane_id → name → ag_* | address → name → ag_* |
| Delivery target | pane_id → name → terminal_id | address → name |

`address` is strictly better than `pane_id`: it is stable for the terminal's
lifetime (no `pane.moved` rekey equivalent — hc terminals don't move), and
`machine/t7` is globally unique where `w1:p1` is only unique per Herdr
instance. The `pane.moved` registry rekey logic simply never fires on hc.

### 4.2 Names and labels

- hc `terminal.name` and `labels` are the metadata channel. Reserve the
  **`herdr-pool.*` label namespace**:
  - `herdr-pool.name` — pool-facing name when the terminal's own name is
    unsuitable (falls back to `terminal.name`, then `address`).
  - `herdr-pool.agent-kind` — optional kind override when the
    `foreground.name`/`argv`/`status.app` heuristic is wrong.
  - `herdr-pool.scope` — optional scope override (escape hatch; §5).
- The adapter reads these labels in `list_agents`/`get_agent` and maps them
  into `RuntimeAgent.name`/`kind`. Nothing else writes `herdr-pool.*`.
- `agent.rename` has no pool-level equivalent need — renaming is cosmetic;
  skip in v1.

### 4.3 Self identity

- herdr: `HERDR_PANE_ID`/`WORKSPACE_ID`/`TAB_ID` (+ plugin-context JSON) →
  `pane.current` → `agent.get` (unchanged).
- hc: `HC_TERMINAL_ID` env inside terminals → `terminal.get` →
  `RuntimeAgent`. There is no "current terminal" RPC and no workspace/tab —
  `SelfEnv` collapses to `{ handle }` on hc. `resolve_self` becomes
  `runtime.self_identity(env)`; the env-parsing half stays in
  `herdr-pool-core` (pure, testable) with a backend branch.
- `upsert_self` ghost-record guard (`from_plugin_context` + no agent + no
  existing record) ports as-is; hc has no plugin panes so the flag is
  simply never set there.

### 4.4 `ag_*` stability

Pool `agent_id` (`ag_<uuidv7-short>`) is minted by the store and survives
runtime flips **only if** the registry can re-anchor the same logical agent
across backends. v1 rule: flipping `HERDR_POOL_RUNTIME` starts a fresh
registry view — old `ag_*` records for the other runtime's handles are
marked offline by the normal sweep (they'll never reappear live). This is
acceptable because a runtime flip implies a different host environment
(herdr panes vs hc terminals are different processes); pretending continuity
would deliver to stale handles. Document in skill/README: "switching
runtime re-keys peers; in-flight threads keep history but live handles
refresh on next registry sync."

## 5. (d) Scope resolution — `scope = cwd`, unchanged

The MVP rule already is "scope = launch cwd, not workspace/tab":
`resolve_scope(cwd, foreground_cwd, process_info)`. On hc this maps
directly:

- `cwd` → `Terminal.cwd` (launch directory — stable)
- `foreground_cwd` → `Terminal.foreground.cwd` (live chdir/subprocess)
- `process_info` fallback → **no hc equivalent** (`pane.process_info` has
  no analog). `terminal.get` always carries `cwd`, so the fallback is
  herdr-only; the trait's `RuntimeAgent` just leaves both fields `None`
  when unknown and `ScopeUnresolved` propagates as today.

**Decision: scope key stays `sha256(canonical_path)[..16]` — do NOT
machine-qualify in v1.** Two machines with the same path would collide,
but cross-machine pools are explicitly post-MVP (HerdCluster is the fleet
layer). When cross-machine lands, the fix is a scope *namespace* prefix
(`machine:<id>:<hash>`), not a different hashing scheme — note it in §8.

Optional escape hatch (cheap, include in hc-adapter bead): if a terminal
carries label `herdr-pool.scope=<path>`, the adapter resolves scope from
that path instead of `cwd`. Useful for daemons/ssh-forwarded terminals
whose cwd lies. Default off; document as advanced.

`ScopeMode::GitRoot` (`HERDR_POOL_SCOPE_MODE=git`) is backend-agnostic
already — it post-processes the canonical path. No change.

## 6. (e) Delivery semantics — the real work

This is where hc differs most and where the RFC must be concrete.

### 6.1 The `agent.prompt` → `terminal.send` mapping

herdr `agent.prompt` is a *semantic* operation: it refuses
(`agent_blocked`, `agent_not_ready`) or submits with typed errors.
hc `terminal.send` is a *keystroke* operation: it pastes text+enter and
reports `{started, status, turn}`. There is no send-time rejection for
"blocked" — a blocked agent's TUI will receive the text.

**Adapter contract for `send_prompt` (hc impl):**

1. `terminal.get(handle)` → inspect `status.state` + `state`:
   - `state=finished` or terminal gone → `SendOutcome::Failed(NotFound)`.
   - `status.state=blocked` → `SendOutcome::Deferred(Blocked)` — **do not
     send**. (Pre-send poll is mandatory; see 6.2.)
   - `status.state` absent (program doesn't report 7501) → proceed; treat
     as unknown, rely on `started`/`turn` after send.
   - `status.state=idle|working|done` → proceed.
2. `terminal.send(handle, text, if="idle")` — use the `if` gate so the core
   itself refuses to inject into a busy/blocked prompt rather than us
   racing the poll. Exact `if` vocabulary (`idle|working` per gap
   analysis) is verified in the adapter bead; the trait doesn't care —
   the adapter picks the strictest gate that still delivers when working
   (herdr queues prompts while `working`; hc `if=idle` alone would
   under-deliver, so likely `if=working` is NOT wanted — decide empirically:
   prefer send-when-not-blocked).
3. Map the result:
   - `{started: true}` → `Delivered{started: Some(true)}`
   - `{started: false}` → `Delivered{started: Some(false)}` — delivered but
     program didn't go working; still counts as delivered (the text is in
     the terminal), but the delivery worker may want a `terminal.wait`
     confirmation for `started:false` — v1: count as delivered, log it.
   - `{started: null}` → `Delivered{started: None}` (no status reporting).
   - JSON-RPC error / transport → `RuntimeError` → retry path (unchanged).

### 6.2 Blocked detection without `agent_blocked`

herdr delivery relies on the `agent_blocked` *error* to defer. hc has no
such error, so the **status poll in step 1 is the detection**. Consequences:

- `DeferReason::Blocked` is produced by the adapter from
  `status.state == "blocked"`, not from an API error. The delivery worker's
  `BLOCKED_BACKOFF` (15s) + rescue-ensure path triggers identically —
  `SendOutcome::Deferred(Blocked)` is the same branch.
- **Coverage caveat:** `status.state=blocked` only exists if the agent
  program self-reports a 7501 record. For CLIs without 7501 support the
  adapter cannot see blocked — mail gets typed into a blocked TUI.
  Mitigation v1: none (accepted degradation, same as any terminal-paster).
  Mitigation v2 (later): `terminal.read` tail heuristic or
  `terminal.wait` timeout → treat "sent but never `started`" as suspect.
  Track per-`agent_kind` 7501 coverage in §8 Q2 and revisit.
- `interactive_ready`/`launch_pending`: hc has neither. The adapter maps
  `state=live` + `status.reported==true` (or first status seen) →
  `interactive_ready=true`, `launch_pending=false`; a terminal with
  `state=live` but no status report yet after N seconds → keep
  `launch_pending=true` so the NOT_READY_BACKOFF gate still applies during
  startup. Simplest honest mapping: `launch_pending = !status.reported` for
  the first ~10s of terminal life (adapter tracks first-seen timestamps in
  memory; on daemon restart it re-derives from `turn`/`created` fields if
  present — verify field availability in adapter bead).

### 6.3 Wait / read / keys

- `agent.wait` → `terminal.wait`: reasons differ (`status`/`exit`/`output`/
  `timeout`/`not_started`/`message`/`children`). The trait's `WaitRequest`
  is reason-agnostic `{handle, until_status?, timeout_ms?, output_match?}`;
  the hc adapter translates `until_status` → status-word wait. Used by
  rescue verify and any CLI `--wait`.
- `agent.read` → `terminal.read`: returns screen+history text; no
  `detection` source — fine, pool only ever used `recent`/`visible`
  content snapshots.
- `agent.send_keys` → `terminal.keys` (+ `terminal.interrupt` as a nicer
  stop primitive the rescue flow can prefer when available — adapter
  capability flag).
- `pane.process_info` → `terminal.get` `cwd`/`foreground.cwd`/`argv` —
  simpler, always populated.

### 6.4 Events / registry sync

herdr events are pane/tab/workspace-scoped and need client-side filtering;
hc events are terminal-scoped with `seq` resume — **simpler**. Map:

- `events.subscribe` snapshot `{seq, terminals[]}` → initial
  `Vec<RuntimeAgent>` (better than herdr: snapshot+stream in one call,
  `seq` gives resume).
- Notifications `{type, terminal, data, seq}` → `RuntimeEvent`:
  - terminal created/updated → `AgentSeen`/`AgentStatusChanged`
  - terminal finished/removed → `AgentGone`
  - status.state transitions → `AgentStatusChanged`
  - `machine/t7` remote addresses flow through unchanged (adapter keeps
    them qualified).
- **Spike rule that becomes architecture:** `events.subscribe` hijacks the
  connection — notifications-only afterwards. The hc runtime therefore
  owns **two** `hc connect` processes: one dedicated event connection
  (hello → events.subscribe → read loop) and one RPC connection for
  `call()`. `HcClient` already supports this (two instances). The herdr
  runtime keeps its one-connection-per-request + one subscribe socket.
  The trait hides this: `subscribe_events` just works.
- `pane.moved` has no hc analog → `HandleRekeyed` never fires; `t7` is
  stable. Registry sync loses a whole failure class.
- `workspace.*`/`tab.*` events don't exist — irrelevant, scope is cwd.

### 6.5 What degrades on hc (accepted v1)

| Feature | herdr | hc v1 |
|---|---|---|
| `notification.show` toasts (delivery opt-in, blocked, dead-letter, rescue needs-human) | works | **no-op** — log to `routerd.log` only; `RuntimeCaps::notify=false` |
| Popup inbox UI (`plugin.pane.open`) | works | N/A — no plugin panes; inbox stays CLI |
| `agent.explain`, `pane.report_*` | unused by pool | n/a |
| Cross-machine peers | impossible | possible via `machine/t7` — **deferred** (§8) |
| 7501 status (blocked/idle accuracy) | manifest+hooks detection | only if CLI self-reports — coverage TBD |
| Plugin startup hook spawning daemon | `[[startup]]` | unchanged — `herdr-pool`/`herdr-routerd` still installed by the plugin; on an hc-only host the user runs `herdr-routerd start` manually or via shell rc |

## 7. (f) Rollout — mapped to existing beads

The bead graph already exists; this RFC fixes the order and the contract
each bead implements. Recommended execution order:

1. **`runtime-trait-kud`** — create `crates/herdr-runtime`: trait +
   `RuntimeAgent`/`SendOutcome`/`RuntimeEvent`/`RuntimeCaps`/`RuntimeError`
   types + `HerdrRuntime` impl wrapping `HerdrClient` (mechanical: each
   method is a thin map over existing calls — `agent_prompt` error codes →
   `SendOutcome`, `agent.list` → `Vec<RuntimeAgent>`, etc.). No behavior
   change; all existing tests keep passing through the trait or bypass it.
   *Do not* port call sites yet — trait lands dark.
2. **`hc-adapter-ika`** — `HcRuntime` impl over a promoted `hc-spike`
   client (move `client.rs`/`error.rs` into `herdr-runtime/src/hc/` or a
   `hc-client` module; keep spike binary for manual probing). Implements
   §6 semantics: two-connection model, status poll + `if` gate, label
   reads, `HC_TERMINAL_ID` self-identity. Unit-testable against a fake
   `hc connect` shell script emitting canned JSON-RPC.
3. **`identity-scope-port-vfr`** — `resolve_self`/`upsert_self`/
   `record_for_agent`/`agent_handle`/`delivery_target` take
   `&dyn AgentRuntime`; `SelfEnv` gains the hc branch. Store schema
   unchanged (`pane_id` column stores runtime handle).
4. **`events-registry-sync-khm`** — `RegistrySync` consumes
   `EventStream`; herdr event parsing moves behind `HerdrRuntime`'s
   normalized `RuntimeEvent`s.
5. **`delivery-worker-port-8ty`** — `DeliveryWorker`/`RescueManager`/
   `NudgeManager` take `Arc<dyn AgentRuntime>`; switch on `SendOutcome`.
   Rescue flow ports with degraded notify (no-op on hc).
6. **`cli-context-port-364`** — `Ctx` holds `Arc<dyn AgentRuntime>`;
   `HERDR_POOL_RUNTIME` selection; `ensure_daemon` propagates env.
7. **`dual-runtime-flag-jaj`** — flip default plumbing live, runtime stamp
   in state dir, mismatch warning.
8. **`e2e-docs-jxi`** — fake-hc e2e harness (a `hc connect` fake speaking
   NDJSON JSON-RPC on stdio), skill/README/CHANGELOG/AGENTS.md updates.

Each port bead is independently mergeable because the trait lands first
and ports are mechanical per call site. No big-bang rewrite.

## 8. (g) Open questions

1. **`terminal.send` semantics under `status.state=blocked`/`working`.**
   Does the core queue, reject, or type anyway? Does `if=idle` vs
   `if=working` change delivery guarantees? → Resolve empirically in
   `hc-adapter-ika` with a blocked fixture agent; the trait's `SendOutcome`
   already accommodates either answer.
2. **7501 status coverage per agent CLI.** Which of claude/codex/
   opencode/devin/droid/hermes actually emit `status.state`? Without it,
   `blocked` detection and `interactive_ready` gating degrade silently.
   → Spike script: spawn each CLI in hc, drive to a permission prompt,
   read `terminal.get`. Result decides whether v1 needs the `terminal.read`
   heuristic fallback.
3. **`events.subscribe` `seq` resume semantics.** Can a reconnecting
   subscriber pass last-seen `seq` to skip replay, or does it always
   re-snapshot? → Check `rpc.discover`/docs; affects daemon restart cost
   (probably trivial either way — snapshot is cheap).
4. **Scope across machines.** `cwd`-only keys collide for identical paths
   on different cores. Deferred with cross-machine pools; when it lands,
   namespace the scope key (`machine:<id>:<hash>`) rather than changing
   the hash input — existing single-machine keys stay valid.
5. **Notification strategy on hc.** Accept no-op v1? Or emit a desktop
   notification via `osascript`/`notify-send` from the daemon (works
   today, zero hc dependency, slightly hacky)? → Decide in e2e-docs bead;
   trait's `notify` default is already a no-op so this is a one-method
   change.
6. **`terminal.wait` `output` substring matching.** Needed if we ever
   implement `pane.wait_for_output`-equivalent features (inbox popup is
   already N/A on hc). Confirm param shape in adapter bead; not blocking.
7. **Kind detection heuristic quality.** `foreground.name`/`argv`/
   `status.app` vs herdr's manifest detection — will `agent_kind` be
   right often enough for `peers` display and rescue eligibility? →
   `herdr-pool.*` label override is the escape hatch; measure in dogfood.

## 9. Non-goals (explicit)

- No production Rust changes in this bead (RFC only).
- No cross-machine pool delivery in v1 (addressing supports it; scope and
  UX don't).
- No replacement of herdr-plugin manifest/installer — unchanged.
- No async/tokio — hc client stays sync stdio, matching workspace rules.
- No store schema migration — `pane_id` column semantics widen to
  "runtime handle"; document, don't rename.
