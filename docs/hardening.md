# Fuzz tests, performance limits, and reproducible builds

This page describes the checks from spec 11 M6 and spec 10, sections 1 and 6.
The bounded fuzz tests in `crates/vk-fuzz` run with the normal test suite.
The other checks are separate from `mise run ci`.

## Fuzz tests

Each target is a function with the type `fn(&[u8])` in `crates/vk-fuzz/src/targets.rs`.
The functions must not panic, stop responding, or allocate memory without a limit.
Two test systems use these functions.

| Test system | Location | Toolchain | Runs in CI |
|---|---|---|---|
| Deterministic tests with random bytes and modified seeds | `crates/vk-fuzz/tests/random.rs` | Stable Rust | Yes. Default: 300 cases per target. The `vt_parse` and `mux_frame_decode` targets run 30 cases each. |
| cargo-fuzz with libFuzzer | `fuzz/` | Nightly Rust and cargo-fuzz | No |

The stable tests use the generator and mutator in `crates/vk-fuzz/src/rng.rs`.
They do not add `proptest`, `arbitrary`, or `bolero` to `Cargo.lock`.
The `libfuzzer-sys` dependency belongs to the separate workspace in `fuzz/Cargo.toml`.
It does not change the results of `cargo deny check` for the root workspace.

### Targets

| Target | Input | Required result |
|---|---|---|
| `holder_proto_decode` | `FrameBuf` chunks and raw postcard data as `ToHolder` / `FromHolder` | No panic. Reject oversized input. |
| `render_frame_decode` | `FrameBuf` chunks and raw postcard data as `ServerFrame` / `ClientFrame` | No panic |
| `jsonrpc_decode` | Line-based parsing of `rpc::Request` / `Response` / `Notification` | No panic |
| `key_grammar` | `parse_key`, `parse_binding`, `expand_range` | `format_key` followed by `parse_key` returns the original value |
| `vt_parse` | Random bytes and resizes through `vk_term::Engine::feed` | No panic or abort. Complete within the time limit. |
| `socks5_handshake` | Bytes passed to `vk-preview` `socks::handshake` | No panic |
| `mux_frame_decode` | Bytes passed to the `vk-remote` frame decoder and a live `Mux` | No panic or blocked response |
| `hook_payloads` | Arbitrary JSON passed to `interaction_from_hook` for each harness and event | No panic |
| `policy_match` | URLs, allow rules, and IP addresses passed to `vk-browser::policy` | No panic. Block metadata and link-local addresses unless a rule permits them. |
| `kitty_probe` | Terminal replies and SGR mouse reports passed to `vk-browser::probe` | No panic. Consumed length stays within bounds. |
| `compat_import` | Herdr config TOML and session JSON | No panic |
| `manifest_toml` | Harness manifests passed to `parse_raw`, `Loaded::new`, its methods, and `VersionReq` | No panic |

Seed files in `fuzz/corpus/<target>/` include valid messages, test fixtures, and boundary cases.
To regenerate them, run:

```sh
cargo test -p vk-fuzz --test random export_seed_corpus -- --ignored
```

### Run the tests

For a bounded run with stable Rust, use:

```sh
mise run fuzz-smoke
```

For more cases with arithmetic overflow checks, use a debug build:

```sh
VK_FUZZ_CASES=100000 VK_FUZZ_SEED=3 cargo test -p vk-fuzz --test random
```

For a libFuzzer run, install nightly Rust and cargo-fuzz first.
Then run:

```sh
FUZZ_TIME=300 mise run fuzz
```

To test one target, set `FUZZ_TARGET`:

```sh
FUZZ_TARGET=policy_match mise run fuzz
```

If nightly Rust or cargo-fuzz is missing, `mise run fuzz` prints installation instructions.
It then exits with status 0 without running the tests.

A failed test prints the exact input.
Add that input to a regression test beside the affected code.
Also add it to the seed files for that target.

### Fixed defects

The policy tests found an IPv4-mapped IPv6 prefix error.
A rule such as `::ffff:10.0.0.7/128` became an IPv4 address but kept its IPv6 prefix length.
The expression `u32::MAX << (32 - bits)` then overflowed.
This caused a panic in debug builds and an incorrect mask in release builds.

The prefix now starts at bit 96.
A mapped rule with a prefix below 96 matches no addresses.
The regression test is `mapped_v6_rule_prefix_is_relative_to_bit_96`.

### Planned OSS-Fuzz integration

The target functions and cargo-fuzz directory already support separate builds.
The remaining work is:

1. Add input dictionaries where necessary. Examples include terminal escape sequences, JSON-RPC method names, and key names.
2. Submit `projects/vibeke/` to `google/oss-fuzz` with the files below.
3. Build the native VT engine with the OSS-Fuzz `$CC` and `$CFLAGS` variables. This lets ASan check the C code.
4. Set `-rss_limit_mb` to 256 for `vt_parse`.
5. Add a regression test and a seed for each reported defect.
6. Run nightly tests in project CI for one CPU-hour per target.
7. Before version 1.0, run all targets under OSS-Fuzz for seven days without a new crash.

| File | Contents |
|---|---|
| `project.yaml` | Rust language, repository, contact, libFuzzer engine, address sanitizer, and x86_64 architecture. Add the undefined behavior sanitizer after the native VT engine passes its checks. |
| `Dockerfile` | Base image `gcr.io/oss-fuzz-base/base-builder-rust`, Zig 0.16, and the repository. Zig builds the vendored libghostty-vt library. |
| `build.sh` | Build with `cargo fuzz build --fuzz-dir fuzz -O`. Copy binaries to `$OUT`. Create `$OUT/<target>_seed_corpus.zip` from each seed directory. |

Some planned targets still need test support:

- `vt_resize_interleave` and `serialize` need an API to compare snapshots.
- `compat_socket` needs a socket test driver.
- `transcript_parse` needs a target.
- `osc_image` needs tests for decoder limits.

## Performance limits

`mise run perf-budgets` runs `scripts/perf-budgets.sh`.
It compares measurements with the limits in spec 10, section 1.
Results go to `target/perf-budgets/`.
By default, the command reports results and exits with status 0.
Set `PERF_STRICT=1` to fail the command when a checked limit is exceeded.

| Measurement | Command or source | Limit |
|---|---|---|
| VT parser throughput | `cargo test --release -p vk-term --test recovery throughput -- --ignored` | At least 300 MB/s |
| Added keystroke latency | `vibeke debug latency` | p50 at most 1 ms. p99 at most 3 ms. |
| Remote bandwidth | `vibeke debug bandwidth`, with `PERF_MACHINE=<label>` | Idle: 0 B/s. Unfocused spinner: at most 2 KiB/s. Focused spinner: at most 8 KiB/s. Results require manual review. |
| Idle CPU, memory, and wakeups | `vibeke debug idle`. Set `PERF_IDLE=0` to skip this check. | Server: at most 0.3% CPU, 2 wakeups/s, and 25 MiB. Holder: at most 2 MiB. TUI: at most 30 MiB. |
| VT conformance | `cargo test -p vk-term --test conformance`, also in `mise run test` | No unexpected failures. Set `VK_CONFORMANCE_REPORT=1` to print category counts and expected failures. |
| Browser frames | `cargo run -p vk-browser --example bench`, with `PERF_BROWSER=1` | Manual review against spec 10, section 1.6 |

The idle test uses a separate server with 30 idle panes and an attached TUI without a display.
It records the system load.
On a busy host, it marks results with “(loaded)”.

These limits apply to the two reference machines in spec 10, section 2.1.
Results from other hardware give an estimate.
CI does not yet reject a pull request when these measurements exceed a limit.
That check needs a store of past results.
The planned check also rejects regressions above 10% against the median of the last five `main` runs.

## Reproducible builds

`mise run repro-check` runs `scripts/repro-check.sh`.
The script builds the Linux musl release binary twice in separate target directories.
It uses `cargo zigbuild --release --locked` and compares the SHA-256 hashes.
Set `REPRO_TARGETS` to select targets.
The default is `x86_64-unknown-linux-musl`.

Both builds use these settings:

- `SOURCE_DATE_EPOCH` is the HEAD commit time unless explicitly set. Releases use the tag commit time.
- `--remap-path-prefix` covers the workspace, `CARGO_HOME`, `RUSTUP_HOME`, and target directory.
- Compiler settings include `-C strip=symbols` and `CARGO_INCREMENTAL=0`.
- Environment settings include `TZ=UTC` and `LC_ALL=C`.

### Recorded result

On October 6, 2026, two clean builds on one macOS arm64 host produced the same binary.
The source was commit `858036a` with the M6 documentation changes.
The tools were Rust 1.99.0 and Zig 0.16.0.
The target was `x86_64-unknown-linux-musl`.
Each cold build took approximately 14 minutes.
Both builds produced this SHA-256 hash:

```text
40e33616587d25f66ba32e9665da2eebec01bd3d9ce7f008aa8f9017d91257a2
```

This result covers two builds on one machine.
It does not verify `aarch64-unknown-linux-musl`, separate machines, different checkout paths, or different host operating systems.
It also does not use a fixed container image.

The check does not calculate a separate hash for the vendored libghostty-vt Zig cache.
The final binary comparison includes the output of the Zig build.

`dist.sh` does not yet use these build settings.
Thus, the result does not cover artifacts from `mise run dist`.
CI does not run this check yet.
Separate CI runners and published release digests remain planned work.
