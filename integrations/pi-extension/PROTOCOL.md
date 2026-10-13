# @vibeke/pi-extension wire contract

Implemented by `src/protocol.ts` / `src/index.ts`; normative design in [DESIGN.md](DESIGN.md). The server side lives in `crates/vk-server/src/agents/mod.rs`.

## Transport

Unix socket at `$VIBEKE_SOCKET`, newline-delimited JSON-RPC 2.0. The extension is inert unless `VIBEKE=1`, `VIBEKE_SOCKET` and `VIBEKE_PANE_TOKEN` are all set (`HERDR_ENV` alone: no-op).

1. `client.hello {client: "vibeke-pi-extension", kind: "agent", token: $VIBEKE_PANE_TOKEN, version}` (id-bearing request; reply ignored).
2. `adapter.signal {harness: "pi"|"omp", pid, event, payload}` -> `{}` (id-bearing; replies ignored; never awaited). `pid` is the harness process. The server uses it to find the pane's harness process and to tell agents that the pane's agent started apart from it.

`harness` is `"omp"` when running under Bun or after a `tool_approval_requested` event is seen (or `VIBEKE_PI_HOST=pi|omp` overrides), else `"pi"`. The token is only sent in `client.hello`, not per call.

Connection behaviour: one persistent connection, queue cap 500 (overflow clears the queue), reconnect backoff 50 ms -> 2 s doubling, 5 failed tries per burst, then idle until the next signal after 2 s. Sockets and timers are `unref`'d.

**Every connect** (first and every reconnect) sends `Snapshot` immediately after hello, before any queued signal.
- First connect with a clean queue: the snapshot's `seq` is *lower* than the queued signals', so the server must not discard them (they follow it).
- Reconnect, or after a queue overflow: the queue is cleared; the snapshot `seq` is fresh. The server should replace its view of the run from the snapshot and ignore signals with `seq` <= snapshot `seq`.

`seq` is in every signal payload: strictly monotonic, `Date.now()*1000 + n`.

## Signals (`adapter.signal` events; payload fields, plus `seq`)

| event | payload |
|---|---|
| `SessionStart` | `session_id, transcript_path` (absolute session file), `source` (`startup`/`new`/`resume`/`fork`/`switch`/`branch`), `model?`, `host` (`pi`/`omp`), `host_version?`, `extension_version`. Sent on `session_start`, `session_switch`, `session_branch`. |
| `TurnStarted` | `prompt?` (the full request text, bounded at 8 KiB of UTF-8, cut on a character boundary), `prompt_truncated` (true only when `prompt` was cut; present with `prompt`), `prompt_preview?` (<= 200 chars, kept for older servers). From `input` with source != `extension`; all absent when the turn begins from `agent_start` |
| `Working` | `{}` (`agent_start`, `auto_retry_end`) |
| `TurnEnded` | `stop_reason?, last_message?` (<= 2000 chars). pi: `agent_settled` (immediate); both hosts: `agent_end` debounced 250 ms, cancelled by `agent_start`/`input`. At most one per turn. |
| `Settling` | `{}` (omp `session_stop` only; does not end the turn) |
| `ToolStarted` | `call_id, tool, input` (redacted: keys matching `/(key\|token\|secret\|password\|authorization)/i` -> `"[redacted]"`; JSON > 8 KiB -> `{_truncated, _bytes, preview}`) |
| `ToolEnded` | `call_id, tool, ok, file_path?` (absolute; only for successful write/edit style tools whose input was cached; this folds `FileChanged`) |
| `Usage` | `input, output, cache_read, cache_write, cost?` (from `turn_end.message.usage`) |
| `Error` | `message, retrying, rate_limited` (`auto_retry_start`; `retrying` is always true) |
| `Compacting` | `phase: "start"\|"end"` (end only after a start) |
| `SessionEnded` | `reason` (`session_shutdown`; queue flushed with a 300 ms cap, then the connection is closed) |
| `ApprovalRequested` | `call_id, tool, reason, approval_mode` (omp `tool_approval_requested`) |
| `ApprovalResolved` | `call_id, approved` |
| `Snapshot` | `session_id, session_file, is_streaming, turn_index, model, host, host_version?, extension_version, pending_tool_calls: [{call_id, tool, input}], open_approvals: [{call_id, tool, reason}], pending_dialogs: [{dialog_id, method, title, message?, options?}]` (values null when unknown; `pending_dialogs` = wrapper dialogs whose native dialog is still open, same fields as their gate payload) |
| `DialogResolved` | `dialog_id, by: "native", value` (the native dialog answered first) |
| `BgTasks` | `background_tasks: [{id, type: "subagent"\|"shell", status: "running", description, tool_use_id?}]` (the whole current set, Claude hook shape), `kinds` (the kinds whose whole set the list carries). See "Background work". |

With `VIBEKE_HEADLESS_OWNER=1` only `Snapshot`, `SessionStart`, `ToolEnded` and `BgTasks` are sent and the uiContext wrapper is off.

## Background work

Work that keeps running after the tool call that started it. A blocking sub-agent tool that returns when its child is done is a normal tool call and is not reported here.

| Host | Source | `type` | `id` | `description` |
| --- | --- | --- | --- | --- |
| omp | `ctx.getAsyncJobSnapshot().running` (`task` and `bash` with `async.enabled`; `task` sub-agents run inside the omp process) | `task` -> `subagent`, `bash` -> `shell` | the job id (`bg_1`, the agent id) | the job label (agent id, or the command cut at 120 chars) |
| pi | pi-subagents events on `pi.events`: `subagent:async-started` `{id, pid, sessionId, agent, agents, goal, task}` and `subagent:async-complete` `{runId \| id}` | `subagent` | the async run id | `agent: goal` |

- `tool_use_id` (omp only) comes from the `tool_execution_end` result of the call that started the job (`details.async.jobId`, and `details.progress[].id` for pending task spawns).
- pi-subagents runs of another session id are left out. A run whose runner `pid` no longer exists is dropped (it ended without a completion event).
- `kinds`: `["shell", "subagent"]` with omp's job snapshot, `["subagent"]` on pi, else `[]`.
- Sent when the set changes (checked after every host event), after `SessionStart` when the set is not empty (the server clears its set on `SessionStart`), and at every turn end once a source exists (omp) or a non-empty set was sent before. While the set is not empty the extension checks it every 2 s, because jobs end while the host is idle.
- After a reconnect that dropped the queue, the set is sent again right after the `Snapshot` once a non-empty set was ever sent.

## Dialog bridge (uiContext wrapper, TUI mode only)

On each wrapped `confirm`/`select`/`input` call the extension opens a **separate** connection (hello + request) and sends

`adapter.gate {harness, pid, event: "Dialog", payload: {method: "confirm"|"select"|"input", title, message?, options?: string[], dialog_id}}`

The server parks the request until a decision exists. Reply: `{decision: {value: boolean|string} | null, interaction?, idempotency_key?}` (`idempotency_key` = `"<interaction>:<decision_rev>"`).

- `decision.value` of the right type (boolean for confirm, one of `options` for select, string for input): the extension aborts pi's native dialog (its linked `AbortController`, exactly once) and returns the value to the calling extension, then sends `adapter.delivery_ack {interaction, idempotency_key, applied: true}` on its **main** connection (pane-token hello). Unanswered acks are re-sent after each reconnect of the main connection (up to 100 kept).
- Gate connection dropped before a reply while the native dialog is still open: the extension reconnects (50 ms -> 2 s backoff, until the dialog settles) and re-issues the identical request (same `dialog_id`); the server re-attaches the interaction by its native ref (an already-decided one returns its recorded decision).
- `decision: null` or an invalid value: keep waiting for the native dialog.
- The native dialog answering first: the extension closes the gate connection (the server must treat that as "resolved elsewhere") and sends `DialogResolved`.
- Server unreachable: native dialog only (fail-open).

## Ride-along messages

Other panes can send messages to this pane. The extension hands them to the agent where the agent reads input anyway. It calls `message.take {}` on the **main** connection (pane token) and waits at most 2 seconds. The reply is `{context: string|null, messages: [string]}`. The `context` text is ready to show: it is framed as untrusted input from another pane and says how to reply. `null` means nothing is queued. Any error, timeout or missing connection counts as `null`. At most one call is in flight at a time.

| Point | Host event | What the extension does with a non-null `context` |
| --- | --- | --- |
| Turn start | `before_agent_start` | Returns `{message: {customType: "vibeke-message", content, display: true}}`. |
| After a tool call | `tool_execution_end` | `pi.sendMessage(msg, {deliverAs: "steer"})`. |
| Turn end | `agent_end`, `agent_settled` | `pi.sendMessage(msg, {deliverAs: "followUp", triggerTurn: true})`. The new turn starts through `agent_start`, with no `input` event. |

With `VIBEKE_HEADLESS_OWNER=1` the extension never calls `message.take`. The server delivers messages to headless runs itself.

## Control channel (TUI mode only)

Model listing and switching for `agent.models` / `agent.set_model`, and the session's own commands for `agent.commands`. Started on the first event whose context has `mode: "tui"` (never with `VIBEKE_HEADLESS_OWNER=1`, where the server speaks pi's RPC directly).

On its own connection (pane-token hello) the extension long-polls

`adapter.control {harness, ops: ["models", "set_model", "commands"], reply?}` -> `{request: {id, op, params} | null}`

`ops` lists what the host offers (`ctx.modelRegistry.getAvailable`, `pi.setModel`, `pi.getCommands`). The server parks the poll for up to 25 s; `request: null` means "poll again". The result of a request goes out with the next poll as `reply: {id, ok: true, result}` or `{id, ok: false, error}`.

| op | params | result |
|---|---|---|
| `models` | `{}` | `{models: [{id: "provider/id", label, description?: provider, current}]}` |
| `set_model` | `{model: "provider/id", scope: "session"\|"default"}` | `{model, default_changed}` via `pi.setModel(model)`. pi saves every switch as its default, so it switches only for `scope: "default"` (`default_changed: true`) and refuses any other scope without switching; the server refuses such a request first (`conflict`, `reason: "persists_default"`) so the client can ask the user. omp keeps a switch to the session and refuses `scope: "default"` |
| `commands` | `{}` | `{commands: [{name, description}]}` from `pi.getCommands()` |

A response that is an error or has no `request` member (a server without the channel) ends the loop for good; a dropped connection reconnects with backoff.
