# 02 — Data model and event log

The event log is the source of truth. Every table in §3 is a projection that can be rebuilt by replaying §2. Phase 2 (mobile/web, inbox, analytics) consumes exactly these types, so they are designed for that now.

## 1. Entity model

```
Machine 1─* Session 1─* Group? 1─* Workspace 1─* Tab 1─* Pane ─? AgentRun ─* Interaction
                                     │                    │          │
                                     ├─? Task ────────────┘          ├─* Turn ─* Item   (structured adapters only)
                                     │    └─ Worktree, PortRange, Env │
                                     └─* Preview ◄── discovered from pane process tree / declared
```

### 1.1 Core entities

**Machine** — `{ id, label, kind: local|ssh|quic, address, os, arch, vibeke_version, status: connected|connecting|degraded|offline, last_seen }`

**Session** — `{ id, name, machine_id, created_at, server_pid, server_version }`

**Group** — optional hierarchy for workspaces. `{ id, name, parent_group_id?, collapsed, order }`

**Workspace** — `{ id, handle "w3", name?, root_path, repo: {vcs: git|jj|none, remote_url?, default_branch?}?, group_id?, task_id?, order, created_at }`. Name defaults to repo/folder name; identity is `root_path`.

**Tab** — `{ id, handle "w3:t2", workspace_id, title?, auto_title, number, layout: LayoutNode, focused_pane_id, zoomed_pane_id?, order }`. Tabs show `number` alongside custom titles.

**LayoutNode** — `Split{dir: h|v, children: [(LayoutNode, ratio)]} | Leaf{pane_id}`; floating panes are a separate list `[{pane_id, rect%, z}]` on the tab.

**Pane** — `{ id, handle "w3:p5", tab_id, title?, cwd (tracked via OSC 7 / proc), shell_cmd, holder: {pid, socket, child_pid, fg_cmdline, exited?, exit_code?}, size, created_by: user|agent|plugin|api, created_by_ref?, unread: bool, marked_unread: bool, pinned: bool }`

**Harness** — loaded from manifests (see 04); not stored as events except `harness.manifest_loaded` for audit.

**AgentRun** — one live agent instance in a pane.
```
{ id, handle "a12", name? ([a-z][a-z0-9_-]{0,31}, unique among live runs),
  pane_id, harness: "claude"|"codex"|"pi"|"omp"|…, harness_version?,
  mode: tui|headless,
  integration: hooks|extension|rpc|app_server|acp|self_report|screen,   # highest-fidelity channel in use
  harness_session_id?, transcript_path?, resume: {argv: [..], cwd}?,
  model?, task_id?, parent_run_id? (subagent/spawned-by),
  state: AgentState, started_at, ended_at?, end_reason? }
```

**AgentState** (one machine for every harness):

| state | meaning | typical sources |
|---|---|---|
| `starting` | process launched, not yet ready for input | process, adapter `session_start` |
| `working` | model/tool loop active | adapter turn/tool events, screen spinner |
| `needs_approval` | blocked on a permission/approval Interaction | hook `PermissionRequest`, extension `tool_call` gate, app-server approval request, screen manifest |
| `needs_answer` | blocked on a question / choice / plan review Interaction | `AskUserQuestion`, `elicitation`, plan-mode review, screen |
| `idle` | ready for input, user has seen the result | adapter `stop`/`agent_end` + seen |
| `done` | ready for input, finished while unseen | as idle, before user focuses |
| `error` | turn failed (API error, rate limit, crash in loop) — process still alive | adapter error events, screen |
| `rate_limited` | waiting on provider limits; carries `resets_at?` | adapter / transcript |
| `exited` | process gone | holder |
| `unknown` | agent present, can't classify | — |

Every state value is stored as `{ state, since, source: adapter|self_report|screen|process|user, confidence: 0..1, detail? }`. **Precedence**: adapter > self_report > screen > process. A lower-precedence source may only override a higher one after the higher one has been silent for `stale_after` (per harness, default 20 s while `working`), and the UI marks such states as inferred.

**Interaction** — what an agent needs from a human. This is the core Phase 2 object; Phase 1 creates/answers them through TUI and CLI.
```
{ id, handle "i42", run_id, pane_id, kind: approval|question|plan_review|notice,
  status: open|answered|expired|cancelled|answered_elsewhere,
  opened_at, answered_at?, answered_by?: {client_kind, client_id, user},
  title, body_md?,
  # approval
  action?: { tool: "Bash"|"Edit"|"WebFetch"|..., summary, command?, paths?: [..], diff?: unified, url?, risk: low|medium|high|unknown, risk_reasons?: [..] },
  # question / plan_review
  questions?: [{ id, prompt, multi: bool, options: [{id, label, description?}], allow_free_text: bool }],
  plan_md?,
  # answering
  answer_channel: native|keystrokes|none,      # native = hook response / extension / rpc; keystrokes = verified key injection
  answer?: { decision?: allow|allow_always|deny, choices?: {question_id: [option_id]}, text?, scope?: once|session|rule },
  policy_match?: { rule_id, effect } }
```
`answered_elsewhere` covers the user answering directly in the agent's TUI (adapter observes the resolution).

**Turn / Item** (structured adapters only; optional for screen-only harnesses). Modeled after Codex app-server's Thread/Turn/Item and ACP so mapping is lossless:
```
Turn { id, run_id, seq, started_at, ended_at?, input_summary, status: running|completed|interrupted|failed, usage?: {input_tokens, output_tokens, cache_read, cache_write, cost_usd?} }
Item { id, turn_id, seq, kind: user_message|assistant_message|reasoning|tool_call|tool_result|file_change|command|plan|subagent|error,
       started_at, ended_at?, summary, payload_ref? }   # large payloads stored in blob store, not in the event
```
`file_change` items carry `{path, op: create|modify|delete|rename, lines_added?, lines_removed?}` — used by the collision tracker (05) and Phase 2 evidence bundles.

**Task** — a unit of work, typically one task workspace.
```
{ id, handle "k7", title, slug, workspace_id, repo_root, isolation: worktree|jj_workspace|container|none,
  worktree_path?, branch?, base_ref?, port_range?: [start, end], env_file?, setup: {script?, status, log_ref?},
  runs: [run_id], status: active|parked|finished|archived, created_at, finished_at? }
```

**Preview** — `{ id, handle "v4", machine_id, pane_id?, task_id?, origin: discovered|declared|agent, scheme, host, port, path?, pid?, process_cmd?, label?, local_url?, status: up|down, last_screenshot_ref?, first_seen, last_seen }`

**Notification** — `{ id, kind: agent_state|interaction|bell|osc9|osc777|plugin|system, subject_ref, title, body, urgency: low|normal|high, created_at, delivered: [{channel, at}], read_at? }`

**Blob** — content-addressed store (`blake3`) under `state/blobs/` for screenshots, large diffs, tool outputs, uploaded images. Events reference blobs by hash.

## 2. Event log

### 2.1 Envelope
```json
{ "seq": 18342, "ts": 1791232838418, "v": 1,
  "type": "agent.state_changed",
  "subject": { "pane": "01J…", "run": "01J…" },
  "actor": { "kind": "adapter", "id": "claude-hooks" },
  "data": { "from": "working", "to": "needs_approval", "source": "adapter", "confidence": 1.0, "interaction": "01J…" } }
```
- `seq` is a gapless per-session u64 assigned by the state actor.
- `actor.kind`: `user|client|cli|agent|adapter|plugin|system|remote`.
- Events are immutable. Corrections are new events.

### 2.2 Event types (Phase 1 set; additive later)

| Namespace | Events |
|---|---|
| `session.*` | `started`, `server_restarted {prev_pid, recovered_panes}`, `config_reloaded`, `stopped` |
| `machine.*` | `added`, `connected`, `disconnected`, `degraded {reason}`, `removed` |
| `group.*` / `workspace.*` / `tab.*` | `created`, `renamed`, `moved`, `closed`, `focused`, `layout_changed` |
| `pane.*` | `created`, `closed`, `resized`, `focused`, `title_changed`, `cwd_changed`, `process_changed {fg_cmdline}`, `exited {code}`, `bell`, `marked_unread`, `seen`, `pinned`, `recovered {method: snapshot+replay|ring_only|lost}` |
| `agent.*` | `detected {harness, via}`, `started`, `identified {harness_session_id, transcript_path}`, `state_changed`, `named`, `turn_started`, `turn_completed {usage}`, `item {kind, summary}` (sampled/compacted), `file_changed {path, op}`, `subagent_started/finished`, `resume_handle {argv}`, `exited`, `released` |
| `interaction.*` | `opened`, `updated`, `answered {by, answer, channel}`, `answer_delivered`, `answer_failed {reason}`, `expired`, `cancelled`, `resolved_elsewhere` |
| `policy.*` | `rule_added`, `rule_removed`, `rule_matched {interaction, effect}` |
| `task.*` | `created`, `setup_started/finished/failed`, `run_attached`, `status_changed`, `archived`, `collision_detected {paths, runs}` |
| `worktree.*` | `created`, `removed`, `branch_changed` |
| `preview.*` | `discovered`, `declared`, `up`, `down`, `forwarded {local_url}`, `screenshot_captured {blob}`, `console_error` (sampled) |
| `notification.*` | `created`, `delivered`, `read` |
| `plugin.*` | `installed`, `enabled`, `disabled`, `crashed`, `action_invoked` |
| `client.*` | `attached {kind, machine?}`, `detached` |

High-frequency signals (pane output, cursor moves, every streamed token) are **not** events. `agent.item` events are emitted at item granularity (start/end), never per token.

### 2.3 Subscription semantics

- `events.subscribe { after_seq?: u64, types?: [glob], subjects?: {workspace?, pane?, run?}, include_snapshot?: bool }`
- If `include_snapshot`, the server first sends `events.snapshot { at_seq, projections }`, then live events with `seq > at_seq` — atomic, no race.
- Delivery is ordered and at-least-once per connection; clients dedupe by `seq`.
- Retention: events kept 30 days or 2 M rows (configurable); `agent.item` compacted after 7 days to per-turn summaries. When `after_seq` is older than retention → `events.truncated {earliest_seq}` and the client must use a snapshot.
- Remote machines: the local server proxies subscriptions; events from a remote session are namespaced by machine and keep the remote `seq` (`{machine:"devbox", seq:…}`), so a client can resume per machine.

## 3. SQLite schema (abbreviated)

```sql
CREATE TABLE events (seq INTEGER PRIMARY KEY, ts INTEGER NOT NULL, type TEXT NOT NULL,
  subject_json TEXT, actor_json TEXT, data_json TEXT NOT NULL, v INTEGER NOT NULL);
CREATE INDEX events_type_ts ON events(type, ts);

CREATE TABLE workspaces (id TEXT PRIMARY KEY, handle TEXT UNIQUE, name TEXT, root_path TEXT, repo_json TEXT,
  group_id TEXT, task_id TEXT, ord REAL, created_at INTEGER, closed_at INTEGER);
CREATE TABLE tabs (id TEXT PRIMARY KEY, handle TEXT UNIQUE, workspace_id TEXT, title TEXT, number INTEGER,
  layout_json TEXT, floating_json TEXT, focused_pane_id TEXT, zoomed_pane_id TEXT, ord REAL, closed_at INTEGER);
CREATE TABLE panes (id TEXT PRIMARY KEY, handle TEXT UNIQUE, tab_id TEXT, title TEXT, cwd TEXT, shell_cmd TEXT,
  holder_json TEXT, cols INTEGER, rows INTEGER, unread INTEGER, marked_unread INTEGER, pinned INTEGER,
  created_by TEXT, closed_at INTEGER);
CREATE TABLE vt_snapshots (pane_id TEXT PRIMARY KEY, holder_offset INTEGER, engine TEXT, engine_version TEXT,
  blob_hash TEXT, taken_at INTEGER);
CREATE TABLE agent_runs (id TEXT PRIMARY KEY, handle TEXT UNIQUE, name TEXT, pane_id TEXT, harness TEXT,
  harness_version TEXT, mode TEXT, integration TEXT, harness_session_id TEXT, transcript_path TEXT,
  resume_json TEXT, model TEXT, task_id TEXT, parent_run_id TEXT, state_json TEXT,
  started_at INTEGER, ended_at INTEGER, end_reason TEXT);
CREATE UNIQUE INDEX agent_runs_live_name ON agent_runs(name) WHERE ended_at IS NULL AND name IS NOT NULL;
CREATE TABLE interactions (id TEXT PRIMARY KEY, handle TEXT UNIQUE, run_id TEXT, pane_id TEXT, kind TEXT,
  status TEXT, payload_json TEXT, answer_json TEXT, answer_channel TEXT, opened_at INTEGER, answered_at INTEGER);
CREATE INDEX interactions_open ON interactions(status) WHERE status = 'open';
CREATE TABLE turns (...); CREATE TABLE items (...);
CREATE TABLE tasks (...); CREATE TABLE previews (...); CREATE TABLE notifications (...);
CREATE TABLE policy_rules (id TEXT PRIMARY KEY, scope_json TEXT, matcher_json TEXT, effect TEXT,
  created_by TEXT, created_at INTEGER, hits INTEGER DEFAULT 0, last_hit_at INTEGER);
CREATE TABLE plugin_kv (plugin_id TEXT, key TEXT, value BLOB, PRIMARY KEY(plugin_id, key));
CREATE VIRTUAL TABLE scrollback_fts USING fts5(pane_id UNINDEXED, segment UNINDEXED, line_no UNINDEXED, text);
CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER);
```

Migrations are forward-only, embedded in the binary, run at server start inside a transaction; a pre-migration backup copy of `state.db` is kept (last 3).

## 4. Policy rules (Phase 1 engine, Phase 2 learning)

Approval Interactions are checked against `policy_rules` before being surfaced:

```toml
# ~/.config/vibeke/config.toml  (or .vibeke/policy.toml in a repo, which must be trusted first)
[[policy.rule]]
scope   = { workspace = "~/code/samplehub" }       # or repo remote, task, harness, global
match   = { tool = "Bash", command_regex = '^(pnpm|npm) (test|run lint)( |$)' }
effect  = "allow"                                    # allow | deny | ask

[[policy.rule]]
match   = { tool = "Bash", command_regex = 'rm -rf|git push --force|curl .*\| *(ba)?sh' }
effect  = "deny"
```

- Policy is evaluated **only for harnesses whose adapter can gate natively** (Claude `PermissionRequest`/`PreToolUse` hooks, pi/omp extension `tool_call`, Codex app-server approvals). For screen-only harnesses, policy can only *suggest*.
- Vibeke policy never *loosens* a harness's own permission config beyond what that harness's native mechanism allows; it can answer "allow" on the user's behalf only for prompts the harness actually raised.
- Every automatic decision emits `policy.rule_matched` and is visible in the pane's agent timeline.
- Phase 1 records `(interaction fingerprint → human decision)` so Phase 2 can propose rules ("you approved `pnpm test` 30× in samplehub").
