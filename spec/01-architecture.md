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

- One holder per pane. It owns the PTY master fd and is the parent of the pane's child process. It is deliberately small (target < 2,500 lines, no full VT parsing, no async runtime beyond `mio`/`polling`) so it rarely needs updating.
- It listens on `$RUNTIME/<session>/holders/<pane-ulid>.sock` (0600, peer-UID checked; access additionally requires the per-holder secret the server stored when spawning it, see 09).
- It keeps an **output journal**: a ring (default 16 MiB, configurable) of raw PTY bytes with a monotonically increasing byte offset, interleaved with **markers** (`Resize{cols,rows}`, `InputAck{input_id}`, `CutPoint`) so replay preserves ordering between output, resizes and input.
- It runs a **tiny escape-boundary state machine** (UTF-8 continuation + ESC/CSI/OSC/DCS/APC framing only, ~200 lines, no screen model). It records *safe cut points*: offsets that are not inside a UTF-8 sequence or an escape sequence. Checkpoints and replay always start at a safe cut point.
- **Byte-triggered checkpoints**: when the bytes since the last acknowledged checkpoint reach 50% of the ring, the holder sends `CheckpointWanted{offset}`; the server snapshots the VT state at the nearest cut point (03 §4). The timer-based checkpoint (idle 2 s, at most every 30 s) remains as a second trigger.
- **Server-absent query answering**: while no server is attached, the holder answers only queries whose answer doesn't depend on screen state: DA1 (`CSI c`), DA2 (`CSI > c`), DA3, XTVERSION, and DECRQM for a fixed set of modes it tracks itself (bracketed paste, focus events, kitty keyboard flags as last set). Queries that need screen state (DSR 6 cursor position, OSC 10/11 colour queries, XTWINOPS size reports) are **queued** and answered by the server on reattach, in order, if the app is still waiting (≤ 5 s old); otherwise dropped. This is documented as a known limitation.
- **Input**: every `Input` carries a client-generated `input_id` (u64, unique per server epoch). The holder writes the bytes, records `InputAck{input_id}` in the journal and replies with an ack. The holder keeps the last 4,096 input ids and silently drops duplicates. Semantics are **at-most-once**: a retry after a lost ack is deduped; input not acked before a crash may be lost and is reported to the client as `input_unconfirmed` (never replayed automatically).
- **Pipe mode**: headless harness processes (RPC/app-server/ACP over stdio, 04) also run under a holder, which owns their stdin/stdout pipes instead of a PTY, journals protocol frames (JSONL) and keeps the process alive across server restarts. On reattach the adapter replays journaled frames after its last processed offset and reconciles pending protocol requests (e.g. open approvals) with the harness (04 §8).
- Exactly one server attaches at a time (lease with fencing token, so an orphaned old server can't write).
- If the child exits, the holder keeps its journal and exit status until the server acknowledges, then exits.
- Holder protocol is **stable across Vibeke versions**: major bumps only, with the server able to talk to N-1 holders.

**What is guaranteed across a server crash, restart or upgrade** (the honest contract; tested in 10 §5):

| Guarantee | Level |
|---|---|
| Pane processes keep running | **Guaranteed** (holders are independent processes) |
| No input applied twice | **Guaranteed** (input ids + holder dedupe) |
| Input sent but not acked before the crash | May be lost; client is told (`input_unconfirmed`) |
| Scrollback that had left the screen | **Guaranteed** up to the last archive flush (≤ 1 s), restored from the archive (§4) |
| On-screen content | **Best-effort**: snapshot at a safe cut point + replay of journal bytes after it. Replay runs with a `replaying` flag that suppresses side effects: notifications, bells, OSC 9/777, OSC 52 clipboard writes, terminal query replies, and archive writes for already-archived lines |
| TUI apps (Claude, Codex, pi, omp, editors) | After replay the server sends a **resize nudge** (cols−1, then cols → two SIGWINCHs) so apps that redraw on SIGWINCH repaint fully; screen then matches exactly |
| Raw shells / non-redrawing apps | Visually close; may show artifacts if the journal overflowed before a checkpoint (recovery method recorded as `ring_only` and shown once in the pane) |
| Headless (pipe-mode) runs | Process survives; protocol frames after the last processed offset are replayed; pending requests reconciled |

**Server restart / upgrade flow**: new server starts → opens `state.db` → for each live pane, connects to its holder → `Status` → loads the latest VT snapshot → replays journal bytes after the snapshot's cut point with `replaying=true` → answers queued queries → resize nudge → normal operation. `vibeke update` is a normal server restart, not a special handoff.

**Reboot**: holders die. On next start, panes are recreated from the layout; panes whose last occupant was an agent with a resume handle are offered (or configured) to resume via the harness's resume argv (see [04-harness-adapters.md](04-harness-adapters.md)).

### 1.3 Server

- Rust, `tokio` multi-threaded runtime. One server process per (machine, session).
- Owns: layout tree, VT engines (one per pane), event log, harness adapters, task workspaces, preview fabric, plugin host, remote links, notification dispatch.
- All mutations go through a single **command bus** (`mpsc`) processed by a state actor. Each command commits one SQLite transaction that updates the state tables **and** appends its events to the event table (transactional outbox, 02 §2). Reads are served from in-memory projections of those tables (`arc-swap` snapshots) to keep the hot path lock-free.
- Heavy per-pane work (VT parsing, screen detectors) runs on per-pane tasks; the state actor never parses bytes.

### 1.4 Clients

- **TUI client**: attaches to the server, receives a render stream (§3.2), composites the UI (sidebar, tab bar, status bar, pane frames, popups) and writes to the host terminal. Keybinding resolution happens **client-side** (latency, per-client keymaps); everything not bound is sent to the server as **logical** key/mouse/paste events plus the host terminal's capability profile. **The server owns the one canonical keyboard encoder** (03 §7): it encodes each logical event for the target pane's current input modes. Raw byte input is an explicit exception (`pane.send_bytes`, paste of raw bytes), never the default path.
- **Multi-client geometry**: a PTY has one size. A **geometry controller lease** is held by the most recently active interactive client (last keypress/mouse/resize); the pane is sized to that client's viewport. Other clients render the same grid letterboxed (smaller grid than their viewport) or cropped to the cursor region (larger grid), never resizing the PTY. Lease changes are debounced (500 ms) to avoid SIGWINCH storms when two clients alternate.
- **CLI client**: JSON-RPC request/response, `--json` default for machine use, human tables with `--pretty` when stdout is a TTY.
- **Phase 2**: a gateway process (`vibeke gateway`) serves web/mobile over HTTPS using the same API. Not built in Phase 1, but the API must already suffice.

## 2. Language, libraries, key technical choices

| Concern | Decision | Notes |
|---|---|---|
| Language | Rust, **latest stable toolchain** (1.99.0 as of 2026-09-28), edition 2024 (the newest edition; the next one is 2027) | **`mise.toml` is the single toolchain source** (Rust with rustfmt/clippy, cargo-nextest, cargo-deny, bun for the pi extension); `mise install` sets up a dev machine and CI uses `jdx/mise-action`. It pins the current stable Rust and is bumped within a week of each 6-week stable release (Renovate PR + CI). No separate `rust-toolchain.toml`. MSRV = the pinned version; we don't support older compilers since we ship binaries, not a library. Use new language features freely (async closures, let-chains, etc.). Single static binary; good PTY/terminal ecosystem. The one non-Rust build input is the vendored libghostty-vt (Zig 0.16, also pinned in `mise.toml`), statically linked. |
| Async | `tokio` in server/clients; `polling` in holder | Holder must stay minimal. |
| PTY | `rustix` + custom openpty/forkpty on Unix; ConPTY on Windows (M6) | Avoid `portable-pty` in the holder to control fd inheritance and setsid precisely. |
| VT engine | **libghostty-vt** (Ghostty's VT core), vendored at a pinned commit and built with Zig 0.16 into the static binary; decided 2026-10-06, replacing the M0 `alacritty_terminal` binding. The `VtEngine` trait stays as an internal seam, but only one implementation ships and is maintained | Hard requirement: serialize/restore of full terminal state **including parser state** (mid-sequence continuation) — the recovery contract (§1.2) depends on it. libghostty-vt provides this natively (snapshot API with VT/UTF-8 continuation), plus kitty graphics and OSC 133. Rationale, embedding and gaps in [03](03-terminal-engine-and-tui.md) §2. |
| TUI rendering | Custom compositor on `crossterm` output + `ratatui` for chrome widgets | Pane contents are blitted from server cell grids, not re-rendered through ratatui widgets. |
| Storage | SQLite (WAL) via `rusqlite`; zstd-compressed scrollback segment files; FTS5 for search | One DB per session. |
| Serialization | JSON (control API, events, config interop), `postcard` (render stream, holder protocol) | |
| Config | TOML (`~/.config/vibeke/config.toml`), hot-reloadable | Herdr config importer. |
| Remote transport | SSH (spawn `vibeke bridge` via `ssh -T`, multiplexed frames over stdio) in M3; QUIC (`quinn`) roaming transport post-1.0 | See [06-remote-and-preview.md](06-remote-and-preview.md). |
| Browser automation (preview) | Chrome DevTools Protocol client (`chromiumoxide` or thin custom CDP) driving a headless Chromium on the machine where the dev server runs | |
| Plugin runtime | Argv actions + long-running plugin processes over JSON-RPC; WASM (wasmtime) components considered post-1.0 | |
| Logging/tracing | `tracing` + rotating JSON logs; OpenTelemetry export optional | Agent events optionally exported as OTel GenAI spans (Phase 2 analytics). |
| Packaging | Static binaries for macOS arm64/x64, Linux x64/arm64 (musl); `curl | sh`, Homebrew, Nix, AUR; Windows zip in M6 | macOS: a signed helper `.app` bundle identity for the server so TCC/Local Network permissions stick. |

## 3. Wire protocols (overview — details in 07)

### 3.1 Control API

- Transport: Unix domain socket `$RUNTIME/<session>/vibeke.sock` (0600), named pipe on Windows. Remote: tunneled through the bridge.
- Framing: newline-delimited JSON, **JSON-RPC 2.0** (`{"jsonrpc":"2.0","id":1,"method":"pane.split","params":{…}}`).
- Methods are namespaced (`machine.*`, `session.*`, `workspace.*`, `group.*`, `tab.*`, `pane.*`, `agent.*`, `interaction.*`, `task.*`, `worktree.*`, `preview.*`, `browser.*`, `notification.*`, `events.*`, `config.*`, `plugin.*`, `layout.*`, `search.*`).
- Every method is described by a JSON Schema generated from Rust types (`schemars`); `vibeke api schema` prints it; TypeScript/Python client bindings are generated in CI.
- **Events**: `events.subscribe {cursor, filter}` → server pushes `events.event` notifications; `events.ack`. A cursor is `{machine_uuid, session_uuid, log_epoch, seq}` (02 §2.3). Gaps cannot happen within the retained window; a client whose cursor is older than retention, or from a different `log_epoch`, gets `events.truncated` and must take a fresh snapshot.

### 3.2 Render stream

The normative wire schema is in [07-api-cli-plugins.md](07-api-cli-plugins.md) §3; this is a summary.

- A client opens a second connection and calls `render.attach {client_id, viewport, capabilities}`; the connection then switches to length-prefixed `postcard` frames.
- Server → client: per-pane **damage frames** (mosh-SSP-style state sync, not a byte relay). Each frame carries `{pane, reset_epoch, base_rev, rev}`: a frame applies only to a client whose current pane state is exactly `base_rev` in the same `reset_epoch`; otherwise the client discards it and the server sends a full keyframe. With several frames in flight, each is computed against the previous frame's `rev`, not the last ack. Frames carry cursor state, images (kitty graphics placements by content hash; the client fetches bytes once), title/bell/notifications.
- Client → server: logical input events (with `input_id`), viewport size, focus, acks.
- Frame pacing: server coalesces damage per client up to the client's refresh rate (default 120 Hz local, adaptive for remote, see 06). Spinner-only damage on unfocused panes is rate-limited (default 4 Hz).
- Because the stream is state-sync, a slow or remote client simply receives fewer, larger diffs; it never blocks the pane.

### 3.3 Adapter channel

How agents talk to Vibeke (details in 04):

- Env injected into every pane: `VIBEKE=1`, `VIBEKE_SOCKET`, `VIBEKE_PANE_ID`, `VIBEKE_WORKSPACE_ID`, `VIBEKE_TAB_ID`, `VIBEKE_SESSION`, `VIBEKE_BIN`, plus Herdr-compat aliases (`HERDR_ENV=1`, `HERDR_PANE_ID`, `HERDR_SOCKET_PATH`, …) when `compat.herdr_env = true`.
- Hook shims (`vibeke hook claude PreToolUse`) and extensions (`@vibeke/pi-extension`) connect to `VIBEKE_SOCKET` and call `adapter.report` / `adapter.interaction.open` / `adapter.interaction.await`.
- Headless harness modes (`pi --mode rpc`, `omp --mode rpc|rpc-ui`, `codex app-server`, `claude -p --output-format stream-json`, ACP agents) run under a holder in **pipe mode** (§1.2) so they survive server restarts, and are driven by an in-server adapter task, with an optional terminal "view" pane rendering the conversation (Phase 1: minimal transcript view; Phase 2: rich UI).

## 4. State, persistence and recovery

Detailed in [02-data-model-and-event-log.md](02-data-model-and-event-log.md). Summary:

- `state.db` per session. **The state tables are the source of truth** (machines, workspaces, groups, tabs, panes, layouts, agent runs, interactions, tasks, previews, notifications, port leases, plugin KV). Every mutation also appends its events to the `events` table **in the same transaction** (transactional outbox). Events serve sync (client catch-up), timelines and audit; they are not used to rebuild state.
- **VT snapshots**: serialized VT state (incl. parser state) + the holder cut-point offset, taken on idle (2 s), at most every 30 s while busy, and whenever the holder signals its journal is 50% consumed (§1.2). Recovery semantics and guarantees: §1.2.
- **Scrollback archive**: lines that scroll off the in-memory buffer are appended to zstd segment files per pane (unwrapped text + optional style runs), flushed at least every second, and indexed in FTS5. Gives unlimited, searchable history (`vibeke search "migration failed"`), surviving restarts.
- **Degraded mode** (disk full, DB I/O error): see 02 §5. In short, the server stops accepting mutations that need persistence, keeps panes and holders running, and says so loudly; it never pretends to persist.

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
  vk-term         # VtEngine trait + libghostty-vt binding (vendor/libghostty-vt, built via zig), screen model, damage tracking, snapshots
  vk-store        # SQLite schema/migrations, event log, projections, scrollback archive, FTS
  vk-server       # state actor, command bus, API server, render server, notification dispatch
  vk-agents       # harness manifests, adapter trait, built-in adapters, screen detector engine
  vk-tasks        # task workspaces: git/jj worktrees, port allocator, env/setup scripts, collision tracker
  vk-sandbox      # execution isolation: Seatbelt/bwrap sandboxes, container & VM providers, egress proxy, credential projection (13)
  vk-remote       # machines, SSH bootstrap, bridge, transport (ssh-stdio, quic), forwarding
  vk-preview      # port discovery, HTTP/WS reverse proxy, CDP browser service, screenshot store
  vk-plugins      # native + Herdr manifests, shared registry, actions/hooks/panes/logs, process host, trust, KV
  vk-tui          # TUI client: compositor, input, keymaps, copy mode, popups, command palette
  vk-cli          # CLI command tree (clap), output formatting
  vk-compat       # Herdr importer, versioned full public CLI/socket facade, per-plugin callback brokers
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
4. **Security**: sockets 0600 in 0700 dirs; peer credential check (`SO_PEERCRED`/`getpeereid`) rejects other UIDs; remote links authenticated by SSH (M3) or by pinned keys (QUIC, post-1.0); plugins get explicit capabilities; known secret patterns are redacted from logs, events and debug bundles (`vk-redact`), and env values are never logged — but operational data (scrollback, snapshots, tool outputs) is stored as-is, locally, with 0600 permissions; see 09 §9 for exactly what is and isn't promised.
5. **Failure isolation**: an adapter or plugin panic never takes down the server (each runs in its own task with `catch_unwind` boundary or out of process); a wedged client is disconnected after 30 s without write progress.
6. **Observability**: `vibeke doctor` (install, sockets, integrations, terminal capabilities, harness versions, permissions), `vibeke debug bundle` (redacted logs + state summary for bug reports).
7. **Herdr compatibility is opt-in and versioned**: full public plugin/CLI/socket compatibility for an explicitly pinned and tested baseline (07 §7.7–8.4), including unchanged plugins. `vk-plugins` owns plugin lifecycle and the per-user registry; `vk-compat` owns wire/CLI projections and brokers bound to approved identities. Native contracts remain independent. Upstream changes require a reviewed baseline update and conformance evidence before advertising support; private Herdr TUI/transport interoperability is outside this contract.
