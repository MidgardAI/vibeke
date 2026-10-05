# 07 — API, CLI, plugins and Herdr compatibility

This section specifies every external interface of the server: the control API (JSON-RPC), the event subscription API, the render stream, the holder protocol, the CLI that mirrors the API, the embedded agent skill, the plugin system, and the Herdr compatibility layer. Types referenced here (`Pane`, `AgentRun`, `Interaction`, `Task`, `Preview`, event envelope, …) are defined in [02-data-model-and-event-log.md](02-data-model-and-event-log.md); process roles and transports in [01-architecture.md](01-architecture.md).

Milestone tags: **[M1]** local core, **[M2]** agents/harnesses, **[M3]** tasks + remote, **[M4]** preview fabric, **[M5]** plugins + compat + QUIC, **[M6]** hardening / Windows / 1.0.

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

Results are plain objects keyed by noun: `{"pane": {...}}`, `{"panes": [...]}`, `{"run": {...}, "interaction": {...}}`. Every mutating result includes `"seq"`: the event-log sequence number after the mutation, so a client can `events.subscribe {after_seq: seq}` with no race.

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

### 1.5 Versioning

- API string `vibeke/1`. Within a major: new methods, new optional params, new result fields, new event types only. Clients must ignore unknown fields and unknown event types.
- `api.schema` returns the full JSON Schema (generated from `vk-proto` via `schemars`); CI diffs it against the previous release and fails on breaking changes.
- Generated clients: `@vibeke/client` (TypeScript, published to npm), `vibeke-client` (Python), and the Rust `vk-proto` crate.

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
| `api.schema` | `{method?}` → `{schema}` |
| `api.methods` | `{}` → `{methods: [{name, milestone, capability, mutating}]}` |
| `server.status` | `{}` → `{pid, version, uptime_ms, session, panes, holders: {live, orphaned}, clients, event_seq, db_size, rss}` |
| `server.reload_config` | `{}` → `{changed: [keys], errors: []}` |
| `server.stop` | `{kill_panes: bool = false}` → `{}` — with `kill_panes:false`, holders keep running and the next server reattaches |
| `server.restart` | `{binary?: path}` → `{new_pid}` — exec's the new server; holders untouched (01 §1.2) |

### 2.2 `session.*` [M1]

| Method | Params → Result |
|---|---|
| `session.snapshot` | `{include?: [workspaces, tabs, panes, runs, interactions, tasks, previews, layouts, machines]}` → `{at_seq, workspaces[], groups[], tabs[], panes[], runs[], interactions[], tasks[], previews[], layouts[], focused: {client_id → {workspace, tab, pane}}}` |
| `session.list` | `{}` → `{sessions: [{name, running, pid?, socket}]}` (scans the runtime dir) |
| `session.create` | `{name}` → `{session}` (spawns a server) |
| `session.stop` | `{name, kill_panes?: false}` → `{}` |
| `session.rename` | `{name, new_name}` → `{session}` |

### 2.3 `machine.*` [M3]

| Method | Params → Result |
|---|---|
| `machine.list` | `{}` → `{machines: [Machine]}` |
| `machine.add` | `{label, address: "user@host[:port]", transport?: ssh|quic, ssh_opts?: [..], install?: auto|ask|never}` → `{machine}` |
| `machine.connect` / `machine.disconnect` | `{machine}` → `{machine}` |
| `machine.remove` | `{machine}` → `{}` |
| `machine.status` | `{machine}` → `{machine, rtt_ms, bandwidth_kbps, remote_version, sessions[]}` |
| `machine.install` | `{machine, channel?}` → `{version}` — install/upgrade the remote binary (confirmation required unless `--yes`) |

### 2.4 `group.*`, `workspace.*` [M1]

| Method | Params → Result |
|---|---|
| `group.create` | `{name, parent?}` → `{group}` |
| `group.rename` / `group.move` / `group.delete` / `group.collapse` | `{group, …}` → `{group}` |
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
| `tab.move` | `{tab, index, workspace?}` → `{tabs}` (numbers are stable; order changes) |
| `tab.focus` | `{tab, client?}` → `{tab}` |
| `tab.close` | `{tab, force?}` → `{}` |

### 2.6 `pane.*` [M1]

| Method | Params → Result |
|---|---|
| `pane.list` | `{workspace?, tab?, has_agent?}` → `{panes}` |
| `pane.get` | `{pane}` → `{pane, run?, open_interactions[]}` |
| `pane.current` | `{}` → `{pane}` (requires pane token / `@current`) |
| `pane.split` | `{pane, direction: right|down|left|up, ratio?: 0.5, cwd?, command?: argv, env?: {k:v}, focus?: false, title?}` → `{pane}` |
| `pane.float` | `{tab, rect?: {x%,y%,w%,h%}, cwd?, command?, focus?}` → `{pane}` [M1] |
| `pane.move` | `{pane, to: {tab} | {workspace} | {new_tab_in: workspace}, position?}` → `{pane, previous_pane_handle}` |
| `pane.resize` | `{pane, direction, cells?|percent?}` → `{layout}` |
| `pane.zoom` | `{pane, zoomed?: toggle}` → `{tab}` |
| `pane.focus` | `{pane, client?}` → `{pane}` |
| `pane.rename` | `{pane, title: string|null}` → `{pane}` (emits `pane.title_changed`) |
| `pane.close` | `{pane, force?}` → `{}` |
| `pane.send_text` | `{pane, text, paste?: auto|bracketed|raw = auto}` → `{bytes}` — `auto` honors the pane's live bracketed-paste mode *(notify ok)* |
| `pane.send_keys` | `{pane, keys: [string]}` → `{}` — key grammar §2.6.1; all keys validated before any byte is written |
| `pane.run` | `{pane, command: string, wait?: bool, timeout_ms?}` → `{}` or (with wait) `{exit_code?, output_tail}` — sends text + Enter; with `wait`, uses OSC 133 prompt marks when the shell integration is active, else falls back to "foreground process returns to shell" |
| `pane.read` | `{pane, source: visible|recent|recent_unwrapped|scrollback|detection, lines?: 200, from_line?, format?: text|ansi|cells, include_cursor?}` → `{text|cells, rows, revision, truncated, scroll}` |
| `pane.wait_output` | `{pane, match?: string, regex?: string, source?, timeout_ms?, since_revision?}` → `{matched: string, line, revision}` |
| `pane.wait_idle` | `{pane, quiet_ms: 2000, timeout_ms?}` → `{revision}` |
| `pane.mark_unread` / `pane.mark_seen` | `{pane}` → `{pane}` |
| `pane.pin` | `{pane, pinned: bool}` → `{pane}` |
| `pane.sync_input` | `{panes: [pane], enabled: bool}` → `{group_id}` — synchronized input |
| `pane.scroll` | `{pane, to: bottom|top|line, line?, delta?}` → `{scroll}` |
| `pane.screenshot` | `{pane, format?: png|svg|html}` → `{blob}` — renders the pane grid (for bug reports and Phase 2) [M4] |

**`pane.read` never scrolls the user's view.** A scrollback read on alt-screen agents can drive the agent's mouse-scroll interface and visibly scrolls the operator's terminal. Vibeke serves history from the VT engine's scrollback plus the scrollback archive (01 §4); for alt-screen agents with structured adapters, transcript history comes from `agent.transcript` (§2.7) instead.

`revision` is a real per-pane monotonically increasing counter (bumped on every damage batch). It is load-bearing: `pane.wait_output {since_revision}` and clients' race guards may rely on it.

#### 2.6.1 Key grammar (shared by `pane.send_keys`, `agent.send_keys`, config keybindings)

- Named keys (case-insensitive): `enter tab esc|escape space backspace|bs delete|del insert home end pageup|pgup pagedown|pgdn up down left right f1…f24`, plus `minus comma period slash backslash semicolon quote backtick lbracket rbracket equal plus ampersand`.
- Single characters are typed literally (`"1"`, `"y"`, `"é"`).
- Chords: `ctrl+c`, `alt+shift+p`, `cmd+k`, `super+x`, modifiers in any order. Modifiers: `ctrl shift alt|meta|opt cmd super hyper`.
- Herdr's grammar is a strict subset, so Herdr scripts work unchanged; Vibeke additionally accepts the keys Herdr rejects (`pageup`, `home`, `end`, `delete`, `insert`).
- Encoding is per pane mode: legacy xterm, `modifyOtherKeys`, or kitty keyboard protocol flags as negotiated by the child (03 §keyboard).
- Literal tmux syntax (`C-c`) is rejected with `invalid_key` and a hint.

### 2.7 `agent.*` [M2]

| Method | Params → Result |
|---|---|
| `agent.list` | `{workspace?, state?: [AgentState], harness?}` → `{runs: [AgentRun + {pane, open_interactions: n}]}` |
| `agent.get` | `{target}` → `{run, pane, open_interactions[], last_turn?}` |
| `agent.start` | `{pane, harness, name?, mode?: tui|headless = tui, args?: [..], env?, model?, task?, ready_timeout_ms?: 30000}` → `{run}` — requires an available shell pane at its prompt (`conflict:pane_busy` otherwise); returns once the adapter (or detector) reports `idle` |
| `agent.spawn` | `{harness, name?, where: {split_of: pane, direction?} | {new_tab_in: workspace} | {task: task} , prompt?, args?, focus?: false}` → `{pane, run}` — convenience: split/tab + start + optional first prompt |
| `agent.prompt` | `{target, text, images?: [blob|path], mode?: send|steer|follow_up = send, wait?: bool, until?: [AgentState], timeout_ms?}` → `{run, turn?}` — submits text + Enter atomically via the best channel (RPC `prompt`/`steer` for headless and extension-capable harnesses, bracketed paste + Enter otherwise). If the run is not working and no lifecycle change occurs within 5 s → `stalled` |
| `agent.wait` | `{target, until?: [AgentState] = [idle, done, needs_approval, needs_answer, error, exited], timeout_ms?}` → `{run, state, interaction?}` |
| `agent.interrupt` | `{target}` → `{run}` — native abort where available (`abort` RPC, Esc for TUIs) |
| `agent.send_keys` | `{target, keys}` → `{}` |
| `agent.read` | `{target, source?: visible|recent|transcript, lines?, format?}` → as `pane.read`, plus `transcript` returns the last N turns from the structured log |
| `agent.transcript` | `{target, after_turn?, limit?: 20, include_items?: summary|full}` → `{turns: [Turn + {items}]}` — structured adapters only (`unsupported` + fallback hint otherwise) |
| `agent.rename` | `{target, name: string|null}` → `{run}` |
| `agent.release` | `{target}` → `{}` — stop tracking (the process keeps running as an untracked pane occupant) |
| `agent.resume` | `{pane?, run: ended_run_id, mode?}` → `{run}` — re-launches via the harness resume argv in the same or a new pane |
| `agent.report` | adapter-only — see 04 §adapter protocol (`{run?, pane, source, state?, harness_session_id?, transcript_path?, resume?, turn?, item?, seq}`) *(notify ok)* |
| `agent.harnesses` | `{}` → `{harnesses: [{id, display, version_detected?, integration_installed, capabilities: {native_approval, native_question, steer, transcript, resume, headless}}]}` |

### 2.8 `interaction.*` [M2]

| Method | Params → Result |
|---|---|
| `interaction.list` | `{status?: open, run?, workspace?, kind?}` → `{interactions}` — sorted by `opened_at` in Phase 1; Phase 2 adds ranking |
| `interaction.get` | `{interaction}` → `{interaction}` |
| `interaction.answer` | `{interaction, decision?: allow|allow_always|deny, choices?: {qid: [oid]}, text?, scope?: once|session|rule, rule?: PolicyRule}` → `{interaction, delivered: bool, channel: native|keystrokes}` — **forbidden** (`permission_denied:self_answer_forbidden`) when the caller's token belongs to the run's own pane or any pane/run descended from it (09 §5) |
| `interaction.cancel` | `{interaction}` → `{interaction}` (user dismisses; adapter delivers deny/escape) |
| `adapter.interaction.open` | adapter-only `{pane, run?, kind, …payload}` → `{interaction}` |
| `adapter.interaction.await` | adapter-only `{interaction, timeout_ms}` → `{answer}` or `timeout` — long-poll used by blocking hooks/extensions |
| `adapter.interaction.resolve` | adapter-only `{interaction, resolution: answered_elsewhere|cancelled|expired}` → `{}` |

Keystroke delivery (screen-only harnesses) is **verified**: the adapter sends navigation keys, re-reads the detector screen, and only sends Enter when the highlighted option matches the chosen option (a proven technique, now in the server so every client benefits). On mismatch → `answer_failed {reason: "selection_mismatch"}` and the interaction stays open.

### 2.9 `policy.*` [M2]

| Method | Params → Result |
|---|---|
| `policy.list` | `{scope?}` → `{rules}` (merged view: global, user, trusted repo files) |
| `policy.add` | `{rule: PolicyRule}` → `{rule}` |
| `policy.remove` | `{rule_id}` → `{}` |
| `policy.test` | `{action: {tool, command?, paths?, url?}, scope}` → `{effect, rule?}` — dry-run |
| `policy.trust` | `{path}` → `{trusted: true, digest}` — trust a repo-local `.vibeke/` directory at its current content digest (09 §4) |

### 2.10 `task.*`, `worktree.*` [M3]

| Method | Params → Result |
|---|---|
| `task.create` | `{title, repo: path, base?: ref, isolation?: worktree|jj_workspace|none, slug?, branch?, agents?: [{harness, name?, prompt?}], setup?: bool = true, ports?: n, group?}` → `{task, workspace, panes[], runs[]}` |
| `task.list` | `{status?, repo?}` → `{tasks}` |
| `task.get` | `{task}` → `{task, workspace, runs, previews, collisions[]}` |
| `task.park` / `task.resume` | `{task}` → `{task}` — park = stop agents gracefully, keep worktree |
| `task.finish` | `{task, remove_worktree?: ask|true|false, delete_branch?: false}` → `{task}` |
| `task.archive` | `{task}` → `{task}` |
| `task.setup_log` | `{task}` → `{text}` |
| `worktree.list` | `{repo?: path, cwd?: path}` → `{worktrees: [{path, branch, head, task?, workspace?, locked, prunable}]}` |
| `worktree.create` | `{repo|cwd, branch, path?, base?, open?: bool, focus?: false}` → `{worktree, workspace?}` |
| `worktree.open` | `{path, focus?: false}` → `{workspace}` |
| `worktree.remove` | `{path, force?: false}` → `{job}` — **async**; progress via `worktree.removed` event |
| `worktree.repo_root` | `{cwd}` → `{repo_root, vcs}` |

### 2.11 `preview.*`, `browser.*` [M4]

| Method | Params → Result |
|---|---|
| `preview.list` | `{machine?, task?, pane?, status?}` → `{previews}` |
| `preview.declare` | `{port, host?: 127.0.0.1, scheme?: http, path?, label?, pane?, task?}` → `{preview}` |
| `preview.open` | `{preview, client?: "local-browser"|client_id}` → `{local_url}` — ensures forwarding then opens in the client machine's browser |
| `preview.url` | `{preview}` → `{local_url, remote_url}` — no side effects (for agents: what URL to tell the human) |
| `preview.forget` | `{preview}` → `{}` |
| `browser.screenshot` | `{preview?|url, path?, viewport?: {w, h, dpr}, full_page?: false, wait_for?: {selector|network_idle|ms}, device?: "iphone-15"|…}` → `{blob, path_on_machine, width, height}` — runs headless Chromium **on the machine where the dev server runs**; result also written to `$TMPDIR/vibeke-shots/<hash>.png` so an agent can read it as a file |
| `browser.console` | `{preview|url, since_ms?, level?: error|warn|all}` → `{entries: [{ts, level, text, source}]}` |
| `browser.navigate` | `{session?: browser_session, url}` → `{browser_session, status}` |
| `browser.eval` | `{browser_session, expression}` → `{value}` — gated by capability `browser.script` |
| `browser.close` | `{browser_session}` → `{}` |
| `image.show` | `{blob|path, pane?: @current, max_cols?, max_rows?}` → `{}` — inline display in the TUI via kitty graphics/iTerm2/sixel passthrough; text fallback shows dimensions + `vibeke open` hint |
| `image.upload` | `{pane, mime, data_b64 | path_on_client}` → `{path_on_machine, blob}` — client→remote image transfer (paste/drag) |

### 2.12 `notification.*` [M1]

| Method | Params → Result |
|---|---|
| `notification.list` | `{unread_only?: true, limit?}` → `{notifications}` |
| `notification.send` | `{title, body?, urgency?: normal, subject?: pane|run|task, sound?: bool}` → `{notification}` — for scripts/plugins |
| `notification.read` | `{notification|all: true}` → `{}` |
| `notification.config` | `{}` → `{channels: [os, terminal_bell, osc9, sound, plugin:<id>], rules}` |

### 2.13 `events.*` [M1]

| Method | Params → Result |
|---|---|
| `events.subscribe` | `{after_seq?: u64, types?: [glob], subjects?: {workspace?, tab?, pane?, run?, task?}, include_snapshot?: false, machine?: label|"*"}` → `{subscription_id, at_seq}` then notifications |
| `events.unsubscribe` | `{subscription_id}` → `{}` |
| `events.read` | `{after_seq?, before_seq?, types?, subjects?, limit?: 500}` → `{events, next_seq}` — paginated history |
| `events.wait` | `{types, subjects?, after_seq?, timeout_ms?}` → `{event}` — one-shot wait (CLI-friendly) |

Server push (JSON-RPC notification):
```json
{"jsonrpc":"2.0","method":"events.event","params":{"subscription_id":"s1","event":{"seq":18342,"ts":1791232838418,"v":1,"type":"agent.state_changed","subject":{…},"actor":{…},"data":{…}}}}
```
- `include_snapshot: true` → first push is `events.snapshot {subscription_id, at_seq, projections}` (same shape as `session.snapshot`), then events with `seq > at_seq`.
- Ordered, at-least-once per subscription; clients dedupe by `seq` (or `(machine, seq)` for `machine:"*"`).
- Back-pressure: each subscription has a 10,000-event queue; overflow → `events.overflow {subscription_id, resume_from_seq}` and the subscription is closed; the client resubscribes with `after_seq` (cheap: the log is durable). Never silent loss.
- Cursor too old → error `truncated {earliest_seq}`.
- Glob types: `agent.*`, `interaction.opened`, `pane.{created,closed}`.

### 2.14 `layout.*`, `config.*`, `search.*` [M1]

| Method | Params → Result |
|---|---|
| `layout.export` | `{tab|workspace}` → `{layout: LayoutSpec}` (TOML/JSON-serializable, includes commands and cwds) |
| `layout.apply` | `{layout, workspace?, new_workspace?: {cwd, name}}` → `{workspace, tabs, panes}` |
| `config.get` | `{key?}` → `{value, source: default|user|repo|cli}` |
| `config.set` | `{key, value, persist?: false}` → `{}` — runtime override |
| `config.validate` | `{path?}` → `{errors: [{line, col, message}]}` |
| `search.query` | `{q, scope?: {workspace?, pane?, run?}, sources?: [scrollback, transcript, events], limit?: 50, regex?: false}` → `{hits: [{pane, run?, source, line, text, ts, context}]}` — FTS5 over archive + transcripts |

### 2.15 `blob.*` [M1]

| Method | Params → Result |
|---|---|
| `blob.put` | `{mime, data_b64}` (≤ 16 MiB) or `{mime, path}` (server reads local file) → `{hash, size}` |
| `blob.get` | `{hash, range?}` → `{mime, data_b64}` |
| `blob.stat` | `{hash}` → `{mime, size, created_at, refs}` |

### 2.16 `plugin.*` [M5]

| Method | Params → Result |
|---|---|
| `plugin.list` | `{}` → `{plugins: [{id, version, enabled, kind: actions|process, capabilities, status}]}` |
| `plugin.install` | `{source: "owner/repo"|path|url, ref?, accept_capabilities?: [..]}` → `{plugin, requested_capabilities}` — returns `permission_denied:capabilities_not_accepted` until confirmed |
| `plugin.link` | `{path}` → `{plugin}` (dev mode, hot reload) |
| `plugin.enable` / `plugin.disable` / `plugin.remove` | `{plugin}` → `{plugin}` |
| `plugin.action` | `{plugin, action, context?: {workspace?, pane?}}` → `{exit_code, stdout_tail}` |
| `plugin.kv.get` / `plugin.kv.set` / `plugin.kv.delete` / `plugin.kv.list` | plugin-only, scoped to caller's plugin id |
| `ui.contribute` | plugin-only — see §7.4 |

### 2.17 `integration.*` [M2]

| Method | Params → Result |
|---|---|
| `integration.list` | `{}` → `{integrations: [{harness, installed, version, files: [path], up_to_date}]}` |
| `integration.install` | `{harness, scope?: user|project}` → `{files_changed}` — writes hook configs / extension registration; idempotent; never overwrites user hooks (merges, marks its entries) |
| `integration.uninstall` | `{harness}` → `{files_changed}` |
| `integration.doctor` | `{harness?}` → `{checks: [{name, ok, detail}]}` |

---

## 3. Render stream protocol [M1]

Opened by `render.attach` on a fresh connection; after the JSON response the connection switches to binary frames. Defined in `vk-proto::render` with `postcard`; frame = `u32 LE length | u8 frame_type | postcard payload`.

```json
→ {"jsonrpc":"2.0","id":1,"method":"render.attach","params":{"client_id":"c7","viewport":{"cols":220,"rows":60,"px_w":3520,"px_h":1920},"caps":{"truecolor":true,"kitty_graphics":true,"sixel":false,"iterm2_images":false,"kitty_keyboard":true,"osc52":true,"hyperlinks":true,"max_fps":120,"sync_output":true}}}
← {"jsonrpc":"2.0","id":1,"result":{"protocol":1,"frame_types":[…]}}
```

### 3.1 Server → client frames

| Type | Name | Payload |
|---|---|---|
| 0x01 | `Hello` | `{protocol, server_version, session, palette, theme}` |
| 0x02 | `Layout` | full UI model for this client: workspaces/tabs tree, active tab layout rects, floating panes, sidebar model (agent states, unread, pins), status bar segments (incl. plugin segments) |
| 0x03 | `PaneFull` | `{pane, generation, cols, rows, cells: RLE-encoded rows, cursor, modes}` — on attach, resize, or when the client lost sync |
| 0x04 | `PaneDiff` | `{pane, generation, base_frame, rows: [(row_idx, RLE cells)], scroll_region_shift?, cursor, modes}` — damage since client's last acked frame; `scroll_region_shift` encodes scrolling as a single op |
| 0x05 | `Image` | `{hash, mime, size, data?}` — data sent once per client; placements reference hash |
| 0x06 | `ImagePlacement` | `{pane, id, hash, cell_rect, z, crop}` / removal |
| 0x07 | `Notify` | `{notification}` (toast) |
| 0x08 | `Bell` | `{pane}` |
| 0x09 | `Clipboard` | `{selection: clipboard|primary, data}` — OSC 52 from a pane, delivered to the attached client (policy-gated, 09 §8) |
| 0x0A | `Title` | `{pane, title}` |
| 0x0B | `ModeChange` | `{pane, mouse_mode, bracketed_paste, kitty_kbd_flags, focus_events, alt_screen}` |
| 0x0C | `Popup` | server-driven modal (confirmations, pickers): `{id, kind, model}` |
| 0x0D | `Pong` | `{nonce, server_ts}` |
| 0x0E | `Goodbye` | `{reason}` |

Cell encoding: `{ch: u32 grapheme-id or inline char, width: u8, fg, bg, ul_color, attrs: u16, link_id?}`; graphemes beyond one scalar are interned per stream (`GraphemeTable` updates piggyback on diffs). Hyperlink targets interned similarly.

### 3.2 Client → server frames

| Type | Name | Payload |
|---|---|---|
| 0x81 | `Ack` | `{pane, frame}` — per pane; server diffs against the last acked state (SSP semantics) |
| 0x82 | `Input` | `{pane, bytes}` — already encoded by the client for the pane's negotiated keyboard mode |
| 0x83 | `Key` | `{pane, key: KeyEvent}` — alternative: server encodes (used by thin clients) |
| 0x84 | `Mouse` | `{pane, event, cell, px?, mods}` |
| 0x85 | `Paste` | `{pane, text}` (server applies bracketed paste if enabled) |
| 0x86 | `Resize` | `{viewport}` |
| 0x87 | `Command` | `{rpc: JSON-RPC request}` — UI commands piggyback here to keep ordering with input |
| 0x88 | `Focus` | `{pane}` |
| 0x89 | `Ping` | `{nonce, client_ts}` |
| 0x8A | `ViewHint` | `{visible_panes, fps_cap}` — lets the server skip work for panes not on screen |

Pacing: server sends at most `min(client.max_fps, adaptive)` diffs per pane; unfocused panes whose damage is spinner-only are capped at `render.background_fps` (default 4). A pane whose diff rate exceeds bandwidth budget gets frames dropped *between acks* (state-sync makes this lossless in the end state).

---

## 4. Holder protocol [M1]

Between server and `vibeke hold`. Kept deliberately small and **separately versioned** (`holder/1`). Lives in `vk-proto::holder`, no dependency on the rest of the server.

Frame: `u32 LE length | u8 type | postcard payload`. Socket: `$RUNTIME/<session>/holders/<pane-ulid>.sock`, 0600.

| Dir | Type | Message | Payload |
|---|---|---|---|
| S→H | 0x01 | `Hello` | `{proto_min, proto_max, server_pid, server_boot_id}` |
| H→S | 0x02 | `HelloOk` | `{proto, holder_version, pane_id, child_pid, started_at, ring: {start_offset, end_offset, capacity}, lease: {epoch, holder_token}}` |
| S→H | 0x03 | `Acquire` | `{epoch: prev+1, server_pid}` — takes the lease; the holder rejects frames carrying an older epoch (**fencing**: an orphaned old server can never write after a new one acquired) |
| H→S | 0x04 | `Acquired` | `{epoch}` |
| S→H | 0x05 | `Attach` | `{epoch, from_offset}` |
| H→S | 0x06 | `Output` | `{offset, bytes}` — `offset` is absolute byte offset of the first byte; replay then live |
| H→S | 0x07 | `Gap` | `{requested, available_from}` — ring overflowed past `from_offset` |
| S→H | 0x08 | `Input` | `{epoch, bytes}` |
| S→H | 0x09 | `Resize` | `{epoch, cols, rows, px_w, px_h}` |
| S→H | 0x0A | `Signal` | `{epoch, sig: INT|TERM|HUP|KILL|WINCH|CONT|STOP, target: fg_pgrp|child}` |
| S→H | 0x0B | `Status?` | `{}` |
| H→S | 0x0C | `Status` | `{child_pid, fg_pgid, fg_cmdline, fg_cwd?, exited: bool, exit_code?, signal?, tty_modes: {echo, icanon}}` |
| H→S | 0x0D | `ChildExited` | `{exit_code?, signal?}` |
| S→H | 0x0E | `AckExit` | `{epoch}` → holder flushes and exits |
| S→H | 0x0F | `Checkpoint` | `{epoch, offset}` — server tells the holder which offset is safely captured in a VT snapshot; holder may report it in `HelloOk` |
| either | 0x10 | `Ping` / 0x11 `Pong` | keepalive (10 s) |

- Spawn: server execs `vibeke hold --pane <ulid> --socket <path> --ring 16MiB --cwd <dir> --env-file <tmp 0600, deleted after read> -- <argv>`; holder double-forks, `setsid`, opens the PTY, spawns the child with the PTY as controlling terminal, writes `ready` on an inherited pipe, and closes it.
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
vibeke group     list|create|rename|move|delete
vibeke tab       list|create|rename|move|focus|close
vibeke pane      list|get|current|split|float|move|resize|zoom|focus|rename|close
                 send-text|send-keys|run|read|wait-output|wait-idle
                 mark-unread|pin|sync-input|scroll|screenshot
vibeke agent     list|get|start|spawn|prompt|wait|interrupt|send-keys|read|transcript
                 rename|release|resume|harnesses
vibeke interaction list|get|answer|cancel   (alias: vibeke ask …)
vibeke policy    list|add|remove|test|trust
vibeke task      new|list|get|park|resume|finish|archive|setup-log
vibeke worktree  list|create|open|remove|repo-root
vibeke preview   list|declare|open|url|forget
vibeke browser   screenshot|console|navigate|eval|close
vibeke image     show|upload
vibeke notification list|send|read
vibeke events    tail [--types agent.*] [--after-seq N] [--follow] | read | wait
vibeke search    <query> [--pane p] [--workspace w] [--source scrollback,transcript]
vibeke layout    export|apply
vibeke machine   list|add|connect|disconnect|remove|status|install
vibeke integration list|install <harness>|uninstall <harness>|doctor
vibeke plugin    list|install|link|enable|disable|remove|action
vibeke config    path|get|set|validate|edit|reset-keys
vibeke import    herdr [--config] [--session] [--dry-run]
vibeke api       schema|methods|call <method> [json]      # raw access
vibeke doctor    [--fix] [--rebuild]
vibeke debug     bundle [--out file] | holders | replay <pane>
vibeke update    [--check] [--channel stable|preview] [--rollback]
vibeke channel   get|set <stable|preview>
vibeke completion <zsh|bash|fish|nu|powershell>
vibeke hook      <harness> <event>                          # called by agent hooks (04)
vibeke hold      …                                          # internal
vibeke bridge                                               # internal (remote)
vibeke --skill | --default-config | --version | --help
```

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
- `--dry-run` supported on every mutating command that touches the filesystem (`task new`, `worktree remove`, `integration install`, `import herdr`, `plugin install`).

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
8. **Previews and screenshots (new)**: after starting a dev server, run `vibeke preview list --pane @current` (or `preview declare --port N`), give the user `vibeke preview url <v>`; verify UI work with `vibeke browser screenshot <v> --out /tmp/x.png` and then read the PNG as an image; check `vibeke browser console <v> --level error`. Works identically on remote machines — the screenshot is taken next to the server.
9. **Images to the user**: `vibeke image show <path>` displays inline in the user's TUI.
10. **Ordinary commands**: `pane split` + `pane run --wait` (OSC 133 aware) + `pane read --source recent_unwrapped`.
11. **Search**: `vibeke search` across scrollback and transcripts instead of asking the user to scroll.
12. **Safety rules**: don't close what you didn't create; don't `server stop`; don't `--force`; don't install plugins/integrations or change policy without explicit user instruction; JSON errors on stderr with exit 1, usage errors exit 2, timeouts exit 3.

The skill is tested: CI runs a scripted agent (Claude Code headless) through the skill's examples against a test session and asserts the resulting events (10 §4.6).

---

## 7. Plugin system [M5]

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

- Executed with argv (no shell), cwd = plugin dir, env: `VIBEKE=1`, `VIBEKE_SOCKET`, `VIBEKE_PLUGIN_ID`, `VIBEKE_PLUGIN_TOKEN` (capability-scoped token), `VIBEKE_PLUGIN_DATA_DIR`, `VIBEKE_PLUGIN_CONFIG_DIR`, `VIBEKE_CONTEXT_WORKSPACE`, `VIBEKE_CONTEXT_PANE`; Herdr aliases (`HERDR_SOCKET_PATH`, `HERDR_PLUGIN_CONFIG_DIR`) when the plugin came from a `herdr-plugin.toml`.
- stdout/stderr captured to the plugin log; last 4 KiB returned by `plugin.action`; non-zero exit raises a notification.
- Event hooks: `[[on]] event = "worktree.created" command = [...]` — the event JSON is passed on stdin.

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
- Marketplace index: a static JSON index built daily by a GitHub Action from repos tagged topic **`vibeke-plugin`** (and, read-only, `herdr-plugin` repos that pass the compat checker), published to `plugins.vibeke.dev/index.json`; `vibeke plugin search <q>` reads it. No server-side code execution; the index stores repo, ref, manifest summary, capabilities and stars.
- Dev loop: `vibeke plugin link <path>` → registers in dev mode; the server watches the manifest and the process command's files (configurable globs) and hot-restarts the plugin process on change; argv actions are re-read each invocation. `vibeke plugin logs <id> -f`.
- Herdr plugin import: a `herdr-plugin.toml` is parsed into the same model (`[[build]]`, `[[actions]]`, `[[panes]]`, event hooks, link handlers map 1:1). Herdr plugins get the Herdr env aliases and the compat socket path (§8.3) so their `herdr` CLI calls keep working.

---

## 8. Herdr compatibility layer (`vk-compat`) [M5; importer in M1]

Goal: an existing Herdr user switches with one command, and **existing socket clients work unmodified** against Vibeke. Scope is bounded to what is listed here.

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
| `~/.config/herdr/plugins.json`, `plugins/` | listed; each offered for `plugin install` via §7.6 import |
| Herdr integrations (`~/.claude/hooks/herdr-agent-state.sh`, `~/.codex/hooks.json` + `herdr-agent-state.sh`, pi extension) | detected; `vibeke integration install` installs Vibeke's alongside (they no-op outside Herdr because they check `HERDR_ENV`) |

Report printed as a table; nothing is deleted from `~/.config/herdr`.

### 8.2 Environment aliases [M1]

With `compat.herdr_env = true` (default on when an import has been done), every pane also gets: `HERDR_ENV=1`, `HERDR_SOCKET_PATH=<compat socket>`, `HERDR_PANE_ID=<herdr-style id>`, `HERDR_WORKSPACE_ID`, `HERDR_TAB_ID`, `HERDR_BIN_PATH=<vibeke shim>`. The shim `herdr` (installed only if no real `herdr` is on PATH, or explicitly via `vibeke compat install-shim`) maps the Herdr CLI subset (`pane …`, `agent …`, `workspace …`, `tab …`, `worktree …`, `pane report-agent`) onto Vibeke methods. Existing Herdr integrations therefore report into Vibeke as `self_report` sources.

### 8.3 Compat socket [M5]

- Path layout mirrors Herdr's so existing socket clients' session discovery works: default session `<herdr_root>/herdr.sock`, named sessions `<herdr_root>/sessions/<name>/herdr.sock`, where `<herdr_root>` defaults to `$RUNTIME/<session>/herdr-compat/` and is exported via `HERDR_SOCKET_PATH`. Users who run socket clients set `HERDR_SOCKET_PATH` or `compat.herdr_socket_path = "~/.config/herdr/herdr.sock"` when Herdr is no longer installed. The socket is removed on clean stop (clients use socket presence as liveness).
- Wire format exactly as Herdr: newline JSON `{"id": "<string>", "method": "...", "params": {...}}`; **one request per connection, server closes after the response**, except `events.subscribe` which streams. Response `{"id", "result": {"type": "<result_type>", …}}`; errors `{"id": "", "error": {"code": "<snake_case>", "message"}}`. Request line cap ≥ 1 MiB. Integer ids → `invalid_request`.
- Ids: Herdr-style `w<ulid-ish>`, `<ws>:t<n>`, `<ws>:p<n>` handles are produced by a stable mapping table (compat ids never reused; `pane.move` yields a new id like Herdr).
- `agent_status` mapping: `working → working`, `needs_approval|needs_answer → blocked`, `done → done`, `idle → idle`, `starting|unknown|error|rate_limited → unknown` (`error`/`rate_limited` additionally set `agent_status_detail`, ignored by Herdr clients). `exited` → no agent.
- `agent_session` on panes: `{source: "vibeke:<harness>", agent, kind: "id"|"path", value}` derived from `AgentRun.harness_session_id` / `transcript_path` (pi uses `kind:"path"`), and, cleared when the run ends (avoids the stale-ref problem).
- `revision` is real (pane revision counter). `scroll` provided.

**Method mapping (everything existing socket clients call, plus near neighbours):**

| Herdr method (params) | Herdr `result.type` | Vibeke implementation |
|---|---|---|
| `session.snapshot {}` | `session_snapshot` → `{version, protocol, workspaces, tabs, panes, agents, layouts, focused_workspace_id, focused_tab_id, focused_pane_id}` | `session.snapshot` projected to Herdr shapes; `focused_*` = most recently active TUI client's focus; `protocol: 20` (the Herdr protocol level we emulate) |
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
| `pane.read {pane_id, source: visible|recent|recent_unwrapped|detection, lines, format: text|ansi}` | `pane_read` → `{read: {text, truncated, revision}}` | `pane.read` — **never** moves the operator's view, even for `lines > viewport_rows` on alt-screen agents (improvement; background `visible` polls are unaffected) |
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

Event translation (compat stream; snake_case `event` field, dot-form subscription `type`):

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

Conformance: `tests/compat/` replays recorded bridge traffic (fixtures from `bridge/mux/herdr/fixture.ts`) against the compat socket and diffs response shapes; CI also runs a pinned socket-client release against a Vibeke test session (smoke: dashboard loads, reply sent, key sent, tab created, events stream). Out of scope: Herdr's TUI-specific client methods, plugin-pane internals, `herdr --remote` wire protocol, and anything not listed above (`method_not_found` with a hint to the native API).
