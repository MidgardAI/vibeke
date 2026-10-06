# Hardening: fuzzing, performance budgets, reproducible builds

M6 groundwork (spec 11 M6, spec 10 §1, §6). Nothing here changes what `mise run ci` does on
stable, except that the bounded fuzz property tests (`crates/vk-fuzz`) run with the normal test
suite.

## Fuzzing

All fuzz targets are plain functions in `crates/vk-fuzz/src/targets.rs`, `fn(&[u8])`, that must
never panic, hang or allocate without bound. Two front ends share them:

| Front end | Where | Toolchain | Runs in CI |
|---|---|---|---|
| Property tests: random bytes plus mutated seeds, deterministic | `crates/vk-fuzz/tests/random.rs` | stable | yes (300 cases per target by default, a tenth for `vt_parse` and `mux_frame_decode`) |
| cargo-fuzz / libFuzzer binaries | `fuzz/` (own `[workspace]`, not a root member) | nightly + `cargo-fuzz` | no |

No fuzzing crates were added to the workspace (`proptest`, `arbitrary` and `bolero` are not in
`Cargo.lock`); the property tests use a 40-line splitmix64 generator and mutator in
`crates/vk-fuzz/src/rng.rs`, so `cargo deny check` is unaffected. The only new dependency is
`libfuzzer-sys`, and it lives in `fuzz/Cargo.toml`, outside the workspace.

### Targets

| Target | Input | Oracle |
|---|---|---|
| `holder_proto_decode` | `FrameBuf` chunked stream and raw postcard as `ToHolder` / `FromHolder` | no panic; oversize rejected |
| `render_frame_decode` | same, as `ServerFrame` / `ClientFrame` | no panic |
| `jsonrpc_decode` | `rpc::Request` / `Response` / `Notification`, per-line parsing | no panic |
| `key_grammar` | `parse_key`, `parse_binding`, `expand_range` | `format_key` then `parse_key` round-trips |
| `vt_parse` | random bytes plus resizes into `vk_term::Engine::feed` | no panic or abort, bounded time |
| `socks5_handshake` | bytes into `vk-preview` `socks::handshake` | no panic |
| `mux_frame_decode` | `vk-remote` mux `Frame` decoding, and a live `Mux` fed the bytes | no panic or hang |
| `hook_payloads` | arbitrary JSON into `interaction_from_hook` for every harness and event | no panic |
| `policy_match` | URLs, allow rules and IPs into `vk-browser::policy` | no panic; metadata and link-local never allowed without a rule |
| `kitty_probe` | terminal replies and SGR mouse reports into `vk-browser::probe` | no panic; consumed length in bounds |
| `compat_import` | Herdr config TOML and session JSON importers | no panic |
| `manifest_toml` | harness manifests: `parse_raw`, `Loaded::new` and its methods, `VersionReq` | no panic |

Seed corpora are in `fuzz/corpus/<target>/` (valid messages, fixtures and edge cases). Regenerate
with `cargo test -p vk-fuzz --test random export_seed_corpus -- --ignored`.

### Running

```sh
mise run fuzz-smoke                       # stable, release build, bounded
VK_FUZZ_CASES=100000 VK_FUZZ_SEED=3 cargo test -p vk-fuzz --test random   # deeper; debug build also catches arithmetic overflow
FUZZ_TIME=300 mise run fuzz               # libFuzzer, needs nightly + cargo-fuzz
FUZZ_TARGET=policy_match mise run fuzz    # one target
```

`mise run fuzz` prints a hint and exits 0 if nightly or cargo-fuzz is missing. Run the stable
property tests in a debug build for long runs: overflow checks turn silent wraparound into
panics (this is how the policy bug below was found). A failing property case prints the exact
input; add it as a regression unit test next to the code and as a seed.

### Found so far

* `vk-browser::policy`: a v4-mapped IPv6 allow rule such as `::ffff:10.0.0.7/128` canonicalised
  to IPv4 but kept its IPv6 prefix length, so `u32::MAX << (32 - bits)` overflowed (panic in
  debug, wrong mask in release). The prefix is now counted from bit 96, and a mapped rule with
  a prefix under 96 matches nothing. Regression test: `mapped_v6_rule_prefix_is_relative_to_bit_96`.

### OSS-Fuzz integration plan (M6)

1. Make the targets self-contained for OSS-Fuzz: they already are (`vk-fuzz` is a path crate and
   `fuzz/` is a standard cargo-fuzz layout). Add a `dictionary` per target where useful (escape
   introducers for `vt_parse` and `kitty_probe`, JSON-RPC method names, key names).
2. Open a PR to `google/oss-fuzz` with `projects/vibeke/`:
   * `project.yaml`: `language: rust`, `main_repo`, primary contact, `fuzzing_engines: [libfuzzer]`,
     `sanitizers: [address]` (add `undefined` once the native VT engine is clean), `architectures: [x86_64]`.
   * `Dockerfile`: `FROM gcr.io/oss-fuzz-base/base-builder-rust`, install `zig 0.16` (needed by
     `vk-term`'s vendored libghostty-vt build, spec 03 §2.2), copy the repo.
   * `build.sh`: `cargo fuzz build --fuzz-dir fuzz -O`, copy the binaries to `$OUT`, and zip each
     `fuzz/corpus/<target>` to `$OUT/<target>_seed_corpus.zip`.
3. Native code: `vt_parse` exercises libghostty-vt through FFI. Build it with the OSS-Fuzz
   `$CC`/`$CFLAGS` so ASan covers the C side, and keep `-rss_limit_mb` at the spec's 256 MiB
   OOM oracle for that target.
4. Triage: findings arrive as private OSS-Fuzz issues; each fix lands with a regression case in
   the crate's own tests plus a new seed. Keep the nightly (1 CPU-hour per target) in our own
   CI as well, as spec 10 §6 requires.
5. Gate for 1.0 (spec 10 §8): seven days of fuzzing with no new crashes, all targets running
   under OSS-Fuzz.

Not yet covered (spec 10 §6 targets that need harness work): `vt_resize_interleave` reflow
invariants and `serialize` round-trip equality (need a snapshot comparison API), `compat_socket`
(needs a socket-level driver), `transcript_parse`, `osc_image` decoder limits.

## Performance budgets

`mise run perf-budgets` (`scripts/perf-budgets.sh`) runs the existing measurements and prints a
table against the spec 10 §1 budgets. It is report-only: exit status is 0 unless
`PERF_STRICT=1`. Raw output goes to `target/perf-budgets/`.

| Measurement | Source | Budget |
|---|---|---|
| VT parse throughput | `cargo test --release -p vk-term --test recovery throughput -- --ignored` | >= 300 MB/s (1.2) |
| Added keystroke latency | `vibeke debug latency` | p50 <= 1 ms, p99 <= 3 ms (1.1) |
| Remote bandwidth (`PERF_MACHINE=<label>`) | `vibeke debug bandwidth` | idle 0 B/s, spinner <= 2 KiB/s unfocused, <= 8 KiB/s focused (1.5); printed for review |
| Idle CPU / RSS / wakeups (`PERF_IDLE=0` skips) | `vibeke debug idle` (isolated server, 30 idle panes, headless attached TUI; load average recorded, verdicts marked "(loaded)" on a busy host) | server <= 0.3% CPU, <= 2 wakeups/s, holder <= 2 MiB, server <= 25 MiB, TUI <= 30 MiB (1.3) |
| VT conformance (`cargo test -p vk-term --test conformance`, also in `mise run test`) | in-repo corpus, `VK_CONFORMANCE_REPORT=1` prints per-category counts and expected failures | 0 unexpected failures (4.1) |
| Browser frame path (`PERF_BROWSER=1`) | `cargo run -p vk-browser --example bench` | 1.6; printed for review |

Budgets are defined on the two reference machines (spec 10 §2.1); numbers from other hardware
are indicative. Turning this into the PR gate (fail on a budget breach or a >10% regression
against the median of the last five `main` runs) needs the benchmark-history store from spec 10
§2 and is not part of this groundwork.

## Reproducible builds (status and plan)

Release builds already use `--locked` and pinned toolchains (`mise.toml`, `scripts/dist.sh`).
Remaining for the Linux release set: build in a fixed container image, set `SOURCE_DATE_EPOCH`
from the tag commit, pass `--remap-path-prefix` for the workspace and `CARGO_HOME`, vendor or
checksum the Zig cache for libghostty-vt, and verify by building twice on separate runners and
comparing `sha256sum` of `dist/`. Publish the digests with the release notes.
