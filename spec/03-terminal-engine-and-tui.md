# 03 — Terminal engine and TUI client

Scope: how pane bytes become screen state on the server (§1–§4), how that state reaches a TUI client and the host terminal (§5–§6), how input flows the other way (§7), the terminal features we must carry end to end (§8–§10), copy mode and scrollback (§11), and render pacing and CPU budgets (§12). It builds on [01-architecture.md](01-architecture.md) §3.2 (render stream) and §4 (VT snapshots), and on [02-data-model-and-event-log.md](02-data-model-and-event-log.md) (`vt_snapshots`, `scrollback_fts`). Config keys are listed in full in [08-ux-config-and-keybindings.md](08-ux-config-and-keybindings.md).

Design rule: **the server owns screen state and input encoding, the client owns presentation.** The server parses every pane's bytes once and keeps the authoritative grid. Clients get cell diffs (state sync), never raw pane bytes. That one decision gives us multi-client attach, cheap remote rendering, recovery after a restart, detection on structured screen data, and a per-client re-encoding of features the host terminal may not support.

---

## 1. The `VtEngine` trait

`vk-term` wraps one concrete engine behind a trait. Everything else in the server (detectors, snapshots, search, render server) talks only to the trait and to `vk-term`'s own screen model (§3).

```rust
pub trait VtEngine: Send + 'static {
    /// Construct with initial size and options (scrollback rows kept in memory, reflow on/off, colour palette).
    fn new(size: GridSize, opts: &EngineOptions) -> Self where Self: Sized;

    /// Feed PTY output bytes. Must accept partial UTF-8 / partial escape sequences across calls.
    /// Returns side effects the engine could not apply to the grid by itself.
    fn feed(&mut self, bytes: &[u8], out: &mut Vec<EngineEffect>);

    fn resize(&mut self, size: GridSize, px: Option<PixelSize>);   // reflows soft-wrapped lines when supported

    /// Rows whose content/attributes changed since the last call; clears the dirty set.
    fn take_damage(&mut self) -> Damage;

    /// Read access for rendering, detection and search.
    fn visible_row(&self, y: u16) -> RowRef<'_>;      // cells: grapheme, width, style, hyperlink id, image refs
    fn scrollback_len(&self) -> usize;
    fn scrollback_row(&self, idx: usize) -> RowRef<'_>; // 0 = oldest kept in memory
    fn cursor(&self) -> CursorState;                  // pos, shape, blink, visible
    fn modes(&self) -> TermModes;                     // alt screen, app cursor/keypad, bracketed paste, mouse mode+encoding,
                                                      // focus reporting, kitty kbd flag stack, modifyOtherKeys level, sync-update (2026)
    fn title(&self) -> Option<&str>;
    fn cwd(&self) -> Option<&str>;                    // OSC 7
    fn palette(&self) -> &Palette;                    // incl. OSC 4/10/11/12 overrides

    /// Pop rows that left the in-memory scrollback (for the archive, §11.2).
    fn drain_evicted(&mut self, out: &mut Vec<OwnedRow>);

    /// Lossless state serialization for snapshots (01 §4). Must round-trip: modes, palette, charset state,
    /// tab stops, scroll region, saved cursor, kitty kbd stack, hyperlinks, image placements (by hash),
    /// AND parser state (a partially received escape sequence or UTF-8 sequence). Hard requirement (§2).
    fn serialize(&self, w: &mut dyn std::io::Write) -> Result<SnapshotMeta>;
    fn deserialize(r: &mut dyn std::io::Read, opts: &EngineOptions) -> Result<Self> where Self: Sized;

    /// Answer queries the application sends (DA1/DA2, DSR, XTVERSION, kitty kbd query, OSC 10/11 colour queries,
    /// DECRQM). Responses go back to the PTY, not to the client.
    fn take_replies(&mut self, out: &mut Vec<u8>);

    /// While true (journal replay after a restart, 01 §1.2), the engine still updates the grid but
    /// `take_replies` returns nothing and side-effect EngineEffects are dropped by the pane task.
    fn set_replaying(&mut self, replaying: bool);
}

pub enum EngineEffect {
    Bell,
    Notify { kind: NotifyKind /* Osc9 | Osc777 | Osc99 */, title: Option<String>, body: String },
    Clipboard { op: ClipOp /* Set | Query */, selection: ClipSel, data: Option<Vec<u8>> },  // OSC 52
    Hyperlink { id: u32, uri: String },               // OSC 8 registry entry
    ShellMark { kind: Osc133 /* PromptStart | CommandStart | CommandEnd{exit} | OutputStart */, row: AbsRow },
    Image(ImageEvent),                                // kitty graphics / sixel / iTerm2, normalized (§9)
    ColorQueryNeedsClient { slot: ColorSlot },        // only if the snapshot palette can't answer
    Progress { state: ProgressState, pct: Option<u8> },  // OSC 9;4 (ConEmu/Windows Terminal progress)
    TitleChanged, CwdChanged,
}

pub struct Damage { pub rows: SmallBitSet /* visible rows */, pub scrolled: i32 /* rows scrolled up since last take */, pub full: bool }
```

Rules:
- `feed` must never block or allocate unboundedly per call. Large outputs are chunked to 64 KiB by the pane task.
- `serialize` output is versioned by `(engine, engine_version)`. A snapshot from a different engine or version is discarded and recovery falls back to ring-only replay (01 §1.2).
- The trait is an internal seam, not a promise of pluggability. **Exactly one engine ships**, pinned after the M0 spike. Engine gaps are fixed by patching that engine (upstream PR or a fork under our org), not by re-implementing terminal state around it. The only thing `vk-term` owns outside the engine is the shared width table used by both server and client (§10.3).

## 2. M0 spike: choosing the engine

Three candidates, each wrapped in a throwaway `VtEngine` implementation in `spikes/vt-<name>`:

| Candidate | What it is (Oct 2026) | Expected strengths | Expected risks |
|---|---|---|---|
| **libghostty-vt** | Ghostty's VT core extracted as a zero-dependency Zig library with a C ABI (`include/ghostty/vt.h`). Parser API plus terminal-state API (Discussion #11348). Runs on macOS/Linux/Windows/WASM. | Battle-tested conformance from Ghostty, SIMD parsing (>100 MB/s), kitty keyboard + graphics, modern OSCs, reflow. | **API signatures still in flux**. Zig toolchain in our build. FFI surface for row/cell iteration and serialize may be incomplete (we may need to upstream). |
| **wezterm-term** | WezTerm's terminal model crate (git dependency, not a stable crates.io release) plus `termwiz`. | Pure Rust, mature, kitty graphics + sixel + iTerm2 images, kitty keyboard, hyperlinks, reflow. | Not published as a stable crate. Maintenance pace tied to WezTerm. Some internal APIs not meant for embedding. |
| **alacritty_terminal** | Alacritty's terminal crate on crates.io. | Pure Rust, stable crate, fast, kitty keyboard protocol, good damage tracking. | **No inline graphics** (kitty/sixel/iTerm2 would be ours to build). Fewer OSCs (no OSC 133, limited OSC 8 metadata). |

### 2.1 Criteria and weights

| # | Criterion | Weight | Measurement |
|---|---|---|---|
| C1 | Conformance | 20 | `esctest2` pass rate (subset relevant to xterm-compat) + `vttest` menus 1–8 screen-diffed against xterm reference dumps |
| C2 | Modern input/mode support | 15 | Kitty keyboard protocol flags 1–31 (push/pop/query), modifyOtherKeys 1/2, DECSET 2004/1004/1006/1016/2026, synchronized updates |
| C3 | Graphics | 15 | Kitty graphics (transmit/place/delete, unicode placeholders), sixel decode, iTerm2 OSC 1337 File |
| C4 | Serialize/restore | 15 | Round-trip test: feed corpus → serialize → deserialize → feed more → screen equals unbroken run. Missing support counts against the engine, weighted by how much we'd have to write. |
| C5 | Throughput & memory | 10 | `cat` of 200 MB mixed corpus; MB/s; RSS at 10k-row scrollback × 50 panes; damage computation cost |
| C6 | Unicode correctness | 10 | Grapheme clusters, ZWJ emoji, VS-15/VS-16, CJK wide, combining marks — against our width table tests (§10.3) |
| C7 | Reflow on resize | 5 | Shrink/grow corpus with soft-wrapped lines, prompt marks preserved |
| C8 | Embedding ergonomics & maintenance | 10 | Release cadence, API stability promise, licence (must be MIT/Apache/compatible), build complexity (Zig in CI?), upstream responsiveness to one PR we send during the spike |

Score each 0–5, multiply by weight, max 500.

### 2.2 Procedure (two weeks, one engineer)

1. **Corpus** (shared, committed under `tests/vt-corpus/`): asciicasts recorded from Claude Code, Codex, pi, omp, OpenCode, Gemini CLI, vim, htop, lazygit, `git log --graph`, a Next.js dev server, plus `esctest2` and `vttest` captures and a kitty-graphics fixture set.
2. Implement `feed`, `visible_row`, `take_damage`, `modes`, `resize` for all three. Implement `serialize` as far as each engine allows.
3. Run the C1–C8 harness (`cargo xtask vt-bench`), producing a Markdown table and an HTML side-by-side screen diff for failures.
4. Sanity check by hand: run the real agents in a prototype pane for a day per engine.

### 2.3 Decision rubric

- **Hard gate (C4):** serialize/restore must round-trip the full state *including parser state* across a cut in the middle of escape and UTF-8 sequences (test: cut the corpus at every 4,093rd byte, serialize, restore, continue; screen must equal the uninterrupted run). An engine that fails the gate is out, unless we can close the gap with a patch to that engine inside the spike and the patch is upstreamable or small enough to carry.
- Among engines passing the gate, pick the highest score (§2.1), with maintenance cost (C8) as tie-breaker.
- libghostty-vt's current C header still marks VT/UTF-8 continuation handling as unfinished, so it can only win if that is resolved upstream or by our patch during the spike.
- **One engine.** The losing adapters are deleted after the decision; we don't keep alternates compiling. Switching engines later is a planned project with its own spike, not a feature flag.
- No effort estimates for engine gaps are made before the spike measures them.

**Acceptance (M0):** a written decision record (`.adr/0001-vt-engine.md`) with the score table, the C4 gate results, corpus diffs, and the list of gaps we own (with sizes measured during the spike).

## 3. Server-side screen model

Each pane has a `PaneScreen` owned by its pane task:

```rust
struct PaneScreen {
    engine: Box<dyn VtEngine>,
    rows: Vec<RowCache>,          // per visible row: content hash (blake3 of cells+styles), last-changed frame no.
    frame_no: u64,                // bumped on each damage application
    scroll_epoch: u64,            // increments by scrolled rows; lets clients shift instead of resend
    images: ImageTable,           // placements on this screen, keyed by content hash (§9)
    links: HyperlinkTable,
    marks: Vec<ShellMark>,        // OSC 133 prompt/command marks, absolute row numbers
    last_output_at: Instant,
    activity: ActivityTracker,    // spinner/animation detection for pacing (§12.3)
    detect_view: DetectView,      // plain-text bottom-N rows snapshot for screen detectors (04)
}
```

Processing loop per pane task:

```
loop {
  bytes = holder.recv()                      // up to 64 KiB
  engine.feed(bytes, &mut effects)
  write engine.take_replies() to holder      // query responses stay pane-local, never routed via clients
  handle effects (notifications, clipboard policy, links, images, marks)
  damage = engine.take_damage()
  update rows[] hashes for damaged rows; skip rows whose hash didn't change (engines over-report)
  activity.observe(damage)                   // classify: idle | streaming text | spinner-only | full redraw
  publish PaneDelta{frame_no, scroll, changed_rows} to the render server (watch channel: latest wins)
  if detectors subscribed and the bottom area changed: refresh detect_view (debounced 50 ms)
  drain_evicted → scrollback archiver (§11.2)
}
```

The render server keeps, **per client and per pane**, the last frame acknowledged and the set of row hashes that client holds. A frame for a client is computed as: rows whose hash differs from what that client has, plus a scroll instruction when `scroll_epoch` advanced (the client shifts its copy, then receives only the newly exposed rows). Recomputing a frame is O(rows) hash comparisons. That is cheap enough to do at frame rate for every client.

**Acceptance:** with 30 panes, 3 of them streaming output at 1 MB/s, server CPU for parsing + damage < 15% of one core on an M2. Idle CPU, see §12.

## 4. Snapshots and recovery

Implements the recovery contract in 01 §1.2. `vt_snapshots(pane_id, holder_offset, engine, engine_version, blob_hash, taken_at)`.

- **Triggers**: 2 s after output stops; at most every 30 s while busy; **when the holder sends `CheckpointWanted`** because 50% of its journal has been written since the last acknowledged checkpoint; on graceful shutdown.
- **Cut points**: snapshots are taken only at an offset the holder has marked as a safe cut point (not inside UTF-8 or an escape sequence). The pane task feeds up to the cut point, serializes, then continues. Because serialize also captures parser state (§2.3 gate), a cut point is belt-and-braces, not a correctness requirement.
- Snapshot = `engine.serialize()` + `PaneScreen` metadata (marks, link table, image hashes), zstd-compressed and stored as a blob. `holder_offset` = the cut-point offset.
- Snapshot work runs on a blocking-pool thread from a cloned engine state if the engine supports cheap clone; otherwise feeding is paused (bounded, < 5 ms for a 200×60 screen with 10k scrollback, else the snapshot is retried at the next cut point).
- **Recovery** (server start, per live pane):
  1. Deserialize the latest snapshot; `set_replaying(true)`.
  2. `Attach{from_offset: holder_offset}`; feed journal bytes and apply journaled `Resize` markers in order. `InputAck` markers update the input-id dedupe window.
  3. If the journal starts after `holder_offset` (overflow), reset the screen and replay the whole journal from its first cut point: `pane.recovered{method: ring_only}`.
  4. `set_replaying(false)`. Answer the holder's queued screen-dependent queries (≤ 5 s old) from current state.
  5. If the foreground process is a TUI (alternate screen active, or a harness manifest marks it), send the **resize nudge** (cols−1 then cols, 50 ms apart) so the app repaints.
  6. Restore scrollback above the screen from the archive (§11.2), not from the journal.

**Acceptance** (matches 10 §5): kill -9 the server while 10 agents and 10 shells are mid-output → zero processes lost, zero inputs applied twice, no replayed side effects (no duplicate notifications, clipboard writes or query replies); agent TUI panes equal a reference run cell-for-cell after the resize nudge in 100% of runs; raw-shell panes are tracked by a separate visual-fidelity metric (10 §5.2), not a gate.

## 5. Render stream (server → TUI client)

**The normative wire schema is [07-api-cli-plugins.md](07-api-cli-plugins.md) §3** (`vk-proto::render`). This section only describes the behavior the TUI relies on:

- **State sync, not byte relay.** Frames carry changed rows (run-length encoded cells, interned styles), scroll shifts, cursor, client-relevant modes, image placement deltas, and chrome deltas (sidebar rows, tab bar, status segments, interaction badges). Image bytes are sent once per client per content hash.
- **Revisions.** Every pane frame carries `{reset_epoch, base_rev, rev}`. A client applies a frame only if its copy of that pane is at exactly `base_rev` in the same `reset_epoch`; otherwise it drops the frame and asks for a keyframe. With several frames in flight, each is computed against the previous frame's `rev` (a chain), never against the last ack, so scroll and image operations apply in order. A `reset_epoch` bump (recovery, engine reset, resize of the PTY) always comes with a keyframe.
- **Flow control.** At most `W` unacknowledged frames per pane per client (default 2 local, 1 remote). When the window is full the server stops computing frames for that client and, when it catches up, sends one frame from the client's last acked rev to current state. A slow client gets fewer frames, never stale ones.
- **Input** goes client → server as logical `InputEvent`s with an `input_id` (§7); the server is the only encoder.
- `HostCaps` (§6.1) tell the server which image encodings and clipboard paths a client supports. They never change pane semantics (see §10.3, §10.4 for width and theme).

**Multi-client geometry.** A PTY has one size. The **geometry controller lease** (01 §1.4) belongs to the most recently active interactive client; the PTY is sized to that client's pane rect. Other clients receive the same grid and present it: letterboxed with a dim border if their rect is larger, or cropped to a window that follows the cursor (with a `⋯ cropped` badge and scroll-to-pan) if smaller. Zoom and sidebar visibility are per-client presentation choices and never resize the PTY unless that client holds the lease. Lease handover is debounced 500 ms.

## 6. TUI client compositor

### 6.1 Host terminal capability detection

At attach, before entering the alternate screen:
- Query DA1, DA2, XTVERSION, kitty keyboard (`CSI ? u`), kitty graphics (`APC G a=q`), DECRQM for 2026 (sync update), 2004, 1004, 1006, 1016, OSC 11 background colour (light/dark), and `CSI 14 t`/`16 t` for pixel size. Timeout 150 ms total, with a DA1 sentinel to detect "no more answers".
- Merge with `TERM`, `TERM_PROGRAM` and `terminal.host_overrides` (escape hatch for terminals that lie).
- Result: `HostCaps { kitty_kbd, kitty_graphics, sixel, iterm2_images, sync_update, truecolor, undercurl, osc52: Allowed|Unknown, osc8, focus_events, pixel_size, background: Light|Dark|Unknown, notifications: Osc9|Osc777|Osc99|None }`.
- `vibeke doctor terminal` prints this table with a pass/warn per feature.

### 6.2 Composition

The client keeps a `ClientScreen`: its copy of each visible pane's rows plus chrome models. Each frame:
1. Apply `PaneFrame` deltas (shift + replace rows) into per-pane buffers.
2. Compose the full host-sized grid: chrome (sidebar, tab bar, status bar, borders, popups) via `ratatui` widgets rendering into the same cell grid, and pane content blitted from pane buffers into their rects. Pane cells are copied, not re-rendered through widgets.
3. Diff against the previous composed grid and emit the minimal escape sequence stream (cursor moves, SGR changes, text), wrapped in synchronized-update (`CSI ? 2026 h/l`) when the host supports it.
4. Images: place via kitty graphics (with unicode placeholders so they clip correctly at pane edges), else sixel, else iTerm2 inline, else a `[image 640×480 · click to open]` placeholder cell span (§9).
5. Cursor: shown at the focused pane's cursor with the app's requested shape; hidden while popups have focus.

The compositor runs on its own thread with a frame budget. It never waits on the network; it draws whatever state it has.

**Acceptance:** on a 300×80 host with 6 visible panes, composing a frame with one changed row costs < 0.3 ms; full redraw < 4 ms; emitted bytes for a one-row change < 400 B.

## 7. Input pipeline

```
host terminal ─► client decoder ─► keymap (client-side) ─┬─► Vibeke command (palette, split, …) ─► API
                                                          └─► InputEvent ─► server ─► per-pane encoder ─► holder ─► app
```

### 7.1 Client side: decode everything the host can tell us

- At attach the client **requests the richest protocol the host supports**: push kitty keyboard flags `0b11111` (disambiguate, report event types, alternate keys, all keys as escapes, associated text) if supported; else enable `modifyOtherKeys=2`; else fall back to legacy decoding. Restore the host's prior state on detach or crash (installed in a panic hook and signal handlers).
- Decode into a normalized event:

```rust
struct KeyEvent {
    key: Key,                      // logical: Char(char) | Named(Enter|Tab|Esc|Backspace|F(n)|Up|…) | Keypad(..)
    base_layout_key: Option<char>, // kitty "alternate key" (base layout), used for keybinding matching on non-US layouts
    shifted: Option<char>,
    text: Option<String>,          // associated text, what the user actually typed (AltGr output, dead keys, IME)
    mods: Mods,                    // shift ctrl alt super hyper meta caps num
    kind: Press | Repeat | Release,
}
enum InputEvent { Key(KeyEvent), Paste(String), Mouse(MouseEvent), FocusIn, FocusOut, Raw(Vec<u8>) /* escape hatch */ }
```

- Keybinding matching uses `base_layout_key` + mods, so `prefix+shift+t` works on Norwegian and German layouts.
- **AltGr**: when the host reports associated text, a key with `ctrl+alt` (AltGr on Windows/Linux) and non-empty `text` is **text input, not a chord**. It never matches a keybinding unless the binding explicitly names `altgr+…`. With legacy encodings we can't distinguish them. `keys.altgr_mode = "text" | "chord"` (default `text` on non-US keyboard locales).

### 7.2 Server side: the one canonical encoder

The server is the **only** place that turns logical key events into bytes for an app. The client sends `InputEvent::Key` with the decoded logical key; it never pre-encodes. `InputEvent::Raw` exists only for explicit raw-byte APIs (`pane.send_bytes`) and is never produced by key handling. Every `InputEvent` carries an `input_id` that the holder acks (01 §1.2).


Each pane's app negotiates its own keyboard mode (kitty flags stack via `CSI > flags u`, `modifyOtherKeys` via `CSI > 4 ; n m`, DECCKM, DECKPAM). The VT engine tracks it, and the per-pane encoder emits **exactly what that app asked for**, independent of the host:

| App requested | Shift+Enter is sent as | Ctrl+I vs Tab | Alt+x |
|---|---|---|---|
| kitty flags ≥ 1 | `CSI 13;2u` | distinct (`CSI 105;5u` vs `\t`) | `CSI 120;3u` |
| modifyOtherKeys 2 | `CSI 27;2;13~` | distinct | `CSI 27;3;120~` |
| legacy | `\r` (or `\n` if `keys.shift_enter_legacy = "lf"`) | identical `\t` | `ESC x` |

This avoids the class of bugs where Shift+Enter or modified keys are lost or mangled depending on host terminal. The **host** only needs to tell the client what was pressed; the **pane** gets what it asked for. Release events are sent only to apps that requested event types.

Special cases:
- **Claude Code / Codex newline**: both accept Shift+Enter when they enable kitty or modifyOtherKeys. If a harness manifest declares `input.newline = "backslash-enter"` or similar (04), the encoder applies it when the app is in legacy mode.
- **Bracketed paste**: `Paste` events are wrapped in `ESC[200~ … ESC[201~` only if the pane enabled DECSET 2004. Embedded `ESC[201~` in the payload is stripped (paste injection defence). Pastes > 1 MiB ask for confirmation in the TUI. Over API, `pane.send_text {bracketed: auto|always|never}`.
- **Focus events**: forwarded only to the focused pane, and only if it enabled DECSET 1004. Switching tabs sends FocusOut/FocusIn to the old and new pane (apps like vim and Claude Code rely on this for redraw).
- **Mouse**: the client hit-tests chrome vs pane. Pane-targeted events are translated to pane-local coordinates and encoded in the mode/encoding the pane enabled (X10/1000/1002/1003, SGR 1006, SGR-pixels 1016). If the pane has no mouse mode, wheel scrolls Vibeke's scrollback view and drag selects (copy mode, §11), with `shift` bypassing app mouse capture as usual.

**Acceptance — keyboard fidelity matrix** (`tests/keyboard/`): for host terminals {Ghostty, Kitty, WezTerm, iTerm2, Terminal.app, Alacritty, Windows Terminal (M6), foot, GNOME Terminal} × layouts {US, Norwegian, German, French AZERTY} × app modes {kitty, modifyOtherKeys, legacy}, a scripted test sends key sequences (via each terminal's automation: kitty `@ send-text`, wezterm cli, AppleScript/xdotool) and a probe app in the pane records the bytes. Pass = byte-exact match with the expected table. The matrix runs nightly on macOS + Linux runners. Results are published in docs as a support grid.

## 8. Shell integration and OSCs

| Sequence | Behaviour |
|---|---|
| OSC 0/1/2 title | Pane auto-title (shown unless the user set a custom title). |
| OSC 7 cwd | Pane `cwd` (used for new splits with `new_cwd = "follow"`, task attribution, preview attribution). Falls back to `/proc/<fg_pid>/cwd` or `proc_pidinfo` on macOS when absent. |
| OSC 133 A/B/C/D | Prompt and command marks: copy mode `[`/`]` jumps between prompts; "select last command output"; command exit code shown in the pane frame for 5 s on non-zero; `pane.read --source last-command`. Vibeke ships zsh/bash/fish snippets (`vibeke shell-integration zsh`) for shells that don't emit them. |
| OSC 8 hyperlinks | Stored per pane in `HyperlinkTable`. Rendered to the host as OSC 8 when supported. **Ctrl (Cmd on macOS) hover** underlines the whole link, including wrapped and partially off-screen links. Ctrl+click opens via the host if it handles OSC 8, else via `open`/`xdg-open` on the **client's** machine. For remote panes, `localhost:PORT` URLs are rewritten to their preview-fabric URL (06). Plain-text URLs are detected with a linkifier as a fallback. |
| OSC 52 clipboard | Set: allowed by default for local panes (`clipboard.osc52_write = "allow"`; remote panes follow `clipboard.remote_write`, default `ask_once`, 06 A9), forwarded to the client, which writes it to the host via OSC 52 or, if the host lacks it, the client OS clipboard (pbcopy, wl-copy, xclip, clip.exe). Works for remote panes because the bytes travel over the render stream. Query (read): denied by default (`osc52_read = "deny" \| "ask" \| "allow"`); "ask" pops an interaction-style prompt naming the pane. |
| OSC 9 / OSC 777 / OSC 99 notifications | `EngineEffect::Notify` → `notification.created{kind: osc9\|osc777}` → notification pipeline (08 §7). |
| OSC 9;4 progress | Shown as a thin progress bar in the pane's sidebar row and tab. |
| OSC 4/10/11/12 colour set/query | Per-pane palette. Queries are answered from the **current Vibeke theme palette** (§10.4), so apps detect light/dark correctly. |
| OSC 1337 (iTerm2) | `File=` images → §9. `SetUserVar` → pane metadata (plugins can read it). Others ignored. |
| DCS tmux passthrough | Unwrapped when `terminal.allow_passthrough = true` (default false), for apps that assume tmux. |

## 9. Graphics

Normalized internal model: `ImageEvent::{Transmit{hash, w, h, fmt, bytes}, Place{hash, pane_cell_rect, z, crop}, Delete{selector}}`. Every image is stored once in the pane's `ImageTable` keyed by content hash. Images ≥ 256 KiB go to the blob store.

- **Inbound** (app → pane): kitty graphics (direct, file, temp-file and shared-memory transmission; the server reads the file/shm itself, since the client may be remote), unicode placeholders (U+10EEEE), sixel (decoded to RGBA), and iTerm2 `File=` (decoded). All become `ImageEvent`s.
- **Outbound** (client → host), per `HostCaps`: kitty graphics with unicode placeholders (preferred: correct clipping in splits and with scrolling), else sixel (re-encoded and clipped to the pane rect), else iTerm2 inline, else a text placeholder plus `vibeke image open <hash>`.
- **Remote**: image bytes cross the link once per client per hash (`WantImage`); placements are tiny. A 2 MB screenshot shown in 3 places costs 2 MB once.
- Limits: `graphics.max_image_bytes` (default 32 MiB), `graphics.max_total_per_pane` (256 MiB, LRU-evicted).

This is the foundation for previews and screenshots rendered inline (06): a screenshot captured on a remote machine is just an image placed in a pane or popup.

**Acceptance:** `kitty +kitten icat`, `chafa`, `timg`, `viu` and a sixel test file render correctly in a split pane on Ghostty, Kitty, WezTerm and iTerm2, locally and over a remote link, and are clipped correctly when the split is resized.

## 10. Text styling, Unicode and theme

### 10.1 Colour
24-bit colour end to end. If the host lacks truecolor (`COLORTERM` unset and no detection), the client quantizes to 256 colours. Panes always see `COLORTERM=truecolor` and `TERM=xterm-256color` (configurable; a `vibeke` terminfo entry is installed by `vibeke doctor --fix` for apps that want it).

### 10.2 Underlines
SGR 4:0–4:5 (none/single/double/curly/dotted/dashed) and SGR 58/59 underline colour are preserved in `Style` and emitted to hosts that support them. Otherwise they degrade to a single underline in the default colour.

### 10.3 Width and graphemes
`vk-term` owns a width function shared by server and client so both always agree:
- Grapheme segmentation per UAX #29 (`unicode-segmentation`). Width from Unicode 16 East Asian Width + emoji presentation.
- **VS-16** (U+FE0F) forces width 2 and **VS-15** forces width 1 for emoji that have both presentations. ZWJ sequences count as one cluster of width 2.
- The **pane's** width semantics are per pane and never depend on which client is attached: mode 2027 (grapheme clustering) if the app enables it, else `terminal.grapheme_width = "unicode" | "legacy"` (default `legacy`, matching what most apps' own width calculations assume).
- Each **client** adapts presentation to its host: if the host's width behaviour differs from the pane's for a cell, the compositor pads or replaces the cluster (e.g. emits an explicit cursor move after a wide emoji) so alignment holds on that host. A host difference never changes the grid other clients see.
- Test vectors: `tests/unicode/width.txt` (> 2,000 cases), run against both the engine and our renderer.

### 10.4 Theme and light/dark propagation
- The Vibeke theme defines the chrome palette **and** the default pane palette (16 ANSI + fg/bg/cursor/selection).
- Chrome theme is per client: `theme.auto_switch = true` makes each client follow its own host background (OSC 11 query on focus-in, plus DECSET 2031 colour-scheme notifications where supported), switching between `dark_name` and `light_name`.
- The **pane** palette is shared state, so it follows one source: the client holding the geometry controller lease (§5), debounced 2 s, or a fixed choice (`theme.pane_palette = "follow-controller" | "dark" | "light"`).
- On a pane-palette switch: (1) every pane's default palette is updated; (2) panes that enabled DECSET 2031 receive the colour-scheme-change report `CSI ? 997 ; 1|2 n`; (3) OSC 10/11 queries now answer with the new colours.
- Per-pane palette overrides that an app set with OSC 4/10/11 are kept until the app resets them.

## 11. Copy mode, selection and scrollback

### 11.1 Copy mode
Entered by `prefix+[` (and `prefix+e` for the editor flow below), mouse wheel up in a pane without mouse mode, or `vibeke pane copy-mode`.
- **Vi keys** by default (`h j k l w b e 0 $ g G H M L ctrl+u ctrl+d v V ctrl+v y`), emacs set available (`copy_mode.keys = "vi" | "emacs"`). Every key is rebindable.
- **Search**: `/` forward, `?` backward, `n`/`N`, smart-case, regex toggle `ctrl+r`. Matches are highlighted across the whole scrollback (in-memory rows + archived segments via FTS for "find in older history" prompts) (D#563).
- Prompt jumps `[`/`]` (OSC 133), "select output of command under cursor" `o`.
- Yank → OSC 52 to the host and the system clipboard (§8). Optional `clipboard.copy_on_select = true` copies when a mouse selection ends (D#748). On Linux with X11/Wayland it can also set PRIMARY (`copy_mode.primary_selection = true`).
- Selection is rectangular with `ctrl+v`, line-wise with `V`, and unwraps soft wraps when copying.

### 11.2 Unlimited, searchable scrollback
- In memory: `terminal.scrollback_lines` (default 10,000) per pane.
- Evicted rows → archiver task → append to the current segment file `scrollback/<pane-ulid>/<seg>.zst` (zstd, 1 MiB uncompressed per segment). Each row is stored as `text` plus optional `style runs` (`terminal.archive_styles = true`). Unwrapped text lines go into `scrollback_fts`.
- Alternate-screen content is not archived (it never enters scrollback). Agent transcripts cover that gap (04).
- `vibeke search <query> [--pane --workspace --since]` and the TUI search palette (`prefix+/`) query FTS across all panes; selecting a result opens copy mode at that line, loading the segment.
- Retention: `terminal.archive_max_per_pane` (default 200 MiB compressed) and `archive_days` (30); the oldest segments are deleted first.

### 11.3 Edit scrollback in `$EDITOR`
`prefix+e` dumps the pane's scrollback (in-memory + the last `N` archived lines, unwrapped, optionally with ANSI) to a temp file and opens `$VISUAL`/`$EDITOR` in a **popup pane** at the line matching the current viewport. On close, the temp file is deleted. `copy_mode.editor_include_ansi = false`.

**Acceptance:** search across 50 panes × 1M archived lines returns first results in < 150 ms; a copy-mode yank of a 500-line wrapped selection pastes byte-identical text in vim on the host; copy-on-select and PRIMARY work on Wayland and X11.

## 12. Render pacing and CPU budgets

### 12.1 Frame pacing
- Local clients: up to `ui.max_fps` (default 120). Frames are produced only when damage exists. An idle screen produces **zero** frames.
- Coalescing window: damage is collected for at most `1000/max_fps` ms from the first change, then sent. Keystroke echo bypasses coalescing: if input was sent to a pane within the last 50 ms, the next damage on that pane is flushed immediately (latency first).
- Remote clients: adaptive (06), starting at 60 fps and backing off by measured RTT and throughput.

### 12.2 Idle and load budgets (CI-enforced with `tests/perf/`)

| Scenario | Budget (M2 MacBook Air, release build) |
|---|---|
| Server idle, 30 panes, 15 agents idle | < 0.3% CPU avg over 60 s; 0 wakeups/s from timers we own except a 1 Hz housekeeping tick |
| Client idle (attached, nothing changing) | < 0.2% CPU; 0 frames |
| 15 agents "working" with spinners, 1 focused | server + client ≤ 5% of one core combined (10 §1.3 sets ≤ 3% for 5 spinners) |
| Keystroke-to-echo added latency (local) | p50 ≤ 1 ms, p99 ≤ 3 ms over the bare terminal (same as 10 §1.1) |
| `cat` 200 MB into a focused pane | completes ≤ 1.3× bare-terminal time; UI stays responsive (input latency p99 < 30 ms) |
| Close tab with 4 panes | < 50 ms |
| Memory | ≤ 25 MiB server baseline + ≤ 6 MiB per pane at 10k scrollback (same as 10 §1.3) |

### 12.3 Spinner and animation throttling
`ActivityTracker` classifies each pane's recent damage:
- `spinner_only`: ≤ 3 cells changing in a cycle with a period < 500 ms and no other row changes (Claude's ✻ spinner, Codex's dots, progress bars). Heuristic plus a per-harness `animation_regions` hint from manifests (04).
- `streaming`: append-like changes at the bottom.
- `redraw`: large-area changes.

Policy: unfocused or invisible panes classified `spinner_only` are sent at most `ui.background_animation_fps` (default 4; 1 for remote, 0 = freeze). Focused panes are never throttled. Agent *state* is never derived from these throttled frames: detectors read the server-side `detect_view`, which is always current.

### 12.4 Invisible panes
Panes not visible in any attached client's viewport (other tabs, zoomed-out) are still parsed (needed for detection and snapshots) but generate no render frames. Their damage is folded into a "dirty" flag and sent as one full frame when they become visible.
