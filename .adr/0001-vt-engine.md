# ADR 0001: VT engine is libghostty-vt

- Status: accepted (decided 2026-10-06; binding landed and C4 gate passed 2026-10-06)
- Spec: [spec/03 §1–§4](../spec/03-terminal-engine-and-tui.md), [spec/01 §2](../spec/01-architecture.md)
- Supersedes: the M0 `alacritty_terminal` binding (vendored + patched `alacritty_terminal`/`vte`)

## Decision

Vibeke ships exactly one VT engine: **libghostty-vt**, Ghostty's VT core built as a C-ABI static
library. Reasons (spec/03 §2.1):

- **C4 recovery is native.** `include/ghostty/vt/snapshot.h` serializes terminal state, every
  screen, the unfinished VT/UTF-8 parser continuation and history (CRC-protected record stream).
  The alacritty binding needed a snapshot patch to `alacritty_terminal`, a fix to `vte` and our
  own pending-bytes tracker.
- **More of the terminal is the engine's:** kitty graphics (with a C API), OSC 7/8/52/133,
  notifications, progress, DA/XTVERSION/size queries, colour queries and overrides.
- **Conformance and speed:** Ghostty's own core with a SIMD parser (3.5x the alacritty binding,
  below).
- **Embeddable from Rust:** exposes a C ABI that links statically into a Rust binary.
- **Costs:** Zig 0.16 in the build, a C API marked work-in-progress, and snapshot format v1 has
  no cross-version compatibility promise (a snapshot from another pin is discarded and recovery
  falls back to ring-only replay, spec/03 §1).

## How it is embedded

| Item | Value |
|---|---|
| Upstream | https://github.com/ghostty-org/ghostty @ `35a81a980bb9fce09a1ea762a68b55f8eb3477ed` (2026-10-05, Ghostty 1.3.2-dev, libghostty-vt 0.1.0-dev) |
| Vendored source | `vendor/libghostty-vt/`: only the files `zig build -Demit-lib-vt` reads (~17 MB, 951 files, unmodified); manifest `vendor/libghostty-vt.vendor.json`, patch log `vendor/libghostty-vt.patches.md` (no patches) |
| Toolchain | Zig `0.16.0` (the pin's `minimum_zig_version`) in `mise.toml` |
| Build | `crates/vk-term/build.rs`: copies the vendored tree into `OUT_DIR`, runs `zig build -Demit-lib-vt -Doptimize=ReleaseFast -Dtarget=<triple> -Dversion-string=…`, links `libghostty-vt.a` statically. Targets: `aarch64-macos`, `x86_64-macos`, `x86_64-linux-musl`, `aarch64-linux-musl`, `x86_64-linux-gnu`, `aarch64-linux-gnu`. Zig's own dependencies (uucode, highway, wuffs, …) are hash-pinned in `build.zig.zon` and fetched once into the global Zig cache. Clean checkout + empty Zig cache → `cargo test --release -p vk-term` in ~2 min on an M1 Pro |
| FFI | `crates/vk-term/src/ghostty_sys.rs`, hand-written for exactly the symbols used; the `ffi_layout` test checks every struct layout and enum constant against the library's own ABI manifest (`ghostty_type_json`). Only `engine.rs` uses it |
| Versioning | `ENGINE = "libghostty-vt"`, `ENGINE_VERSION = "libghostty-vt+35a81a9"` |

### FFI surface

Terminal: `ghostty_terminal_{new,free,resize,set,vt_write,get,continuation_buf,grid_ref,grid_ref_track}`;
tracked refs: `ghostty_tracked_grid_ref_{free,has_value,point,set}`; cells and rows:
`ghostty_grid_ref_{cell,row,graphemes,style}`, `ghostty_cell_get{,_multi}`, `ghostty_row_get`;
snapshots: `ghostty_snapshot_encode`, `ghostty_snapshot_decoder_{new_buf,set,decode,free}`;
render state (damage and cursor shape): `ghostty_render_state_{new,free,update,clean,get}`,
`ghostty_render_state_row_iterator_{new,free,next_dirty}`; `ghostty_type_json` (tests).
Effect callbacks: write_pty, bell, xtversion, title_changed, size, device_attributes,
pwd_changed, clipboard_write, clipboard_read, desktop_notification, progress_report,
semantic_prompt, reset.

### What Vibeke still owns around the engine

- **Snapshot envelope** (`VKG1` magic + postcard header + engine snapshot): engine version,
  decoded cwd, modifyOtherKeys level, `scrolled_total` and its baseline. Continuation tracking
  (`GHOSTTY_TERMINAL_OPT_CONTINUATION_MAX_BYTES`, 65 MiB) is set before the first byte; restores
  use `GHOSTTY_SNAPSHOT_DECODER_OPT_RETAIN_CONTINUATION`, so restored terminals snapshot again.
- **`tracker.rs`**, reduced to the two sequences Ghostty parses but does not expose:
  `CSI > 4 ; Pv m` (the modifyOtherKeys *level*; Ghostty keeps a level-2 bool) and OSC 99 (kitty
  notifications; Ghostty drops them). It skips text with a search for ESC and is resynchronised
  after a restore from the engine's exported continuation. Everything else the M0 tracker did
  (pending bytes, OSC 7/9/777/133, XTVERSION, DA3, sync 2026) is now the engine's.
- **Exact scrollback window.** Ghostty prunes history by whole pages, so how many rows it keeps
  depends on page layout, which differs between a live and a restored terminal (the first gate
  run diverged on `top.raw`: 1643 vs 1993 history rows). The engine keeps a margin of two pages
  over the requested scrollback and `history_len()` exposes exactly the newest `scrollback` rows.
- **`scrolled_total`.** No monotonic counter in the C API; a tracked grid ref anchored on the top
  active row of the primary screen measures how many lines passed it. Writes are chunked to less
  than the retained history so the anchor cannot be pruned.
- **Identification** uses the engine callbacks filled from `vk_proto::ident`, so DA1/DA2/DA3/
  XTVERSION match the holder byte for byte (unit test `identification_matches_holder`).

## Gate results (M1 Pro, 2026-10-06)

`cargo test --release -p vk-term` — all pass:

| Test | Result |
|---|---|
| C4: cut every 4,093rd byte → snapshot → restore → continue, vs uninterrupted run | pass on all of `tests/vt-corpus`: emoji (217 B, 1 cut), git-log (2,237 B, 1), ls (73,943 B, 19 cuts, 3 mid-sequence), top (613,885 B, 150), vim (11,905 B, 3), synthetic (31,177 B, 8 cuts, 5 mid-sequence) |
| C4: cut at every byte of the first 3 KiB of the synthetic stream (3,071 cuts) | pass |
| Replay suppresses side effects (bell, OSC 9, OSC 52, DA, DSR) but keeps TitleChanged/Cwd | pass |
| Chunked UTF-8 split equals unbroken | pass |

The compared state is cursor, modes, a mode bitset, title, input modes, sync flag,
modifyOtherKeys level, cwd, `scrolled_total`, every history row and every visible row (styles,
graphemes, wrap flags).

## Performance

`cargo test --release -p vk-term -- --ignored --nocapture` (120×40, 10,000 rows scrollback,
200 MB of the corpus in 64 KiB chunks, `take_damage` after each chunk):

| | libghostty-vt binding | alacritty binding (M0) |
|---|---|---|
| Throughput | **332–371 MB/s** (3 runs) | 103 MB/s |
| Snapshot size, full 10k scrollback | **1.3 MiB** (1,308 KiB) | 9.5 MiB |
| Snapshot time | **0.9–2.2 ms** | 29 ms |
| Restore time | 2.1–6.8 ms | — |

Throughput meets the 300 MB/s target (spec/10 §1). `esctest2`/`vttest` baselines are not recorded
yet (spec/03 §2.3 lists them; no harness exists in-tree).

## Gaps we own (spec/03 §2.4)

- **Kitty image payloads are not in snapshots**; placeholder cells survive. Re-transmitting
  images from `PaneScreen`'s `ImageTable` on restore is still to be built; until then images are
  lost across a server restart until the app redraws.
- **Sixel and iTerm2 `OSC 1337 File=`** are not decoded by libghostty-vt.
- **Width policy is the engine's** (build time); the shared width function (spec/03 §10.3) must
  match it — `tests/unicode/width.txt` against both is not written yet.
- **Unstable C API**: expect breakage when moving the pin; `ghostty_sys.rs` is the only FFI code
  and `ffi_layout` catches layout/enum drift.
- **No local patches.** Upstream candidates instead of tracker items: a C API query for the
  modifyOtherKeys level, and routing OSC 99 to the desktop-notification effect.

## Behaviour differences from the alacritty binding

- `history_len()` is 0 while the alternate screen is active (the C API only reads the active
  screen's history); the old binding reported the primary screen's size.
- `scrolled_total` is exact while running and across restores, except one case: a snapshot taken
  on the alternate screen, restored, and then, in the same write in which the app leaves the
  alternate screen, enough primary output to make Ghostty prune a page. It then undercounts by
  the lines in that write.
- OSC 10/11/12/4 queries are answered by the engine from the `Palette` defaults *plus* any colour
  the app set itself (OSC 4/10/11/12); the old binding always answered the defaults.
- `NotifyKind` for OSC 9 vs OSC 777 is inferred from the title (the engine's callback merges
  them): an OSC 777 with an empty title is reported as `Osc9`.
- `Effect::Mark` covers the OSC 133 kinds the engine reports (A/B/C/D); other letters are no
  longer passed through.
- Styles now carry `attr::BLINK`; DECSET 9 maps to `MouseMode::X10`.
- In-band size reports (mode 2048) raised by `resize()` are dropped (no output channel), as
  before.
- `term_mode()` returns a Vibeke-defined mode bitset (documented in `engine.rs`), no longer
  alacritty's `TermMode` bits. `NoSync` (alacritty-specific) is gone.
- `snapshot()` returns an empty vector if the engine refuses to encode (only when one unfinished
  sequence exceeds 65 MiB); `restore` rejects it and recovery falls back to ring-only replay.
