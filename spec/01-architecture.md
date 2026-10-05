# 01 — Architecture

This document fixes the decisions every other spec section builds on. Sections 02–11 refine components; they must not contradict this file without updating it.

## 1. Process model

```
                         ┌───────────────────────────── machine ─────────────────────────────┐
  terminal (Ghostty…)    │                                                                     │
  ┌──────────────┐       │   ┌────────────────────────── vibeke server (session "default") ──┐│
  │ vibeke (TUI) │◄─────►│   │  API (JSON-RPC)   Render streams   Event log (SQLite)          ││
  └──────────────┘ unix  │   │  Layout/state     VT engines       Harness adapters            ││
  ┌──────────────┐ sock  │   │  Worktrees/tasks  Preview fabric   Plugin host                 ││
  │ vibeke CLI   │◄─────►│   └───────▲────────────────▲─────────────────▲───────────────────┘│
  └──────────────┘       │           │ holder proto   │ adapter channel │ plugin JSON-RPC     │
  ┌──────────────┐       │   ┌───────┴──────┐  ┌──────┴────────┐  ┌─────┴──────┐              │
  │ plugin procs │◄─────►│   │ vibeke hold  │  │ hooks/ext/RPC │  │  plugins   │              │
  └──────────────┘       │   │  (1 / pane)  │  │ from agents   │  └────────────┘              │
                         │   │  PTY master  │  └───────────────┘                               │
                         │   │  └─ child ───┼─► claude / codex / pi / omp / zsh …              │
                         │   └──────────────┘                                                 │
                         └─────────────────────────────────────────────────────────────────────┘
  remote machine: identical; the local server connects to it over SSH (or QUIC) via `vibeke bridge`.
```

### 1.1 Binaries

A **single binary** `vibeke` (symlink `vk`) with subcommands. Roles:

| Role | Invocation | Lifetime |
|---|---|---|
| TUI client | `vibeke` / `vibeke attach [--session s] [--machine m]` | while the user is attached |
| CLI client | `vibeke <noun> <verb> …` | one request |
| Server | `vibeke server [--session s]` (auto-spawned, daemonized) | until stopped; restartable at will |
| Holder | `vibeke hold --pane <id> …` (spawned by server, double-forked, setsid) | as long as the child process lives |
| Bridge | `vibeke bridge` (spawned over SSH on a remote machine) | while the remote link is up |
| Hook shim | `vibeke hook <harness> <event>` (called by agent hook configs) | one hook invocation |

### 1.2 Holder processes (the durability core)

- One holder per pane. It owns the PTY master fd and is the parent of the pane's child process. It is deliberately tiny (target < 1,500 lines, no VT parsing, no async runtime beyond `mio`/`polling`), so that it essentially never needs updating.
- It listens on `$RUNTIME/<session>/holders/<pane-ulid>.sock` (0600).
- It keeps an **output ring** (default 16 MiB, configurable) of raw PTY bytes with a monotonically increasing byte offset.
- Protocol (binary, length-prefixed, versioned; spec in [07-api-cli-plugins.md](07-api-cli-plugins.md) §Holder protocol): `Hello{version}`, `Attach{from_offset}` → `Output{offset, bytes}` stream, `Input{bytes}`, `Resize{cols, rows, px_w, px_h}`, `Signal{sig}`, `Status` → `{pid, fg_pgid, fg_cmdline, exited?, exit_code}`, `Kill`.
- Exactly one server attaches at a time (lease with fencing token, so an orphaned old server can't write).
- If the child exits, the holder keeps its ring and exit status until the server acknowledges, then exits.
- Holder protocol is **stable across Vibeke versions**: major bumps only, with the server able to talk to N-1 holders.

**Server restart / upgrade flow**: new server starts → reads `state.db` → for each live pane, connects to its holder → receives `Status` → replays ring bytes from the last checkpoint offset stored with the VT snapshot (see §4) → panes are restored exactly, processes never noticed.

**Reboot**: holders die. On next start, panes are recreated from the layout; panes whose last occupant was an agent with a resume handle are offered (or configured) to resume via the harness's resume argv (see [04-harness-adapters.md](04-harness-adapters.md)).

### 1.3 Server

- Rust, `tokio` multi-threaded runtime. One server process per (machine, session).
- Owns: layout tree, VT engines (one per pane), event log, harness adapters, task workspaces, preview fabric, plugin host, remote links, notification dispatch.
- All mutations go through a single **command bus** (`mpsc`) processed by a state actor that appends events to the log and updates in-memory projections. Reads are served from projections (`arc-swap` snapshots) to keep the hot path lock-free.
- Heavy per-pane work (VT parsing, screen detectors) runs on per-pane tasks; the state actor never parses bytes.

### 1.4 Clients

- **TUI client**: attaches to the server, receives a render stream (§3.2), composites the UI (sidebar, tab bar, status bar, pane frames, popups) and writes to the host terminal. All input goes to the server as keys/mouse/paste events; the server routes it (keybinding resolution happens **client-side** for latency and per-client keymaps, then the client sends either a command or raw input for the focused pane).
- **CLI client**: JSON-RPC request/response, `--json` default for machine use, human tables with `--pretty` when stdout is a TTY.
- **Phase 2**: a gateway process (`vibeke gateway`) serves web/mobile over HTTPS using the same API. Not built in Phase 1, but the API must already suffice.

## 2. Language, libraries, key technical choices

| Concern | Decision | Notes |
|---|---|---|
| Language | Rust, **latest stable toolchain** (1.99.0 as of 2026-09-28), edition 2024 (the newest edition; the next one is 2027) | `rust-toolchain.toml` pins the current stable and is bumped within a week of each 6-week stable release (Renovate PR + CI). MSRV = the pinned version; we don't support older compilers since we ship binaries, not a library. Use new language features freely (async closures, let-chains, etc.). Single static binary; good PTY/terminal ecosystem. |
| Async | `tokio` in server/clients; `polling` in holder | Holder must stay minimal. |
| PTY | `rustix` + custom openpty/forkpty on Unix; ConPTY on Windows (M6) | Avoid `portable-pty` in the holder to control fd inheritance and setsid precisely. |
| VT engine | Behind a `VtEngine` trait. **M0 spike decides** between `libghostty-vt` (Ghostty's VT core via C ABI), `wezterm-term`, and `alacritty_terminal` | Selection criteria in [03-terminal-engine-and-tui.md](03-terminal-engine-and-tui.md) §2: conformance (esctest/vttest), kitty keyboard, kitty graphics, OSC 8/52/133, reflow, perf, snapshot/serialize support, license. |
| TUI rendering | Custom compositor on `crossterm` output + `ratatui` for chrome widgets | Pane contents are blitted from server cell grids, not re-rendered through ratatui widgets. |
| Storage | SQLite (WAL) via `rusqlite`; zstd-compressed scrollback segment files; FTS5 for search | One DB per session. |
| Serialization | JSON (control API, events, config interop), `postcard` (render stream, holder protocol) | |
| Config | TOML (`~/.config/vibeke/config.toml`), hot-reloadable | Herdr config importer. |
| Remote transport | SSH (spawn `vibeke bridge` via `ssh -T`, multiplexed frames over stdio) in M3; QUIC (`quinn`) roaming transport in M5 | See [06-remote-and-preview.md](06-remote-and-preview.md). |
| Browser automation (preview) | Chrome DevTools Protocol client (`chromiumoxide` or thin custom CDP) driving a headless Chromium on the machine where the dev server runs | |
| Plugin runtime | Argv actions (Herdr-compatible) + long-running plugin processes over JSON-RPC; WASM (wasmtime) components considered for M6 | |
| Logging/tracing | `tracing` + rotating JSON logs; OpenTelemetry export optional | Agent events optionally exported as OTel GenAI spans (Phase 2 analytics). |
| Packaging | Static binaries for macOS arm64/x64, Linux x64/arm64 (musl); `curl | sh`, Homebrew, Nix, AUR; Windows zip in M6 | macOS: a signed helper `.app` bundle identity for the server so TCC/Local Network permissions stick. |

## 3. Wire protocols (overview — details in 07)

### 3.1 Control API

- Transport: Unix domain socket `$RUNTIME/<session>/vibeke.sock` (0600), named pipe on Windows. Remote: tunneled through the bridge.
- Framing: newline-delimited JSON, **JSON-RPC 2.0** (`{"jsonrpc":"2.0","id":1,"method":"pane.split","params":{…}}`).
- Methods are namespaced (`machine.*`, `session.*`, `workspace.*`, `group.*`, `tab.*`, `pane.*`, `agent.*`, `interaction.*`, `task.*`, `worktree.*`, `preview.*`, `browser.*`, `notification.*`, `events.*`, `config.*`, `plugin.*`, `layout.*`, `search.*`).
- Every method is described by a JSON Schema generated from Rust types (`schemars`); `vibeke api schema` prints it; TypeScript/Python client bindings are generated in CI.
- **Events**: `events.subscribe {after_seq, filter}` → server pushes `events.event` notifications; `events.ack`; gaps are impossible because the log is durable (clients that fall too far behind get `events.truncated` with the earliest available seq and must re-read projections).

### 3.2 Render stream

- A client opens a second connection and calls `render.attach {client_id, viewport, capabilities}`; the connection then switches to length-prefixed `postcard` frames.
- Server → client: per-pane **damage frames** (changed rows/cells since the client's last ack'd frame, mosh-SSP-style state sync, not a byte relay), cursor state, images (kitty graphics placements by content hash; the client fetches bytes once), title/bell/notifications.
- Client → server: input events, resize, focus, acks.
- Frame pacing: server coalesces damage per client up to the client's refresh rate (default 120 Hz local, adaptive for remote, see 06). Spinner-only damage on unfocused panes is rate-limited (default 4 Hz).
- Because the stream is state-sync, a slow or remote client simply receives fewer, larger diffs; it never blocks the pane.

### 3.3 Adapter channel

How agents talk to Vibeke (details in 04):

- Env injected into every pane: `VIBEKE=1`, `VIBEKE_SOCKET`, `VIBEKE_PANE_ID`, `VIBEKE_WORKSPACE_ID`, `VIBEKE_TAB_ID`, `VIBEKE_SESSION`, `VIBEKE_BIN`, plus Herdr-compat aliases (`HERDR_ENV=1`, `HERDR_PANE_ID`, `HERDR_SOCKET_PATH`, …) when `compat.herdr_env = true`.
- Hook shims (`vibeke hook claude PreToolUse`) and extensions (`@vibeke/pi-extension`) connect to `VIBEKE_SOCKET` and call `adapter.report` / `adapter.interaction.open` / `adapter.interaction.await`.
- Headless harness modes (`pi --mode rpc`, `omp --mode rpc|rpc-ui`, `codex app-server`, `claude -p --output-format stream-json`, ACP agents) are driven by an in-server adapter task, with an optional terminal "view" pane rendering the conversation (Phase 1: minimal transcript view; Phase 2: rich UI).

## 4. State, persistence and recovery

Detailed in [02-data-model-and-event-log.md](02-data-model-and-event-log.md). Summary:

- `state.db` per session: event log table, projection tables (machines, workspaces, groups, tabs, panes, layouts, agent runs, interactions, tasks, previews, notifications), plugin KV storage.
- **VT snapshots**: every pane's screen + recent scrollback is checkpointed (serialized VT state + holder byte offset) on idle (no output for 2 s) and at most every 30 s while busy. Recovery = load snapshot + replay ring bytes after the offset. If the ring overflowed past the offset, recovery falls back to "reset screen, replay the whole ring", which is visually close enough for TUIs that redraw.
- **Scrollback archive**: lines that scroll off the in-memory buffer are appended to zstd segment files per pane (unwrapped text + optional style runs) and indexed in FTS5. Gives unlimited, searchable history (`vibeke search "migration failed"`), surviving restarts.
- Projections are always rebuildable from the log (`vibeke doctor --rebuild`).

## 5. Directory layout (XDG; macOS uses the same XDG paths)

```
~/.config/vibeke/
  config.toml                 # user config
  harnesses/*.toml            # user harness manifests (override/extend built-ins)
  plugins/                    # installed plugins
  themes/*.toml
~/.local/state/vibeke/<session>/
  state.db                    # event log + projections
  scrollback/<pane-ulid>/*.zst
  logs/server.log, logs/holder-<pane>.log
  previews/screenshots/…      # captured screenshots (content-addressed)
$XDG_RUNTIME_DIR/vibeke/<session>/   (macOS: $TMPDIR/vibeke-$UID/<session>/)
  vibeke.sock                 # control + render
  holders/<pane-ulid>.sock
  herdr-compat.sock           # optional Herdr-compatible API
~/.vibeke/worktrees/<repo>/<task-slug>/   # default task worktree root (configurable)
```

## 6. Crate layout (Cargo workspace)

```
crates/
  vk-proto        # API types, JSON-RPC envelopes, event types, render frames, holder protocol; schemars
  vk-hold         # holder (library + bin entry), PTY spawning, ring buffer — minimal deps
  vk-term         # VtEngine trait + chosen engine binding, screen model, damage tracking, snapshots
  vk-store        # SQLite schema/migrations, event log, projections, scrollback archive, FTS
  vk-server       # state actor, command bus, API server, render server, notification dispatch
  vk-agents       # harness manifests, adapter trait, built-in adapters, screen detector engine
  vk-tasks        # task workspaces: git/jj worktrees, port allocator, env/setup scripts, collision tracker
  vk-sandbox      # execution isolation: Seatbelt/bwrap sandboxes, container & VM providers, egress proxy, credential projection (13)
  vk-remote       # machines, SSH bootstrap, bridge, transport (ssh-stdio, quic), forwarding
  vk-preview      # port discovery, HTTP/WS reverse proxy, CDP browser service, screenshot store
  vk-plugins      # manifest, argv actions, plugin process host, capabilities, KV storage
  vk-tui          # TUI client: compositor, input, keymaps, copy mode, popups, command palette
  vk-cli          # CLI command tree (clap), output formatting
  vk-compat       # Herdr config/session importer, Herdr-compatible socket shim
  vibeke          # the binary: dispatches roles
  vk-redact       # secret redaction shared by logs, events, debug bundles (09)
tools/            # dev/test-only crates (10): vk-chaos, vk-keytest, vk-bench, vk-fixture
integrations/
  claude/         # hook config templates (installed by `vibeke integration install claude`)
  codex/          # hooks.json templates, app-server adapter notes
  pi-extension/   # @vibeke/pi-extension (TypeScript) — works for pi and omp
  opencode/       # opencode plugin
  gemini/         # hook config templates
tests/
  e2e/            # spawn server+holders, drive via API, assert screens/events
  harness-golden/ # recorded sessions per harness version (asciicast + hook/event traces)
  keyboard/       # keyboard fidelity matrix fixtures
docs/             # user docs (generated site)
```

## 7. Cross-cutting decisions

1. **IDs**: internally ULIDs. Public short handles are stable and never reused within a session: workspace `w3`, tab `w3:t2`, pane `w3:p5`, interaction `i42`, task `k7`, preview `v4`, machine label (`devbox`). Fully qualified remote form: `devbox/w3:p5`. API accepts either form; agents may also be addressed by unique name.
2. **Time**: all timestamps UTC RFC 3339 with ms in the API; i64 unix-ms in storage.
3. **Versioning**: API is `vibeke/1`; additive changes only within a major. Event payloads carry `v`. Holder protocol versioned separately.
4. **Security**: sockets 0600 in 0700 dirs; peer credential check (`SO_PEERCRED`/`getpeereid`) rejects other UIDs; remote links authenticated by SSH (M3) or by pinned keys (QUIC, M5); plugins get explicit capabilities; secrets are redacted in logs and never put in events (env values are not logged).
5. **Failure isolation**: an adapter or plugin panic never takes down the server (each runs in its own task with `catch_unwind` boundary or out of process); a wedged client is disconnected after 30 s without write progress.
6. **Observability**: `vibeke doctor` (install, sockets, integrations, terminal capabilities, harness versions, permissions), `vibeke debug bundle` (redacted logs + state summary for bug reports).
7. **Herdr compatibility is opt-in and bounded**: importer + env aliases + a compat socket implementing the subset of the Herdr socket API that common plugins use (documented in 07). We do not chase Herdr's API forever.
