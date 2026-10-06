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
| `client.appearance` | `{dark: bool, source?: osc11\|csi996}` → `{appearance, colorfgbg}` — host terminal light/dark report (theme propagation, 08 §11) [M4] |
| `client.focus` | `{pane \| url: "vibeke://focus?session=…&pane=…", raise?: true}` → `{pane, client, focused, raised, host}` — focus in the most recently active attached client and raise its host terminal (08 §7.1 click-to-focus); full scope only [M4] |
| `theme.get` / `theme.set_mode` | `{}` / `{mode: auto\|light\|dark\|null}` → `{appearance, colorfgbg, reports?}` — `set_mode` is a runtime override, not persisted [M4] |
| `status.segments` | `{pane?, client?}` → `{segments: {machine, session, workspace, task, branch, ports, attention, agents_summary, cpu, clock, sync_input}, focus, appearance, client_side: [mode, prefix_indicator]}` — data for the built-in status-bar segments (08 §4) from that client's focus [M4] |

`render.attach` and `client.hello` accept an optional `host: {bundle_id?, term_program?}` (the host terminal's `__CFBundleIdentifier` / `TERM_PROGRAM`), used to raise the window on click-to-focus and to enable native notifications (08 §7.1). *(M4: server side implemented; the TUI does not send it yet.)*
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
| `group.list` / `group.add` / `group.remove` | `{}` → `{groups: [Group + {agent_summary}], ungrouped}`; `{group, workspace, index?}` / `{workspace}` → `{workspace, group}` |

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
| `pane.float` | `{tab, rect?: {x%,y%,w%,h%}, cwd?, command?, focus?}` → `{pane}` [M4] — *implemented:* without `pane`, a new floating pane (default 70%×70% centred); with `pane`, floats a tiled pane (`conflict` for a tab's last tiled pane) or moves/resizes/raises a floating one. Floats live in `Tab.floating: [{pane, x, y, w, h, z}]` (percent), not in `layout` |
| `pane.embed` | `{pane, target?, direction?: right, ratio?}` → `{pane, tab}` — a floating pane back into the tiling, split next to `target` (default the focused tiled pane) [M4] |
| `tab.floats` | `{tab?, visible?}` → `{tab}` — show/hide all floats (`Tab.floats_hidden`; toggles without `visible`) [M4] |
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
| `pane.screenshot` | `{pane, format?: png|svg|html}` → `{blob}` — renders the pane grid (for bug reports and Phase 2) [M3] |

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
| `agent.start` | `{pane, harness, name?, mode?: tui|headless = tui, args?: [..], env?, model?, task?, ready_timeout_ms?: 30000}` → `{run}` — requires an available shell pane at its prompt (`conflict:pane_busy` otherwise); returns once the adapter (or detector) reports `idle` |
| `agent.spawn` | `{harness, name?, where: {split_of: pane, direction?} | {new_tab_in: workspace} | {task: task} , prompt?, args?, focus?: false}` → `{pane, run}` — convenience: split/tab + start + optional first prompt |
| `agent.prompt` | `{target, text, images?: [blob|path], mode?: send|steer|follow_up = send, wait?: bool, until?: [AgentState], timeout_ms?}` → `{run, turn?}` — submits text + Enter atomically via the best channel (RPC `prompt`/`steer` for headless and extension-capable harnesses, bracketed paste + Enter otherwise). If the run is not working and no lifecycle change occurs within 5 s → `stalled` |
| `agent.wait` | `{target, until?: [WaitCondition] = [idle, done, needs_approval, needs_answer, error, exited], timeout_ms?}` → `{run, state, interaction?}` — `WaitCondition` = an execution state (04 §2.4) or a derived condition: `needs_approval`/`needs_answer` = an open interaction of that kind, `done` = idle with a turn completed after the wait started (never read state) |
| `agent.interrupt` | `{target}` → `{run}` — native abort where available (`abort` RPC, Esc for TUIs) |
| `agent.send_keys` | `{target, keys}` → `{}` |
| `agent.read` | `{target, source?: visible|recent|transcript, lines?, format?}` → as `pane.read`, plus `transcript` returns the last N turns from the structured log |
| `agent.transcript` | `{target, after_turn?, limit?: 20, include_items?: summary|full}` → `{turns: [Turn + {items}]}` — structured adapters only (`unsupported` + fallback hint otherwise) |
| `agent.rename` | `{target, name: string|null}` → `{run}` |
| `agent.release` | `{target}` → `{}` — stop tracking (the process keeps running as an untracked pane occupant) |
| `agent.resume` | `{pane?, run: ended_run_id, mode?}` → `{run}` — re-launches via the harness resume argv in the same or a new pane |
| `agent.report` | adapter-only — see 04 §adapter protocol (`{run?, pane, source, state?, harness_session_id?, transcript_path?, resume?, turn?, item?, seq}`) *(notify ok)* |
| `agent.harnesses` | `{}` → `{harnesses: [{id, display, version_detected?, integration_installed, capabilities: {native_approval, native_question, steer, transcript, resume, headless}}]}` |

### 2.8 `interaction.*` [M1]

| Method | Params → Result |
|---|---|
| `interaction.list` | `{status?: open, run?, workspace?, kind?}` → `{interactions}` — sorted by `opened_at` in Phase 1; Phase 2 adds ranking |
| `interaction.get` | `{interaction}` → `{interaction}` |
| `interaction.answer` | `{interaction, decision?: allow|allow_always|deny, choices?: {qid: [oid]}, text?, scope?: once|session|rule, rule?: PolicyRule, idempotency_key?, actor?, expected_decision_rev?}` → `{interaction, delivery: {state, channel: native|keystrokes}}` — `state` per the delivery state machine in 02/04; repeating the same `idempotency_key` with the same answer returns the recorded state (`duplicate: true`) and never delivers again; a different answer under that key is a `conflict`. `actor` (full-scope callers only) labels `answered_by`, e.g. `gateway:the maintainer's phone` (16 §7.7); the key is kept separately as `answer_key`. `expected_decision_rev` makes the answer a compare-and-set: if the interaction moved on, the call fails with `conflict` (`stale: …`). **Forbidden** (`permission_denied:self_answer_forbidden`) when the caller's token belongs to the run's own pane or any pane/run descended from it — this is *authorizing* (09 §5.1.1) |
| `interaction.cancel` | `{interaction}` → `{interaction}` (user dismisses; adapter delivers deny/escape) |
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
| `policy.trust` | `{path, digest?}` → `{repo, digest, setup_script}` — trust a repo-local `.vibeke/` directory at its current blake3 digest (09 §4); `digest` makes it conditional on the reviewed content (conflict if changed). Full scope only. |

### 2.10 `task.*`, `worktree.*` [M1; remote tasks M3, jj M4; tracking per 15 T1–T3]

Pane-scoped callers (agents) may read tasks but not `task.track`, `task.intent.update`, `task.bind/unbind`, `task.set`, `task.message.*` (except get), `task.review.accept`, `task.check.run/cancel` or `attention.update` (15 §11). All tracking mutations accept `idempotency_key`; a repeat with the same key and payload returns the recorded result with `replayed: true`, a different payload is `conflict{reason: idempotency_key_reused}`.

| Method | Params → Result |
|---|---|
| `task.create` | `{title, repo: path, base?: ref, isolation?: worktree|jj_workspace|none, slug?, branch?, agents?: [{harness, name?, prompt?}], setup?: bool = true, ports?: n, group?}` → `{task, workspace, panes[], runs[]}` — *M4:* `isolation` also accepts `auto` (default from `tasks.checkout`/`tasks.vcs`: jj workspace when the repo has `.jj` and `jj` is installed, else worktree); the choice is recorded as `Task.checkout`; `task.get` of a jj task returns `branch_status {vcs: jj, branch, bookmark_exists, bookmarks, change_id, commit_id, dirty, conflict}` and `jj {workspace, colocated}`; `task.finish {remove_worktree}` runs `jj workspace forget` and deletes the directory (never for `none`) |
| `task.list` | `{status?, repo?}` → `{tasks}` |
| `task.get` | `{task}` → `{task, workspace, runs, previews, collisions[]}` |
| `task.park` / `task.resume` | `{task}` → `{task}` — park = stop agents gracefully, keep worktree |
| `task.finish` | `{task, remove_worktree?: ask|true|false, delete_branch?: false}` → `{task}` |
| `task.archive` | `{task}` → `{task}` |
| `task.setup_log` | `{task}` → `{text}` |
| `task.track` | (15 T1) `{run | pane, turn? | turns?: [n], title?, objective?, criterion?: text | [text | {text, required?, evaluation?, checks?}], constraint?, stop_at?, stop_detail?, target_branch?, idempotency_key?}` → `{task, intent, binding, baseline, review_base}` — attached task + intent revision 1 + binding, atomically; `conflict{reason: binding_unverified}` unless the run's identity is deterministic (structured session id); never spawns, sends or runs setup |
| `task.sources` | `{run | pane, limit?}` → `{run, identity_verified, native_conversation_id, turns: [{n, prompt, native_conversation_id, started_at_ms, ended_at_ms}]}` — exact recorded prompts for the Track form |
| `task.detail` | `{task}` → `{task, intent, bindings, runs, baseline, uncommunicated, messages}` |
| `task.intent.get` | `{task, revision?}` → `{intent, current_revision, revisions, uncommunicated: [criterion_id]}` |
| `task.intent.update` | `{task, expected_revision?, title?, objective?, criterion?/criteria?, add_criterion?, constraint?, stop_at?, stop_detail?, idempotency_key?}` → `{task, intent, uncommunicated, note}` — record-only; never sends |
| `task.bind` / `task.unbind` | `{task, run | pane, role?, start_turn?, end_turn?}` → `{binding, pending}` (a switch away from another task's active binding takes effect at the next turn boundary; a suspended binding is continued) / `{task, binding?}` → `{closed}` |
| `task.set` | `{task, expected_rev?, priority?, effort?: quick|minutes|deep|unknown}` → `{task}` |
| `task.message.prepare` / `.send` / `.get` / `.cancel` | `{task, text, communicates_intent?}` → `{message, recipient, send_path: prompt_input|open_pane_only, unsafe?}`; `{message, retry?, retry_despite_unknown?}` → `{message}` or `conflict{reason: send_unsafe, detail, fallback: open_pane_to_send}` with zero bytes written; delivery states `prepared|sending|delivered|delivery_unknown|failed|cancelled` (15 §9) |
| `task.review.get` / `.candidates` / `.diff` / `.accept` | (15 T2) deterministic package (`label`, `assessment`, `observed_commands`/`claims`, `checks`, `check_runs`, `acceptance {acceptance, status, outdated_reasons}`, `sources_verified`, `actions.accept`; flat client aliases `revision`, `criteria`, `observed`, `blockers`); `diff {task, subject?, path?, max_bytes?}` → `{diff, truncated, total_bytes, base_sha, head_sha}` from the subject's commits only; `accept {task, intent_revision, subject_id, exceptions:[{criterion, reason}], package_revision?}` validated against a state token under the commit lock (`conflict{reason: review_changed|subject_not_committed|exceptions_required|sources_unverified}`) |
| `task.check.list` / `.authorize` / `.run` / `.get` / `.cancel` | per-candidate authorization (15 §6.3); `definition_digest` must match the definition the user saw (`conflict{reason: definition_changed}`); `run` without a grant → `conflict{reason: authorization_required}`; one execution per idempotency key |
| `attention.list` / `attention.update` | (15 T3) ranked items with explanations, `coverage {complete, notes, scope?, excluded?}`, optional `five_minute`; `update {key, seen?, snooze_until_ms?, pin?}` never answers or accepts |
| `task.operation.get` | `{idempotency_key}` → `{known, method?, result?, at_ms?, expired?}` — mutation receipt (30-day window); unknown never means safe to repeat |
| `worktree.list` | `{repo?: path, cwd?: path}` → `{worktrees: [{path, branch, head, task?, workspace?, locked, prunable}]}` |
| `worktree.create` | `{repo|cwd, branch, path?, base?, open?: bool, focus?: false}` → `{worktree, workspace?}` |
| `worktree.open` | `{path, focus?: false}` → `{workspace}` |
| `worktree.remove` | `{path, force?: false}` → `{job}` — **async**; progress via `worktree.removed` event |
| `worktree.repo_root` | `{cwd}` → `{repo_root, vcs}` |

### 2.11 `preview.*`, `browser.*` [M3]

Semantics in [06](06-remote-and-preview.md) Part B.

| Method | Params → Result |
|---|---|
| `preview.list` | `{machine?, task?, pane?, status?: suggested|up|down|all}` → `{previews}` — suggestions only with `status: suggested|all` |
| `preview.declare` | `{port, host?: 127.0.0.1, scheme?: http, path?, label?, pane?, task?}` → `{preview}` |
| `preview.promote` / `preview.dismiss` | `{preview}` → `{preview}` — accept or hide a discovered suggestion |
| `preview.open` | `{preview, mode?: profile|proxy, client?: client_id}` → `{opened_in: profile|proxy|default_browser, url}` — profile mode (default) launches/reuses the Vibeke browser profile routed over SOCKS5 to the preview's machine and opens `http://localhost:<port>/<path>`; proxy mode returns a tokenized `*.vibeke.localhost` URL |
| `preview.url` | `{preview, mode?}` → `{remote_url, profile_url, proxy_url?}` — no side effects (for agents: what URL to tell the human) |
| `preview.mirror` / `preview.unmirror` | `{preview}` → `{local_port}` — explicit, unauthenticated raw local port (06 B4); full scope only |
| `preview.forget` | `{preview}` → `{}` |
| `preview.profile_list` / `preview.profile_reset` | `{machine?|task?}` → `{profiles}` / `{}` |

**As built (Goal 03 Stage 1; `vk-server::preview`).** `Preview` = `{id, handle: "v<N>", machine, pane?: pane ULID, task?, port, path, label?, url, scheme, status: suggested|declared|up|down|gone, source: declared|listener|output_url|banner, pid?, first_seen_ms, last_seen_ms}`; JSON results add `pane_handle` and `task_handle`. Previews are entity documents (`kind = "preview"`, gone ones closed) and `SessionModel.previews` (render stream; last field). A target is `v4`, a ULID, or `devbox/v4`; `machine` in params does the same. Machine-qualified calls are executed by the local server on the remote server over its own bridge link.

| Method | Params → Result (as built) |
|---|---|
| `preview.declare` | `{port, path?: "/", label?, scheme?: http|https, pane?: target (default: the calling pane), task?}` → `{preview, cursor}` — a live preview on that port (any source) is promoted and becomes `source: declared`; otherwise a new `declared` record, probed at once (→ `up`/`down`). Event `preview.declared`. |
| `preview.list` | `{machine?, task?, pane?, all?: bool, status?: suggested|declared|up|down|all}` → `{previews, machine}` — suggestions only with `all` or `status`. |
| `preview.get` | `{preview}` → `{preview}` |
| `preview.promote` | `{preview}` → `{preview}` — `suggested → up` (event `preview.up`). |
| `preview.forget` (alias `preview.dismiss`) | `{preview}` → `{}` — closes the record (event `preview.gone`) and suppresses re-suggestion of the same `(port, pid)`. |
| `preview.url` | `{preview}` → `{remote_url, profile_url}` |
| `preview.open` | `{preview | url, window?: bool, split?, machine?}` → `{opened_in: "window"|"default_browser", url, machine, profile, profile_dir, browser, browser_kind, pid, reused, socks_port, route: none|loopback|remote}` — Stage 1 opens windows only: without `window: true` (or `preview.mode = "window"`) → `unsupported {fallback: "window"}`. Opening promotes a suggestion. URLs must be `http(s)`; from a pane only loopback URLs and only this machine's previews (`permission_denied`). A profile already open for another machine/route → `conflict`; no browser found → `unsupported`. Event `preview.opened`. |
| `preview.profile.list` (aliases `preview.profile_list`, `preview.profile {action: "list"}`) | `{}` → `{profiles: [{name, path, running, pid?, machine?, route?, bytes}], root}` |
| `preview.profile.reset` (aliases `preview.profile_reset`, `preview.profile {action: "reset", profile}`) | `{profile}` → `{profile, removed}` — `conflict` while its browser runs; not allowed from a pane. |
| `preview.status` | `{}` → `{socks_port?, browsers: [{profile, machine, route, pid, running}], links: [{machine, connected, bytes_in, bytes_out, rtt_ms}], accepted, rejected}`; `server.status` also carries `preview: {socks_port, browsers, previews}`. |

Events: `preview.discovered`, `preview.declared`, `preview.up`, `preview.down`, `preview.gone` (subject `{preview, preview_handle, pane, task, machine}`, data `{preview}`) and `preview.opened` (data `{url, opened_in, profile, browser}`). `preview.test.register_browser {pid, profile, machine, route?}` exists only when the server runs with `VIBEKE_TEST_HOOKS=1`. Not built yet: `preview.mirror`/`unmirror`, proxy mode (B4), `client` routing of `open`.

CLI: `vibeke preview declare <port> [--path p] [--label l] [--pane p] [--task k]`, `list [--all] [--task k] [--pane p]`, `get|url|promote|forget <v>`, `open <v|machine/v|url> --window`, `profile list|reset <name>`, `status`. With `--machine m`, `open`, `url`, `profile` and `status` run on the local server with `machine: m` (the browser runs on the viewing machine); the others are forwarded to `m` as usual.

**As built (Goal 03 Stage 2; `vk-server::browser_pane`): browser panes (06 B3.2).** `preview.open` without `window` now opens a **browser pane**: `{preview | url, split?: right|down|left|up|tab|float, pane?, focus?}` → `{opened_in: "pane", pane, pane_handle, tab, url, machine, source_pane}` (the pane goes into the layout of the preview's machine; `float` = right split for now). `preview.open` also accepts `profile` (non-pane callers; used by the window handover).

| Method | Params → Result (as built) |
|---|---|
| `browser.pane.create` | `{pane?, preview? | url?, split?, machine?, focus?: true, focus_client?}` → `{opened_in: "pane", pane, pane_handle, tab, url, machine, source_pane}` — on the owning server; `pane` defaults to the preview's pane, then the caller's pane / focused pane. URLs: `http(s)` (`localhost:5173/x` gets `http://`); from a pane only loopback, only next to the caller's own panes, never moving focus. Events `pane.created {kind: "browser"}`, `tab.layout_changed`. |
| `browser.pane.update` | `{pane, url?, title?, history?, history_index?}` → `{pane}` — persists navigation reported by the viewing client (remote owners); event `browser.navigated`. Not from a pane. |
| `browser.pane.list` | `{}` → `{panes: [{pane, handle, tab, browser}]}` |
| `browser.status` | `{}` → `{browsers: [{profile, dpr, pid, targets, up_ms}], targets: [{pane, owner, url, title, profile, machine, route, running, screencast, viewers, frames, fps, decode_ms, css, frame, history, error}], windowed, tiles_sent, media_bytes}` — the media host's view. |
| `browser.command` | `{pane, cmd: back|forward|reload|stop|navigate|screenshot|text|window|pane, url?, hard?, text?}` → `{pane}` (`window` → the `preview.open` window result; `pane` → `{pane, profile}`) — for panes rendered by this server. Not from a pane. |

`Pane` gained `browser: BrowserPane? {url, machine ("" = owner), task?, preview?, source_pane?, history, history_index, title}` (last field). Config: `[preview] pane_browser` (headless Chromium for panes; default Playwright's headless shell or `$VIBEKE_CHROMIUM`). CLI: `vibeke preview open <v|machine/v|url> [--split right|down|tab|float | --window] [--pane p]`, `vibeke browser status|list|open <url>|command <pane> <cmd> [--url u]`, `vibeke debug fake-chromium` (tests). The agent-facing `browser.session_*` family below is Stage 3.
| `browser.session_open` | `{preview?|url?, viewport?: {w, h, dpr}, device?, color_scheme?: light|dark}` → `{browser_session}` — headless context **on the machine where the dev server runs**, owned by the caller's pane/run |
| `browser.navigate` | `{browser_session, url|path, wait?}` → `{status, final_url}` — destination rules apply to redirects too (06 B5) |
| `browser.click` / `browser.type` / `browser.press` | `{browser_session, selector|text|role, text?, key?, submit?}` → `{}` |
| `browser.wait` | `{browser_session, for: load|networkidle|{selector}|{ms}}` → `{}` |
| `browser.eval` | `{browser_session, expression}` → `{value}` — gated by capability `browser.script` |
| `browser.screenshot` | `{browser_session?|preview?|url?, full_page?, selector?, viewport?, device?}` → `{blob, path_on_machine, width, height, meta: ScreenshotMeta}` — `meta.environment` and `meta.code {task, head_sha, dirty_digest}` per 06 B6; file also written to `$TMPDIR/vibeke-shots/<hash>.png` |
| `browser.console` | `{browser_session|preview, since_ms?, level?: error|warn|all}` → `{entries: [{ts, level, text, source}]}` |
| `browser.network` | `{browser_session|preview, failed_only?, since_ms?}` → `{entries: [{ts, method, url, status?, error?, blocked_by_policy?}]}` |
| `browser.dom` | `{browser_session|preview, selector?, format?: text|html|a11y}` → `{content}` |
| `browser.diff` | `{a: blob, b: blob, threshold?}` → `{blob, changed_ratio, regions}` — refuses different `environment.kind` unless `force` |
| `browser.session_close` | `{browser_session}` → `{}` |

**As built (Goal 03 Stage 3; `vk-server::agent_browser`).** Sessions run on the server that serves the call (use `--machine m` for another machine's previews; a machine-qualified preview is refused with that hint). `session` accepts the handle (`b3`) or the ULID (`session_id`); `browser_session` and `target` are accepted as aliases. Pane-scoped callers see and drive only sessions owned by their pane or panes it created (others are `not_found`); while a human holds a session every pane-scoped call on it fails with `human_control` (code -32004, retryable). New error kinds: `destination_denied` (-32003, `details {url, reason}`), `human_control` (-32004, `details {session}`), `navigation_failed` (-32004, `details {url, error}`).

| Method | Params → Result (as built) |
|---|---|
| `browser.open` (alias `browser.session_open`) | `{preview? \| url?, viewport?: "WxH" \| {width, height}, dpr?, color_scheme?: light\|dark, dark?, wait?, timeout_ms?}` → `{session, session_id, owner: {pane, pane_handle, run} \| {user}, preview, url, created_ms, viewport, human_control, screencast, proxy_port, machine, environment: {kind: "remote_headless", machine, runner: "host", browser, fresh_context: true}, status?, final_url?, title?}` — no target opens `about:blank`; the destination is checked before anything starts; a failed first navigation closes the session. Opening a suggested preview promotes it. Event `browser.session_opened`. |
| `browser.navigate` | `{session, url \| path, wait?: load\|domcontentloaded\|none, timeout_ms?: 15000}` → `{session, status, final_url, title}` — `path` (`/x`) is relative to the current origin; `destination_denied` for refused targets and denied redirect hops; `navigation_failed` for other network errors; `timeout`. |
| `browser.click` | `{session, selector?: css \| "text=Label", text?, x?, y?, click_count?, timeout_ms?: 5000}` → `{session, x, y, element: {tag, text, width, height} \| null}` — waits for a visible element, scrolls it into view, clicks its centre (`not_found` with `details.selector`). |
| `browser.type` | `{session, selector?, text, submit?, clear?, timeout_ms?}` → `{session, typed}` — focuses `selector` (else the focused element), `Input.insertText`; `submit` presses Enter. |
| `browser.press` | `{session, key}` → `{session, key}` — Vibeke key grammar (`enter`, `ctrl+a`, `shift+tab`) and Playwright names (`ArrowDown`, `Control+A`); `invalid_key`. |
| `browser.wait` | `{session, for: load\|networkidle\|"selector:<css>"\|"ms:<n>", timeout_ms?}` → `{session, waited}` |
| `browser.eval` | `{session, expression}` → `{session, value}` — `returnByValue`, promises awaited; script errors → `invalid_params {exception}`; results over 256 KiB refused. From a pane only with `preview.browser_script = true` (else `permission_denied {capability: "browser.script"}`). |
| `browser.screenshot` | `{session, full_page?, selector?, inline?}` → `{session, id, handle, blob, path_on_machine, width, height, bytes, binding, label, meta: ScreenshotMeta, data_b64?, mime?}` — PNG stored content-addressed at `<state>/blobs/<h2>/<blake3>.png` (0600) with a `<blake3>.json` sidecar, plus a `screenshot` record (`screenshot.*` below, Stage 4) carrying environment, `code` (CodeState) and running-build identity/binding; `inline` adds base64 up to 8 MiB; full page is clipped at 16 384 CSS px. Events `browser.screenshot {blob, url, width, height, screenshot}` and `screenshot.captured`. The CLI's `--out f.png` fetches inline and writes locally (the server never writes to caller-chosen paths). |
| `browser.diff` (Stage 4) | `{a, b, threshold?: 0.1, force?, inline?}` → `{a: {id, handle, blob, environment, label, binding, head_sha, width, height}, b: …, threshold, channel_threshold, width, height, size_mismatch, a_size, b_size, changed_pixels, total_pixels, changed_ratio, regions: [{x, y, width, height, pixels}] (largest first, ≤ 50), regions_total, forced, blob, path_on_machine, bytes, data_b64?}` — `a`/`b` are screenshot ids or handles (`s3`), or raw blob hashes (full scope only). A pixel changed when any RGBA channel differs by more than `threshold × 255`; different sizes compare on the union canvas (extra area counts as changed). Regions are connected 16 px cells of changes. The diff image (after-image faded grey, changes red) is a blob with a `screenshot_diff` sidecar, expired with `screenshots.keep_days`. Different `environment.kind` → `invalid_params {reason: "environment_mismatch"}` unless `force`. Pane scope: both screenshots must be visible to the pane. |
| `browser.snapshot` (alias `browser.dom`) | `{session, format?: a11y\|text\|html, selector?, max_bytes?: 102400}` → `{session, url, format, content, truncated}` — `a11y` is the accessibility tree as an indented outline (`role "name" [value] {states}`). |
| `browser.console` | `{session, level?: error\|warn\|all, since_ms? \| since?: "5m", limit?: 200}` → `{session, entries: [{ts, level, text, source: console\|exception\|network\|dialog\|…, url?, line?}]}` |
| `browser.network` | `{session, failed_only? (CLI --failed), since_ms? \| since?, limit?}` → `{session, entries: [{ts, method, url, type, status, error, mime?, duration_ms?, blocked_by_policy?, layer?: proxy\|fetch\|api}]}` — `failed_only`: errors, policy blocks and status ≥ 400. |
| `browser.close` (alias `browser.session_close`) | `{session}` → `{session, closed}` — disposes the context, stops its proxy. Event `browser.session_closed {reason: closed\|idle\|owner_pane_closed\|owner_run_ended\|browser_exited\|open_failed}`. |
| `browser.list` | `{}` → `{sessions: [...], browser: <status>, machine}` — only sessions the caller may see. |
| `browser.status` | `{}` → `{running, pid?, product?, binary, kind, sessions, denied, profile_dir, idle_timeout_ms, uptime_ms?}` |
| `browser.install` | `{confirm?, version?, url?, sha256?}` → without `confirm`: `{plan: {version, platform, url, sha256, checksum_known, dir, binary, installed}, confirm_required: true}`; with `confirm`: downloads, verifies, installs → `{installed, binary, plan}`. Full scope only. |
| `browser.take_over` / `browser.release` | `{session}` → `{session, human_control}` — full scope only; events `browser.taken_over` / `browser.released {by}`. |
| `browser.attach_screencast` / `browser.detach_screencast` / `browser.screencast_frame` | `{session}` → `{session, delivery: "internal+poll", width, height}` / `{session, detached}` / `{session, after_seq?}` → `{session, seq, mime: "image/jpeg", width, height, received_ms, data_b64 \| null}` — full scope only. In-process consumers (the Stage 2 browser pane) use `AgentBrowsers::attach_screencast` (latest-wins `watch` channel) and `AgentBrowsers::human_input`. |

Events: `browser.session_opened`, `browser.session_closed`, `browser.request_denied {session, url, reason, layer, resource_type}` (≤ 20 per session per 10 s), `browser.screenshot`, `browser.taken_over`, `browser.released`; subject `{browser_session, session_id, pane, machine}`. Not built: `--device`, one-shot screenshots of a preview/URL without a session, `browser.diff --baseline task-start`.

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
| `screenshot.open` | `{id}` → as `get` with the image inline (the CLI writes it to `$TMPDIR/vibeke-screenshots/<id>.png` and opens it with `open`/`xdg-open`; `VIBEKE_NO_OPEN=1` only reports the path). |
| `screenshot.delete` | `{id, force?}` → `{id, handle, deleted, blob_removed}` — human only (`permission_denied` for panes); a screenshot referenced by a review acceptance needs `force` (`conflict {reason: "referenced_by_acceptance"}`). The blob goes when no other record references it. Event `screenshot.deleted {ids, reason: deleted\|retention}`. |

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
| `image.show` | `{blob|path, pane?: @current, max_cols?, max_rows?}` → `{}` — inline display in the TUI via kitty graphics/iTerm2/sixel passthrough; text fallback shows dimensions + `vibeke open` hint |
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
| `events.unsubscribe` | `{subscription_id}` → `{}` |
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

### 2.14 `layout.*`, `config.*`, `search.*` [M1]

| Method | Params → Result |
|---|---|
| `layout.export` | `{tab|workspace}` → `{layout: LayoutSpec}` (TOML/JSON-serializable, includes commands and cwds) |
| `layout.apply` | `{layout, workspace?, new_workspace?: {cwd, name}}` → `{workspace, tabs, panes}` |
| `config.get` | `{key?}` → `{value, source: default|user|repo|cli}` |
| `config.set` | `{key, value, persist?: false}` → `{}` — runtime override |
| `config.validate` | `{path?}` → `{errors: [{line, col, message}]}` |
| `search.query` | `{q, scope?: {workspace?, pane?, run?}, sources?: [scrollback, transcript, events], limit?: 50, regex?: false}` → `{hits: [{pane, run?, source, line, text, ts, context}]}` — FTS5 over archive + transcripts |
| `layout.list` / `layout.get` | `{}` → `{layouts: [{name, description, cwd, tabs, panes, valid}]}`; `{name}` → `{layout}` — named layouts from `[layouts.<name>]` [M4] |

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

`search.query` takes `{q | text, pane?, workspace?, machine?, since?: epoch-ms | "30m"/"2h"/"7d", limit?: 50, context?: 2, regex?: false, sources?: [live, archive]}`. It returns `{hits: [{pane, pane_handle, workspace, title, source: screen|scrollback|archive, live, line, text, ts?, context: {before, after}, position: {line, from, to}}], truncated}`. Live panes are searched first, newest line first. Archive hits come from FTS5, with rows still in memory reported once, as live hits. With `regex`, the archive segments of the panes in scope are scanned instead. `line` is the absolute history line, shared by the archive, the in-memory scrollback and the screen. `machine` must name this machine; other machines are reached with the global `--machine`. Transcript and event sources are not searched yet.

`pane.read {source: "archive", from?, to?, lines?: 200}` returns `{rows: [{n, text, wrapped}], text, first, end, from, to, more_before, more_after}`. It covers archive segments, then the in-memory scrollback, then the screen, at most 5000 rows per call. It also works for closed panes whose archive is still on disk (`archive_panes`, 02 §3).

Read scope (09 §5.1 rule 4) applies to `pane.read`, `pane.wait_output` and `search.query` for pane-scoped callers. `security.pane_scope.read = "workspace"` (default) \| `session` \| `self`.

### 2.14a `desk.*`, `draft.*`, `notes.*` [research R2/R3; server + CLI built, TUI not yet]

**Session desk** (R2 "Find and reopen previous work"). A conversation index separate from `search.query`/scrollback FTS: a background indexer reads the native transcripts of runs Vibeke has seen (`AgentRun.transcript_path`; Claude JSONL and Codex rollouts via the same parser as `agent.transcript`, so turn `n` matches) and, only when the user opts in, extra transcript directories per harness (`[desk] roots`). Rows are (machine, harness, native session id, repo/cwd, turn `n`, role/kind, text, timestamp, source byte offset) in an FTS5 table in `<state>/<session>/desk.db` — a derived, rebuildable file with its own connection, never written under the state lock. Indexing is incremental (size, mtime, byte offset of the last complete line, turns seen; a shrunk file is re-indexed), bounded per pass (`pass_bytes`, at most 4 MiB per file per pass) and runs every `interval_s`; a search first indexes up to 2 MiB so a just-finished turn is findable. Model reasoning is not indexed; items are capped at 8 KiB.

| Method | Params → Result |
|---|---|
| `desk.search` | `{text, repo?, harness?, since?, until?, session?, limit?: 20, sort?: relevance\|recent, fresh?: true}` → `{hits: [{session, path, harness, machine, repo, cwd, turn, role, kind, ts, offset, snippet, status: live\|resumable\|none, live: {run, pane}, resume: {argv, command, run}}], index: {sources, rows, forgotten_sessions}}`. `repo` is a directory (matched as main repository root or cwd prefix); `since`/`until` take epoch ms, `7d`/`12h`/`30m` or `YYYY-MM-DD`. **live** = a current run is in that native session; **resumable** = a resume argv is known (a stored run's, else the harness's own) |
| `desk.sessions` | `{repo?, harness?, limit?: 50}` → `{sessions: [{session, harness, machine, repo, cwd, workspace, run, first_ts, last_ts, turns, rows, paths, status, live, resume}]}`, most recent first |
| `desk.open` | `{session, turn?, path?, focus?: false}` → `{status, action: focus_live_pane\|resume_native_session\|start_new_agent_with_context, turn_items, focused, actions: [{id: focus, label: "Focus live pane"}, {id: resume_native, label: "Resume native session", command}, {id: new_agent, label: "Start new agent with context"}]}`. Focuses the live run's pane **only** with `focus: true` (an explicit user action); never resumes |
| `desk.context` | `{session, turns?: [n]\|"3-5,7", path?, objective?}` → `{package: {objective, decisions, remaining_work, selected_turns: [{turn, user, assistant, tool_calls}], repository: {repo, cwd, revision: {revision, branch, uncommitted_files}}}, text}` — deterministic (no model call): the objective defaults to the first prompt, decisions/remaining work are labelled line extracts for the user to edit, default turns are the last three |
| `desk.resume` | `{session, mode?: native, pane?, workspace?}` → **Resume native session**: `conflict:session_live` while it runs (focus it instead); `unsupported` without a resume handle; else types the resume argv into the given free pane (`conflict:pane_busy` if it has an agent) or a new tab in the session's workspace at its cwd (a stored run without `pane` keeps `agent.resume`'s behaviour) → `{mode, label, command, run}`. `{session, mode: new_agent, workspace?, start?: false, harness?, pane?, turns?, objective?}` → **Start new agent with context**: builds the `desk.context` package and saves it as a workspace draft; with `start` also starts the harness (no prompt) in a free pane → `{draft, package, sent: false, run?, pane?}`. Nothing is ever sent: the user edits and sends the draft (`draft.send`) |
| `desk.forget` | `{session}\|{repo}\|{workspace}\|{before}` → `{rows_deleted, sessions_forgotten}` — deletes index rows; sessions are tombstoned so the indexer never re-adds them; `before` deletes older rows. Event `desk.forgotten {scope} {rows, sessions}` (no text) |
| `desk.status` | `{}` → `{db, selection: {runs, roots}, exclude, retention_days, counts, sources: [{path, harness, session, origin: run\|root, repo, rows, offset, size, pending_bytes, excluded}], last_pass}` — makes source selection, exclusions and retention visible |
| `desk.index` | `{budget?}` → `{pass: {registered, bytes, rows, purged, reset, pending, errors}}` |

**Drafts composer** (R3 "Keep drafts outside the live agent input"). Persistent drafts per workspace or tracked task, stored as `draft` entities (state tables; content never enters events), plus one notes document per workspace (`workspace_notes`) that is sent only when the user includes it. Limits: 64 KiB text, 20 attachments, 200 drafts per scope.

| Method | Params → Result |
|---|---|
| `draft.create` | `{scope?: workspace\|task, id?, text, title?, attachments?: [{kind: file\|screenshot, path\|blob\|data_b64, name?, label?}], idempotency_key?}` → `{draft}`. `id` defaults to the caller's workspace. `path` must be an existing absolute path on this server's machine; `blob` is a `blob.put` hash; `data_b64` is stored now in the pane inbox (06 A11.4) — remote clients use `blob`/`data_b64` |
| `draft.update` | `{draft, text?, title?, attachments?, add_attachment?, remove_attachment?: index, expected_rev?}` → `{draft}` (`conflict:draft_changed`, `conflict:send_in_progress`) |
| `draft.get` / `draft.list` | `{draft}` → `{draft}`; `{scope?, id?, all?}` → `{drafts}` in user order (archived drafts are not listed). A send left `sending` by a server restart is reported and persisted as `delivery_unknown` |
| `draft.reorder` | `{order: [draft ids]}` → listed drafts first in that order, the rest after |
| `draft.delete` | `{draft}` → `{deleted}` |
| `draft.combine` | `{ids: [two or more of one scope], title?, separator?: "\n\n", delete_sources?: false}` → new `{draft}` with joined text, de-duplicated attachments and `combined_from` |
| `draft.check` | `{target_run, draft?}` → `{run, pane, harness, native_conversation_id, send_path: prompt_input\|open_pane_only, unsafe?, follow_up: prompt_input, steer: false, hidden_attachments}` |
| `draft.send` | `{draft, target_run, idempotency_key, include_notes?: false, keep?: false, retry_despite_unknown?}` → `{draft, send, send_path}`. Uses task messages' guarded prompt-input path (15 §9) without a tracked task: the run and its native conversation are fixed when the send starts; it requires a known native conversation, an idle run, no open interaction, no attached TUI focused on the pane, a provably empty input box and every attachment visible to the pane (`sandbox::can_see`); otherwise `conflict:send_unsafe {fallback: open_pane_to_send, draft_kept: true}` with **zero bytes written** and no attempt recorded. Delivery holds the pane input lock, rechecks, submits and is `delivered` only when a turn at or after the baseline starts in that conversation with the text; otherwise `delivery_unknown` (or `failed`). The sent text is the draft, then `Attached files:` with one path per attachment, then `Notes:` only with `include_notes`. A delivered draft is archived unless `keep`. Receipts are owner-aware (`review::receipts`); the same key replays the current outcome. After `delivery_unknown`: `conflict:reconcile_first` until `draft.reconcile`, then `retry_despite_unknown` |
| `draft.reconcile` | `{draft}` → `{send, receipt: {known, …}, may_retry, note}` — a late matching turn upgrades the attempt to `delivered`; a still-uncertain attempt is marked reconciled, which is what allows a warned retry |
| `notes.get` / `notes.set` | `{workspace?}` → `{notes: {workspace, text, rev}}`; `{workspace?, text, expected_rev?}` → `{notes}` |

Pane-scoped callers see only their own workspace's desk results, drafts and notes (`desk.search`/`desk.sessions`/`desk.context` are filtered to sources whose run was in that workspace; files found only under opted-in roots are invisible to panes). `draft.send`, `draft.reconcile`, `desk.open`, `desk.resume`, `desk.forget`, `desk.index` and `desk.status` are forbidden for pane scope (`authorize()`). Events (`draft.created/updated/deleted/reordered/sending/delivered/delivery_unknown/send_failed/reconciled`, `notes.updated`, `desk.forgotten`) carry ids, revisions, sizes and counts only. File attachments are referenced as paths on the pane's machine and checked with the sandbox visibility helper; there is no remote path translation, so a draft must live on the server that runs the target pane. Configuration: `[desk] index = true`, `roots = {}` (e.g. `claude = ["~/.claude/projects"]`, `codex = ["~/.codex/sessions"]`), `exclude = []`, `retention_days = 90`, `interval_s = 15`, `pass_bytes = 8388608`, `max_root_files = 5000`. The index is per server (machine); multi-machine desk aggregation is not built.

### 2.15 `blob.*` [M1]

| Method | Params → Result |
|---|---|
| `blob.put` | `{mime, data_b64}` (≤ 16 MiB) or `{mime, path}` (server reads local file) → `{hash, size}` |
| `blob.get` | `{hash, range?}` → `{mime, data_b64}` |
| `blob.stat` | `{hash}` → `{mime, size, created_at, refs}` |
| `blob.begin` / `.append` / `.commit` / `.abort` | Chunked upload for drops/pastes into remote panes (06 A11): `{name, size, sha256?}` → `{upload_id}` (size ≤ `paste.max_auto_bytes`); `{upload_id, offset, data_b64}` (≤ 1 MiB decoded, offset must match); `{upload_id}` → `{path}` (inbox `<blake3-12>/<name>`, 0600); bound to the opening connection, ≤ 16 concurrent, idle uploads purged after 10 min |

### 2.15a `git.*` [16 G2]

Read-only views of a pane's working tree, used by the gateway's Changes screen (16 §7.7). Git runs with fsmonitor, external diff and textconv disabled, a 5 s timeout and output caps; untracked files are read without following symlinks; secret-looking files (`.env*`, keys, credential files) are reported without content.

| Method | Params → Result |
|---|---|
| `git.status` | `{pane}` (pane scope: own pane only) or `{path}` (full scope) → `{repo_root, branch?, upstream?, ahead, behind, clean, truncated, files: [{path, orig_path?, x, y, kind: modified|added|deleted|renamed|untracked|conflicted, staged, adds?, dels?, binary, secret}]}`; `not_found:not_a_repo` outside a repository |
| `git.diff` | `{pane|path, file, staged?}` → `{file, diff, truncated, binary, untracked, secret?}`; `file` must be a relative path listed by `git.status`; diff against `HEAD` (or the index with `staged`), capped at 512 KiB |

### 2.16 `plugin.*` [M5]

| Method | Params → Result |
|---|---|
| `plugin.list` | `{}` → `{plugins: [{id, version, enabled, kind: actions|process, capabilities, status}]}` |
| `plugin.install` | `{source: "owner/repo[/subdir]"\|path\|url, ref?, accept_capabilities?: [..], trust?: scoped\|herdr_legacy}` → `{plugin, requested_capabilities}` — returns `permission_denied:capabilities_not_accepted` until confirmed; Herdr manifests use the explicit legacy trust grant in §7.7 |
| `plugin.link` | `{path}` → `{plugin}` (dev mode, hot reload) |
| `plugin.enable` / `plugin.disable` / `plugin.remove` | `{plugin}` → `{plugin}` |
| `plugin.action` | `{plugin, action, context?: {workspace?, pane?}}` → `{exit_code, stdout_tail}` |
| `plugin.action.list` / `plugin.action.invoke` / `plugin.log.list` | native aliases for the asynchronous action/log service; the compat endpoint preserves Herdr's exact params and response shapes (§7.7, §8.3), including returning a running log record before completion |
| `plugin.pane.open` / `plugin.pane.focus` / `plugin.pane.close` / `popup.close` | managed terminal entrypoints and session-modal popups; Herdr-compatible behavior in §7.7 |
| `plugin.kv.get` / `plugin.kv.set` / `plugin.kv.delete` / `plugin.kv.list` | plugin-only, scoped to caller's plugin id |
| `ui.contribute` | plugin-only — see §7.4 |

### 2.17 `integration.*` [M1]

| Method | Params → Result |
|---|---|
| `integration.list` | `{}` → `{integrations: [{harness, installed, version, files: [path], up_to_date}]}` |
| `integration.install` | `{harness, scope?: user|project}` → `{files_changed}` — writes hook configs / extension registration; idempotent; never overwrites user hooks (merges, marks its entries) |
| `integration.uninstall` | `{harness}` → `{files_changed}` |
| `integration.doctor` | `{harness?}` → `{checks: [{name, ok, detail}]}` |

---

## 3. Render stream protocol [M1]

**This section is the single normative definition of the render stream.** 01 §3.2 and 03 describe behaviour and rationale and link here; message names, fields and encodings are defined only in `vk-proto::render` and this table.

Opened by `render.attach` on a fresh connection, authenticated by the same `client.hello` identity rules as control connections (09 §3.2); after the JSON response the connection switches to binary frames. `postcard`; frame = `u32 LE length | u8 frame_type | postcard payload`.

*Version negotiation (Goal 03 review follow-up):* postcard is positional, so any change to a type reachable from `ServerFrame`/`ClientFrame` (even an appended `#[serde(default)]` field) changes the encoding and bumps the render protocol (`vk_proto::render::PROTOCOL`, now **2**: browser panes and media frames). The client sends `params.protocol`; the server refuses a different or missing one with error kind `version_mismatch` (`details {server_protocol, client_protocol, server_version}`, message with an upgrade hint) and closes the connection; the client refuses a reply whose `result.protocol` differs. There is no compatible downgrade representation: the older side must be upgraded (`vibeke machine upgrade <machine>`).

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
vibeke preview   list|declare|promote|dismiss|open|url|mirror|unmirror|forget|profile
vibeke browser   open|navigate|click|type|press|wait|eval|screenshot|snapshot|console|network|dom|close|list|status|install|take-over|release   (diff: not built)
vibeke mcp       stdio MCP server (06 B7)
vibeke image     show|upload
vibeke notification list|send|read
vibeke events    tail [--types agent.*] [--after-seq N] [--follow] | read | wait
vibeke search    <query> [--pane p] [--workspace w] [--since 2h] [--regex] [--context n] [--sources live,archive]
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

---

## 8. Herdr compatibility layer (`vk-compat`) [M5; importer in M1]

Goal: existing **Herdr plugins, CLI automation and socket clients run unmodified** against Vibeke's public compatibility surface. M5 covers the full public extension contract of the supported baseline, including the host APIs available to plugins. Import remains available in M1; the current SSH replacement goal does not acquire an M5 dependency.

### 8.0 Supported baseline and meaning of full compatibility

Initial required baseline: **Herdr v0.9.3**, tag resolved on 2026-10-06 to commit **`7b116c05bfda646af39d2524c54e70c751f57ee8`**. This pins a target for implementation; no current compatibility certification is implied. Reference material at that revision:

- [Plugin authoring contract](https://github.com/herdrdev/herdr/blob/7b116c05bfda646af39d2524c54e70c751f57ee8/docs/preview/website/src/content/docs/plugins.mdx), [plugin schema](https://github.com/herdrdev/herdr/blob/7b116c05bfda646af39d2524c54e70c751f57ee8/src/api/schema/plugins.rs), [plugin CLI implementation](https://github.com/herdrdev/herdr/blob/7b116c05bfda646af39d2524c54e70c751f57ee8/src/cli/plugin.rs).
- [Public CLI reference](https://github.com/herdrdev/herdr/blob/7b116c05bfda646af39d2524c54e70c751f57ee8/docs/preview/website/src/content/docs/cli-reference.mdx) and [socket API reference](https://github.com/herdrdev/herdr/blob/7b116c05bfda646af39d2524c54e70c751f57ee8/docs/preview/website/src/content/docs/socket-api.mdx). The compiled baseline's `herdr api schema --json`, CLI help and behavior are the final oracle; preview documentation can contain differences from the shipped binary.

At M5, check in `tests/compat/herdr/<version>/` containing the source revision, reference-binary checksums, full API schema, CLI grammar/help snapshots, manifest fixtures and a complete conformance inventory. Every public command, request, response, event, manifest field and platform branch must have an implementation mapping and tests. A single client trace or a selection of popular plugins is insufficient coverage. Valid baseline operations may not be stubbed, ignored, or rejected as `method_not_found` because Vibeke has not implemented them.

Compatibility preserves observable arguments/defaults, output and error shapes, exit codes, identifiers, lifecycle, focus/layout effects and timing contracts. Native API differences stay on the native endpoint. `herdr --version`, `api schema`, `ping` and snapshot metadata from the compatibility launcher/endpoint describe the tested emulation target consistently; `vibeke doctor` separately identifies Vibeke and reports the target and coverage status. Before the full gate passes, report partial support explicitly rather than advertising the baseline as fully supported.

**Boundary:** full compatibility here means the public plugin/automation contract. Implement public CLI operations using Vibeke's own runtime, including attachment and remote routing where exposed by the CLI. Running an original Herdr TUI binary against Vibeke, reproducing Herdr's private binary rendering/live-handoff transport, or sharing its internal session files in place is not required. Public UI-control methods and plugin-pane APIs are included. Private-transport exclusion must never be used to omit a public CLI operation. M5 certifies macOS/Linux; Windows adds the same contract in M6.

Record each supported version independently. New Herdr releases trigger schema/help/behavior diffs and a compatibility update; never widen the advertised baseline merely because a semver comparison passes. Retain fixtures for every advertised version and document explicit migration/deprecation before retiring one. `min_herdr_version` is a prerequisite check, not proof that an arbitrary historical or future plugin is compatible.

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
