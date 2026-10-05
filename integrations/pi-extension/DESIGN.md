# @vibeke/pi-extension — design

This is the single extension that gives **pi** (`@earendil-works/pi-coding-agent`, verified against 0.84.1) and **omp** (`@oh-my-pi/pi-coding-agent`, verified against 17.2.12) the **extension transport** to Vibeke (spec 04 §2.1): execution state, session identity and resume, file-change tracking, usage, reconnect snapshots (the `reconcile` capability). **It is observe-only: it never blocks, gates or answers tool calls.** Vibeke does not provide a permission system for pi; users who want approvals install a pi permission extension of their choice, and Vibeke surfaces — and can answer — that extension's dialogs (§4) without replacing pi's own UI. Which capabilities this yields per host version is published in 04 §2.3. The parent spec is `spec/04-harness-adapters.md` §6.3.

## 1. Can one extension serve both? Yes, with a small capability layer

| Aspect | pi | omp | Consequence |
|---|---|---|---|
| Extension API | `export default function (pi: ExtensionAPI)` | the same factory model; omp re-exports a legacy shim so imports of `@earendil-works/pi-coding-agent` / `@mariozechner/pi-coding-agent` resolve to omp (`src/extensibility/legacy-pi-coding-agent-shim.ts`, `plugins/legacy-pi-compat`) | One source file. **Import types only** (`import type`), so at runtime there are no package imports that could resolve differently |
| Runtime | Node (pi is distributed via npm and runs on node) | Bun (compiled binary) | Use only `node:net`, `node:path`, `node:crypto` and `process`. Both runtimes support them. No `bun:` and no npm deps |
| Install location | `~/.pi/agent/extensions/vibeke/index.ts` or `pi install npm:@vibeke/pi-extension` | `~/.omp/agent/extensions/vibeke.ts`, or `--extension` / `--hook` | The installer writes a 1-line loader per host that re-exports the shared file |
| "Settled" signal | `agent_settled` (no retry, compaction or follow-up left) | none. `session_stop` is a **vetoable settling pass**: it fires when a turn is *about to* settle and any handler may request a continuation turn (`stop_hook_active` on re-entry), so it is **not** completion | pi: `agent_settled` is definitive. omp: `session_stop` → signal `Settling`; execution becomes `idle` only on debounced `agent_end` (250 ms) with no `agent_start` following. Vibeke's own `session_stop` handler never requests a continuation |
| Session switch | `session_start{reason: new\|resume\|fork}` | `session_switch`, `session_branch`, plus `session_start` | Handle all of them; each re-identifies |
| Native approvals | none; pi has no permission system by design | `tool_approval_requested{toolCallId, toolName, reason, approvalMode}` / `tool_approval_resolved{approved}` | On omp, observe these and open/resolve Interactions (omp owns the decision; answering from Vibeke = jump to pane, or best-effort keystrokes if enabled). On pi, approvals come only from a user-installed permission extension, surfaced per §4 |
| Blocking hook | `tool_call` → `{block, reason, terminate}` (may also revise `input`) | the same; for loop-dispatched calls emitted at arg-prep time, **before** `tool_execution_start` and before omp's own approval gate (verified in 17.2.12 `session/agent-session.ts#beforeToolCall`; calls omp already denies never reach extensions) | **Not used for gating.** The extension only caches `input` by `toolCallId` here and always returns `undefined` |
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
- **Ordering**: a single send queue plus a monotonic `seq` (`Date.now()*1000 + n`) on every signal, for server-side dedupe.
- **Reconnect repair (correctness does not depend on the queue).** While disconnected the queue is *not* trusted as a history: when it overflows it is simply cleared. On every successful (re)connect the extension sends `adapter.snapshot` built from live host state, equivalent to RPC `get_state`:
  `{session_id, session_file, is_streaming, turn_index, model, pending_tool_calls: [{toolCallId, toolName, input(redacted)}], open_approvals: [{toolCallId, toolName, reason}] /* requested without resolved (omp) */, seq}`.
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
| `tool_call` | cache `input` by `toolCallId`; return `undefined` (never blocks) |
| `tool_approval_requested` (omp) | `InteractionOpen {kind: approval, native_ref: toolCallId, payload: {tool, reason, approvalMode}, answer: native_ui_only}` |
| `tool_approval_resolved` (omp) | `InteractionResolved {native_ref, resolution: approved ? allowed : denied}` |
| `agent_end` | start the 250 ms debounce → `TurnEnded` unless superseded by `agent_start` or retry |
| `agent_settled` (pi) | immediate `TurnEnded`, cancelling the debounce |
| `session_stop` (omp) | `Settling {turn_id, stop_hook_active}` only; does **not** end the turn (vetoable pass). The `agent_end` debounce decides |
| `auto_retry_start` | `Error {retrying: true, message}`; if it matches the rate-limit regex → `RateLimited {}` (`overloaded\|rate.?limit\|429\|5xx\|timeout…`) |
| `auto_retry_end` | `working` |
| `compaction_*` / `auto_compaction_*` / `session_compact` | `Compacting {start\|end}` |
| `model_select` | `Raw` plus a model update |
| `session_shutdown` | `SessionEnded {reason}`; flush the queue with a 300 ms cap |

**Redaction**: tool inputs over 8 KiB are truncated. Values of keys matching `/(key|token|secret|password|authorization)/i` are replaced with `"[redacted]"`. Bash commands are sent as-is, because they're needed for the timeline and risk labels.

## 4. Surfacing permission-extension dialogs (no Vibeke gate)

Vibeke deliberately has **no approval gate for pi**. pi's design leaves permissions to extensions, so Vibeke's job is only to make *the user's chosen* permission extension's dialogs visible and answerable where that's possible without taking over pi's UI.

| pi mode | What happens | Interaction capability |
|---|---|---|
| **TUI** (normal interactive pi) — **primary path: shared-uiContext wrapper** | Verified in pi 0.84.1 `dist/core/extensions/runner.js`: every extension's `ctx.ui` is a getter returning `runner.uiContext`, i.e. **one shared `ExtensionUIContext` object for all extensions**, resolved at call time; `ExtensionUIDialogOptions` has `signal?: AbortSignal` ("programmatically dismiss the dialog") and `timeout`. The extension wraps `confirm`, `select` and `input` on that shared object (§4.1). pi still renders its **own native dialog, unchanged**, in the agent's pane; Vibeke additionally opens an Interaction and can answer it from a card for unfocused panes. `editor` has no options argument → observed only. `ctx.ui.custom()` widgets are not covered. | `observe` + `answer_native: extension dialogs` — **relies on pi internals**, granted per pi version only when golden tests pass |
| **TUI, fallback** | If the wrapper isn't installed (older/changed pi, wrapper self-check failed): screen manifest for pi's generic dialog widget flags an inferred Interaction for unfocused panes (`source: screen`, `answerable: false`); card action = jump to pane. Opt-in best-effort keystrokes behind `agents.harness.pi.keystroke_answers = false`. | `observe` (inferred); `answer_keystroke` opt-in |
| **RPC / headless** (`pi --mode rpc`, owned by Vibeke's `RpcAdapter`) | Every `ctx.ui.select/confirm/input/editor` dialog is emitted as `{"type":"extension_ui_request","id","method","title",…,"timeout"?}` and blocks until the client sends `{"type":"extension_ui_response","id","value"\|"confirmed"\|"cancelled"}` (pi `rpc.md` §Extension UI Protocol). The `RpcAdapter` maps each to an Interaction and answers natively (the wrapper stays inert in RPC mode to avoid double-reporting). Fire-and-forget methods become notifications/status. | `observe` + `answer_native` (public protocol) |

### 4.1 The uiContext wrapper (TUI mode)

```ts
const wrapped = new WeakSet<object>();
function ensureWrapped(ui: ExtensionUIContext) {          // called on load, session_start, session_switch, reload, and before each signal flush
  if (!ui || wrapped.has(ui) || ctx.mode !== "tui") return; // runner may swap uiContext (setUIContext) → new object gets wrapped
  for (const m of ["confirm", "select", "input"] as const) {
    const orig = ui[m].bind(ui);
    ui[m] = async (...args) => {
      const opts = lastArgIsOptions(m, args) ?? {};
      const ac = new AbortController();
      link(opts.signal, ac);                               // caller's own signal still dismisses the dialog
      const native = orig(...withOptions(m, args, { ...opts, signal: ac.signal }));   // caller's timeout preserved
      const inter = openInteraction(m, args);              // kind: confirm → approval, select/input → question; payload title/message/options;
                                                           // source: "pi extension dialog" (calling extension unknown)
      if (!inter) return native;                           // Vibeke unreachable → just the native dialog (fail-open: it's the user's plugin)
      const winner = await Promise.race([native.then(v => ({ by: "native", v })), inter.answer.then(v => ({ by: "vibeke", v }))]);
      if (winner.by === "native") { inter.resolvedElsewhere(winner.v); return winner.v; }
      ac.abort();                                          // dismiss pi's native dialog
      inter.delivered();
      return winner.v;                                     // confirm → boolean, select → option string, input → text
    };
  }
  wrapped.add(ui);
}
```

- This is **observe + answer**, not a Vibeke gate: the dialog belongs to the user's permission extension, which decides what to do with the answer. If Vibeke is unreachable, nothing changes for the user.
- In the focused pane the user simply answers pi's native dialog; cards appear only for unfocused panes (08 §1). Answering a card dismisses the native dialog through the signal, and pi's UI updates itself.
- **Relies on internals** (shared mutable `uiContext`, signal semantics). A self-check on load verifies the shape (methods present, writable, signal honoured in a no-UI probe) and otherwise disables the wrapper and falls back to screen detection. Golden tests per pi version: native-first answer, Vibeke-first answer (native dialog dismissed exactly once), caller timeout, caller abort, uiContext swap on `/reload`, two dialogs from two extensions concurrently.
- **omp**: omp 17.2.12 `src/extensibility/extensions/runner.ts` keeps a single `#uiContext` and puts that same object on every extension context (`ui: this.#uiContext`, captured at context creation rather than via a getter; swapped by `setUIContext`), and `ExtensionUIDialogOptions` has `signal` and `timeout`. The same wrapper should apply; whether the object's methods are writable and the signal dismisses omp's dialog is **[verify M0]**. Note omp's own extension-tool safety prompt (`extensions/wrapper.ts`, `uiContext.select(…, ["Approve","Deny"])`) would also pass through the wrapper and become an Interaction.

**omp** has its own approval system: the extension observes `tool_approval_requested` / `tool_approval_resolved` (§3) and never gates. omp's own approval dialog in the focused pane is omp's UI; for unfocused panes the card answers through the wrapper where omp's prompt goes through `uiContext` ([verify M0]), else offers "jump to pane".

**Upstream (optional):** [UPSTREAM-PROPOSAL.md](UPSTREAM-PROPOSAL.md) asks pi to make this path official (observer events + `ctx.ui.resolve`), so the wrapper's reliance on internals can be dropped.

**Not affected:** Vibeke's sandbox boundary approvals (egress, push, credential use — spec 13) are Vibeke's own and are enforced by the sandbox/egress proxy, not by this extension.

## 5. Headless (RPC) cooperation

When Vibeke's `RpcAdapter` spawns `pi --mode rpc` / `omp --mode rpc-ui`, it sets `VIBEKE_HEADLESS_OWNER=1`:

- The RPC stream already carries agent, turn, message and tool events, and extension UI requests (`select`/`confirm`/`input`/`editor`) which Vibeke turns into Interactions and answers natively (§4).
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
- **Contract**: a fake Vibeke socket asserts ordering, dedupe `seq`, snapshot-on-reconnect (and that a dropped queue still yields correct server state), observation fail-open under server kill (the agent is never blocked or slowed), `tool_call` always returning `undefined`, the uiContext wrapper's race/abort semantics against a fake uiContext, and input caching across `tool_call` → `tool_execution_end`.
- **Golden** (in `tests/harness-golden/pi`, `/omp`): real pi/omp against the mock LLM provider (pi custom provider → `tests/mock-llm`). Covers startup, prompt, tool, edit, a third-party permission extension's confirm dialog (TUI: answered via the uiContext wrapper, and native-first; RPC: answered natively; wrapper disabled: detected on screen), retry, compaction, `/new`, `/fork`, `/resume` and exit.
- **Performance**: a handler adds ≤ 0.5 ms per event on the host's hot path. All sends are non-blocking; nothing in the extension ever awaits the server on the host's hot path.
