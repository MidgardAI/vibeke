# 00 — Vision and scope

## One sentence

**Vibeke is a terminal workspace for supervising coding agents. It understands agents structurally instead of by reading their screens, keeps every process alive through anything short of a reboot, and treats remote machines, previews and screenshots as if they were local.**

Phase 1 (this spec) is the terminal runtime. Phase 2 (outlined in [12-phase-2-outlook.md](12-phase-2-outlook.md)) adds the mobile/web supervision surface, the attention inbox, evidence bundles and merge orchestration. Phase 1 must lay every foundation Phase 2 needs.

## Landscape and design bets

Be honest about where the market already is:

- **cmux** already has a scriptable browser pane (navigate, DOM snapshot, click, type, eval), listening ports in the sidebar, `cmux ssh` workspaces whose browser routes through the remote network, scp image upload, and OSC 9/99/777 notification rings. Preview/screenshots alone are not novel.
- **Claude Code agent view** and the **Codex app** already give background sessions, attention grouping, peek-and-reply, worktrees and mobile control — each for its own vendor only.
- Terminal multiplexers for agents already offer multi-machine sessions.

So "multi-machine", "previews" and "an attention sidebar" are table stakes, not differentiators. The things we need are architectural, not features:

| Problem | Consequence | Vibeke |
|---|---|---|
| Agent state read from screen manifests matched against the bottom of the pane | Detection breaks every time an agent changes its TUI; a phone companion inherits the same fragility ("cannot read this dialog") | **Structured-first**: per-harness adapters consume hooks, extension events, RPC/app-server streams and transcripts. Screen reading is the last fallback, and every state carries its `source` and `confidence` |
| Server owns PTYs in-process | A server crash or update kills panes | **Per-pane holder processes** own each PTY. The server is disposable: processes survive crash/upgrade. Screen restoration is best-effort with a forced redraw; scrollback comes back from the archive (01 §4) |
| Approvals answered by typing keystrokes into a TUI | Remote/mobile control becomes "remote keyboard" (a phone-companion log: 334 raw keypresses vs 151 replies, mostly Backspace/Enter/Down/Space) | **Approvals and questions are first-class objects** (`Interaction`), answered through an API and delivered through the harness's native channel with a recoverable delivery state machine; verified keystrokes are an explicitly best-effort fallback |
| Events can be lost | Clients must poll and resync | **SQLite state + transactional event outbox** with cursors; reconnecting clients catch up exactly |
| No execution isolation; worktrees are a helper | Agents in the same cwd overwrite each other; yolo agents run with full user access | **Task workspaces** (worktree per agent) **and execution isolation** — OS sandbox, container or VM — so yolo runs are contained ([13](13-sandboxes-and-vms.md)) |
| Fixed set of supported agents | Custom harnesses (pi with your extensions, omp, your own) are second-class | **Harness manifests**: any CLI agent is described by a TOML file; first-class adapters for Claude Code, Codex, pi, omp, then OpenCode, Gemini CLI, ACP-generic |

## Differentiation

Each item alone can be copied; the bet is the combination, shipped open source and fast:

1. **Bring your own harness, structurally.** pi, omp, personal wrappers (`espi`), Hermes and any manifest-described CLI get the same depth of integration (state, interactions, resume, usage) as Claude Code and Codex. Screen-scraping tools guess; vendor tools only support themselves.
2. **Interactions as objects with native delivery.** Approvals, questions and plan reviews have ids, risk, and a delivery state machine; they are answered via hook response / extension / RPC, not keystrokes — from cards for agents you're not looking at, while the focused agent keeps its own UI. Vibeke surfaces each harness's own approval mechanism (for pi: whichever permission extension the user installed) rather than adding its own. This is the foundation for answering from anywhere (Phase 2 phone surface).
3. **Safe yolo.** `--yolo` runs agents without approvals *inside* an OS sandbox, container or VM; commands are free and only boundary actions (egress outside the allowlist, push, credential use) are gated. User-typed yolo on the host is supported and clearly badged. No terminal multiplexer offers this; cloud products only isolate in their own cloud.
4. **Terminal-agnostic and headless-friendly.** Runs inside any modern terminal and on headless Linux devboxes over SSH — unlike macOS-only GUI apps.
5. **Evidence (Phase 2).** Tasks come back with an `EvidenceRecord` (exact base/head revision, checks actually run, screenshots with their environment label) so review becomes review-by-exception ([12](12-phase-2-outlook.md)).

## Alternatives considered

| Option | For | Against | Decision |
|---|---|---|---|
| **Sidecar on an existing multiplexer** (a daemon using its socket API + agent hooks) | Fastest path to the supervision layer; the multiplexer keeps carrying terminal fidelity | Inherits screen-scraped state as ground truth for anything the sidecar doesn't see; can't own process durability, execution isolation, or the pane environment (PATH shims, sandbox spawn) | **Fallback.** If the M1 supervision-slice gate fails (see metrics), we pivot to a sidecar and keep adapters/interaction/policy crates |
| **Greenfield (chosen)** | Architecture fits the differentiators; one coherent API for Phase 2 | Large scope; must reach daily-usable quickly | **Chosen by the maintainer.** Mitigated by a narrow M1 and ruthless deferral (11) |

## Goals (Phase 1)

1. **Supervision slice first**: a daily-usable runtime (workspaces, tabs, panes, splits, sidebar, detach/attach, copy mode) with structured agents and native interactions for Claude Code, pi/omp and Codex (M1), before broader coverage.
2. **Structured agent understanding** via capability-tested adapters (04) and harness manifests for custom harnesses; unified state model and `Interaction` objects.
3. **Process durability**: processes survive server crash, upgrade and client disconnect; after reboot, agents resume via harness resume commands. Screen restoration is best-effort (01 §4).
4. **Isolation as a choice**: one-command task workspace (worktree + branch + ports + env + agent), with execution isolation (OS sandbox, container, VM) so yolo agents can run contained ([13](13-sandboxes-and-vms.md)).
5. **Remote + preview**: SSH machines, previews reachable locally (browser profile over SOCKS through the bridge), scriptable remote headless browser and screenshots.
6. **API + event outbox** that the Phase 2 mobile/web surface can be built on without changing the server.
7. **Full Herdr plugin and automation compatibility**: config/session importer and env aliases first (M1); unmodified Herdr plugins, the complete public CLI/socket extension contract for a pinned, tested Herdr baseline (M5; Windows M6). Version policy and conformance gates are in 07 §7.7–8.4.
8. **Quality bars**: measured keyboard fidelity, CPU/bandwidth budgets, golden tests per harness version.

**Toolchain:** Rust, latest stable (edition 2024), pinned in `mise.toml` and bumped each stable release (01 §2).

## Success metrics (gate for M1 and every later milestone)

Measured on the maintainer's real workload against a 2-week baseline of the current setup (which stays installed during dogfooding):

| Metric | Definition | M1 target |
|---|---|---|
| Operator interventions | Raw keystrokes/menu navigation sent to agent TUIs to unblock them (from prior audit log vs Vibeke input log) | −70% |
| Blocked time | Sum over runs of time spent in `needs_approval`/`needs_answer` before an answer is delivered | −50% |
| Approval delivery failures | Interactions ending in `delivery_failed` or `delivery_unknown` / all answered | < 1% |
| State accuracy | Sampled agent states that match ground truth (hook/transcript replay) | > 99% for adapter-sourced, reported separately for screen-sourced |
| Human review minutes per accepted change | Time from task `done` to merge/accept, active attention only (Phase 2 primary; tracked from M3) | baseline in M3 |

If M1 misses the first two targets materially, stop and re-evaluate (sidecar fallback above).

## Non-goals (Phase 1)

The basic M1 interaction list is specified in 08. [15](15-task-outcomes-review-and-attention.md) separately stages the richer desktop task/review/inbox workflow after Goal 01, without making it an SSH daily-driver prerequisite or requiring the mobile gateway first.

- Mobile/web UI, push notifications, attention inbox UI, evidence bundles, merge queue, planner (Phase 2+). Phase 1 ships the data and APIs for them; a minimal phone decision surface may come early via an existing phone companion on the Herdr compatibility layer (M5).
- Being a terminal emulator application (we run *inside* Ghostty/Kitty/WezTerm/iTerm2/Windows Terminal like tmux does). A native GUI client is a possible Phase 3.
- Building our own coding agent.
- Cloud-hosted sandboxes (Phase 2). Local OS sandbox, container and VM levels are Phase 1 ([13](13-sandboxes-and-vms.md)).
- Windows as a first-class host before M6 (the architecture must not preclude it — ConPTY, named pipes).
- Before 1.0 unless demanded: QUIC/predictive echo, plugin marketplace, policy learning, best-of-N comparison UI, automatic shared-cwd "split into task" migration, synchronized input. Herdr's private binary TUI/transport interoperability is outside the public plugin/automation compatibility contract (07 §8.0).

## Users

- **Primary**: a developer running 3–15 agent sessions across several repos on one or more machines (laptop + devbox), mixing vendors (Claude Code + Codex) and custom harnesses (pi/omp with personal extensions). the maintainer's current setup is the reference: a tmux-style multiplexer, samplehub/dashboard/backend/storefront workspaces, Claude + Codex side by side, sibling `*-todo` worktree directories created by hand.
- **Secondary**: harness authors who want their agent to integrate deeply (documented adapter protocol, ~50 lines to integrate).
- **Tertiary**: plugin authors.

## Design principles

0. **The focused pane belongs to the agent.** Vibeke never draws over, re-renders or intercepts keys in the agent TUI you're looking at; its structured surfaces (cards, peek, inbox) are for agents you are not looking at (08 §0).
1. **Structured first, screen last.** If the harness can tell us, ask the harness. Never present a guess as fact: every agent state has a `source` and `confidence`, and the UI shows when it is guessing.
2. **The server is disposable; processes are not.** Processes, scrollback, state and open interactions survive a server restart; on-screen restoration is best-effort (01 §1.2).
3. **Every change is observable.** State lives in SQLite; each change also appends an event in the same transaction. Clients subscribe with a cursor and never miss a change.
4. **One API, many surfaces.** The TUI is just a client. Anything the TUI can do, the CLI and API can do.
5. **Local feel for remote work.** Latency, previews, images, clipboard and ports should not reveal that the pane is on another machine.
6. **Bring your own harness.** Built-in adapters are written against the same public adapter interface third parties use.
7. **Boring, inspectable state.** SQLite + plain files under XDG dirs; `vibeke doctor` explains everything.
8. **Measured quality.** Latency, CPU, bandwidth and keyboard fidelity are tested in CI with budgets, not vibes.

## Glossary

| Term | Meaning |
|---|---|
| **Machine** | A host running a Vibeke server (local or remote via SSH). |
| **Session** | A named, isolated Vibeke server instance on a machine (`default` unless `--session`). |
| **Workspace** | A group of tabs, usually bound to a project root (repo). Optionally nested in **Groups**. |
| **Tab** | A layout of panes within a workspace. |
| **Pane** | A terminal (PTY + process) in a tab's layout. Panes can also be *floating*. |
| **Holder** | The tiny per-pane process (`vibeke hold`) that owns the PTY master and child process. |
| **Harness** | A kind of coding agent CLI (claude, codex, pi, omp, opencode, gemini, custom…), described by a manifest. |
| **Adapter** | Code (built-in or external) that turns a harness's native signals into Vibeke events. |
| **Agent run** | One live instance of a harness in a pane, with a harness session id and resume handle. |
| **Interaction** | Something an agent needs from a human: approval, question, plan review, notice. Has an id and can be answered via API. |
| **Task workspace** | A workspace created for a unit of work: worktree/branch + env + ports + agent(s). |
| **Preview** | A discovered or declared HTTP service (dev server) reachable from the client through the preview fabric. |
| **Event outbox** | The sequence-numbered events table written in the same transaction as state changes; used for client sync (short retention) and agent/decision history (longer retention). Not a full event-sourcing log. |
