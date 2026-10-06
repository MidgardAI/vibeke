# 07 — API, CLI, plugins and Herdr compatibility

This section specifies every external interface of the server: the control API (JSON-RPC), the event subscription API, the render stream, the holder protocol, the CLI that mirrors the API, the embedded agent skill, the plugin system, and the Herdr compatibility layer. Types referenced here (`Pane`, `AgentRun`, `Interaction`, `Task`, `Preview`, event envelope, …) are defined in [02-data-model-and-event-log.md](02-data-model-and-event-log.md); process roles and transports in [01-architecture.md](01-architecture.md).

Milestone tags (plan in [11](11-milestones.md)): **[M1]** supervision slice (core runtime + Claude/Codex/pi/omp + interactions + worktree tasks), **[M2]** safe yolo (sandbox/container) + more harnesses, **[M3]** remote + preview, **[M4]** VMs + polish/parity, **[M5]** compatibility + plugins, **[M6]** hardening / Windows / 1.0; **[post-1.0]** deferred.

---

## 1. Control API — conventions

### 1.1 Transport and framing

- Socket: `$RUNTIME/<session>/vibeke.sock` (0600, dir 0700), named pipe `\\.\pipe\vibeke-<uid>-<session>` on Windows [M6]. Remote sessions are reached through the local server (§1.6) or `--machine`.
- Framing: one JSON object per line (`\n`), UTF-8. Clients **must** split on `\n` only (U+2028/U+2029 are legal inside JSON strings — the same trap pi's RPC docs call out). Max line 16 MiB; larger payloads (screenshots, uploads) go through blob methods (§2.15).
- Protocol: JSON-RPC 2.0. `id` may be number or string. Notifications (no `id`) are accepted for fire-and-forget methods marked *(notify ok)*.
- **Connections are long-lived and multiplexed** (not one request per connection): a client can pipeline many requests and hold subscriptions on the same connection. Responses may arrive out of order; correlate by `id`.
- Handshake: the first request on a connection **should** be `client.hello`; without it the connection gets `client_kind: "anonymous"` and the default capability set for its peer (see 09 §3).

```json
→ {"jsonrpc":"2.0","id":1,"method":"client.hello","params":{"client":"vibeke-cli","version":"1.0.0","api":"vibeke/1","kind":"cli","token":"<optional pane/plugin token>"}}
← {"jsonrpc":"2.0","id":1,"result":{"server_version":"1.0.0","api":"vibeke/1","session":"default","machine":"local","capabilities":["*"],"features":["render.v1","events.v1","preview.v1"]}}
```

### 1.2 Targets

Every method that operates on an object accepts a `target` string or a typed id field. Accepted forms (01 §7.1):

| Form | Example | Resolves to |
|---|---|---|
| Short handle | `w3`, `w3:t2`, `w3:p5`, `a12`, `i42`, `k7`, `v4` | that object in the connection's session |
| Machine-qualified | `devbox/w3:p5` | object on remote machine `devbox` |
| ULID | `01J9…` | exact object |
| Agent name | `reviewer` | live AgentRun with that name (and, for pane methods, its pane) |
| `@current` | — | the calling pane (from the connection's pane token / `VIBEKE_PANE_ID`) |
| `@focused` | — | the focused pane of the *most recently active TUI client* (explicit opt-in; never a default for mutating methods called from a pane) |

Methods called with a pane token and no target default to `@current`, never `@focused` (so an omitted target never hits another client's focus).

### 1.3 Result shape

Results are plain objects keyed by noun: `{"pane": {...}}`, `{"panes": [...]}`, `{"run": {...}, "interaction": {...}}`. Every mutating result includes `"cursor"` (§2.13 `Cursor`, with `seq` the event-log sequence number after the mutation), so a client can `events.subscribe {after: cursor}` with no race.

### 1.4 Errors

JSON-RPC `error: {code, message, data: {kind, details?, retryable: bool}}`. `data.kind` is the stable machine-readable string; `code` groups:

| Code | `kind` examples | Meaning |
|---|---|---|
| -32700 / -32600 / -32601 / -32602 | `parse_error`, `invalid_request`, `method_not_found`, `invalid_params` | standard JSON-RPC |
| -32001 | `not_found` (`details.object: pane|tab|workspace|run|interaction|task|preview|machine|plugin`) | target does not exist |
| -32002 | `ambiguous_target` | e.g. agent name prefix matches several |
| -32003 | `permission_denied` | capability missing (see 09) — includes `self_answer_forbidden` |
| -32004 | `conflict` | state precondition failed: `pane_busy`, `name_taken`, `interaction_closed`, `worktree_exists`, `layout_too_small` |
| -32005 | `timeout` | wait methods; `details.last_state` included |
| -32006 | `stalled` | `agent_prompt_stalled`: no lifecycle change within 5 s of a prompt |
| -32007 | `unsupported` | harness/adapter cannot do this (e.g. native answer on screen-only harness); `details.fallback` names what is possible |
| -32008 | `remote_unavailable` | machine offline/degraded; `retryable: true` |
| -32009 | `rate_limited` | per-connection or per-token request budget exceeded |
| -32010 | `truncated` | event cursor older than retention; `details.earliest_seq` |
| -32011 | `invalid_key` | key grammar rejected (`details.key`) |
| -32012 | `untrusted` | repo-local config/policy not yet trusted (09 §4) |
| -32050 | `internal` | bug; includes `details.trace_id` for `vibeke debug bundle` |

*As built (Batch 2A, 2026-10-06): `rate_limited` budgets and spawn limits* (09 §5.1 rules 7 and 8, `crates/vk-server/src/limits.rs`). Checked in `dispatch` before every pane-scoped call; full-scope callers are never limited. Each pane token has a token bucket of `burst` requests refilled at `rate` per second (default 50 and 10); `agent.spawn`, `agent.start` and `task.create {agents}` from a pane are limited to `spawns_per_min` per pane in any 60 s window (default 10). Exceeding either is `rate_limited` (`retryable: true`, `details: {limit: requests|spawn, retry_after_ms, scope: pane}`). Agent lineage follows `created_by = agent:<pane>`: a pane-scoped call that creates a pane (`pane.split|float`, `tab.create`, `workspace.create`, `agent.spawn`, `task.create`, `layout.apply`, `browser.pane.create`) deeper than `max_spawn_depth` (default 3) is `permission_denied {reason: spawn_depth_exceeded, depth, max}`, and an agent start while the root pane's lineage has `max_descendant_runs` live runs (default 20) is `permission_denied {reason: descendant_runs_exceeded}`. Every refusal records a `security.rate_limited {pane} {method, limit}` event and, at most once a minute per pane, a notification ("w1:p3 is spawning agents rapidly"). Configuration `[security.limits] burst, rate, spawns_per_min, max_spawn_depth, max_descendant_runs`, re-read on config reload. Budgets are per server process (in memory) and are not shared between sessions. Test: `crates/vibeke/tests/api_surface_2a.rs` (`pane_budgets_and_spawn_depth`).

### 1.5 Versioning

- API string `vibeke/1`. Within a major: new methods, new optional params, new result fields, new event types only. Clients must ignore unknown fields and unknown event types.
- `api.schema` returns the full JSON Schema (2020-12); CI diffs it against the previous release and fails on breaking changes.
- Generated clients: `@vibeke/client` (TypeScript, published to npm), `vibeke-client` (Python), and the Rust `vk-proto` crate.
- **As built (M6 groundwork, 2026-10-06).** The schema is not derived with `schemars`: most handlers parse `Value` ad hoc, so the shapes are written once in a registry, `crates/vk-server/src/api_schema.rs`, in a small notation (`crates/vk-server/src/shape.rs`: `{field, field?: type = default}`, `[T]`, `A|B`, `{*: T}`, CamelCase shared types mirroring the `vk-proto` model structs). **Optional is not nullable:** `field?: T` may be absent but is never `null`; a field the server can send as `null` says so (`field: T|null`, or `field?: T|null` for absent-or-null), the emitter writes `anyOf: [T, {type: null}]` only then, and the generated clients follow it exactly (TypeScript `f?: T` vs `f?: T | null`; Python `NotRequired[T]` vs `NotRequired[Optional[T]]`). Bare `true`/`false` are JSON boolean literals (`remove_worktree?: ask|bool`, `requires_confirmation: true` → `{"const": true}`); quote them for the strings. The registry has an entry for every method in every `METHODS` table (params and result), every event type the server emits (subject and data), the notifications (`events.event`, `events.overflow`) and the error kinds (`vk_proto::rpc::ErrorKind::ALL`, code, retryable, `details` shape). Objects are open (unknown fields allowed, 07 §1.5) and every mutating result gains an optional `cursor` (§1.3). Everything else is derived from it: `docs/api/methods.json` (each method's `params`/`result` in the notation), `docs/api/vibeke-1.schema.json`, `vibeke debug api-schema [--out FILE] [--method NAME]` (offline, prints the bundle of the running binary), the `api.schema {method?}` method (`{schema}`: the bundle, or one method's `x-method` entry with the `$defs`) and the generated clients. Bundle layout: shared types under `$defs`, `x-methods {name: {mutating, scope, pane_scope, params, result}}`, `x-events {type: {subject, data}}`, `x-notifications`, `x-errors [{kind, code, retryable, details}]`, `x-transport`. Tests keep the registry honest: every method has a shape and no shape is orphaned, every event type emitted through `.event(…)` is listed, and a live server's results (about 80 methods, including `task.create`/`task.finish {remove_worktree: true}`, `preview.url` before a proxy origin exists, `preview.declare {tls_origin}` and a TLS `preview.open`) and event payloads validate against their shapes twice: with the registry's validator and with a real JSON Schema 2020-12 validator (`jsonschema`, dev-dependency, no network resolvers) against the emitted bundle; a further test checks that the two agree on null/boolean/enum edge cases (`crates/vibeke/tests/api_schema_live.rs`). The preview and browser method shapes were audited against their handlers (all `preview.open` result variants `pane|window|default_browser|proxy`, `mode: pane|window|proxy`, `tls_origin`, `no_open`, the TLS result fields `tls_origin`, `session_ttl_s`, `ca: PreviewCa`; agent `browser.*` session shapes; the preview lifecycle events `preview.discovered|declared|up|down|gone`).
- **Clients as built.** `clients/typescript` (`@vibeke/client`: Node `net`, `VibekeClient.connect/call/events`, `types.gen.ts` with `Methods`, `EventMap`, `ErrorKind`, `isEvent`; events are an async iterator, errors are `VibekeError`, overflow ends the stream with `EventOverflow`; no dependencies, runs on Node >= 22.18 or Bun) and `clients/python` (`vibeke-client`: stdlib asyncio, Python 3.11+, TypedDicts for every shape, `Client` with one typed coroutine per method, `EventStream` async iterator). The generated files (`types.gen.ts`, `types_gen.py`, `api_gen.py`) are checked in; `cargo test -p vibeke --test api_clients` fails when they drift from the registry (`VIBEKE_UPDATE_CLIENTS=1` regenerates) and also runs both clients' unit tests against an in-process mock and both example scripts against a real isolated server (`server.status`, `workspace.list`, an event subscription that receives the `workspace.created` of a workspace the script creates); it skips when `node`/`bun`/`python3` are missing. Typed examples `examples/preview_tls.{ts,py}` declare a preview with `tls_origin`, open it through the proxy over https without a browser and print the CA; they run end to end against the isolated server, and `tsc --noEmit` / `mypy` (when installed; the tests skip otherwise) typecheck both clients with them, including deliberate mistakes that must stay type errors (`remove_worktree: "true"`, `mode: "profile"`). Both runtimes check the socket before connecting exactly like the CLI's `check_socket_trust` (`checkSocketTrust` / `check_socket_trust`: under the runtime root every directory up to the root is a real 0700 directory owned by the caller, never a symlink; an explicit socket elsewhere needs a parent owned by the caller that is not group- or world-writable; the socket must be a socket owned by the caller): a refused socket gets no connection and no byte (the pane token included); an explicit `insecure` option skips the check. A subscription's stream is registered while its `events.subscribe` response line is handled, so events and an `events.overflow` arriving in the same read are applied in order; closing a stream sends `events.unsubscribe` and pushes for unknown subscription ids are dropped, never buffered. Not yet: publishing to npm/PyPI (the TypeScript package is `private` and ships `.ts` sources), a Windows named-pipe transport (M6 Windows), and a release-over-release schema diff in CI (the freeze test covers method flags only).

### 1.6 Remote routing

The local server is the single entry point for clients. A request whose target is machine-qualified, or which carries `"machine": "devbox"` in params, is forwarded over the remote link and the response relayed. `machine.*` methods are always local. CLI: `vibeke --machine devbox <cmd>` sets `machine` on every request.

---

## 2. Method catalog

Notation: `params → result`. `?` = optional. All methods return `seq` when mutating (omitted below for brevity). Milestone in brackets.

### 2.1 `client.*`, `api.*`, `server.*` [M1]

| Method | Params → Result |
|---|---|
| `client.hello` | `{client, version, api, kind: tui|cli|plugin|gateway|agent, token?}` → `{server_version, api, session, machine, capabilities[], features[]}` |
| `client.list` | `{}` → `{clients: [{id, kind, client, version, attached_at, peer: {uid, pid?, machine?}, focused_pane?}]}` |
| `client.appearance` | `{dark: bool, source?: osc11\|csi996}` → `{appearance, colorfgbg}` — host terminal light/dark report (theme propagation, 08 §11) [M4] |
| `client.focus` | `{pane \| url: "vibeke://focus?session=…&pane=…", raise?: true}` → `{pane, client, focused, raised, host}` — focus in the most recently active attached client and raise its host terminal (08 §7.1 click-to-focus). Full scope only [M4] |
| `theme.get` / `theme.set_mode` | `{}` / `{mode: auto\|light\|dark\|null}` → `{appearance, colorfgbg, reports?}` — `set_mode` is a runtime override, not persisted [M4] |
| `status.segments` | `{pane?, client?}` → `{segments: {machine, session, workspace, task, branch, ports, attention, agents_summary, cpu, clock, sync_input}, focus, appearance, client_side: [mode, prefix_indicator]}` — data for the built-in status-bar segments (08 §4) from that client's focus [M4] |

`render.attach` and `client.hello` accept an optional `host: {bundle_id?, term_program?}` (the host terminal's `__CFBundleIdentifier` / `TERM_PROGRAM`), used to raise the window on click-to-focus and to enable native notifications (08 §7.1). *(M4: implemented; the TUI sends it to the server on its own machine only.)*
| `api.schema` | `{method?}` → `{schema}` — the JSON Schema bundle (§1.5), or with `method` that method's `x-method` entry plus `$defs`. `not_found` for an unknown method |
| `api.methods` | `{}` → `{methods: [{name, milestone, capability, mutating}]}` |
| `server.status` | `{}` → `{pid, version, uptime_ms, session, panes, holders: {live, orphaned}, clients, event_seq, db_size, rss}` |
| `server.reload_config` | `{}` → `{changed: [keys], errors: []}` |
| `server.stop` | `{kill_panes: bool = false}` → `{}` — with `kill_panes:false`, holders keep running and the next server reattaches |
| `server.restart` | `{binary?: path}` → `{new_pid}` — exec's the new server. Holders untouched (01 §1.2) |

*As built (Batch 2A, 2026-10-06).* `server.restart {binary?}` → `{new_pid, binary}`: snapshots every pane, answers, then `exec`s `<binary> server --session <s>` in place (default: the server's own binary; `binary` must be an absolute path to an executable file). The pid stays the same (`new_pid` is that pid); every descriptor is close-on-exec, so the new image binds the socket and takes the state lock afresh and reattaches the still-running holders like any server start (`session.server_restarted`). If `exec` fails the old server keeps running. Full scope only. `vibeke server restart [--binary PATH]` calls it and waits until the server answers with a fresh uptime; `--machine` keeps the stop-and-reconnect path. `server.reload_config` (= `config.reload`, §2.14) re-reads `config.toml` and returns `{changed, errors: [{line, col, message}], warnings}`.

*As built (3D, 2026-10-06, `crates/vk-server/src/hardening.rs`).* `storage.status {}` → `{degraded, ephemeral, archive_rows_skipped, db: {path, bytes}, events: {count, first_seq, last_seq, retention: {sync_days, history_days, max_rows, blob_days}}, backups: [{name, schema_version, created_at, bytes}], keep_backups, blobs: {count, bytes}, cursor: {machine_uuid, session_uuid, log_epoch}}` reports the event log, the effective `[events]` retention, the pre-migration backups and degraded-mode state; `storage.prune {}` runs the retention sweep now (events past retention and over the row cap, old turns, unreferenced old blobs) → `{events_aged, events_capped, events_remaining, stream_removed, blobs_removed, blob_bytes}`, refused with `storage_unavailable` while degraded. Both are full scope only. `server.status` carries `ephemeral` (UI changes applied in memory only) next to `degraded`.

### 2.2 `session.*` [M1]

| Method | Params → Result |
|---|---|
| `session.snapshot` | `{include?: [workspaces, tabs, panes, runs, interactions, tasks, previews, layouts, machines]}` → `{at_seq, workspaces[], groups[], tabs[], panes[], runs[], interactions[], tasks[], previews[], layouts[], focused: {client_id → {workspace, tab, pane}}}` |
| `session.list` | `{}` → `{sessions: [{name, running, pid?, socket}]}` (scans the runtime dir) |
| `session.create` | `{name}` → `{session}` (spawns a server) |
| `session.stop` | `{name, kill_panes?: false}` → `{}` |
| `session.rename` | `{name, new_name}` → `{session}` |

*As built (Batch 2A, 2026-10-06, `crates/vk-server/src/session_api.rs`).* A session entry is `{name, running, socket, state, current, pid?}`. `session.list` scans the runtime root (a `vibeke.sock`) and the state root (a `state.db`) and probes each socket. `session.create {name}` validates the name (1-64 of `[A-Za-z0-9._-]`, no leading dot), refuses a running one (`conflict: name_taken`), spawns `<binary> server --session <name>` detached (its own session id, log in that session's `logs/server.log`) and waits up to 10 s for its socket. `session.stop {name, kill_panes?}` on the current session is `server.stop`; on another session it calls that server's `server.stop` and waits until its socket is gone; not running answers `stopped: false`. `session.rename {name, new_name}` moves the state and runtime directories of a stopped session: a running session is `conflict: session_running`, a stopped one whose holders still listen (stopped without `--kill-panes`) is `conflict: holders_live`, an existing target `conflict: name_taken`. `create`, `stop` and `rename` are full scope only. CLI `vibeke session list|new <name>|stop <name> [--kill-panes]|rename <a> <b>`. *3D:* `session.info` (the Session entity) and the `session.started`/`session.stopped` events are described under §2.3.


| Method | Params → Result |
|---|---|
| `machine.list` | `{}` → `{machines: [Machine]}` |
| `machine.add` | `{label, address: "user@host[:port]", transport?: ssh|quic, ssh_opts?: [..], install?: auto|ask|never}` → `{machine}` |
| `machine.connect` / `machine.disconnect` | `{machine}` → `{machine}` |
| `machine.remove` | `{machine}` → `{}` |
| `machine.status` | `{machine}` → `{machine, rtt_ms, bandwidth_kbps, remote_version, sessions[]}` |
| `machine.install` | `{machine, channel?}` → `{version}` — install/upgrade the remote binary (confirmation required unless `--yes`) |

*As built (3D, 2026-10-06, `crates/vk-server/src/machines.rs`).* The Machine entity (02 §1.1) is persisted per session (`machine` entities; `session_info` for the Session). The server registers its own machine at start (`id` = the machine uuid, `kind: local`, `os`, `arch`, `vibeke_version`, `status: connected`, `last_seen_ms` = now). Remote federation stays client-owned, so a client reports a remote it connected to with `machine.upsert {label, kind?: ssh|quic = ssh, id?, address?, os?, arch?, vibeke_version?, status?: connected|connecting|degraded|offline, reason?}` → `{machine, created, cursor}` (id defaults to `m-<blake3(label)[..16]>`; this machine and a `local` kind are refused, `conflict` / `invalid_params`), and reports link changes the same way. `machine.list {}` → `{machines: [Machine], local}` (this machine first, then remotes by label); `machine.get {machine: id|label}` → `{machine}`; `machine.remove {machine}` → `{removed, cursor}` (not this machine). `session.info {}` → `{session: {id, name, machine_id, created_at_ms, server_pid, server_version}, machine, cursor}`. Events: `session.started {pid, version, machine, name, prev_pid, fresh}` at every server start (`prev_pid` is the pid the stored Session last recorded, so a crash shows as a start without a `session.stopped`), `session.stopped {pid, reason}` on a graceful stop, `machine.added {label, kind, address}`, `machine.connected`, `machine.disconnected {reason?}`, `machine.degraded {reason?}` (only on a status transition; `connecting` and repeated reports are state only, `last_seen_ms` moves on connected/degraded reports), `machine.removed`. `machine.upsert` and `machine.remove` are full scope only; `session.info`, `machine.list` and `machine.get` are open to panes. The CLI's `vibeke machine …` verbs (saved machines, link verbs) still run client-side against `[remote.machine]` and do not call these methods; the TUI/remote client does not report to `machine.upsert` yet (client-owned, 1E's code): until it does, the registry holds this machine and whatever API clients report. `machine.add|connect|disconnect|status|install` of this table remain the client verbs.

### 2.4 `group.*`, `workspace.*` [M1]

| Method | Params → Result |
|---|---|
| `group.create` | `{name, parent?}` → `{group}` |
| `group.rename` / `group.move` / `group.delete` / `group.collapse` | `{group, …}` → `{group}` |
| `group.list` / `group.add` / `group.remove` | `{}` → `{groups: [Group + {agent_summary}], ungrouped}`. `{group, workspace, index?}` / `{workspace}` → `{workspace, group}` |

*Implementation (M4):* `Group {id, handle g<N>, name, parent?, collapsed, order, workspaces: [workspace_id]}`. Membership is the group's ordered `workspaces` list, not a `group_id` on the workspace (02 §1.1 deviation; a workspace is in at most one group). `group.move {parent?, index? | delta?}` refuses cycles. `group.delete` moves members and child groups up to the parent; nothing is closed. `workspace.create {group}`, `workspace.move {workspace, group: id | ""}` and `workspace.list {group?}` (every entry gains `group`) apply. All `group.*` mutations are refused for pane scope. `session.snapshot` includes `groups`.
| `workspace.list` | `{group?}` → `{workspaces: [Workspace + {agent_summary: {working, needs_input, done, idle}, tab_count, pane_count}]}` |
| `workspace.get` | `{workspace}` → `{workspace}` |
| `workspace.create` | `{cwd, name?, group?, focus?: false, layout?: LayoutSpec, command?: argv}` → `{workspace, tab, root_pane}` |
| `workspace.rename` | `{workspace, name: string|null}` → `{workspace}` (null clears to auto-name) |
| `workspace.move` | `{workspace, group?, index}` → `{workspaces}` |
| `workspace.focus` | `{workspace, client?}` → `{workspace}` |
| `workspace.close` | `{workspace, force?: false}` → `{}` — refuses with `conflict:pane_busy` if a non-shell foreground process runs, unless `force` |

### 2.5 `tab.*` [M1]

| Method | Params → Result |
|---|---|
| `tab.list` | `{workspace?}` → `{tabs}` |
| `tab.create` | `{workspace, cwd?, title?, focus?: false, command?: argv, layout?: LayoutSpec}` → `{tab, root_pane}` |
| `tab.rename` | `{tab, title: string|null}` → `{tab}` |
| `tab.move` | `{tab, index, workspace?}` → `{tabs}` (numbers are stable. Order changes) |
| `tab.focus` | `{tab, client?}` → `{tab}` |
| `tab.close` | `{tab, force?}` → `{}` |

### 2.6 `pane.*` [M1]

| Method | Params → Result |
|---|---|
| `pane.list` | `{workspace?, tab?, has_agent?}` → `{panes}` |
| `pane.get` | `{pane}` → `{pane, run?, open_interactions[]}` |
| `pane.current` | `{}` → `{pane}` (requires pane token / `@current`) |
| `pane.split` | `{pane, direction: right|down|left|up, ratio?: 0.5, cwd?, command?: argv, env?: {k:v}, focus?: false, title?}` → `{pane}` |
| `pane.float` | `{tab, rect?: {x%,y%,w%,h%}, cwd?, command?, focus?}` → `{pane}` [M4] — *implemented:* without `pane`, a new floating pane (default 70%×70% centered). With `pane`, floats a tiled pane (`conflict` for a tab's last tiled pane) or moves/resizes/raises a floating one. Floats live in `Tab.floating: [{pane, x, y, w, h, z}]` (percent), not in `layout` |
| `pane.embed` | `{pane, target?, direction?: right, ratio?}` → `{pane, tab}` — a floating pane back into the tiling, split next to `target` (default the focused tiled pane) [M4] |
| `tab.floats` | `{tab?, visible?}` → `{tab}` — show/hide all floats (`Tab.floats_hidden`. Toggles without `visible`) [M4] |
| `pane.move` | `{pane, to: {tab} | {workspace} | {new_tab_in: workspace}, position?}` → `{pane, previous_pane_handle}` |
| `pane.resize` | `{pane, direction, cells?|percent?}` → `{layout}` |
| `pane.zoom` | `{pane, zoomed?: toggle}` → `{tab}` |
| `pane.focus` | `{pane, client?}` → `{pane}` |
| `pane.rename` | `{pane, title: string|null}` → `{pane}` (emits `pane.title_changed`) |
| `pane.close` | `{pane, force?}` → `{}` |
| `pane.send_text` | `{pane, text, paste?: auto|bracketed|raw = auto}` → `{bytes}` — `auto` honors the pane's live bracketed-paste mode *(notify ok)* |
| `pane.send_keys` | `{pane, keys: [string]}` → `{}` — key grammar §2.6.1. All keys validated before any byte is written |
| `pane.run` | `{pane, command: string, wait?: bool, timeout_ms?}` → `{}` or (with wait) `{exit_code?, output_tail}` — sends text + Enter. With `wait`, uses OSC 133 prompt marks when the shell integration is active, else falls back to "foreground process returns to shell" |
| `pane.read` | `{pane, source: visible|recent|recent_unwrapped|scrollback|detection, lines?: 200, from_line?, format?: text|ansi|cells, include_cursor?}` → `{text|cells, rows, revision, truncated, scroll}` |
| `pane.wait_output` | `{pane, match?: string, regex?: string, source?, timeout_ms?, since_revision?}` → `{matched: string, line, revision}` |
| `pane.wait_idle` | `{pane, quiet_ms: 2000, timeout_ms?}` → `{revision}` |
| `pane.mark_unread` / `pane.mark_seen` | `{pane}` → `{pane}` |
| `pane.pin` | `{pane, pinned: bool}` → `{pane}` |
| `pane.sync_input` | `{panes: [pane], enabled: bool}` → `{group_id}` — synchronized input |
| `pane.scroll` | `{pane, to: bottom|top|line, line?, delta?}` → `{scroll}` |
| `pane.screenshot` | `{pane, format?: png|svg|html}` → `{blob}` — renders the pane grid (for bug reports and Phase 2) [M3] |

*As built (Batch 2A, 2026-10-06, `crates/vk-server/src/pane_api.rs`).* `pane.move {pane, to: {tab} | {workspace} | {new_tab_in: workspace}, direction?: right, anchor?, focus?}` → `{pane, tab, previous_pane_handle, source_tab_closed}` moves a tiled pane (its process keeps running): next to `anchor` or the destination's focused pane, into the workspace's focused (else first) tab, or into a new tab. Across workspaces the pane takes a handle in the destination workspace (`w2:p7` keeps its number); an emptied source tab closes; the only pane of a workspace's only tab cannot leave its workspace (`conflict: last_pane`); floating panes are `conflict: floating` (embed first). Events `pane.moved`, `tab.layout_changed`, `tab.created|closed`. Pane scope: own panes, same workspace only. `pane.scroll {pane, to?: bottom|top|line, line?, delta?, client?}` → `{scroll: {offset, total, at_bottom}}`: scrolling is client-side (the TUI's viewport), so the server publishes the request as `pane.scroll_requested {offset, total, client}` (offset = rows above the live screen, clamped to the scrollback; `delta` is relative to the last requested offset) and clients that follow it move their view; the TUI does not consume it yet (lane 2B). `pane.screenshot {pane, format?: ansi|text|html = ansi, source?: visible|recent, lines?, include_cursor?, inline?}` → `{blob: {hash, size, mime, path}, format, source, cols, rows, lines, revision, cursor?, data?}` captures the grid from the VT engine: `text` (trimmed lines), `ansi` (SGR colors and attributes, each row reset) or `html` (a standalone dark page, xterm-256 palette), stored in the session blob store (`blob.get`); `inline` also returns the text. CLI `vibeke pane move|scroll|screenshot` (`--to-tab`, `--to-workspace`, `--new-tab-in`; `screenshot --out FILE`).

*As built (v1 remainder, 2026-10-06; `pane_render.rs`, `sync_input.rs`, `tab_renumber.rs`).* `pane.screenshot {format: svg|png}` renders the same grid in the pane's palette (theme default colours, catppuccin mocha or latte by appearance; xterm cube for 16–255) on an 8x16 cell grid with an 8 px margin: SVG as one `<text>` per span stretched to its columns (`textLength`) over background runs, bold/italic/dim/underline/strike/inverse/hidden as attributes; PNG rasterized in-process with the dependency-free `font8x8` bitmap font (ASCII, Latin-1, box drawing, blocks, Greek, Hiragana; wide characters centred in two cells, others a hollow box), encoded with `image` (already in the graph through vk-browser), capped at 24 Mpx (`invalid_params {reason: too_large}`). Results add `width`, `height`; `inline` PNG comes as `data_b64`; `include_cursor` outlines (SVG) or inverts (PNG) the cursor cell. `pane.sync_input {action?: start|stop|status, enabled?, panes?|tab?, pane?, group?, all?, include_agents?}` → `{group_id, group?, excluded?, stopped?, groups}` keeps server-side sync groups (memory only): input a user sends to one member (attached clients' keys/pastes/mouse, full-scope `pane.send_text|keys|bytes`) is written to the other members; pane-scoped input never fans out. Agent panes join only with `include_agents` (`excluded: [{reason: agent}]` otherwise) and a member that starts an agent later stops receiving unless it was included; browser panes, plugin surfaces and exited panes never join; members with an open interaction are skipped. A pane is in at most one group; a group left with one member ends. Event `pane.sync_input_changed {enabled, panes, tab, reason}`; `status.segments` reports the focused pane's group in `sync_input`. It is independent of the TUI's client-side sync (both on the same panes mirror twice). Full scope only. `tab.renumber {workspace?}` → `{workspace, tabs, changed}` numbers the workspace's tabs 1..n in their order (handles `wN:tM` follow, the next tab gets n+1; pane handles unchanged), event `tab.renumbered {tabs: [{tab, from, to}]}`, full scope only. CLI `vibeke pane sync-input start|stop|status [--panes a,b|--tab t] [--include-agents]`, `vibeke tab renumber [workspace]`.

**`pane.read` never scrolls the user's view.** A scrollback read on alt-screen agents can drive the agent's mouse-scroll interface and visibly scrolls the operator's terminal. Vibeke serves history from the VT engine's scrollback plus the scrollback archive (01 §4); for alt-screen agents with structured adapters, transcript history comes from `agent.transcript` (§2.7) instead.

`revision` is a real per-pane monotonically increasing counter (bumped on every damage batch). It is load-bearing: `pane.wait_output {since_revision}` and clients' race guards may rely on it.

#### 2.6.1 Key grammar (shared by `pane.send_keys`, `agent.send_keys`, config keybindings)

- Named keys (case-insensitive): `enter tab esc|escape space backspace|bs delete|del insert home end pageup|pgup pagedown|pgdn up down left right f1…f24`, plus `minus comma period slash backslash semicolon quote backtick lbracket rbracket equal plus ampersand`.
- Single characters are typed literally (`"1"`, `"y"`, `"é"`).
- Chords: `ctrl+c`, `alt+shift+p`, `cmd+k`, `super+x`, modifiers in any order. Modifiers: `ctrl shift alt|meta|opt cmd super hyper`.
- Herdr's grammar is a strict subset, so Herdr scripts work unchanged; Vibeke additionally accepts the keys Herdr rejects (`pageup`, `home`, `end`, `delete`, `insert`).
- Encoding is per pane mode: legacy xterm, `modifyOtherKeys`, or kitty keyboard protocol flags as negotiated by the child (03 §keyboard).
- Literal tmux syntax (`C-c`) is rejected with `invalid_key` and a hint.

### 2.7 `agent.*` [M1]

| Method | Params → Result |
|---|---|
| `agent.list` | `{workspace?, state?: [AgentState], harness?}` → `{runs: [AgentRun + {pane, open_interactions: n}]}` |
| `agent.get` | `{target}` → `{run, pane, open_interactions[], last_turn?}` |
| `agent.start` | `{pane, harness, name?, mode?: tui|headless = tui, args?: [..], env?, model?, task?, ready_timeout_ms?: 30000}` → `{run}` — requires an available shell pane at its prompt (`conflict:pane_busy` otherwise). Returns once the adapter (or detector) reports `idle` |
| `agent.spawn` | `{harness, name?, where: {split_of: pane, direction?} | {new_tab_in: workspace} | {task: task} , prompt?, args?, focus?: false}` → `{pane, run}` — Creates a pane or tab, starts the agent, and optionally sends the first prompt. |
| `agent.prompt` | `{target, text, images?: [blob|path], mode?: send|steer|follow_up = send, wait?: bool, until?: [AgentState], timeout_ms?}` → `{run, turn?}` — submits text + Enter atomically via the best channel (RPC `prompt`/`steer` for headless and extension-capable harnesses, bracketed paste + Enter otherwise). If the run is not working and no lifecycle change occurs within 5 s → `stalled` |
| `agent.wait` | `{target, until?: [WaitCondition] = [idle, done, needs_approval, needs_answer, error, exited], timeout_ms?}` → `{run, state, interaction?}` — `WaitCondition` is an execution state (04 §2.4) or a derived condition. `needs_approval`/`needs_answer` means an open interaction of that type. `done` means idle after a turn completes since the wait started. It does not mean that the user read the result. |
| `agent.interrupt` | `{target}` → `{run}` — native abort where available (`abort` RPC, Esc for TUIs) |
| `agent.send_keys` | `{target, keys}` → `{}` |
| `agent.read` | `{target, source?: visible|recent|transcript, lines?, format?}` → as `pane.read`, plus `transcript` returns the last N turns from the structured log |
| `agent.transcript` | `{target, after_turn?, limit?: 20, include_items?: summary|full}` → `{turns: [Turn + {items}]}` — structured adapters only (`unsupported` + fallback hint otherwise). As built (gateway and `before`/`items` callers): `{target, before?, limit?}` → `{run, turns: [{n, ts, items: [{kind: text|thinking|tool_call|tool_result, ts (epoch ms or null), …}], duration_ms (last item ts − turn start, null when unknown), tool_count, subagent_count}], next_before}`. `subagent_count` counts tool calls that start a subagent (`Task`/`Agent`, or the agent-spawning tool in Codex rollouts) |
| `agent.turns` | `{run, after_seq?, limit?: 100}` → `{run, turns: [Turn], next_after_seq}` — the Turn/Item stream (02 §1.1), full scope only; `run` is a handle, a live run id, or an ended run's id. As built (3D) in §2.7a |
| `agent.items` | `{turn?, run?, kind?, after_seq?, limit?: 200}` → `{items: [Item], next_after_seq}` — items of one turn (ordered by `seq`, pageable) or of a whole run (recording order); full scope only |
| `agent.rename` | `{target, name: string|null}` → `{run}` |
| `agent.release` | `{target}` → `{}` — stop tracking (the process keeps running as an untracked pane occupant) |
| `agent.resume` | `{pane?, run: ended_run_id, mode?}` → `{run}` — re-launches via the harness resume argv in the same or a new pane |
| `agent.report` | adapter-only — see 04 §adapter protocol (`{run?, pane, source, state?, harness_session_id?, transcript_path?, resume?, turn?, item?, seq}`) *(notify ok)* |
| `agent.harnesses` | `{}` → `{harnesses: [{id, display, version_detected?, integration_installed, capabilities: {native_approval, native_question, steer, transcript, resume, headless}}]}` |

### 2.7a Turn/Item stream [3D, as built 2026-10-06]

`agent.turns` and `agent.items` read the structured record of what a run did (02 §1.1 Turn/Item), kept in `state.db` as `stream_turn` / `stream_item` entities (separate from the tracking `turn` / `tool_item` records of 15, which hold exact prompts for tracked tasks). `crates/vk-server/src/items.rs` maps the hook vocabulary that every harness family ends in (`on_signal`: Claude, Codex, Gemini, OpenCode, pi/omp extensions, ACP) onto it: `UserPromptSubmit` opens a turn (a turn still open is closed `interrupted`) with a `user_message` item; `PreToolUse` a `command` (shell tools) or `tool_call` item (open until its result); `PostToolUse` closes it and adds a `tool_result` item (and, for `Edit`/`Write`/`MultiEdit`/`NotebookEdit`, a `file_change` item with `{path, op, lines_added?, lines_removed?}`); `PostToolUseFailure` an `error` item; `Stop`/`Interrupt` an `assistant_message` item and the turn's end (`completed`/`interrupted`); `StopFailure` an `error` item and `failed`; `SubagentStart/Stop` a `subagent` item plus the events `agent.subagent_started/finished {agent_id, agent_type}`. Items arriving with no open turn open an implicit one. A run ending closes its open turn in the same transaction. Other transports record through the same functions (`turn_started`, `item`, `item_finished`, `turn_ended`, `usage_updated`).

Every item emits one `agent.item {kind, summary, item, turn, seq, payload_ref}` event (sync tier, item granularity, never per token). Summaries are whitespace-collapsed, `vk_redact`-redacted and cut at 200 characters; text longer than 2 KiB (tool output, long messages) is redacted, cut at 1 MiB and stored in the unified blob store (`source: payload`, readable by the owning pane's workspace) and named by `payload_ref` (a blake3 hash for `blob.get`); the event never carries it. Per-turn usage is the delta of the run's session totals against the totals when the turn began (`usage_baseline`); the transcript parse lands after `Stop`, so `usage` is filled shortly after the turn ends. Retention: turns that ended before `events.history_retention` are removed with their items by the hourly sweep (02 §2.3); payload blobs they referenced become collectable by `blob.gc`. Both methods are full scope only (items carry tool summaries). Tests: `crates/vk-server/src/items_tests.rs`.

### 2.8 `interaction.*` [M1]

| Method | Params → Result |
|---|---|
| `interaction.list` | `{status?: open, run?, workspace?, kind?}` → `{interactions}` — sorted by `opened_at` in Phase 1. Phase 2 adds ranking |
| `interaction.get` | `{interaction}` → `{interaction}` |
| `interaction.answer` | `{interaction, decision?: allow|allow_always|deny, choices?: {qid: [oid]}, text?, scope?: once|session|rule, rule?: PolicyRule, idempotency_key?, actor?, expected_decision_rev?}` → `{interaction, delivery: {state, channel: native|keystrokes}}` — `state` per the delivery state machine in 02/04. Repeating the same `idempotency_key` with the same answer returns the recorded state (`duplicate: true`) and never delivers again. A different answer under that key is a `conflict`. `actor` (full-scope callers only) labels `answered_by`, e.g. `gateway:the maintainer's phone` (16 §7.7). The key is kept separately as `answer_key`. `expected_decision_rev` makes the answer a compare-and-set: if the interaction moved on, the call fails with `conflict` (`stale: …`). The server returns `permission_denied:self_answer_forbidden` for tokens from the run's own pane or any descendant pane or run. This call grants authorization (09 §5.1.1). |
| `interaction.cancel` | `{interaction}` → `{interaction}` (user dismisses. Adapter delivers deny/escape) |
| `adapter.interaction.open` | adapter-only `{pane, run?, kind, …payload}` → `{interaction}` |
| `adapter.interaction.await` | adapter-only `{interaction, timeout_ms}` → `{answer}` or `timeout` — long-poll used by blocking hooks/extensions. *Retrieving* a decision for the caller's own pane is allowed (09 §5.1.1) |
| `adapter.interaction.resolve` | adapter-only `{interaction, resolution: resolved_elsewhere|cancelled|expired}` → `{}` |

Keystroke delivery (screen-only harnesses) is **verified**: the adapter sends navigation keys, re-reads the detector screen, and only sends Enter when the highlighted option matches the chosen option (a proven technique, now in the server so every client benefits). On mismatch → `interaction.delivery_failed {reason: "selection_mismatch"}` and the interaction stays open. Keystroke delivery is best-effort (04 §7.3 rule 5).

### 2.9 `policy.*` [M1]

| Method | Params → Result |
|---|---|
| `policy.list` | `{scope?}` → `{rules}` (merged view: global, user, trusted repo files) |
| `policy.add` | `{rule: PolicyRule}` → `{rule}` |
| `policy.remove` | `{rule_id}` → `{}` |
| `policy.test` | `{action: {tool, command?, paths?, url?}, scope}` → `{effect, rule?}` — dry-run |
| `policy.trust` | `{path, digest?}` → `{repo, digest, setup_script}` — trust a repo-local `.vibeke/` directory at its current blake3 digest (09 §4). `digest` makes it conditional on the reviewed content (conflict if changed). Full scope only. |

*As built (lane 2A security, 2026-10-06; `vk-server/src/policy_api.rs`, `security.rs`).* All `policy.*` are full scope only. `policy.list {scope?: path | {cwd|repo|pane|run}}` → `{rules: [PolicyRuleInfo], repos: [{repo, file, exists, trusted, allow_policy_grants, rules, errors}]}`; `PolicyRuleInfo = {id, source: config|user|repo, repo?, match: {tool?, command_regex?, path_glob?, url_glob?}, effect: allow|deny|ask, scope?, note?, ignored?, created_at_ms?, created_by?}`. `policy.add` takes `{rule}` or the same fields at the top level (CLI `vibeke policy add --effect allow --tool Bash --command-regex '^ls'`) and needs at least one matcher. `policy.remove {rule_id}` removes API-added rules only (config and repository rules live in their files: `invalid_params`). `policy.test {action: {tool, command?, paths?, url?}, scope?}` → `{effect, rule, user_rule, repo_rule, repo, reason}` (CLI flags `--tool --command --path --url --scope`). `policy.trust` additionally takes `allow_policy_grants?: bool` and returns `allow_policy_grants`, `policy_rules` and `policy_errors?`. Evaluation and sources: 09 §4 "As built: approval policy". Related security methods (09): `auth.revoke_token {pane}`, `auth.elevate {reason?, timeout_ms?, request?, wait?}`, `auth.elevate.decide {request, decision}`, `auth.list` (09 §3.2); `audit.tail|search|verify` (09 §11); `integration.doctor {harness?}` (09 §5.3). CLI: `vibeke pane revoke-token`, `vibeke auth revoke-token|elevate|decide|list`, `vibeke audit tail|search|verify`, `vibeke doctor --audit`, `vibeke debug bundle [--out f] [--include-scrollback] [--include-pane p]` (09 §9.5).

### 2.10 `task.*`, `worktree.*` [M1; remote tasks M3; tracking per 15 T1–T4]

Pane-scoped callers (agents) may read tasks but not `task.track`, `task.intent.update`, `task.bind/unbind`, `task.set`, `task.message.*` (except get), `task.review.accept`, `task.check.run/cancel`, `attention.update`, or the T4 mutations `task.review.snapshot`, `task.review.request_reviewer`, `task.review.start_reviewer`, `task.review.note.classify` and `task.dependency.add/remove` (15 §11). All tracking mutations accept `idempotency_key`; a repeat with the same key and payload returns the recorded result with `replayed: true`, a different payload is `conflict{reason: idempotency_key_reused}`.

| Method | Params → Result |
|---|---|
| `task.create` | `{title, repo: path, base?: ref, isolation?: worktree|none, slug?, branch?, agents?: [{harness, name?, prompt?}], setup?: bool = true, ports?: n, group?}` → `{task, workspace, panes[], runs[]}` — `isolation` is `worktree|none|auto` (default from `tasks.checkout`. `auto` is a worktree). The choice is recorded as `Task.checkout`. `jj_workspace` is refused with `invalid_params` (jj support was removed for v1, 2026-10-06). `task.finish {remove_worktree}` removes the worktree (never for `none`) |
| `task.list` | `{status?, repo?}` → `{tasks}` |
| `task.get` | `{task}` → `{task, workspace, runs, previews, collisions[]}` |
| `task.park` / `task.resume` | `{task}` → `{task}` — park = stop agents gracefully, keep worktree |
| `task.finish` | `{task, remove_worktree?: ask|true|false, delete_branch?: false}` → `{task}` |
| `task.archive` | `{task}` → `{task}` |
| `task.setup_log` | `{task}` → `{text}` |
| `task.track` | (15 T1) `{run | pane, turn? | turns?: [n], title?, objective?, criterion?: text | [text | {text, required?, evaluation?, checks?}], constraint?, stop_at?, stop_detail?, target_branch?, idempotency_key?}` → `{task, intent, binding, baseline, review_base}` — attached task + intent revision 1 + binding, atomically. `conflict{reason: binding_unverified}` unless the run's identity is deterministic (structured session id). Never spawns, sends or runs setup |
| `task.sources` | `{run | pane, limit?}` → `{run, identity_verified, native_conversation_id, turns: [{n, prompt, native_conversation_id, started_at_ms, ended_at_ms}]}` — exact recorded prompts for the Track form |
| `task.detail` | `{task}` → `{task, intent, bindings, runs, baseline, uncommunicated, messages}` |
| `task.intent.get` | `{task, revision?}` → `{intent, current_revision, revisions, uncommunicated: [criterion_id]}` |
| `task.intent.update` | `{task, expected_revision?, title?, objective?, criterion?/criteria?, add_criterion?, constraint?, stop_at?, stop_detail?, idempotency_key?}` → `{task, intent, uncommunicated, note}` — record-only. Never sends |
| `task.bind` / `task.unbind` | `{task, run | pane, role?, start_turn?, end_turn?}` → `{binding, pending}` (a switch away from another task's active binding takes effect at the next turn boundary. A suspended binding is continued) / `{task, binding?}` → `{closed}` |
| `task.set` | `{task, expected_rev?, priority?, effort?: quick|minutes|deep|unknown, effort_source?: user|heuristic|assistant:<request>}` → `{task}` — `effort_source` (default `user`) is recorded in `task.updated`. Estimates are only ever applied through this explicit call |
| `task.message.prepare` / `.send` / `.get` / `.cancel` | `{task, text, communicates_intent?}` → `{message, recipient, send_path: prompt_input|open_pane_only, unsafe?}`. `{message, retry?, retry_despite_unknown?}` → `{message}` or `conflict{reason: send_unsafe, detail, fallback: open_pane_to_send}` with zero bytes written. Delivery states `prepared|sending|delivered|delivery_unknown|failed|cancelled` (15 §9) |
| `task.review.get` / `.candidates` / `.diff` / `.accept` | (15 T2) deterministic package (`label`, `assessment`, `observed_commands`/`claims`, `checks`, `check_runs`, `acceptance {acceptance, status, outdated_reasons}`, `sources_verified`, `actions.accept`. Flat client aliases `revision`, `criteria`, `observed`, `blockers`). `diff {task, subject?, path?, max_bytes?}` → `{diff, truncated, total_bytes, base_sha, head_sha}` from the subject's commits only. `accept {task, intent_revision, subject_id, exceptions:[{criterion, reason}], package_revision?}` validated against a state token under the commit lock (`conflict{reason: review_changed|subject_not_committed|exceptions_required|sources_unverified}`). T4 adds `review_notes`, `reviewer_runs`, `dependencies`, `effort` and `snapshot` to the package, accepts `dirty_snapshot` subjects, and `diff` returns `content_sha` (the snapshot commit for a snapshot) |
| `task.review.snapshot` | (15 T4) `{task, idempotency_key?}` → `{subject, snapshot: {commit, tree, staged_tree, ref_name, attempts}, label, note}` — captures staged + unstaged + untracked (binary included. Ignored files excluded) work through a private index copy into an immutable commit kept under `refs/vibeke/snapshots/<commit>`. The user's index, files and branches are untouched. Consistent only when the checkout's change digest and HEAD agree before and after (3 attempts): otherwise `conflict{reason: workspace_changing, label: "Workspace changing — verification subject unavailable"}`. A clean checkout is `conflict{reason: nothing_to_snapshot}`. The `dirty_snapshot` subject is the current, accept-capable candidate while the checkout still holds exactly that content. `task.check.*` run on it in a disposable checkout of the snapshot commit (same per-candidate authorization). Acceptance revalidates the digest (`review_changed`) |
| `task.review.request_reviewer` / `.start_reviewer` | (15 T4) `{task, harness?: claude, subject?, prompt?}` → `{request, prompt, prompt_digest, requires_confirmation: true, confirm_with}` — a reviewable prompt built from the package (or the user's edited text). Launches and sends nothing. `start_reviewer {request, prompt_digest, pane? | split_of?, direction?}` → `{request, run, binding}` — `conflict{reason: prompt_mismatch}` (nothing launched) unless the digest names the prepared prompt. Starts the harness via the `agent.start` path (the prompt is its initial prompt, sent once. No focus change) and binds the run with role `review`. Repeat → `replayed` |
| `task.review.notes` / `.note.classify` | (15 T4) `{task}` → `{notes, reviewer_runs}`. Settled reviewer turns become notes (`FINDING [blocking|concern|nit] …` lines, else one `unstructured` note. `category: agent_claim`, author = the reviewer run). Unassessed blocking/concern/unstructured notes and user-marked blockers on the reviewed subject are `blocking_concern` blockers (no Ready). `classify {note, classification: blocking|not_blocking|dismissed, reason?}` — user only. Dismissing needs a reason (`conflict{reason: classification_refused}`) |
| `task.dependency.add` / `.remove` / `.list` | (15 T4) `{task, depends_on, kind?: blocks|related}` → `{edge {id, task, depends_on, kind, confirmed_by, created_at_ms}}` — `conflict{reason: dependency_cycle, path} | self_dependency | duplicate}`. `remove {edge} | {task, depends_on, kind?}` closes the edge (history). `list {task?}` → `{dependencies: {depends_on, dependents, blocks_open_tasks}}`. Event `task.dependency_changed {action: added|removed, kind}` (history tier). `attention.list` items carry `blocks_tasks` (open tasks waiting through confirmed `blocks` edges, transitively) and explain "blocks N linked tasks" |
| `task.effort.estimate` | (15 T4) `{task}` → `{effort: {set, heuristic: {effort, source: heuristic, label, reasons}}, model_estimate}` — read-only. The heuristic (diff lines/files, failing checks, human criteria) is also in `task.review.get` (`effort`) and as `attention.list` items' `effort_estimate {effort, source: heuristic}` when the user has not set effort. Ranking keeps using the user's value |
| `task.check.list` / `.authorize` / `.run` / `.get` / `.cancel` | per-candidate authorization (15 §6.3). `definition_digest` must match the definition the user saw (`conflict{reason: definition_changed}`). `run` without a grant → `conflict{reason: authorization_required}`. One execution per idempotency key |
| `attention.list` / `attention.update` | (15 T3) ranked items with explanations, `coverage {complete, notes, scope?, excluded?}`, optional `five_minute`. `update {key, seen?, snooze_until_ms?, pin?}` never answers or accepts |
| `task.operation.get` | `{idempotency_key}` → `{known, method?, result?, at_ms?, expired?}` — mutation receipt (30-day window). Unknown never means safe to repeat |
| `worktree.list` | `{repo?: path, cwd?: path}` → `{worktrees: [{path, branch, head, task?, workspace?, locked, prunable}]}` |
| `worktree.create` | `{repo|cwd, branch, path?, base?, open?: bool, focus?: false}` → `{worktree, workspace?}` |
| `worktree.open` | `{path, focus?: false}` → `{workspace}` |
| `worktree.remove` | `{path, force?: false}` → `{job}` — Runs asynchronously. Reports progress through the `worktree.removed` event |
| `worktree.repo_root` | `{cwd}` → `{repo_root, vcs}` |

*As built (Batch 2A, 2026-10-06, `crates/vk-server/src/task_park.rs`).* `task.park {task}` → `{task, stopped: [{run, handle, pane, harness, name, pane_closed, resumable}]}` on an `active` task: each live run bound to the task or running in its workspace is interrupted (`agent.interrupt`), then its harness process group gets SIGTERM (an agent started from a shell leaves the shell pane open; an agent that is the pane's own process closes the pane), and the run ends with reason `parked`. The worktree, workspace, shells and port lease stay. `task.resume {task}` → `{task, resumed: [AgentRun], skipped: [{run, reason}]}` restarts each stopped run from its native session through the `agent.resume` path (its old shell pane when free, else a new tab of the task's workspace); runs without a resume handle are skipped. An attached task (15 §4.3) only changes its record. Events `task.status_changed`, `task.parked {runs}`, `task.resumed {runs, skipped}`; `conflict` for parking a parked or finished task and resuming one that is not parked. Full scope only. `task.create {dry_run: true}` returns the plan (`repo_root`, `slug`, `plan: {checkout, path, branch, base}`) without creating anything; `worktree.remove {dry_run: true}` reports `{dirty_files, unpushed_commits, would_remove}` and removes nothing. 

*As built (v1 remainder, 2026-10-06; `task_lifecycle.rs`, `vk_tasks::recreate_worktree`, `PortLeases::re_lease`).* `task.setup_log {task, max_bytes?: 262144}` → `{task, path, exists, text, size, truncated, setup_status}` (tail of `<worktree>/.vibeke/setup.log`, cut at a line boundary; pane scope: own workspace's tasks). `task.archive {task, force?}` → `{task, job, stopped, branch_kept, worktree_removed}` refuses a dirty checkout or unpushed commits (`conflict {reason: dirty_checkout, dirty_files, unpushed_commits}`) unless `force`, then parks the agents (resume handles kept in the park record), removes the worktree (branch kept), releases the ports and closes the record (event `task.archived`); an attached task only changes its record. `task.adopt {path | pane, title?, slug?, focus?}` → `{task, workspace, created_workspace, warnings}` records an existing git worktree (or a pane's cwd) as an owned task without moving anything: a port block is leased, the workspace rooted there (the pane's with `pane`) gets the task, no setup runs; the main working tree becomes a `checkout: none` task; one task per checkout (`conflict {reason: already_tracked}`). `task.recreate {task}` re-adds a `missing` task's worktree at its recorded path from its branch (`git worktree prune` of stale metadata first; refused if the path exists or the branch is gone), status back to `active` (event `task.recreated`). `task.forget {task, force?}` closes a missing or finished task's record (others need `force`), releases its ports, retires previews and never touches files (event `task.forgotten`). `task.ports {task}` → `{lease, env}` (`VIBEKE_PORT_BASE|END`, `VIBEKE_TASK`, `VIBEKE_TASK_SLUG`, `VIBEKE_WORKTREE`, mapped port names); `task.ports.re_lease {task}` moves the task to another block of the same size, never the old one (event `task.ports_changed`; running panes keep the old env). A task's cached PR first seen `MERGED` (by `task.pr`, and the 60 s reconcile loop reading the cache) emits `task.cleanup_suggested {reason: pr_merged, pr, url, hint}` once per PR plus a low notification. Archive, adopt, recreate, forget and re-lease are full scope only. CLI `vibeke task setup-log|archive|adopt|recreate|forget|ports [--re-lease]`.

*As built (2026-10-06, M5 slice 2):* `worktree.create {repo|cwd, branch, base?, open?: false, focus?: false, name?}` creates the worktree under `[tasks] root` (branch template and fetch settings from `[tasks]`, overridable per call) and returns `{worktree: {path, branch, base_ref, repo_root, created_branch}, workspace?, tab?, root_pane?}`; `path` is not supported yet. `worktree.open {path, focus?, name?}` reuses the workspace rooted at the worktree or creates one: `{worktree, workspace, created}`. They emit `worktree.created` / `worktree.opened`; `task.create` emits `worktree.created` for worktree checkouts too. Pane-scoped callers cannot pass `focus: true`.

### 2.11 `preview.*`, `browser.*` [M3]

Semantics in [06](06-remote-and-preview.md) Part B.

| Method | Params → Result |
|---|---|
| `preview.list` | `{machine?, task?, pane?, status?: suggested|up|down|all}` → `{previews}` — suggestions only with `status: suggested|all` |
| `preview.declare` | `{port, host?: 127.0.0.1, scheme?: http, path?, label?, pane?, task?}` → `{preview}` |
| `preview.promote` / `preview.dismiss` | `{preview}` → `{preview}` — accept or hide a discovered suggestion |
| `preview.open` | `{preview, mode?: profile|proxy, client?: client_id}` → `{opened_in: profile|proxy|default_browser, url}` — profile mode (default) launches/reuses the Vibeke browser profile routed over SOCKS5 to the preview's machine and opens `http://localhost:<port>/<path>`. Proxy mode returns a tokenized `*.vibeke.localhost` URL |
| `preview.url` | `{preview, mode?}` → `{remote_url, profile_url, proxy_url?}` — no side effects (for agents: what URL to tell the human) |
| `preview.mirror` / `preview.unmirror` | `{preview}` → `{local_port}` — explicit, unauthenticated raw local port (06 B4). Full scope only |
| `preview.forget` | `{preview}` → `{}` |
| `preview.profile_list` / `preview.profile_reset` | `{machine?|task?}` → `{profiles}` / `{}` |

**As built (Goal 03 Stage 1; `vk-server::preview`).** `Preview` = `{id, handle: "v<N>", machine, pane?: pane ULID, task?, port, path, label?, url, scheme, status: suggested|declared|up|down|gone, source: declared|listener|output_url|banner, pid?, first_seen_ms, last_seen_ms}`; JSON results add `pane_handle` and `task_handle`. Previews are entity documents (`kind = "preview"`, gone ones closed) and `SessionModel.previews` (render stream; last field). A target is `v4`, a ULID, or `devbox/v4`; `machine` in params does the same. Machine-qualified calls are executed by the local server on the remote server over its own bridge link.

| Method | Params → Result (as built) |
|---|---|
| `preview.declare` | `{port, path?: "/", label?, scheme?: http|https, pane?: target (default: the calling pane), task?}` → `{preview, cursor}` — a live preview on that port (any source) is promoted and becomes `source: declared`. Otherwise a new `declared` record, probed at once (→ `up`/`down`). Event `preview.declared`. |
| `preview.list` | `{machine?, task?, pane?, all?: bool, status?: suggested|declared|up|down|all}` → `{previews, machine}` — suggestions only with `all` or `status`. |
| `preview.get` | `{preview}` → `{preview}` |
| `preview.promote` | `{preview}` → `{preview}` — `suggested → up` (event `preview.up`). |
| `preview.forget` (alias `preview.dismiss`) | `{preview}` → `{}` — closes the record (event `preview.gone`) and suppresses re-suggestion of the same `(port, pid)`. |
| `preview.url` | `{preview}` → `{remote_url, profile_url}` |
| `preview.open` | `{preview | url, window?: bool, split?, machine?}` → `{opened_in: "window"|"default_browser", url, machine, profile, profile_dir, browser, browser_kind, pid, reused, socks_port, route: none|loopback|remote}` — Stage 1 opens windows only: without `window: true` (or `preview.mode = "window"`) → `unsupported {fallback: "window"}`. Opening promotes a suggestion. URLs must be `http(s)`. From a pane only loopback URLs and only this machine's previews (`permission_denied`). A profile already open for another machine/route → `conflict`. No browser found → `unsupported`. Event `preview.opened`. |
| `preview.profile.list` (aliases `preview.profile_list`, `preview.profile {action: "list"}`) | `{}` → `{profiles: [{name, path, running, pid?, machine?, route?, bytes}], root}` |
| `preview.profile.reset` (aliases `preview.profile_reset`, `preview.profile {action: "reset", profile}`) | `{profile}` → `{profile, removed}` — `conflict` while its browser runs. Not allowed from a pane. |
| `preview.status` | `{}` → `{socks_port?, browsers: [{profile, machine, route, pid, running}], links: [{machine, connected, bytes_in, bytes_out, rtt_ms}], accepted, rejected}`. `server.status` also carries `preview: {socks_port, browsers, previews}`. |

Events: `preview.discovered`, `preview.declared`, `preview.up`, `preview.down`, `preview.gone` (subject `{preview, preview_handle, pane, task, machine}`, data `{preview}`) and `preview.opened` (data `{url, opened_in, profile, browser}`). `preview.test.register_browser {pid, profile, machine, route?}` exists only when the server runs with `VIBEKE_TEST_HOOKS=1`. Not built yet: `client` routing of `open` (proxy mode and mirrors: see "As built (preview fabric completion)" below).

CLI: `vibeke preview declare <port> [--path p] [--label l] [--pane p] [--task k]`, `list [--all] [--task k] [--pane p]`, `get|url|promote|forget <v>`, `open <v|machine/v|url> --window`, `profile list|reset <name>`, `status`. With `--machine m`, `open`, `url`, `profile` and `status` run on the local server with `machine: m` (the browser runs on the viewing machine); the others are forwarded to `m` as usual.

**As built (preview fabric completion; `vk-server::preview_fabric`, `vk_preview::{proxy, tls}`): proxy mode, mirrors, task previews, TLS probe, Firefox (06 B2, B3.4, B4).**

| Method | Shape |
|---|---|
| `preview.open` (proxy mode) | `{preview, mode: "proxy" \| proxy: true, no_open? \| open: false, machine?}` → `{opened_in: "proxy", url, proxy_url, host, proxy_port, machine, preview, opened, token_ttl_s: 60, remote_url, caveats, open_url?}` — also chosen by `preview.mode = "proxy"` when neither `window` nor `split` is given. `mode: "window"` is accepted as an alias of `window: true`. Registers a **fresh, unguessable** `*.vibeke.localhost` origin on the viewing machine's proxy for this open (128 random bits; any previous origin of the preview and its sessions are revoked), mints a one-time 60 s token and opens `open_url` in the default browser (`opened: false` with `no_open`/`--no-open` or `VIBEKE_NO_OPEN=1`). `open_url` (with `vk_token`) is returned to **full-scope callers only**; pane-scoped callers get the plain `url` of the origin they opened. Also `session_ttl_s` (28800), `tls_origin` and, for TLS, `ca {path, sha256, spki_sha256, trust}`. A URL instead of a preview → `invalid_params`. Event `preview.opened {url: the preview's own URL, opened_in: "proxy", tls_origin}` — never the proxy hostname or URL. |
| `preview.url` | adds `proxy_url` (no credential) when the preview already has a proxy origin **that the caller may see** (one it opened itself, or any for a full-scope client), else `null` — still no side effects. `preview.status.proxy.routes` is filtered the same way. |
| `preview.mirror` | `{preview: "devbox/v4"}` → `{local_port, machine, preview, preview_handle, url, addrs, since_ms, accepted, rejected, authenticated: false, peer_check: "same_user", warning, already?}` — full scope only (`permission_denied` from a pane). Remote previews only (`invalid_params` for a local one). `conflict` when the port is busy on this machine or mirrored for another preview. Event `preview.mirrored {local_port, url, warning}`. |
| `preview.unmirror` | `{preview: "devbox/v4" \| port}` (or `{port}`) → `{local_port, machine, preview}`. `not_found` if no such mirror. Event `preview.unmirrored`. |
| `preview.status` | adds `proxy: null \| {port, routes: [{host, machine, preview, handle, port, scheme}], stats: {requests, denied, websockets}}` and `mirrors: [mirror…]`. `server.status.preview` adds `proxy_port` and `mirrors` (count). |
| `task.create` | result adds `previews: [preview + {name, from}]` (declared from the task's `[previews]` with leased ports) and `preview_warnings: [string]`. Accepts `previews: {name: {port_env\|offset\|port, path?, label?, scheme?}}` (absolute `port` allowed here, not from repo config). `task.finish` retires the task's previews (`preview.gone`). |

Preview discovery classifies TLS listeners (`scheme: "https"`, `url: https://localhost:<port>/`); `https` output URLs need a TLS page probe. `[preview]` keys added: `proxy_port` (47800; `0` = ephemeral), `profile_browser = "firefox"` (Firefox window on `<profile>-firefox`; `browser_kind: "firefox"` in the `preview.open` window result).

CLI: `vibeke preview open <v|machine/v> --proxy [--no-open]` (prints the origin, and the one-time link when it did not open a browser), `vibeke preview mirror <machine/v>`, `vibeke preview unmirror <machine/v|port>`; with `--machine m` these run on the local server (viewing machine) like `open`.

**As built (Goal 03 Stage 2; `vk-server::browser_pane`): browser panes (06 B3.2).** `preview.open` without `window` now opens a **browser pane**: `{preview | url, split?: right|down|left|up|tab|float, pane?, focus?}` → `{opened_in: "pane", pane, pane_handle, tab, url, machine, source_pane}` (the pane goes into the layout of the preview's machine; `float` = right split for now). `preview.open` also accepts `profile` (non-pane callers; used by the window handover).

| Method | Params → Result (as built) |
|---|---|
| `browser.pane.create` | `{pane?, preview? | url?, split?, machine?, focus?: true, focus_client?}` → `{opened_in: "pane", pane, pane_handle, tab, url, machine, source_pane}` — on the owning server. `pane` defaults to the preview's pane, then the caller's pane / focused pane. URLs: `http(s)` (`localhost:5173/x` gets `http://`). From a pane only loopback, only next to the caller's own panes, never moving focus. Events `pane.created {kind: "browser"}`, `tab.layout_changed`. |
| `browser.pane.update` | `{pane, url?, title?, history?, history_index?, device?, viewport?: "WxH" \| "fit", fit?}` → `{pane}` — persists navigation reported by the viewing client (remote owners). Event `browser.navigated`. `device` (a `vk_browser::devices` preset: `iphone-15`, `pixel-8`, `ipad`, `desktop-1280/1440/1920`) or `viewport` (64..=8192 CSS px per edge) pins the page size (06 B3.2, letterboxed). `viewport: "fit"` / `fit: true` unpins. Both at once or an unknown name → `invalid_params`. Event `browser.viewport_changed {device, viewport}`. Not from a pane. |
| `browser.pane.console` | `{pane, toggle?: true, focus?: false}` → `{pane, pane_handle, browser_pane}` (opened) \| `{closed, pane_handle, browser_pane}` (toggled off) \| `{…, existing: true}` (`toggle: false`) — the console/network split: a pane under the browser pane (30 %) running `vibeke browser console --pane <id> --follow`, marked `created_by = "browser-console:<browser pane id>"`. Closed by the server when its browser pane goes. Not from a pane. |
| `browser.pane.console_push` | `{pane, entries: [console/network entry]}` → `{pane, stored}` — The media host sends captured entries to the browser pane owner. The page must use a loopback URL. Limits: one batch per second and 200 entries per batch. The owner keeps only known fields in its ring buffer. It removes sensitive data again and limits text to 8 KiB. Pane tokens cannot call this method. |
| `browser.pane.list` | `{}` → `{panes: [{pane, handle, tab, browser}]}` |
| `browser.status` | `{}` → `{browsers: [{profile, dpr, pid, targets, up_ms}], targets: [{pane, owner, url, title, profile, machine, route, running, screencast, viewers, frames, fps, decode_ms, css, frame, history, error}], windowed, tiles_sent, media_bytes}` — the media host's view. |
| `browser.command` | `{pane, cmd: back|forward|reload|stop|navigate|screenshot|text|window|pane, url?, hard?, text?}` → `{pane}` (`window` → the `preview.open` window result. `pane` → `{pane, profile}`) — for panes rendered by this server. Not from a pane. |

`Pane` gained `browser: BrowserPane? {url, machine ("" = owner), task?, preview?, source_pane?, history, history_index, title, watch?, device?, viewport?}` (fields appended). `browser.pane.create` and `preview.open` (pane) also take `device` / `viewport` (validated as in `browser.pane.update`). **`browser.console` / `browser.network` with `pane`** (instead of `session`): `{pane, kind?: all|console|network, level?: error, errors?, failed?, after?: seq, since?, limit?: 200}` → `{pane, entries: [{seq, kind, ts, …}], last_seq, source: local|relayed|none, url}` — a browser pane's capture (06 B3.2) in the agent browser's entry shapes (`console {level, text, source, url, line}`, `network {method, url, type, status, mime, error, blocked_reason, duration_ms}`) plus `origin` and `document` (the frame the entry came from), redacted with control characters escaped (the response's `url` too), one `seq` across both rings (500 entries each); from the page rendered here, else the copy relayed by its media host; from a pane only for that browser pane's console split or the agent that created the browser pane. `blob.commit {upload_id, stage?: "browser"}`: `browser` puts the upload in the server's private drop directory, the only place `BrowserCmd::DropFiles` paths may come from. CLI (06 B3.2 page I/O): `vibeke browser console --pane <p> [--follow] [--console|--network] [--errors] [--since 5m]` (follow keys `c`/`n`/`e`/`a`/`q`), `vibeke browser console-split <p>`, `vibeke browser viewport <p> <WxH|fit> [--device <preset>]`, `vibeke preview open … --device iphone-15 | --viewport 390x844`. Config: `[preview] pane_browser` (headless Chromium for panes; default Playwright's headless shell or `$VIBEKE_CHROMIUM`). CLI: `vibeke preview open <v|machine/v|url> [--split right|down|tab|float | --window] [--pane p]`, `vibeke browser status|list|open <url>|command <pane> <cmd> [--url u]`, `vibeke debug fake-chromium` (tests). The agent-facing `browser.session_*` family below is Stage 3.
| `browser.session_open` | `{preview?|url?, viewport?: {w, h, dpr}, device?, color_scheme?: light|dark}` → `{browser_session}` — headless context **on the machine where the dev server runs**, owned by the caller's pane/run |
| `browser.navigate` | `{browser_session, url|path, wait?}` → `{status, final_url}` — destination rules apply to redirects too (06 B5) |
| `browser.click` / `browser.type` / `browser.press` | `{browser_session, selector|text|role, text?, key?, submit?}` → `{}` |
| `browser.wait` | `{browser_session, for: load|networkidle|{selector}|{ms}}` → `{}` |
| `browser.eval` | `{browser_session, expression}` → `{value}` — gated by capability `browser.script` |
| `browser.screenshot` | `{browser_session?|preview?|url?, full_page?, selector?, viewport?, device?}` → `{blob, path_on_machine, width, height, meta: ScreenshotMeta}` — `meta.environment` and `meta.code {task, head_sha, dirty_digest}` per 06 B6. File also written to `$TMPDIR/vibeke-shots/<hash>.png` |
| `browser.console` | `{browser_session|preview, since_ms?, level?: error|warn|all}` → `{entries: [{ts, level, text, source}]}` |
| `browser.network` | `{browser_session|preview, failed_only?, since_ms?}` → `{entries: [{ts, method, url, status?, error?, blocked_by_policy?}]}` |
| `browser.dom` | `{browser_session|preview, selector?, format?: text|html|a11y}` → `{content}` |
| `browser.diff` | `{a: blob, b: blob, threshold?}` → `{blob, changed_ratio, regions}` — refuses different `environment.kind` unless `force` |
| `browser.session_close` | `{browser_session}` → `{}` |

**As built (Goal 03 Stage 3; `vk-server::agent_browser`).** Sessions run on the server that serves the call (use `--machine m` for another machine's previews; a machine-qualified preview is refused with that hint). `session` accepts the handle (`b3`) or the ULID (`session_id`); `browser_session` and `target` are accepted as aliases. Pane-scoped callers see and drive only sessions owned by their pane or panes it created (others are `not_found`); while a human holds a session every pane-scoped call on it fails with `human_control` (code -32004, retryable). New error kinds: `destination_denied` (-32003, `details {url, reason}`), `human_control` (-32004, `details {session}`), `navigation_failed` (-32004, `details {url, error}`).

| Method | Params → Result (as built) |
|---|---|
| `browser.open` (alias `browser.session_open`) | `{preview? \| url?, viewport?: "WxH" \| {width, height}, dpr?, color_scheme?: light\|dark, dark?, wait?, timeout_ms?}` → `{session, session_id, owner: {pane, pane_handle, run} \| {user}, preview, url, created_ms, viewport, human_control, screencast, proxy_port, machine, environment: {kind: "remote_headless", machine, runner: "host", browser, fresh_context: true}, status?, final_url?, title?}` — no target opens `about:blank`. The destination is checked before anything starts. A failed first navigation closes the session. Opening a suggested preview promotes it. Event `browser.session_opened`. |
| `browser.navigate` | `{session, url \| path, wait?: load\|domcontentloaded\|none, timeout_ms?: 15000}` → `{session, status, final_url, title}` — `path` (`/x`) is relative to the current origin. `destination_denied` for refused targets and denied redirect hops. `navigation_failed` for other network errors. `timeout`. |
| `browser.click` | `{session, selector?: css \| "text=Label", text?, x?, y?, click_count?, timeout_ms?: 5000}` → `{session, x, y, element: {tag, text, width, height} \| null}` — waits for a visible element, scrolls it into view, clicks its center (`not_found` with `details.selector`). |
| `browser.type` | `{session, selector?, text, submit?, clear?, timeout_ms?}` → `{session, typed}` — focuses `selector` (else the focused element), `Input.insertText`. `submit` presses Enter. |
| `browser.press` | `{session, key}` → `{session, key}` — Vibeke key grammar (`enter`, `ctrl+a`, `shift+tab`) and Playwright names (`ArrowDown`, `Control+A`). `invalid_key`. |
| `browser.wait` | `{session, for: load\|networkidle\|"selector:<css>"\|"ms:<n>", timeout_ms?}` → `{session, waited}` |
| `browser.eval` | `{session, expression}` → `{session, value}` — `returnByValue`, promises awaited. Script errors → `invalid_params {exception}`. Results over 256 KiB refused. From a pane only with `preview.browser_script = true` (else `permission_denied {capability: "browser.script"}`). |
| `browser.screenshot` | `{session, full_page?, selector?, inline?}` → `{session, id, handle, blob, path_on_machine, width, height, bytes, binding, label, meta: ScreenshotMeta, data_b64?, mime?}` — PNG stored content-addressed at `<state>/blobs/<h2>/<blake3>.png` (0600) with a `<blake3>.json` sidecar, plus a `screenshot` record (`screenshot.*` below, Stage 4) carrying environment, `code` (CodeState) and running-build identity/binding. `inline` adds base64 up to 8 MiB. Full page is clipped at 16 384 CSS px. Events `browser.screenshot {blob, url, width, height, screenshot}` and `screenshot.captured`. The CLI's `--out f.png` fetches inline and writes locally (the server never writes to caller-chosen paths). |
| `browser.diff` (Stage 4) | `{a, b, threshold?: 0.1, force?, inline?}` → `{a: {id, handle, blob, environment, label, binding, head_sha, width, height}, b: …, threshold, channel_threshold, width, height, size_mismatch, a_size, b_size, changed_pixels, total_pixels, changed_ratio, regions: [{x, y, width, height, pixels}] (largest first, ≤ 50), regions_total, forced, blob, path_on_machine, bytes, data_b64?}` — `a`/`b` are screenshot ids or handles (`s3`), or raw blob hashes (full scope only). A pixel changed when any RGBA channel differs by more than `threshold × 255`. Different sizes compare on the union canvas (extra area counts as changed). Regions are connected 16 px cells of changes. The diff image (after-image faded gray, changes red) is a blob with a `screenshot_diff` sidecar, expired with `screenshots.keep_days`. Different `environment.kind` → `invalid_params {reason: "environment_mismatch"}` unless `force`. Pane scope: both screenshots must be visible to the pane. |
| `browser.snapshot` (alias `browser.dom`) | `{session, format?: a11y\|text\|html, selector?, max_bytes?: 102400}` → `{session, url, format, content, truncated}` — `a11y` is the accessibility tree as an indented outline (`role "name" [value] {states}`). |
| `browser.console` | `{session, level?: error\|warn\|all, since_ms? \| since?: "5m", limit?: 200}` → `{session, entries: [{ts, level, text, source: console\|exception\|network\|dialog\|…, url?, line?}]}` |
| `browser.network` | `{session, failed_only? (CLI --failed), since_ms? \| since?, limit?}` → `{session, entries: [{ts, method, url, type, status, error, mime?, duration_ms?, blocked_by_policy?, layer?: proxy\|fetch\|api}]}` — `failed_only`: errors, policy blocks and status ≥ 400. |
| `browser.close` (alias `browser.session_close`) | `{session}` → `{session, closed}` — disposes the context, stops its proxy. Event `browser.session_closed {reason: closed\|idle\|owner_pane_closed\|owner_run_ended\|browser_exited\|open_failed}`. |
| `browser.list` | `{}` → `{sessions: [...], browser: <status>, machine}` — only sessions the caller may see. |
| `browser.status` | `{}` → `{running, pid?, product?, binary, kind, sessions, denied, profile_dir, idle_timeout_ms, uptime_ms?}` |
| `browser.install` | `{confirm?, version?, url?, sha256?}` → without `confirm`: `{plan: {version, platform, url, sha256, checksum_known, dir, binary, installed}, confirm_required: true}`. With `confirm`: downloads, verifies, installs → `{installed, binary, plan}`. Full scope only. |
| `browser.take_over` / `browser.release` | `{session}` → `{session, human_control}` — full scope only. Events `browser.taken_over` / `browser.released {by}`. |
| `browser.watch` | `{session \| agent_pane, pane?, split?: right\|down\|left\|up\|tab, focus?: true, focus_client?}` → `{opened_in: "watch", session, read_only: true, pane, pane_handle, tab, url, machine, source_pane}` — full scope only (`permission_denied` from a pane). `agent_pane` picks that pane's newest open session (`not_found` "… has no open browser session"). Creates a browser pane with `watch = <session>` in this server's layout next to `pane` (default: the session's owner pane). The pane is read-only until `BrowserCmd::TakeOver(true)` (render stream), which sets human control held by `pane:<id>` (events `browser.taken_over {by, via: "watch_pane"}`). `TakeOver(false)` or closing the pane releases it (`browser.released`). CLI: `vibeke browser watch <session> [--pane p] [--split …] [--machine m]`. |
| `browser.attach_screencast` / `browser.detach_screencast` / `browser.screencast_frame` | `{session}` → `{session, delivery: "internal+poll", width, height}` / `{session, detached}` / `{session, after_seq?}` → `{session, seq, mime: "image/jpeg", width, height, received_ms, data_b64 \| null}` — full scope only. In-process browser panes use `AgentBrowsers::attach_screencast` (latest-wins `watch` channel) and `AgentBrowsers::human_input`. |

Events: `browser.session_opened`, `browser.session_closed`, `browser.request_denied {session, url, reason, layer, resource_type}` (≤ 20 per session per 10 s), `browser.screenshot`, `browser.taken_over`, `browser.released`; subject `{browser_session, session_id, pane, machine}`. `preview.console_error {count, source, text, url, line, session}` (subject `{preview, preview_id, pane, task, machine}`; at most 5 per preview per 10 s, redacted, 06 B5). `browser.open` takes `device` (preset name, 06 B5); `browser.screenshot` without `session` but with `url`/`preview` is a one-shot capture in a throwaway context (`one_shot: true`); sessions opened from a pane reach only their own previews unless `[browser] session_previews = "machine"` (denied with `foreign_preview`). Not built: `browser.diff --baseline task-start`.

CLI: `vibeke browser open <preview|url> [--viewport WxH] [--dark]`, `navigate <s> <url|/path>`, `click <s> <css|text=…>` (or `--x --y`), `type <s> [selector] <text> [--submit] [--clear]`, `press <s> <key>`, `wait <s> <for>`, `eval <s> <js>`, `screenshot <s> [--full-page] [--selector css] [--out f.png]`, `snapshot|dom <s> [--format a11y|text|html]`, `console <s> [--level error] [--since 5m]`, `network <s> [--failed]`, `close <s>`, `list`, `status`, `install [--yes] [--sha256 hex] [--url u]` (shows the plan and asks; without a terminal needs `--yes`), `take-over <s>`, `release <s>`, `diff <a> <b> [--threshold 0.1] [--force] [--out diff.png]`.

**`screenshot.*` (as built, Goal 03 Stage 4; 06 B6).** Records of entity kind `screenshot` (`ScreenshotMeta`):

```
{id, handle: "s<N>", kind: "screenshot", blob, mime, width, height, bytes, created_at_ms, taken_at,
 environment: {kind: remote_headless|local_pane|window|local_proxy, machine, runner, browser, browser_version,
               viewport: {width, height}, dpr, color_scheme, device, fresh_context, profile},
 label,                                   // "devbox · headless · fresh context" | "your browser pane · profile devbox"
 url, final_url, title, preview, preview_id, session, full_page, selector,
 taken_by: {kind: agent|user|plugin, pane, run, client}, pane, run, task, workspace,
 code: {repo, origin_url, head_sha, dirty_digest, dirty_state: clean|dirty|unknown, captured_at_ms, warnings} | null,
 code_note,                               // why `code` is null
 runtime: {status: known|unknown, source: probe|header|caller|none, build_id, head_sha, dirty_digest,
           dirty_state, started_at_ms, fixture, observed_at_ms, detail},
 binding: bound|illustrative, binding_reason}
```

| Method | Params → Result |
|---|---|
| `screenshot.list` | `{task?, preview?, run?, since?: ms \| "1h", since_ms?, limit?: 50}` → `{screenshots: [ScreenshotMeta + {path_on_machine, exists}], count, total}` — newest first. Pane scope: only screenshots whose `workspace` is the pane's workspace. |
| `screenshot.get` | `{id}` (id or `sN`) → `ScreenshotMeta + {path_on_machine, exists, data_b64? (inline)}` — other workspaces' screenshots are `not_found` for panes. |
| `screenshot.open` | `{id}` → as `get` with the image inline (the CLI writes it to `$TMPDIR/vibeke-screenshots/<id>.png` and opens it with `open`/`xdg-open`. `VIBEKE_NO_OPEN=1` only reports the path). |
| `screenshot.delete` | `{id, force?}` → `{id, handle, deleted, blob_removed}` — human only (`permission_denied` for panes). A screenshot referenced by a review acceptance needs `force` (`conflict {reason: "referenced_by_acceptance"}`). The blob goes when no other record references it. Event `screenshot.deleted {ids, reason: deleted\|retention}`. |

Events: `screenshot.captured {id, handle, task, binding, environment, label, blob, preview, head_sha}` (metadata only; subject `{screenshot, handle, pane, task, workspace, machine}`), `screenshot.deleted`. In-process: `vk_server::screenshots::record_screenshot(server, png, ShotInputs) -> ScreenshotMeta` (the browser pane passes `environment.kind = local_pane`, optionally `runtime`/`checkout`).

CLI: `vibeke screenshot list [--task t] [--preview v4] [--run r] [--since 1h]`, `get <sN> [--out f.png]`, `open <sN>`, `diff <a> <b>` (= `browser diff`), `delete <sN> [--force]`, `code-state [dir]` (local, no server: the checkout's `CodeState` JSON — a dev script serves it at `/__vibeke_build` so its screenshots can be `bound`).

#### 2.11.1 `vibeke mcp` (as built, Goal 03 Stage 3)

A stdio MCP server for agent harnesses (06 B7): JSON-RPC 2.0, one message per line; protocol versions `2025-11-25` (preferred), `2025-06-18`, `2025-03-26`, `2024-11-05`; methods `initialize` (→ `{protocolVersion, capabilities: {tools: {listChanged: false}}, serverInfo: {name: "vibeke", title, version}, instructions}`), `ping`, `tools/list`, `tools/call`, empty `resources/list`/`prompts/list`; notifications are accepted silently; batches answered as arrays; unknown method -32601, unknown tool -32602, bad JSON -32700.

| Tool | API method | Arguments |
|---|---|---|
| `preview_declare` | `preview.declare` | `{port, path?, label?}` |
| `preview_list` | `preview.list` | `{all?}` |
| `browser_open` | `browser.open` | `{preview?, url?, viewport?, color_scheme?}` |
| `browser_navigate` | `browser.navigate` | `{session, url}` |
| `browser_click` | `browser.click` | `{session, selector?, x?, y?}` |
| `browser_type` | `browser.type` | `{session, selector?, text, submit?, clear?}` |
| `browser_press` | `browser.press` | `{session, key}` |
| `browser_wait` | `browser.wait` | `{session, for, timeout_ms?}` |
| `browser_eval` | `browser.eval` | `{session, expression}` |
| `browser_screenshot` | `browser.screenshot` (+`inline`) | `{session, full_page?, selector?}` → `[{type: image, data, mimeType: "image/png"}, {type: text, <metadata JSON>}]` |
| `browser_snapshot` | `browser.snapshot` | `{session, format?, selector?}` → text |
| `browser_console` | `browser.console` | `{session, level?, since_ms?}` |
| `browser_network` | `browser.network` | `{session, failed_only?, since_ms?}` |
| `browser_close` | `browser.close` | `{session}` |
| `browser_diff` (Stage 4) | `browser.diff` (+`inline`) | `{a, b, threshold?, force?}` → `[{type: image, <diff PNG>}, {type: text, <stats JSON>}]` |

Results are text content with the API result as JSON; API errors are `{isError: true}` results whose text is `<kind>: <message>` plus details. The process connects to the pane's server like the CLI (`VIBEKE_SOCKET`/`VIBEKE_SESSION`, token from `VIBEKE_PANE_TOKEN`; process ancestry also yields pane scope) and reconnects once after a server restart. Installed with `vibeke integration install <claude|codex> --mcp` (06 B7).
| `image.show` | `{blob|path, pane?: @current, max_cols?, max_rows?}` → `{}` — inline display in the TUI via kitty graphics/iTerm2/sixel passthrough. Text fallback shows dimensions + `vibeke open` hint |
| `image.upload` | `{pane, mime, data_b64 | path_on_client}` → `{path_on_machine, blob}` — client→remote image transfer (paste/drag) |

### 2.12 `notification.*` [M1]

| Method | Params → Result |
|---|---|
| `notification.list` | `{unread_only?: true, limit?}` → `{notifications}` |
| `notification.send` | `{title, body?, urgency?: normal, subject?: pane|run|task, sound?: bool}` → `{notification}` — for scripts/plugins |
| `notification.read` | `{notification|all: true}` → `{}` |
| `notification.config` | `{}` → `{channels: [os, terminal_bell, osc9, sound, plugin:<id>], rules}` — *M4:* `{channels: [toast, native, osc, …], native: {backend, unavailable_reason}, rules: {on, suppress_when_focused, coalesce_ms, quiet_hours}, hosts}` |

*M4:* every `Notification` carries `channels`: where the pipeline delivered it, or why it skipped a channel (`native`, `native:headless`, `native:unavailable`, `coalesced`, `suppressed:focused`, `quiet_hours`, `filtered:<kind>`). The render-stream `Notify` frame carries `delivered: [native]`, so clients skip their own OSC forward.

### 2.13 `events.*` [M1]

| Method | Params → Result |
|---|---|
| `events.subscribe` | `{after?: Cursor, types?: [glob], subjects?: {workspace?, tab?, pane?, run?, task?}, include_snapshot?: false, machine?: label|"*"}` → `{subscription_id, at: Cursor}` then notifications |
| `events.unsubscribe` | `{subscription_id}` → `{unsubscribed: bool}` — *as built:* ends a subscription of the same connection (its server task is aborted; pushes already queued may still arrive and clients drop them); idempotent, `false` for an unknown or already finished id; pane scope `open`. Only on a control connection (like `events.subscribe`); a connection's subscriptions end when it closes. |
| `events.read` | `{after?: Cursor, before?: Cursor, types?, subjects?, limit?: 500}` → `{events, next: Cursor}` — paginated history (within retention) |
| `events.wait` | `{types, subjects?, after?: Cursor, timeout_ms?}` → `{event}` — one-shot wait (CLI-friendly) |

Server push (JSON-RPC notification):
```json
{"jsonrpc":"2.0","method":"events.event","params":{"subscription_id":"s1","event":{"seq":18342,"ts":1791232838418,"v":1,"type":"agent.state_changed","subject":{…},"actor":{…},"data":{…}}}}
```
- `include_snapshot: true` → first push is `events.snapshot {subscription_id, at_seq, projections}` (same shape as `session.snapshot`), then events with `seq > at_seq`.
- **Cursor identity**: `Cursor = {machine_uuid, session_uuid, log_epoch, seq}`. `machine_uuid` is generated once per machine (labels can be renamed), `session_uuid` once per session (named sessions share a machine), and `log_epoch` changes whenever the log's sequence could rewind (restore from backup, DB recreated). A cursor whose `(machine_uuid, session_uuid, log_epoch)` doesn't match the current log is rejected with `cursor_epoch_mismatch {current: Cursor}` and the client must resync from a snapshot. `seq` alone is accepted as shorthand for local single-session scripts.
- Ordered, at-least-once per subscription; clients dedupe by full cursor (for `machine:"*"` each event carries its machine's cursor).
- Back-pressure: each subscription has a 10,000-event queue; overflow → `events.overflow {subscription_id, resume_from: Cursor}` and the subscription is closed; the client resubscribes with `after` (cheap while within retention). Never silent loss.
- Cursor too old → error `truncated {earliest_seq}`.
- Glob types: `agent.*`, `interaction.opened`, `pane.{created,closed}`.
- *As built (batch 2 review, 2026-10-06; `vk-server/src/run.rs`).* `events.subscribe` runs the same per-call checks as every dispatched method before it subscribes (pane scope, `token_revoked`, `elevation_expired`, 09 §3.2), including for history (`after`). The subscription re-checks its caller before every delivery, whenever a token or elevation is revoked (a revocation wakes every subscription) and when an elevated caller's grant runs out; a caller that lost access gets `events.closed {subscription_id, reason}` (in the schema registry's notifications) and nothing more. Test: `run::auth_tests::revocation_closes_subscriptions_and_refuses_new_ones`.

### 2.14 `layout.*`, `config.*`, `search.*` [M1]

| Method | Params → Result |
|---|---|
| `layout.export` | `{tab|workspace}` → `{layout: LayoutSpec}` (TOML/JSON-serializable, includes commands and cwds) |
| `layout.apply` | `{layout, workspace?, new_workspace?: {cwd, name}}` → `{workspace, tabs, panes}` |
| `config.get` | `{key?}` → `{value, source: default|user|repo|cli}` |
| `config.set` | `{key, value, persist?: false}` → `{}` — runtime override |
| `config.validate` | `{path?}` → `{errors: [{line, col, message}]}` |
| `search.query` | `{q, scope?: {workspace?, pane?, run?}, sources?: [scrollback, transcript, events], limit?: 50, regex?: false}` → `{hits: [{pane, run?, source, line, text, ts, context}]}` — FTS5 over archive + transcripts |
| `scrollback.forget` | `{pane}\|{workspace}\|{before}\|{all: true}` (exactly one), `dry_run?: false`, `plan?` → `{scope, pane_ids, plan, dry_run, panes, segments_deleted, bytes_deleted, fts_rows_deleted, archive_panes_dropped}` — deletes archived scrollback for the scope: zstd segments, `scrollback_fts` rows and `archive_panes` metadata. `scope` is canonical (pane/workspace id, absolute `before` ms), `pane_ids` the resolved panes (null for `before`/`all`), `plan` a digest of both. If the scope no longer matches `plan`, the server returns `conflict`. The CLI sends the dry run's `scope` + `plan` to confirm the deletion. This prevents a focus change or relative cutoff from changing the deletion target. Idempotent. `before` is segment-granular. Does not delete events, blobs, the desk index, drafts or notes (02, 09 §9.3). Event `scrollback.forgotten {scope} {panes, segments, bytes, fts_rows, panes_dropped}` (no text). Full scope only. CLI `vibeke forget` asks first unless `--yes`. |
| `layout.list` / `layout.get` | `{}` → `{layouts: [{name, description, cwd, tabs, panes, valid}]}`. `{name}` → `{layout}` — named layouts from `[layouts.<name>]` [M4] |

*As built (Batch 2A, 2026-10-06, `crates/vk-server/src/config_api.rs`, `crates/vk-config/src/edit.rs`).* Sources are, lowest first, `default < user < repo < runtime < cli` (v1 remainder, `vk_config::layers`): `repo` is a trusted repository's `.vibeke/config.toml` (`[tasks]`/`[preview]` keys over the user's; `[[policy.rule]]` deny/ask only, appended), layered in when `config.get` gets `repo`, `cwd` or `pane` (the CLI sends its current directory); `cli` is `--config-override key=value` (repeatable, value parsed as TOML else a string) and `VIBEKE_CONFIG_OVERRIDE="k=v;k=v"`, validated before the command runs and exported so a server it starts inherits them. The result adds `cli_overrides`, `layers: [{source, path?, trusted?, applied?, keys?}]` and `repo {root, file, trusted, applied}`. `config.get {key?}` → `{key?, value, source, path, overrides, errors}`: the effective value (dotted key, `not_found {object: config_key}` for an unknown one) or the whole config. `config.set {key, value, persist?: false}` → `{key, value, persisted, path, changed}` validates the resulting config first (type errors and unknown keys are `invalid_params`, nothing applied); without `persist` it is a runtime override for this server process (the process-wide overlay `Config::load` applies to `config.toml`), with `persist` it edits `config.toml` with `toml_edit` (comments, ordering and the edited line's trailing comment kept, missing tables created) and writes it atomically (temp file, fsync, rename; the file mode is kept, new files 0600), clearing that key's override. `value: null` removes the key (back to its default). `config.validate {path?}` → `{path, valid, errors, warnings}` parses a file without applying it. `config.reload` (= `server.reload_config`) re-reads the file. The server runs the config watcher (`config.watch = true`, 250 ms debounce): a valid change is applied and emits `session.config_reloaded {changed_keys, new_panes_only, source: watch|api|set|set_persist}`; a file that fails to parse or validate emits `session.config_rejected {errors}` and the applied config stays. Server modules that read config on demand see the new file at once; the applied config feeds `[security.limits]`. `config.set` and `config.reload` are full scope only. CLI `vibeke config get [key]|set <key> <value> [--persist]|reload` (path, validate and default stay local).

**Implemented (M4).**

`layout.export {tab | workspace, format?: toml}` returns `{layout, scope, toml?}`. `layout.apply` takes `{layout: LayoutSpec | name, doc?: TOML/JSON text, name?, workspace? | new_workspace?: {cwd, name}, cwd?, ws_name?, focus?}`. A JSON `doc` may be a whole export result. Applying validates first: at most 64 panes, depth 16. It spawns one pane per leaf, builds the split tree with normalised ratios, creates floats, then types each `run` line once the shell has drawn. It emits `layout.applied`. `workspace.create {layout}` and `tab.create {layout}` go through the same path. The layout's `cwd` wins over the caller's cwd; the CLI sends its own cwd as `default_cwd`. Export turns a pane started with a command into `command` (`sh -c` is unwrapped). It turns a foreground command typed into a shell, or an agent's harness id, into `run`. Cwds are relative to the workspace root.

`LayoutSpec` (`vk_proto::layout_spec`), TOML/JSON:

```toml
name = "dev"                     # optional
cwd = "~/code/app"               # workspace root for a new workspace
[[tab]]
title = "edit"
focus = true
[tab.pane]                       # a node: split + children, or a leaf
split = "right"                  # right = side by side · down = stacked
[[tab.pane.children]]
size = 0.6                       # share of the parent (normalised)
run = "nvim ."                   # typed into the shell after start
[[tab.pane.children]]
cwd = "src"                      # relative to the tab/layout cwd
command = ["npm", "run", "dev"]  # the pane's process (string → /bin/sh -c)
[[tab.float]]
command = "lazygit"
rect = { x = 15, y = 15, w = 70, h = 70 }
```

`search.query` takes `{q | text, pane?, workspace?, machine?, since?: epoch-ms | "30m"/"2h"/"7d", limit?: 50, context?: 2, regex?: false, sources?: [live, archive]}`. It returns `{hits: [{pane, pane_handle, workspace, title, source: screen|scrollback|archive, live, line, text, ts?, context: {before, after}, position: {line, from, to}}], truncated}`. Live panes are searched first, newest line first. Archive hits come from FTS5, with rows still in memory reported once, as live hits. With `regex`, the archive segments of the panes in scope are scanned instead. `line` is the absolute history line, shared by the archive, the in-memory scrollback and the screen. `pane.read {source: archive}` also returns `mem_first` (the first in-memory line of a live pane, `null` for a closed one): clients page memory with `FetchHistory` and older rows with `pane.read`. `machine` must name this machine; other machines are reached with the global `--machine`. Transcript and event sources are not searched yet.

`pane.read {source: "archive", from?, to?, lines?: 200}` returns `{rows: [{n, text, wrapped}], text, first, end, from, to, more_before, more_after}`. It covers archive segments, then the in-memory scrollback, then the screen, at most 5000 rows per call. It also works for closed panes whose archive is still on disk (`archive_panes`, 02 §3).

Read scope (09 §5.1 rule 4) applies to `pane.read`, `pane.wait_output` and `search.query` for pane-scoped callers. `security.pane_scope.read = "workspace"` (default) \| `session` \| `self`.

### 2.14a `desk.*`, `draft.*`, `notes.*` [research R2/R3; server + CLI built; TUI built, 08 §6.7]

**Session desk** (R2 "Find and reopen previous work"). A conversation index separate from `search.query`/scrollback FTS: a background indexer reads the native transcripts of runs Vibeke has seen (`AgentRun.transcript_path`; Claude JSONL and Codex rollouts via the same parser as `agent.transcript`, so turn `n` matches) and, only when the user opts in, extra transcript directories per harness (`[desk] roots`). Rows are (machine, harness, native session id, repo/cwd, turn `n`, role/kind, text, timestamp, source byte offset) in an FTS5 table in `<state>/<session>/desk.db` — a derived, rebuildable file with its own connection, never written under the state lock. Indexing is incremental (size, mtime, byte offset of the last complete line, turns seen; a shrunk file is re-indexed), bounded per pass (`pass_bytes`, at most 4 MiB per file per pass) and runs every `interval_s`; a search first indexes up to 2 MiB so a just-finished turn is findable. Model reasoning is not indexed; items are capped at 8 KiB.

| Method | Params → Result |
|---|---|
| `desk.search` | `{text, repo?, harness?, since?, until?, session?, limit?: 20, sort?: relevance\|recent, fresh?: true}` → `{hits: [{session, path, harness, machine, repo, cwd, turn, role, kind, ts, offset, snippet, status: live\|resumable\|none, live: {run, pane}, resume: {argv, command, run}}], index: {sources, rows, forgotten_sessions}}`. `repo` is a directory (matched as main repository root or cwd prefix). `since`/`until` take epoch ms, `7d`/`12h`/`30m` or `YYYY-MM-DD`. **live** = a current run is in that native session. **resumable** = a resume argv is known (a stored run's, else the harness's own) |
| `desk.sessions` | `{repo?, harness?, limit?: 50}` → `{sessions: [{session, harness, machine, repo, cwd, workspace, run, first_ts, last_ts, turns, rows, paths, status, live, resume}]}`, most recent first |
| `desk.open` | `{session, turn?, path?, focus?: false}` → `{status, action: focus_live_pane\|resume_native_session\|start_new_agent_with_context, turn_items, focused, actions: [{id: focus, label: "Focus live pane"}, {id: resume_native, label: "Resume native session", command}, {id: new_agent, label: "Start new agent with context"}]}`. Focuses the live run's pane **only** with `focus: true` (an explicit user action). Never resumes |
| `desk.context` | `{session, turns?: [n]\|"3-5,7", path?, objective?}` → `{package: {objective, decisions, remaining_work, selected_turns: [{turn, user, assistant, tool_calls}], repository: {repo, cwd, revision: {revision, branch, uncommitted_files}}}, text}` — The result is deterministic and does not use a model. The first prompt is the default objective. The user can edit labeled extracts for decisions and remaining work. The default selection is the last three turns. |
| `desk.resume` | `{session, mode?: native, pane?, workspace?}` → **Resume native session**: `conflict:session_live` while it runs (focus it instead). `unsupported` without a resume handle. Otherwise, the method types the resume command into the given pane or a new tab in the session's workspace. The new tab uses the session's working directory. A pane with an agent returns `conflict:pane_busy`. A stored run without `pane` uses `agent.resume` behavior. The result is `{mode, label, command, run}`. `{session, mode: new_agent, workspace?, start?: false, harness?, pane?, turns?, objective?}` → **Start new agent with context**: builds the `desk.context` package and saves it as a workspace draft. With `start` also starts the harness (no prompt) in a free pane → `{draft, package, sent: false, run?, pane?}`. Nothing is ever sent: the user edits and sends the draft (`draft.send`) |
| `desk.forget` | `{session}\|{repo}\|{workspace}\|{before}` → `{rows_deleted, sessions_forgotten}` — deletes index rows. Sessions are tombstoned so the indexer never re-adds them. `before` deletes older rows. Event `desk.forgotten {scope} {rows, sessions}` (no text) |
| `desk.status` | `{}` → `{db, selection: {runs, roots}, exclude, retention_days, counts, sources: [{path, harness, session, origin: run\|root, repo, rows, offset, size, pending_bytes, excluded}], last_pass}` — makes source selection, exclusions and retention visible |
| `desk.index` | `{budget?}` → `{pass: {registered, bytes, rows, purged, reset, pending, errors}}` |

**Drafts composer** (R3 "Keep drafts outside the live agent input"). Persistent drafts per workspace or tracked task, stored as `draft` entities (state tables; content never enters events), plus one notes document per workspace (`workspace_notes`) that is sent only when the user includes it. Limits: 64 KiB text, 20 attachments, 200 drafts per scope.

| Method | Params → Result |
|---|---|
| `draft.create` | `{scope?: workspace\|task, id?, text, title?, attachments?: [{kind: file\|screenshot, path\|blob\|data_b64, name?, label?}], idempotency_key?}` → `{draft}`. `id` defaults to the caller's workspace. `path` must be an existing absolute path on this server's machine. `blob` is a `blob.put` hash. `data_b64` is stored now in the pane inbox (06 A11.4) — remote clients use `blob`/`data_b64` |
| `draft.update` | `{draft, text?, title?, attachments?, add_attachment?, remove_attachment?: index, expected_rev?}` → `{draft}` (`conflict:draft_changed`, `conflict:send_in_progress`) |
| `draft.get` / `draft.list` | `{draft}` → `{draft}`. `{scope?, id?, all?}` → `{drafts}` in user order (archived drafts are not listed). A send left `sending` by a server restart is reported and persisted as `delivery_unknown` |
| `draft.reorder` | `{order: [draft ids]}` → listed drafts first in that order, the rest after |
| `draft.delete` | `{draft}` → `{deleted}` |
| `draft.combine` | `{ids: [two or more of one scope], title?, separator?: "\n\n", delete_sources?: false}` → new `{draft}` with joined text, de-duplicated attachments and `combined_from` |
| `draft.check` | `{target_run, draft?}` → `{run, pane, harness, native_conversation_id, send_path: prompt_input\|open_pane_only, unsafe?, follow_up: prompt_input, steer: false, hidden_attachments}` |
| `draft.send` | `{draft, target_run, idempotency_key, include_notes?: false, keep?: false, retry_despite_unknown?}` → `{draft, send, send_path}`. Uses task messages' guarded prompt-input path (15 §9) without a tracked task: the run and its native conversation are fixed when the send starts. The run must be idle with a known native conversation and no open interaction. No attached TUI can focus the pane. The input box must be verifiably empty. Each attachment must be visible to the pane (`sandbox::can_see`). Otherwise `conflict:send_unsafe {fallback: open_pane_to_send, draft_kept: true}` with **zero bytes written** and no attempt recorded. Delivery holds the pane input lock while it repeats the checks and submits the text. The result is `delivered` only after a matching turn starts at or after the baseline in the same conversation. Otherwise `delivery_unknown` (or `failed`). The sent text is the draft, then `Attached files:` with one path per attachment, then `Notes:` only with `include_notes`. A delivered draft is archived unless `keep`. Receipts are owner-aware (`review::receipts`). The same key replays the current outcome. After `delivery_unknown`: `conflict:reconcile_first` until `draft.reconcile`, then `retry_despite_unknown` |
| `draft.reconcile` | `{draft}` → `{send, receipt: {known, …}, may_retry, note}` — a late matching turn upgrades the attempt to `delivered`. A still-uncertain attempt is marked reconciled, which is what allows a warned retry |
| `notes.get` / `notes.set` | `{workspace?}` → `{notes: {workspace, text, rev}}`. `{workspace?, text, expected_rev?}` → `{notes}` |

Pane-scoped callers see only their own workspace's desk results, drafts and notes (`desk.search`/`desk.sessions`/`desk.context` are filtered to sources whose run was in that workspace; files found only under opted-in roots are invisible to panes). `draft.send`, `draft.reconcile`, `desk.open`, `desk.resume`, `desk.forget`, `desk.index` and `desk.status` are forbidden for pane scope (`authorize()`). Events (`draft.created/updated/deleted/reordered/sending/delivered/delivery_unknown/send_failed/reconciled`, `notes.updated`, `desk.forgotten`) carry ids, revisions, sizes and counts only. File attachments are referenced as paths on the pane's machine and checked with the sandbox visibility helper; there is no remote path translation, so a draft must live on the server that runs the target pane. Configuration: `[desk] index = true`, `roots = {}` (e.g. `claude = ["~/.claude/projects"]`, `codex = ["~/.codex/sessions"]`), `exclude = []`, `retention_days = 90`, `interval_s = 15`, `pass_bytes = 8388608`, `max_root_files = 5000`. The index is per server (machine); multi-machine desk aggregation is not built.

### 2.15 `blob.*` [M1]

| Method | Params → Result |
|---|---|
| `blob.put` | `{mime, data_b64}` (≤ 16 MiB) or `{mime, path}` (server reads local file) → `{hash, size}` |
| `blob.get` | `{hash, range?}` → `{mime, data_b64}` |
| `blob.stat` | `{hash}` → `{mime, size, created_at, refs}` |
| `blob.begin` / `.append` / `.commit` / `.abort` | Chunked upload for drops/pastes into remote panes (06 A11): `{name, size, sha256?}` → `{upload_id}` (size ≤ `paste.max_auto_bytes`). `{upload_id, offset, data_b64}` (≤ 1 MiB decoded, offset must match). `{upload_id}` → `{path}` (inbox `<blake3-12>/<name>`, 0600). Bound to the opening connection, ≤ 16 concurrent, idle uploads purged after 10 min |

*As built (Batch 2A, 2026-10-06, `crates/vk-server/src/blob_api.rs`).* Hashes are blake3 (64 lowercase hex). A blob is found in the session blob store (`<state>/blobs/<h2>/<hash>.<ext>`, with `<hash>.json` metadata: screenshots, pane screenshots) or in the pane inbox (`blob.put` and chunked uploads, `inbox/<hash12>/<name>`, counted only when the content really hashes to the hash). `blob.stat {hash}` → `{hash, mime, size, created_at, refs, path}` (`refs` = stored copies; mime from the metadata, else the extension). `blob.get {hash, range?: {offset?, length?}}` → `{hash, mime, size, offset, length, data_b64}`, at most 16 MiB per call (larger blobs are read in ranges). The stores are unified as of 3D (below). CLI `vibeke blob get <hash> [--out FILE]|stat <hash>`.

*Unified store as built (3D, 2026-10-06, `crates/vk-store/src/blobs.rs`, `crates/vk-server/src/blob_store.rs`).* Everything is a blob in `<state>/blobs/<h2>/<hash>.<ext>` with a `<hash>.json` sidecar whose `source` names the writer: `screenshot` (screenshots, pane screenshots, diff images), `inbox` (uploads) and `payload` (Turn/Item payloads, 2.7a). `blob.put` and chunked `blob.commit` still write the inbox file the agent needs a path for and now also copy it into the store (`source: inbox`, sidecar `{name, mime, pane, workspace}`; an existing sidecar is never overwritten, so a screenshot stays a screenshot; browser-staged drops and unpacked directories are not blobs). `blob.get/stat` read the store copy first and fall back to the inbox file only for uploads whose ingest failed or that predate this (`refs` counts the readable copies, so an ingested upload is 1). At server start, uploads this session recorded in `blob_owner` before the unification are ingested once (`adopt_legacy`, only files that really hash to the recorded hash). `blob.stats {}` → `{count, bytes, by_source: {source: {count, bytes}}, path}` and `blob.gc {dry_run?: false, older_than_days?: 30}` → `{dry_run, older_than_days, removed, bytes, kept_referenced, kept_young, kept_uncollectable, hashes}` (full scope only): `blob.gc` removes only `inbox` and `payload` blobs that no item references and that are older than the age (file mtime); screenshots follow their own retention (`[screenshots] keep_days`) and blobs without a source are never collected. The hourly storage sweep (`[events] blob_retention`, default 30 days) runs the same collection. Tests: `crates/vk-server/src/blob_store_tests.rs`, `crates/vk-store/src/blobs.rs`.

*Ownership as built (batch 2 review, 2026-10-06).* Knowing a hash is not authorization. The pane inbox is shared by every session of the installation, so `blob.put` / `image.upload` / `blob.commit` record the uploader in the session's store (`kv blob_owner/<hash>`: pane, its workspace, client) and `blob.get` / `blob.stat` find an inbox file only when this session recorded it, never by searching another session's uploads. A pane-scoped caller reads only blobs it or its workspace owns: inbox uploads by that record, blob-store files by their metadata's `pane`/`workspace`. Anything else answers `not_found`, exactly like an absent hash (no existence oracle), and `refs` counts only the copies the caller may read. Test: `blob_api::tests::blob_reads_are_scoped_to_the_owning_session_and_pane`.

### 2.15a `git.*` [16 G2]

Read-only views of a pane's working tree, used by the gateway's Changes screen (16 §7.7). Git runs with fsmonitor, external diff and textconv disabled, a 5 s timeout and output caps; untracked files are read without following symlinks; secret-looking files (`.env*`, keys, credential files) are reported without content.

| Method | Params → Result |
|---|---|
| `git.status` | `{pane}` (pane scope: own pane only) or `{path}` (full scope) → `{repo_root, branch?, upstream?, ahead, behind, clean, truncated, files: [{path, orig_path?, x, y, kind: modified|added|deleted|renamed|untracked|conflicted, staged, adds?, dels?, binary, secret}]}`. `not_found:not_a_repo` outside a repository |
| `git.diff` | `{pane|path, file, staged?}` → `{file, diff, truncated, binary, untracked, secret?}`. `file` must be a relative path listed by `git.status`. Diff against `HEAD` (or the index with `staged`), capped at 512 KiB |
| `git.diff` with `base` or `range` | `{pane|path, base: <ref>, file?}` diffs that revision against the working tree. `{pane|path, range: "a..b"|"a...b", file?}` diffs committed history. With `file` → `{file, rev, diff, truncated, binary, untracked: false, secret?}` (any safe relative path, not only changed ones). Without → `{rev, files: [{path, adds?, dels?, binary, secret}], truncated}` (≤ 2000). `base` and `range` are exclusive |
| `git.log` | `{pane|path, base?, limit?: 50 (≤ 200)}` → `{commits: [{sha, short, author, ts (epoch ms), subject}], truncated}`, newest first. With `base`, only commits in `base..HEAD` |
| `fs.list` | `{pane, path?: ""}` → `{path, entries: [{name, kind: file|dir|symlink|other, size? (files), ignored, secret}], truncated}` — one level of the directory `path` relative to the repository root of the pane's cwd. Directories first, then by name. ≤ 2000 entries. Symlinks are listed, never followed. `.git` is not listed. Secret-looking entries are listed with `secret: true` and no size, and a secret directory lists as `{secret: true, entries: []}` |
| `fs.read` | `{pane, path}` → `{path, text?, binary, truncated, size, secret}` — at most 512 KiB of a regular file (`truncated` when larger, cut on a character boundary). `binary` (NUL in the first 8 KiB) returns no text. Secret-looking files return `{secret: true}` without content |

Refs (`base`, both sides of `range`) must match `^[A-Za-z0-9][A-Za-z0-9._/@{}^~-]{0,200}$` and contain no `..`; a range is exactly two refs joined by `..` or `...`. They are passed after `--end-of-options`, so a ref can never be read as an option (`invalid_params` otherwise). `fs.*` paths must be relative with only normal components (no `..`, no absolute paths, no `.git` component) and are opened with an `openat` walk using `O_NOFOLLOW` at every component, so a symlink anywhere in the path is refused (`permission_denied`). `ignored` comes from `git check-ignore`. Every git call uses the same hardened runner as `git.status` (also `log.showSignature=false`). Pane-scope callers may only target their own pane.

### 2.15b `assistant.*` [14; as built 2026-10-06]

User-invoked LLM drafts (spec 14). Off by default (`[assistant] enabled = false`); every method refuses pane-scoped callers (`permission_denied`, `details.scope = "pane"`). Errors carry `details.category` (14 §8: `disabled`, `not_configured`, `permission_denied`, `context_too_large`, `queue_full`, `budget_exhausted`, `authentication_failed`, `rate_limited`, `provider_unavailable`, `invalid_output`, `timeout`, `cancelled`, `interrupted`) and, for consent/preview problems, `details.reason` (`consent_required`, `consent_invalidated`, `operation_not_granted`, `context_class_not_granted`, `preview_mismatch`, `preview_expired`, `not_awaiting_confirmation`, `assistant_read_only`, `idempotency_key_reused`). CLI noun: `vibeke assist <verb>`.

| Method | Params → Result |
|---|---|
| `assistant.status` | `{}` → `{enabled, configured, config_problem?, coordinator: {machine, session}, profile?: {id, connection, adapter, model, endpoint_host, credential: "env:NAME"\|"file:PATH"\|..., limits, pricing_usd_per_mtok?}, limits, today: {utc_day, used, reserved, remaining}, auto_send, consents, requests: {awaiting_confirmation, queued, running, stored}, operations, background: false}` — no secrets, no provider contact |
| `assistant.providers` | `{}` → `{connections: [{id, adapter, endpoint, endpoint_error?, credential, verified}], profiles, default_profile}` |
| `assistant.consent` | `{workspace?, connection?\|profile?, classes?: [selected_text\|structured_state\|review_package\|screen], operations?: [op], auto_send?: [op]}` → `{consent, notice}`. Default classes exclude `screen`. The grant binds the canonical workspace path to the connection's adapter+endpoint fingerprint |
| `assistant.revoke` | `{workspace?, connection?}` → `{revoked, cancelled_requests}`. Cancels that workspace's unfinished requests |
| `assistant.generate` | `{operation: suggest_task_details\|review_summary\|pane_title\|briefing\|handoff\|effort_estimate, profile?, idempotency_key?, retry_of?, inputs \| top-level: {run?, turns?: [n], pane?, task?, workspace?, include_screen?}}` → `{request, preview: {digest, system, user, model, adapter, endpoint_host, execution_machine, max_output_tokens, bytes, estimated_input_tokens, estimated_max_cost_usd?, sources, omitted, redactions, notice}, requires_confirmation, confirm_with?}`. Consent is checked before content is retrieved. Nothing is sent unless the operation is in both `[assistant] auto_send` and the workspace consent's `auto_send` |
| `assistant.confirm` | `{request, preview_digest}` → `{request}` (state `queued`). Rechecks consent, endpoint, queue, rate and daily budget. Sends exactly the previewed payload |
| `assistant.get` | `{request}` → `{request: {id, operation, state: awaiting_confirmation\|queued\|running\|done\|failed\|cancelled\|interrupted, workspace, inputs (IDs only), profile, connection, adapter, model, endpoint_host, machine, prompt_version, sources (metadata + digests), omitted, redactions, context_digest, preview_digest, payload_bytes, estimated_input_tokens, max_output_tokens, usage: {input_tokens?, output_tokens?}, attempts, estimated_cost_usd?, finish_reason?, error?: {category, message}, output?: {generated: true, label, ...operation fields}, timestamps}}` |
| `assistant.list` | `{workspace?, state?, limit?}` → `{requests}` (without outputs) |
| `assistant.cancel` | `{request}` → `{request}`. Aborts the provider call. A canceled active request still counts its reservation against the day's budget |
| `assistant.purge` | `{request}\|{workspace}\|{all: true}` → `{purged}`. Deletes records and generated outputs (cancels unfinished ones first) |

Operation outputs are validated drafts (unknown fields dropped, cited `source_refs`/`targets` must belong to the request). `suggest_task_details` → `{title, objective, constraints, criteria (required: false), suggested_checks (selected: false), stop_at, questions}` for the user to edit and pass to `task.track`; `review_summary` → `{summary, changes, validation: [{text, basis: recorded_check|observed_command|agent_claim|unverified, source_refs}], outstanding, risks}`; `pane_title` → `{title}` (never applied); `briefing` → `{items: [{text, kind, urgency, targets, source_refs}], coverage}`; `handoff` → `{objective, decisions, attempts, remaining, evidence, open_questions}` (never sent); `effort_estimate` (15 T4, `--task`, class `review_package`) → `{effort: quick|minutes|deep, rationale, source_refs, estimate_source: assistant, applied: false, apply_with: {method: task.set, params: {effort}}}` (never applied). Events: `assistant.request_created`, `assistant.request_started`, `assistant.request_finished`, `assistant.consent_granted`, `assistant.consent_revoked`, `assistant.purged` — metadata only (operation, state, adapter, connection, model, endpoint host, source/redaction counts, payload bytes, token counts, attempts, estimated cost, error category).

### 2.16 `plugin.*` [M5]

*M5 first slice (as built, 2026-10-06):* `plugin.list`, `plugin.action.list {plugin?}` (actions of every registered plugin, `available` only when trusted and enabled), `plugin.action.run {plugin, action | action: "<plugin>.<action>", pane?, workspace?, tab?}` → `{log}` (returns the running log record at once) and `plugin.log.list {plugin?, limit?}` are implemented in `vk-server/src/compat.rs`, plus `compat.herdr.call {method, params}` (the CLI shim's transport) and `compat.status`. Install/link/trust/enable/disable/unlink/uninstall are CLI operations on the per-user registry file (`vibeke plugin …`), not API methods yet. `plugin.install` over the API, `plugin.pane.*` (native), `plugin.kv.*` and `ui.contribute` are not built; git sources are CLI installs (see "Plugin completion" in §7.7). Later additions: `plugin.list` entries carry `origin`, `keybindings`, `isolate`, `sandbox_available`, `max_concurrent` and `settings_warnings`; `plugin.action.list` also returns `keybindings`; `plugin.registry.notify` (full scope) announces a registry change (`plugin.registry_changed`); `compat.ui.state` also returns `agent_views`.

*M5 slice 2 (as built, 2026-10-06):* the compat endpoint (and `compat.herdr.call`) also implements `plugin.link {path}`, `plugin.unlink/enable/disable {plugin_id}` (refused from panes; link never builds or trusts) and `plugin.pane.open {plugin_id, pane|entrypoint, placement?, direction?, cwd?, focus?, pane_id?, workspace_id?}` / `plugin.pane.focus` / `plugin.pane.close` for the `split`, `tab`, `zoomed` and `overlay` placements (an overlay is a zoomed pane that restores the prior focus when it ends; real overlay rendering and `popup` need the TUI, so `popup` returns `unsupported`). Each plugin pane runs in an ordinary Vibeke pane with its own broker for the pane's lifetime and the `HERDR_PANE_ID`/`HERDR_TAB_ID`/`HERDR_WORKSPACE_ID` of that pane. `popup.close`, `plugin.kv.*`, `ui.contribute` and installs from repositories are not built.

*M5 slice 3 (TUI, as built, 2026-10-06):* `plugin.pane.open` takes every placement. `popup` and `overlay` start the command in a floating pane of the target tab whose `created_by` is the tag `plugin-surface:<popup|overlay>:<width>:<height>:<plugin>/<entrypoint>` (`vk_proto::model::PluginSurface`), so every client recognizes it from the model without a render-protocol change; no `tab.layout_changed` is emitted. A popup gets no pane id (`pane: null` in the result), is hidden from the compat projection (pane lists, counts, events, focus: a focused popup reports the pane underneath), runs without `HERDR_PANE_ID`, and its broker's default pane is the pane underneath; one popup per session (`popup_busy` otherwise). `popup.close` (compat; a plugin closes only its own popup, `popup_not_found` when none is open) and the native `plugin.surface.close {pane}` close a popup/overlay and give the focus back to the pane that had it; both also close when their command exits. New native methods: `plugin.link_handler.list` → `{handlers: [{plugin_id, handler_id, title, pattern, action_id, available, status}]}`, `plugin.link.open {plugin, handler, url, pane?}` → `{log}` (the URL must match the handler's pattern; the action gets `HERDR_PLUGIN_CLICKED_URL`/`HERDR_PLUGIN_LINK_HANDLER_ID` and `clicked_url`/`link_handler_id` in its context JSON, source `link_handler`), `compat.ui.state` → `{window_title, popup}`, and `plugin.action.run` accepts `source: palette|keybinding`. TUI behavior: 08 §5, §6.3, §6.8, §10.3.

| Method | Params → Result |
|---|---|
| `plugin.list` | `{}` → `{plugins: [{id, version, enabled, kind: actions|process, capabilities, status}]}` |
| `plugin.install` | `{source: "owner/repo[/subdir]"\|path\|url, ref?, accept_capabilities?: [..], trust?: scoped\|herdr_legacy}` → `{plugin, requested_capabilities}` — returns `permission_denied:capabilities_not_accepted` until confirmed. Herdr manifests use the explicit legacy trust grant in §7.7 |
| `plugin.link` | `{path}` → `{plugin}` (dev mode, hot reload) |
| `plugin.enable` / `plugin.disable` / `plugin.remove` | `{plugin}` → `{plugin}` |
| `plugin.action` | `{plugin, action, context?: {workspace?, pane?}}` → `{exit_code, stdout_tail}` |
| `plugin.action.list` / `plugin.action.invoke` / `plugin.log.list` | native aliases for the asynchronous action/log service. The compat endpoint preserves Herdr's exact params and response shapes (§7.7, §8.3), including returning a running log record before completion |
| `plugin.pane.open` / `plugin.pane.focus` / `plugin.pane.close` / `popup.close` | managed terminal entrypoints and session-modal popups. Herdr-compatible behavior in §7.7 |
| `plugin.kv.get` / `plugin.kv.set` / `plugin.kv.delete` / `plugin.kv.list` | plugin-only, scoped to caller's plugin id |
| `ui.contribute` | plugin-only — see §7.4 |

### 2.17 `integration.*` [M1]

| Method | Params → Result |
|---|---|
| `integration.list` | `{}` → `{integrations: [{harness, installed, version, files: [path], up_to_date}]}` |
| `integration.install` | `{harness, scope?: user|project}` → `{files_changed}` — writes hook configs / extension registration. Idempotent. Never overwrites user hooks (merges, marks its entries) |
| `integration.uninstall` | `{harness}` → `{files_changed}` |
| `integration.doctor` | `{harness?}` → `{checks: [{name, ok, detail}]}` |

---

## 3. Render stream protocol [M1]

**This section is the single normative definition of the render stream.** 01 §3.2 and 03 describe behaviour and rationale and link here; message names, fields and encodings are defined only in `vk-proto::render` and this table.

Opened by `render.attach` on a fresh connection, authenticated by the same `client.hello` identity rules as control connections (09 §3.2); after the JSON response the connection switches to binary frames. `postcard`; frame = `u32 LE length | u8 frame_type | postcard payload`.

*Version negotiation (Goal 03 review follow-up):* postcard is positional, so any change to a type reachable from `ServerFrame`/`ClientFrame` (even an appended `#[serde(default)]` field) changes the encoding and bumps the render protocol (`vk_proto::render::PROTOCOL`, now **4**: 2 = browser panes and media frames, 3 = `BrowserPane.device`/`viewport` and `BrowserCmd::DropFiles`, 4 = terminal effects (03 §8, §9): appended `Row.mark`/`Row.links` and `SessionModel.pane_live`, appended frames `ServerFrame::ClipboardQuery`/`Image`/`PaneImages` and `ClientFrame::ClipboardReply`). The client sends `params.protocol`; the server refuses a different or missing one with error kind `version_mismatch` (`details {server_protocol, client_protocol, server_version}`, message with an upgrade hint) and closes the connection; the client refuses a reply whose `result.protocol` differs. There is no compatible downgrade representation: the older side must be upgraded (`vibeke machine upgrade <machine>`).

```json
→ {"jsonrpc":"2.0","id":1,"method":"render.attach","params":{"client_id":"c7","viewport":{"cols":220,"rows":60,"px_w":3520,"px_h":1920},"caps":{"truecolor":true,"kitty_graphics":true,"sixel":false,"iterm2_images":false,"kitty_keyboard":true,"osc52":true,"hyperlinks":true,"max_fps":120,"sync_output":true}}}
← {"jsonrpc":"2.0","id":1,"result":{"protocol":1,"frame_types":[…]}}
```

### 3.0 Revisions, epochs and geometry

- **Per pane, per client** the server tracks `(epoch, rev)`. `epoch` increments on anything that invalidates incremental state (client attach, pane resize/reflow, alt-screen switch, VT reset, server recovery); `rev` increments per frame within an epoch.
- Every `PaneDiff` carries `{epoch, base_rev, rev}` and is computed against **`base_rev` = the client's last acked rev**. The client applies a diff only if its current state is exactly `base_rev` in the same `epoch`; otherwise it drops it and sends `Resync{pane}`. The server never sends two in-flight diffs with the same `base_rev`: while a diff is unacked it either waits or sends a `PaneFull` (bounded by `render.max_unacked = 2`). This makes scroll ops and image placements safe to apply in order.
- **Geometry controller.** A PTY has one size. Per pane, exactly one attached client holds the **geometry lease** (default: the client that most recently sent input to or focused that pane; `pane.size_policy = "latest" | "smallest" | "pinned"`). Other clients render the pane at the controller's size, cropped or letterboxed in their own layout (the TUI shows a `⇲ 180×50 (other client)` hint). Lease changes resize the PTY and start a new epoch.
- **Client-local presentation never mutates shared terminal semantics.** Host-dependent choices (emoji width tables, theme light/dark, font metrics) are per client; the server's VT state and the PTY see one canonical configuration (03).

### 3.1 Server → client frames

| Type | Name | Payload |
|---|---|---|
| 0x01 | `Hello` | `{protocol, server_version, session, palette, theme}` |
| 0x02 | `Layout` | full UI model for this client: workspaces/tabs tree, active tab layout rects, floating panes, sidebar model (agent states, unread, pins), status bar segments (incl. plugin segments), geometry leases |
| 0x03 | `PaneFull` | `{pane, epoch, rev, cols, rows, cells: RLE-encoded rows, cursor, modes}` |
| 0x04 | `PaneDiff` | `{pane, epoch, base_rev, rev, ops: [ScrollUp{top,bottom,n} \| Rows{(row_idx, RLE cells)} \| Clear{rect}], cursor, modes}` |
| 0x05 | `Image` | `{hash, mime, size, data?}` — data sent once per client; placements reference hash |
| 0x06 | `ImagePlacement` | `{pane, epoch, rev, id, hash, cell_rect, z, crop}` / removal |
| 0x07 | `Notify` | `{notification}` (toast) |
| 0x08 | `Bell` | `{pane}` |
| 0x09 | `Clipboard` | `{selection: clipboard|primary, data, origin_machine}` — OSC 52 from a pane, policy-gated (06 A9, 09 §7) |
| 0x0A | `Title` | `{pane, title}` |
| 0x0B | `ModeChange` | `{pane, mouse_mode, bracketed_paste, kitty_kbd_flags, focus_events, alt_screen}` |
| 0x0C | `Popup` | user-invoked modal (confirmations, pickers, interaction cards, peek): `{id, kind, model, invoked_by: client_action}` — the server never sends an unsolicited `Popup` over a client's focused pane (08 §0) |
| 0x0D | `Pong` | `{nonce, server_ts}` |
| 0x0E | `Goodbye` | `{reason}` |
| 0x0F | `InputAck` | `{input_id, status: written \| rejected{reason} \| dropped_offline}` |
| 0x10 | `GeometryLease` | `{pane, holder_client, cols, rows}` |

Cell encoding: `{ch: u32 grapheme-id or inline char, width: u8, fg, bg, ul_color, attrs: u16, link_id?}`; graphemes beyond one scalar are interned per stream (`GraphemeTable` updates piggyback on diffs, scoped to the epoch). Hyperlink targets interned similarly.

### 3.2 Client → server frames

| Type | Name | Payload |
|---|---|---|
| 0x81 | `Ack` | `{pane, epoch, rev}` |
| 0x82 | `Key` | `{input_id, pane, key: KeyEvent}` — **the normal input path**: logical key events; the server's single canonical encoder turns them into bytes for the pane's negotiated keyboard mode (03) |
| 0x83 | `RawInput` | `{input_id, pane, bytes}` — exceptional: raw passthrough (copy of unrecognized host sequences, explicit "send raw" mode); scope-checked like any write |
| 0x84 | `Mouse` | `{input_id, pane, event, cell, px?, mods}` |
| 0x85 | `Paste` | `{input_id, pane, text}` (server applies bracketed paste if enabled; large pastes chunked) |
| 0x86 | `Resize` | `{viewport}` |
| 0x87 | `Command` | `{rpc: JSON-RPC request}` — UI commands piggyback here to keep ordering with input |
| 0x88 | `Focus` | `{pane}` |
| 0x89 | `Ping` | `{nonce, client_ts}` |
| 0x8A | `ViewHint` | `{visible_panes, fps_cap}` |
| 0x8B | `Resync` | `{pane}` — request a `PaneFull` |

**Input delivery.** `input_id` is a client-generated u64 (monotonic per client). The server writes each input to the holder with the same id (§4) and sends `InputAck` once the holder confirmed the write to the PTY, or `rejected` (scope, `input_locked_open_interaction` per 09 §5.1.4, pane exited) — never silently. On reconnect the client may resend unacked inputs with their original ids; the holder dedupes by `(server epoch, input_id)` within its window, so an input is written at most once. Offline remote panes reply `dropped_offline` (06 A7).

Pacing: server sends at most `min(client.max_fps, adaptive)` frames per pane; unfocused panes whose damage is spinner-only are capped at `ui.background_animation_fps` (default 4, 08 §11). State-sync means dropped intermediate frames are lossless in the end state.

**Media channel as built (Goal 03 Stage 2).** Appended to `vk-proto::render`: `ServerFrame::Media(MediaFrame)` (changed cell-aligned tiles of one browser pane frame; `reset` = every tile, after a geometry change or for a new viewer; tile data `Shm {name, len}` for a same-machine client that set `shm`, else `ZlibRgba`), `ServerFrame::BrowserState {pane, state: BrowserStatus {url, title, loading, can_back, can_forward, env, windowed, error, notice, css_w, css_h}}`, `ClientFrame::MediaView {panes: [MediaPane], shm, key_releases}` (the visible set; `MediaPane {pane, owner, spec: BrowserPane, cols, rows, cell_w, cell_h, dpr}`), `ClientFrame::MediaAck {pane, seq}` and `ClientFrame::Browser {input_id, pane, cmd: BrowserCmd}` (acked with `InputAck`). Unacked media frames per pane: 2 (local client) / 1 (remote); they are written after cell frames; a client must unlink shm tiles it does not hand to its host. Semantics: 03 §5, 06 B3.2.

**Browser pane page I/O as built (06 B3.2 "not built yet" items, render protocol 3).** `BrowserCmd::DropFiles(Vec<String>)` (appended): paths on the rendering server's machine the user confirmed; the server re-checks each (absolute, readable regular file after resolving symlinks, ≤ 50 MiB, ≤ 16 files) and gives them to an open file chooser (`DOM.setFileInputFiles`) or drops them at the last pointer position (`Input.dispatchDragEvent` enter/over/drop); the outcome is a `BrowserStatus.notice`. Page clipboard writes reuse `ServerFrame::Clipboard {selection: Clipboard, data, pane: <browser pane>}`, sent to the pane's viewers; the TUI judges them as writes from the machine that owns the pane. `BrowserPane.device`/`viewport` (appended) travel in `MediaPane.spec`; the media host fits the pinned viewport into the content area itself (frames keep the content area's size, the page centred on a neutral fill), so the tile protocol is unchanged.

**Event push as built (Goal 03 Stage 3 follow-up).** The `render.attach` result lists the server's optional features: `{"protocol": 1, "client_id": …, "features": ["event_push"]}`. A client may send `ClientFrame::Subscribe {types: [glob], after: Option<i64>}` **only** to a server that lists `event_push` (an older server's postcard decoder would drop the connection on an unknown variant; such servers are polled with `events.read` as before). `types` use the `events.read` globs (`client.confirm_*`, `interaction.*`); an empty list unsubscribes; a new `Subscribe` replaces the previous one; with `after` the server first replays matching events after that sequence number (at most 1000; older ones via `events.read`). The server answers with `ServerFrame::Events {events: [PushedEvent {seq, kind, json}], lagged}` — `json` is the event object exactly as `events.read` returns it (`seq, ts, v, tier, type, subject, actor, data`; JSON because postcard can't carry arbitrary `serde_json::Value`), batched, in sequence order, deduplicated across replay and live delivery. `lagged: true` means the server's broadcast buffer overflowed for this client (events were dropped); the client catches up with `events.read` from its cursor. Both variants are appended (`Events` after `BrowserState`, `Subscribe` after `Browser`). New events for this: `client.attached` / `client.detached {subject: {client}, data: {kind, remote}}` (render attach/detach) and `client.devices_changed {subject: {client}, data: {devices}}` (a gateway's `client.devices` list changed). The TUI subscribes to `client.confirm_*`, `client.attached`, `client.detached`, `client.devices_changed`, `interaction.*`, `browser.session_*`, `browser.taken_over`, `browser.released` and `screenshot.*`.

**Scroll reports as built (M5 slice 3).** `ClientFrame::ScrollView {pane, offset, total}` is appended after `Subscribe`: the client's copy-mode viewport is `offset` rows above the live screen (0 = back at the bottom) of `total` known rows. It is sent only to servers whose `render.attach` features list `scroll_report` (coalesced to one per 150 ms, the last change flushed by the 250 ms tick). The server keeps the last offset per pane (`scroll` in compat pane records) and emits `pane.scroll_changed {subject: pane, data: {offset, total, client}}` when it changes, which the compat projection turns into Herdr's `pane.scroll_changed` (`scroll` in the payload). Appending a variant leaves existing encodings unchanged, so the protocol number stays 2.

**Sync input as built (batch 2 review, 2026-10-06).** `ClientFrame::SyncInput {input_id, pane, input: SyncPayload::Key(KeyEvent) | SyncPayload::Paste(String), include_agent}` is appended after `ClipboardReply`, sent only to servers whose `render.attach` features list `sync_input` (older servers get the plain `Key`/`Paste` frames, with client-side exclusion only). It carries a key or paste the TUI mirrors from the focused pane (08 §5); `include_agent` says the user explicitly added that pane (or `ui.sync_input.include_agents`). The server, not the client's possibly stale model, enforces exclusion: a `SyncInput` to a pane with a live agent run (a run on that pane without `ended_at_ms`) and `include_agent: false` is dropped with `InputAck Rejected`, so an agent that started after the client's last model update never receives mirrored prompts or approval keys. A read-only client's `SyncInput` is rejected like any input. Test: `run::auth_tests::mirrored_input_never_reaches_a_live_agent_pane_unless_included`.

**Watch mode as built.** `BrowserPane` gets a last field `watch: Option<String>` (`#[serde(default)]`; the agent browser session the pane shows), `BrowserStatus` gets `watch: Option<String>`, `human_control: bool`, `controlled_here: bool` (appended, `#[serde(default)]`), and `BrowserCmd::TakeOver(bool)` is appended. A client reports a watch pane in its `MediaView` to the **pane's own server** (the machine running the agent's browser), never to the laptop's media host; frames are the agent screencast scaled into the pane and cut into the usual tiles. Semantics: 06 B7.

---

## 4. Holder protocol [M1]

Between server and `vibeke hold`. Kept deliberately small and **separately versioned** (`holder/1`). Lives in `vk-proto::holder`, no dependency on the rest of the server.

Frame: `u32 LE length | postcard(enum)` — the postcard enum discriminant is the type (Goal 01; the type numbers below document intent). Implemented additions: `Rejected{reason}`, `ReplayDone{offset, queued_queries}` (end of the replayed region; carries queries the holder queued while no server was attached), `CheckpointWanted{offset}`, `FgChanged`. Pipe mode is specified but not built yet (needed with headless adapters). Socket: `$RUNTIME/<session>/holders/<pane-ulid>.sock`, 0600, peer-UID checked.

**Authentication.** At spawn the server generates a 32-byte **holder key** and passes it to the holder through the 0600 env file (not the child's env). `HelloOk` carries a random `nonce`; `Acquire` must carry `hmac = HMAC-SHA256(holder_key, nonce ‖ epoch)`. Every later frame from the server is accepted only on the authenticated, acquired connection. The key is persisted (0600) in `state.db` so a restarted server can re-acquire. Pane processes never see it (09 §3.1).

| Dir | Type | Message | Payload |
|---|---|---|---|
| S→H | 0x01 | `Hello` | `{proto_min, proto_max, server_pid, server_boot_id}` |
| H→S | 0x02 | `HelloOk` | `{proto, holder_version, pane_id, mode: pty|pipe, child_pid, started_at, ring: {start_offset, end_offset, capacity}, last_checkpoint?, nonce}` |
| S→H | 0x03 | `Acquire` | `{epoch: prev+1, server_pid, hmac}` — takes the lease; frames with an older epoch are rejected (**fencing**) |
| H→S | 0x04 | `Acquired` | `{epoch}` |
| S→H | 0x05 | `Attach` | `{epoch, from_offset}` |
| H→S | 0x06 | `Output` | `{offset, stream: pty|stdout|stderr, bytes, replay: bool}` — `replay = true` for bytes older than the moment of `Attach`. The server feeds replayed bytes to the VT engine with **side effects suppressed** (no notifications, bells, clipboard writes, query responses, archive appends that already happened); live bytes have `replay = false` |
| H→S | 0x07 | `Gap` | `{requested, available_from}` — ring overflowed past `from_offset` |
| H→S | 0x12 | `Marker` | `{offset, kind: Resize{cols, rows, px_w, px_h} \| InputWritten{input_id} \| ServerDetached \| ServerAttached{epoch}}` — markers are stored in the ring's side index, so replay interleaves resizes with bytes in the exact order they hit the PTY |
| S→H | 0x08 | `Input` | `{epoch, input_id, bytes}` |
| H→S | 0x13 | `InputAck` | `{input_id, offset_at_write, status: written|duplicate|child_exited|failed}` — sent only after every byte reached the PTY (`failed`: write error or PTY closed with bytes pending; the id leaves the dedupe set). Dedupe window: last 4,096 `input_id`s |
| S→H | 0x09 | `Resize` | `{epoch, cols, rows, px_w, px_h}` → holder applies `TIOCSWINSZ` and records a `Marker{Resize}` |
| S→H | 0x0A | `Signal` | `{epoch, sig: INT|TERM|HUP|KILL|WINCH|CONT|STOP, target: fg_pgrp|child}` |
| S→H | 0x0B | `Status?` | `{}` |
| H→S | 0x0C | `Status` | `{child_pid, fg_pgid, fg_cmdline, fg_cwd?, exited: bool, exit_code?, signal?, tty_modes: {echo, icanon}}` |
| H→S | 0x0D | `ChildExited` | `{exit_code?, signal?}` |
| S→H | 0x0E | `AckExit` | `{epoch}` → holder flushes and exits |
| S→H | 0x0F | `Checkpoint` | `{epoch, offset}` — offset safely captured in a VT snapshot; reported back in `HelloOk.last_checkpoint` |
| either | 0x10 | `Ping` / 0x11 `Pong` | keepalive (10 s) |

**Pipe mode (headless processes).** `vibeke hold --pipe` spawns the child with stdin/stdout/stderr pipes instead of a PTY. Used for headless harness processes (`pi --mode rpc`, `omp --mode rpc`, `codex app-server`, ACP agents, `claude -p --output-format stream-json`) so that **their protocol streams survive a server restart** like PTY panes do: stdout/stderr go into the ring as `Output{stream}`, stdin writes use `Input`. On re-attach, the adapter replays (`replay = true`) only to rebuild its parser state, then reconciles with the harness (e.g. `get_state`, `thread/read`) before acting on any pending request — it never re-sends a request it cannot prove was unanswered. `Resize`/`Signal{WINCH}` are ignored in pipe mode.

- Spawn: server execs `vibeke hold --pane <ulid> --socket <path> --ring 16MiB [--pipe] --cwd <dir> --env-file <tmp 0600, deleted after read> -- <argv>`; holder double-forks, `setsid`, opens the PTY (or pipes), spawns the child, writes `ready` on an inherited pipe, and closes it.
- Holder never parses terminal output. It *does* track the foreground process group (`tcgetpgrp`) and read its cmdline/cwd (`/proc` or `libproc`), because that is needed by detectors even when no server is attached.
- No server attached for > `holder.orphan_timeout` (default: never) → keep running. `vibeke doctor` lists orphaned holders; `vibeke hold --list`/`--kill <pane>` exist for recovery.
- Compatibility rule: server N supports holder protocol `holder/N` and `holder/N-1`; holders are never force-upgraded while their child lives.

---

## 5. CLI

The CLI is generated around the API: every noun is an API namespace, every verb a method. `vibeke <noun>` with no verb prints that noun's help (never executes), and no mutating command runs with all-default args by omission of a verb.

### 5.1 Global flags

`--session <name>`, `--machine <label>`, `--json` / `--pretty` (default: JSON when stdout is not a TTY, pretty tables when it is), `--quiet`, `--timeout <ms>`, `--socket <path>`, `--no-color`, `-v/-vv` (tracing to stderr).

### 5.2 Exit codes

| Code | Meaning |
|---|---|
| 0 | success (for `wait` commands: the awaited condition happened) |
| 1 | server/API error — JSON error object on stderr (`{"error":{"kind":…,"message":…}}`) |
| 2 | CLI usage error (bad flags, unknown command) |
| 3 | timeout (wait commands) — distinct from 1 so scripts can loop |
| 4 | server not running (and `--no-spawn` given) |
| 5 | permission denied (capability or trust) |

### 5.3 Command tree

```
vibeke                                    # launch/attach TUI (default session)
vibeke attach [--session s] [--machine m] [--readonly]
vibeke status [server|client]             # human summary; --json for scripts
vibeke server [start|stop|restart|status|reload-config] [--kill-panes]
vibeke session list|new <name>|stop <name>|rename <a> <b>

vibeke workspace list|get|create|rename|move|focus|close
vibeke group     list|create|rename|move|delete|collapse|add|remove
vibeke tab       list|create|rename|move|focus|close
vibeke pane      list|get|current|split|float|embed|move|resize|zoom|focus|rename|close
                 send-text|send-keys|run|read|wait-output|wait-idle
                 mark-unread|pin|sync-input|scroll|screenshot
vibeke agent     list|get|start|spawn|prompt|wait|interrupt|send-keys|read|transcript
                 rename|release|resume|harnesses
vibeke interaction list|get|answer|cancel   (alias: vibeke ask …)
vibeke policy    list|add|remove|test|trust
vibeke task      new|list|get|park|resume|finish|archive|setup-log
vibeke worktree  list|create|open|remove|repo-root
vibeke preview   list|declare|promote|dismiss|open|url|mirror|unmirror|forget|profile|show
vibeke browser   open|navigate|click|type|press|wait|eval|screenshot|snapshot|console|network|dom|close|list|status|install|take-over|release|diff
vibeke mcp       stdio MCP server (06 B7)
vibeke image     show|upload
vibeke notification list|send|read
vibeke events    tail [--types agent.*] [--after-seq N] [--follow] | read | wait
vibeke search    <query> [--pane p] [--workspace w] [--since 2h] [--regex] [--context n] [--sources live,archive]
vibeke assist    status|providers|consent|revoke|generate|confirm|show|list|cancel|purge   # 14, off by default
vibeke layout    export|apply|list|get     # apply <name | file.toml | --doc text> [--workspace w | --cwd d]
vibeke focus     <pane | vibeke://focus?session=…&pane=…>   # click-to-focus target (08 §7.1)
vibeke theme     get|set-mode <auto|light|dark>
vibeke status    segments [--pane p]
vibeke desk      search <words>|sessions|open <session> [--turn n] [--focus]|resume <session> [--mode new_agent [--start]]|context|forget|status|index
vibeke draft     new <text|-> [--workspace w|--task t] [--file p] [--screenshot p]|list|show|edit|check|send <draft> --run r [--include-notes]|reconcile|combine <ids…>|reorder <ids…>|rm
vibeke notes     get|set <text|-> [--workspace w]
vibeke machine   list|add|connect|disconnect|remove|status|install
vibeke integration list|install <harness>|uninstall <harness>|doctor
vibeke plugin    list|install|link|unlink|enable|disable|remove|uninstall|config-dir|action|log|logs|pane
vibeke config    path|get|set|validate|edit|reset-keys
vibeke import    herdr [--config] [--session] [--dry-run]
vibeke api       schema|methods|call <method> [json]      # raw access
vibeke doctor    [--fix] [--rebuild-index]   # --rebuild-index: rebuild FTS + derived caches (state tables are the source of truth, 02)
                                # as built: --rebuild-index rebuilds scrollback_fts/archive_panes from the segments, offline, server stopped (02); --fix is not built
vibeke forget    --pane p|--workspace w|--before t|--all [--yes] [--dry-run]   # scrollback archive only (02, 09 §9.3); method scrollback.forget
vibeke debug     bundle [--out file] | holders | replay <pane> | api-schema [--out file] [--method name]
vibeke update    [--check] [--channel stable|preview] [--rollback]
vibeke channel   get|set <stable|preview>
vibeke completion <zsh|bash|fish|nu|powershell>
vibeke hook      <harness> <event>                          # called by agent hooks (04)
vibeke hold      …                                          # internal
vibeke bridge                                               # internal (remote)
vibeke --skill | --default-config | --version | --help
```

*As built (Batch 2A, 2026-10-06; batch 2 review).* `vibeke attach --readonly` says `client.hello {readonly: true}` on the TUI's connection before `render.attach`. Read-only is sticky for the connection (a later hello can't lift it): every mutating JSON-RPC method on it (the catalog's `mutating` flag; unknown methods count as mutating) is refused with `permission_denied {reason: readonly}` before dispatch, without side effects, on the plain JSON-RPC lines as well as on the render stream. After `render.attach` the server answers that client's key, paste, mouse, sync and browser input with `InputAck Rejected`, refuses its mutating commands the same way and never makes it the geometry leader (not at attach, not on focus, not on `ViewHint`), so an observer can't resize another client's PTYs; a read-only attach shows the local machine only. Tests: `run::auth_tests::readonly_hello_refuses_every_mutation_without_side_effects`, `readonly_attach_never_leads_geometry`. `vibeke events tail [--types t,…] [--after-seq N | --lines N] [--follow]` prints one event per line (the last N events, or everything after N), and with `--follow` subscribes and resubscribes after an overflow. `vibeke api call <method> [json]` sends a raw request. `vibeke completion bash|zsh|fish|nu|powershell` prints a completion script generated from the command table (nouns and verbs). `vibeke session …`, `config get|set|reload`, `server restart [--binary]`, `blob get|stat`, `pane move|scroll|screenshot` and `task park|resume` map to the methods above. Not yet: `channel`, `debug bundle|holders|replay`, `doctor --fix`, `config edit|reset-keys`, `image`, `pane sync-input`.
- *As built (v1 remainder, server lane; overlaps the note above, tidied later):* *As built (Batch 2A, 2026-10-06).* `vibeke attach --readonly` says `client.hello {readonly: true}` on the TUI's connection before `render.attach`; the server then answers that client's key, paste, mouse and browser input with `InputAck Rejected`, refuses its mutating commands (`permission_denied {reason: readonly}`) and never lets it lead the pane geometry; a read-only attach shows the local machine only. `vibeke events tail [--types t,…] [--after-seq N | --lines N] [--follow]` prints one event per line (the last N events, or everything after N), and with `--follow` subscribes and resubscribes after an overflow. `vibeke api call <method> [json]` sends a raw request. `vibeke completion bash|zsh|fish|nu|powershell` prints a completion script generated from the command table (nouns and verbs). `vibeke session …`, `config get|set|reload`, `server restart [--binary]`, `blob get|stat`, `pane move|scroll|screenshot` and `task park|resume` map to the methods above. `vibeke config edit [--no-reopen]` opens `$VISUAL`/`$EDITOR`/`vi` on `config.toml` (created from the commented default), validates the saved file and on a terminal offers to reopen on errors (exit 1 when left invalid); `vibeke config reset-keys [--all] [--yes] [--dry-run]` drops every `[keys]` setting (`[[keys.command]]` kept unless `--all`) after saving `config.toml.<unix>.bak`. `vibeke shell-integration zsh|bash|fish` prints the OSC 133/7 snippets (03 §8). Not yet: `channel`, `debug holders|replay`, `doctor --fix`, `image`.

### 5.4 Examples (normative behavior)

```bash
# split right, keep focus, start a reviewer, prompt and wait for a settled state
P=$(vibeke pane split --current --direction right --cwd "$PWD" --no-focus | jq -r .pane.handle)
vibeke agent start reviewer --harness codex --pane "$P"
vibeke agent prompt reviewer "Review the diff; report actionable findings only." --wait --timeout 600000

# one-shot equivalent
vibeke agent spawn reviewer --harness codex --split-of @current --prompt "…" --wait

# task with isolated worktree, two agents, ports allocated
vibeke task new "fix login redirect" --repo . --agent claude:impl --agent codex:review

# answer the oldest open approval from a script (refused if run from that agent's own pane)
vibeke ask list --json | jq -r '.interactions[0].handle' | xargs -I{} vibeke ask answer {} --allow

# remote preview: screenshot the dev server the agent started on devbox
vibeke --machine devbox preview list
vibeke --machine devbox browser screenshot v4 --device iphone-15 --out shot.png

# events for scripting
vibeke events tail --types 'agent.state_changed' --follow | jq -c '.data'
```

### 5.5 Output conventions

- JSON output = the API result object verbatim (no wrapping), so `jq` paths in docs match API docs.
- Pretty output: tables with handles first; agent states colored and suffixed with `~` when `source != adapter` (inferred).
- Long-running commands (`wait`, `events tail --follow`) print one JSON object per line.
- Errors on stderr; never mix human text into stdout in JSON mode.
- `--dry-run` supported on every mutating command that touches the filesystem (`task new`, `worktree remove`, `integration install`, `import herdr`, `plugin install`). *As built:* all five, plus `forget`; `task new` and `worktree remove` send `dry_run: true` and the server returns the plan without touching anything (§2.10).

---

## 6. Embedded agent skill (`vibeke --skill`)

Printed from the binary (no network), versioned with the binary, also installable as a Claude Code / pi / omp skill by `vibeke integration install <harness> --skill`. Structure (normative outline; final text lives in `crates/vibeke/skill/SKILL.md`):

1. **Frontmatter**: name `vibeke`, description scoped narrowly — use only when the user mentions Vibeke or asks to coordinate panes/agents/tasks/previews; requires `VIBEKE=1`.
2. **Guard**: `test "${VIBEKE:-}" = 1` else stop. Prefer `--current`/`@current`. Never target `@focused`.
3. **Discover**: `vibeke --help`, `vibeke <noun>` for group help, `vibeke api methods`. Parse JSON results; never predict handles.
4. **Topology vs. agents**: pane = terminal, agent = recognized occupant, with the Vibeke state machine (`needs_approval`, `needs_answer`, `done`, `rate_limited`, inferred `~` states).
5. **Delegate in isolation (new)**: prefer `vibeke task new` over a sibling pane in the same cwd when the delegated work edits files. Explain collisions and `vibeke task get <k> --collisions`.
6. **Coordinate agents**: `agent spawn`, `agent prompt --wait`, `agent wait --until needs_answer`, `agent transcript` for structured output (instead of scraping alt-screen output and the "write your answer to a file" workaround, which remains as the fallback for screen-only harnesses).
7. **Interactions (new)**: you can *see* other agents' open interactions (`vibeke ask list`) but you **cannot answer your own**, and you should not answer other agents' approvals unless the user explicitly delegated that; prefer summarizing them for the user.
8. **Previews and screenshots (new)**: after starting a dev server, run `vibeke preview list --pane @current` (or `preview declare --port N`), give the user `vibeke preview url <v>` (the human opens it with `vibeke preview open`, which routes their browser profile to this machine's `localhost`); verify UI work by driving a session (`vibeke browser open <v>` → `click`/`type` → `screenshot --out /tmp/x.png`) and then read the PNG as an image — the screenshot records the commit it shows; check `vibeke browser console <v> --level error`. Works identically on remote machines — the screenshot is taken next to the server.
9. **Images to the user**: `vibeke image show <path>` displays inline in the user's TUI.
10. **Ordinary commands**: `pane split` + `pane run --wait` (OSC 133 aware) + `pane read --source recent_unwrapped`.
11. **Search**: `vibeke search` across scrollback and transcripts instead of asking the user to scroll.
12. **Safety rules**: don't close what you didn't create; don't `server stop`; don't `--force`; don't install plugins/integrations or change policy without explicit user instruction; JSON errors on stderr with exit 1, usage errors exit 2, timeouts exit 3.

The skill is tested: CI runs a scripted agent (Claude Code headless) through the skill's examples against a test session and asserts the resulting events (10 §4.6).

---

## 7. Plugin system [M5]

**Compatibility requirement:** existing Herdr plugins for the supported baseline (§8.0) must install and run without changes to their manifests, source, commands or callback protocol. Full plugin compatibility includes the public Herdr CLI/socket surface those plugins can call, not just manifest parsing or a handful of methods. This is an M5 delivery requirement, not a claim about the current implementation. Vibeke process plugins, native UI contributions and KV storage are additional facilities; imported plugins do not have to adopt them.

### 7.1 Two plugin kinds, one manifest

`vibeke-plugin.toml` at the plugin root (Herdr's `herdr-plugin.toml` is also accepted — §7.6):

```toml
id = "demo.phone-bridge"        # reverse-DNS-ish, unique
name = "Phone bridge"
version = "0.3.0"
min_vibeke = "1.0.0"
platforms = ["linux", "macos"]
description = "…"
license = "MIT"

# Kind A: argv actions (Herdr-compatible)
[[build]]
command = ["bun", "run", "build"]

[[actions]]
id = "status"
title = "Show status"
contexts = ["workspace", "pane"]       # where it appears in the palette / context menu
command = ["bash", "scripts/status.sh"]
keybinding = "prefix+alt+s"            # optional default; user config overrides

# Kind B: long-running process
[process]
command = ["bun", "run", "dist/main.js"]
restart = "on-failure"                 # never | on-failure | always
autostart = true

[capabilities]                         # requested; user approves at install (09 §6)
events_read   = ["agent.*", "interaction.*", "pane.created"]
panes_read    = true                   # pane.read / search
panes_write   = false                  # send_text/keys/run
agents_control = false                 # agent.start/prompt/interrupt
interactions_answer = false            # high risk; shown in red at install
tasks_write   = false
preview_access = false
network       = ["api.github.com"]     # advisory in M5 (declared, audited); enforced with sandboxing in M6
filesystem    = ["$PLUGIN_DATA"]       # advisory in M5
ui            = ["sidebar_section", "status_segment", "pane", "palette", "keybindings"]
storage       = true
```

### 7.2 Argv actions (Kind A)

- Executed with argv (no shell), cwd = plugin dir, env: `VIBEKE=1`, `VIBEKE_SOCKET`, `VIBEKE_PLUGIN_ID`, `VIBEKE_PLUGIN_TOKEN` (capability-scoped token), `VIBEKE_PLUGIN_DATA_DIR`, `VIBEKE_PLUGIN_CONFIG_DIR`, `VIBEKE_CONTEXT_WORKSPACE`, `VIBEKE_CONTEXT_PANE`. Imported Herdr plugins receive the complete environment and compatibility launcher in §7.7.
- stdout/stderr captured to the plugin log; last 4 KiB returned by `plugin.action`; non-zero exit raises a notification.
- Native event hooks: `[[on]] event = "worktree.created" command = [...]` — the event JSON is passed on stdin. Herdr's `[[events]] on = "worktree.created"` uses its original payload/environment contract (§7.7); importing it must not silently substitute the native hook ABI.

### 7.3 Process plugins (Kind B)

- Spawned by the server; speaks the **same JSON-RPC API** over its stdin/stdout (or connects to `VIBEKE_SOCKET` with its token — both supported; stdio preferred for lifecycle coupling).
- Token-scoped: every call is checked against approved capabilities; violations → `permission_denied` and a `plugin.capability_violation` audit event.
- Lifecycle: `plugin.initialize {config, api}` → plugin replies with `{contributions}`; `plugin.shutdown` with 5 s grace. Crash → exponential backoff restart (max 5 / 10 min), then disabled with a notification.
- Resource limits [M6]: CPU/memory via `setrlimit`, optional macOS sandbox profile / Linux Landlock for filesystem+network enforcement.

### 7.4 UI contributions

Declared at init or later via `ui.contribute`; all are data, rendered by the TUI (no plugin-drawn terminal escape codes in chrome):

| Contribution | Shape | Rendering |
|---|---|---|
| `sidebar_section` | `{id, title, items: [{id, label, badge?, state_color?, on_select: action}] , order}` | collapsible section under workspaces |
| `status_segment` | `{id, text, color?, side: left|right, priority, on_click?}` | status bar (tmux-style) |
| `pane` | `{id, title, command: argv}` | plugin-provided terminal pane (overlay/popup/split/tab/zoomed) |
| `palette_command` | `{id, title, contexts, action}` | command palette entry |
| `keybinding` | `{key, action}` | default binding; user config wins; conflicts reported by `vibeke doctor` |
| `pane_decoration` | `{pane, badge?, border_color?, title_suffix?}` | small per-pane decorations (e.g. CI status) |
| `link_handler` | `{scheme|regex, action}` | handle clicks on matching links |
| `harness` | path to a harness manifest | lets plugins ship harness support (04) |

Updates are debounced to 10 Hz per plugin.

### 7.5 Storage

`plugin.kv.*`: per-plugin namespace in `state.db` (`plugin_kv`), values ≤ 1 MiB, total quota 64 MiB per plugin (configurable). Plus a private data dir `~/.local/state/vibeke/plugins/<id>/`.

### 7.6 Install, marketplace, dev loop

- `vibeke plugin install owner/repo[@ref]`: clone (shallow) → read manifest → show capabilities diff → confirm → run `[[build]]` → enable. Updates show capability *changes* and require re-consent if they widen.
- Marketplace index **[post-1.0; design kept]**: M5 ships install-by-repo (`vibeke plugin install owner/repo`) and `plugin link`; the searchable marketplace follows after 1.0. A static JSON index built daily by a GitHub Action from repos tagged topic **`vibeke-plugin`** (and, read-only, `herdr-plugin` repos that pass the compat checker), published to `plugins.vibeke.dev/index.json`; `vibeke plugin search <q>` reads it. No server-side code execution; the index stores repo, ref, manifest summary, capabilities and stars.
- Native dev loop: `vibeke plugin link <path>` → registers in dev mode; the server watches the manifest and the process command's files (configurable globs) and hot-restarts the plugin process on change; argv actions are re-read each invocation. `vibeke plugin logs <id> -f`. Herdr links retain upstream reload/startup semantics; native hot restart is an explicit option and never reruns a Herdr startup hook on an ordinary file change.
- Herdr plugin import: retain the original `herdr-plugin.toml` and source tree and implement §7.7 in full. Do not translate away fields, require new capability declarations, or patch plugin code. Migration copies config/state into Vibeke-owned locations after consent; the running Herdr installation remains untouched.

### 7.7 Full Herdr plugin contract

The baseline is pinned in §8.0. Its manifest schema, CLI implementation and API schema are authoritative when prose examples differ. Platform support is macOS/Linux in M5 and Windows in M6; external runtimes and tools required by a plugin must be installed on its execution machine.

**Manifest and installation**

- Accept all baseline metadata and every field of `[[build]]`, `[[startup]]`, `[[actions]]`, `[[events]]`, `[[panes]]` and `[[link_handlers]]`. Match identifier validation, qualified action resolution, defaults, contexts (`global`, `workspace`, `tab`, `pane`, `selection`), regex handling, warnings and platform inheritance/overrides. Evaluate `min_herdr_version` against the tested Herdr baseline, never Vibeke's version. A missing implementation for a valid baseline field is a release blocker; a manifest requiring a newer baseline receives a clear version error.
- Support `owner/repo[/subdir]`, `--ref`, `--yes`, local directories and direct manifest paths with Herdr's CLI grammar. A build runs only for an install, after source/build/trust review; linking does not build. Abort registration on build failure or manifest mutation during the build. Preserve origin, requested ref, resolved commit and managed checkout metadata.
- Keep registrations and enabled state **per user, shared across that machine's sessions**, including installs/links while no server is running. Store them atomically under `~/.config/vibeke/plugins.json`; running sessions observe committed registry changes. Each session maintains its own runtime, focus context, startup invocations and command logs. Reload manifests with upstream warning/error behavior; missing files remain diagnosable through `plugin.list`.
- Match reinstall, enable/disable, unlink and uninstall behavior: a local link cannot be silently replaced by a managed install; unlink preserves files; uninstall removes only the managed checkout. Preserve plugin-owned config/state. Import never runs code or overwrites the source Herdr registry implicitly.

**Commands, context and callbacks**

- Preserve argv boundaries, cwd, inherited user environment, platform command resolution and build/runtime environment separation. Build steps do not inherit runtime socket credentials, pane context or plugin authority. No implicit shell, dependency installation, or language restriction.
- Supply `HERDR_ENV=1`, `HERDR_SOCKET_PATH`, `HERDR_BIN_PATH`, `HERDR_PLUGIN_ID`, `HERDR_PLUGIN_ROOT`, `HERDR_PLUGIN_CONFIG_DIR`, `HERDR_PLUGIN_STATE_DIR` and `HERDR_PLUGIN_CONTEXT_JSON`; supply `HERDR_WORKSPACE_ID`, `HERDR_TAB_ID`, `HERDR_PANE_ID` only in the contexts where upstream supplies them. Per-entrypoint values include `HERDR_PLUGIN_ACTION_ID`, `HERDR_PLUGIN_EVENT`, `HERDR_PLUGIN_EVENT_JSON`, `HERDR_PLUGIN_ENTRYPOINT_ID`, `HERDR_PLUGIN_CLICKED_URL` and `HERDR_PLUGIN_LINK_HANDLER_ID`. Clear stale inherited context variables before populating the invocation.
- Match the complete `PluginInvocationContext` schema: workspace/tab identity and labels, working directories, worktree provenance, focused pane/agent/status, selection, invocation source, correlation id, clicked URL and handler id. Preserve omitted-versus-null behavior and Herdr ids throughout. Fill missing context from the correct session/client as upstream does.
- `HERDR_BIN_PATH` is an absolute path to Vibeke's Herdr-compatible launcher. Prepend a **private invocation PATH directory** containing `herdr` (and the Windows equivalent in M6), so both this variable and bare `herdr` commands reach the same Vibeke session even with real Herdr installed. Do not replace the user's global Herdr executable. Session selection follows Herdr's precedence; it never falls through to a live Herdr server. Remote plugins execute and resolve paths on the target machine.
- Raw socket callbacks use unchanged Herdr JSON and require no Vibeke-specific `client.hello` or token fields. A broker bound to the invocation's approved identity supplies authorization outside that wire format (§8.3, 09 §6). Both CLI and direct socket calls must work, including callbacks from plugin-owned panes and long-lived children.
- Provide persistent config/state directories outside replaceable source checkouts. Existing plugin file formats and databases remain plugin-owned; KV storage is optional. Report the actual paths through the environment and `plugin config-dir`; migration is copy-based, reports conflicts and supports rollback to Herdr's untouched data.

**Lifecycle and UI behavior**

- Preserve asynchronous action invocation: return the initial command log and context promptly, then expose running/completed/failed state, timestamps, exit status and separate stdout/stderr through `plugin.log.list`. A successful launch is not a successful action. Native `plugin.action` may wait, but the compat method must not change upstream timing or response shape.
- Run `[[startup]]` once per enabled plugin per server activation after session restore and API readiness, including takeover/restart. Do not trigger it on attach, link, enable, config reload or a development file change. Record failures without taking down the server; never reinterpret these hooks as supervised process plugins.
- Deliver `[[events]]` using the baseline event names, payloads, invocation context, dispatch and logging behavior. Match action qualification, disabled/missing-plugin errors, command concurrency/log limits and spawn/exit failures. Reproduce restart/reload behavior rather than silently replaying hooks from Vibeke's durable outbox.
- Implement `plugin.pane.open/focus/close` for every placement: `overlay`, `popup`, `split`, `tab`, `zoomed`, including all targeting, environment, focus, cwd, direction and size parameters. Preserve plugin ownership through pane moves/swaps and restore prior focus/zoom when an overlay closes.
- Popups remain session-modal resources: no pane id, pane/agent enumeration, persistence or pane lifecycle events; no `HERDR_PANE_ID` in their process environment. Preserve their underlying focus context, dimension rules, input delivery, busy/error responses and `popup.close` semantics. Do not normalize them into ordinary panes.
- Preserve `[[keys.command]] type = "plugin_action"` bindings and action contexts. Link handlers use the baseline matching order, modifier, regex and action resolution and receive the original URL/handler context. Public UI APIs called by plugins, including window-title overrides, layouts and agent-view projections, are required in §8.3.

**Trust and revocation**

Herdr plugins have no capability declaration and expect user-level host access. Import therefore requires an explicit **`herdr_legacy` trust grant**, showing source/commit and build/runtime entrypoints and explaining the broad Herdr API, filesystem, environment and network access. This preserves upstream behavior without pretending that a scoped token contains arbitrary host code. Native plugins retain capability-scoped defaults; stricter execution of a legacy plugin is opt-in and labeled restricted, not fully compatible. The unchanged manifest needs no added fields. Consent is stored in Vibeke's registry, outside the plugin source. In the compatibility CLI, `--yes` accepts the displayed legacy trust terms when the caller already has installation authority; it never elevates a restricted caller. Raw link/install calls require an existing matching grant or an explicit authorized trust decision before activation.

Bind all callback paths to that grant; disable/unlink/uninstall or trust revocation disables future execution and revokes its broker access. Legacy callback authority lasts for the authorized invocation/process lifetime, including plugin panes and server recovery, rather than expiring after the native 60-second action token window. A pane-scoped agent or restricted plugin cannot gain broader authority by invoking a trusted legacy plugin: require an authorized operator invocation or enforce the caller's narrower scope (09 §6).

**As built (M5 first slice, 2026-10-06; partial).** Item-by-item status is in [docs/herdr-compat-inventory.md](../docs/herdr-compat-inventory.md), generated from `vk-compat/src/herdr/inventory.rs`.

- *Manifests* (`vk_compat::herdr::manifest`): every field the 99 real manifests use is parsed and validated (`id name version min_herdr_version platforms description`, `[[build]]`, `[[startup]]`, `[[actions]]`, `[[events]]`, `[[panes]]`, `[[link_handlers]]`, `[[keys.command]]`), with per-platform twins sharing an id, entry platforms overriding the plugin's, qualified action resolution, compiled link regexes, unknown-key/unknown-event warnings and `min_herdr_version` checked against 0.9.3. Fixtures: the 99 manifests listed in the plugin catalog, copied verbatim to `tests/compat/herdr/0.9.3/manifests/` (source record in its `index.json`) and checked against the record's SHA-256, ids, actions and events. *Unverified* against the baseline schema: identifier character sets, the default `contexts` (`global`) and the default placement (`overlay`).
- *Registry and trust* (`vk_compat::herdr::registry`): `plugins.json` next to `config.toml`, atomic and shared by all sessions, usable with no server. `vibeke plugin install <dir|manifest>` copies into a managed checkout under `$STATE/plugins/checkouts/<id>/<commit|local>-<nonce>/` (see "Immutable checkouts" below); `plugin link` registers in place and never builds. Every plugin is **inactive until `vibeke plugin trust <id> --legacy`** (or `install --yes`), which shows source, manifest SHA-256 and every entrypoint (build, startup, actions with contexts, event hooks, panes, link handlers). The grant is bound to the manifest digest and root; any manifest change sets `stale_trust` until re-reviewed. The build runs once after the grant, with no socket, launcher, context or pane identity; a failed build revokes the grant (and aborts an `install --yes`). Unlink keeps files, uninstall removes only the managed checkout, and plugin config/state live outside it. Pane-scoped callers cannot change the registry or grant trust; a plugin invocation cannot grant trust.
- *Invocation* (`vk_compat::herdr::launch`, `vk-server/src/compat.rs`): argv only, cwd = plugin root, relative `command[0]` resolved against the root, inherited environment with stale `HERDR_*`/pane credentials removed, then `HERDR_ENV`, `HERDR_SOCKET_PATH` (private broker), `HERDR_BIN_PATH` (private launcher `$RUNTIME/<session>/herdr-compat/bin/herdr`, prepended to `PATH`), `HERDR_PLUGIN_ID/ROOT/CONFIG_DIR/STATE_DIR/CONTEXT_JSON`, the context's `HERDR_WORKSPACE_ID/TAB_ID/PANE_ID` and `HERDR_PLUGIN_ACTION_ID/EVENT/EVENT_JSON/ENTRYPOINT_ID`. The context JSON is a subset of `PluginInvocationContext` (source, correlation id, ids, plugin version).
- *Lifecycle*: actions run asynchronously with log records (`running` → `completed`/`failed`, timestamps, exit code, separate 64 KiB stdout/stderr tails, 100 records per server; on disk each invocation's output is bounded by `output_max_bytes`, see "Concurrency and log limits"); `[[events]]` hooks fire on projected Herdr events; `[[startup]]` runs once per server activation for active plugins, never on attach/link/enable/reload.
- *Slice 2 (2026-10-06)*: plugin panes (`split`, `tab`, `zoomed`, `overlay`; see §2.16); broker re-issue after a server restart and the long-running rule (`[[startup]]` brokers follow the startup process's group, plugin-pane brokers the pane, action/hook brokers close when their process exits; 09 §6); output written by a per-invocation capture helper (a separate process holding the plugin's stdout/stderr pipes) into 0600 files and tailed from them, so it survives a restart, redacted with `vk-redact`; log records persisted per session; metadata-only audit events (`plugin.invocation_started/finished`, `plugin.api_call`, `plugin.pane_opened`, `actor.kind = plugin`); and copy-only migration of Herdr plugin config/state (`vibeke plugin migrate --from <dir> [--plugin id]... [--dry-run] [--link]`, `--rollback`). The migration source is only the directory the user names and is never modified; existing destination files are conflicts and are left alone. Herdr's per-plugin directory layout is unverified, so the planner accepts `plugins/<id>/{config,state}`, `plugins/{config,state}/<id>`, `plugin-{config,state}/<id>` and explicit `config_dir`/`state_dir` registry fields.
- *Slice 3 (TUI, 2026-10-06)*: popups and real overlays, palette entries, `[[keys.command]] type = "plugin_action"` bindings, link handlers, the window title and `pane.scroll_changed` (§2.16, §3.2, 08 §5/§6.3/§6.8/§10.3).
- *Plugin completion (as built, 2026-10-06)*:
  - **Repository sources.** `vibeke plugin install owner/repo[/subdir][@ref] [--ref R] [--yes] [--dry-run]` (also `herdr plugin install …`) fetches `https://github.com/owner/repo` with a hardened shallow git (`vk_compat::herdr::source`): a fresh repository without template hooks, one `fetch --depth 1` of the tag, branch or commit sha (the remote `HEAD` when none; `@ref` and `--ref` must agree), a detached checkout of exactly the resolved commit, no submodules, `core.hooksPath=/dev/null`, `core.fsmonitor=false`, no credential helpers or prompts, the user's and the system's git configuration ignored, protocols limited to https and file (`protocol.allow=never` plus an allowlist, `GIT_ALLOW_PROTOCOL=https:file`), 180 s per step. Options and refs cannot be smuggled in (`-`-prefixed names and refs, `..`, a subdirectory leaving the repository are refused). The registry records `origin {kind: git, path: <url>[/subdir], repo, requested_ref, commit}` and the managed checkout is the tree without `.git`. The `herdr_legacy` grant pins the manifest digest, the tree digest **and the resolved commit**; `vibeke plugin update <id>` (or installing again) re-fetches the recorded repository and ref, and a different commit, manifest or tree leaves the plugin inactive until `plugin trust` reviews it again (the trust terms show the commit); the same commit with an identical tree and no `[[build]]` keeps the grant. **Immutable checkouts** (`reviews/2026-10-06-codex-leftovers-review.md` finding 4): an install or update copies and verifies the new tree in a staging area outside the registry lock, moves it to a new path `$STATE/plugins/checkouts/<id>/<commit12|local>-<nonce>/` that nothing references yet, and publishes it by saving the registry once under the lock (new root, origin/commit and the kept or dropped grant together). The checkout the registry currently names is never modified, so a crash or a failed save before that write leaves the old reviewed checkout in use, and an invocation that resolved the old entry just before the switch runs exactly the old reviewed files. Launchers read the registry under its shared lock, so they never see a half-made publish. Managed checkouts are read-only on disk (owner write bits removed) except while their `[[build]]` runs, so a plugin's own bytecode or log files cannot change the reviewed tree by accident; replaced checkouts are removed by a later install once unreferenced for 24 h, uninstall removes all of them, and the old flat `checkouts/<id>/` layout is moved aside on the next install. Every launch of a managed checkout re-verifies the **whole-tree digest** against the grant (the reviewed tree, re-recorded after a successful build), cached by a stat fingerprint of the tree (path, mode, size, inode, mtime and ctime of every entry), so a dependency changed in place (an unchanged `main.py` importing a changed `helper.py`) is refused with "changed since it was reviewed" although the manifest and entrypoint digests still match. Linked development directories are reviewed in place and pinned by the manifest and referenced-file digests only. **Symlinks** (finding 10): an install refuses any symlink in the tree that is absolute, climbs above the plugin root or resolves outside it; an entrypoint (or a directory along its path) that resolves outside the plugin root is refused by `plugin trust` and makes the status `broken`; an internal symlink entrypoint is pinned by its link text *and* the contents of the file it resolves to, so changing the target makes the grant stale. The URL base can be redirected only when `VIBEKE_TEST_HOOKS=1` and `VIBEKE_PLUGIN_GIT_BASE` is set (tests use `file://` bare repositories; nothing in the tests touches the network). The `--ref` flag spelling and the subdirectory grammar are unverified against the baseline CLI.
  - **Manifest default key bindings.** `[[keys.command]] type = "plugin_action"` entries of a plugin that is trusted and enabled are installed; the server (`plugin.list` per plugin and `plugin.action.list.keybindings`) reports each as `{plugin_id, key, action: "<plugin>.<action>", description, installed, reason?, conflicts_with?}`. A binding that duplicates or shadows (or is shadowed by) any key of the user's keymap, the built-in defaults or an earlier plugin's binding (`vk_config::binding_clash`) is skipped with `reason: conflict`; an unparsable key is `invalid_key`; an inactive plugin's are `untrusted`/`stale_trust`/`disabled`. User keys always win. The TUI adds the installed bindings to its keymap as `plugin:<machine>:<plugin>.<action>` whenever the list is refreshed (connect, palette, config reload, `plugin.registry_changed`) and drops them with the plugin. The CLI tells a running server about registry changes (`plugin.registry.notify`, best effort, never starting a server), which emits `plugin.registry_changed`.
  - **Action contexts.** `contexts` (`global`, `workspace`, `tab`, `pane`, `selection`; an action with several holds if any applies) filter the command palette: `global` always, `workspace`/`tab`/`pane` need that focus on the machine the entry belongs to, `selection` needs a copy-mode selection (so the palette, which leaves copy mode, never offers it; a key binding fired in copy mode can). A key binding or stale palette id that fires where the action does not apply shows `<title>: not available here (needs <contexts>)` and runs nothing. Qualified contexts (pane kind, harness, task) are not built.
  - **Agent views.** `agent.view.set {target, text, detail?, tone?}` and `agent.view.clear {target?}` (CLI: `herdr agent view-set <target> --text … [--detail …] [--tone info|ok|warn|error]`, `herdr agent view-clear [--target T]`) attach one status line to an agent run. They need a plugin invocation's identity (broker); user-level and pane callers get `permission_denied`. `text` is sanitized (controls and bidi overrides removed, credentials redacted) and cut to 80 characters, `detail` to 512 bytes, a plugin holds at most 16 views (`limit_exceeded`). A view is bound to the grant it was set under: it is hidden and purged when the plugin is disabled, untrusted, unlinked, uninstalled, re-reviewed (new grant) or its run is gone, and re-enabling does not bring it back. `compat.ui.state` returns `agent_views`; `plugin.agent_view_changed` is pushed on every change. The TUI shows `▸ text` after the run's state in the sidebar and `▸ text (plugin)` plus the detail in the peek. Views are in memory (not persisted across a server restart). The method names come from the spec; the parameter and result shapes are Vibeke's own until the baseline schema is captured.
  - **Concurrency and log limits** (`[plugins]` / `[plugins."<id>"]` in `config.toml`; defaults in parentheses): `max_concurrent` (4) running action/hook invocations per plugin (`[[startup]]` processes are not counted): a further `plugin.action.run` or link/key/palette run is refused with `busy` and leaves no record; further `[[events]]` hooks wait in a queue of at most 16 per plugin and start as invocations finish, the overflow is recorded as a failed `busy` record. Output rings per stream: `log_max_bytes` (65536) and `log_max_lines` (1000); output that falls off the front is replaced by a first line `[vibeke: N earlier bytes of output truncated]` and `stdout_truncated_bytes`/`stderr_truncated_bytes` say how much; a burst larger than the cap is skipped without being read into memory. `log_max_records` (25) records are kept per plugin (running ones are never evicted) on top of the 100 per server. Values are clamped to sane ranges and the clamping is reported in `plugin.list` `settings_warnings`. `output_max_bytes` (8 MiB, 64 KiB–1 GiB): what one invocation may write to disk, both streams together. A plugin never receives a plain file as stdout/stderr: its streams are pipes read by a capture helper (`vibeke compat plugin-output`, its own process, so long-running invocations survive a server restart as before) that appends to the invocation's 0600 output files until the budget is spent, then writes one line `[vibeke: output budget of N bytes for this invocation exhausted; further output discarded]` per stream and keeps draining the pipes (the plugin is never blocked or signalled). A sustained writer therefore cannot fill the runtime volume (tested with a 30 MB writer against a 64 KiB budget). These are Vibeke's limits; the baseline's own (unverified) are not emulated.
  - **Restricted legacy mode.** `[plugins."<id>"] isolate = "sandbox"` (default `host`) runs the plugin's actions, event hooks and `[[startup]]` commands under the `sandbox` level (`vk_sandbox::plugin`, 13): the plugin directory and its config directory read-only, its state directory (and a private tmp/cache inside it) read-write, the system and the home allowlist readable, nothing else of the home, none of Vibeke's runtime, state or config (other plugins' state, other brokers, the broker registry `brokers.json` and the plugin registry stay hidden and the runtime dir cannot be listed; only the launcher directory and the `vibeke` binary are re-allowed read-only; inside the sandbox the shim accepts its own broker on the structural path checks alone, since the OS lets it connect to nothing else), no network unless `network = true` — which allows remote IP endpoints only (`(remote ip "*:*")` on macOS, no network namespace on Linux) and **keeps the Unix-socket allowlist**: the invocation's own broker plus, on macOS, the system resolver socket `/private/var/run/mDNSResponder` (there is no egress proxy for plugins) — and the scrubbed environment allowlist of 13 §8 (no `GITHUB_TOKEN`, cloud keys, `SSH_AUTH_SOCK`) plus the invocation's `HERDR_*` values. The `herdr` launcher and broker callbacks keep working inside it. The profile (Seatbelt SBPL, or the Linux bwrap policy) is written to a per-invocation file outside every plugin-writable path. Where no working sandbox exists (`sandbox-exec` missing or nested, `bwrap` missing or refused: probed by actually running a trivial command) the invocation fails with `restricted (sandboxed) plugin mode is not available here: … set isolate = "host"` — there is no fallback to host execution — and `plugin.list` reports `sandbox_available: false` with the reason. Plugin panes (`plugin.pane.open`) are `unsupported` for a sandboxed plugin, and the `[[build]]` step is unchanged (no authority). **Broker capability policy** (finding 1): a sandboxed invocation's broker serves only an allowlist — `ping`, `api.schema`, `session.snapshot`, `workspace.list/get`, `tab.list`, `pane.list/get/current`, `agent.list/get`, `layout.export`, `events.subscribe/wait`, `plugin.list`, `plugin.action.list`, `plugin.log.list` (its own records only), `plugin.action.invoke` of **its own** actions while they are still configured sandboxed, `agent.view.set/clear` (its own views), `notification.show` and `popup.close` (its own popup). Everything else is refused with `permission_denied` (audited as `plugin.api_call`): `pane.run`, `pane.send_text/keys/input`, `agent.send/prompt/start`, workspace/tab/pane creation and layout or metadata changes, `pane.read`, `agent.read`, `pane.wait_for_output`, `pane.process_info`, `worktree.*` (git on the host), registry methods, another plugin's actions and cross-session tickets (`vibeke.invocation_ticket`; a verified ticket also carries the restriction). Host-mode plugins are unchanged. **Broker identity** (finding 2): a broker serves a connection only when the connecting process belongs to the invocation — a descendant of its process, a member of its process group (`[[startup]]`) or a process of its pane, from the peer credentials — or when the request carries the invocation's secret `vibeke_token` (exported as `VIBEKE_HERDR_TOKEN`, sent automatically by the `herdr` launcher, for daemons that left the process tree). Knowing a socket path is not enough, on macOS and Linux alike (plugin panes use ancestry only, no token on their command line). Bindings persisted by older versions (no recorded isolation) are not re-issued after a restart. **Configuration fails closed** (finding 3): an `isolate` value other than `"host"`/`"sandbox"` (including a non-string) refuses the plugin with `isolate = … is not "host" or "sandbox"`; when `config.toml` cannot be read or parsed, or has disappeared, the last `[plugins]` section that loaded decides (kept in memory and persisted as `$STATE/plugins/last-good-settings.json`, outside every plugin-visible path) and a plugin it configured sandboxed is refused (`… was last configured with isolate = "sandbox" and is not run until the configuration loads again`), across server restarts too; with no known-good section a broken file refuses every plugin. `plugin.list` reports `settings_error`. Restricted mode is labeled not fully compatible: a plugin that needs the user's files, credentials or network will fail. Tested with the real macOS sandbox; the Linux chain is generated and unit-tested but unverified on a Linux host.

---

## 8. Herdr compatibility layer (`vk-compat`) [M5; importer in M1]

Goal: existing **Herdr plugins, CLI automation and socket clients run unmodified** against Vibeke's public compatibility surface. M5 covers the full public extension contract of the supported baseline, including the host APIs available to plugins. Import remains available in M1; the current SSH replacement goal does not acquire an M5 dependency.

**Status (2026-10-06): slices 1–3 built plus plugin completion, support partial.** The inventory ([docs/herdr-compat-inventory.md](../docs/herdr-compat-inventory.md)) lists 197 surfaces named by this section or used by the 100-plugin corpus: 94 implemented, 95 partial, 8 missing. Of the 74 socket methods, 1 remains missing: `server.stop` (refused by design); `agent.view.set/clear` are built in Vibeke's own shape (partial) because the spec names them without params and no corpus plugin calls them. It is not yet the exhaustive §8.0 inventory, because the baseline's schema, CLI help and manifest schema have not been captured from the binary. No Herdr binary was run, and no Herdr config, socket or session was touched.

### 8.0 Supported baseline and meaning of full compatibility

Initial required baseline: **Herdr v0.9.3**, tag resolved on 2026-10-06 to commit **`7b116c05bfda646af39d2524c54e70c751f57ee8`**. This pins a target for implementation; no current compatibility certification is implied. Reference material at that revision:

- [Plugin authoring contract](https://github.com/herdrdev/herdr/blob/7b116c05bfda646af39d2524c54e70c751f57ee8/docs/preview/website/src/content/docs/plugins.mdx), [plugin schema](https://github.com/herdrdev/herdr/blob/7b116c05bfda646af39d2524c54e70c751f57ee8/src/api/schema/plugins.rs), [plugin CLI implementation](https://github.com/herdrdev/herdr/blob/7b116c05bfda646af39d2524c54e70c751f57ee8/src/cli/plugin.rs).
- [Public CLI reference](https://github.com/herdrdev/herdr/blob/7b116c05bfda646af39d2524c54e70c751f57ee8/docs/preview/website/src/content/docs/cli-reference.mdx) and [socket API reference](https://github.com/herdrdev/herdr/blob/7b116c05bfda646af39d2524c54e70c751f57ee8/docs/preview/website/src/content/docs/socket-api.mdx). The compiled baseline's `herdr api schema --json`, CLI help and behavior are the final oracle; preview documentation can contain differences from the shipped binary.

At M5, check in `tests/compat/herdr/<version>/` containing the source revision, reference-binary checksums, full API schema, CLI grammar/help snapshots, manifest fixtures and a complete conformance inventory. Every public command, request, response, event, manifest field and platform branch must have an implementation mapping and tests. A single client trace or a selection of popular plugins is insufficient coverage. Valid baseline operations may not be stubbed, ignored, or rejected as `method_not_found` because Vibeke has not implemented them.

Compatibility preserves observable arguments/defaults, output and error shapes, exit codes, identifiers, lifecycle, focus/layout effects and timing contracts. Native API differences stay on the native endpoint. `herdr --version`, `api schema`, `ping` and snapshot metadata from the compatibility launcher/endpoint describe the tested emulation target consistently; `vibeke doctor` separately identifies Vibeke and reports the target and coverage status. Before the full gate passes, report partial support explicitly rather than advertising the baseline as fully supported.

**Boundary:** full compatibility here means the public plugin/automation contract. Implement public CLI operations using Vibeke's own runtime, including attachment and remote routing where exposed by the CLI. Running an original Herdr TUI binary against Vibeke, reproducing Herdr's private binary rendering/live-handoff transport, or sharing its internal session files in place is not required. Public UI-control methods and plugin-pane APIs are included. Private-transport exclusion must never be used to omit a public CLI operation. M5 certifies macOS/Linux; Windows adds the same contract in M6.

Record each supported version independently. New Herdr releases trigger schema/help/behavior diffs and a compatibility update; never widen the advertised baseline merely because a semver comparison passes. Retain fixtures for every advertised version and document explicit migration/deprecation before retiring one. `min_herdr_version` is a prerequisite check, not proof that an arbitrary historical or future plugin is compatible.

*As built:* `tests/compat/herdr/0.9.3/` holds `baseline.toml` (version, commit, reference links; binary checksums, API schema and CLI help are marked `pending`) and the 99 real manifests. The differential harness `crates/vibeke/tests/compat_herdr_diff.rs` runs both sides with fresh HOME/XDG/TMPDIR/runtime dirs, refuses a reference binary that does not report 0.9.3 (or mismatches `VIBEKE_HERDR_SHA256`), maps generated ids through a bijection and compares exit codes and JSON shapes. It runs only with `VIBEKE_HERDR_DIFF=1` and `VIBEKE_HERDR_BIN=/abs/path`, and has not been run. `ping`, `api.schema` and `herdr --version` report the emulated baseline and `partial`; `vibeke compat status` reports the target and coverage counts.

### 8.1 Importer [M1]

`vibeke import herdr [--dry-run]`:

| Herdr source | Vibeke target |
|---|---|
| `~/.config/herdr/config.toml` `[theme]` | `[theme]` (name map; `custom` tokens copied) |
| `[terminal] default_shell, shell_mode, new_cwd` | `[terminal]` same keys |
| `[keys]` (prefix, actions, `[[keys.command]]`, indexed) | `[keys]` — same binding grammar; unknown actions listed in the report |
| `[worktrees] directory` | `[tasks] worktree_root` |
| `[ui]` sidebar widths/collapsed | `[ui]` |
| `[update] channel` | ignored (reported) |
| `session.json` (v3): workspaces → tabs → layout → panes (cwd, `agent_session {source, agent, kind: id|path, value}`) | workspaces/tabs/layouts recreated; panes started with shells in their cwd; panes with an `agent_session` offered for resume via the harness resume argv (`claude --resume <id>`, `codex resume <id>`, `pi --session <path>`) |
| `~/.config/herdr/plugins.json`, `plugins/`, plugin config/state directories | M1: inventory only. M5: preserve manifests/source metadata, offer install/link with legacy trust consent, copy config/state with a conflict report; never modify Herdr's originals (§7.7) |
| Herdr integrations (`~/.claude/hooks/herdr-agent-state.sh`, `~/.codex/hooks.json` + `herdr-agent-state.sh`, pi extension) | detected; `vibeke integration install` installs Vibeke's alongside (they no-op outside Herdr because they check `HERDR_ENV`) |

Report printed as a table; nothing is deleted from `~/.config/herdr`.

### 8.2 Environment aliases and CLI launcher [M1 bootstrap; full contract M5]

With `compat.herdr_env = true` (default on when an import has been done), every pane also gets: `HERDR_ENV=1`, `HERDR_SOCKET_PATH=<compat endpoint>`, `HERDR_PANE_ID=<herdr-style id>`, `HERDR_WORKSPACE_ID`, `HERDR_TAB_ID`, `HERDR_BIN_PATH=<absolute compatibility launcher>`. M1 exposes the implemented bootstrap/reporting surface and reports its partial status; existing integration reports become `self_report` sources. M5 implements the entire baseline public CLI grammar and behavior, including plugin commands, global flags, session selection, JSON/human output and exit status. An explicit `vibeke compat install-shim` can expose the launcher to external automation. Plugin invocations always receive the private PATH launcher (§7.7), irrespective of a real Herdr binary elsewhere on PATH.

All compatibility commands operate on Vibeke-owned sessions/configuration/registrations, including lifecycle and integration management. They must never accidentally stop, upgrade, configure or install into a running Herdr instance. Explicit session selection overrides inherited socket context according to the baseline, and selects the corresponding Vibeke compatibility endpoint with the same caller authority.

### 8.3 Compat socket [M5]

- Path layout mirrors Herdr's so existing socket clients' session discovery works: default session `<herdr_root>/herdr.sock`, named sessions `<herdr_root>/sessions/<name>/herdr.sock`, where `<herdr_root>` defaults to `$RUNTIME/<session>/herdr-compat/` and is exported via `HERDR_SOCKET_PATH`. Users who run socket clients set `HERDR_SOCKET_PATH` or `compat.herdr_socket_path = "~/.config/herdr/herdr.sock"` when Herdr is no longer installed. The socket is removed on clean stop (clients use socket presence as liveness).
- Wire format exactly as Herdr: newline JSON `{"id": "<string>", "method": "...", "params": {...}}`; ordinary requests close after the response and `events.subscribe` streams, as verified against the baseline. Success uses `{"id", "result": {"type": "<result_type>", …}}`; errors use `{"id", "error": {"code": "<snake_case>", "message"}}`. Echo a recoverable string request id on errors; use an empty id only when it cannot be recovered. Match baseline framing, line limits, invalid UTF-8/JSON handling, integer-id rejection and connection closure. No native JSON-RPC envelope or mandatory `client.hello` leaks into this endpoint.
- Ids: Herdr-style `w<n>`, `<ws>:t<n>`, `<ws>:p<n>` handles come from a persisted mapping table with the baseline's allocation/move behavior. Internal Vibeke ULIDs never leak into compat fields.
- Project execution/attention/read state into the baseline's agent status schema and semantics, including wait conditions and occupant binding; structured adapters remain Vibeke's source of truth. Do not add native-only states or fields to compatibility responses. Unknown agent states and session end/resume behavior have differential fixtures.
- `agent_session` is derived from `AgentRun.harness_session_id` / `transcript_path` and retained/cleared in the compat projection according to the baseline lifecycle. Native cleanup rules must not silently change the Herdr response contract.
- `revision` is real (pane revision counter). `scroll` provided.

**Scope rule.** Implement every public method and event in the baseline schema, including methods absent from the table below. The checked-in inventory (§8.0) is exhaustive; these tables are implementation notes and cannot narrow it. `method_not_found` is reserved for methods unknown to the selected baseline. Authentication and authorization failures are explicit errors, never disguised as missing methods.

**As built (M5 slices 1–2).** `[compat.herdr] enabled = true` (off by default; the older `compat.herdr_socket` is honored too) binds Herdr's session layout under `<herdr_root>` = `$RUNTIME/herdr-compat` (shared by all sessions, 0700): the `default` session at `<herdr_root>/herdr.sock`, a named session at `<herdr_root>/sessions/<name>/herdr.sock` (0600). The path is never under Herdr's config directory; it is removed on SIGTERM/SIGINT and on `server.stop`. `compat.herdr_socket_path` is not built. Wire handling (`vk_compat::herdr::wire`): one request per connection; a string `id` is required; an integer id gets `invalid_request` with an empty id; a recoverable string id is echoed on errors; invalid UTF-8 or JSON gets `parse_error`; the line limit is 16 MiB (the baseline's limit is unverified). Caller identity: peer uid and the native pane-ancestry rule. Plugin invocations get a private broker socket bound to the plugin id and grant digest; the grant is re-checked on every request and on every streamed event, so untrust/disable/stale manifest cut a running action's callbacks. Brokers are persisted (`$RUNTIME/<session>/herdr-compat/brokers.json`) and re-issued at the same path after a restart for invocations that are still alive with a matching grant.

Methods are translated onto `api::dispatch`, so native authorization and events apply. Results are projected from the model with Herdr ids, which are Vibeke handles (`w<n>`, `w<n>:t<n>`, `w<n>:p<n>`), so ULIDs never leak. Focus fields and focus changes use the most recently active TUI client. Implemented or partial: `ping`, `api.schema`, `session.snapshot` (`layouts`: one layout snapshot per tab), `workspace.list/get/create/rename/move/focus/close`, `tab.list/create/rename/move/focus/close`, `pane.list/get/current/read/send_text (raw)/send_keys/send_input/run/focus/rename/close/split/wait_for_output/resize/zoom/report_agent/report_agent_session`, `agent.list/get/read/send`, `worktree.list/repo_root`, `notification.show`, `server.reload_config`, `events.subscribe/wait` and `plugin.list/action.list/action.invoke/log.list`. Slice 2 (`vk-server/src/compat/ext.rs`) adds `layout.export` (a binary split tree per tab: `{type: split, split_id, direction, ratio, first, second}` / `{type: pane, pane_id}`; Vibeke's n-ary splits nest as first-vs-rest), `layout.apply` (rearranges the tab's own panes from such a snapshot; every pane exactly once), `layout.set_split_ratio {split_id | pane_id, ratio}`, `pane.process_info` (pid, foreground argv/command, cwd), `pane.move {pane_id, tab_id | target_pane_id, direction?}` and `pane.swap {pane_id, other_pane_id}` within a workspace (cross-workspace moves are refused; ids are stable; both emit the native `pane.moved`), `pane.report_metadata`/`workspace.report_metadata` (merged, shown as `metadata` in pane/workspace records, in memory), `client.window_title.set/clear` (evented as `client.window_title_changed`; since slice 3 the TUI shows it as the outer terminal title, or in the tab bar without `ui.title_sync`), `agent.start` (Herdr agent name → harness; in `pane_id`, else a split of the target pane), `agent.prompt`, `agent.wait {status|until}` (Herdr statuses mapped to Vibeke wait conditions), `agent.rename`, `worktree.create {cwd, branch, base?, open = true, focus?}` and `worktree.open {path, focus?}` (on the new native `worktree.create/open`, 07 §2.10; `[tasks] root` decides the location), `plugin.link/unlink/enable/disable` and `plugin.pane.open/focus/close` (§2.16), and since slice 3 `popup.close` and popup/overlay placements (§2.16). Result types and parameter names not in the table above are unverified. `pane.wait_for_output` also emits `pane.output_matched`. Mutating calls made through a plugin broker are audited (`plugin.api_call`, method and outcome only). Every other inventory method returns `unsupported`, naming the inventory; `server.stop` is refused; unknown methods return `method_not_found`. A test keeps inventory status and dispatch in agreement.

Agent status (`vk_compat::herdr::status`): an open interaction maps to `blocked`; `working`/`starting`/`rate_limited` to `working`; idle with an unseen finished turn to `done`; idle to `idle`; anything else to `unknown`. The workspace takes its most urgent status. The baseline enumeration is unverified. Events (`vk_compat::herdr::events::Projector`): `workspace.created/renamed(+updated)/closed/moved`, `tab.created/closed/renamed`, `pane.created/closed/exited/focused`, derived `tab.focused`/`workspace.focused`, `pane.agent_detected`, `pane.agent_status_changed` (mapped-status changes only), `layout.updated` (from `tab.layout_changed`, carrying a `layout` snapshot), `worktree.removed`, and since slice 2 `tab.moved` (native `tab.moved` from `tab.move`), `pane.moved` (`from_tab_id`/`to_tab_id`), `pane.output_matched` (compat wait matchers) and `worktree.created`/`worktree.opened` (native events from `worktree.create`, `task.create` worktree checkouts and `worktree.open`, with `worktree {path, branch, repo_root}`), which reach both subscriptions and `[[events]]` hooks. The pane-scoped subscription rule is enforced. Since slice 3 `pane.scroll_changed` fires from TUI scroll reports (§3.2), and popups produce no pane events at all. Payloads carry Herdr ids plus the projected record; their exact baseline shape is unverified.

CLI shim (`vk_compat::herdr::cli`, `vk-cli/src/compat.rs`): `vibeke compat herdr <args>`, or the binary invoked as `herdr`. This is the private launcher, and `vibeke compat install-shim` links one into `$XDG_DATA_HOME/vibeke/compat/bin` on request; other directories are refused. Grammar: `herdr <noun> <verb> [positionals] [--flag value]` → `<noun>.<verb>`, covering the corpus's `plugin install|link|unlink|uninstall|enable|disable|list|config-dir|action list|action invoke|log list`, `pane …`, `workspace …`, `tab …`, `agent …`, `worktree …`, `notification show`, `server reload-config`, `api schema`, `ping` and `--version`. Results print as JSON on stdout. Errors print `{"error": {code, message}}` on stderr with exit 1 (5 for permission), and usage errors exit 2. Routing: the invocation's broker when `VIBEKE_HERDR_BROKER` equals `HERDR_SOCKET_PATH` and resolves (plain components, no symlinks) to a socket listed in its session's `brokers.json`, else this session's server via `compat.herdr.call`. A plain `HERDR_SOCKET_PATH` is never used, so the shim cannot reach a live Herdr; `server.*` lifecycle methods are never forwarded. `integration …`, `update`, `config` and `web` are refused with a reason. Flag spellings, positional order and output formatting are unverified against the baseline CLI. Session selection (slice 2): Herdr's global `--session NAME` (anywhere before `--`), then `HERDR_SESSION`, then the invocation's own session (its broker's, else `VIBEKE_SESSION` or vibeke's `--session`). `vibeke compat herdr …` passes everything after `herdr` to the shim unparsed. An explicitly selected session is reached through its native socket and never spawned; selecting another session from a pane is refused (exit 5); a plugin invocation keeps its identity on the destination through a single-use ticket from its own broker (`compat.herdr.call {as_plugin: {session, ticket}}`), which the destination redeems with the issuing session (`compat.invocation.verify`) before re-checking the grant; a process inside a pane of any session cannot switch sessions, with or without its token. Inside a plugin invocation whose broker has closed, the shim fails with `permission_denied` instead of falling back to the user-level socket.

The public compatibility listener derives caller identity from peer credentials and the same pane/process scope rules as the native endpoint, without requiring a new handshake. A plugin invocation gets a private broker endpoint, bound server-side to its approved grant, exposed through `HERDR_SOCKET_PATH`. Approved plugin brokers are available even when `compat.herdr_socket = false` disables the general external listener. The launcher propagates that identity for native calls and session routing; raw JSON clients need no changes. Broker creation, server recovery, revocation and scope enforcement are covered by 09 §6. A restricted caller cannot select a different session or omit a token to acquire a legacy grant.

Required coverage includes all server, notification, client, session, workspace, worktree, tab, pane, popup, layout, agent, event, integration and plugin methods. In particular, implement the surfaces omitted by the earlier subset-only design: `plugin.link/list/unlink/enable/disable`, `plugin.action.list/invoke`, `plugin.log.list`, `plugin.pane.open/focus/close`, `popup.close`, `layout.export/apply/set_split_ratio`, `pane.process_info/move/swap/resize/zoom`, `client.window_title.set/clear`, `agent.view.set/clear`, metadata reporting, and every remaining public schema entry. Full CLI coverage additionally includes operations performed client-side rather than by one matching socket method.

**Initial method mapping (non-exhaustive; validate shapes against §8.0):**

| Herdr method (params) | Herdr `result.type` | Vibeke implementation |
|---|---|---|
| `session.snapshot {}` | `session_snapshot` → `{version, protocol, workspaces, tabs, panes, agents, layouts, focused_workspace_id, focused_tab_id, focused_pane_id}` | `session.snapshot` projected to Herdr shapes; focus and version/protocol metadata follow the selected tested baseline (§8.0), not a hardcoded historical protocol number |
| `workspace.list {}` | `workspace_list` → `workspaces[] {workspace_id, number, label, focused, pane_count, tab_count, active_tab_id, agent_status}` | `workspace.list`; `agent_status` = most urgent among its runs |
| `workspace.create {cwd, focus:false, label?}` | `workspace_created` → `{workspace, tab, root_pane}` | `workspace.create` |
| `workspace.rename {workspace_id, label}` | `workspace_info` | `workspace.rename` (empty string stored literally, as Herdr) |
| `workspace.move {workspace_id, insert_index}` | `workspace_list` | `workspace.move` (renumbers, as Herdr) |
| `workspace.focus {workspace_id}` | `workspace_info` | `workspace.focus` |
| `tab.list {workspace_id?}` | `tab_list` | `tab.list` |
| `tab.create {workspace_id, focus:false, label?, cwd?}` | `tab_created` → `{tab, root_pane}` | `tab.create` |
| `tab.rename {tab_id, label}` | `tab_info` | `tab.rename` |
| `tab.move {tab_id, insert_index}` | `tab_list` (numbers stable) | `tab.move` |
| `tab.focus {tab_id}` | `tab_info` | `tab.focus` |
| `tab.close {tab_id}` | `ok` | `tab.close {force:true}` (Herdr semantics: closes all panes) |
| `pane.list {}` | `pane_list` → `panes[] {pane_id, terminal_id, workspace_id, tab_id, focused, cwd, foreground_cwd, agent, agent_status, agent_session, revision, scroll, label?}` | `pane.list` |
| `pane.get {pane_id}` / `pane.current {}` | `pane_info` | `pane.get` / `pane.current` |
| `pane.read {pane_id, source: visible|recent|recent_unwrapped|detection, lines, format: text|ansi}` | `pane_read` → `{read: {text, truncated, revision}}` | compat read projection preserves baseline wrapping, source/line handling and scroll semantics; native reads retain their own contract |
| `pane.send_text {pane_id, text}` | ack | `pane.send_text {paste: raw}` — raw bytes, no bracketed paste (Herdr semantics; existing clients depend on it) |
| `pane.send_keys {pane_id, keys}` | ack; `invalid_key` on unknown | `pane.send_keys` restricted to Herdr's accepted set in compat mode (so existing clients' validation probes behave identically) |
| `pane.send_input` | ack | `pane.send_text` + keys |
| `agent.send {target, text}` | ack (literal text, no Enter) | `pane.send_text` on the run's pane |
| `pane.focus {pane_id}` | `pane_info` (moves pane+tab+workspace) | `pane.focus` |
| `pane.rename {pane_id, label: string|null}` | `pane_info` (null clears; Herdr emits no event — we emit `pane.title_changed` natively, compat stream emits nothing to match) | `pane.rename` |
| `pane.close {pane_id}` | `ok` / `pane_not_found` | `pane.close {force:true}` |
| `pane.split {pane_id, direction, cwd?, focus?}` | `pane_info` | `pane.split` |
| `pane.wait_for_output` | `pane_output_matched` | `pane.wait_output` |
| `pane.report_agent` / `pane.report_agent_session` | ack | `agent.report` with `source: self_report` (Herdr integrations keep working) |
| `agent.list`, `agent.start`, `agent.prompt`, `agent.wait`, `agent.get`, `agent.read`, `agent.rename` | Herdr shapes | `agent.*` equivalents |
| `worktree.list {cwd}` | `worktree_list` → `worktrees[]` | `worktree.list` |
| `worktree.create {cwd, branch, focus:false}` | `worktree_created` | `worktree.create {open:true}` |
| `worktree.open {cwd, path, focus:false}` | `worktree_opened` | `worktree.open` |
| `worktree.repo_root {cwd}` | `worktree_repo_root` | `worktree.repo_root` |
| `events.subscribe {subscriptions: [{type, pane_id?}]}` | ack `subscription_started`, then `{"event": "<snake_case>", "data": {...}}` lines | internal subscription translated per table below; `pane.agent_status_changed`, `pane.scroll_changed`, `pane.output_matched` require `pane_id` (Herdr rule) |
| `events.wait` | event | `events.wait` |

Initial event translation (compat stream; snake_case `event` field, dot-form subscription `type`; extend to the entire baseline event schema, including loss/reconnect behavior):

| Herdr subscription type | Emitted from Vibeke event |
|---|---|
| `workspace.created/updated/renamed/closed/focused/moved` | `workspace.*` (payload carries full workspace record like Herdr) |
| `tab.created/closed/focused/renamed/moved` | `tab.*` |
| `pane.created/closed/focused/moved/exited` | `pane.*` |
| `pane.agent_detected` | `agent.detected` / `agent.started` (debounced 250 ms) |
| `pane.agent_status_changed` | `agent.state_changed` when the *mapped* Herdr status changes (no event for `needs_approval → needs_answer`, both `blocked`) |
| `pane.output_matched` | from registered `pane.wait_output` matchers |
| `pane.scroll_changed` | scroll position changes |
| `layout.updated` | `tab.layout_changed` (full `PaneLayoutSnapshot`) |
| `worktree.created/opened/removed` | `worktree.*` |

### 8.4 Conformance and release gate

Run the pinned Herdr binary and Vibeke in separate temporary homes/runtime directories and apply identical scripted operations. Compare CLI stdout/stderr/exit codes, request/response schemas, error behavior, event sequences, plugin context/env, files, process effects and rendered terminal interactions. Normalize only declared nondeterministic values (timestamps, generated ids via a bijection, temporary roots and process ids); never normalize missing fields, events, focus behavior or errors away. No test touches the operator's live Herdr sessions or plugin data.

M5 requires:

1. **Complete contract coverage:** every entry in the baseline inventory has passing positive/negative tests; no missing methods, skipped manifest fields or unsupported baseline operations on macOS/Linux. Validate against schema and observed binary behavior, including raw clients and the launcher.
2. **Unmodified plugins:** pin real plugin repo SHAs and execute their original manifests/source through install, link, actions, hooks, logs, terminal placements and removal. Include at least a layout/worktree workflow, event notification, link preview, stateful startup restoration, and a plugin that uses raw socket callbacks. Fixtures exercise any surface those examples miss. Stub only external services/credentials, not Herdr callbacks or plugin code.
3. **Lifecycle and storage:** offline install/link, shared registry across named sessions, enable/disable propagation, build failures, reinstall, missing manifests, restart/takeover, exactly-once-per-activation startup dispatch, config/state preservation, and migration/rollback with real Herdr still installed.
4. **Routing and authority:** both `HERDR_BIN_PATH` and bare `herdr` select Vibeke; explicit sessions and remote execution target the correct host/session; raw callbacks retain the approved identity; long-lived actions/panes survive normal server recovery; revoked/disabled grants reject callbacks; restricted callers cannot invoke a broader plugin to escalate.
5. **Socket-client regression:** replay recorded bridge traffic (`bridge/mux/herdr/fixture.ts`) and run a pinned socket-client smoke test (dashboard, reply, key input, tab creation, event stream) alongside the full suite.

Publish the baseline/platform matrix and failures with the build. Until all required entries pass, label support partial; a passing socket-client smoke test alone never establishes full compatibility. Windows argv/PATHEXT, paths, named pipes and popup behavior become the same release gate at M6. Test scheduling and milestone gates are specified in [10](10-quality-performance-testing.md).
