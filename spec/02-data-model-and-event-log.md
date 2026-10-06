# 02 — Data model and event log

**The SQLite state tables (§3) are the source of truth for session state.** Every session-state mutation commits in one transaction that updates those tables *and* appends the corresponding events to the `events` table (a transactional outbox). Machine/user-wide resources, including the M5 plugin registry, have separate ownership and reconciliation rules (§3, Plugin state ownership). Events exist for three jobs: (1) letting clients catch up after a disconnect without polling, (2) per-agent timelines and audit ("what happened while I was away", who answered what), (3) decision history for Phase 2 (learned policy, evidence). Events are **not** replayed to rebuild state, so pruning them never loses state. Phase 2 (mobile/web, inbox, analytics) consumes exactly these types, so they are designed for that now.

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

*Implemented (3D) in `vk-proto::entities` and `vk-server::machines`:* `Machine`/`Session` are stored as `machine` / `session_info` entities (`SessionInfo` uses `created_at_ms`); the server registers its own machine and session at every start and emits `session.started`; remotes are reported by clients with `machine.upsert` (federation stays client-owned, so the registry holds what clients report). `session.info`, `machine.list|get|upsert|remove` in 07 §2.3; `machine.added|connected|disconnected|degraded|removed` and `session.started|stopped` are emitted. The Workspace `repo {vcs, remote_url, default_branch}` and the five stored AgentRun facets remain as listed in the audit.

**Group** — optional hierarchy for workspaces. `{ id, name, parent_group_id?, collapsed, order }`. *Implemented (M4) as `{id, handle "g1", name, parent?, collapsed, order, workspaces: [workspace_id]}`: membership is the group's ordered list rather than `Workspace.group_id` (the `Workspace` struct is unchanged); events `group.created/renamed/moved/collapsed/closed`, `workspace.moved {group}`.*

**Workspace** — `{ id, handle "w3", name?, root_path, repo: {vcs: git|none, remote_url?, default_branch?}?, group_id?, task_id?, order, created_at }`. Name defaults to repo/folder name; identity is `root_path`.

**Tab** — `{ id, handle "w3:t2", workspace_id, title?, auto_title, number, layout: LayoutNode, focused_pane_id, zoomed_pane_id?, order }`. Tabs show `number` alongside custom titles.

**LayoutNode** — `Split{dir: h|v, children: [(LayoutNode, ratio)]} | Leaf{pane_id}`; floating panes are a separate list `[{pane_id, rect%, z}]` on the tab. *Implemented (M4): `Tab.floating: [{pane, x, y, w, h, z}]` + `Tab.floats_hidden`, appended to the postcard-encoded `Tab`.*

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
  state: AgentStateFacets, started_at, ended_at?, end_reason? }
```

**AgentStateFacets** — mirrors [04](04-harness-adapters.md) §2.4–2.5 exactly (04 is normative). A run's state is five independent facets, each stored as `{ value, since, source: structured|self_report|screen|process|user, confidence: 0..1, detail? }`:

| Facet | Values | Authoritative sources |
|---|---|---|
| **Process liveness** | `starting`, `alive`, `exited{code}` | holder `Status` (always authoritative) |
| **Execution state** | `starting`, `working`, `idle`, `error`, `rate_limited{resets_at?}`, `exited`, `unknown` | structured transports (hooks/extension/protocol) > self_report > screen > process |
| **Pending interactions** | list of open `Interaction` ids (approval / question / plan_review) | the `interactions` table; opened/resolved by transports, answered via the delivery transaction below |
| **Adapter health** | `healthy`, `degraded{reason}`, `disconnected`, `unvalidated_version` | adapter host; stored as `AgentRun.adapter_health` + `last_signal_at` |
| **Read state** (per client/user) | `seen`, `unseen` (since last execution change) | `pane_reads(client_user, pane_id, seen_rev)` |

- `needs_approval` / `needs_answer` are **derived for display** ("≥ 1 open interaction of that kind"), not execution states; a run can be `working` while an approval is open (e.g. a subagent continues).
- `done` is **not** an execution state: it is the UI rendering of `execution = idle` + `read_state = unseen`. Automation (`agent wait`, API) consumes execution state and pending interactions, never read state. `agent wait --until done` is an alias for "idle, and a turn completed after the wait started".

**Arbitration** (normative text: 04 §2.5): (1) process death overrides immediately and cancels open interactions; (2) open interactions stay authoritative until resolved (answer delivered, native resolution, turn end, process death, user dismissal); (3) silence is not staleness — a lower-precedence source may only *add* information (e.g. a screen-detected dialog opens a provisional, lower-confidence interaction and raises `adapter.disagreement`), never overwrite a healthy structured source; (4) only an explicit structured-transport loss (`adapter_health = disconnected`) downgrades to the next transport, marked inferred; (5) simultaneous fresh signals: structured > self_report > screen > process; (6) `unvalidated_version` caps confidence at 0.8.

**Interaction** — what an agent needs from a human. This is the core Phase 2 object; Phase 1 creates/answers them through TUI and CLI.
```
{ id, handle "i42", run_id, pane_id, kind: approval|question|plan_review|notice,
  status: open|answered|resolved_elsewhere|expired|cancelled,
  opened_at, answered_at?, answered_by?: {client_kind, client_id, user},
  title, body_md?,
  # approval
  action?: { tool: "Bash"|"Edit"|"WebFetch"|..., summary, command?, paths?: [..], diff?: unified, url?, risk: low|medium|high|unknown, risk_reasons?: [..] },
  # question / plan_review
  questions?: [{ id, prompt, multi: bool, options: [{id, label, description?}], allow_free_text: bool }],
  plan_md?,
  # answering
  answer_channel: native|keystrokes|none,      # native = hook response / extension / rpc; keystrokes = best-effort verified key injection
  native_ref?: { harness_request_id, transport },   # e.g. Codex approvalId, Claude hook invocation id, pi tool_call id
  deadline?: ts,                                     # e.g. hook timeout; after it the native channel is gone
  decision_rev: u32,                                 # increments if a decision is changed before delivery
  delivery: { state: none|decision_recorded|delivering|delivered|delivery_unknown|failed|superseded|resolved_elsewhere,   # 04 §7.3
              idempotency_key, lease_holder?, attempts, last_error? },
  answer?: { decision?: allow|allow_always|deny, choices?: {question_id: [option_id]}, text?, scope?: once|session|rule },
  policy_match?: { rule_id, effect } }
```
`resolved_elsewhere` covers the user answering directly in the agent's TUI (adapter observes the resolution).

**Answering is a two-step, recoverable transaction:**
1. `interaction.answer` records the decision (`status=answered`, `delivery.state=decision_recorded`, new `decision_rev`) in one DB transaction. First writer wins; later answers from other clients get `already_answered` with the winning decision.
2. A delivery task takes a lease (`delivering`), delivers through the native channel with the `idempotency_key`, and records `delivered` or `failed{reason}`.
3. If the server crashes between 1 and 3, the state on restart is `delivering` with an expired lease → **reconcile before retry**: ask the harness whether the native request is still pending (Codex: pending server request still open; Claude: the blocked hook shim is still connected and waiting; pi/omp: extension snapshot reports the still-open extension dialog / omp approval; pi RPC: the `extension_ui_request` id is still unanswered). Still pending → redeliver (idempotent). Resolved → mark `delivered` or `resolved_elsewhere`. Unknowable → `delivery_unknown`, surfaced to the user; never silently retried.
4. Keystroke delivery is always best-effort: verified before Enter (04 §8), but the app can change between check and keypress, so the result is `delivered` only when the adapter or screen confirms the dialog closed with the expected outcome; otherwise `delivery_unknown`.

**Turn / Item** (structured adapters only; optional for screen-only harnesses). Modeled after Codex app-server's Thread/Turn/Item and ACP so mapping is lossless:
```
Turn { id, run_id, seq, started_at, ended_at?, input_summary, status: running|completed|interrupted|failed, usage?: {input_tokens, output_tokens, cache_read, cache_write, cost_usd?} }
Item { id, turn_id, seq, kind: user_message|assistant_message|reasoning|tool_call|tool_result|file_change|command|plan|subagent|error,
       started_at, ended_at?, summary, payload_ref? }   # large payloads stored in blob store, not in the event
```
`file_change` items carry `{path, op: create|modify|delete|rename, lines_added?, lines_removed?}` — used by the collision tracker (05) and Phase 2 evidence bundles.

*Implemented (3D):* entities `Turn`/`Item`/`FileChange` in `vk-proto::entities`, stored as `stream_turn`/`stream_item` (the tracking `turn`/`tool_item` records of 15 are separate and unchanged), recorded by `vk-server::items` from the hook vocabulary every harness family ends in, with `agent.item`, `agent.subagent_started/finished`, per-turn usage as a delta of session totals, payloads in the blob store and `agent.turns`/`agent.items` to read them (07 §2.7a). The `reasoning` and `plan` kinds exist in the model but no current transport reports them. `file_change` items are recorded; the collision tracker (05 §10) does not read them yet (3A).

**Task** — a unit of work, typically one task workspace.

  *Tracking additions (spec 15 T1, implemented):* `ownership: owned | attached` (attached = tracking work in an existing pane; lifecycle actions never stop processes, close the workspace, release ports or delete files — `task.finish` only changes the record and labels it `finished_without_review` unless accepted), `owner_machine`, `intent_revision`, `priority`, `effort` (`quick|minutes|deep|unknown`), `review_label` (independent of lifecycle `status`), `rev` (expected-revision checks). Stored alongside as their own entity kinds: **TaskIntent** (`task_intent`, immutable per revision, user-confirmed only; carries a bounded verbatim source excerpt ≤ 8 KiB), **TaskRunBinding** (`task_binding`: task, run, native conversation id, half-open turn range, `active|suspended|closed`, origin edge), **Turn** records (`turn`: run, n, native conversation id, exact prompt ≤ 8 KiB, start/end) and **tool items** (`tool_item`: command, cwd, exit code, start/end — the observed-command source), **TaskMessage** (`task_message`, §9 delivery states), mutation **receipts** (`op_receipt`, keyed by idempotency key, kept 30 days), communication coverage (`task_comm`) and the observation baseline (`task_baseline`). `AgentRun.task` stays a compatibility projection; the binding history is authoritative.

  *Review additions (spec 15 T4, implemented):* **ChangeSubject** (`review_subject`) gains kind `dirty_snapshot` with `snapshot {commit, tree, staged_tree, ref_name, attempts}` — the snapshot content is an immutable Git commit (parent = HEAD at capture, fixed author/date) kept reachable by `refs/vibeke/snapshots/<commit>` in the user's repository object store (never the index, worktree or branch refs); its id hashes the commit and tree as well. Further kinds: **SnapRec** (`review_snapshot`, task, subject, HEAD, change digest, content commit, attempts, user), **ReviewNote** (`review_note`: task, subject, author (reviewer run, `agent`), run/turn/binding, severity `blocking|concern|nit|no_findings|unstructured`, text, `category: agent_claim`, classification `unassessed|blocking|not_blocking|dismissed` with the classifying user, reason and time), **ReviewerRequest** (`reviewer_request`: task, subject, harness, exact prompt and digest, `prepared|started|failed`, run, pane, `review` binding), **DependencyEdge** (`task_dependency`: task, depends_on, `blocks|related`, confirming user; removal closes the row). The review projection (`review_projection`) adds `effort_heuristic`.

*Session desk and drafts (research R2/R3, implemented):* **Draft** (`draft`: scope `workspace|task`, scope id, workspace, title, text ≤ 64 KiB, attachments `[{kind: file|screenshot, path}]`, order, `rev`, `combined_from`, the last 10 send attempts with exact text, run, native conversation, `sending|delivered|delivery_unknown|failed` state, idempotency key/owner, turn baseline and `reconciled`; delivered drafts are closed/archived) and **WorkspaceNotes** (`workspace_notes`, one per workspace, `rev`). The conversation index is *not* in `state.db`: it is the derived file `desk.db` (own schema version; FTS5 `conv_fts`, `conv_sources` cursors, `conv_forgotten` tombstones), rebuildable from transcripts, so it adds no state migration. Events `draft.*`, `notes.updated` and `desk.forgotten` are `sync` tier and carry metadata only.

Proposed extension: [15 §4 and §10](15-task-outcomes-review-and-attention.md) separates workspace ownership from attached task records, adds versioned intent and historical run bindings, and defines review/acceptance objects. Goal 01 needs no model change for this proposal. The model below remains its target; future task-history features will use the binding records specified in 15.

```
{ id, handle "k7", title, slug, workspace_id, repo_root, isolation: worktree|jj_workspace|container|none,
  worktree_path?, branch?, base_ref?, port_range?: [start, end], env_file?, setup: {script?, status, log_ref?},
  runs: [run_id], status: active|parked|finished|archived, created_at, finished_at? }
```

**Preview** — `{ id, handle "v4", machine_id, pane_id?, task_id?, origin: discovered|declared|agent, scheme, host, port, path?, pid?, process_cmd?, label?, local_url?, status: up|down, last_screenshot_ref?, first_seen, last_seen }`

**Notification** — `{ id, kind: agent_state|interaction|bell|osc9|osc777|plugin|system, subject_ref, title, body, urgency: low|normal|high, created_at, delivered: [{channel, at}], read_at? }`

**Blob** — content-addressed store (`blake3`) under `state/blobs/` for screenshots, large diffs, tool outputs, uploaded images. Events reference blobs by hash.

*Implemented (3D, `vk-store::blobs`, `vk-server::blob_store`):* one store `<state>/blobs/<h2>/<hash>.<ext>` + `<hash>.json` sidecar with `source: screenshot|inbox|payload`; uploads are ingested next to the inbox file the agent keeps its path to; tool outputs and long messages of the Turn/Item stream are `payload` blobs; `blob.stats`/`blob.gc` maintain it (07 §2.15). The diffs of review snapshots keep their own storage (the repository's object store, 15 T4), not blobs.

## 2. Event log

### 2.1 Envelope
```json
{ "seq": 18342, "ts": 1791232838418, "v": 1, "tier": "sync",
  "type": "interaction.opened",
  "subject": { "pane": "01J…", "run": "01J…", "interaction": "01J…" },
  "actor": { "kind": "adapter", "id": "claude-hooks" },
  "data": { "kind": "approval", "source": "structured", "confidence": 1.0, "native_ref": { "transport": "claude_hook", "harness_request_id": "toolu_…" } } }
```
- `seq` is a gapless per-session u64 assigned inside the mutation's transaction (outbox), so an event exists iff its state change committed.
- `tier`: `sync` (catch-up and live UI; pruned after `events.sync_retention`, default 7 days) or `history` (kept `events.history_retention`, default 1 year): `interaction.*`, `policy.*`, `task.status_changed/archived`, `agent.started/exited` + per-run summaries, `sandbox.boundary_action`, `plugin.installed`.
- `actor.kind`: `user|client|cli|agent|adapter|plugin|system|remote`.
- Events are immutable. Corrections are new events.

### 2.2 Event types (Phase 1 set; additive later)

| Namespace | Events |
|---|---|
| `session.*` | `started`, `server_restarted {prev_pid, recovered_panes}`, `config_reloaded`, `stopped` |
| `machine.*` | `added`, `connected`, `disconnected`, `degraded {reason}`, `removed` |
| `group.*` / `workspace.*` / `tab.*` | `created`, `renamed`, `moved`, `closed`, `focused`, `layout_changed` |
| `pane.*` | `created`, `closed`, `resized`, `focused`, `title_changed`, `cwd_changed`, `process_changed {fg_cmdline}`, `exited {code}`, `bell`, `marked_unread`, `seen`, `pinned`, `recovered {method: snapshot+replay|ring_only|lost}` |
| `adapter.*` | `health_changed {from, to}`, `disagreement {facet, structured, other}` |
| `agent.*` | `detected {harness, via}`, `started`, `identified {harness_session_id, transcript_path}`, `state_changed {facet, from, to, source, confidence}`, `named`, `turn_started`, `turn_completed {usage}`, `item {kind, summary}` (sampled/compacted), `file_changed {path, op}`, `subagent_started/finished`, `resume_handle {argv}`, `exited`, `released` |
| `interaction.*` | `opened`, `updated`, `decided {rev, by, answer, channel}`, `delivery_started`, `delivered`, `delivery_unknown`, `delivery_failed {reason}`, `resolved_elsewhere`, `expired`, `cancelled` (names per 04 §7.3) |
| `policy.*` | `rule_added`, `rule_removed`, `rule_matched {interaction, effect}` |
| `task.*` | `created`, `files_materialized {files, deps}`, `setup_started/finished/failed`, `setup_untrusted {repo, digest, commands}`, `agents_withheld`, `missing {path, reason}`, `recovered`, `branch_changed {expected, actual}` (05 §4); `worktree.orphan_found` (05 §4), `run_attached`, `status_changed`, `archived`, `collision_detected {paths, runs}`; tracking (15, **history** tier): `tracked {intent_revision, binding}`, `intent_updated {revision, previous}`, `binding_changed {state, reason?, from?}`, `message_prepared/sending/delivered/delivery_unknown/failed`, `finished {status, attached, reviewed}`, `updated {priority, effort, effort_source}`, `dependency_changed {action: added|removed, kind}` (T4) |
| `check.*` / `review.*` | (15 T2) `check.authorized/started/finished/cancelled/interrupted`, `review.candidate_created`, `review.accepted`, `review.invalidated`; (T4) `review.snapshot_created {head, content, ref, attempts}`, `review.reviewer_requested {subject, harness, prompt_digest, prompt_bytes}`, `review.reviewer_started`, `review.notes_recorded {turn, count, open_concerns}`, `review.note_classified` — IDs, revisions and metadata only, never prompts, logs or secrets |
| `attention.*` | `preference_changed` (**sync** tier) |
| `worktree.*` | `created`, `removed`, `branch_changed` |
| `preview.*` | `discovered`, `declared`, `up`, `down`, `forwarded {local_url}`, `screenshot_captured {blob}`, `console_error` (sampled) |
| `notification.*` | `created`, `delivered`, `read` |
| `plugin.*` | `installed`, `linked`, `unlinked`, `uninstalled`, `enabled`, `disabled`, `trust_changed`, `registry_observed {generation}`, `crashed`, `action_invoked`, `command_finished`, `capability_violation` — native events; Herdr hooks/subscriptions receive only the baseline's event projection (07 §7.7–8.3) |
| `client.*` | `attached {kind, machine?}`, `detached` |

High-frequency signals (pane output, cursor moves, every streamed token) are **not** events. `agent.item` events are emitted at item granularity (start/end), never per token.

### 2.3 Cursors and subscription semantics

- **Cursor** = `{ machine_uuid, session_uuid, log_epoch, seq }`.
  - `machine_uuid` is generated once per machine install (labels like `devbox` can change; the uuid can't).
  - `session_uuid` is generated when a named session's DB is created (two sessions on one machine never share it).
  - `log_epoch` is a random u64 stored in the DB; it changes whenever the DB is restored from backup, recreated, or `seq` could otherwise go backwards. A cursor from another epoch is never interpreted.
- `events.subscribe { cursor?, types?: [glob], subjects?: {workspace?, pane?, run?}, include_snapshot?: bool }`
- If `include_snapshot`, the server first sends `events.snapshot { at: cursor, state }` (read from the state tables in the same read transaction that determines `at.seq`), then live events with `seq > at.seq` — atomic, no race.
- Delivery is ordered and at-least-once per connection; clients dedupe by `seq`.
- When the cursor is older than retention, or its `log_epoch`/`session_uuid` doesn't match → `events.truncated { current: cursor }` and the client must take a snapshot. This is normal and cheap; it is the only recovery path clients need.
- Retention: `sync` tier pruned after 7 days (configurable, also capped at 2 M rows); `history` tier kept 1 year (configurable). `agent.item` events are `sync` tier; per-turn summaries are written to the `turns` table, which is state, not log.

  *As built (3D, `vk-config::events`, `vk-store::Store::prune_with`, `vk-server::hardening`):* `[events] sync_retention = "7d"`, `history_retention = "365d"`, `blob_retention = "30d"` (a duration or a bare number of days, at least 1) and `max_rows = 2000000` (`0` = no cap); a bad value is a located warning that keeps that key's default. The hourly sweep (`hardening::sweep`, also `storage.prune`) removes events past their tier's age, then, if the log is still over `max_rows`, the oldest `sync` rows, and `history` rows only when no `sync` row is left; it never removes the newest row, so `seq` cannot restart below its past. The same sweep removes turns (and their items) that ended before `history_retention` and collects unreferenced old blobs; it does nothing while degraded. `storage.status` reports the effective values. `sandbox.boundary_action` and `plugin.installed` are `history` tier like the other audit-grade events. Tests: `vk-store` `retention_tests`, `vk-config/tests/events.rs`, `vk-server` `hardening_tests`.
- Remote machines: the local server proxies subscriptions; each remote session keeps its own cursor (`{machine_uuid, session_uuid, log_epoch, seq}`), so a client resumes per machine independently.

## 3. SQLite schema (abbreviated)

```sql
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);   -- machine_uuid, session_uuid, log_epoch
CREATE TABLE events (seq INTEGER PRIMARY KEY, ts INTEGER NOT NULL, type TEXT NOT NULL, tier TEXT NOT NULL,
  subject_json TEXT, actor_json TEXT, data_json TEXT NOT NULL, v INTEGER NOT NULL);
CREATE INDEX events_tier_ts ON events(tier, ts);
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
CREATE TABLE pane_reads (user TEXT, pane_id TEXT, seen_rev INTEGER, seen_at INTEGER, PRIMARY KEY(user, pane_id));
CREATE TABLE port_leases (machine_uuid TEXT, port INTEGER, task_id TEXT, session_uuid TEXT, expires_at INTEGER, PRIMARY KEY(machine_uuid, port));  -- see 05; machine-wide leases are also mirrored in a machine-level lock file
CREATE TABLE plugin_kv (plugin_id TEXT, key TEXT, value BLOB, PRIMARY KEY(plugin_id, key));
CREATE VIRTUAL TABLE scrollback_fts USING fts5(pane_id UNINDEXED, segment UNINDEXED, line_no UNINDEXED, text);
CREATE TABLE archive_panes (pane_id TEXT PRIMARY KEY, workspace TEXT, tab TEXT, handle TEXT, title TEXT, updated_at INTEGER);
CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER);
```

*Archive search as implemented (M4):*
- The shipped FTS table is `scrollback_fts(pane_id, line_no, ts, text)`. There is no `segment` column: `line_no` is the pane's absolute history line, which also names the zstd segment `scrollback/<pane>/<first-line>.zst`.
- Migration 4 adds `archive_panes`, filled at each 1 Hz FTS flush. Archive hits therefore keep their workspace (for read scope, 09 §5.1) and their handle and title after the pane closes.
- Queries quote each word as an FTS5 token, ANDed together; `word*` keeps prefix matching. Filters: panes, workspaces, `ts >= since`.
- **Retention** runs from the hourly housekeeping pass (`Server::archive_retention`): per pane, the oldest closed segments beyond `terminal.archive_max_per_pane` (compressed bytes), then closed segments last written more than `terminal.archive_days` ago (file mtime); `0` disables a limit. The segment being written is never taken. The matching `scrollback_fts` rows (by `pane_id` and the segment's line range `[first_line, next_first_line)`) are deleted together with the files by a recoverable protocol (as built after leftovers review finding 8; before it, segments were unlinked before the commit, so a later failure left rows pointing at deleted history): (1) the index rows are deleted and a purge-journal row (`kv` scope `archive_purge`, key = purge id) is written in one transaction, and the segments are **renamed** into `scrollback/.trash/<purge-id>/<pane>/` (same filesystem; dot directories are not panes); (2) the transaction commits; (3) the staging dir is unlinked and the journal row removed. Any failure before the commit (a segment that can't be moved, the commit itself) moves the staged segments back and rolls the rows back, so a reported failure deletes nothing. A staging dir that can't be unlinked after the commit leaves the purge in force and is removed later. At server start (and before `doctor --rebuild-index`) `Store::recover_archive_purges` settles every staging dir: journal row present → the commit happened, the staged files go; absent → it never committed, the files move back (one whose original path is taken stays staged and is reported). Tests: failure on the second segment, commit failure, unlink failure, crash before and after the commit (`vk-store::purge` tests). A pane left with no segments also loses its remaining FTS rows and its `archive_panes` row. Implementation: `vk_store::Store::purge_archive` with `archive::Select::{All, OlderThan, OverBytes}`; it is idempotent and has a dry-run mode.
- **`vibeke forget`** (server method `scrollback.forget`, full scope only; `--pane p | --workspace w | --before <time> | --all`, exactly one) deletes, for the scope, the segment files, the matching `scrollback_fts` rows and the `archive_panes` rows of panes left with no archive, in one transaction as above, then emits `scrollback.forgotten {scope}` with counts only (`panes, segments, bytes, fts_rows, panes_dropped`; never text). Pane and workspace scopes include closed panes (workspace membership comes from live panes and `archive_panes`) and also drop the open segment's buffer and any rows still waiting for the next FTS flush (a batch the 1 Hz flush has already taken is indexed first: the flush drains and inserts under the archive lock, which purges also take — lock order archive → core → FTS buffer — so an in-flight batch can't re-insert forgotten text after the purge; leftovers review finding 7); a live pane keeps archiving new rows afterwards into a fresh segment. `--before` is segment-granular (a segment goes when its last write is older than the time; one that straddles the time stays whole, in the index too), uses the same time forms as `desk.forget` (date, RFC 3339, `7d`) and never takes a segment being written. The CLI first calls with `dry_run` to show the counts and the canonical plan (`scope` with resolved pane/workspace id or absolute `before` ms, `pane_ids`, and a `plan` digest), then asks `Delete? [y/N]` unless `--yes`, and executes exactly that plan: it sends the dry run's `scope` and `plan`, and the server refuses (`conflict`, nothing deleted) if the scope no longer resolves to the same panes and cutoff — so `--pane @focused` with focus moving while the prompt is open, a workspace that gained a pane, or a relative `--before` can't delete something other than what was shown (leftovers review finding 6) (without a terminal and without `--yes` it exits 2 and deletes nothing); `--dry-run` only reports. Repeating it is a no-op. **It does not delete**: the event log (the spec 09 wording about replacing events with tombstones is not built), blobs/screenshots, the live screen, in-memory scrollback, VT snapshots, the session desk index (`desk.forget`), drafts and notes, assistant records (`assistant.purge`), or native harness transcripts. Rotates nothing: `seq`, `log_epoch` and `session_uuid` are untouched.
- **`vibeke doctor --rebuild-index`** works offline on the session's state dir and refuses (exit 1, nothing changed) while that session's server is running (socket answers or the pidfile's process is alive) or when there is no `state.db`. That probe alone is a snapshot (a server could start right after it), so the exclusion is a lock (as built after leftovers review finding 12): `flock(LOCK_EX)` on `<state>/state.lock` (`vk_server::paths::StateLock`), which the server takes before opening `state.db` and holds for its whole life, and doctor takes after the probe and holds until the rebuild is done. Doctor refuses when the lock is held; a server that starts during a rebuild waits up to 10 s for it and then refuses to start (exit 1) — it never runs alongside the rebuild. Before re-indexing, doctor settles interrupted archive purges (`recover_archive_purges`, above). Test: `crates/vibeke/tests/scrollback_forget.rs` `doctor_rebuild_index_and_server_startup_exclude_each_other` (a server the probe misses; a server started while a paused rebuild holds the lock). In one transaction it empties `scrollback_fts` and re-indexes every `scrollback/<pane>/*.zst` segment. Rows carry no timestamp on disk, so `ts` of re-indexed rows is the segment file's modification time. `archive_panes` keeps the rows of panes that still have segments, restores missing ones from the `pane` entities in `state.db` (workspace, tab, handle, title) when the pane is still known, and drops rows of panes with no segments. A damaged segment (truncated or corrupt zstd, bad row lines) is indexed up to the damage and listed under `damaged`; one with nothing readable is left out of the index and listed under `skipped`; segment files are never modified. The report (text, or JSON with `--json`) gives panes, segments, rows indexed, the FTS and `archive_panes` counts before/after.

Migrations are forward-only, embedded in the binary, run at server start inside a transaction; a pre-migration backup copy of `state.db` is kept (last 3). Restoring a backup always rotates `log_epoch`.

*As built (3D, `vk-store::backup`):* when `Store::open` finds a database whose schema is behind the binary's (an existing database, schema version ≥ 1, pending migrations), it first writes a consistent copy with `VACUUM INTO` to `<state>/backups/state-v<schema>-<ms>.db` (0600, dir 0700) and keeps the newest three; if the copy fails the migration does not run and the server does not start (an unprotected migration is never attempted). A brand-new database takes no backup. `vibeke doctor --list-backups` lists them; `vibeke doctor --restore-backup NAME` (offline: refuses while the server runs, holds the state lock) integrity-checks the copy, keeps the replaced database as `state.db.pre-restore`, installs the backup and rotates `log_epoch` (random u64), leaving `session_uuid` and the machine uuid; the next start migrates it forward again. `storage.status` lists the backups. Tests: `vk-store::backup` unit tests, `crates/vibeke/tests/state_backup.rs`.

### Plugin state ownership [M5]

The per-user `~/.config/vibeke/plugins.json` registry is authoritative for installation source/revision, manifest location/digest, enabled state and approved native capabilities or Herdr legacy trust (07 §7.7, 09 §6). It is available without a running session and updated with a machine-wide lock and atomic replacement. Include a monotonically increasing generation; running sessions reconcile committed generations before dispatching plugin commands and recheck grants before callbacks. Session snapshots must never overwrite or resurrect revoked global grants. Offline changes are picked up at next activation; session event logs record registry observations, not a fictitious cross-session atomic commit.

Each session persists its own command records (invocation id, plugin/install identity, argv, context, start/end, status/exit, log references), plugin-pane ownership, holder/process references and broker bindings in `state.db`. Record invocation mutations with native outbox events; recovery reconnects to still-live held processes and reconciles terminal outcomes without rerunning completed actions. One-shot startup hooks follow the per-activation contract in 07. Compatibility ids and callback bindings survive normal server recovery; authority is revalidated against the current global grant before reuse. Popups retain their upstream transient semantics.

Plugin config and file-based state directories are per user and shared across sessions, separate from replaceable source checkouts. Plugins own file formats, migrations and cross-session coordination. Native `plugin_kv` remains session-scoped, keyed by plugin id; Herdr plugins have no dependency on it. Global registry and plugin-owned files are not reconstructed by replaying a session event log; backup/export includes them explicitly and import reports conflicts without modifying Herdr's source data.

## 4a. Degraded mode (disk full, I/O errors)

Honest behavior when the DB can't be written:

- A failed commit means the mutation **did not happen**: the API returns `storage_unavailable`, no event is emitted, in-memory projections are not changed.
- The server enters `degraded` (`session.degraded` is shown in the TUI status bar and returned by every API call's metadata). Panes, holders and PTY I/O keep working: typing, output and rendering never depend on the DB.
- Mutations that are pure UI convenience (focus, unread marks) are applied in memory and flagged `ephemeral`; they are lost on restart and that is acceptable.
- Interactions can still be answered **only** if the answer can be persisted; otherwise the answer is refused and the user answers in the agent's own UI. We never deliver a decision we couldn't record.
- VT snapshots and scrollback archive writes pause; the recovery guarantee degrades to "ring only" and the UI says so.
- Recovery from degraded is automatic once a probe write succeeds (every 5 s); nothing is replayed from memory.

*As built (3D, `vk-server::{core,hardening,lib}`):* a failed `Server::commit` returns `storage_unavailable` (the mutation did not happen, no event, projections untouched) and sets `model.degraded = "storage unavailable: <error>"`, which the TUI status bar renders as `⚠ storage degraded · ring only`. A `Tx` marked `ephemeral` (focus changes and unread marks on output and focus) is instead applied to the in-memory model without an event, counted (`server.status.ephemeral`, `storage.status.ephemeral`) and lost on restart; it never covers interactions, tasks, layout or anything else a restart or another client must see. `interaction.answer` while degraded is refused up front with `storage_unavailable` ("answer in the agent's own UI", `details.fallback`) before anything is recorded or delivered, and a record failure that slips through maps to the same error rather than `conflict`. `store_snapshot` returns without touching the database and `archive_rows` drops rows (counted in `archive_rows_skipped`) while degraded; `storage.prune` and the sweep refuse. Recovery: housekeeping probes with a write every 5 s (`PROBE_EVERY_MS`), and any successful commit also ends the mode; the ephemeral count resets and nothing is replayed. Test hook: `Store::set_query_only` makes every write fail like a full disk. Tests: `hardening_tests` (failed commit, ephemeral focus/unread, paused snapshots and archive, probe cadence, refused answer, `storage.*`).

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

- Policy is evaluated **only for harnesses whose adapter can gate natively** (Claude `PermissionRequest`/`PreToolUse` hooks, Codex hooks and app-server approvals, ACP `session/request_permission`). **Not pi/omp:** Vibeke has no gate there; approvals belong to the user's pi permission extension (or omp's own modes), and a policy rule can at most auto-answer that extension's dialog where the `answer_native: extension dialogs` capability exists (04 §6.3). For screen-only harnesses, policy can only *suggest*.
- Vibeke policy never *loosens* a harness's own permission config beyond what that harness's native mechanism allows; it can answer "allow" on the user's behalf only for prompts the harness actually raised.
- Every automatic decision emits `policy.rule_matched` and is visible in the pane's agent timeline.
- Phase 1 records `(interaction fingerprint → human decision)` so Phase 2 can propose rules ("you approved `pnpm test` 30× in samplehub").
