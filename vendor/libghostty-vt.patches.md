# libghostty-vt local patches

Every intentional change to the vendored source in `vendor/libghostty-vt/` is listed here with
its reason, upstream PR, removal condition and verification (spec/03 §2.2). The vendored base is
recorded in `vendor/libghostty-vt.vendor.json`.

## Active patches

None. `vendor/libghostty-vt/` is a byte-for-byte subset of upstream commit
`35a81a980bb9fce09a1ea762a68b55f8eb3477ed`.

## Gaps handled outside the engine instead of patching

These are covered in `crates/vk-term/src/tracker.rs` (a byte scanner beside the engine) rather
than by patching Ghostty. Each is a candidate for an upstream PR; when upstream exposes it, drop
the tracker item and re-run the C4 gate (`cargo test --release -p vk-term`).

- **modifyOtherKeys level.** Ghostty records only `modify_other_keys_2` (a bool, set by
  `CSI > 4 ; 2 m`) and does not expose it through the C API. Vibeke's encoder distinguishes
  levels 0, 1 and 2, so the tracker reads `CSI > 4 ; Pv m`. Upstream candidate: a
  `GHOSTTY_TERMINAL_DATA_*` query for the modifyOtherKeys level.
- **OSC 99 (kitty desktop notifications).** Ghostty parses OSC 99 but drops it
  (`stream.zig`: "unimplemented OSC callback"), and recognised OSC numbers are never passed to
  `GHOSTTY_TERMINAL_OPT_UNKNOWN_SEQUENCE`. Upstream candidate: route OSC 99 to the desktop
  notification effect.

## Updating the pin

1. Shallow-clone the new upstream commit and build `zig build -Demit-lib-vt` once in the full
   tree; collect the files listed in `.zig-cache/h/*.txt` manifests (prefix 0, paths under the
   tree, excluding `zig-pkg/`) plus `include/`, `src/build/`, `LICENSE` and every
   `pkg/*/build.zig.zon`.
2. Replace `vendor/libghostty-vt/` with that subset, update `libghostty-vt.vendor.json`, and
   re-apply the patches above (none today).
3. Cross-check the build for every target in `crates/vk-term/build.rs`.
4. Regenerate/verify `crates/vk-term/src/ghostty_sys.rs` against the headers
   (`cargo test -p vk-term ffi_layout` checks struct layouts against `ghostty_type_json()`).
5. Run the C4 gate and record throughput in `.adr/0001-vt-engine.md`.
