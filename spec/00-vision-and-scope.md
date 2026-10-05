# 00 — Vision and scope

## One sentence

**Vibeke is a terminal workspace for supervising coding agents. It understands agents structurally instead of by reading their screens, keeps every process alive through anything short of a reboot, and treats remote machines, previews and screenshots as if they were local.**

Phase 1 (this spec) is the terminal runtime. Phase 2 (outlined in [12-phase-2-outlook.md](12-phase-2-outlook.md)) adds the mobile/web supervision surface, the attention inbox, evidence bundles and merge orchestration. Phase 1 must lay every foundation Phase 2 needs.

## Landscape and design bets

Be honest about where the market already is:

| Problem | Consequence | Vibeke |
|---|---|---|
| Agent state comes from screen manifests matched against the bottom of the pane; hooks are only used for `SessionStart` identity/resume | Detection breaks every time an agent changes its TUI (open issues #803, #4573, #4511, #4649, #1631, #2510). Phone companions inherit the same fragility | **Structured-first**: per-harness adapters consume hooks, extension events, RPC/app-server streams and transcripts. Screen reading is the last fallback, and every state carries its `source` and `confidence` |
| Server owns PTYs in-process; a server crash or update kills panes (live handoff experimental) | Updating or crashing loses running agents | **Per-pane holder processes** (`vibeke hold`) own each PTY. The server is disposable: crash, upgrade, restart — processes keep running and output is replayed |
| Approvals are answered by typing keystrokes into a TUI | Remote/mobile control becomes "remote keyboard" (a phone-companion log: 334 raw keypresses vs 151 replies, mostly Backspace/Enter/Down/Space) | **Approvals and questions are first-class objects** (`Interaction`) answered through an API; the adapter delivers the answer through the harness's native channel (hook response, extension, RPC), falling back to verified keystrokes |
| Events can be lost (`events_lost`) | Clients must poll and resync | **Durable, sequence-numbered event log**; subscribe with a cursor, never lose events |
| No isolation between panes; worktrees are a helper | Agents in the same cwd overwrite each other | **Task workspaces**: worktree/jj-workspace per agent by default, per-task port ranges, env and setup scripts, and edit-collision warnings from adapter tool events |
| Remote = SSH attach + forwarded CLI | Dev servers, browser previews and screenshots on the remote box are invisible locally | **Remote preview fabric**: automatic port discovery and forwarding, per-preview `*.localhost` origins, a remote headless browser for screenshots that both the human and the agents can use, image passthrough both ways |
| Fixed set of supported agents | Custom harnesses (pi with your extensions, omp, your own) are second-class | **Harness manifests**: any CLI agent is described by a TOML file; first-class adapters for Claude Code, Codex, pi, omp, OpenCode, Gemini CLI, plus ACP-generic |
| Plugins: argv actions only; no storage, no sandbox, no native UI | Plugin ecosystem can't build rich features | Argv actions **plus** long-running plugin processes over JSON-RPC with capabilities, storage, UI contributions (sidebar sections, status segments, panes) |

## Goals (Phase 1)

1. **Daily terminal use**: workspaces, tabs, panes, splits, zoom, sidebar with agent states, notifications, persistent named sessions, multiple clients, remote machines over SSH, socket API + CLI, worktrees, plugins, themes, keybindings, copy mode.
2. **Structured agent understanding** for Claude Code, Codex, pi, omp, OpenCode, Gemini CLI, and any harness described by a manifest; unified state machine and `Interaction` model.
3. **Process durability**: processes survive server crash, upgrade and client disconnect; after reboot, agents resume via harness resume commands.
4. **Isolation by default**: one-command task workspace (worktree + branch + ports + env + agent), with optional execution isolation (OS sandbox, container or VM) so "yolo" agents can run contained ([13](13-sandboxes-and-vms.md)).
5. **Remote preview fabric**: open, screenshot and inspect a dev server running on a remote machine as if it were local; share images in both directions.
6. **Durable event log + full API** that the Phase 2 mobile/web surface can be built on without changing the server.
7. **Herdr compatibility layer**: import Herdr config/sessions; optionally expose a Herdr-compatible socket so existing tools work on day one.
8. **Quality bars**: measured keyboard fidelity across major terminals, CPU/bandwidth budgets, golden-test suites for every harness version.

## Non-goals (Phase 1)

- Mobile/web UI, push notifications, the attention inbox UI, evidence bundles, merge queue, planner (Phase 2+). Phase 1 ships the *data and APIs* for them, not the UI.
- Being a terminal emulator application (we run *inside* Ghostty/Kitty/WezTerm/iTerm2/Windows Terminal like tmux does). A native GUI client is a possible Phase 3.
- Building our own coding agent. Vibeke hosts agents; it is not one.
- Cloud-hosted sandboxes. Phase 1 supports local + SSH machines **and local isolation levels — OS sandbox, container, VM — as an alternative to running on the host** ([13](13-sandboxes-and-vms.md)); cloud runners use the same `Runner` abstraction in Phase 2.
- Windows as a first-class host in M1–M4 (planned for M6; the architecture must not preclude it — ConPTY, named pipes).

## Users

- **Primary**: a developer running 3–15 agent sessions across several repos on one or more machines (laptop + devbox), mixing vendors (Claude Code + Codex) and custom harnesses (pi/omp with personal extensions). the maintainer's current setup is the reference: a tmux-style multiplexer, samplehub/dashboard/backend/storefront workspaces, Claude + Codex side by side, sibling `*-todo` worktree directories created by hand.
- **Secondary**: harness authors who want their agent to integrate deeply (documented adapter protocol, ~50 lines to integrate).
- **Tertiary**: plugin authors.

## Design principles

1. **Structured first, screen last.** If the harness can tell us, ask the harness. Never present a guess as fact: every agent state has a `source` and `confidence`, and the UI shows when it is guessing.
2. **The server is disposable; processes are not.** Anything the user cares about survives a server restart.
3. **Everything is an event.** State is a projection of an append-only log. Clients subscribe with a cursor.
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
| **Event log** | The durable, sequence-numbered record of everything that happened in a session. |
