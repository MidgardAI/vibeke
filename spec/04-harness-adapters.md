# 04 — Harness adapters

> How Vibeke understands coding agents. Vibeke works out agent state from structured signals first and falls back to reading the screen only where no structured transport exists.

Status of facts: verified 2026-10-05 against installed versions **Claude Code 2.1.289**, **Codex CLI 0.157.1**, **pi 0.84.1** (`@earendil-works/pi-coding-agent`), **omp 17.2.12** (`@oh-my-pi/pi-coding-agent`). Items marked **[verify M0]** are documented upstream but not yet exercised against a live binary. They must be confirmed by the golden corpus (§12) before the adapter ships.

---

## 1. Goals and non-goals

**Goals**

1. Agent state is split into independent facets (process liveness, execution state, pending interactions, adapter health, per-client read state; §2.4). Every value carries a `source` and a `confidence` (02 §1.1 mirrors this). Where a harness exposes structured signals, Vibeke uses them, so it never guesses where it could know.
2. Approvals, questions and plan reviews become `Interaction` objects. They can be answered from any client (TUI cards for agents you're not looking at — the focused pane always shows the agent's own UI, 08 §0 — CLI, API; mobile in Phase 2). Answers go back through the harness's **native** channel when the capability matrix (§2.3) says one exists for that harness version and launch mode, and through **best-effort verified keystrokes** otherwise. Delivery is a recoverable transaction (§7.3).
3. What Vibeke can do with a harness is published as an explicit **capability matrix** (§2.3), not implied by which transport happens to be in use.
4. **Bring your own harness.** pi with personal extensions, omp, Hermes, a shell wrapper around any of these, or a brand-new CLI can all be described by a TOML manifest. They plug into the same adapter interface that the built-in adapters use.
5. Every resumable harness gets a resume handle, so panes come back after a reboot (01 §1.2).
6. When a harness ships a new version, Vibeke detects it and the golden corpus (§12) re-validates the adapter. Until it does, that version gets only the `observe` capability. No silent breakage.
7. **Heuristic identity never authorizes.** Process/screen/time correlation may *suggest* which pane a run or thread belongs to; it never authorizes answering an Interaction or writing input on the run's behalf (§2.6).

**Non-goals**

- Vibeke does not replace a harness's own permission system. It answers the prompts the harness raises. It never widens permissions beyond what the harness's own mechanism allows (02 §4).
- Phase 1 does not include rich conversation UIs for headless mode. Phase 1 ships a minimal transcript view; Phase 2 builds the rich UI on the same events.
  - *As built (v1, 2026-10-06, `agents/headless/transcript.rs`):* the transcript view of a headless pane keeps what the adapters report as entries: text, and tool calls with a status (`✓` done, `✗` failed with the exit code, `⊘` declined or interrupted), a diff (Codex `fileChange` diffs, Claude `Edit`/`Write` input, pi `details.diff` or edit input, ACP `diff` content; `+`/`-`/`@@` coloured) and output (Codex `aggregatedOutput`, tool-result text; control characters shown as `·`). Tool calls are collapsed to one summary line (`⎿ ✓ +3 −1 · 12 lines (ctrl+o to expand)`); `ctrl+o` typed in the pane toggles all of them and redraws the transcript (screen and scrollback cleared, entries drawn again, the line being typed kept). Output stays append-only otherwise. At most 4000 entries and 64 KiB per tool detail are kept; the expanded view shows up to 400 diff and 200 output lines per call.

---

## 2. Transports, capabilities and state arbitration

Earlier drafts ranked "integration tiers" by fidelity. That was wrong: hooks, extensions and RPC are **transports** with different abilities, and the same transport can be excellent for one harness version and useless for another (omp approvals are visible to its extension but not answerable through it; Claude hooks can answer approvals but answering `AskUserQuestion` is unverified). Vibeke therefore separates *how signals arrive* (transport) from *what Vibeke can do* (capabilities), and publishes the latter per harness version.

### 2.1 Transports

| Transport | `integration` value | Mechanism |
|---|---|---|
| Native hooks | `hooks` | Harness runs `vibeke hook <harness> <event>` on lifecycle events (Claude, Codex, Gemini) |
| In-process extension | `extension` | A Vibeke extension loaded by the harness talks to the socket (pi, omp, OpenCode, Hermes) |
| Headless protocol | `rpc` / `app_server` / `acp` / `stream_json` | Vibeke spawns the harness in its machine protocol and *is* the client: `pi --mode rpc`, `omp --mode rpc-ui`, `codex app-server`, `claude -p --output-format stream-json --input-format stream-json`, any ACP agent |
| Self-report | `self_report` | Agent or wrapper calls `vibeke agent report …` or the Herdr-compatible `pane.report_agent` / `pane.report_agent_session` |
| Screen manifest | `screen` | Region regex/heuristic matching on the VT grid (§9) |
| Process | `process` | Foreground process detection only |
| Transcript | `transcript` | Tail of the harness's session file (§10); history, usage, and post-hoc reconciliation |

A run normally uses several transports at once (Claude in TUI mode: hooks + transcript + screen + process). Transports are inputs to the arbiter (§2.5); none of them "wins" by rank alone.

### 2.2 Capabilities

| Capability | Meaning |
|---|---|
| `observe` | Vibeke can see execution state transitions (working/idle/…) and that an interaction is pending. |
| `gate` | Vibeke can hold a tool call or permission request until it (or policy) decides, through a channel the harness honours. |
| `answer_native` | Vibeke can deliver an answer to a pending interaction through a harness-defined channel (hook decision, extension return, protocol response). Per **interaction kind** (approval / question / plan_review). |
| `answer_keystroke` | Vibeke can attempt delivery by verified keystrokes (§8). Always **best-effort**. |
| `reconcile` | After a disconnect or server restart, Vibeke can ask the harness for the authoritative current state (pending requests, turn status) instead of inferring it. |
| `resume` | A resume argv exists and has been golden-tested. |
| `steer` | Vibeke can inject a message into a running turn (queue/steer), not just a new prompt. |
| `survive_disconnect` | The harness process and its pending interactions survive the Vibeke server going away. True for PTY-hosted TUIs (holder) and for headless processes run under a holder in **pipe mode** (01 §1.2, 07 holder protocol) once golden-tested per harness: the holder journals protocol frames and the adapter replays from its last processed offset and reconciles in-flight requests (e.g. `thread/read`, `get_state`). Built (holder/2): headless runs list `survive_disconnect` and `reconcile`; exercised with fake harnesses, not yet golden-tested against live versions. |

Capabilities are resolved per **(harness, version range, launch mode `tui|headless`, interaction kind)**. A version newer than the newest golden-tested range inherits **only `observe`** (plus `answer_keystroke` if the screen manifest still validates) until the golden corpus (§12) covers it. The resolved set is exposed in `agent.get` and the API as `run.capabilities`, and UIs only offer what is listed (e.g. no "answer" button for an omp approval from a remote client unless `answer_keystroke` is valid for the current screen).

### 2.3 Capability matrix (initial, for the versions installed 2026-10-05)

Legend: ✓ verified · ✓? documented upstream, **[verify M0]** (Claude/Codex/pi/omp; M2 for the other harnesses) · ⌨ keystroke fallback only · — not available.

| Harness / mode | observe | gate | answer_native: approval | answer_native: question | answer_native: plan_review | answer_keystroke | reconcile | resume | steer | survive_disconnect |
|---|---|---|---|---|---|---|---|---|---|---|
| Claude 2.1.x / tui (hooks) | ✓ | ✓ (PreToolUse, PermissionRequest) | ✓ | ✓? (AskUserQuestion via `updatedInput`) | ✓ (ExitPlanMode via PermissionRequest) | ✓ | — (transcript only) | ✓ `--resume` | — | ✓ |
| Claude 2.1.x / headless (stream-json) | ✓ | ✓ (hooks still fire) | ✓ | ✓? | ✓ | — | ✓? (re-read session) | ✓ | ✓? (stream-json user msgs mid-turn) | — (stdio child) |
| Codex 0.157 / tui, per-pane server (shim) | ✓ | ✓ (PreToolUse Bash only) | ✓ (PermissionRequest) | — | — | ✓ | — | ✓ `codex resume` | — | ✓ |
| Codex 0.157 / tui, shared daemon, unlinked | ✓ (display only) | — | — | — | — | ⌨ only after user links | — | ✓ | — | ✓ |
| Codex 0.157 / headless (app-server, Vibeke-owned) | ✓ | ✓ (server→client requests) | ✓ | ✓ (`requestUserInput`) | — | — | ✓ (`thread/read`, `serverRequest/resolved`) | ✓ | ✓ `turn/steer` | ✓? (if run on a socket transport Vibeke can re-attach to) |
| pi 0.84 / tui (extension) | ✓ | — (by design: no Vibeke gate for pi; approvals come from the user's permission extension) | ✓? extension dialogs (`confirm`) via shared-uiContext wrapper — relies on pi internals, per-version golden tests (§6.3); fallback: screen-inferred, jump-to-pane | ✓? extension dialogs (`select`/`input`) via the same wrapper; `editor` observe-only | — | ⌨ opt-in (`keystroke_answers`, default off) | ✓ (extension snapshot, §6.3) | ✓ `--session` | ✓ (`steer` via extension API ✓?) | ✓ |
| pi 0.84 / headless (rpc) | ✓ | — (no Vibeke gate) | ✓ (permission extension's dialogs via Extension UI Protocol) | ✓ (same) | — | — | ✓ `get_state` | ✓ | ✓ `steer`/`follow_up` | — (stdio child) |
| omp 17.2 / tui (extension) | ✓ | — (no Vibeke gate; omp has its own approvals) | ✓? via the uiContext wrapper where omp's prompt goes through `uiContext` **[verify M0]**; else jump-to-pane | ✓? (same) | — | ⌨ opt-in | ✓ (extension snapshot) | ✓ `--resume` | ✓? | ✓ |
| omp 17.2 / headless (rpc-ui) | ✓ | — | ✓? (omp approvals / extension dialogs over rpc-ui) | ✓? | — | — | ✓? | ✓ | ✓ | — |
| OpenCode / tui (plugin) | ✓? | ✓? (`permission.ask`) | ✓? | — | — | ✓ | ✓? (SDK) | ✓? | — | ✓ |
| Gemini CLI / tui (hooks) | ✓? | ✓? (`BeforeTool` deny) | — (confirmation answer unverified) | — | — | ✓ | — | ✓? | — | ✓ |
| Generic ACP / headless | ✓ | ✓ | ✓ (`session/request_permission`) | — | — | — | ✓? (`session/load`) | ✓? | — | — |
| Screen-only manifests | ✓ (confidence < 1) | — | — | — | — | ✓ | — | per manifest | — | ✓ |

The matrix is generated from manifests + golden-test results (`vibeke integration capabilities --json`) and checked into the docs on release; this table is the M1 starting point, not hand-maintained truth.

*M2 implementation status (2026-10-06):* the OpenCode, Gemini and generic-ACP rows are now backed by code, but none of OpenCode/Gemini has been run against a live binary. Their built-in manifests carry the ✓? cells as a `verified = false` capability row (reported by `vibeke integration capabilities` as "documented, unverified"; never granted) and ship **no validated range**, so every version runs `observe` + `answer_keystroke`. A user manifest may assert more (a `*` row in `~/.config/vibeke/harnesses/<id>.toml`; "user-asserted", 04 §13), which is how the OpenCode gate path is exercised in the e2e test. Generic ACP rows are granted (`observe`, `gate`, `answer_native: approval`, `answer_keystroke`) because they are protocol-level and covered by the fake-agent tests; `reconcile`/`resume` (`session/load`) are not wired.

### 2.4 State model: independent facets

02 §1.1 mirrors this model. A run's state is **not** one enum; it is five facets, each with its own source and timestamp:

| Facet | Values | Authoritative sources |
|---|---|---|
| **Process liveness** | `starting`, `alive`, `exited{code}` | holder `Status` (always authoritative) |
| **Execution state** | `starting`, `working`, `idle`, `error`, `rate_limited{resets_at?}`, `exited`, `unknown` | structured transports > self-report > screen > process |
| **Pending interactions** | list of open `Interaction` ids (approval / question / plan_review) | the interaction table; opened/resolved by transports, answered via §7 |
| **Adapter health** | `healthy`, `degraded{reason}`, `disconnected`, `unvalidated_version` | adapter host (hook shims seen recently, extension connected, protocol alive, version in validated range) |
| **Read state** (per client) | `seen`, `unseen` (since last execution change) | client focus/peek events |

Consequences:
- `needs_approval` / `needs_answer` are **derived** for display ("has ≥ 1 open interaction of kind X"), not execution states. A run can be `working` (a subagent continues) while an approval is open.
- **`done` is not an execution state.** It is the UI rendering of `execution = idle` + `read_state(client) = unseen`. Different clients can show the same run as `done` and `idle`. Automation (`agent wait`, API) consumes execution state and pending interactions, never read state; `agent wait --until done` is kept as an alias for "idle, and turn completed after the wait started".

### 2.5 Arbitration (single state machine)

The `StateArbiter` applies these rules, replacing the earlier "3 s cross-check" and "20 s staleness" rules:

1. **Process death overrides immediately.** Holder reports the child (or the matched harness process) gone → execution `exited`, all open interactions → `cancelled{reason: process_exited}`. No debounce.
2. **Open interactions are authoritative until resolved.** An interaction leaves `open` only by: an answer delivered (§7.3), a native resolution signal (Post* hook, `serverRequest/resolved`, `tool_approval_resolved`, ACP response), a turn ending, process death, or explicit user dismissal. Screen silence or a hook gap never closes one.
3. **Silence is not staleness.** A structured transport that said `working` remains authoritative while the process is alive, even if it emits nothing for minutes (long tool calls do this). A lower-precedence transport may only **add information**, never overwrite:
   - a screen match for an approval/question dialog while structured says `working` **opens a provisional interaction** (`source: screen`, confidence from the rule) and raises `adapter.disagreement`. It does not change execution state.
   - a screen `idle` match while structured says `working` changes nothing; it increments the disagreement counter only.
4. **Structured transport loss is explicit.** If a structured channel is known to be gone (extension socket closed, headless protocol EOF, adapter health `disconnected`), the arbiter downgrades: execution state is recomputed from the next available transport (self-report → screen → process), marked `inferred` in the UI, and `reconcile` is attempted if the capability exists.
5. **Precedence among simultaneous fresh signals:** structured (hooks/extension/protocol) > self_report > screen > process, compared by event time with harness `seq` where present (`seq` drop rule for self-reports).
6. **Unknown versions** (adapter health `unvalidated_version`) still run rules 1–5, but every derived state carries `confidence ≤ 0.8` and the UI shows the "?" badge.

Every transition emits `agent.state_changed {facet, from, to, source, confidence}`; disagreements emit `adapter.disagreement {facet, structured, other}` and feed drift detection (§12.3).

### 2.6 Identity binding

- A run is **bound** to a pane only by deterministic evidence: the harness was launched by Vibeke in that pane, or a signal carried that pane's `VIBEKE_PANE_TOKEN` (hooks/extension running in the pane's env), or the user linked it explicitly (`vibeke agent link`).
- Correlation evidence (process start time ≈ thread start, same cwd, thread name, screen text) produces a **suggested** binding with a confidence. Suggested bindings may drive display ("probably this pane") but **never** authorize answering interactions, sending prompts/keys on the run's behalf, or applying policy decisions. The UI offers "Link" to promote a suggestion.

### 2.7 Failure policy: observation vs enforcement

| Path | On Vibeke failure (server gone, timeout, malformed reply) |
|---|---|
| **Observation** (state signals, `observe`-mode interactions, transcript tailing) | **Fail-open**: the harness proceeds exactly as without Vibeke; the shim/extension exits or returns "no decision". |
| **Enforcement the harness itself backs** (Claude/Codex `PermissionRequest` in gate mode) | Fail to the harness's own dialog: return no decision, the native prompt appears. Safe, because the harness still asks. |
| **Enforcement only Vibeke provides** (policy `deny` rules on yolo runs via Claude/Codex pre-tool hooks; sandbox boundary actions in 13) | **Fail-closed**: deny (or pause the tool call) with reason "Vibeke unavailable — approve locally?", escalating to the harness's own prompt where its permission mode can prompt. Never silently allow. (pi/omp have no Vibeke enforcement path: see §6.3.) |

- **No silent failure.** If a harness has a structured transport available but it isn't installed or trusted (e.g. Codex hook trust), `vibeke doctor` and the sidebar show a one-time hint: "Claude in w3:p5 is screen-detected; run `vibeke integration install claude` for exact state".

---

## 3. Adapter architecture

### 3.1 Components

```
                 ┌─────────────── vk-agents (in server) ─────────────────┐
 pane bytes ──►  │ ProcessWatcher ─► HarnessMatcher ─► AdapterHost        │
 holder Status   │                                  │  ├─ HooksAdapter   │◄── vibeke hook … (socket)
                 │                                  │  ├─ ExtensionAdapter◄── @vibeke/pi-extension (socket)
                 │ ScreenDetector (per pane task) ──┤  ├─ RpcAdapter      ◄─► pi/omp --mode rpc (stdio)
                 │                                  │  ├─ AppServerAdapter◄─► codex app-server (stdio/socket)
                 │ TranscriptTailer ────────────────┤  ├─ StreamJsonAdapter◄► claude -p stream-json
                 │                                  │  ├─ AcpAdapter      ◄─► any ACP agent (stdio)
                 │                                  │  └─ ManifestAdapter (self-report/screen)
                 │                                  ▼                      │
                 │                    StateArbiter (facets, §2.5 rules)    │
                 │                                  ▼                      │
                 │                 command bus → state actor → event log   │
                 └──────────────────────────────────────────────────────────┘
```

- **ProcessWatcher**: for each pane, polls the holder's `Status` (fg pgid and cmdline) on change notifications from the holder, plus every 1 s as a fallback. It walks the foreground process group's tree (macOS `proc_listchildpids`/`proc_pidpath`, Linux `/proc/<pid>/{stat,cmdline,exe}`).
- **HarnessMatcher**: matches the process tree against manifest `[detect]` rules (§5.2). The output is `agent.detected {harness, via}`.
- **AdapterHost**: owns one adapter instance per live `AgentRun`. Adapters run as tokio tasks behind a `catch_unwind` boundary (01 §7.5).
- **StateArbiter**: applies the §2.5 rules per facet and emits `agent.state_changed`. It is the only component that writes a run's state facets.
- **CapabilityResolver**: resolves `run.capabilities` from (manifest, detected version, launch mode, golden-test coverage) on run start and whenever the version or transport set changes (§2.2).
- **TranscriptTailer**: an optional per-run file tail (§10) that provides history, usage and a recovery path for any events a hook missed.
- **ScreenDetector**: runs manifest screen rules on damage, rate-limited to 10 Hz per pane (§9).

### 3.2 The `Adapter` trait

```rust
/// One instance per AgentRun. Built-in and external adapters implement the same trait;
/// external (out-of-process) adapters are wrapped by `ExternalAdapter`, which speaks the
/// same calls over JSON-RPC (§3.4).
#[async_trait]
pub trait Adapter: Send + 'static {
    /// Transport-level abilities of this adapter instance. The CapabilityResolver intersects
    /// these with the manifest's validated version ranges to produce `run.capabilities` (§2.2).
    fn capabilities(&self) -> AdapterCaps;

    /// Called once the run is created. `ctx` gives access to the pane (send input, read screen),
    /// the run record, config, the blob store, and the event sink.
    async fn start(&mut self, ctx: AdapterCtx) -> Result<()>;

    /// Inbound signal from a hook shim / extension / self-report for this run.
    async fn on_signal(&mut self, sig: AdapterSignal) -> Result<SignalReply>;

    /// Screen detector produced a match. Structured adapters only use it to add information (§2.5 rule 3).
    async fn on_screen(&mut self, m: ScreenMatch) -> Result<()>;

    /// Deliver a decided answer to an open Interaction. Called by the delivery transaction (§7.3)
    /// with an idempotency key; must be safe to call again with the same key after `reconcile`.
    async fn deliver(&mut self, interaction: &Interaction, decision: &Decision, key: IdempotencyKey) -> Result<DeliveryOutcome>;

    /// Ask the harness for authoritative state after a disconnect/restart (only if `reconcile` capability).
    /// Returns pending native requests (with native ids), turn status, and whether a given decision was applied.
    async fn reconcile(&mut self) -> Result<Option<ReconcileReport>>;

    /// Send a user prompt (agent.prompt). Headless adapters use the protocol; TUI adapters
    /// paste with bracketed-paste awareness + Enter.
    async fn prompt(&mut self, p: PromptInput) -> Result<()>;

    /// Interrupt current turn (Esc / turn/interrupt / abort / session/cancel).
    async fn interrupt(&mut self) -> Result<()>;

    /// Return argv to resume this run after a reboot, if known.
    fn resume_handle(&self) -> Option<ResumeHandle>;

    /// Graceful stop (pane closing, run released).
    async fn stop(&mut self, reason: StopReason) -> Result<()>;
}

pub struct AdapterCaps {
    pub transports: Vec<Integration>,              // hooks | extension | rpc | app_server | acp | stream_json | self_report | screen | process | transcript
    pub observe: bool,
    pub gate: bool,
    pub answer_native: InteractionKinds,           // per kind: approval / question / plan_review
    pub answer_keystroke: bool,                    // best-effort, §8
    pub reconcile: bool,
    pub resume: bool,
    pub steer: bool,
    pub survive_disconnect: bool,
    pub turns_items: bool,                         // emits Turn/Item
    pub usage: bool,                               // emits token/cost usage
    pub subagents: bool,
}

/// What clients see: the resolved, version-aware capability set (§2.2–2.3).
pub struct RunCapabilities { pub caps: AdapterCaps, pub validated: bool, pub version_range: Option<String>, pub mode: LaunchMode }

pub enum AdapterSignal {
    SessionStarted { harness_session_id: String, transcript_path: Option<PathBuf>, source: StartSource, model: Option<String> },
    TurnStarted { turn_ref: Option<String>, prompt_preview: Option<String> },
    TurnEnded { turn_ref: Option<String>, stop_reason: Option<String>, last_message_preview: Option<String> },
    ToolStarted { call_id: String, tool: String, input: serde_json::Value },
    ToolEnded { call_id: String, tool: String, ok: bool, summary: Option<String> },
    FileChanged { path: PathBuf, op: FileOp },
    InteractionOpen { kind: InteractionKind, native_ref: String, payload: InteractionPayload, gate: GateMode },
    InteractionResolved { native_ref: String, resolution: Resolution },   // answered in the harness itself
    Notice { kind: NoticeKind, message: String },                         // idle_prompt, auth, rate limit…
    Usage { input: u64, output: u64, cache_read: u64, cache_write: u64, cost_usd: Option<f64>, model: Option<String> },
    RateLimited { resets_at: Option<DateTime<Utc>>, scope: Option<String> },
    Error { kind: String, message: String, retrying: bool },
    SubagentStarted { native_id: String, kind: Option<String> },
    SubagentEnded { native_id: String },
    Compacting { phase: Phase },
    SessionEnded { reason: String },
    Raw { harness_event: String, payload: serde_json::Value },            // kept for golden tests / debugging only
}
```

`SignalReply` is how an adapter returns a synchronous decision to a blocking hook or extension: `Continue`, `Decision(HookDecision)`, or `Wait(WaitToken)`. The shim keeps its connection open until it receives the decision or reaches its timeout (§7.3–7.4).

### 3.3 Run lifecycle

```
process detected ──► AgentRun{execution: starting, liveness: alive, binding: bound|suggested}
   │  (or agent.start via API → Vibeke launches argv itself, pre-assigning session ids where the harness allows)
   ├─► SessionStarted        → agent.identified {harness_session_id, transcript_path}; resume handle computed; execution idle
   ├─► TurnStarted/ToolStarted → execution working
   ├─► InteractionOpen        → pending interactions += i42 (+ interaction.opened); execution unchanged
   ├─► InteractionResolved / delivery confirmed → pending interactions -= i42
   ├─► TurnEnded              → execution idle; read_state = unseen for every client not currently viewing the pane
   ├─► Error{retrying:false}  → error ;  RateLimited → rate_limited
   ├─► holder: process exited → liveness exited, execution exited, open interactions → cancelled (immediate, §2.5 rule 1)
   └─► harness replaced in same pane (e.g. user quits claude, starts codex) → old run ended(released), new run
```

*Implemented for headless runs (Batch 1 lane 1A, `agents/headless/`):* the lifecycle above is driven by the protocol adapter instead of process detection: the run is created at launch with `integration = headless:<protocol>` and the headless capability set, identified from the pre-assigned id (Claude, pi) or the harness's first report (Codex thread, ACP session), and ended when the holder reports the child's exit (`SessionEnd` + `agent.exited`). Process detection and screen evaluation skip pipe panes. A run whose holder was lost (reboot) is offered for resume like a TUI run; `agent.resume` starts a new headless process continuing the session (`--resume`, `thread/resume`, `--session`, `session/load`).

**Identity.** `AgentRun` identity is `(pane, harness, harness_session_id)`. If `SessionStart` reports a different session id in the same process (e.g. `/clear`, `/new`, `/resume`, fork), the run is **kept** and `agent.identified` is emitted again with the new id. The resume handle is updated and `previous_session_ids` is appended. A new run is only created when the harness process changes.

**Pre-assigned session ids.** When Vibeke launches a harness itself (`vibeke agent start`, task workspaces, reboot resume), it pre-assigns the id wherever the harness allows: `claude --session-id <uuid>`, `pi --session-id <id>`. The run is then identified before the first hook fires, and the resume handle exists from second zero.

### 3.4 External adapters

Some harness authors want deeper integration than a manifest allows but don't want to write Rust in-tree. They can ship an **external adapter**: a process started by the server that speaks JSON-RPC over stdio.

- The server sends `adapter.start {run, manifest, pane}`, `adapter.screen`, `adapter.answer`, `adapter.prompt`, `adapter.interrupt` and `adapter.stop`.
- The adapter sends back `adapter.signal {…AdapterSignal}`, `adapter.pane.send_keys`, `adapter.pane.read`, and `adapter.log`.
- It is declared in the manifest as `[adapter] kind = "external", command = ["my-adapter"]`.
- It is limited to the runs of the harness it declares. The capability model matches plugins (07).

---

## 4. Environment and the adapter channel

Injected into every pane (01 §3.3):

```
VIBEKE=1  VIBEKE_SOCKET=…/vibeke.sock  VIBEKE_PANE_ID=w3:p5  VIBEKE_PANE_ULID=01J…
VIBEKE_WORKSPACE_ID=w3  VIBEKE_TAB_ID=w3:t2  VIBEKE_SESSION=default  VIBEKE_BIN=/…/vibeke
VIBEKE_TASK_ID=k7 (if any)  VIBEKE_PANE_TOKEN=<pane token, see 09 §3.2>
# compat (compat.herdr_env = true, M5 with the compat socket). Goal 01: outer HERDR_* vars are
# stripped from pane env instead — inherited from a Herdr pane they would make Herdr's own hooks
# report into the live Herdr session:
HERDR_ENV=1  HERDR_PANE_ID=w3:p5  HERDR_SOCKET_PATH=…/herdr-compat.sock  HERDR_WORKSPACE_ID=w3  HERDR_TAB_ID=w3:t2
```

- `VIBEKE_PANE_TOKEN` (format, entropy and lifetime defined once in 09 §3.2) authenticates adapter calls as coming from *that pane's* process tree. It is a **binding and anti-mistake mechanism**, not containment: any same-UID process can read it from the pane's environment. Real containment comes only from execution isolation (13). The token stops one pane's agent from posting states for another pane by mistake (e.g. a stale env in a nested shell) and establishes deterministic identity binding (§2.6).
- Adapter methods on the control API live in a separate `adapter.*` namespace. They require the token, except `adapter.report_self`, which can also be called with an explicit `--pane` by the user.
- **Retrieving vs authorizing.** A hook shim or extension may *wait for and retrieve* the decision on an interaction its own pane opened (`adapter.gate`, `adapter.gate_resume`). It can never *submit* a decision: `interaction.answer` rejects any caller holding pane scope for the interaction's own pane or any pane (09 §5).

### 4.1 Adapter API methods (subset of 07)

| Method | Purpose | Blocking |
|---|---|---|
| `adapter.signal {token, harness, signal}` | Deliver any `AdapterSignal` | No (ack < 1 ms) |
| `adapter.gate {token, harness, interaction, native_ref}` | Open an interaction (or re-attach to it by `native_ref`) and wait for the decision | Yes, until decision, `no_decision`, or `timeout_ms` |
| `adapter.gate_resume {token, interaction, delivery_lease?}` | Re-attach after a dropped connection; returns the recorded decision if one exists | Yes |
| `adapter.delivery_ack {token, interaction, idempotency_key, applied: bool}` | Shim/extension confirms it handed the decision to the harness | No |
| `adapter.snapshot {token, harness, snapshot}` | Full current state from an extension after (re)connect (§6.3) | No |
| `adapter.report_self {pane?, harness, state, message?, seq, resume_argv?}` | Self-report transport | No |
| `pane.report_agent` / `pane.report_agent_session` | **Herdr-compatible** self-report, accepted on the compat socket and the main socket with Herdr's param names (`pane_id, source, agent, state: idle\|working\|blocked, message, seq, agent_session_id, agent_session_path, session_start_source`) | No |

`seq` handling for compat reports: reports with `seq` ≤ the last seen seq from the same `source` are dropped. This keeps existing Herdr integrations (including Herdr's own omp extension, which debounces states with a time-based `seq`) working unchanged. Herdr's `blocked` opens a provisional `approval` interaction with `confidence 0.8` (source `self_report`), because self-reports can't distinguish approvals from questions; it is answerable only via `answer_keystroke`.

*Implemented (M2, `agents/selfreport.rs`):* `pane.report_agent`, `pane.report_agent_session` and `adapter.report_self` on the **main** socket with Herdr's parameter names (`pane_id, source, agent, state, message, seq, agent_session_id, agent_session_path, session_start_source`). Seq drop per `(pane, source)`; `blocked` → provisional approval (source `self_report`, confidence 0.8, keystroke-answerable only when the harness's screen manifest has dialog rules), closed by the next non-blocked report from that source; a live structured transport is never overwritten (an `adapter.disagreement` is emitted instead, §2.5 rule 5). Herdr agent names map to harness ids (`claude-code` → `claude`, unknown → `generic-repl`); a report without a run creates one with `integration = self_report`. Pane-scoped callers may only report for their own pane. The outer `HERDR_*` env stays stripped: Herdr's own scripts still no-op in Vibeke panes until the M5 compat socket exports `HERDR_SOCKET_PATH`; today a Herdr-style integration reports by calling the methods above (`vibeke api call pane.report_agent …`).

---

## 5. Harness manifests

Every harness, built-in or user-defined, is described by a TOML manifest.

- **Built-ins** are compiled into the binary and also written to `~/.local/share/vibeke/harnesses/builtin/*.toml` for reference.
- **User manifests** live in `~/.config/vibeke/harnesses/*.toml` and can override a built-in by `id`, or extend it with `extends = "pi"`.
- **Repo-local manifests** (`.vibeke/harnesses/*.toml`) load only for trusted repos.
- **Updated manifests** arrive through the signed manifest channel (§13).

*Implemented (M2):* the schema below lives in `vk-agents::manifest` (serde model, loader, detection, screen engine, key planner); built-ins are `crates/vk-agents/harnesses/*.toml` (`claude`, `codex`, `pi`, `omp`, `opencode`, `gemini`, `hermes`, `acp`, `generic-repl`) plus the `examples/espi.toml` user example. Deviations and details:
- **User dir** = `harnesses/` next to the config file (`VIBEKE_CONFIG` honoured). A user manifest with a built-in id **deep-merges** over it (same semantics as `extends`), so overriding one field keeps the rest; `extends` replaces the parent's `[detect]` (identity belongs to the child).
- **Repo manifests** load only while the repo's `.vibeke/` digest is trusted (`policy.trust`, the same digest as setup scripts), are re-checked after `policy.trust`, detect only inside that repo, and are namespaced **`repo:<id>`** so they can never override or shadow another id (09 §4 rule 4).
- **Code-backed ids** (`claude`, `codex`, `pi`, `omp`) keep detection, screen evaluation and version gating in code (`agents/harness.rs`, `agents/screen.rs`); their manifests document the same data, are the `extends` bases for user wrappers, and the golden replay asserts the manifest copy agrees with the code path (§12). Everything else (`opencode`, `gemini`, `hermes`, `acp:*`, user/repo manifests) is manifest-driven: `Harness::Custom(ManifestRef)` indexes a process-wide registry; a custom manifest inherits its `extends` root's signal mapping ("family"), so `espi`'s pi extension (reporting `harness: "pi"`) keeps the `espi` run.
- Extra fields beyond the schema below: `[launch] transport = "pty"|"acp"` and `acp_argv` (how to start the harness as an ACP agent), `[[capabilities]] verified = false` (✓? rows: reported, never granted) and `golden_run` (remote attestation), `[yolo] flags/pairs/always`, `[screen] rows`, `[[screen.rules]] title_regex/command_regex/rows`, `[rules.dialog] confirm` (key after an accelerator). Not implemented: `[identity]`, `[transcript]` driven by manifests, `[answer]`, `[ui] slash_commands_from`, `[adapter] kind = "external"` (§3.4), `env` detection markers, `cursor_in_region`/`style`/`osc_133` screen matchers.

### 5.1 Schema (abbreviated; the full JSON Schema is generated from `vk-agents::manifest`)

```toml
schema = 1
id = "claude"                       # [a-z][a-z0-9_-]{0,31}; unique
name = "Claude Code"
extends = ""                        # optional parent id; tables deep-merge, arrays replace
min_version = "2.1.0"               # below: capabilities reduced to observe (+ answer_keystroke if screen rules validate), doctor warning
icon = "✳"                          # sidebar glyph
color = "#d97757"

[detect]
# A run is detected when ANY process in the pane's foreground process tree matches ANY rule.
# Rules are evaluated cheapest-first; first match wins; `priority` breaks ties between manifests.
priority = 100
[[detect.process]]
exe_basename = ["claude"]                            # basename of resolved executable
[[detect.process]]
argv0_basename = ["claude"]                          # argv[0], for wrappers that exec -a
[[detect.process]]
interpreter = ["node", "bun"]                        # node/bun shim: interpreter + script path match
script_regex = '(@anthropic-ai/claude-code|/claude-code/).*(cli|claude)\.(m?js|cjs)$'
[[detect.process]]
exe_path_regex = '/nix/store/[^/]+-claude-code-[^/]+/'  # nix wrappers: match store path,
                                                     # also follows `.claude-wrapped` → real exe
[[detect.process]]
env = { CLAUDECODE = "1" }                           # env marker on descendants (read from /proc or KERN_PROCARGS2; best-effort)

[version]
command = ["claude", "--version"]                    # run once per exe path+mtime, cached
regex = '^(?P<v>\d+\.\d+\.\d+)'

[launch]
argv = ["claude"]
# Placeholders: {session_id} {cwd} {prompt} {model} {name} {resume_id}
argv_with_session = ["claude", "--session-id", "{session_id}"]   # used when Vibeke pre-assigns ids
session_id_format = "uuid"                           # uuid | ulid | none (harness chooses)
prompt_arg = "trailing"                              # how to pass an initial prompt: trailing | flag:--prompt | stdin | none
env = {}                                             # extra env for launched runs
ready = { screen_rule = "input_box", timeout_ms = 30000 }  # when `agent.start` returns

[resume]
argv = ["claude", "--resume", "{resume_id}"]
continue_argv = ["claude", "--continue"]             # fallback when id unknown but cwd known
needs_cwd = true

[identity]
# Where session ids / transcripts come from, in order of preference.
sources = ["hook:SessionStart", "preassigned", "transcript_scan"]
transcript_glob = "~/.claude/projects/{cwd_slug}/{session_id}.jsonl"
cwd_slug = "replace:/,-;replace:.,-"                 # path → slug transform

[integration]
transports = ["hooks", "transcript", "screen"]        # transports used in TUI mode (all run together, §2.1)
install = "builtin:claude"                            # installer id (§11) or ["cmd", …] for custom
headless = "stream_json"                              # which headless adapter, if any

[[capabilities]]                                      # §2.2; only ranges covered by the golden corpus are listed
versions = ">=2.1.0, <2.2.0"
mode = "tui"
observe = true
gate = true
answer_native = ["approval", "plan_review"]           # "question" added once AskUserQuestion delivery is golden-tested
answer_keystroke = true
resume = true
survive_disconnect = true

[states]
# Maps harness-native event names to execution-state transitions or interaction opens (§2.4).
# Built-in adapters ship these as code, but declaring them here keeps them visible and overridable.
"hook:UserPromptSubmit" = "working"
"hook:PreToolUse"       = "working"
"hook:PermissionRequest"= "open:approval"
"hook:Stop"             = "idle"
"hook:StopFailure"      = "error"
"hook:SessionEnd"       = "session_ended"           # execution becomes exited only when the holder reports the process gone

[interactions]
# Tool names that mean "question" or "plan review" rather than "approval".
question_tools = ["AskUserQuestion"]
plan_tools     = ["ExitPlanMode"]

[answer]
keystrokes = "screen:claude"                         # screen manifest providing dialog geometry for the best-effort fallback

[screen]
manifest = "claude"                                   # §9 screen manifest id (same file or screen/*.toml)

[transcript]
format = "claude_jsonl"                               # claude_jsonl | codex_rollout | pi_jsonl | omp_jsonl | none | external:cmd
usage = true

[ui]
interrupt_keys = ["esc"]
quit_keys = ["ctrl+c", "ctrl+c"]
slash_commands_from = "transcript"                    # Phase 2 quick-actions source
```

### 5.2 Detection details

- **Process tree walk.** Start at the holder's child, follow the foreground process group (`tcgetpgrp` on the PTY), and include every descendant of the foreground group leader up to depth 6. Harnesses running under `npx`, `bunx`, `uvx`, `mise exec`, `direnv exec`, `nix run` or a `bash -c` wrapper are matched on the descendant that satisfies a rule.
- **Wrapper unwrapping.**
  - nix `makeWrapper` binaries (`.foo-wrapped`) are followed by reading the wrapper's `exec` target from the binary's embedded string, or the `.…-wrapped` sibling.
  - Shebang scripts are matched on the interpreter plus the script path.
  - `exe_path_regex` is checked against the canonicalized exe path. This detects nix-wrapped Claude.
- **Detection cost.** The walk runs on holder fg-change notifications (the holder watches `tcgetpgrp` on read wakeups) and every 1 s otherwise. Budget: < 0.2 ms per pane per walk on Linux, < 1 ms on macOS (libproc). One `proc_pidinfo` per process; results cached by `(pid, start_time)`.
- **Ambiguity.** When two manifests match, the one with higher `priority` wins. For example, a user manifest `hermes-pi` that extends `pi` and matches a wrapper script name gets priority 200 over pi's 100. Ties log a doctor warning.
- **Shared daemons.** Some harnesses run their model loop outside the pane. Codex ≥ 0.15x can attach its TUI to a shared app-server daemon (§6.2). Manifests declare `[detect] remote_loop = "codex_daemon"` so that "the agent is in this pane" does not imply "the hooks fire with this pane's env".
- *Implemented (M2):* detection is a best match over the whole foreground tree: a `vibeke acp-host --harness <id>` host wins outright, then manifests with `priority > 100` (wrappers like `espi` whose real harness is a descendant), then the code-backed built-ins (priority 100), then the remaining manifests. Rules match `exe_basename`/`argv0_basename` also against a shell- or interpreter-run script's name (`sh ./opencode`, `node …/opencode`), `interpreter` accepts versioned names (`python3.12`), `argv_regex` runs on the space-joined argv. nix `.…-wrapped` unwrapping beyond basename/`exe_path_regex` is not implemented.

### 5.3 User-defined harness examples

**A personal pi wrapper** (`~/bin/espi` runs `pi --models anthropic/*,openai/* -e ~/pi-ext/review.ts "$@"`):

```toml
schema = 1
id = "espi"
name = "pi (the maintainer)"
extends = "pi"                          # inherits identity, transcript, extension integration, screen manifest
[detect]
priority = 200
[[detect.process]]
argv_regex = '(^|/)espi( |$)'           # matches the wrapper shell process; pi itself is its descendant
[launch]
argv = ["espi"]
[resume]
argv = ["espi", "--session", "{resume_id}"]
```

**Hermes** (Nous Research Hermes Agent: Python, with a plugin system and an ACP platform). Vibeke ships a Hermes plugin that reports full state.

```toml
schema = 1
id = "hermes"
name = "Hermes Agent"
[[detect.process]]
interpreter = ["python", "python3"]
script_regex = '(/hermes(_agent)?/|/bin/hermes$)'
[[detect.process]]
exe_basename = ["hermes"]
[launch]
argv = ["hermes"]
[resume]
argv = ["hermes", "--resume", "{resume_id}"]   # [verify M2]
[integration]
transports = ["extension", "screen"]
install = "builtin:hermes"                      # drops ~/.hermes/plugins/vibeke-agent-state/
headless = "acp"                                # hermes acp mode if available [verify M2]
# no [[capabilities]] entry yet → observe only until golden-tested
[screen]
manifest = "generic-repl"
```

**A completely custom CLI with no integration**, screen-only plus self-report:

```toml
schema = 1
id = "mybot"
[[detect.process]]
exe_basename = ["mybot"]
[launch]
argv = ["mybot", "chat"]
[integration]
transports = ["self_report", "screen"]   # mybot calls `vibeke agent report --state working` itself
[screen]
manifest = "mybot"             # defined inline below
[[screen.rules]]
id = "approval"
opens = "approval"                       # screen rules open (provisional) interactions; they don't set execution state
region = { rows = "-12..-1" }
all = ['(?i)allow (this|command)\?', '\[y/N\]']
confidence = 0.85
interaction = { kind = "approval", options = [{ id = "allow", keys = ["y", "enter"] }, { id = "deny", keys = ["n", "enter"] }] }
```

---

## 6. Built-in harnesses

Each subsection lists: manifest highlights, the event-to-state mapping, how interactions are opened and answered, TUI mode versus headless mode, and known pitfalls.

### 6.1 Claude Code (`claude`)

**Transports:** hooks + transcript + screen in TUI mode; stream-json in headless mode (Agent SDK in Phase 2). Capabilities: §2.3.

**Hooks installed** (by `vibeke integration install claude`, §11). Every hook is `{"type":"command","command":"<VIBEKE_BIN> hook claude <Event>","timeout":N}`, merged into `~/.claude/settings.json`:

| Hook event (2.1.289) | Matcher | Mode | Vibeke signal → state |
|---|---|---|---|
| `SessionStart` | `*` (startup/resume/clear/compact/fork) | async | `SessionStarted{session_id, transcript_path, source}` → execution `idle`; identity + resume handle |
| `UserPromptSubmit` | — | async | `TurnStarted{prompt_preview}` → `working` |
| `PreToolUse` | `*` | **sync** (gate-capable, timeout 600 s) | `ToolStarted` → `working`. For `AskUserQuestion` → question Interaction (§6.1.2). For policy-matched tools → native decision (§7) |
| `PermissionRequest` | `*` | **sync** (gate-capable) | `InteractionOpen{approval}` (native ref = `tool_use_id`). For `ExitPlanMode` → `plan_review` |
| `PermissionDenied` | — | async | resolves interaction (denied by auto mode) |
| `PostToolUse` / `PostToolUseFailure` | `*` | async | `ToolEnded`; `Edit`/`Write`/`MultiEdit`/`NotebookEdit` → `FileChanged` from `tool_input.file_path`; resolves any open approval for that `tool_use_id` as `resolved_elsewhere` if Vibeke didn't answer it |
| `Notification` | `permission_prompt`, `idle_prompt`, `elicitation_dialog`, `agent_needs_input`, `agent_completed`, `quota_auto_resume_*`, `auth_success` | async | cross-check (`permission_prompt` with no open interaction → open one from screen extraction); `idle_prompt` → execution `idle`; `quota_auto_resume_*` → `rate_limited` |
| `Stop` | — | async | `TurnEnded{stop_reason, last_assistant_message preview}` → execution `idle` (read state `unseen` for non-viewing clients) |
| `StopFailure` | `*` (rate_limit, overloaded, authentication_failed, server_error…) | async | `rate_limit` → `rate_limited`; others → `error` |
| `SubagentStart` / `SubagentStop` | `*` | async | `SubagentStarted/Ended{agent_id, agent_type}` |
| `PreCompact` / `PostCompact` | `*` | async | `Compacting{start/end}` (state stays `working`, detail "compacting") |
| `CwdChanged` | — | async | updates run cwd (pane cwd still comes from OSC 7) |
| `WorktreeCreate` / `WorktreeRemove` | — | async | `worktree.*` events linked to the task (05) |
| `Elicitation` | `*` | **sync** | MCP elicitation → question Interaction (native answer via hook output) **[verify M0]** |
| `SessionEnd` | `*` | async (1.5 s budget) | `SessionEnded{reason}`; run → `exited` once process exits, or stays if `reason=clear` |

"async" means the shim sends the signal and exits immediately. It does not use Claude's `async: true`, because ordering matters and our shim is fast enough (§7.5). "sync" means the shim may hold the hook open while waiting for a decision (§7.3–7.4).

**Common payload fields used:** `session_id`, `transcript_path`, `cwd`, `hook_event_name`, `permission_mode`, `agent_id`/`agent_type` (present inside subagents), `tool_name`, `tool_input`, `tool_use_id`, `prompt_id`.

#### 6.1.1 Approvals (native)

- `PermissionRequest` → Interaction `approval` with:
  - `action.tool = tool_name`;
  - `command` from `tool_input.command`, `paths` from `tool_input.file_path`, `diff` computed for `Edit` (`old_string`/`new_string`) and `Write`;
  - `risk` from §7.6.
- The answer is written as hook output (shape per the current hooks reference; `updatedPermissions` is an **array of typed permission-update entries**):

```json
{"hookSpecificOutput":{"hookEventName":"PermissionRequest",
  "decision":{"behavior":"allow",
    "updatedPermissions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"pnpm test:*"}],
                           "behavior":"allow","destination":"session"}]}}}
```

  Entry `type` values used: `addRules` (rules + behavior + destination), `setMode` (permission mode); destinations `session` (default for Vibeke), `localSettings`/`projectSettings`/`userSettings` only with consent. Exact rule-object field names are pinned by the golden corpus **[verify M0]**.
- `allow_always` maps to an `addRules` entry with `destination: "session"`. A persistent destination is used only if the user picked "always, save to settings" **and** `approvals.claude.persist_always = true`. Otherwise "always" becomes a Vibeke session-scoped policy rule (02 §4), so Claude's own settings files are never edited without consent.
- `deny` → `{"behavior":"deny","message":"<user text or 'Denied from Vibeke'>"}`. "Deny and stop" additionally sets `interrupt: true` (placement per the hooks reference **[verify M0]**).
- **Exit codes are per event.** For `PermissionRequest`, exit 2 is *not honoured*: without a JSON decision the permission flow proceeds unchanged (Claude shows its dialog). For `PreToolUse`, exit 2 blocks the tool; for `Stop`/`SubagentStop` it prevents stopping; for `UserPromptSubmit` it blocks the prompt. The shim therefore never uses exit codes to express decisions — only JSON on stdout with exit 0 (§7.5).
- If the shim times out or the server is gone, it prints nothing and exits 0. For `PermissionRequest` that means Claude shows its native dialog (harness-backed enforcement, §2.7). For a Vibeke-only `PreToolUse` **deny rule** on a yolo run, the shim fails closed: it prints a `permissionDecision: "ask"` (escalate to Claude's own prompt) when the run's permission mode can prompt, else `"deny"` with reason "Vibeke unavailable".

#### 6.1.2 Questions (`AskUserQuestion`) and plan review (`ExitPlanMode`)

- `PreToolUse` with `tool_name = "AskUserQuestion"` opens a `question` Interaction from `tool_input.questions[]` (`question`, `header`, `options[{label, description}]`, `multiSelect`).
- **Native answer** **[verify M0]**: return `permissionDecision: "allow"` with `updatedInput` containing the `answers` map in the shape the Agent SDK's `canUseTool` uses for AskUserQuestion. Until the golden corpus proves this for a version range, `answer_native: question` is **absent** from that range's capabilities and answers use best-effort keystrokes (§8) with the `claude` screen manifest's question-dialog geometry.
- `ExitPlanMode` arrives through `PermissionRequest` (or `PreToolUse`). It opens a `plan_review` Interaction with `plan_md = tool_input.plan`.
  - Approve → `behavior: allow`.
  - Reject with feedback → `behavior: deny` with `message` = the feedback. Claude stays in plan mode and revises.

#### 6.1.3 TUI vs headless

| | TUI mode (default) | Headless mode (`mode = headless`) |
|---|---|---|
| Launch | `claude [--session-id X]` in a PTY | `claude -p --output-format stream-json --input-format stream-json --verbose --session-id X [--permission-prompt-tool …]` driven by `StreamJsonAdapter` |
| State | hooks | stream events (`system/init`, `assistant`, `user`/`tool_result`, `result` with `usage`, `total_cost_usd`) |
| Approvals | hooks | hooks still installed (they fire in `-p` too); or `--permission-prompt-tool` pointed at Vibeke's MCP permission tool **[verify M0]** |
| Pane content | Claude's own TUI | Vibeke's minimal transcript renderer (01 §3.3) |
| Use | interactive work | `vibeke task run`, Phase 2 mobile-originated tasks, scripted fan-out |

*Implemented (Batch 1 lane 1A, `agents/headless/claude.rs`; fake harness only, live **[verify M0]**):* `agent.start {harness: "claude", mode: "headless"}` runs `claude -p --input-format stream-json --output-format stream-json --verbose --permission-prompt-tool stdio --session-id <pre-assigned>` (`--resume <id>` on `agent.resume`) under a pipe-mode holder (01 §1.2). Approvals use the stdio control protocol the Agent SDK uses, not hooks: `control_request {subtype: can_use_tool, tool_name, input, permission_suggestions}` → approval Interaction (`AskUserQuestion` → question, `ExitPlanMode` → plan review), native ref `rpc:<request_id>`, answered with `control_response {behavior: allow, updatedInput}` (allow-always adds the suggestions as `updatedPermissions`; deny carries a message; question answers go in `updatedInput.answers`); `control_cancel_request` resolves it; other control requests are refused. Hooks stay silent (`VIBEKE_HEADLESS_OWNER=1`). The run is identified at launch from the pre-assigned id; `system/init` re-identifies only if the id differs. Prompts are stream-json user messages (mid-turn messages queue in Claude: steer = follow-up); `agent.interrupt` sends `control_request {subtype: interrupt}`; `result` gives the turn's usage and `total_cost_usd` (treated as a per-turn delta **[verify M0]**) and `Stop`/`StopFailure`.

#### 6.1.4 Pitfalls

- `--agent` mode: AskUserQuestion can show as idle under `claude --agent` if detection relies on the screen. Hooks fire regardless, so the hooks transport fixes it. Keep a golden case for it.
- `bypassPermissions`/`dontAsk` modes: `PermissionRequest` never fires. That is expected; the run is marked `yolo: true` (13 §3) and the UI shows the permission mode badge from `permission_mode`. `PreToolUse` still fires, so Vibeke `deny` rules remain enforceable (cooperative guardrail, 13 §10).
- Subagent hooks carry `agent_id`. Interactions raised inside subagents are attributed to the subagent Item, but surface on the parent run.

### 6.2 Codex CLI (`codex`)

**Transports:** hooks + transcript + screen in TUI mode (with a per-pane embedded app-server via the PATH shim); app-server as owner in headless mode; app-server as a **display-only observer** for unlinked shared-daemon sessions. Capabilities: §2.3.

**Hook events (0.157):**
- `SessionStart`, `SessionEnd`, `SubagentStart`, `SubagentStop`, `PreToolUse` (Bash only), `PermissionRequest`, `PostToolUse`, `PreCompact`, `PostCompact`, `UserPromptSubmit`, `Stop`, `Interrupt`.
- Payloads include `session_id`, `turn_id`, `transcript_path`, `cwd`, `hook_event_name`, `model` and `permission_mode`.
- Hooks live in `~/.codex/hooks.json` (or `config.toml [hooks]`) and need `features.hooks` (stable, default on).

**Hook trust.** Codex only runs a non-managed hook after the user has reviewed and trusted its exact definition. Trust is recorded under `[hooks.state."<file>:<event>:<i>:<j>"]` in `~/.codex/config.toml`. Consequences:

- The installer writes `~/.codex/hooks.json` entries, then tells the user to run `/hooks` in Codex once to trust them. `vibeke integration status codex` reads `hooks.state` to report trusted/untrusted per hook.
- The installer never writes `hooks.state` itself. That would bypass a security control. Managed (MDM) installs are documented separately.
- Hook definitions are kept **byte-stable** across Vibeke upgrades (the command is `<VIBEKE_BIN> hook codex <Event>`, with `VIBEKE_BIN` a stable symlink `~/.local/bin/vibeke`), so trust survives upgrades.

| Hook | Signal → state |
|---|---|
| `SessionStart` | identity (`session_id` = thread id), resume handle `codex resume <id>` |
| `UserPromptSubmit` | `TurnStarted` → `working` |
| `PreToolUse` (Bash) | `ToolStarted`; policy gate for shell commands (`permissionDecision: deny` / `allow` + `updatedInput`) |
| `PermissionRequest` | approval Interaction opened; native answer `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"\|"deny","message"?}}}` |
| `PostToolUse` | `ToolEnded` |
| `Stop` | `TurnEnded` → execution `idle` |
| `Interrupt` | turn interrupted → execution `idle` |
| `SubagentStart/Stop`, `Pre/PostCompact`, `SessionEnd` | as for Claude |

**App-server protocol (verified from `codex app-server generate-json-schema`, 0.157.1):**

- **Server→client requests** (these become Interactions):
  - `item/commandExecution/requestApproval` (`approvalId, command, commandActions, cwd, itemId, threadId, turnId, reason, proposedExecpolicyAmendment, networkApprovalContext`);
  - `item/fileChange/requestApproval`;
  - `item/permissions/requestApproval`;
  - `item/tool/requestUserInput` (`questions, isBlocking, autoResolutionMs`) → `question`;
  - `mcpServer/elicitation/request` → `question`;
  - plus legacy `execCommandApproval`/`applyPatchApproval`.
- **Decisions:**
  - `accept`, `acceptForSession`, `acceptWithExecpolicyAmendment{execpolicy_amendment}` (→ `allow_always` scoped to a Codex execpolicy rule), `decline`, `cancel` (decline + interrupt turn).
  - Mapping: allow→`accept`, allow_always→`acceptForSession` (or `acceptWithExecpolicyAmendment` when the user picks "always for this command pattern"), deny→`decline`, deny+stop→`cancel`.
- **Notifications used:**
  - `thread/started`, `thread/status/changed`, `turn/started`, `turn/completed`, `item/started`, `item/completed`, `turn/diff/updated`, `turn/plan/updated`, `thread/tokenUsage/updated`, `account/rateLimits/updated`, `serverRequest/resolved` (answered elsewhere), `hook/started/completed`, `thread/compacted`, `error`, `warning`.
  - `item/*/delta` notifications are consumed for the transcript view only. They never become events.
- **Client requests used:** `initialize`, `thread/start`, `thread/resume`, `thread/fork`, `thread/read`, `thread/loaded/list`, `turn/start`, `turn/steer`, `turn/interrupt`, `account/rateLimits/read`, `review/start` (Phase 2 evidence).

**The shared daemon problem**. Codex ≥ 0.15x defaults to `daemon_auto_start = true`. The TUI connects to a per-user app-server daemon (`~/.codex/app-server-control/app-server-control.sock`; `codex app-server proxy --sock` exists in 0.157.1), and **hooks run in the daemon's environment, not the pane's**. `VIBEKE_PANE_ID`/`VIBEKE_PANE_TOKEN` are therefore missing, and several panes share one process. Vibeke binds Codex runs to panes **deterministically or not at all** (§2.6):

1. **Per-pane embedded server (primary).** Every Codex started in a Vibeke pane — by `agent start`, tasks, resume, *or the user typing `codex`* (PATH shim below) — gets `--disable daemon_auto_start`, so the TUI runs its own embedded app-server under the pane's env and hooks carry the pane token. **[verify M0]** that the flag yields an embedded server in 0.157; if not, use `-c` with the equivalent config key. This is the only mode with `gate`/`answer_native`.
2. **Daemon observer (display-only).** For a Codex that bypassed the shim (alias, absolute path, `CODEX_VIBEKE_SHIM=0`) and attached to the shared daemon:
   - The adapter connects to the daemon control socket as an additional app-server client (`initialize` with `clientInfo.name = "vibeke"`), lists loaded threads (`thread/loaded/list`), and reads each thread's state (`thread/read`). Listing threads does not subscribe to their streams; the observer explicitly attaches to each thread it wants to follow **[verify M0]** which call achieves a read-only subscription without becoming the thread's driving client.
   - **No prompt-text correlation.** `turn/started` carries an *empty* `items` list in the documented protocol, so typed input cannot be matched to threads. Start time ≈ `thread/started`, same `cwd`, and the thread `name` may produce a **suggested** binding shown as "codex (shared daemon) — probably w3:p5 · Link?".
   - A suggested or unlinked run is **display-only**: no answers, prompts, keys or policy decisions are routed through it. Interactions from its threads are listed as "unlinked" and can be answered only in the Codex TUI itself, or after the user links the run (`vibeke agent link w3:p5 --thread <id>`, or the Link button). Linking is a deterministic user act and upgrades capabilities to the app-server observer set (answering via the daemon's `serverRequest` responses becomes possible **[verify M0]** that a non-originating client may respond).
   - Hooks arriving without a pane token are attributed only to linked threads by `session_id`; otherwise they update the unlinked thread record.
3. **Screen fallback** for liveness and display, with `answer_keystroke` only on a pane the user has linked or that was started under the shim.

**PATH shim for user-typed Codex (makes strategy 1 the common case).** Vibeke prepends `~/.local/share/vibeke/shims` to `PATH` in every pane (`agents.shims = true`, default on; per-harness opt-out). The `codex` shim is a tiny exec wrapper: it locates the real `codex` later in `PATH`, adds `--disable daemon_auto_start` (unless the user passed an explicit daemon flag or `CODEX_VIBEKE_SHIM=0`), and `exec`s it with **all user arguments untouched** — so `codex -a never -s danger-full-access` behaves exactly as typed, just with a per-pane embedded server whose hooks carry `VIBEKE_PANE_ID`/`VIBEKE_PANE_TOKEN`. The same shim mechanism is available to any harness manifest (`[launch] shim_args = [...]`). `vibeke doctor` warns when an alias/function in the user's shell shadows the shim. **[verify M0]** that hooks (`SessionStart`, `PreToolUse`, `Stop`) still fire under `-a never` / `danger-full-access`; `PermissionRequest` will not, by design.

**Headless mode:** `codex app-server` owned by `AppServerAdapter`, one process per run (or one per Vibeke session, multiplexing threads, behind `agents.harness.codex.headless_shared = true`). The app-server runs under a holder in **pipe mode** (01 §1.2), so the process and its stdio protocol stream survive a server restart; on reattach the adapter replays journaled frames after its last processed offset, then reconciles with `thread/read` before re-answering any pending server→client request (delivery transaction, §7.3). Until re-attach is golden-tested for a Codex version, the capability matrix lists `survive_disconnect = false` for it and Vibeke falls back to `thread/resume` + `thread/read`.

*Implemented (Batch 1 lane 1A, `agents/headless/codex.rs`; fake app-server only, live **[verify M0]**):* one `codex app-server` per run under a pipe-mode holder. `initialize` (clientInfo `vibeke`) → `initialized` → `thread/start {cwd}` (`thread/resume {threadId}` on `agent.resume`, falling back to a new thread if resume fails) → `SessionStart` with the thread id. Prompts are `turn/start`; mid-turn `send`/`steer` use `turn/steer {expectedTurnId}`, `follow_up` queues until `turn/completed` in the session's durable queue (below). Items: `commandExecution` → Bash, `fileChange` → Edit (file path from `changes[0]`), `mcpToolCall` → its tool, `webSearch`; `agentMessage` → transcript and last message; `turn/completed` → `Stop`/`Interrupt`/`StopFailure`; `thread/tokenUsage/updated` → session-total usage. Server requests: v2 command/file/permission approvals (`accept`/`acceptForSession`/`decline`), legacy `execCommandApproval`/`applyPatchApproval` (`approved`/`approved_for_session`/`denied`), `item/tool/requestUserInput` (answers by question id) and `mcpServer/elicitation/request` become Interactions with native ref `rpc:<id>`; `serverRequest/resolved` resolves them; unknown requests get `-32601`. After a restart the adapter always sends `thread/read` (an idle thread completes a turn the journal shows running; `notLoaded` → `thread/resume`) before re-opening pending requests. The capability row of a headless run includes `survive_disconnect` because the pipe-mode holder provides it independently of the Codex version; deltas, `account/rateLimits/updated`, `cancel` (decline + interrupt) and `headless_shared` are not used.

*As built (v1 adapter items, 2026-10-06; fakes only, live **[verify M0]**):*
- **Rate limits.** `account/rateLimits/updated` (`{rateLimits: {primary, secondary}}`, each `{usedPercent, windowDurationMins, resetsAt}` with `resetsAt` in epoch seconds; snake_case accepted) sets the run's `rate_limit` to the most constrained window (`usage::app_server_rate_limit`). A window at 100 % marks the run `rate_limited` once (`agent.rate_limited`, execution `rate_limited`) and prints a note in the pane; lower snapshots only update `rate_limit`.
- **Shared app-server** (`agents.harness.codex.headless_shared = true`, `agents/headless/codex_mux.rs`): every headless Codex run keeps its own pane, pipe-mode holder, journal and adapter, but its child is a relay, `vibeke codex-mux --socket <runtime>/codex-mux.sock -- codex app-server …`, which bridges stdio to the session's mux and starts it when none answers (`--serve`, detached with `setsid`, one per socket under `codex-mux.lock`, stderr in `codex-mux.log`). The mux runs the single `codex app-server`: `initialize` reaches it once and later clients get the cached result, only the first `initialized` is forwarded, client request ids are rewritten to mux-unique ids, a thread belongs to the client whose `thread/start|resume|fork` response carried it, server requests and notifications naming a thread (`threadId`, `thread.id`, `conversationId`) go to its owner (notifications for a thread whose owner is not known yet are held, at most 256), `serverRequest/resolved` follows its request, everything else (`account/rateLimits/updated`) is broadcast, and a request for a thread with no live owner is refused with `-32603`. The mux exits 30 s after its last client and when the app-server exits (every relay then ends its run). Isolated runs never share. Because each run still sees a private stdio stream, replay, reconcile and restart survival are unchanged. Tests: `codex_mux::tests` (routing, a real relay pair against a fake app-server), `crates/vibeke/tests/headless.rs::codex_headless_shared_app_server_multiplexes_runs`.
- **Isolation.** When Vibeke isolates the run at the `sandbox` level (`isolate: "sandbox"`, or a contained task whose box is a sandbox), Codex's own Seatbelt sandbox, which cannot nest inside Vibeke's, is switched off the way the PTY path does it: `agents.harness.codex.isolated_args`, default `["-c", 'sandbox_mode="danger-full-access"']`, is inserted after the binary (`codex -c sandbox_mode="danger-full-access" app-server`). No app-server flag for this is documented in the repo, so it is a config option; the `-c` root override reaching the app-server's threads is **[verify M0]**. Approvals are unchanged (the app-server still asks Vibeke).

### 6.3 pi (`pi`) and omp (`omp`)

These two are one adapter family. omp (oh-my-pi) is a fork of pi. It keeps pi's extension model and ships a legacy shim, so extensions importing `@earendil-works/pi-coding-agent` or `@mariozechner/pi-coding-agent` resolve to omp's API (`src/extensibility/legacy-pi-coding-agent-shim.ts`, `plugins/legacy-pi-compat`).

**Transports:** extension via **`@vibeke/pi-extension`** (one package for both; design in `integrations/pi-extension/DESIGN.md`) + transcript + screen in TUI mode; RPC (`pi --mode rpc`, `omp --mode rpc` / `rpc-ui`) in headless mode. Capabilities: §2.3.

**Where the extension loads:**
- *Implemented:* `vibeke integration install pi|omp` writes the bundled extension (`integrations/pi-extension/dist/vibeke.js`, single ESM file, no deps) to `~/.pi/agent/extensions/vibeke/index.js` / `~/.omp/agent/extensions/vibeke.js` with a managed-file header; unmanaged files are never touched and other extensions are listed as foreign. `VIBEKE_PI_HOME`/`VIBEKE_OMP_HOME` redirect to scratch copies. Whether pi/omp load `.js` at these paths is unverified against live binaries.
- pi: `~/.pi/agent/extensions/vibeke/index.ts`, or `packages` in settings.json via `pi install npm:@vibeke/pi-extension`.
- omp: `~/.omp/agent/extensions/vibeke.ts`, or `--extension`.

**Event mapping (verified against pi 0.84.1 docs and omp 17.2.12 `extensions/types.ts`):**

| Event | pi | omp | Vibeke signal → state |
|---|---|---|---|
| `session_start {reason}` | ✓ (startup/new/resume/fork) | ✓ | identity: `ctx.sessionManager.getSessionFile()`/`getSessionId()` → `SessionStarted` |
| `session_switch` / `session_branch` | — (pi uses session_start reason) | ✓ | re-identify |
| `input` | ✓ | ✓ | `TurnStarted{prompt_preview}` (only `source !== "extension"`) |
| `agent_start` | ✓ | ✓ | `working` |
| `turn_start` / `turn_end` | ✓ | ✓ | Turn records (`turnIndex`), usage from `turn_end.message.usage` |
| `tool_execution_start` / `_end` | ✓ | ✓ | `ToolStarted` / `ToolEnded`. `tool_execution_end` carries only `toolCallId, toolName, result, isError` — **no input** — so the extension caches each call's input by `toolCallId` from `tool_call` / `tool_execution_start` and emits `FileChanged{path}` for `write`/`edit` on end from the cached input |
| `tool_call` (can block, can revise input — **Vibeke never does either**) | ✓ | ✓ (loop-dispatched calls: emitted at arg-prep time, **before** concurrency scheduling, `tool_execution_start` and omp's own approval gate) | input cached by `toolCallId`; always returns `undefined` |
| `tool_approval_requested` / `_resolved` | — (pi has no built-in approvals) | ✓ (`toolCallId, toolName, reason, approvalMode` / `approved`) | approval Interaction opened/resolved (observe; answering = jump to pane, or opt-in best-effort keystrokes) |
| `agent_end` | ✓ | ✓ | provisional `idle` (debounced 250 ms) |
| `agent_settled` | ✓ | — | definitive execution `idle` (pi only) |
| `session_stop` | — | ✓ (vetoable settling pass) | **not completion**: fired when a main-agent turn is *about to* settle; any handler may request one continuation turn (`stop_hook_active` signals a re-entry). Vibeke records `settling` and keeps execution `working`; execution becomes `idle` only when no continuation follows (next `agent_start` absent within the debounce, or the session reports not streaming). Vibeke's own handler never vetoes. |
| `auto_retry_start/end` | ✓ | ✓ | `error{retrying:true}`; `rate_limited` if message matches rate-limit |
| `compaction_*` / `auto_compaction_*` / `session_compact` | ✓ | ✓ | `Compacting` |
| `model_select` | ✓ | — | run.model |
| `session_shutdown` | ✓ | ✓ | `SessionEnded` |
| `goal_updated`, `todo_reminder` | — | ✓ | Phase 2 task/plan items (stored as `Raw`) |

**Approvals:**
- **No Vibeke approval gate for pi or omp.** pi has no permission system by design ("runs with all permissions"; `docs/security.md`) and leaves approvals to extensions; omp has its own approval modes. Vibeke does not add a permission system to either: the `@vibeke/pi-extension` is **observe-only** (it never blocks or revises a `tool_call`). Users who want approvals in pi install a permission extension of their choice.
- **Surfacing permission-extension dialogs** (design: `integrations/pi-extension/DESIGN.md` §4):
  - **pi RPC/headless mode:** every dialog an extension opens with `ctx.ui.select/confirm/input/editor` is emitted by pi as an `extension_ui_request` (with `id`, `method`, `title`, `message`/`options`, optional `timeout`) and blocks until the client replies with an `extension_ui_response` (`value`, `confirmed` or `cancelled`) — pi `rpc.md` §Extension UI Protocol. The `RpcAdapter` opens one Interaction per request (`confirm` → `approval` when it reads as a permission prompt, else `question`; `select` → `question` with options; `input`/`editor` → `question` with free text) and answers natively. This works for **any** permission extension with no per-extension code.
  - **pi TUI mode — primary: shared-uiContext wrapper.** Verified in pi 0.84.1 (`dist/core/extensions/runner.js`): every extension's `ctx.ui` is a getter returning the runner's single `uiContext`, and dialog options accept `signal` (programmatic dismiss) and `timeout`. The extension wraps `confirm`/`select`/`input` on that shared object (re-wrapped idempotently on `session_start`/reload/uiContext swap, tracked in a `WeakSet`). The wrapper calls the original with a linked `AbortController` signal, so **pi renders its native dialog unchanged**, and concurrently opens an Interaction (`confirm` → `approval`, `select`/`input` → `question`; source "pi extension dialog"). Native answer first → `resolved_elsewhere`; Vibeke answer first → abort the native dialog via the signal and return Vibeke's answer to the calling extension; Vibeke unreachable → native only (fail-open; it is the user's plugin's dialog). `editor` is observe-only; `ctx.ui.custom()` widgets are not covered. This is observe + answer, **not a Vibeke gate**. It **relies on pi internals**, so `answer_native: extension dialogs` is granted per pi version only when the golden cases pass; a load-time self-check disables it otherwise. Design: `integrations/pi-extension/DESIGN.md` §4.1.
  - **pi TUI fallback:** a screen manifest for pi's generic dialog widget flags an inferred Interaction for unfocused panes (`source: screen`, `answerable: false`; card action = jump to pane); best-effort keystrokes behind `agents.harness.pi.keystroke_answers = false`.
  - **Upstream (optional):** `integrations/pi-extension/UPSTREAM-PROPOSAL.md` asks pi to make this path official (`ui_request`/`ui_response` observer events, optional `ctx.ui.resolve`), removing the dependence on internals.
- **omp** approvals: omp's own approval dialog is observed via `tool_approval_requested`/`_resolved` (verified against 17.2.12 `session/agent-session.ts#beforeToolCall`: loop-dispatched `tool_call` precedes omp's approval gate; Vibeke uses that only to cache inputs). omp 17.2.12 keeps a single `#uiContext` shared by all extension contexts (`extensions/runner.ts`, `ui: this.#uiContext`) with `signal`/`timeout` dialog options, so the same wrapper should answer omp prompts that go through `uiContext` (including omp's extension-tool safety prompt) — **[verify M0]**; otherwise answering from a card = jump to pane, or opt-in keystrokes. If a later omp exposes a resolve API, the manifest's `[[capabilities]]` for that version range may add `answer_native: approval` after golden tests. Nested device dispatches and direct non-loop executions emit `tool_call` from the wrapper instead; the adapter treats both the same, keyed by `toolCallId`.
- **Reconnect repair.** The extension does not rely on a lossy outbound queue for correctness. On every (re)connect it sends `adapter.snapshot` with the session's full current state (session id/file, streaming or not, pending tool calls with cached inputs, open approval dialogs seen via `tool_approval_requested` without a matching `_resolved`, model, last turn index), equivalent to RPC `get_state`. The arbiter replaces its view of the run from the snapshot; any queued events older than the snapshot are dropped. This gives the extension transport the `reconcile` capability.
- **RPC/RPC-UI mode:**
  - pi's Extension UI Protocol (`extension_ui_request` with `select`/`confirm`/`input`/`editor` on stdout, `extension_ui_response` on stdin) maps 1:1 to `question`/`approval` Interactions with native answers — this is how a user's permission extension is answered from Vibeke.
  - omp's `rpc-ui` mode is the equivalent and also carries tool approvals **[verify M0]**.
  - `steer`/`follow_up` map to `agent.prompt --steer|--follow-up`.
  - `abort` maps to `interrupt`.
  - `get_session_stats` provides usage.

*Implemented (Batch 1 lane 1A, `agents/headless/pi.rs`; fake pi only, live **[verify M0]**):* `agent.start {harness: "pi"|"omp", mode: "headless"}` runs `pi --mode rpc [--session-id X]` / `omp --mode rpc` (resume: `pi --session <id>`, `omp --resume <id>`) under a pipe-mode holder, framing on `\n` only. `get_state` at start gives `SessionStart` (session id and file); `prompt` (mid-turn `steer`/`follow_up`) → turn; `tool_execution_start`/`_end` → items (input cached by `toolCallId`); `message_end` → transcript; `turn_end.message.usage` → usage; `agent_end` → `Stop`; compaction → `PreCompact`/`PostCompact`; `abort` = interrupt. `extension_ui_request` `confirm` → approval, `select` → question with options, `input`/`editor` → free-text question, answered with `extension_ui_response {confirmed | value | cancelled}`; fire-and-forget methods are rendered (`notify`) or ignored. After a restart `get_state` reconciles a run that settled while unobserved. `rpc-ui` and `get_session_stats` are not used.

*As built (v1 adapter items, 2026-10-06; fakes only, live **[verify M0]**):*
- **omp `rpc-ui`.** `agent.start {harness: "omp", mode: "headless"}` runs `omp --mode rpc-ui` (the manifest's `headless = "rpc-ui"`). omp's tool approvals: `tool_approval_requested {toolCallId, toolName, args?, reason}` is shown in the pane and remembered; the `confirm`/`select` dialog that names the `toolCallId`, or else the next dialog after an unmatched request, becomes an **approval** for that tool call (tool name and input from the request or the cached `tool_execution_start`), so it is risk-scored and policy rules apply. A `confirm` is answered `confirmed`; a `select` with the option whose words read as allow (no "always"/"session"), allow always, or deny (else `cancelled`). `tool_approval_resolved` withdraws a dialog still open (answered in omp, `resolved_elsewhere`). Headless omp runs list `answer_native:approval` besides `answer_native:extension_dialog`. Whether omp's rpc-ui uses exactly these events and dialog shapes is unverified.
- **`get_session_stats`.** Sent after every `agent_end` and on reconcile; its `tokens {input, output, cacheRead, cacheWrite}` and `cost` (number or `{total}`) replace the run's usage as a session total (`turn_end` deltas keep it current during a turn). A build that rejects the command is ignored silently.

**Identity and resume:**
- pi: transcripts at `~/.pi/agent/sessions/--<cwd-slug>--/<ts>_<uuid>.jsonl`; resume `pi --session <path|id>`; pre-assign `pi --session-id <id>`; fork `pi --fork <id>`.
- omp: `~/.omp/agent/sessions/…`; resume `omp --resume <id|path>`; pre-assign **[verify M0]**. Headless RPC processes run under a holder in pipe mode and survive server restarts; the extension/adapter re-syncs with `get_state` after reattach. Until golden-tested, `survive_disconnect = false` and they are resumed from their session file instead.
- omp can import Claude and Codex sessions (`--from-claude`, `--from-codex`). This is exposed as "Continue this Claude session in omp" in the pane menu.

### 6.4 OpenCode (`opencode`)

- **Extension transport**: a plugin in `~/.config/opencode/plugins/vibeke.ts` (`export const Vibeke = async ({ project, client, $, directory, worktree }) => ({ event, "tool.execute.before", "permission.ask" … })`).
- **Events used:** `session.created`, `session.status`, `session.idle` → execution `idle`, `session.error` → `error`, `permission.asked`/`permission.replied` → approval open/resolve, `tool.execute.before/after` → tool items, `file.edited` → `FileChanged`, `message.updated` → usage, `todo.updated` → Raw, `session.compacted`.
- **Native approval answering**: through the plugin's `permission.ask` hook, which sets `output.status = "allow" | "deny"`, or through the OpenCode SDK `client` **[verify M2]**.
- **Subagents**: OpenCode subagents could show idle when detection ignores child sessions. They are tracked from `session.created` with a parent id, so a parent stays `working` while any child session is busy.
- **Headless**: `opencode serve` HTTP API + SSE event stream, and ACP **[verify M2]**.
- *Implemented (M2, unverified against a live OpenCode):* `vibeke integration install opencode` writes the managed plugin `integrations/opencode-plugin/vibeke.ts` to `~/.config/opencode/plugin/vibeke.ts` (`plugin/` per the upstream docs, not `plugins/` **[verify M2]**; `VIBEKE_OPENCODE_HOME` redirects; unmanaged files are never overwritten; other plugins listed as foreign). The plugin forwards `session.created/status/idle/error/compacted`, `permission.replied`, `file.edited`, completed assistant `message.updated` and the `chat.message`, `tool.execute.before/after`, `permission.ask` hooks through `vibeke hook opencode <event>` (the same fail-open shim; `message.part.updated` is never forwarded). `agents/opencode.rs` maps them onto the hook vocabulary (so turns/items/tracking work): child sessions (`parentID`) keep the parent `working` until every child is idle; `permission.ask` is gate-capable and answered with `{"status": "allow"|"deny"}` (allow-always → allow: the hook has no "always"), but only when a capability row grants `answer_native: approval` — none does for unvalidated versions, so by default it opens an observe interaction answered by keystrokes on the (synthetic, unverified) TUI dialog geometry. Usage from `message.updated` tokens/cost. The SDK `client`, `opencode serve`/SSE and reconcile are not used.

### 6.5 Gemini CLI (`gemini`)

- **Hooks transport**: hooks in `~/.gemini/settings.json`: `SessionStart`, `SessionEnd`, `BeforeAgent` (→ working), `AfterAgent` (→ execution `idle`), `BeforeTool` (gate-capable, decision `deny`), `AfterTool`, `Notification` (tool confirmation → approval Interaction), `PreCompress`.
- **Gemini's "silence is mandatory" rule**: the hook's stdout must be exactly one JSON object. The shim writes diagnostics to stderr only.
- **Whether a hook can answer the native confirmation**: **[verify M2]**. Fallback is verified keystrokes.
- **Headless**: Gemini's ACP mode (Gemini CLI was ACP's first agent) through `AcpAdapter`.
- *Implemented (M2, unverified against a live Gemini CLI):* `vibeke integration install gemini` merges `SessionStart, BeforeAgent, AfterAgent, BeforeTool (.*, gate timeout in ms), AfterTool, Notification, PreCompress, SessionEnd` into `~/.gemini/settings.json` (Claude-like groups with `"name": "vibeke"`; recognised by command, no marker key; `VIBEKE_GEMINI_HOME` redirects). Whether hooks need an explicit enable flag in settings and whether `timeout` is milliseconds are **[verify M2]**. `agents/gemini.rs` maps `BeforeAgent`→turn, `BeforeTool/AfterTool`→tool items (tool names `run_shell_command`→Bash, `replace`→Edit, `write_file`→Write…; Gemini payloads carry no call id, so ids are synthesized per pane FIFO), `AfterAgent`→turn end with `prompt_response`, `Notification{ToolPermission}`→ an observe-only approval (keystrokes) closed by the matching `AfterTool`, `PreCompress`→compacting. `BeforeTool` deny (policy enforcement) is not wired; `AfterModel` usage is parsed when that hook is installed by hand (not installed by default). Headless ACP: `vibeke agent start --harness gemini --acp ""` runs `gemini --experimental-acp` through the ACP host (`acp:gemini`) **[verify M2]**.

### 6.6 Generic ACP agents (`acp:*`)

Any agent in the ACP registry can be run headless.

- **Mapping:**
  - `session/new` / `session/load` → run identity.
  - `session/prompt` → turn.
  - `session/update` (`agent_message_chunk`, `tool_call`, `tool_call_update` with `status: pending|in_progress|completed|failed` and `kind: read|edit|delete|move|search|execute|think|fetch|switch_mode|other`, `plan`, `available_commands_update`, `current_mode_update`) → items.
  - `session/request_permission{toolCall, options[{optionId, name, kind: allow_once|allow_always|reject_once|reject_always}]}` → approval Interaction, answered natively with `{outcome: {outcome: "selected", optionId}}`.
  - `session/cancel` → interrupt.
- **Client capabilities Vibeke offers the agent:**
  - `fs/read_text_file` and `fs/write_text_file`, scoped to the task worktree.
  - `terminal/*`, which creates **real Vibeke panes** (a "terminal" requested by an ACP agent becomes a sibling pane the user can see).
- *Implemented (M2, `agents/acp.rs`):* ACP runs are launched by Vibeke (`vibeke agent start --acp "<cmd …>"`, `--harness <id> --acp ""` for a manifest's `acp_argv`, or a manifest with `[launch] transport = "acp"`) as `vibeke acp-host --harness acp:<name> -- <agent argv>` **in the pane**: the host is the ACP client (JSON-RPC over the agent's stdio: `initialize`, `session/new` (or `session/load` with `--resume` when `loadSession`), `session/prompt`, `session/cancel`), renders a minimal transcript in the pane, edits the input line itself (raw mode; Enter submits, Esc cancels the turn) and reports through the pane token like a hook shim. Mapping: session id → `SessionStart`; prompt → `UserPromptSubmit`; `tool_call`/`tool_call_update` (completed/failed) → `PreToolUse`/`PostToolUse(Failure)` with `kind` → tool name and `locations[0].path` → file path; `agent_message_chunk` → last message; prompt result `stopReason` → `Stop` (`refusal`/`max_tokens` → `StopFailure`), `usage` on the result when an agent reports it; `session/request_permission` → approval Interaction through `adapter.gate` (native ref = `toolCallId`; the ACP options are kept as the interaction's `acp_options` question) answered with `{outcome: {outcome: "selected", optionId}}` (allow → `allow_once`, allow-always → `allow_always`, deny → `reject_once`, fallbacks within the same polarity). The host's own numbered prompt is the "native dialog": with the pane focused (observe mode) or after release-on-focus the user answers there (→ resolved elsewhere), and remote keystroke answers work through the `acp` manifest's screen rules. `fs/read_text_file`/`fs/write_text_file` are served, scoped to the session cwd; `terminal/*` is **not** offered (`clientCapabilities.terminal = false`), so ACP terminals as Vibeke panes remain open. The host process lives in the pane's holder, so it survives a server restart; reconciling an in-flight permission after a restart is not implemented for the pane-hosted path.
- *Implemented (Batch 1 lane 1A, `agents/headless/acp.rs`):* `agent.start --acp "<cmd>" --mode headless` runs the agent itself under a pipe-mode holder with the server as the ACP client (same mapping as the host; `fs/*` answered inside the session cwd, unknown requests `-32601`; permission requests are Interactions with native ref `rpc:<id>`). This path reconciles: after a restart the journal replay re-opens or delivers pending permissions, and when the ring no longer reaches the session start the adapter sends **`session/load`**, whose replayed `session/update`s rebuild the transcript without re-emitting events. `agent.resume` of a headless ACP run starts a new agent with `session/load` (the command is kept in the run's `headless/<pane>` record).
- *As built (v1 adapter items, 2026-10-06, `agents/headless/acp_term.rs`):* **`terminal/*`** on the headless path, offered as `clientCapabilities.terminal = true`. `terminal/create {command, args, env, cwd, outputByteLimit}` splits a **real pane** below the run's pane running the command (`/bin/sh -c <command>` without `args`, else the argv; `env` through `/usr/bin/env`), titled `acp: <command>`, and answers `{terminalId}`; `terminal/output` returns the pane's text (scrollback plus screen, the newest `outputByteLimit` bytes cut at a character boundary, `truncated`) and `exitStatus` once exited; `terminal/wait_for_exit` is answered when the process exits (`{exitCode, signal}`, signal names like `SIGKILL`); `terminal/kill` closes the pane (the output stays readable); `terminal/release` kills a running command and forgets the id. A watcher task per terminal keeps the latest output snapshot (the pane closes when its process exits) and listens for `pane.exited` (subscribed before the pane is created). Sandboxing is that of `fs/*`: the `cwd` must resolve, symlinks included, inside the session cwd (`-32002` otherwise). **A run Vibeke isolates gets neither fs nor terminals**: the server would carry them out on the host, outside the box, so `clientCapabilities` says `fs.readTextFile = fs.writeTextFile = terminal = false` and requests are refused with `-32002` (the record's `isolated` flag, set from the isolation level `agent.start` prepared). Terminal ids and panes are kept in the record; a restarted server watches the panes again (a pane gone meanwhile reports an exit with unknown status), and the replay hands still-unanswered requests to the session again, a create answering with the terminal it already made. The pane-hosted `vibeke acp-host` path still does not offer terminals. Tests: `agents/headless/tests.rs` (`acp_terminal_capability_and_isolation`, `sessions::acp_terminals_are_confined_and_answer_exit_and_output`, `sessions::isolated_acp_runs_refuse_host_terminals_and_fs`), `acp_term::tests`, `crates/vibeke/tests/headless.rs::acp_headless_terminals_run_as_panes`.
- *As built (review batch 1, 2026-10-06, `agents/headless/{mod,acp}.rs`):* durability rules shared by every headless adapter. **(a) fs requests:** the path is resolved completely, the final component included; anything resolving outside the session cwd (through `..`, a symlinked directory or a symlinked file, or a dangling symlink) is refused with `-32002`, and the file is opened with `O_NOFOLLOW`. **(b) No side effects during replay:** automatic requests (ACP `fs/*`) run only for live frames; a replayed request only rebuilds state and `reconcile` answers the ones the journal shows unanswered. A completed write is recorded in the record's `auto_done` (persisted before its response is written), so a write that ran just before a crash is answered without being repeated. **(c) Idempotent approval delivery:** the response's holder input id derives from the decision's idempotency key (FNV-1a, bits 63 and 62 set), so a retry after a reconnect or restart is the same input and the holder's dedupe collapses it; a request whose response is still in the in-flight ledger is never re-delivered. **(d) Evidence only:** after a truncated replay (`gap`) a request the journal never showed proves nothing: a recorded decision becomes `delivery_unknown` (`journal_truncated`, with a note in the pane) and an unanswered interaction stays open; `delivered` / `resolved_elsewhere` need the journal to show the request (or to reach back to the session start). **(e) Queued prompts are durable:** prompts acknowledged before the session is ready, and follow-ups for adapters without a native follow-up queue (Codex), are kept in the record's `queued` list, persisted before `agent.prompt` acks; they are handed to the adapter in order once it is ready (a follow-up only when no turn runs and no earlier prompt is in flight), and a restarted server delivers them. Tests: `agents/headless/tests.rs` (`sessions::*`, `acp_fs_requests_never_follow_symlinks_out_of_the_cwd`).

### 6.7 Other built-ins (screen + self-report)

Cursor agent, Copilot CLI, Devin CLI, Droid, Kimi, Kilo, Qoder, Mastra Code, Antigravity CLI, Grok, Amp, Aider, Letta, Muse and Qwen ship as screen manifests with `observe` (+ `answer_keystroke` where dialog geometry is golden-tested). Where their CLIs offer hooks, ACP or plugin APIs, they are upgraded in the same manifest without code changes, through `[integration] transports` / `headless = "acp"` or an external adapter, and gain capabilities only as golden tests cover them. Detection for all 16 is an M2 target.

---

## 7. Interactions, gating and policy

### 7.1 Opening

An adapter opens an Interaction with `InteractionOpen{kind, native_ref, payload, gate}`. Then:

1. **Identity check.** The run must be **bound** (§2.6). Interactions on suggested/unlinked runs are recorded as display-only (`answerable: false`) and skip steps 2–3.
2. Run the policy check (02 §4). If a rule matches with `allow` or `deny` **and** the run's capabilities include `gate` or `answer_native` for this kind, the decision is recorded (`decided_by: policy`, `policy.rule_matched`) and goes through the delivery transaction (§7.3). The agent never visibly blocks. Without those capabilities, policy can only annotate ("policy would allow").
3. Otherwise, add the Interaction to the run's pending list (execution state unchanged, §2.4) and raise a notification (urgency from risk, §7.6).
4. Decide the **gate mode** (§7.2).

### 7.2 Gate modes: observe vs gate

There's a tension. If a sync hook blocks while waiting for Vibeke, the harness's own dialog never appears, so a user sitting at that pane can't answer natively. If the hook returns immediately, remote answering needs keystrokes. The rule that resolves it: **the focused pane belongs to the agent** (08 §1) — whenever a pane is focused by an attached client, the harness's own dialog must be what the user sees.

`approvals.mode` per harness or workspace (default `auto`):

| Mode | Behavior |
|---|---|
| `observe` | Shim reports the interaction and returns "no decision" at once. The harness shows its native dialog. Other clients can answer only via **best-effort verified keystrokes** (§8), if `answer_keystroke` is valid. Resolution is observed through Post*/`serverRequest/resolved`/`tool_approval_resolved`/screen. |
| `gate` | Requires the `gate` capability (Claude, Codex, headless/ACP; **not pi/omp**). Shim holds the hook call until Vibeke decides, or until `gate_timeout`. The Interaction is shown as a **card** (sidebar, peek, inbox, notification — never drawn over the pane). Any client with answer scope can answer. **Release on focus:** if an attached client focuses the pane while the hook is held, Vibeke releases it with "no decision" so the harness's own dialog appears in the focused pane (harness-backed approvals only); the Interaction continues in observe mode. On timeout the outcome follows §2.7: harness-backed approvals return no decision (native dialog appears); Vibeke-only enforcement fails closed. |
| `auto` (default) | `gate` when the pane is **not focused by any attached client** at the moment the interaction opens, or when the run is headless, or when the user is remote (Phase 2) — with release-on-focus as above. Otherwise `observe`. |

`gate_timeout` defaults to 30 min. Claude's command-hook timeout defaults to 600 s, so the installer sets the `PermissionRequest`/`PreToolUse` hook `timeout` to 1800 s. Codex hook timeouts are set to match.

### 7.3 Answer delivery as a recoverable transaction

Recording a decision and delivering it to the harness are two separate steps; a crash, a dropped shim connection, a hook timeout, two clients answering at once, or the user answering in the harness TUI can all happen in between. Every Interaction therefore carries a **delivery record**:

```
Delivery {
  interaction_id,
  native_ref,                 // tool_use_id | Codex requestId/approvalId | pi/omp toolCallId | ACP request id | screen dialog fingerprint
  decision_rev: u32,          // increments if a decision is superseded before delivery (e.g. user changes their mind)
  idempotency_key,            // blake3(interaction_id, decision_rev); harness-side duplicate guard where possible
  channel: native | keystroke,
  lease: { holder: shim|extension|adapter_task|keystroke_verifier, conn_id, expires_at },
  deadline,                   // after which the decision is void (harness timeout, gate_timeout)
  state: decision_recorded | delivering | delivered | delivery_unknown | failed{reason} | superseded | resolved_elsewhere,
  attempts: [ { at, channel, outcome } ]
}
```

**State machine**

```
decision_recorded ──lease acquired──► delivering ──ack/native resolution──► delivered
        │                                 │
        │ (first answer wins: a second    ├─ crash / conn drop / timeout before ack ──► delivery_unknown
        │  client's answer → rejected     │
        │  "already answered by X")       └─ harness rejects / dialog changed / deadline passed ──► failed{reason}
        ▼
  resolved_elsewhere  (native resolution observed for native_ref before delivery — e.g. user answered in the TUI)
```

Rules:

1. **First decision wins** per `decision_rev`. Later answers from other clients get `interaction.already_answered {by, decision}`. A user may supersede only while state is `decision_recorded` (not yet delivering).
2. **Never retransmit blindly.** From `delivery_unknown`, the adapter first **reconciles** (if the run has `reconcile`): Codex `thread/read` / `serverRequest/resolved`; pi/omp extension snapshot; ACP/stream-json session state; Claude: the shim reconnecting with `adapter.gate_resume` *is* the reconcile (the hook process either still waits — deliver — or is gone — then check transcript/Post* signals for the `tool_use_id`). Only if reconciliation shows the native request is **still pending** is the decision re-sent with the same idempotency key. If the request is gone and the outcome can't be determined, the interaction ends `delivery_unknown` and the UI says so ("Vibeke couldn't confirm your answer reached Claude").
3. **Shim/extension side:** the hook process holds the lease while it waits; on receiving the decision it writes stdout and calls `adapter.delivery_ack{idempotency_key, applied: true}` **before** exiting where the harness allows (for Claude/Codex the ack is sent just before writing stdout; the subsequent `PostToolUse`/`PermissionDenied`/`serverRequest/resolved` signal confirms `delivered`).
4. **Server restart:** open interactions and delivery records live in `state.db`. A shim that loses its connection reconnects once with `adapter.gate_resume{interaction, conn_id}`; the server re-issues the lease and returns the recorded decision if one exists.
5. **Keystroke delivery is best-effort** (§8): its outcome is `delivered` only when the dialog disappears *and* a native or screen resolution consistent with the chosen option is observed; otherwise `delivery_unknown`. The UI never presents keystroke delivery as guaranteed.

Events: `interaction.decided {rev, by}`, `interaction.delivery_started`, `interaction.delivered`, `interaction.delivery_unknown`, `interaction.delivery_failed {reason}`, `interaction.resolved_elsewhere`.

### 7.4 Blocking protocol (shim side)

```
shim: connect VIBEKE_SOCKET → adapter.gate{token, harness:"claude", event:"PermissionRequest", native_ref, payload, gate_hint}
server: policy fast-path → reply {decision, idempotency_key} within ~1 ms → shim acks, prints hook JSON, exit 0
        or reply {wait: true, interaction:"i42", lease} and keep the request open
        … later → {decision, idempotency_key} | {no_decision}       → shim acks + prints JSON, or prints nothing; exit 0
shim: if the socket drops or the server restarts → reconnect once with adapter.gate_resume{interaction, conn_id}
      → else apply §2.7: harness-backed approval → print nothing (native dialog); Vibeke-only deny rule → fail closed
```

### 7.5 Shim performance and safety (`vibeke hook`)

- Same static binary, `hook` subcommand, with no tokio runtime (blocking std I/O on a Unix socket). It reads stdin (capped at 4 MiB), wraps it, sends it, and reads the reply.
- **Budget**: p50 ≤ 5 ms, p99 ≤ 15 ms for non-blocking events, measured in CI (`hyperfine` on Linux and macOS runners). No dynamic loading of config: the shim reads only env and stdin.
- **Observation fails open**:
  - If not inside Vibeke (`VIBEKE` unset), exit 0 silently.
  - If the socket is missing, exit 0.
  - If the reply is malformed, exit 0 with no stdout.
- **Decisions are JSON only.** The shim never prints to stdout except a valid decision JSON (Gemini requires this) and **always exits 0**. Exit-code semantics differ per harness and per event (Claude: exit 2 blocks `PreToolUse`/`UserPromptSubmit`, prevents `Stop`, and is *ignored* for `PermissionRequest`), so a crash must never be interpreted as a decision; Vibeke-only enforcement is expressed as JSON (`permissionDecision: "deny"`/`"ask"`), never via exit codes.
- **Payload hygiene**: `tool_input` values over 64 KiB are truncated in the event, with the full payload in the blob store. Env is never forwarded; secret patterns are redacted per 09 §9.

### 7.6 Risk scoring (Phase 1 heuristic; Phase 2 learns)

`risk` is computed on open:
- **high**: destructive shell patterns (`rm -rf`, `git push --force`, `git reset --hard`, `curl … | sh`, `chmod -R`, `sudo`, `DROP TABLE`, `kubectl delete`, writes outside the workspace root, editing `.env*`/credentials files, network to non-allowlisted hosts).
- **medium**: package installs, migrations, git commits/pushes, edits in more than 5 files.
- **low**: read-only commands, test/lint/build runners matched from `package.json` scripts / `Makefile` / `justfile` / `Cargo.toml`.
- **unknown**: everything else.

Risk drives notification urgency and the card's default selection. Deny is pre-selected for high risk.

### 7.7 Fingerprints for learning

Each answered approval stores `fingerprint = hash(harness, tool, normalized_command_prefix | path_glob, workspace)` with the decision. Phase 1 exposes `vibeke policy suggest`, which lists fingerprints approved ≥ N times with 0 denials, as ready-to-paste rules. Phase 2 surfaces this in the inbox.

---

## 8. Answer delivery by verified keystrokes

When no native channel exists (screen-only harnesses, observe mode, omp's native dialog), Vibeke can answer by typing. This is **best-effort delivery**: verification makes a wrong selection unlikely, but the application can change between the last screen read and Enter, and input locking prevents other *clients* from interleaving without freezing the application itself. Outcomes are therefore reported per §7.3 rule 5, and keystroke answering is only offered on **bound** runs (§2.6) with a validated screen manifest. The technique: send arrows, re-read the screen, and press Enter only when the pointer is on the tapped row.

Algorithm (in `vk-agents::keys::Verifier`):

1. **Locate the dialog.** Use the harness's screen manifest dialog rule (§9) to get the option rows, the pointer glyph/style (e.g. `❯`, reverse video, a specific SGR fg color), and the expected labels.
2. **Validate freshness.** The dialog must match the Interaction's options (label fuzzy match ≥ 0.9) and must not have changed since the last screen damage. Otherwise abort with `delivery_failed{reason: "dialog_changed"}`.
3. **Plan the moves.** Read the current pointer row and compute the delta to the target row. Prefer direct accelerators when the manifest declares them (e.g. `1`/`2`/`3`, `y`/`n`), since those are atomic.
4. **Step and verify.** Send one move key, wait for damage (≤ 300 ms), re-read, and confirm the pointer moved to the expected row. Retry at most twice per step.
5. **Commit only on match.** When the pointer row's label equals the target label (and the pointer style matches), send Enter. Otherwise send nothing and fail.
6. **Confirm.** Wait ≤ 2 s for the dialog to disappear **and** a native or screen resolution consistent with the chosen option. Emit `interaction.delivered`; if the dialog disappeared without a consistent resolution, `interaction.delivery_unknown`; if aborted before Enter, `interaction.delivery_failed`.
7. **Multi-select / free text.** Toggle with Space and verify a checkbox glyph per row. Free text is typed with bracketed paste when the pane has it enabled, then verified by reading back the input line.

**Escape safety.** A user-requested Esc on a dialog Vibeke cannot parse requires a confirm step in the UI ("Send Esc to an unrecognized dialog?").

**Input locking.** While a verified sequence runs, other clients' input to that pane is queued (≤ 2 s) so keystrokes don't interleave. The local user's own typing aborts the sequence instead.

---

## 9. Screen detection manifests

Screen detection is the fallback. Even so, it must be explicit, versioned and tested.

### 9.1 DSL

```toml
schema = 1
id = "claude"
harness = "claude"
versions = ">=2.0.0, <3.0.0"        # semver range this manifest was validated against
[defaults]
region = { rows = "-30..-1" }        # negative = from bottom of the visible screen; alt-screen aware
normalize = ["strip_sgr_except_fg", "collapse_spaces", "nfc"]

[[rules]]
id = "working"
state = "working"
any = ['(?m)^\s*[✻✶✳✢·*]\s+\w+…\s+\(', 'esc to interrupt']
confidence = 0.8
hold_ms = 400                        # must persist this long (debounce spinner gaps)

[[rules]]
id = "approval"
opens = "approval"
all = ['Do you want to (proceed|make this edit|create)', '(?m)^\s*❯?\s*1\.\s+Yes']
confidence = 0.95
[rules.interaction]
kind = "approval"
title_from = { regex = 'Do you want to (.+)\?', group = 1 }
command_from = { region = "box_above_question", trim = true }
[rules.dialog]                       # geometry used by the keystroke verifier (§8)
options_regex = '(?m)^\s*(?P<ptr>❯)?\s*(?P<n>\d)\.\s+(?P<label>.+?)\s*$'
pointer = { glyph = "❯" }
accelerators = "digits"              # pressing "2" selects option 2 directly
map = { allow = 1, allow_always = 2, deny = 3 }

[[rules]]
id = "question"
opens = "question"
all = ['(?m)^\s*☐|☒', 'Enter to select']
confidence = 0.9
[rules.dialog]
options_regex = '(?m)^\s*(?P<ptr>❯)?\s*(?P<n>\d+)\.\s+(?P<label>.+)$'
pointer = { glyph = "❯" }
multi_toggle = "space"

[[rules]]
id = "input_box"                     # used for `ready` and idle
state = "idle"
all = ['(?m)^\s*>\s', '(?m)^\s*╰─+╯\s*$']
confidence = 0.6
```

Other matchers: `style = { fg = "#d97757", bold = true }` for color-dependent markers (opencode chips, highlighted rows); `cursor_in_region = true`; `title_regex` on the OSC 0/2 window title; `osc_133 = "prompt|command|output"` for shell-integration marks; `not = [...]` exclusions; `region = "alt_screen_only"`.

### 9.2 Engine

- Evaluated on damage, at most 10 Hz per pane, and only on the rows the rules' regions cover. Regexes are compiled once (Rust `regex`, no backtracking).
- **Budget**: ≤ 0.3 ms per pane per evaluation for a 200×60 screen.
- The output is a `ScreenMatch{rule_id, state?, opens?, confidence, captures, dialog?}`, sent to the run's adapter and the arbiter, which apply §2.5 (screen matches add information; they never overwrite structured state).
- **Unknown-dialog heuristic**: a boxed region with numbered options and a pointer glyph that matches no rule opens a provisional `question` interaction with confidence 0.5 and a "Vibeke can't read this dialog" hint, rather than silently showing `idle`.
- *Implemented (M2):* the DSL subset `any`/`all`/`not`, per-rule bottom-row `rows`, `state`/`opens`, `title_regex`/`command_regex` captures and `[rules.dialog]` (`options_regex`, `pointer`, `accelerators = digits|letters|arrows`, `confirm`, decision `map`) runs in `vk-agents::manifest` for every manifest-driven harness (dialog rules first, then state rules; options = the last contiguous block). Keystroke delivery (§8) uses the manifest key plan for those harnesses. Not implemented: `hold_ms`, `normalize`, color/cursor/OSC-133/title matchers, alt-screen regions, the unknown-dialog heuristic.

---

## 10. Transcript ingestion

Transcripts provide history beyond scrollback, usage and cost, and a recovery path for missed events.

| Harness | Location | Format notes |
|---|---|---|
| Claude | `~/.claude/projects/<cwd-slug>/<session_id>.jsonl` (from `transcript_path`) | one JSON per line; `type: user\|assistant\|system\|summary`; `message.usage {input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens}`; `message.model`; tool_use/tool_result blocks; subagent sidechains |
| Codex | `transcript_path` from hooks (rollout JSONL under `~/.codex/sessions/YYYY/MM/DD/`); newer versions keep paginated thread history in `thread_history_*.sqlite`. Prefer `thread/read` / `thread/turns/list` via app-server when available | `token_count` events, `turn_context`, `response_item`s |
| pi | `~/.pi/agent/sessions/--<path>--/<ts>_<uuid>.jsonl` (tree-structured entries; see pi `session-format.md`) | `message` entries with `usage {input, output, cacheRead, cacheWrite, cost{total}}` |
| omp | `~/.omp/agent/sessions/…` (path from `ctx.sessionManager.getSessionFile()`) | pi-compatible entries + omp extensions |
| OpenCode | via SDK/server (`session.messages`) | — |

**TranscriptTailer**:
- Uses `notify` (FSEvents/inotify) plus offset tracking.
- Parses incrementally and emits `turn_completed{usage}` and `item` summaries only when no structured transport already provided them (dedupe by native ids). It also serves `reconcile` for hooks-only runs (e.g. confirming a `tool_use_id` was approved and executed).
- Writes compact per-turn records to `turns`/`items` (02).
- Raw transcripts are **never copied**. Vibeke stores pointers plus summaries. Full-text search indexes transcript text in FTS5 (`source = transcript`), and the index respects `search.index_transcripts = true|false`.

**Usage, cost and limits:**
- `Usage` is aggregated per run, task, workspace and day.
- Cost uses the harness-reported cost when present (pi `cost.total`, Claude `total_cost_usd` in stream-json). Otherwise it is computed from a bundled, signed price table keyed by model id. Subscription-billed runs show tokens, not dollars.
- Rate limits come from: Claude `StopFailure{rate_limit}` and `Notification{quota_auto_resume_*}`; Codex `account/rateLimits/updated` (with reset times); pi/omp `auto_retry_*` with rate-limit messages. They populate `rate_limited{resets_at}` and a per-account "limits" status segment.
- *Implemented (M2, `agents/usage.rs`):* `AgentRun.usage {input, output, cache_read, cache_write, cost_usd?, model, source}` (session totals) and `AgentRun.rate_limit {limited, resets_at_ms?, scope, used_percent?, message}` (both `#[serde(default)]`, render-stream safe). Sources: Claude transcript JSONL re-read at every `Stop` (assistant `message.usage`, de-duplicated by `message.id`; `costUSD` when present); Codex rollout at `Stop` (last `event_msg`/`token_count`: `total_token_usage` with cached input split out, `rate_limits.primary/secondary` → the most-used window with `resets_at`/`resets_in_seconds`); pi/omp extension `Usage` (summed); OpenCode `message.updated` tokens/cost (once per message id); Gemini `AfterModel usageMetadata`; ACP `usage` on the prompt result. Rate limits: Claude `StopFailure{rate_limit}`/`quota_auto_resume_*`, Codex windows at 100 %, OpenCode/pi retry messages matching rate-limit patterns (`rate_limited` execution). Not implemented: the app-server `thread/tokenUsage/updated`/`account/rateLimits/updated` (no headless Codex adapter yet), price tables, per-turn usage records, the limits status segment. The Claude/Codex field names are the ones the transcript tailer already used, not re-verified against new versions.

---

## 11. `vibeke integration` command

```
vibeke integration list                    # built-in + user manifests, installed?, version seen, transports in use
vibeke integration install <id|all> [--dry-run] [--scope user|project]
vibeke integration status [<id>] [--json]  # per-harness: files, hook trust (codex), version compat, last signal seen
vibeke integration uninstall <id|all>
vibeke integration doctor [<id>]           # runs a synthetic round-trip: launches harness in a scratch pane if possible,
                                           # waits for SessionStart signal, reports resolved capabilities and latency
vibeke integration capabilities [<id>] [--json]   # the §2.3 matrix as resolved for installed versions
```

Installer rules (all integrations):

1. **Idempotent.** Re-running produces byte-identical files. Every managed entry carries the marker `vibeke-integration=<id>@<version>`: a JSON key `"_vibeke"` where the format tolerates unknown keys, a comment where it doesn't, or a file header for files Vibeke owns.
2. **Never clobber user hooks.** JSON settings are **merged**, not replaced. Vibeke appends its own hook objects to each event's array and only ever removes objects carrying its marker. Unknown keys and user hooks are preserved byte-for-byte, with key order kept by `serde_json` `preserve_order`. A backup `settings.json.vibeke-bak-<ts>` is written before the first modification.
3. **Coexist with Herdr.** Herdr's entries (`herdr-agent-state.sh`, `herdr-omp-agent-state.ts`, `~/.hermes/plugins/herdr-agent-state`) are left alone. Both can run. Inside Vibeke panes Herdr's scripts no-op, because `HERDR_ENV` points to the compat socket, which accepts and de-duplicates them. `vibeke integration status` lists foreign integrations it found. `--replace-herdr` removes Herdr's entries only with explicit consent.
4. **Atomic writes**: write a temp file in the same dir, fsync, then rename. Abort if the file changed between read and write (mtime+hash check).
5. **Trust-gated harnesses** (Codex hooks, pi project extensions): print the exact step the user must take and verify with `status`.
6. **Stable command paths**: hooks reference `~/.local/bin/vibeke` (a symlink managed by the installer), never a versioned path, so upgrades don't break the hooks or Codex trust.
7. **Scope**: `--scope project` writes `.claude/settings.local.json` / `.codex/hooks.json` in the repo, for teams. The default is user scope.

| id | Files touched | Notes |
|---|---|---|
| `claude` | `~/.claude/settings.json` (`hooks.*`) | 15 events (§6.1) |
| `codex` | `~/.codex/hooks.json` | requires `/hooks` trust once; checks `features.hooks` |
| `pi` | `~/.pi/agent/extensions/vibeke/` (or `pi install npm:@vibeke/pi-extension`) | |
| `omp` | `~/.omp/agent/extensions/vibeke.ts` (thin loader re-exporting the package) | |
| `opencode` | `~/.config/opencode/plugin/vibeke.ts` (managed file; `VIBEKE_OPENCODE_HOME`) | implemented M2, unverified |
| `gemini` | `~/.gemini/settings.json` (`hooks`; `VIBEKE_GEMINI_HOME`) | stdout-silence rule; implemented M2, unverified |
| `hermes` | `~/.hermes/plugins/vibeke-agent-state/{plugin.yaml,__init__.py}` | Python; reports full lifecycle via `adapter.signal` |

---

## 12. Testing: golden corpus and version drift

### 12.1 Golden corpus (`tests/harness-golden/`)

For each `(harness, version)`:
- `session.cast`: asciicast v2 of a scripted session covering startup, prompt, working, approval dialog, question dialog, plan review, subagent, compaction, rate-limit/error (where reproducible), exit and resume.
- `signals.jsonl`: the hook/extension/app-server messages captured during the same run (shim tee mode `VIBEKE_HOOK_TEE=dir`).
- `expected.jsonl`: the expected `agent.state_changed` / `interaction.*` sequence with timestamps relative to the cast.

Recorded by `vibeke dev record-harness <id> --script scenarios/<name>.yaml`. The script drives the harness with prompts designed to trigger each state, using a cheap model or a provider stub (`ANTHROPIC_BASE_URL` / pi custom provider pointing at a scripted mock LLM server bundled in `tests/mock-llm/`), so recordings are deterministic and free.

**CI**:
- Replays the cast through the VT engine and the screen detector, and replays signals through the adapter, with time warp.
- Asserts the event sequence. Tolerances: state change within ±250 ms; interaction payloads field-exact.
- Answer-delivery tests run against a live harness binary inside a container (nightly) and against a recorded "dialog simulator" (per PR).

*Implemented (M2) — synthetic corpus only:* `tests/harness-golden/<id>/synthetic/` holds **hand-written** fixtures (each `meta.toml` says `synthetic = true`; they are synthesized from the spec examples and the shapes the adapters were built against, **not** recordings): `screens/*.txt` + `expected.jsonl` (state, dialog kind/options/pointer/title/command, keys per decision) for `claude`, `codex`, `pi` (shared by `omp`), `opencode`, `gemini`, `generic-repl` and the ACP host's own prompt, plus `signals.jsonl` + `signals.expected.jsonl` for OpenCode and Gemini. `agents/golden.rs` replays every manifest's screen rules and the server evaluator against them, checks key plans, replays the signals through the mappings, and fails when a manifest with screen rules has no corpus (drift guard). No `session.cast`, no recorder (`vibeke dev record-harness`), no mock LLM, no nightly live re-recording yet; capability grants are therefore still not derived from golden coverage.

### 12.2 Required coverage before an adapter ships

Every row in the §6 mapping tables needs at least one golden case. **A capability is granted to a (harness, version range, mode, interaction kind) only when its golden cases pass** — the §2.3 matrix is generated from this coverage. Delivery-transaction cases are mandatory for every `answer_native` grant: server killed while the shim waits (→ `gate_resume` delivers once), shim killed after decision recorded (→ reconcile, no duplicate), two clients answering concurrently (→ first wins), user answers in the harness TUI first (→ `resolved_elsewhere`), deadline passes (→ `failed`). For Vibeke-only enforcement (deny rules on yolo runs), a fail-closed case: server unreachable → tool denied or escalated to the harness prompt, never allowed. For gate mode, a release-on-focus case: focusing the pane releases the held hook and the native dialog appears exactly once. For pi, an extension-dialog case: a third-party permission extension's `confirm` is answered natively in RPC mode and answered via the uiContext wrapper in TUI mode (both native-first and Vibeke-first, native dialog dismissed exactly once; caller timeout/abort honoured; wrapper re-applied after `/reload`), and detected on screen when the wrapper is disabled. The keystroke verifier needs cases for: pointer already on target, move up, move down, wrap-around, dialog redraw mid-sequence (must abort), and user typing mid-sequence (must abort).

### 12.3 Version drift detection

- On every detection, `[version] command` runs (cached by exe path + mtime). A version outside the manifest's validated range, or newer than the latest golden recording, emits `agent.harness_version_unvalidated`, sets adapter health `unvalidated_version`, and **reduces the run's capabilities to `observe`** (+ `answer_keystroke` if the screen manifest still validates on the live screen). The sidebar shows a subtle "?" on the run's state badge, and `vibeke doctor` lists it.
- *Implemented (Goal 02):* the version is read once per harness per server lifetime (`<harness> --version`, off the state path; parses `omp/17.2.12`, `codex-cli 0.160.1`, `2.1.290 (Claude Code)`); validated ranges live in `agents/harness.rs::validated` (Claude 2.1.x, Codex 0.157–0.160, pi 0.84–0.90, omp 17.x — the versions the adapters were built against, **not** golden-replay-verified). Outside them: health `unvalidated_version`, capabilities `observe` + `answer_keystroke`, native answering withheld, `agent.harness_version_unvalidated`.
- **Disagreement telemetry** (local only)
- *Turn/item capture for tracking (15 T1, implemented):* `UserPromptSubmit`/`TurnStarted` record a Turn (exact prompt ≤ 8 KiB, native conversation id); `PreToolUse`/`PostToolUse` (and the extension's `ToolStarted/ToolEnded`) record tool items with command, cwd and exit code when the harness reports one (Claude's Bash `tool_response` has none → shown as unknown). A `SessionStart` with a different `session_id` on the same run (`/clear`, `/new`, resume elsewhere) suspends active task bindings; compaction keeps them.: counts of adapter-vs-screen disagreements, interactions resolved by an unknown path, and answer failures, per harness version. A spike after an upgrade raises a one-time notification: "Claude 2.2.0 behaves differently than Vibeke expects; exact-state may be degraded. `vibeke integration doctor claude`".
- **Opt-in anonymous report** (off by default): `vibeke report harness-drift` produces a redacted bundle (version, rule ids that failed, no screen content unless the user includes it) for the maintainers.
- *Implemented (M2):* `vibeke integration doctor [--json] [<id>]` runs each manifest's `[version] command` (only when the binary is on `PATH`) and reports `version`, `validated_range`, `status = validated | unvalidated | not_installed`, the capabilities a run would get (observe + keystrokes when unvalidated) and the documented-but-unverified (✓?) set, plus the integration file state; `vibeke integration capabilities` prints the same matrix. Manifest-driven harnesses gate on the manifest's range (or a user/repo `*` row). The server's `agent.manifests` lists loaded manifests with sources and warnings; `agent.manifests_reload` re-reads them. Disagreement telemetry, the drift notification and the nightly CI remain open.
- **Nightly CI** installs the latest published version of each harness (npm/brew/cargo), re-records the scenarios against the mock LLM, and opens an issue automatically on golden diffs. Target: adapters updated within 48 h of a breaking harness release.

---

## 13. Manifest update channel

Harness UIs change faster than Vibeke releases. Vibeke fetches remote detection manifests, signed and auditable:

- **Channel URL**: `https://manifests.vibeke.dev/v1/{stable|preview}/index.json`, polled every 6 h and on `vibeke integration update`. Disable with `[update] manifest_check = false`.
- **Index**: `{ serial, created_at, manifests: [{id, version, sha256, url, min_vibeke, max_vibeke}] }`.
- **Signing**: minisign/ed25519, verified against public keys compiled into the binary (two keys for rotation). Unsigned or unknown-key payloads are rejected. `serial` must strictly increase, which prevents rollback.
- **Scope**: only declarative content (detect rules, screen rules, state maps, version ranges, accelerators). **Never executable code, and never installer actions.** Remote manifests cannot change `[launch]` or `[resume]` argv, `[integration] install`, or anything that spawns processes. Those fields are stripped on load, with a warning.
- **Capabilities over the channel**: a remote manifest may extend `[[capabilities]]` to a new version range only with a `golden_run` attestation (CI run id + corpus hash) from the maintainers' drift CI (§12.3); without it, new ranges stay `observe`-only. User manifests can grant capabilities locally, shown as "user-asserted" in `vibeke integration capabilities`.
- **Precedence**: user manifests > remote manifests > compiled-in built-ins. An unknown harness id in a remote index is skipped without warnings.
- **Audit**: every applied update emits `harness.manifest_loaded {id, version, source: remote, serial}`. `vibeke integration list --sources` shows provenance, and `vibeke integration pin <id> <version>` freezes a manifest.

*Implemented (M2, client side only, `agents/channel.rs`):* `vibeke integration update [--url U]` is the **only** trigger (no 6-hourly polling; nothing enables it automatically; `VIBEKE_MANIFEST_CHANNEL=0` refuses even explicit updates; `https://` or `file://` only). It fetches `index.json` + `index.json.minisig` and verifies the minisign signature against the embedded release keys (`vk_remote::bootstrap::trusted_keys`: current and next; the channel has no key of its own); an index without a valid signature is refused (the error names the expected key ids) unless `VIBEKE_ALLOW_UNSIGNED_MANIFESTS=1` (development opt-in, loudly warned; recorded as `verified = "unsigned-dev"`). Then: `serial` must exceed the cached serial (rollback refused), only built-in ids are taken (unknown ids skipped silently), `min_vibeke..max_vibeke` is honoured, every file's sha256 must match the index, and the set is staged and swapped into `<state>/manifests/remote/` atomically with `remote-state.json`. The loader strips `[launch]`, `[resume]`, `[adapter]`, `integration.install` and `version.command` from remote manifests and drops capability rows without a `golden_run` attestation. Not implemented: the public channel itself, `harness.manifest_loaded` events, `--sources`, `pin`.

---

## 14. Implementation order (milestones per [11](11-milestones.md))

1. **M0**: harness reality check — every **[verify M0]** item in this section run against the live binaries; delivery-transaction prototype against Claude hooks and the pi extension.
2. **M1** (supervision slice): process detection plus the manifest loader; screen DSL engine with Claude, Codex, pi and omp screen manifests (fallback); state facets + arbiter (§2.4–2.5); CapabilityResolver and generated matrix; Claude hooks adapter (full §6.1); observe-only pi/omp extension with snapshot-on-reconnect; pi extension-dialog Interactions (TUI uiContext wrapper + RPC protocol); pi generic-dialog screen manifest as fallback; Codex hooks + PATH shim + per-pane embedded server; Interaction model with the delivery transaction (§7.3), observe/gate modes with release-on-focus and cards for unfocused panes, policy fast-path, best-effort verified keystrokes; transcript tailer for history and resume; user-typed yolo detection; `vibeke integration` command; golden corpus (incl. delivery chaos cases) for those 4 harnesses with the mock LLM.
3. **M2**: OpenCode plugin; Gemini hooks; ACP-generic; custom harness manifests (`espi`, Hermes); compat self-report; Codex display-only daemon observer + explicit linking; usage/cost/rate-limit extraction and transcript-based reconcile; `[sandbox]`/`[auth]` manifest sections used by containers (13); external adapter protocol; signed manifest channel; nightly drift CI.
4. **M3**: headless adapters (pi/omp RPC, codex app-server, claude stream-json, ACP generic) under holders in pipe mode — needed for remote image delivery (06) and later for the Phase 2 gateway.

### 14.1 M2 harness work: what is unverified (2026-10-06)

Built in M2 (see the *Implemented (M2)* notes above): manifests + registry + repo trust, OpenCode plugin and Gemini hooks (+ installers), ACP host, compat self-report, synthetic golden corpus + replay + `integration doctor`, signed-channel client, usage/rate-limit extraction. Exercised only by unit tests and fake harnesses (`crates/vibeke/tests/harnesses.rs`); **nothing was run against a live OpenCode, Gemini CLI, Hermes or third-party ACP agent, and no model was called.** Still **[verify M2]**:
- OpenCode: plugin directory (`plugin/` vs `plugins/`), hook/event names and payload shapes, `permission.ask` output semantics, TUI permission dialog geometry, `--session`/`--prompt`/`opencode acp` flags.
- Gemini CLI: hook names/payload fields, settings shape (enable flag, `timeout` unit, `name` field), whether a hook can answer the tool confirmation, dialog geometry, `--resume`/`--prompt-interactive`/`--experimental-acp` flags.
- Hermes: detection paths, `--resume`, `hermes acp`; the Vibeke Hermes plugin (`~/.hermes/plugins/vibeke-agent-state/`) is not written — Hermes is screen + self-report only.
- ACP: real agents' option kinds, `loadSession` resume, `usage` reporting; `terminal/*` is offered on the headless path only (fake agent tested), not by the pane-hosted host.
- Golden corpus: all fixtures are synthetic; capabilities are still not generated from golden coverage; no recorder, mock LLM or nightly drift CI.
- `[sandbox]` (`read`, `write`, `[sandbox.network] allow`) and `[auth]` (`env`, `files`, `home_env`) manifest sections: built (lane 2E, 13 §15.1), filled for every built-in, honoured only from built-in and user manifests (stripped from repo and remote-channel manifests).
- Not started from the M2 list: Codex display-only daemon observer + linking, external adapter protocol (§3.4), Cursor/Copilot/… screen manifests (§6.7).
