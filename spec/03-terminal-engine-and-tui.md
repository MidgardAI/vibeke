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
- The trait is an internal seam, not a promise of pluggability. **Exactly one engine ships**: libghostty-vt (§2). Engine gaps are fixed by patching that engine (upstream PR or a fork under our org), not by re-implementing terminal state around it. The only thing `vk-term` owns outside the engine is the shared width table used by both server and client (§10.3).
- *As built (lane 1B):* `vk_term::VtEngine` (`vk-term/src/vt.rs`) is the seam, implemented by `vk_term::Engine`: `create, feed, resize, take_damage, size, visible_row, scrollback_len/row, cursor, modes, title, cwd, scrolled_total, prompt_lines, last_command, serialize/deserialize, set_replaying`. `EngineEffect` is `vk_term::Effect`: `Reply, Bell, TitleChanged, Notify, Clipboard, ClipboardQuery, Cwd, Mark {kind: A|B|C|D, exit}, Progress {state, pct}, UserVar {name, value}`. Hyperlinks and shell marks are not effects: the engine keeps them in its grid and every `Row` carries them (`Row.mark` = none / prompt / prompt continuation, `Row.links` = `{col, cols, uri}` runs), so they survive scrollback, reflow and snapshots without a side table. Images are read from the engine's kitty store (`Engine::image_placements`, `image_rgba`). Not built: `drain_evicted` (the pane task archives from `history_row`), `ColorQueryNeedsClient`, `PixelSize` on resize.

## 2. Engine: libghostty-vt (decided 2026-10-06)

**Decision:** the one engine is **libghostty-vt**, Ghostty's VT core built as a C-ABI library from the Ghostty source tree. It replaces the `alacritty_terminal` binding written during M0. The three-way scored spike originally planned here was not run. The decision rests on the evidence below; the C4 gate (§2.3) is still the acceptance test for the binding.

### 2.1 Why

| Concern | libghostty-vt (Oct 2026) | alacritty_terminal (what M0 built) |
|---|---|---|
| **C4 recovery (hard gate)** | Native snapshot API (`include/ghostty/vt/snapshot.h`, upstream since 2026-08-03): a CRC-protected record stream with terminal state, every screen, the **unfinished VT/UTF-8 parser continuation**, then history. Incremental restore: the terminal is usable at READY, history pages prepend afterwards. Covers modes (current/saved/default), palette and overrides, PWD, title, tab stops, scroll region, cursor and saved cursor, charsets, kitty keyboard stack, semantic prompts. | Needed a vendored patch to `alacritty_terminal` (snapshot/restore), a vendored fix to `vte` (dropped character after a split UTF-8 codepoint), and our own `Tracker` capturing pending parser bytes. |
| Graphics | Kitty graphics parsed by the engine (transmit/place/delete, unicode placeholders), with a C API (`kitty_graphics.h`). | None; all inline graphics would be ours to build. |
| OSCs and shell integration | OSC 7, 8, 52, 133 (semantic prompts), notifications and others parsed by the engine. | OSC 7/9/99/777/133, XTVERSION, DA3, modifyOtherKeys tracked outside the engine by us. |
| Conformance | Ghostty's own terminal core; SIMD parser. | Good, but narrower. |
| Embeddable from Rust | libghostty-vt exposes a C ABI that links statically into a Rust binary on macOS, Linux (incl. musl) and Windows. | — |
| Cost | Zig 0.16 in the build; C API marked work-in-progress (breaking changes possible); snapshot format v1 has no binary-compatibility guarantee. | Pure Rust, stable crate. |

The C4 gate was the reason this spec previously ranked libghostty-vt as risky ("continuation handling unfinished"). Upstream has since built exactly that requirement, and the remaining costs are build-time, not correctness.

### 2.2 How we embed it

- **Vendored source, pinned commit.** The Ghostty source subset needed for `zig build -Demit-lib-vt` lives in `vendor/libghostty-vt/` with a `vendor/libghostty-vt.vendor.json` (source commit) and `vendor/libghostty-vt.patches.md` (every local patch: reason, upstream PR, removal condition, verification). Updating the pin is a reviewed change that re-runs the C4 gate and the VT corpus.
- **Build.** `vk-term`'s `build.rs` runs `zig build -Demit-lib-vt -Doptimize=ReleaseFast -Dtarget=<zig triple>` into `OUT_DIR` and links the static library. Targets: `aarch64-macos`, `x86_64-macos`, `x86_64-linux-musl`, `aarch64-linux-musl` (Windows in M6). The single-static-binary promise (01 §2) holds.
- **Toolchain.** Zig 0.16.0 is pinned in `mise.toml` next to Rust, so `mise install` and CI (`jdx/mise-action`) provide it. No system Zig is assumed.
- **Bindings.** Raw FFI generated with `bindgen` from the vendored headers and checked in (regenerated when the pin moves), behind a small safe wrapper in `vk-term`. [libghostty-rs](https://github.com/uzaaft/libghostty-rs) (MIT/Apache, wraps the snapshot API on master) is a reference and may be used directly if it tracks our pin. Third-party bindings that lag Ghostty must not hold back the pin.
- **Snapshot versioning.** `engine = "libghostty-vt"`, `engine_version = <vendored commit>`. Per §1, a snapshot from another version is discarded and recovery falls back to ring-only replay, so the format's lack of a compatibility guarantee costs at most one degraded recovery across an upgrade.

### 2.3 Acceptance gate (carried over from M0)

- **C4 hard gate:** feed the corpus (`tests/vt-corpus/`), cut at every 4,093rd byte and at every byte of a synthetic stream full of split escape and UTF-8 sequences, snapshot, restore into a fresh terminal, continue; the screen and modes must equal the uninterrupted run. The existing `crates/vk-term/tests/recovery.rs` is ported to the new binding unchanged in intent.
- Continuation tracking (`GHOSTTY_TERMINAL_OPT_CONTINUATION_MAX_BYTES`) must be enabled **before** the first byte is fed; the encoder refuses to snapshot a mid-sequence parser otherwise. Restores use `GHOSTTY_SNAPSHOT_DECODER_OPT_RETAIN_CONTINUATION` so the restored terminal can be snapshotted again.
- Throughput measured against 10 §1 (≥ 300 MB/s target) and recorded; `esctest2`/`vttest` pass rates recorded as the CI baseline (10 §3).
  - *As built (lane 1B):* esctest2 is GPL-2.0, so it is neither vendored nor derived from (the workspace is Apache-2.0); `crates/vk-term/tests/conformance.rs` keeps independently written cases in its spirit (now also OSC 8 links, OSC 133 marks, SetUserVar, OSC 9;4). vttest is MIT/X11: `crates/vk-term/tests/vttest_derived.rs` replays the escape programs of its cursor-movement screens (the `*`/`+` border with the frame of `E`s, the autowrap demo, controls inside CSI, leading zeros) exactly as vttest emits them and checks the screen vttest asks the user to verify, whole and byte by byte.
- Decision record: `.adr/0001-vt-engine.md` summarizing §2.1, the gate results and the gaps below.

### 2.4 Gaps we own

- **Kitty image payloads are not in snapshots.** Placeholder cells survive, but image and placement state does not. `PaneScreen` already keeps images in its own `ImageTable` by content hash (§9); on restore, Vibeke re-transmits the stored images and placements into the engine before replay. Until that is built, images after a server restart are lost (the app's next redraw usually brings them back).
- **Sixel and iTerm2 `OSC 1337 File=` images** are not decoded by libghostty-vt. Inbound decoding to `ImageEvent` is ours (§9), or deferred if no real app needs it.
- **Width policy is build-time in the engine.** The shared width function (§10.3) must match the vendored Ghostty's grapheme/width behaviour, so server and client agree; `tests/unicode/width.txt` runs against both.
- **Unstable C API.** Expect breaking changes when moving the pin; the safe wrapper in `vk-term` is the only code that touches FFI.
- **Local patches** (if any, e.g. exposing the modifyOtherKeys level) are upstreamed where possible and tracked in the patch log.

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

*As built (lane 1B):* there is no `PaneScreen` type; the pane task's `Screen` holds the engine, and links, marks and images stay inside the engine (above). Live effect state that is not grid content (OSC 9;4 progress, the last non-zero exit code, OSC 1337 user vars) is kept by the server in `SessionModel.pane_live` (memory only, `vk-server/src/term_effects.rs`).

**Acceptance:** with 30 panes, 3 of them streaming output at 1 MB/s, server CPU for parsing + damage < 15% of one core on an M2. Idle CPU, see §12.

## 4. Snapshots and recovery

Implements the recovery contract in 01 §1.2. `vt_snapshots(pane_id, holder_offset, engine, engine_version, blob_hash, taken_at)`.

- **Triggers**: 2 s after output stops; at most every 30 s while busy; **when the holder sends `CheckpointWanted`** because 50% of its journal has been written since the last acknowledged checkpoint; on graceful shutdown.
- **Cut points**: snapshots are taken only at an offset the holder has marked as a safe cut point (not inside UTF-8 or an escape sequence). The pane task feeds up to the cut point, serializes, then continues. Because the libghostty-vt snapshot also captures the unfinished parser continuation (§2.3 gate), a cut point is belt-and-braces, not a correctness requirement.
- Snapshot = `engine.serialize()` (the libghostty-vt snapshot stream) + `PaneScreen` metadata (marks, link table, image hashes), zstd-compressed and stored as a blob. Kitty image payloads are not in the engine snapshot (§2.4); they come from the `ImageTable` blobs. `holder_offset` = the cut-point offset.
- Snapshot work runs on a blocking-pool thread from a cloned engine state if the engine supports cheap clone; otherwise feeding is paused (bounded, < 5 ms for a 200×60 screen with 10k scrollback, else the snapshot is retried at the next cut point).
- **Recovery** (server start, per live pane):
  1. Deserialize the latest snapshot (the terminal is renderable at READY; history pages may finish restoring in the background); re-transmit stored images and placements (§2.4); `set_replaying(true)`.
  2. `Attach{from_offset: holder_offset}`; feed journal bytes and apply journaled `Resize` markers in order. `InputAck` markers update the input-id dedupe window.
  3. If the journal starts after `holder_offset` (overflow), reset the screen and replay the whole journal from its first cut point: `pane.recovered{method: ring_only}`.
  4. `set_replaying(false)`. Answer the holder's queued screen-dependent queries (≤ 5 s old) from current state.
  5. If the foreground process is a TUI (alternate screen active, or a harness manifest marks it), send the **resize nudge** (cols−1 then cols, 50 ms apart) so the app repaints.
  6. Restore scrollback above the screen from the archive (§11.2), not from the journal.

**Acceptance** (matches 10 §5): kill -9 the server while 10 agents and 10 shells are mid-output → zero processes lost, zero inputs applied twice, no replayed side effects (no duplicate notifications, clipboard writes or query replies); agent TUI panes equal a reference run cell-for-cell after the resize nudge in 100% of runs; raw-shell panes are tracked by a separate visual-fidelity metric (10 §5.2), not a gate.

## 5. Render stream (server → TUI client)

**The normative wire schema is [07-api-cli-plugins.md](07-api-cli-plugins.md) §3** (`vk-proto::render`). This section only describes the behavior the TUI relies on:

- **Media channel.** Pixel content that is not terminal output (browser panes and watched agent sessions, 06 B3.2/B7) travels as changed image tiles on a separate latest-wins channel per pane, at lower priority than cell frames, and only while the pane is visible on that client.
  - *As built (Goal 03 Stage 2):* the channel shares the render stream connection. The client declares its visible browser panes with `ClientFrame::MediaView {panes: [MediaPane {pane, owner, spec, cols, rows, cell_w, cell_h, dpr}], shm, key_releases}` (replaces the previous set; an empty set stops everything) and the server answers with `ServerFrame::Media(MediaFrame {pane, seq, width, height, cell_w, cell_h, tile_cols, tile_rows, grid_cols, grid_rows, reset, tiles: [MediaTile {index, col, row, cols, rows, w, h, data: Shm{name,len} | ZlibRgba | Rgba}]})` and `ServerFrame::BrowserState {pane, state}` (chrome). The client acks with `MediaAck {pane, seq}`; at most 2 (local) / 1 (remote) media frames per pane are unacked, and media frames are written after the cell frames of each session-loop pass. Latest-wins is per subscriber: dirty tiles accumulate while the client can't take more and are read from the current frame when it can. Browser input goes back as `ClientFrame::Browser {input_id, pane, cmd: BrowserCmd}` (key, text, mouse in CSS px, wheel, navigate, back/forward, reload, stop, window, screenshot). Variants were appended to `ServerFrame`/`ClientFrame`, so existing postcard discriminants are unchanged — but that does not make the stream compatible: postcard is positional, and the appended `Pane.browser` field shifts every following pane and model field (Goal 03 review). The render protocol is therefore 2, and `render.attach` refuses any other version (07 §3). Media frames go out in parts sharing a `seq` (only the first carries `reset`; each part is acked). Details: 06 B3.2 implementation notes.
- **Event push.** *As built (Goal 03 Stage 3 follow-up):* servers whose `render.attach` result lists `event_push` push the events a client subscribed to (`ClientFrame::Subscribe` → `ServerFrame::Events`, 07 §3) on the same connection, so the TUI no longer polls `events.read` every second for the confirm overlay, nor `client.list` every 15 s for the devices indicator (refreshed on `client.attached` / `client.detached` / `client.devices_changed`; a 120 s safety poll remains). One `events.read` after each (re)connect and after a `lagged` frame picks up anything published before the subscription; older servers keep the polling path.
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
- *As built (Goal 03 Stage 2):* the attach probe also sends the browser-pane batch from `vk-browser::probe` before the DA1 sentinel — kitty `a=q` direct and over a 3-byte shm object (unlinked afterwards), `CSI 16 t`, `14 t`, `18 t`, DECRQM 1016 — and folds the answers into `HostCaps {kitty_graphics, kitty_shm, iterm2_images (TERM_PROGRAM=iTerm.app without kitty graphics), cell_w, cell_h, dpr_x100, sgr_pixels}`. The cell size prefers TIOCGWINSZ pixel size ÷ cells (re-read on every resize, so font zoom follows), else `CSI 16 t`, else 10×20 (the host scales tiles into their cells). The DPR is a heuristic (`VIBEKE_DPR`, else 2 when cells are ≥ 28 px tall) `[verify M3: host]`. Kitty graphics imply truecolor (placeholder ids are 24-bit colours).
- `vibeke doctor terminal` prints this table with a pass/warn per feature.
- *As built (lane 1B):* the probe adds DECRQM 2004, 1004 and 1006 (bracketed paste, focus events, SGR mouse) and waits at most 150 ms in total (`term::PROBE_TIMEOUT`; `vibeke doctor` keeps 200 ms). `ProbeResult`/`HostCaps` gained `undercurl` (kitty keyboard or a known terminal), `osc8` (known terminals, VTE, Windows Terminal), `focus_events`, `sgr_mouse`, `bracketed_paste`, `sixel` (DA1 attribute 4) and `notifications` (`Osc99` kitty, `Osc777` Ghostty/WezTerm/foot, `Osc9` iTerm2, else none). `terminal.host_overrides` is applied by the client after the probe (`ProbeResult::apply_overrides`, `caps::graphics_overrides` for `kitty_graphics`, `kitty_shm`, `iterm2_images`, `sgr_pixels`). `vibeke doctor` reports the new rows; a separate `doctor terminal` subcommand is still not built.

### 6.2 Composition

The client keeps a `ClientScreen`: its copy of each visible pane's rows plus chrome models. Each frame:
1. Apply `PaneFrame` deltas (shift + replace rows) into per-pane buffers.
2. Compose the full host-sized grid: chrome (sidebar, tab bar, status bar, borders, popups) via `ratatui` widgets rendering into the same cell grid, and pane content blitted from pane buffers into their rects. Pane cells are copied, not re-rendered through widgets.
3. Diff against the previous composed grid and emit the minimal escape sequence stream (cursor moves, SGR changes, text), wrapped in synchronized-update (`CSI ? 2026 h/l`) when the host supports it.
4. Images: place via kitty graphics (with unicode placeholders so they clip correctly at pane edges), else sixel, else iTerm2 inline, else a `[image 640×480 · click to open]` placeholder cell span (§9).
5. Cursor: shown at the focused pane's cursor with the app's requested shape; hidden while popups have focus.
6. **The focused pane is the agent's** (08 §0): the compositor draws Vibeke content over the focused pane's cells only for user-invoked popups (palette, goto, peek, cards), never spontaneously; toasts and attention badges live in chrome (sidebar, tab bar, status bar). Vibeke never re-renders an agent's TUI from structured data in a pane — pane cells always come from the agent's PTY.

The compositor runs on its own thread with a frame budget. It never waits on the network; it draws whatever state it has.

**Acceptance:** on a 300×80 host with 6 visible panes, composing a frame with one changed row costs < 0.3 ms; full redraw < 4 ms; emitted bytes for a one-row change < 400 B.

## 7. Input pipeline

```
host terminal ─► client decoder ─► keymap (client-side) ─┬─► Vibeke command (palette, split, …) ─► API
                                                          └─► InputEvent ─► server ─► per-pane encoder ─► holder ─► app
```

### 7.1 Client side: decode everything the host can tell us

- At attach the client **requests the richest protocol the host supports**: push kitty keyboard flags `0b11111` (disambiguate, report event types, alternate keys, all keys as escapes, associated text) if supported; else enable `modifyOtherKeys=2`; else fall back to legacy decoding. Restore the host's prior state on detach or crash (installed in a panic hook and signal handlers).
  - *As built (lane 1B):* the client writes `CSI > 31 u` itself (`term::KITTY_FLAGS`; crossterm has no constant for bit 16). crossterm 0.29's decoder accepts the extra text field but drops it, so the associated text is not yet used for AltGr; that needs the client's own CSI u decoder.
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
| OSC 52 clipboard | Set: allowed by default for local panes (`clipboard.osc52_write = "allow"`; remote panes follow `clipboard.remote_write`, default `ask_once`, 06 A9), forwarded to the client, which writes it to the host via OSC 52 and, when the client is not running over SSH, also the client OS clipboard (pbcopy, wl-copy, xclip; §11.1 copy delivery). Works for remote panes because the bytes travel over the render stream. Query (read): denied by default (`osc52_read = "deny" \| "ask" \| "allow"`); "ask" pops an interaction-style prompt naming the pane. |
| OSC 9 / OSC 777 / OSC 99 notifications | `EngineEffect::Notify` → `notification.created{kind: osc9\|osc777}` → notification pipeline (08 §7). |
| OSC 9;4 progress | Shown as a thin progress bar in the pane's sidebar row and tab. |
| OSC 4/10/11/12 colour set/query | Per-pane palette. Queries are answered from the **current Vibeke theme palette** (§10.4), so apps detect light/dark correctly. |
| OSC 1337 (iTerm2) | `File=` images → §9. `SetUserVar` → pane metadata (plugins can read it). Others ignored. |
| DCS tmux passthrough | Unwrapped when `terminal.allow_passthrough = true` (default false), for apps that assume tmux. |

*As built (lane 1B; the engine parsed these before, the server dropped them):*
- **OSC 133**: prompt rows come from the engine grid (`Row.mark`). Copy mode `[`/`]` jump between prompt rows of the loaded scrollback (at the top, older rows load and the next press continues), `o` selects the output of the command at or above the cursor (command-line and continuation rows and trailing blank rows excluded), `y` yanks it; `[keys.copy_mode]` actions `prompt_prev`, `prompt_next`, `select_output`. `pane.read {source: "last-command"}` (also `last_command`) returns the previous command's output at an idle prompt, the running command's output otherwise, with `running`, `exit_code` and `prompt_line`, or `not_found` with `details.reason = "no_marks"`. A non-zero `D` exit code goes to `pane_live.last_exit` and is shown for 5 s (08 §3); the next command's `C` clears it. Archived rows keep no marks. The `vibeke shell-integration` snippets are not built.
- **OSC 7**: unchanged, plus a live fallback: without OSC 7, `pane_cwd` reads the foreground process group's (else the child's) cwd now (`/proc/<pid>/cwd`, `proc_pidinfo`), then the holder's last report, then the start cwd.
- **OSC 8**: links are on rows (`Row.links`, URIs over 2 KiB dropped). The client passes them to hosts with `HostCaps::osc8` (`OSC 8 ; id=vk<hash> ; uri`, only `http https mailto file ftp` targets without control characters) and handles hover/click itself (08 §6.8). The localhost → preview-fabric rewrite is the existing `nav::open_url` behaviour: loopback URLs open as a browser pane on the pane's machine.
- **OSC 52 read**: the engine's query goes to one client, the most recently active one showing the pane (`ServerFrame::ClipboardQuery`); no client → denied. The client applies its own `clipboard.osc52_read` (the clipboard read is that machine's): `deny` answers "denied" and toasts that the pane tried; `allow` reads (`pbpaste`/`wl-paste`/`xclip`/`xsel`, or `VIBEKE_CLIPBOARD_READ_CMD`) and toasts how many bytes went to which pane; `ask` queues a non-modal notice and `review_clipboard` (`prefix+y`) opens a prompt naming the pane (`y` once, `a` this pane until detach, `n`/`esc` deny). Over SSH without the override the read is denied (the clipboard there is not the user's). Only a granted reply (`ClientFrame::ClipboardReply`, from that client, once, ≤ 1 MiB) is written to the pane as `OSC 52 ; c|p ; base64`; a denial writes nothing. At most two open queries per pane.
- **OSC 9;4**: `pane_live.progress` (`normal | error | indeterminate | paused`, pct clamped to 100; state 0 clears), drawn in the tab and sidebar (08 §3).
- **OSC 1337 SetUserVar**: read by `vk-term`'s side scanner (the engine logs it as unimplemented), names `[A-Za-z0-9_.-]` ≤ 64 bytes, values ≤ 4 KiB, control characters stripped, at most 32 per pane, an empty value removes; in `pane_live.user_vars` and `pane.get` → `live`.
- None of these effects is replayed during journal replay after a restart.

## 9. Graphics

Normalized internal model: `ImageEvent::{Transmit{hash, w, h, fmt, bytes}, Place{hash, pane_cell_rect, z, crop}, Delete{selector}}`. Every image is stored once in the pane's `ImageTable` keyed by content hash. Images ≥ 256 KiB go to the blob store.

- **Inbound** (app → pane): kitty graphics (direct, file, temp-file and shared-memory transmission; the server reads the file/shm itself, since the client may be remote) and unicode placeholders (U+10EEEE) are parsed by libghostty-vt and read through its kitty graphics API. Measured in Goal 03 Stage 0 (`crates/vk-browser/tests/engine_parse.rs`): the vendored engine acknowledges raw `f=24/32` direct transmission, chunking and placeholders, but answers `f=100` PNG with "unsupported format" until Vibeke supplies a PNG decoder, `t=s`/`t=t` with "unsupported medium" (the server must read shm/files itself, as above), and rejects `o=z` streams containing dynamic-Huffman blocks ("decompression failed", upstream to check). Sixel (decoded to RGBA) and iTerm2 `File=` (decoded) are not handled by the engine and are decoded by `vk-term` (§2.4). All become `ImageEvent`s.
- **Outbound** (client → host), per `HostCaps`: kitty graphics with unicode placeholders (preferred: correct clipping in splits and with scrolling), else sixel (re-encoded and clipped to the pane rect), else iTerm2 inline, else a text placeholder plus `vibeke image open <hash>`.
  - *Browser panes (Goal 03 Stage 2):* each media tile is its own kitty image (`a=T,U=1,c=,r=,p=1,q=2`) under a stable per-pane id; the pane's content cells are placeholders (`U+10EEEE` + row and column diacritics, id in a 24-bit fg colour) composed into the grid like any text, so the existing diff writes them once per layout and popups clip them. Tile data goes as `t=s` shm names when the server is on this machine and the host reads shm, else as chunked `t=d,o=z` (raw RGBA where `o=z` is known to fail). iTerm2 without kitty graphics gets OSC 1337 PNGs of the whole pane at ≤ 5 fps. Verified against Vibeke's own engine (each tile `OK`, placeholders on screen; `vk-tui` `browser` tests); real-host rendering is `[verify M3: host]`.
- **Remote**: image bytes cross the link once per client per hash (`WantImage`); placements are tiny. A 2 MB screenshot shown in 3 places costs 2 MB once.
- Limits: `graphics.max_image_bytes` (default 32 MiB), `graphics.max_total_per_pane` (256 MiB, LRU-evicted).
- *As built (lane 1B), inbound kitty graphics on terminal panes:* `vk-term` sets the engine's kitty storage limit to 256 MiB per screen and its APC buffer to fit a 32 MiB image, enables the shared-memory and temp-file (temp directory only) media and keeps `t=f` (arbitrary paths) off, since the server would read them on the program's behalf; it installs a bounded PNG decoder (`image` crate, ≤ 10 000 px per side, ≤ 32 MiB RGBA) so `f=100` works. The limits are constants (`MAX_IMAGE_BYTES`, `MAX_IMAGES_PER_PANE`); the `graphics.*` config keys are not read yet. The render session lists the visible non-virtual placements after each pane frame: `ServerFrame::Image {hash, width, height, rgba_z}` once per client per content hash (the image cropped to the placement's source rectangle), then `ServerFrame::PaneImages {pane, epoch, places}` when the set or a position changed. The client transmits each image's pixels once to a kitty host (`a=t`, the cached zlib stream as an `o=z` payload unless the host is Vibeke or `VIBEKE_KITTY_ZLIB=0`; ids from 2^24 up) and sizes it with one virtual placement (`a=p,U=1,p=1`, re-sent without pixels when the size changes), then draws unicode placeholders in the covered pane cells, iterating only the cells inside the pane (clipped at the pane edges and under popups). Placeholder cells carry no placement id, so an image has one size on the host at a time, its first visible placement's; other sizes of it that frame get the label. Transmissions are capped at 16 MiB of output per frame (at least one image goes; the rest follow on the next frames). Without kitty graphics it draws an `[image W×H]` label. Its cache is bounded (256 MiB, unplaced images evicted first); when a pane places an image evicted while unplaced, the client sends `Resync` for the pane and the server, which forgets that the pane's images were sent, sends the pixels again. Not built: virtual placements made by the program (its placeholder cells reach the host without the image), z-order below text, sixel/iTerm2 inbound, re-transmission after a restart (§2.4), and blob-store spill.

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
- Prompt jumps `[`/`]` (OSC 133), "select output of command under cursor" `o`. *Built (lane 1B), see §8.*
- Yank → OSC 52 to the host and the system clipboard (§8). `clipboard.copy_on_select = true` (the default) copies when a mouse selection ends (D#748). On Linux with X11/Wayland it can also set PRIMARY (`clipboard.primary_selection = true`).
- **Mouse selection** (built, `vk-tui::selection`): a left drag selects (highlighted while it grows) and enters copy mode; double click selects a word (everything but blanks, quotes, brackets, `,` `;` `|` and box drawing, so paths and URLs select whole; follows soft wraps), triple click the logical line. Dragging past the pane's top/bottom edge scrolls (and keeps scrolling on a 60 ms timer while the pointer stays there); the wheel scrolls during a drag and the selection end follows the pointer; drags and the release are tracked outside the pane. On release the text is copied (soft wraps joined, trailing blanks trimmed) with a `copied N chars` / `copy failed — why` toast. The scrollback viewer (§11.3) selects over its wrapped lines the same way (`y` copies a kept selection). With `primary_selection`, a middle click pastes the last copy into the pane.
- **Mouse-reporting apps** (agent TUIs, vim): `shift`+drag or `alt`/`option`+drag selects in Vibeke, a plain drag goes to the app. `clipboard.mouse_select_in_apps = "always"` makes every left drag a Vibeke selection; the app then gets clicks (press and release are sent on release, once the press is known not to be a drag) and the wheel. Host terminals keep some modifiers for their own native selection: Ghostty keeps `shift`+drag while mouse reporting is on (`mouse-shift-capture = false`, its default; `alt` is reported to the app, here Vibeke), iTerm2 keeps `option`+drag (reports `shift`). So `shift` works in iTerm2, `alt` in Ghostty, and `always` needs no modifier in either.
- **Copy delivery** (`vk-tui::copyout`): every copy is sent as OSC 52 (in a tmux DCS passthrough when `TMUX` is set; skipped above `clipboard.remote_write_max_bytes`) and, when the TUI runs on the machine with the display (no `SSH_CONNECTION`/`SSH_TTY`), also through `pbcopy` / `wl-copy` / `xclip` / `xsel`. Over SSH from iTerm2 (`LC_TERMINAL=iTerm2`) the first copy also toasts once: "If paste doesn't work: iTerm2 → Settings → General → Selection → enable 'Applications in terminal may access clipboard'" (iTerm2 silently drops OSC 52 otherwise). `VIBEKE_CLIPBOARD_CMD` replaces the platform tool (test hook).
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
