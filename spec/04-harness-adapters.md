# 04 — Harness adapters

> How Vibeke understands coding agents. Vibeke works out agent state from structured signals first and falls back to reading the screen only where no structured transport exists.

Status of facts: verified 2026-10-05 against installed versions **Claude Code 2.1.289**, **Codex CLI 0.157.1**, **pi 0.84.1** (`@earendil-works/pi-coding-agent`), **omp 17.2.12** (`@oh-my-pi/pi-coding-agent`). Items marked **[verify M2]** are documented upstream but not yet exercised against a live binary. They must be confirmed by the golden corpus (§12) before the adapter ships.

---

## 1. Goals and non-goals

**Goals**

1. Every agent state carries a `source` and a `confidence` (02 §1.1). Where a harness exposes structured signals, Vibeke uses them, so it never guesses where it could know.
2. Approvals, questions and plan reviews become `Interaction` objects. They can be answered from any client (TUI overlay, CLI, API; mobile in Phase 2). Answers go back through the harness's **native** channel when one exists, and through **verified keystrokes** otherwise.
3. **Bring your own harness.** pi with personal extensions, omp, Hermes, a shell wrapper around any of these, or a brand-new CLI can all be described by a TOML manifest. They plug into the same adapter interface that the built-in adapters use.
4. Every resumable harness gets a resume handle, so panes come back after a reboot (01 §1.2).
5. When a harness ships a new version, Vibeke detects it and the golden corpus (§12) re-validates the adapter. No silent breakage.

**Non-goals**

- Vibeke does not replace a harness's own permission system. It answers the prompts the harness raises. It never widens permissions beyond what the harness's own mechanism allows (02 §4).
- Phase 1 does not include rich conversation UIs for headless mode. Phase 1 ships a minimal transcript view; Phase 2 builds the rich UI on the same events.

---

## 2. Integration tiers

Each harness run uses the **highest available tier**. Tiers can be layered: for example, Claude in TUI mode uses tier 1 (hooks) for state and tier 4 (screen) as a liveness cross-check.

| Tier | `integration` value | Mechanism | State fidelity | Interactions | Answer channel |
|---|---|---|---|---|---|
| **1a Native hooks** | `hooks` | Harness runs `vibeke hook <harness> <event>` on lifecycle events (Claude, Codex, Gemini) | High | Full, from hook payloads | `native` (blocking hook decision), else verified keys |
| **1b In-process extension** | `extension` | A Vibeke extension loaded by the harness talks to the socket (pi, omp, OpenCode, Hermes) | High | Full | `native` (extension blocks the tool call / answers UI) |
| **2 Headless protocol** | `rpc` / `app_server` / `acp` | Vibeke spawns the harness in its machine protocol and *is* the client: `pi --mode rpc`, `omp --mode rpc-ui`, `codex app-server`, `claude -p --output-format stream-json --input-format stream-json`, any ACP agent | Highest (turns/items/usage) | Full, first-class requests | `native` (protocol response) |
| **3 Self-report** | `self_report` | Agent or wrapper calls `vibeke agent report …` or the Herdr-compatible `pane.report_agent`/`pane.report_agent_session` | As good as the reporter | Optional | Whatever the reporter supports |
| **4 Screen manifest** | `screen` | Region regex/heuristic matching on the VT grid (§9) | Medium, confidence < 1 | Extracted from screen | Verified keystrokes |
| **5 Process only** | — | Foreground process detected, no other signal | `working`/`unknown`/`exited` only | None | — |

Rules:

- **Precedence** follows 02 §1.1: adapter (tiers 1–2) > self_report > screen > process. A lower tier may only override a higher one after the higher one has been silent for `stale_after`. The UI then marks the state as *inferred*.
- **Cross-check.** When tiers 1–2 say `working` but the screen manifest has matched an approval dialog for more than 3 s, an `adapter.disagreement` diagnostic is logged and the state becomes `needs_approval` with `source=screen`. A missed hook must never hide a blocked agent from the user. Disagreement counters per harness version feed drift detection (§12.3).
- **No silent failure.** If a harness has an available tier-1 integration but it is not installed or not trusted (e.g. Codex hook trust), `vibeke doctor` and the sidebar show a one-time hint: "Claude in w3:p5 is screen-detected; run `vibeke integration install claude` for exact state".

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
                 │                                  │  └─ ManifestAdapter (generic, tiers 3–5)
                 │                                  ▼                      │
                 │                    StateArbiter (precedence, staleness) │
                 │                                  ▼                      │
                 │                 command bus → state actor → event log   │
                 └──────────────────────────────────────────────────────────┘
```

- **ProcessWatcher**: for each pane, polls the holder's `Status` (fg pgid and cmdline) on change notifications from the holder, plus every 1 s as a fallback. It walks the foreground process group's tree (macOS `proc_listchildpids`/`proc_pidpath`, Linux `/proc/<pid>/{stat,cmdline,exe}`).
- **HarnessMatcher**: matches the process tree against manifest `[detect]` rules (§5.2). The output is `agent.detected {harness, via}`.
- **AdapterHost**: owns one adapter instance per live `AgentRun`. Adapters run as tokio tasks behind a `catch_unwind` boundary (01 §7.5).
- **StateArbiter**: applies precedence and staleness and emits `agent.state_changed`. It is the only component that writes `AgentRun.state`.
- **TranscriptTailer**: an optional per-run file tail (§10) that provides history, usage and a recovery path for any events a hook missed.
- **ScreenDetector**: runs manifest screen rules on damage, rate-limited to 10 Hz per pane (§9).

### 3.2 The `Adapter` trait

```rust
/// One instance per AgentRun. Built-in and external adapters implement the same trait;
/// external (out-of-process) adapters are wrapped by `ExternalAdapter`, which speaks the
/// same calls over JSON-RPC (§3.4).
#[async_trait]
pub trait Adapter: Send + 'static {
    /// Static capabilities, used by UI/clients to decide what to offer.
    fn capabilities(&self) -> AdapterCaps;

    /// Called once the run is created. `ctx` gives access to the pane (send input, read screen),
    /// the run record, config, the blob store, and the event sink.
    async fn start(&mut self, ctx: AdapterCtx) -> Result<()>;

    /// Inbound signal from a hook shim / extension / self-report for this run.
    async fn on_signal(&mut self, sig: AdapterSignal) -> Result<SignalReply>;

    /// Screen detector produced a match (tier 4). Adapters at higher tiers usually just cross-check.
    async fn on_screen(&mut self, m: ScreenMatch) -> Result<()>;

    /// Deliver a human/policy answer to an open Interaction.
    async fn answer(&mut self, interaction: &Interaction, answer: &Answer) -> Result<AnswerOutcome>;

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
    pub integration: Integration,          // hooks | extension | rpc | app_server | acp | self_report | screen
    pub native_answers: InteractionKinds,  // which kinds can be answered natively
    pub can_gate: bool,                    // can hold a tool call until Vibeke decides (policy)
    pub turns_items: bool,                 // emits Turn/Item
    pub usage: bool,                       // emits token/cost usage
    pub resume: bool,
    pub subagents: bool,
    pub steer: bool,                       // can queue messages mid-turn
}

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

`SignalReply` is how an adapter returns a synchronous decision to a blocking hook or extension: `Continue`, `Decision(HookDecision)`, or `Wait(WaitToken)`. The shim keeps its connection open until it receives the decision or reaches its timeout (§7.3).

### 3.3 Run lifecycle

```
process detected ──► AgentRun{state: starting, integration: best-available}
   │  (or agent.start via API → Vibeke launches argv itself, pre-assigning session ids where the harness allows)
   ├─► SessionStarted        → agent.identified {harness_session_id, transcript_path}; resume handle computed
   ├─► TurnStarted/ToolStarted → working
   ├─► InteractionOpen        → needs_approval | needs_answer   (+ interaction.opened)
   ├─► InteractionResolved / answer delivered → working
   ├─► TurnEnded              → done (if pane unseen) | idle (if focused by an attached client)
   ├─► Error{retrying:false}  → error ;  RateLimited → rate_limited
   ├─► process exits / SessionEnded → exited (+ agent.exited, open interactions → cancelled)
   └─► harness replaced in same pane (e.g. user quits claude, starts codex) → old run ended(released), new run
```

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
VIBEKE_TASK_ID=k7 (if any)  VIBEKE_RUN_TOKEN=<random 128-bit, per pane>
# compat (compat.herdr_env = true, default true in M1–M4):
HERDR_ENV=1  HERDR_PANE_ID=w3:p5  HERDR_SOCKET_PATH=…/herdr-compat.sock  HERDR_WORKSPACE_ID=w3  HERDR_TAB_ID=w3:t2
```

- `VIBEKE_RUN_TOKEN` authenticates adapter calls as coming from *that pane's* process tree. The socket already restricts access to the same UID. The token stops one pane's agent from posting states for another pane by mistake, e.g. a stale env in a nested shell that was moved panes.
- Adapter methods on the control API live in a separate `adapter.*` namespace. They require the token, except `adapter.report_self`, which can also be called with an explicit `--pane` by the user.

### 4.1 Adapter API methods (subset of 07)

| Method | Purpose | Blocking |
|---|---|---|
| `adapter.signal {token, harness, signal}` | Deliver any `AdapterSignal` | No (ack < 1 ms) |
| `adapter.gate {token, harness, interaction}` | Open an interaction and wait for a decision | Yes, until decision or `timeout_ms` |
| `adapter.report_self {pane?, harness, state, message?, seq, resume_argv?}` | Tier-3 self-report | No |
| `pane.report_agent` / `pane.report_agent_session` | **Herdr-compatible** self-report, accepted on the compat socket and the main socket with Herdr's param names (`pane_id, source, agent, state: idle\|working\|blocked, message, seq, agent_session_id, agent_session_path, session_start_source`) | No |

`seq` handling is the same as Herdr's. Reports with `seq` ≤ the last seen seq from the same `source` are dropped. This keeps existing Herdr integrations (including Herdr's own omp extension, which debounces states with a time-based `seq`) working unchanged. Herdr's `blocked` maps to `needs_approval` with `confidence 0.8`, because Herdr can't distinguish approvals from questions.

---

## 5. Harness manifests

Every harness, built-in or user-defined, is described by a TOML manifest.

- **Built-ins** are compiled into the binary and also written to `~/.local/share/vibeke/harnesses/builtin/*.toml` for reference.
- **User manifests** live in `~/.config/vibeke/harnesses/*.toml` and can override a built-in by `id`, or extend it with `extends = "pi"`.
- **Repo-local manifests** (`.vibeke/harnesses/*.toml`) load only for trusted repos.
- **Updated manifests** arrive through the signed manifest channel (§13).

### 5.1 Schema (abbreviated; the full JSON Schema is generated from `vk-agents::manifest`)

```toml
schema = 1
id = "claude"                       # [a-z][a-z0-9_-]{0,31}; unique
name = "Claude Code"
extends = ""                        # optional parent id; tables deep-merge, arrays replace
min_version = "2.1.0"               # below: degrade to screen tier with a doctor warning
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
preferred = "hooks"                                   # hooks | extension | rpc | app_server | acp | self_report | screen
install = "builtin:claude"                            # installer id (§11) or ["cmd", …] for custom
headless = "stream_json"                              # which headless adapter, if any

[states]
# Maps harness-native event names to Vibeke transitions. Built-in adapters ship these as code,
# but declaring them here keeps them visible and overridable.
"hook:UserPromptSubmit" = "working"
"hook:PreToolUse"       = "working"
"hook:PermissionRequest"= "needs_approval"
"hook:Stop"             = "done"
"hook:StopFailure"      = "error"
"hook:SessionEnd"       = "exited"

[interactions]
# Tool names that mean "question" or "plan review" rather than "approval".
question_tools = ["AskUserQuestion"]
plan_tools     = ["ExitPlanMode"]

[answer]
native = ["approval", "question", "plan_review"]     # kinds answerable natively
keystrokes = "screen:claude"                         # screen manifest providing dialog geometry for fallback

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
preferred = "extension"
install = "builtin:hermes"                      # drops ~/.hermes/plugins/vibeke-agent-state/
headless = "acp"                                # hermes acp mode if available [verify M2]
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
preferred = "self_report"      # mybot calls `vibeke agent report --state working` itself
[screen]
manifest = "mybot"             # defined inline below
[[screen.rules]]
id = "approval"
state = "needs_approval"
region = { rows = "-12..-1" }
all = ['(?i)allow (this|command)\?', '\[y/N\]']
confidence = 0.85
interaction = { kind = "approval", options = [{ id = "allow", keys = ["y", "enter"] }, { id = "deny", keys = ["n", "enter"] }] }
```

---

## 6. Built-in harnesses

Each subsection lists: manifest highlights, the event-to-state mapping, how interactions are opened and answered, TUI mode versus headless mode, and known pitfalls.

### 6.1 Claude Code (`claude`)

**Integration:** tier 1a hooks in TUI mode. Tier 2 stream-json in headless mode (and the Agent SDK in Phase 2).

**Hooks installed** (by `vibeke integration install claude`, §11). Every hook is `{"type":"command","command":"<VIBEKE_BIN> hook claude <Event>","timeout":N}`, merged into `~/.claude/settings.json`:

| Hook event (2.1.289) | Matcher | Mode | Vibeke signal → state |
|---|---|---|---|
| `SessionStart` | `*` (startup/resume/clear/compact/fork) | async | `SessionStarted{session_id, transcript_path, source}` → `starting`→`idle`; identity + resume handle |
| `UserPromptSubmit` | — | async | `TurnStarted{prompt_preview}` → `working` |
| `PreToolUse` | `*` | **sync** (gate-capable, timeout 600 s) | `ToolStarted` → `working`. For `AskUserQuestion` → question Interaction (§6.1.2). For policy-matched tools → native decision (§7) |
| `PermissionRequest` | `*` | **sync** (gate-capable) | `InteractionOpen{approval}` → `needs_approval`. For `ExitPlanMode` → `plan_review` |
| `PermissionDenied` | — | async | resolves interaction (denied by auto mode) |
| `PostToolUse` / `PostToolUseFailure` | `*` | async | `ToolEnded`; `Edit`/`Write`/`MultiEdit`/`NotebookEdit` → `FileChanged` from `tool_input.file_path`; resolves any open approval for that `tool_use_id` as `answered_elsewhere` if Vibeke didn't answer it |
| `Notification` | `permission_prompt`, `idle_prompt`, `elicitation_dialog`, `agent_needs_input`, `agent_completed`, `quota_auto_resume_*`, `auth_success` | async | cross-check (`permission_prompt` with no open interaction → open one from screen extraction); `idle_prompt` → `done`/`idle`; `quota_auto_resume_*` → `rate_limited` |
| `Stop` | — | async | `TurnEnded{stop_reason, last_assistant_message preview}` → `done`/`idle` |
| `StopFailure` | `*` (rate_limit, overloaded, authentication_failed, server_error…) | async | `rate_limit` → `rate_limited`; others → `error` |
| `SubagentStart` / `SubagentStop` | `*` | async | `SubagentStarted/Ended{agent_id, agent_type}` |
| `PreCompact` / `PostCompact` | `*` | async | `Compacting{start/end}` (state stays `working`, detail "compacting") |
| `CwdChanged` | — | async | updates run cwd (pane cwd still comes from OSC 7) |
| `WorktreeCreate` / `WorktreeRemove` | — | async | `worktree.*` events linked to the task (05) |
| `Elicitation` | `*` | **sync** | MCP elicitation → question Interaction (native answer via hook output) **[verify M2]** |
| `SessionEnd` | `*` | async (1.5 s budget) | `SessionEnded{reason}`; run → `exited` once process exits, or stays if `reason=clear` |

"async" means the shim sends the signal and exits immediately. It does not use Claude's `async: true`, because ordering matters and our shim is fast enough (§7.4). "sync" means the shim may hold the hook open while waiting for a decision (§7.3).

**Common payload fields used:** `session_id`, `transcript_path`, `cwd`, `hook_event_name`, `permission_mode`, `agent_id`/`agent_type` (present inside subagents), `tool_name`, `tool_input`, `tool_use_id`, `prompt_id`.

#### 6.1.1 Approvals (native)

- `PermissionRequest` → Interaction `approval` with:
  - `action.tool = tool_name`;
  - `command` from `tool_input.command`, `paths` from `tool_input.file_path`, `diff` computed for `Edit` (`old_string`/`new_string`) and `Write`;
  - `risk` from §7.5.
- The answer is written as hook output:

```json
{"hookSpecificOutput":{"hookEventName":"PermissionRequest",
  "decision":{"behavior":"allow","updatedPermissions":{"rule":"Bash(pnpm test:*)"}}}}
```

- `allow_always` maps to `updatedPermissions` only if the user picked "always" **and** `approvals.claude.persist_always = true`. Otherwise "always" becomes a Vibeke session-scoped policy rule (02 §4), so Claude's own settings files are never edited without consent.
- `deny` → `{"behavior":"deny","message":"<user text or 'Denied from Vibeke'>","interrupt":false}`. "Deny and stop" sets `interrupt: true`.
- If the shim times out or the server is gone, the shim prints nothing and exits 0 (fail-open). Claude then shows its native dialog, and screen + keystroke fallback applies.

#### 6.1.2 Questions (`AskUserQuestion`) and plan review (`ExitPlanMode`)

- `PreToolUse` with `tool_name = "AskUserQuestion"` opens a `question` Interaction from `tool_input.questions[]` (`question`, `header`, `options[{label, description}]`, `multiSelect`).
- **Native answer** **[verify M2]**: return `permissionDecision: "allow"` with `updatedInput` containing the `answers` map in the shape the Agent SDK's `canUseTool` uses for AskUserQuestion. If the golden test shows 2.1.x ignores `answers` coming from hooks, the adapter switches that harness version to **verified keystrokes** (§8) automatically, using the `claude` screen manifest's question-dialog geometry.
- `ExitPlanMode` arrives through `PermissionRequest` (or `PreToolUse`). It opens a `plan_review` Interaction with `plan_md = tool_input.plan`.
  - Approve → `behavior: allow`.
  - Reject with feedback → `behavior: deny` with `message` = the feedback. Claude stays in plan mode and revises.

#### 6.1.3 TUI vs headless

| | TUI mode (default) | Headless mode (`mode = headless`) |
|---|---|---|
| Launch | `claude [--session-id X]` in a PTY | `claude -p --output-format stream-json --input-format stream-json --verbose --session-id X [--permission-prompt-tool …]` driven by `StreamJsonAdapter` |
| State | hooks | stream events (`system/init`, `assistant`, `user`/`tool_result`, `result` with `usage`, `total_cost_usd`) |
| Approvals | hooks | hooks still installed (they fire in `-p` too); or `--permission-prompt-tool` pointed at Vibeke's MCP permission tool **[verify M2]** |
| Pane content | Claude's own TUI | Vibeke's minimal transcript renderer (01 §3.3) |
| Use | interactive work | `vibeke task run`, Phase 2 mobile-originated tasks, scripted fan-out |

#### 6.1.4 Pitfalls

- `--agent` mode: AskUserQuestion can show as idle under `claude --agent` if detection relies on the screen. Hooks fire regardless, so this is fixed by tier 1. Keep a golden case for it.
- `bypassPermissions`/`dontAsk` modes: `PermissionRequest` never fires. That is expected, and the UI shows the permission mode badge from `permission_mode`.
- Subagent hooks carry `agent_id`. Interactions raised inside subagents are attributed to the subagent Item, but surface on the parent run.

### 6.2 Codex CLI (`codex`)

**Integration:** tier 1a hooks in TUI mode (embedded app-server), and tier 2 app-server (as observer or as owner).

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
| `PermissionRequest` | approval Interaction → `needs_approval`; native answer `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"\|"deny","message"?}}}` |
| `PostToolUse` | `ToolEnded` |
| `Stop` | `TurnEnded` → `done`/`idle` |
| `Interrupt` | turn interrupted → `idle` |
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

**The shared daemon problem**. Codex ≥ 0.15x defaults to `daemon_auto_start = true`. The TUI connects to a per-user app-server daemon (`~/.codex/app-server-control/app-server-control.sock`), and **hooks run in the daemon's environment, not the pane's**. `VIBEKE_PANE_ID` is therefore missing, and several panes share one process. Vibeke handles this with three strategies, in order:

1. **Per-pane embedded server (default for Vibeke-launched Codex).** When Vibeke launches Codex (`agent start`, tasks, resume), it adds `--disable daemon_auto_start`. The TUI then runs its own embedded app-server under the pane's env, and hooks carry `VIBEKE_PANE_ID`. **[verify M2]** that the flag yields an embedded server in 0.157; if not, use `-c` with the equivalent config key.
2. **Daemon observer (for user-launched Codex attached to the shared daemon).**
   - The Codex adapter connects to the daemon control socket as an additional app-server client (`initialize` with `clientInfo.name = "vibeke"`).
   - It calls `thread/loaded/list` and subscribes to thread notifications.
   - It **correlates threads to panes** using:
     - (a) the TUI process in the pane: its start time ≈ `thread/started` time, with the same `cwd`;
     - (b) **input correlation**: the text the user typed or pasted into the pane (Vibeke sees pane input) matched against `turn/started`'s user item text within 2 s;
     - (c) the thread `name` if set.
   - Correlation confidence is recorded. Below 0.9 the run shows "codex (shared daemon, unlinked)", and the user can link it manually with `vibeke agent link w3:p5 --thread <id>`.
   - Hooks arriving without `VIBEKE_PANE_ID` but with a `session_id` matching a correlated thread are routed to that run.
3. **Screen fallback** when neither is possible.

**PATH shim for user-typed Codex (makes strategy 1 the common case).** Vibeke prepends `~/.local/share/vibeke/shims` to `PATH` in every pane (`agents.shims = true`, default on; per-harness opt-out). The `codex` shim is a tiny exec wrapper: it locates the real `codex` later in `PATH`, adds `--disable daemon_auto_start` (unless the user passed an explicit daemon flag or `CODEX_VIBEKE_SHIM=0`), and `exec`s it with **all user arguments untouched** — so `codex -a never -s danger-full-access` behaves exactly as typed, just with a per-pane embedded server whose hooks carry `VIBEKE_PANE_ID`. The same shim mechanism is available to any harness manifest (`[launch] shim_args = [...]`). `vibeke doctor` warns when an alias/function in the user's shell shadows the shim. **[verify M2]** that hooks (`SessionStart`, `PreToolUse`, `Stop`) still fire under `-a never` / `danger-full-access`; `PermissionRequest` will not, by design.

**Headless mode:** `codex app-server` (stdio) owned by `AppServerAdapter`, one process per run (or one per Vibeke session, multiplexing threads, behind `codex.headless_shared = true`).

### 6.3 pi (`pi`) and omp (`omp`)

These two are one adapter family. omp (oh-my-pi) is a fork of pi. It keeps pi's extension model and ships a legacy shim, so extensions importing `@earendil-works/pi-coding-agent` or `@mariozechner/pi-coding-agent` resolve to omp's API (`src/extensibility/legacy-pi-coding-agent-shim.ts`, `plugins/legacy-pi-compat`).

**Integration:** tier 1b via **`@vibeke/pi-extension`** (one package for both; design in `integrations/pi-extension/DESIGN.md`). Tier 2 via RPC (`pi --mode rpc`, `omp --mode rpc` / `rpc-ui`).

**Where the extension loads:**
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
| `tool_execution_start` / `_end` | ✓ | ✓ | `ToolStarted` / `ToolEnded`; `write`/`edit` → `FileChanged{input.path}` |
| `tool_call` (**can block**) | ✓ | ✓ | policy gate + optional Vibeke approval gate (§7) |
| `tool_approval_requested` / `_resolved` | — (pi has no built-in approvals) | ✓ (`toolCallId, toolName, reason, approvalMode` / `approved`) | approval Interaction opened/resolved (observe; answer via keys, see below) |
| `agent_end` | ✓ | ✓ | provisional `done` (debounced 250 ms) |
| `agent_settled` | ✓ | — | definitive `done`/`idle` (pi only) |
| `session_stop` | — | ✓ (can veto) | definitive `done`/`idle` for omp |
| `auto_retry_start/end` | ✓ | ✓ | `error{retrying:true}`; `rate_limited` if message matches rate-limit |
| `compaction_*` / `auto_compaction_*` / `session_compact` | ✓ | ✓ | `Compacting` |
| `model_select` | ✓ | — | run.model |
| `session_shutdown` | ✓ | ✓ | `SessionEnded` |
| `goal_updated`, `todo_reminder` | — | ✓ | Phase 2 task/plan items (stored as `Raw`) |

**Approvals:**
- **pi** has no permission system by design ("runs with all permissions"; `docs/security.md`). The Vibeke extension adds an **optional gate**: when `VIBEKE_GATE=1`, or when policy rules exist for the workspace, `tool_call` calls `adapter.gate` and returns `{ block: true, reason }` on deny. In the TUI, `ctx.ui.confirm` is *not* used; the Vibeke overlay is the dialog. When gating is off, pi never blocks.
- **omp** has native approval modes. The extension observes `tool_approval_requested` and opens an Interaction with `answer_channel = keystrokes` (the omp dialog is in the TUI). Native answering is still open: if `tool_call` fires *before* omp's approval prompt for the same `toolCallId`, the gate can pre-decide (allow → omp still asks unless its mode is permissive; deny → `block`). **[verify M2]** the ordering. If omp exposes an approval-resolve API to extensions in a later version, the manifest bumps `answer.native`.
- **RPC/RPC-UI mode:**
  - pi's Extension UI Protocol (`extension_ui_request` with `select`/`confirm`/`input`/`editor` on stdout, responses on stdin) maps 1:1 to `question`/`approval` Interactions with native answers.
  - omp's `rpc-ui` mode is the equivalent and also carries tool approvals **[verify M2]**.
  - `steer`/`follow_up` map to `agent.prompt --steer|--follow-up`.
  - `abort` maps to `interrupt`.
  - `get_session_stats` provides usage.

**Identity and resume:**
- pi: transcripts at `~/.pi/agent/sessions/--<cwd-slug>--/<ts>_<uuid>.jsonl`; resume `pi --session <path|id>`; pre-assign `pi --session-id <id>`; fork `pi --fork <id>`.
- omp: `~/.omp/agent/sessions/…`; resume `omp --resume <id|path>`; pre-assign **[verify M2]**.
- omp can import Claude and Codex sessions (`--from-claude`, `--from-codex`). This is exposed as "Continue this Claude session in omp" in the pane menu.

### 6.4 OpenCode (`opencode`)

- **Tier 1b**: a plugin in `~/.config/opencode/plugins/vibeke.ts` (`export const Vibeke = async ({ project, client, $, directory, worktree }) => ({ event, "tool.execute.before", "permission.ask" … })`).
- **Events used:** `session.created`, `session.status`, `session.idle` → `done`, `session.error` → `error`, `permission.asked`/`permission.replied` → approval open/resolve, `tool.execute.before/after` → tool items, `file.edited` → `FileChanged`, `message.updated` → usage, `todo.updated` → Raw, `session.compacted`.
- **Native approval answering**: through the plugin's `permission.ask` hook, which sets `output.status = "allow" | "deny"`, or through the OpenCode SDK `client` **[verify M2]**.
- **Subagents**: OpenCode subagents could show idle when detection ignores child sessions. They are tracked from `session.created` with a parent id, so a parent stays `working` while any child session is busy.
- **Tier 2**: `opencode serve` HTTP API + SSE event stream, and ACP **[verify M2]**.

### 6.5 Gemini CLI (`gemini`)

- **Tier 1a**: hooks in `~/.gemini/settings.json`: `SessionStart`, `SessionEnd`, `BeforeAgent` (→ working), `AfterAgent` (→ done), `BeforeTool` (gate-capable, decision `deny`), `AfterTool`, `Notification` (tool confirmation → approval Interaction), `PreCompress`.
- **Gemini's "silence is mandatory" rule**: the hook's stdout must be exactly one JSON object. The shim writes diagnostics to stderr only.
- **Whether a hook can answer the native confirmation**: **[verify M2]**. Fallback is verified keystrokes.
- **Tier 2**: Gemini's ACP mode (Gemini CLI was ACP's first agent) through `AcpAdapter`.

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

### 6.7 Other built-ins (screen tier + self-report at launch)

Cursor agent, Copilot CLI, Devin CLI, Droid, Kimi, Kilo, Qoder, Mastra Code, Antigravity CLI, Grok, Amp, Aider, Letta, Muse and Qwen ship as manifests at tier 4. Where their CLIs offer hooks, ACP or plugin APIs, they are upgraded in the same manifest without code changes, either through `[integration] preferred = "acp"` or through an external adapter.

---

## 7. Interactions, gating and policy

### 7.1 Opening

An adapter opens an Interaction with `InteractionOpen{kind, native_ref, payload, gate}`. Then:

1. Run the policy check (02 §4). If a rule matches with `allow` or `deny`, decide immediately. The Interaction is recorded with `status: answered`, `answered_by: policy`, and `policy.rule_matched` is emitted. The agent never visibly blocks.
2. Otherwise, mark the Interaction `open` and set run state to `needs_approval` or `needs_answer`. Raise a notification (urgency from risk, §7.5).
3. Decide the **gate mode** (§7.2).

### 7.2 Gate modes: observe vs gate

There's a tension. If a sync hook blocks while waiting for Vibeke, the harness's own dialog never appears, so a user sitting at that pane in the TUI can't answer natively. If the hook returns immediately, remote answering needs keystrokes.

`approvals.mode` per harness or workspace (default `auto`):

| Mode | Behavior |
|---|---|
| `observe` | Shim reports the interaction and returns "no decision" at once. The harness shows its native dialog. Vibeke answers from other clients via **verified keystrokes** (§8). Resolution is observed through Post*/`serverRequest/resolved`/`tool_approval_resolved`/screen. |
| `gate` | Shim holds the hook or extension call until Vibeke decides, or until `gate_timeout`. The Vibeke TUI draws an **in-pane approval overlay** (bottom sheet inside the pane frame: summary, command/diff preview, `[a]llow  [A]lways  [d]eny  [D]eny+stop  [v]iew`). Any client can answer. On timeout the shim returns no decision and the native dialog appears (observe semantics from then on). |
| `auto` (default) | `gate` when the pane is **not focused by any attached client** at the moment the interaction opens, or when the run is headless, or when the user is remote (Phase 2). Otherwise `observe`. If an observed interaction stays unanswered for `escalate_after` (60 s) and the user leaves the pane, nothing changes; the native dialog stays up and remote answering uses keystrokes. |

`gate_timeout` defaults to 30 min. Claude's command-hook timeout is 600 s by default, so the installer sets the `PermissionRequest`/`PreToolUse` hook `timeout` to 1800 s. Codex hook timeouts are set to match.

### 7.3 Blocking protocol (shim side)

```
shim: connect VIBEKE_SOCKET → adapter.gate{token, harness:"claude", event:"PermissionRequest", payload, gate_hint}
server: policy fast-path → reply {decision} within ~1 ms            → shim prints hook JSON, exit 0
        or reply {wait: true, interaction:"i42"} and keep the request open
        … later → {decision} | {no_decision}                      → shim prints JSON or nothing, exit 0
shim: if the socket drops or the server restarts → reconnect once with adapter.gate_resume{interaction}
      (server state survives restarts; open interactions are in state.db) → else exit 0 (fail-open)
```

### 7.4 Shim performance and safety (`vibeke hook`)

- Same static binary, `hook` subcommand, with no tokio runtime (blocking std I/O on a Unix socket). It reads stdin (capped at 4 MiB), wraps it, sends it, and reads the reply.
- **Budget**: p50 ≤ 5 ms, p99 ≤ 15 ms for non-blocking events, measured in CI (`hyperfine` on Linux and macOS runners). No dynamic loading of config: the shim reads only env and stdin.
- **Fail-open everywhere**:
  - If not inside Vibeke (`VIBEKE` unset), exit 0 silently.
  - If the socket is missing, exit 0.
  - If the reply is malformed, exit 0 with no stdout.
  - The shim never prints to stdout except a valid decision JSON. Gemini requires this.
  - Exit codes other than 0 are never used. Exit 2 means "block" to Claude, Codex and Gemini, and a crash must not block a user's tool.
- **Payload hygiene**: `tool_input` values over 64 KiB are truncated in the event, with the full payload in the blob store. Env and secrets are never forwarded.

### 7.5 Risk scoring (Phase 1 heuristic; Phase 2 learns)

`risk` is computed on open:
- **high**: destructive shell patterns (`rm -rf`, `git push --force`, `git reset --hard`, `curl … | sh`, `chmod -R`, `sudo`, `DROP TABLE`, `kubectl delete`, writes outside the workspace root, editing `.env*`/credentials files, network to non-allowlisted hosts).
- **medium**: package installs, migrations, git commits/pushes, edits in more than 5 files.
- **low**: read-only commands, test/lint/build runners matched from `package.json` scripts / `Makefile` / `justfile` / `Cargo.toml`.
- **unknown**: everything else.

Risk drives notification urgency and the overlay's default focus. Deny is pre-selected for high risk.

### 7.6 Fingerprints for learning

Each answered approval stores `fingerprint = hash(harness, tool, normalized_command_prefix | path_glob, workspace)` with the decision. Phase 1 exposes `vibeke policy suggest`, which lists fingerprints approved ≥ N times with 0 denials, as ready-to-paste rules. Phase 2 surfaces this in the inbox.

---

## 8. Answer delivery by verified keystrokes

When no native channel exists (screen-tier harnesses, observe mode, omp's native dialog), Vibeke answers by typing. It must never hit a wrong option. The technique: send arrows, re-read the screen, and press Enter only when the pointer is on the tapped row.

Algorithm (in `vk-agents::keys::Verifier`):

1. **Locate the dialog.** Use the harness's screen manifest dialog rule (§9) to get the option rows, the pointer glyph/style (e.g. `❯`, reverse video, a specific SGR fg color), and the expected labels.
2. **Validate freshness.** The dialog must match the Interaction's options (label fuzzy match ≥ 0.9) and must not have changed since the last screen damage. Otherwise abort with `answer_failed{reason: "dialog_changed"}`.
3. **Plan the moves.** Read the current pointer row and compute the delta to the target row. Prefer direct accelerators when the manifest declares them (e.g. `1`/`2`/`3`, `y`/`n`), since those are atomic.
4. **Step and verify.** Send one move key, wait for damage (≤ 300 ms), re-read, and confirm the pointer moved to the expected row. Retry at most twice per step.
5. **Commit only on match.** When the pointer row's label equals the target label (and the pointer style matches), send Enter. Otherwise send nothing and fail.
6. **Confirm.** Wait ≤ 2 s for the dialog to disappear or for a native resolution signal. Emit `interaction.answer_delivered` or `answer_failed`.
7. **Multi-select / free text.** Toggle with Space and verify a checkbox glyph per row. Free text is typed with bracketed paste when the pane has it enabled, then verified by reading back the input line.

**Escape safety.** A user-requested Esc on a dialog Vibeke cannot parse requires a confirm step in the UI ("Send Esc to an unrecognized dialog?").

**Input locking.** While a verified sequence runs, other clients' input to that pane is queued (≤ 2 s) so keystrokes don't interleave. The local user's own typing aborts the sequence instead.

---

## 9. Screen detection manifests (tier 4)

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
state = "needs_approval"
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
state = "needs_answer"
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
- The output is a `ScreenMatch{rule_id, state, confidence, captures, dialog?}`, sent to the run's adapter, which decides (§2 precedence).
- **Unknown-dialog heuristic**: a boxed region with numbered options and a pointer glyph that matches no rule produces `needs_answer` with confidence 0.5 and a "Vibeke can't read this dialog" hint, rather than silently showing `idle`.

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
- Parses incrementally and emits `turn_completed{usage}` and `item` summaries only when no tier-1/2 source already provided them (dedupe by native ids).
- Writes compact per-turn records to `turns`/`items` (02).
- Raw transcripts are **never copied**. Vibeke stores pointers plus summaries. Full-text search indexes transcript text in FTS5 (`source = transcript`), and the index respects `search.index_transcripts = true|false`.

**Usage, cost and limits:**
- `Usage` is aggregated per run, task, workspace and day.
- Cost uses the harness-reported cost when present (pi `cost.total`, Claude `total_cost_usd` in stream-json). Otherwise it is computed from a bundled, signed price table keyed by model id. Subscription-billed runs show tokens, not dollars.
- Rate limits come from: Claude `StopFailure{rate_limit}` and `Notification{quota_auto_resume_*}`; Codex `account/rateLimits/updated` (with reset times); pi/omp `auto_retry_*` with rate-limit messages. They populate `rate_limited{resets_at}` and a per-account "limits" status segment.

---

## 11. `vibeke integration` command

```
vibeke integration list                    # built-in + user manifests, installed?, version seen, tier in use
vibeke integration install <id|all> [--dry-run] [--scope user|project]
vibeke integration status [<id>] [--json]  # per-harness: files, hook trust (codex), version compat, last signal seen
vibeke integration uninstall <id|all>
vibeke integration doctor [<id>]           # runs a synthetic round-trip: launches harness in a scratch pane if possible,
                                           # waits for SessionStart signal, reports tier achieved and latency
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
| `opencode` | `~/.config/opencode/plugins/vibeke.ts` | |
| `gemini` | `~/.gemini/settings.json` (`hooks`) | stdout-silence rule |
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

### 12.2 Required coverage before an adapter ships

Every row in the §6 mapping tables needs at least one golden case. The keystroke verifier needs cases for: pointer already on target, move up, move down, wrap-around, dialog redraw mid-sequence (must abort), and user typing mid-sequence (must abort).

### 12.3 Version drift detection

- On every detection, `[version] command` runs (cached by exe path + mtime). A version outside the manifest's validated range, or newer than the latest golden recording, emits `agent.harness_version_unvalidated`. The sidebar shows a subtle "?" on the run's state badge, and `vibeke doctor` lists it.
- **Disagreement telemetry** (local only): counts of adapter-vs-screen disagreements, interactions resolved by an unknown path, and answer failures, per harness version. A spike after an upgrade raises a one-time notification: "Claude 2.2.0 behaves differently than Vibeke expects; exact-state may be degraded. `vibeke integration doctor claude`".
- **Opt-in anonymous report** (off by default): `vibeke report harness-drift` produces a redacted bundle (version, rule ids that failed, no screen content unless the user includes it) for the maintainers.
- **Nightly CI** installs the latest published version of each harness (npm/brew/cargo), re-records the scenarios against the mock LLM, and opens an issue automatically on golden diffs. Target: adapters updated within 48 h of a breaking harness release.

---

## 13. Manifest update channel

Harness UIs change faster than Vibeke releases. Vibeke fetches remote detection manifests, signed and auditable:

- **Channel URL**: `https://manifests.vibeke.dev/v1/{stable|preview}/index.json`, polled every 6 h and on `vibeke integration update`. Disable with `[update] manifest_check = false`.
- **Index**: `{ serial, created_at, manifests: [{id, version, sha256, url, min_vibeke, max_vibeke}] }`.
- **Signing**: minisign/ed25519, verified against public keys compiled into the binary (two keys for rotation). Unsigned or unknown-key payloads are rejected. `serial` must strictly increase, which prevents rollback.
- **Scope**: only declarative content (detect rules, screen rules, state maps, version ranges, accelerators). **Never executable code, and never installer actions.** Remote manifests cannot change `[launch]` or `[resume]` argv, `[integration] install`, or anything that spawns processes. Those fields are stripped on load, with a warning.
- **Precedence**: user manifests > remote manifests > compiled-in built-ins. An unknown harness id in a remote index is skipped without warnings.
- **Audit**: every applied update emits `harness.manifest_loaded {id, version, source: remote, serial}`. `vibeke integration list --sources` shows provenance, and `vibeke integration pin <id> <version>` freezes a manifest.

---

## 14. Implementation order (feeds 10-milestones)

1. **M1**: process detection plus the manifest loader; screen DSL engine with Claude, Codex, pi and omp screen manifests; self-report; `vibeke hook` shim skeleton.
2. **M2**: Claude hooks adapter (full §6.1); pi/omp extension; Codex hooks + per-pane embedded server; Interaction model, observe/gate overlay, policy fast-path, verified keystrokes; `vibeke integration` command; golden corpus for those 4 harnesses with the mock LLM.
3. **M3**: Codex daemon observer and correlation; OpenCode plugin; Gemini hooks; transcript tailer plus usage/cost/limits.
4. **M4**: headless adapters (pi/omp RPC, codex app-server, claude stream-json, ACP generic); external adapter protocol; signed manifest channel; nightly drift CI.
