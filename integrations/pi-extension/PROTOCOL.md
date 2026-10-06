# @vibeke/pi-extension wire contract

Implemented by `src/protocol.ts` / `src/index.ts`; normative design in [DESIGN.md](DESIGN.md). The server side lives in `crates/vk-server/src/agents/mod.rs`.

## Transport

Unix socket at `$VIBEKE_SOCKET`, newline-delimited JSON-RPC 2.0. The extension is inert unless `VIBEKE=1`, `VIBEKE_SOCKET` and `VIBEKE_PANE_TOKEN` are all set (`HERDR_ENV` alone: no-op).

1. `client.hello {client: "vibeke-pi-extension", kind: "agent", token: $VIBEKE_PANE_TOKEN, version}` (id-bearing request; reply ignored).
2. `adapter.signal {harness: "pi"|"omp", event, payload}` -> `{}` (id-bearing; replies ignored; never awaited).

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
| `TurnStarted` | `prompt_preview?` (<= 200 chars; from `input` with source != `extension`; absent when the turn begins from `agent_start`) |
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
| `Snapshot` | `session_id, session_file, is_streaming, turn_index, model, host, host_version?, extension_version, pending_tool_calls: [{call_id, tool, input}], open_approvals: [{call_id, tool, reason}]` (values null when unknown) |
| `DialogResolved` | `dialog_id, by: "native", value` (the native dialog answered first) |

With `VIBEKE_HEADLESS_OWNER=1` only `Snapshot`, `SessionStart` and `ToolEnded` are sent and the uiContext wrapper is off.

## Dialog bridge (uiContext wrapper, TUI mode only)

On each wrapped `confirm`/`select`/`input` call the extension opens a **separate** connection (hello + request) and sends

`adapter.gate {harness, event: "Dialog", payload: {method: "confirm"|"select"|"input", title, message?, options?: string[], dialog_id}}`

The server parks the request until a decision exists. Reply: `{decision: {value: boolean|string} | null, interaction?}`.

- `decision.value` of the right type (boolean for confirm, one of `options` for select, string for input): the extension aborts pi's native dialog (its linked `AbortController`, exactly once) and returns the value to the calling extension.
- `decision: null` or an invalid value: keep waiting for the native dialog.
- The native dialog answering first: the extension closes the gate connection (the server must treat that as "resolved elsewhere") and sends `DialogResolved`.
- Server unreachable: native dialog only (fail-open).
