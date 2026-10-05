# @vibeke/pi-extension — design

This is the single extension that gives **pi** (`@earendil-works/pi-coding-agent`, verified against 0.84.1) and **omp** (`@oh-my-pi/pi-coding-agent`, verified against 17.2.12) the **extension transport** to Vibeke (spec 04 §2.1): execution state, session identity and resume, file-change tracking, usage, reconnect snapshots (the `reconcile` capability), and optional approval gating. Which capabilities this yields per host version is published in 04 §2.3. The parent spec is `spec/04-harness-adapters.md` §6.3.

## 1. Can one extension serve both? Yes, with a small capability layer

| Aspect | pi | omp | Consequence |
|---|---|---|---|
| Extension API | `export default function (pi: ExtensionAPI)` | the same factory model; omp re-exports a legacy shim so imports of `@earendil-works/pi-coding-agent` / `@mariozechner/pi-coding-agent` resolve to omp (`src/extensibility/legacy-pi-coding-agent-shim.ts`, `plugins/legacy-pi-compat`) | One source file. **Import types only** (`import type`), so at runtime there are no package imports that could resolve differently |
| Runtime | Node (pi is distributed via npm and runs on node) | Bun (compiled binary) | Use only `node:net`, `node:path`, `node:crypto` and `process`. Both runtimes support them. No `bun:` and no npm deps |
| Install location | `~/.pi/agent/extensions/vibeke/index.ts` or `pi install npm:@vibeke/pi-extension` | `~/.omp/agent/extensions/vibeke.ts`, or `--extension` / `--hook` | The installer writes a 1-line loader per host that re-exports the shared file |
| "Settled" signal | `agent_settled` (no retry, compaction or follow-up left) | none. `session_stop` is a **vetoable settling pass**: it fires when a turn is *about to* settle and any handler may request a continuation turn (`stop_hook_active` on re-entry), so it is **not** completion | pi: `agent_settled` is definitive. omp: `session_stop` → signal `Settling`; execution becomes `idle` only on debounced `agent_end` (250 ms) with no `agent_start` following. Vibeke's own `session_stop` handler never requests a continuation |
| Session switch | `session_start{reason: new\|resume\|fork}` | `session_switch`, `session_branch`, plus `session_start` | Handle all of them; each re-identifies |
| Native approvals | none; pi has no permission system by design | `tool_approval_requested{toolCallId, toolName, reason, approvalMode}` / `tool_approval_resolved{approved}` | On omp, observe these and open/resolve Interactions (best-effort keystroke answers only). On pi, Vibeke's optional gate is the only approval mechanism — so it **fails closed** (§4) |
| Blocking hook | `tool_call` → `{block, reason, terminate}` (may also revise `input`) | the same; for loop-dispatched calls emitted at arg-prep time, **before** `tool_execution_start` and before omp's own approval gate (verified in 17.2.12 `session/agent-session.ts#beforeToolCall`; calls omp already denies never reach extensions) | Shared gate implementation |
| `tool_execution_end` payload | `{toolCallId, toolName, result, isError}` — **no input** | same | Cache each call's input by `toolCallId` at `tool_call` / `tool_execution_start`; read it back at `_end`; evict on end or after 10 min |
| Extra events | `model_select`, `thinking_level_select` | `goal_updated`, `todo_reminder`, `ttsr_triggered`, `credential_disabled`, `mcp_notification`, `user_python` | Register defensively. Unknown event names must not throw. Wrap each `pi.on` in try/catch, because hosts may validate names |
| Headless | `--mode rpc` (strict LF-only JSONL; extension UI protocol over stdio) | `--mode rpc` and `--mode rpc-ui` | The extension also runs in RPC mode (`ctx.mode === "rpc"`). It still reports state, unless `VIBEKE_HEADLESS_OWNER=1` says the Vibeke RpcAdapter already owns the stream, in which case it only reports what RPC lacks (file changes) |

**Host detection**: check `process.versions.bun` and the presence of `tool_approval_requested` handlers; omp also sets `PI_*` env plus its own. Expose `host: "pi" | "omp" | "unknown"` in the `hello` signal for diagnostics only. Behavior is capability-based, not host-based.

## 2. Wire protocol to Vibeke

- **Connection**: Unix socket at `process.env.VIBEKE_SOCKET` (Windows named pipe later). On load the extension is **inert** unless `VIBEKE === "1"`, `VIBEKE_SOCKET` and `VIBEKE_PANE_TOKEN` (09 §3.2) are all set. That makes it safe to install globally.
- **Fallback to Herdr compat**: if only `HERDR_ENV=1` is present (the user runs plain Herdr), do nothing. Herdr's own extension handles that case. Don't double-report.
- **Transport**: one persistent connection, newline-delimited JSON-RPC 2.0, reconnecting with backoff (50 ms → 2 s, max 5 tries per burst). A crashing or absent server must never affect the agent.
- **Messages**:
  - `adapter.signal {token, harness, signal}`: fire-and-forget. Queued in memory (cap 500) while connected-but-slow; flushed in order.
  - `adapter.snapshot {token, harness, snapshot}`: sent first on **every (re)connect**; see below.
  - `adapter.gate {token, harness, interaction, timeout_ms}` / `adapter.gate_resume`: request/response, used from `tool_call`.
  - `adapter.delivery_ack {token, interaction, idempotency_key, applied}`: after a gate decision has been returned to the host.
- **Ordering**: a single send queue plus a monotonic `seq` (`Date.now()*1000 + n`) on every signal, for server-side dedupe.
- **Reconnect repair (correctness does not depend on the queue).** While disconnected the queue is *not* trusted as a history: when it overflows it is simply cleared. On every successful (re)connect the extension sends `adapter.snapshot` built from live host state, equivalent to RPC `get_state`:
  `{session_id, session_file, is_streaming, turn_index, model, pending_tool_calls: [{toolCallId, toolName, input(redacted)}], open_approvals: [{toolCallId, toolName, reason}] /* requested without resolved (omp) */, pending_gates: [{interaction, toolCallId}], seq}`.
  The server's arbiter replaces its view of the run with the snapshot and discards queued signals with `seq` ≤ snapshot `seq`. This is what gives the extension transport the `reconcile` capability (04 §2.2).

## 3. Event handling

| Host event | Action |
|---|---|
| load | send `hello {host, host_version, extension_version, pid, cwd}` |
| `session_start`, `session_switch`, `session_branch` | read `ctx.sessionManager.getSessionFile()` (absolute path) and `getSessionId()` → `SessionStarted {harness_session_id, transcript_path, source: event.reason ?? "startup", model: ctx.model?.id}` |
| `input` (source ≠ `"extension"`) | `TurnStarted {prompt_preview: first 200 chars}` |
| `agent_start` | `state working` (as a `TurnStarted` without preview if `input` didn't fire, e.g. RPC prompt) |
| `turn_start` / `turn_end` | turn records; `turn_end.message.usage` → `Usage {input, output, cacheRead, cacheWrite, cost.total}` |
| `tool_execution_start` | cache `args` by `toolCallId` (if not already cached from `tool_call`); `ToolStarted {call_id: toolCallId, tool: toolName, input: redacted(args)}` |
| `tool_execution_end` | `ToolEnded {ok: !isError}`; for `write` / `edit` (and omp's edit variants) → `FileChanged {path: resolve(ctx.cwd, cachedInput(toolCallId).path), op}`; evict the cache entry. If no cached input exists (extension loaded mid-call), emit `ToolEnded` only |
| `tool_call` | cache `input` by `toolCallId`; **gate** (§4) |
| `tool_approval_requested` (omp) | `InteractionOpen {kind: approval, native_ref: toolCallId, payload: {tool, reason, approvalMode}, gate: observe}` |
| `tool_approval_resolved` (omp) | `InteractionResolved {native_ref, resolution: approved ? allowed : denied}` |
| `agent_end` | start the 250 ms debounce → `TurnEnded` unless superseded by `agent_start` or retry |
| `agent_settled` (pi) | immediate `TurnEnded`, cancelling the debounce |
| `session_stop` (omp) | `Settling {turn_id, stop_hook_active}` only; does **not** end the turn (vetoable pass). The `agent_end` debounce decides |
| `auto_retry_start` | `Error {retrying: true, message}`; if it matches the rate-limit regex → `RateLimited {}` (`overloaded\|rate.?limit\|429\|5xx\|timeout…`) |
| `auto_retry_end` | `working` |
| `compaction_*` / `auto_compaction_*` / `session_compact` | `Compacting {start\|end}` |
| `model_select` | `Raw` plus a model update |
| `session_shutdown` | `SessionEnded {reason}`; flush the queue with a 300 ms cap |

**Redaction**: tool inputs over 8 KiB are truncated. Values of keys matching `/(key|token|secret|password|authorization)/i` are replaced with `"[redacted]"`. Bash commands are sent as-is, because they're needed for policy and risk.

## 4. Gate (`tool_call`)

```ts
pi.on("tool_call", async (event, ctx) => {
  if (!gateEnabled()) return;                         // VIBEKE_GATE=1 or server says policy exists for this cwd
  const decision = await gate({
    kind: "approval",
    native_ref: event.toolCallId,
    action: describe(event.toolName, event.input, ctx.cwd),  // command / paths / diff preview for edit
  }, { timeoutMs: cfg.gateTimeoutMs, signal: ctx.signal });  // Esc in pi aborts the wait
  if (decision.kind === "allow") { ack(decision); return; }
  if (decision.kind === "deny") {
    ack(decision);
    return { block: true, reason: decision.text ?? "Denied in Vibeke", terminate: decision.stop === true };
  }
  // no_decision / timeout / server unavailable / malformed reply:
  // pi has no approval system of its own, so an enabled gate is Vibeke-only enforcement → FAIL CLOSED.
  if (ctx.hasUI) {
    const ok = await ctx.ui.confirm(`Vibeke unavailable — run ${summary(event)}?`);   // local confirmation in pi's own UI
    return ok ? undefined : { block: true, reason: "Not confirmed locally (Vibeke unavailable)" };
  }
  return { block: true, reason: "Vibeke unavailable; gated tool call blocked" };
});
```

- `gateEnabled()` is cached. On `session_start` the extension asks `adapter.policy_hint {cwd}`, and the server replies `{gate: bool, mode: observe|gate|auto}`. Re-evaluated on the `policy.*` events the server pushes on the same connection.
- **Parallel tools**: pi preflights sibling `tool_call`s sequentially, so the gate sees one call at a time. Batching approvals (Phase 2) is done server-side by grouping interactions that open within 1 s for the same run.
- **pi without the gate**: never blocks. That matches pi's documented philosophy, and the user must opt in. Observation (signals, snapshots) always fails open regardless of gate state.
- **omp**: the gate runs only when Vibeke policy or gate mode requires it. Otherwise omp's native approval flow is observed. Verified ordering (17.2.12): loop-dispatched `tool_call` precedes omp's approval gate, so a Vibeke "deny" pre-empts omp's prompt, while "allow" leaves omp's own prompt in place (that is omp's decision). On omp the fail-closed fallback is unnecessary when omp's own approval mode would prompt anyway: on Vibeke failure the extension returns `undefined` and omp asks. It fails closed only when omp's mode is permissive (yolo) and a Vibeke `deny` rule is configured (cooperative guardrail, spec 13 §10). Golden test `omp/approval-ordering` pins this per version.

## 5. Headless (RPC) cooperation

When Vibeke's `RpcAdapter` spawns `pi --mode rpc` / `omp --mode rpc-ui`, it sets `VIBEKE_HEADLESS_OWNER=1`:

- The RPC stream already carries agent, turn, message and tool events, and extension UI requests (`select`/`confirm`/`input`/`editor`) which Vibeke answers natively.
- The extension then reports only identity (session file path) and `FileChanged`, so nothing is double-counted.
- The RPC client must split records on `\n` only, not with a Node `readline`-style splitter that breaks on U+2028/U+2029 (pi `rpc.md` §Framing). This is a requirement on the Rust `RpcAdapter`.

## 6. Packaging and install

```
integrations/pi-extension/
  src/index.ts          # the extension (no runtime imports beyond node:*)
  src/protocol.ts       # JSON-RPC client, queue, reconnect
  src/describe.ts       # tool → action summary, redaction, diff preview
  test/                 # vitest: fake socket server; runs under node and bun in CI
  package.json          # name @vibeke/pi-extension, "pi" package metadata for `pi install`
  DESIGN.md
```

- **pi**: `vibeke integration install pi` runs `pi install npm:@vibeke/pi-extension` when online. Otherwise it copies the bundled single-file build from the Vibeke binary to `~/.pi/agent/extensions/vibeke/index.ts`.
- **omp**: writes `~/.omp/agent/extensions/vibeke.ts` containing the bundled single-file build, with the header `// managed by vibeke (vibeke-integration=omp@<v>); reinstall overwrites`. It is placed beside, not replacing, other extensions.
- **Versioning**: the extension sends `extension_version`. The server reports `integration_outdated` in `vibeke integration status` when the bundled version is newer.

## 7. Tests

- **Unit**: event → signal mapping per host fixture (pi 0.84 and omp 17.2 event payload fixtures captured from real runs).
- **Contract**: a fake Vibeke socket asserts ordering, dedupe `seq`, snapshot-on-reconnect (and that a dropped queue still yields correct server state), observation fail-open under server kill, **gate fail-closed** under server kill/timeout/malformed reply (local confirm with UI, block without), delivery ack, and input caching across `tool_call` → `tool_execution_end`.
- **Golden** (in `tests/harness-golden/pi`, `/omp`): real pi/omp against the mock LLM provider (pi custom provider → `tests/mock-llm`). Covers startup, prompt, tool, edit, gate deny, retry, compaction, `/new`, `/fork`, `/resume` and exit.
- **Performance**: a handler adds ≤ 0.5 ms per event on the host's hot path. All sends are non-blocking except the gate.
