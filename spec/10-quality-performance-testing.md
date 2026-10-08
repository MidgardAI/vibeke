# 10 — Quality, performance and testing

Terminal multiplexers for agents tend to suffer from three classes of problems: keyboard/terminal fidelity (Kitty protocol, AltGr, Shift+Enter, undercurl, emoji width), performance with many agents (CPU/fans on macOS, 3.6 GB/h remote bandwidth per animating pane, 570 ms tab close, Windows freezes), and detection fragility. This section makes each of those a **measured budget with a CI gate**, not a hope.

Milestones (plan in [11](11-milestones.md)): **M0** spikes, **M1** supervision slice, **M2** safe yolo + more harnesses, **M3** remote + preview, **M4** VMs + polish/parity, **M5** compatibility + plugins, **M6** hardening / Windows / 1.0; **post-1.0** deferred (QUIC, marketplace, …).

---

## 1. Performance budgets

All budgets are measured on the two reference machines (§2.1) unless noted. "Added" latency = latency with Vibeke minus latency of the same program running directly in the host terminal.

### 1.1 Interactive latency

| Metric | Budget | Gate from |
|---|---|---|
| Keystroke → screen, **added**, local, echo in a shell (p50 / p99) | ≤ 1 ms / **≤ 3 ms** | M1 |
| Keystroke → screen, added, local, while 30 other panes stream output at 1 MB/s total | p99 ≤ 5 ms | M1 |
| Keystroke → screen over remote link, added on top of network RTT (p99), SSH transport | ≤ 8 ms | M3 |
| Remote with **predictive local echo** (QUIC, post-1.0), perceived echo latency for printable chars at 150 ms RTT | ≤ 16 ms for ≥ 90% of keystrokes in a shell / editor | post-1.0 |
| Pane focus switch, tab switch, workspace switch (input → fully drawn) | ≤ 16 ms p99 | M1 |
| Tab close with 4 panes | ≤ 50 ms | M1 |
| Split pane → new shell prompt visible | ≤ 150 ms (dominated by shell startup; Vibeke's share ≤ 20 ms) | M1 |
| `vibeke` CLI round-trip for `pane.list` (process start → exit) | ≤ 15 ms | M1 |
| Render frame rate on focused pane under full-screen output | sustains host refresh (≥ 120 Hz on ProMotion) without dropping input | M1 |

### 1.2 Throughput

| Metric | Budget | Gate |
|---|---|---|
| `cat` of a 1 GiB file in one pane: time vs. host terminal directly | ≤ 1.3× | M1 |
| Sustained output parse throughput per pane (VT engine) | ≥ 300 MB/s on reference M-series | M0 spike criterion |
| Holder ring write overhead | ≤ 2% CPU at 100 MB/s | M1 |

### 1.3 Resource usage

| Metric | Budget | Gate |
|---|---|---|
| Server idle CPU, 30 panes (20 idle agents at prompt, 10 shells), no client attached | ≤ 0.3% of one core | M1 |
| Server + TUI CPU, 30 panes, 5 agents showing spinners, TUI attached and visible | ≤ 3% of one core | M1 |
| Same, TUI attached but host terminal occluded/unfocused | ≤ 1% (frame pacing drops to background fps) | M1 |
| Holder idle RSS | ≤ 2 MiB + ring (ring is lazily allocated, grows to cap) | M1 |
| Server RSS per pane (10k lines scrollback, typical TUI agent) | ≤ 6 MiB | M1 |
| Server RSS baseline (no panes) | ≤ 25 MiB | M1 |
| TUI client RSS | ≤ 30 MiB | M1 |
| Wakeups: idle server | ≤ 2/s total (no polling loops; timers coalesced) | M1 |
| Disk write rate, idle session | ≤ 10 KiB/s (snapshots only on change) | M1 |
| `state.db` growth, 10 agents working 8h | ≤ 200 MiB (events + items; scrollback archive separate) | M1 |

#### 1.3.1 Measured baseline (2026-10-06, first measurement, after the server idle-wakeup fix and after the TUI deadline loop)

Tool: `vibeke debug idle --panes 30 --seconds N --repeat R` (also a row group in `mise run perf-budgets`). It starts an isolated server (private runtime/state dirs), 30 idle panes (20 `cat` as agents waiting for input, 10 `sh`), samples with `proc_pid_rusage` (macOS: CPU, `ri_resident_size`, `ri_interrupt_wkups`, `ri_pkg_idle_wkups`; Linux: `/proc` stat/status, voluntary context switches as the wakeup proxy, untested), first with no client and then with a headless TUI (`vibeke attach` in a PTY, answering terminal queries) attached. Medians of 3 windows of 20 s, release build, Apple M1 Pro (10 cores), macOS 26. **The host was heavily loaded by other builds for every run (load1 14 to 82 on 10 cores), so none of these numbers is a valid gate result;** the tool marks verdicts "(loaded)" when load1 > cores/2. CPU and wakeup counts are the process's own, so contention inflates them less than latency, but treat budget compliance as unconfirmed until re-measured on a quiet reference machine.

| Metric | Budget | Measured (4 runs) | Verdict |
|---|---|---|---|
| Server idle CPU, 30 panes, no client | <= 0.3% | 0.07 to 0.13% | within budget (loaded host) |
| Server + TUI CPU, 30 idle panes, TUI attached | <= 1% / 3% | 0.12 to 0.17% (server ~0.1, TUI 0.03 to 0.06) | within budget (loaded host); spinner scenarios not yet exercised |
| Holder idle RSS | <= 2 MiB + ring | 1.1 to 1.7 MiB mean | within budget |
| Server RSS, no panes | <= 25 MiB | 21.7 to 22.3 MiB | within budget |
| Server RSS per idle pane | <= 6 MiB (10k lines) | 0.00 to 0.05 MiB (idle, no scrollback yet) | not meaningful until panes hold scrollback |
| TUI client RSS | <= 30 MiB | 8.4 to 9.9 MiB | within budget |
| Server idle wakeups | <= 2/s | first measurement **11 to 13/s** (pkg-idle 0/s); after the fix below **0.9 to 1.3/s** (median of 3 x 20 s; per window 1.3 / 1.05 / 0.45, the first windows still inside discovery's 30 s fast period after the panes were created), 0.3 to 0.5/s with a TUI attached | **within budget** after the fix (loaded host); wakeup counts are not load-dependent and were stable across runs |
| Server + TUI CPU after the fix | <= 1% | server 0.05 to 0.09% no client, 0.01 to 0.02% attached (was ~0.1); TUI 0.02 to 0.04% | within budget (loaded host) |
| TUI idle wakeups | informational | 7.7 to 8.1/s, all of it the TUI's 250 ms tick (`vk-tui/src/app.rs`; with the tick disabled: 0.0/s); after the deadline-driven loop below **0.0/s** (10 panes, 3 x 20 s: before 8.0/s at load1 14.5 to 6.7, after 0.0/s at load1 39.4 to 7.8; TUI CPU 0.04% to 0.00%) | **addressed** (loaded host; wakeup counts are not load-dependent) |
| Holder CPU / wakeups, 30 holders | informational | 0.00% / 0.0 per s | blocked in poll |

Cause of the wakeup miss, profiled (2026-10-06) by disabling one periodic task at a time under `vibeke debug idle` (no client, 30 panes; load1 5 to 35 during the runs): the review live-state watcher, a `std::thread` polling the model revision every 50 ms (`review.rs`, started at server start), **~8.7/s**; the 1 s housekeeping tick (`run.rs`) **~2.3/s**; the 2 s preview discovery tick **~1.4/s** and the 2 s sandbox tick **~1.1/s** (these two fired on the same timer-wheel slot, together ~1.4/s); the desk indexer (15 s) ~0.1/s; screenshot retention (hourly) ~0. The per-pane 500 ms interval contributed **0**: it only ran while a pane had output not yet covered by a snapshot. With all of them disabled the server measured 0.0/s.

Fix (spec 10 §1.3 "no polling loops; timers coalesced"): the review watcher blocks on the model-revision watch channel (the 50 ms nap remains only to coalesce bursts); housekeeping (archive/FTS flush, storage probe while degraded) runs only when archive rows or a storage failure wake it, still batched at most once a second, plus the hourly prune; the sandbox tick stops when no execution context or broker is left and restarts with the next one; preview discovery stays on its 2 s period while previews are live or within 30 s of pane output / foreground change / pane create or close, then backs off (4, 8, 16, 30 s), and the next pane output wakes a backed-off loop at once (fg-change events still rescan immediately); per-pane snapshot intervals were replaced by one server-wide deadline scheduler (`vk-server/src/timers.rs`) where a pane arms a deadline only while output is pending (once per batch, re-armed only if output continued), deadlines rounded up to a 250 ms grid so panes share wakeups, and a fully idle pane has nothing armed. `server.status` reports the counters (`timers`: snapshot arms/fires/pending, scheduler wakeups, housekeeping runs, discovery passes and period, sandbox tick); `crates/vibeke/tests/idle_timers.rs` asserts idle panes arm nothing and that discovery finds a listener within 5 s of its output while backed off. Residual idle wakeups: discovery's 30 s fallback poll, the desk indexer, the tokio blocking pool. The TUI's 250 ms tick (toasts, prefix timeout, inbox/tasks/gateway/parity/assist ticks, sidebar ages) was the remaining client-side cost; it is now deadline-driven (below).

TUI fix (2026-10-06): the fixed 250 ms `tokio::time::interval` in the TUI run loop is gone. `App::deadlines()` (`vk-tui/src/deadline.rs`, one `deadlines()` per feature next to its tick) computes what actually needs time from the current state and the loop sleeps until the earliest one, or until input / a server frame when nothing is armed; the housekeeping (`App::on_tick`) runs after every wakeup, so whatever an event made due is handled on that wakeup and deadlines only cover what time alone changes. Armed only while relevant: a toast's expiry; the prefix-key timeout; age labels (a *working* agent's `working · 12s`, repainted when the shown second/minute/hour changes, and the inbox / desk / gallery / pending-operations ages once a second while one of those is open; idle, done and waiting agents show static labels and arm nothing, where the old tick repainted at 4 Hz whenever any agent run existed); the confirm overlay's countdown (when the shown second changes) and expiry; per connected machine the `events.read` poll (1 s, only for servers without event push or before the catch-up read) and `client.list` (15 s without push; with push the list follows pushed `client.*` events and its 120 s safety net rides on other wakeups instead of a timer), plus the re-send after a lost request (10 s); the inbox refresh (open and stale only); the task view's message poll (open with messages still sending); the assist `assistant.get` poll (while waiting); the status bar (enabled only: the clock's minute if a `clock` segment is shown, `status.segments` 1 s after a model change or every 10 s); the preview-mirror poll (10 s, only with mirrors or live remote previews); the iTerm2 inline-image repaint the rate limit held back; the coalesced copy-mode `ScrollView` report and the throttled refocus of an open plugin popup. Media acks and pending-operation reconciliation are event-driven (written after a draw / on reconnect) and need no timer; copy-mode cursor blink is the host terminal's. Wakeups land 1 ms after a deadline (checks written as `elapsed() > limit`) and never sooner than 5 ms after the previous one, so a deadline the housekeeping could not clear cannot spin. Unit tests (`deadline.rs`) assert an idle App (push-capable server, caught up) arms no deadline and that each feature arms its own deadline at its own interval and stops when it no longer applies.

Not measured: disk write rate, `state.db` growth, spinner CPU scenarios, 10k-line scrollback RSS, Linux numbers, `powermetrics` energy. Reproduce: `mise run perf-budgets` (`PERF_IDLE_SECONDS`, `PERF_IDLE_REPEAT`, `PERF_IDLE=0` to skip).

### 1.4 Startup and recovery

| Metric | Budget | Gate |
|---|---|---|
| Cold start `vibeke` → TUI drawn, no server running, restoring 30-pane layout (shells) | ≤ 300 ms to first frame, ≤ 1.5 s until all shells at prompt | M1 |
| Attach to running server → first full frame | ≤ 50 ms local, ≤ 1 RTT + 100 ms remote | M1/M3 |
| **Server restart (kill -9 → new server) with 30 live panes**: time until every pane is reattached, replayed and (for TUI apps) repainted after the resize nudge | ≤ 1 s | M1 |
| Inputs applied twice across a server restart | **0** (hard gate) | M1 |
| Processes lost across server restart / upgrade | **0** (hard gate) | M1 |
| Server upgrade (`vibeke update`) → back to interactive | ≤ 2 s | M1 |
| Reboot → layout restored + resumable agents offered | ≤ 3 s after login (excluding agent startup) | M1 |

> **As built.** `crates/vibeke/tests/timing.rs` times four of these against a real server through the CLI: cold start to the first answered call (≤ 300 ms), `render.attach` to its reply (median of five, ≤ 50 ms), `kill -9` restart with 30 panes until the full pane list returns (≤ 1 s), and a simulated reboot (server and all 30 holders killed) until every slot has a fresh live process (≤ 3 s). The bounds are generous by default (5 s / 2 s / 30 s / 60 s) so a shared runner or a debug build never flakes; `VIBEKE_PERF_STRICT=1` switches to the budgets above. Each run prints its measurement and, with `VIBEKE_TIMING_REPORT=<file>`, appends `name<TAB>ms` for the perf gate (§2.2). Not timed yet: first frame of the TUI, "all shells at prompt", repaint after the nudge, `vibeke update`.

### 1.5 Remote bandwidth

| Metric | Budget | Gate |
|---|---|---|
| Idle remote pane (cursor blink handled client-side) | 0 B/s | M3 |
| Agent spinner animating in an **unfocused** remote pane | ≤ 2 KiB/s | M3 |
| Agent spinner in focused remote pane | ≤ 8 KiB/s | M3 |
| Typing in a shell | ≤ 2× the bytes of a plain SSH session | M3 |
| Full-screen redraw (e.g. `htop` at 1 Hz, 200×60) | ≤ 15 KiB/s | M3 |
| Reconnect after network change (QUIC) | resume ≤ 1 RTT, no full redraw if state survives | post-1.0 |

### 1.6 Preview fabric

| Metric | Budget | Gate |
|---|---|---|
| Port discovery: dev server starts listening → `preview.discovered` event | ≤ 1 s local, ≤ 2 s remote | M3 |
| Proxy added latency per request (loopback, local) | ≤ 1 ms p99 | M3 |
| HMR websocket round-trip over remote link | RTT + ≤ 5 ms | M3 |
| `browser.screenshot` of a warm preview (browser already running) | ≤ 1.5 s p95 | M3 |
| Cold screenshot (browser launch) | ≤ 4 s p95 | M3 |

### 1.7 Product metrics (the gate that matters)

Performance budgets prove Vibeke is not worse than a multiplexer. These prove it is better for supervising agents. Measured on the dogfood fleet from the local metrics file (§9), compared against a two-week baseline taken with the previous setup before switching.

| Metric | Definition | Gate |
|---|---|---|
| Operator interventions per agent-hour | keystrokes/replies sent to agent panes that were answers to dialogs (not new prompts), per hour of agent `working` time | ≤ 50% of baseline by the end of the first release that includes structured adapters |
| Blocked time | median and p90 time an agent spends in `needs_approval`/`needs_answer` before it is answered | p90 ≤ 50% of baseline |
| Approval delivery failures | `delivery_failed` + `delivery_unknown` / all answered Interactions | ≤ 0.5% native channel; ≤ 5% keystroke fallback |
| Wrong-state rate | detector-disagreement incidents where the shown state was wrong for > 10 s, per agent-hour | ≤ 0.05 |
| Human review minutes per accepted change | time from task `finished` to merge/accept, active-focus time only | tracked from the first release with tasks; gate set after baseline (Phase 2 north star) |

If these don't improve materially over the baseline, the release does not graduate from preview, regardless of performance numbers.

---

## 2. How performance is measured

### 2.1 Reference environments

- **macOS**: Apple M-series laptop (CI: GitHub `macos-14` arm64 runners for regression trend; release gate on a dedicated self-hosted Mac mini to avoid noisy-neighbor variance).
- **Linux**: 4-vCPU x86_64 VM (CI: dedicated self-hosted runner with CPU frequency pinned; `ubuntu-latest` only for functional tests).
- **Windows** (M6): Windows 11 x64 self-hosted.
- Network emulation for remote tests: Linux `tc netem` profiles: `lan` (1 ms, 1 Gbit), `wifi` (20 ms ±5, 1% loss), `mobile` (150 ms ±40, 2% loss, 5 Mbit), `bad` (300 ms, 5% loss, reorder).

### 2.2 Instruments

- **`vk-bench` harness** (crate in `tests/bench`): spawns a server + N holders in a temp runtime dir, attaches a **headless TUI client** whose output terminal is a virtual VT (the same engine), and measures:
  - *Added latency*: inject a key via the client's input path at T0; detect the glyph in the client's virtual screen at T1. Baseline: same program attached directly to a PTY whose output feeds the virtual VT. Report p50/p90/p99/max over 10k samples.
  - *Throughput*: time to drain fixed corpora (`seq 1 10000000`, recorded `cargo build` output, recorded Claude Code session asciicast, `htop` capture) through holder → server → client.
  - *CPU/wakeups*: `getrusage` deltas + `perf stat` (Linux) / `powermetrics` (macOS release gate) over 10-minute windows with scripted workloads.
  - *Bandwidth*: bytes on the bridge stdio / QUIC socket per scenario.
- **Real-terminal latency** (release gate, not every PR): `typometer`-style camera-free method — a test app in the host terminal (Ghostty, Kitty, iTerm2) measures key→pixel via screen capture APIs on macOS; compared with and without Vibeke.
- **Recovery timing**: chaos harness (§5) timestamps `kill -9` → all panes `recovered`.
- Results are written as JSON and pushed to a benchmark dashboard (static site from `gh-pages`, `github-action-benchmark`). **PR gate**: fail if any budget is exceeded, or if a metric regresses > 10% vs. `main` median of last 5 runs (with a re-run to rule out noise).

> **As built.** `vk-bench` and the dashboard do not exist; the interim gate is `scripts/perf-gate.sh` (workflow `perf.yml`, PRs touching `crates/**` plus nightly). It is built for shared runners, where absolute numbers are meaningless: five samples of the startup/recovery timings (§1.4) and the VT throughput test, each normalized by a machine calibration (a fixed hashing workload and a fixed process-spawn workload) taken in the same sample, reduced to a median, and compared with the ratios in `tests/perf/baseline.json` by `scripts/perf_compare.py`. A metric fails only when the median ratio is worse than the baseline by more than 100% (`PERF_TOLERANCE`), never when the samples disagree by more than 3x (reported as noisy), and a platform with no baseline entry is report-only; every run uploads a `candidate.json` to adopt (`PERF_UPDATE=1 sh scripts/perf-gate.sh` on a quiet machine). The checked-in baseline is darwin-arm64 only; Linux runners report until a Linux candidate is committed. The ">10% vs main" rule and the absolute-budget checks stay with `scripts/perf-budgets.sh` (report-only) until a quiet reference machine exists (§2.1). The gate's own logic is covered by `scripts/tests/perf-compare-test.sh`.

---

## 3. Test strategy overview

| Layer | Tooling | Runs |
|---|---|---|
| Unit | `cargo nextest`, `proptest` for layout math, key grammar, ID mapping, policy matching, redaction | every PR, all OSes |
| Snapshot tests | `insta` for API JSON shapes, CLI pretty output, rendered chrome | every PR |
| VT conformance | esctest2, vttest (automated), own escape-sequence corpus | every PR (Linux), nightly all OSes |
| Keyboard fidelity matrix | `vk-keytest` + real terminals | nightly + release gate |
| API e2e | `tests/e2e` (Rust) + `@vibeke/client` TS e2e | every PR |
| Harness golden tests | recorded sessions per harness version | every PR (replay); weekly live (real CLIs) |
| Chaos / recovery | `vk-chaos` | every PR (short), nightly (long) |
| Soak | 50 agents, 24 h | weekly + release gate |
| Fuzzing | `cargo-fuzz` (libFuzzer), OSS-Fuzz application at M6 | continuous (nightly 1 h per target), corpus in repo |
| Remote | netem profiles, two containers/VMs | every PR (lan), nightly (all profiles) |
| Compat | complete pinned Herdr CLI/socket/manifest differential suite + unchanged plugins + socket-client replay/smoke (07 §8.4) | every PR touching compat/plugins or a covered API; full matrix nightly and release gate from M5 |
| Security | §12 of 09 red-team agent | every PR |
| Perf | §2 | every PR (subset), nightly (full), release gate (real terminals) |

**Toolchain**: CI builds and tests with the latest stable Rust pinned in `mise.toml` (01 §2), bumped within a week of each stable release by an automated PR that must pass the full PR gate; a nightly job also builds with the upcoming beta to catch breakage early. No MSRV older than the pinned stable is tested or supported.

Coverage target: ≥ 80% line coverage on `vk-proto`, `vk-hold`, `vk-store`, `vk-agents` (adapter logic), `vk-compat`; the TUI is covered by snapshot and e2e tests instead.

> **As built.** `proptest` (dev-dependency of `vk-fuzz`, `crates/vk-fuzz/tests/props.rs`, 64 cases per property on a PR, `PROPTEST_CASES` for the nightly) covers layout math (one rect per pane, inside the area, no overlap; non-empty with room), key grammar (parse ⇄ print round trip, modifier order irrelevant, no panics), redaction (idempotent, planted credentials removed, assignments keep their key) and the destination policy (metadata/link-local never allowed without a rule, IPv4-mapped forms included). ID mapping and `insta` snapshots are not adopted. Tight areas can produce empty layout rects (a documented property of `rects`). Workflows added next to `ci.yml`: `nightly.yml` (full chaos, libFuzzer on every target, high-count property runs, VT on every OS, beta toolchain build, soak smoke), `weekly.yml` (long chaos, long fuzz, corpus minimization, multi-hour soak), `perf.yml` (§2.2), `coverage.yml` (`cargo llvm-cov nextest`; `scripts/coverage-check.py` prints the per-crate table and fails below the target only when the repository variable `COVERAGE_ENFORCE` is `true`), `repro.yml` (the real double build of `scripts/repro-check.sh`) and `api-schema.yml` (§8.2). None needs a secret or publishes anything. Not built: remote netem runs, the automated Rust-bump PR, self-hosted and Windows runners.

---

## 4. Test suites in detail

### 4.1 VT conformance [M0 selection, M1 gate]

- **esctest2** (George Nachman's suite, xterm reference) run headless against `vk-term` through a PTY: target ≥ the chosen engine's upstream pass rate; any regression fails CI. Known deviations are listed in `tests/vt/expected-failures.toml` with justification.
- **Status (2026-10-06)**: neither esctest2 nor vttest is vendored (no network in the build), so the M0 "measured esctest pass rate" is **not** available. In its place `crates/vk-term/tests/conformance.rs` is a deterministic in-repo suite (239 cases in 10 categories: cursor movement, erase/edit, scroll regions incl. DECLRMM, SGR, DEC modes, OSC 7/8/9/52/133/777, kitty keyboard flags, DA/DSR/DECRQM/DECRQSS/XTWINOPS replies, wide/combining/grapheme clusters, wrap) written from ECMA-48 and xterm ctlseqs, each also fed byte by byte (split invariance). It runs in the normal `cargo nextest run --workspace` (about 0.1 s). Baseline for libghostty-vt: **237 pass, 2 expected failures** (DECSTR leaves DECCKM set; no reply to DECXCPR `CSI ? 6 n`). Divergences are `.xfail("why")` in the table and must keep failing, so a fix forces the marker to be removed. Behaviour worth knowing: grapheme clustering (ZWJ, VS16, flags, skin tones) only applies after DECSET 2027; by default widths are per code point. OSC 8 links render their text but the render rows do not carry the URI. To run upstream esctest2 later: start the terminal under test as a Vibeke pane and run `python3 esctest/esctest.py --expected-terminal xterm` inside it (it needs a real PTY and the terminal as the program); record failures as `xfail` entries in the same style. Keyboard matrix (tier-1 real terminals) is unchanged: physical, not automatable here.
- **vttest** screens automated by scripted input and screen-diff against golden captures for menus 1–8 and 11 (xterm extensions).
- **Own corpus** (`tests/vt/corpus/`): sequences that real agents/tools emit — synchronized output (DECSET 2026), OSC 8 hyperlinks, OSC 52, OSC 133 prompt marks, OSC 7 cwd, OSC 9/777 notifications, kitty keyboard flags push/pop, kitty graphics (transmit/place/delete, chunked, shared memory refused), sixel, undercurl/colored underline (SGR 4:3, 58), VS-16 emoji and ZWJ sequences, CJK wide chars, combining marks, RTL text, DECSTBM scroll regions, alternate screen with saved cursor, bracketed paste, focus events, mouse modes 1000/1002/1003/1006/1016, title stack. Each case: input bytes → expected cell grid (+ attributes) snapshot.
- **Grapheme width consistency**: property test that server cell widths match the client-side compositor's widths for the Unicode 16 test file, and a per-terminal width probe at TUI startup (query cursor position after printing ambiguous glyphs) to detect host-terminal width disagreement and adapt.
- **Reflow**: resize property tests (random output + random resizes; invariant: logical lines preserved, cursor within bounds, no panic).
- **Snapshot/restore round-trip**: for every corpus case, `serialize → deserialize → render` equals the original grid (required for §5 recovery).

### 4.2 Keyboard fidelity matrix [M1 gate; nightly]

The goal: every key and chord the user presses reaches the program in the pane exactly as it would if the program ran directly in the host terminal.

- **Terminals**: Ghostty, Kitty, WezTerm, iTerm2, Terminal.app, Alacritty, foot (Linux), GNOME Terminal/VTE, Windows Terminal (M6), and **nested**: Vibeke inside tmux, tmux inside Vibeke, Vibeke inside Vibeke (remote attach), Vibeke over SSH from each terminal.
- **Keyboard modes in the pane program**: legacy, `modifyOtherKeys=2`, kitty keyboard protocol flags 1, 1|2, 1|2|4|8|16 (disambiguate, report events, alternates, all keys as escapes, associated text).
- **Key set**: all printable ASCII, Shift+Enter, Ctrl+Enter, Alt+Enter, Ctrl+Shift+letter, Alt+letter, Ctrl+punctuation (`ctrl+/`, `ctrl+.`), function keys F1–F24 with modifiers, navigation keys with modifiers, keypad, AltGr compositions on DE/NO/FR layouts, dead keys, IME composition (Japanese/Chinese input) on macOS, emoji input, `cmd+` keys where the terminal forwards them, key repeat and release events.
- **Harness**: `vk-keytest` is a small program run inside the pane that enables a given keyboard mode and logs the exact bytes it receives. Driver:
  - Linux: real terminals run under Xvfb/Wayland headless (`weston --backend=headless`, `cage`); keys injected with `xdotool`/`wtype`/`ydotool` per layout.
  - macOS (self-hosted runner, logged-in session): keys injected via `CGEventPost` from a signed helper with Accessibility permission; terminals launched via `open -na`.
  - Each test sends the key to (a) `vk-keytest` directly in the host terminal and (b) `vk-keytest` inside Vibeke, and asserts identical received bytes (modulo documented intentional differences, e.g. Vibeke prefix key).
- Output: a matrix report (terminal × mode × key → pass/fail/diff) published per nightly; release gate = no regressions and all "tier 1" cells (Ghostty, Kitty, WezTerm, iTerm2 × legacy + kitty flags 1|2|8 × the common-chord set) green.
- Includes **Shift+Enter in Claude Code / Codex / pi / omp** as named cases (the most-reported agent-specific keyboard bug).

### 4.3 API end-to-end [M1+]

- `tests/e2e`: each test gets an isolated runtime dir + session, starts a real server binary, and drives it via the JSON-RPC client. Pane programs are deterministic fixtures (`vk-fixture`: scripted output, prompts, alt-screen apps, exit codes, OSC 133 shell).
- Coverage: every method in the catalog (07 §2) has at least one success test and one error test per documented error kind; generated test list from `api.methods` fails CI if a method has no test.
- Event contract tests: for each mutating method, assert exact emitted events (type, subjects, actor) and that `seq` returned in the result equals the last emitted event's seq.
- Subscription tests: `include_snapshot` atomicity under concurrent mutations (property test: snapshot + subsequent events reproduce final projections), overflow → resubscribe, truncated cursors.
- CLI tests: golden `--json` outputs and exit codes (0/1/2/3/4/5) for representative commands; `vibeke <noun>` with no verb never mutates (asserted by event count).
- TS client e2e: the generated `@vibeke/client` runs a subset to catch schema/codegen drift.

> **As built.** The per-method rule is enforced by `crates/vibeke/tests/api_method_coverage.rs`: every method in the schema catalog must appear in an integration test (`crates/*/tests`, plus vk-server's in-process `*_tests.rs`), either as the quoted name (`"pane.list"`) or as the CLI spelling that maps to it in `vk_cli::COMMANDS`. The exceptions are listed with a reason in `crates/vibeke/tests/api_method_allowlist.txt` (42 methods at the time of writing); the test also fails when a listed method becomes covered or no longer exists, so the list only shrinks. The check is textual and does not yet require both a success and an error test per method. `vk-fixture`, event-contract tests, the subscription-atomicity property test, the "noun with no verb" check and exit-code goldens are not built.

### 4.4 Harness golden tests [M1+]

Agent detection and adapters are where multiplexers break most often; every harness version change must be caught before users do.

- **Recordings** (`tests/harness-golden/<harness>/<version>/<scenario>/`): `pty.cast` (asciicast v2 with timing and resize events), `hooks.jsonl` / `extension.jsonl` / `rpc.jsonl` (structured channel traffic with timestamps), `transcript.jsonl` (harness session file), `expected-events.jsonl` (normalized Vibeke events: state transitions, interactions with payloads, turns/items, resume handle).
- **Scenarios per harness**: startup → idle; prompt → working → done; tool approval (allow / deny / always); AskUserQuestion single + multi select; plan-mode review; subagent spawn; long tool run; API error / rate limit; interrupt (Esc); compaction; `/rename`; exit; resume. Harnesses: Claude Code, Codex (TUI + app-server), pi (TUI + rpc), omp (TUI + rpc + rpc-ui), OpenCode, Gemini CLI, plus a generic manifest-only harness and an ACP agent.
- **Replay mode (every PR)**: feed recordings through the adapter + detectors with a virtual clock; diff produced events against expected. Screen-only replay (structured channels removed) asserts the fallback detectors still produce the right *coarse* states with `source: screen`.
- **Live mode (weekly + on harness release)**: a scheduled workflow installs the latest version of each harness CLI, runs the scenarios against real models using cheap models/fixed seeds where possible (or harness test/mock providers: pi custom provider, Codex `--oss`/mock), records new traces, and opens a PR "harness drift: claude 2.1.290" with the event diff. Renovate-style bot watches npm/GitHub releases of each harness to trigger runs.
- **Interaction delivery verification**: in live mode, answer each interaction via the API and assert the harness proceeded with the chosen option (native channel and keystroke fallback both tested).
- Version support policy: last 3 minor versions of each built-in harness are tested in replay; manifest `tested_versions` ranges updated by the drift PRs.

### 4.5 Remote tests [M3+]

- Two containers (or VMs on macOS runners) with SSH between them; netem profiles from §2.1. Scenarios: attach, type, run TUI agent, network drop for 5/30/120 s with reconnect, client sleep/resume, remote server restart, version skew (remote N-1), port forward + HMR websocket, screenshot round-trip, image upload, clipboard gating prompts.
- Bandwidth budgets (§1.5) asserted per scenario.

### 4.6 Agent skill test [M1+]

A headless Claude Code (and pi) session with only `vibeke --skill` as guidance executes scripted user requests ("start a codex reviewer next to you and summarize its findings", "take a screenshot of the dev server you started") against a test session; assertions on events (correct targeting with `@current`, no `@focused`, no forbidden methods, task used for editing delegation). Run nightly with a small model; flaky outcomes are tracked, not gating, until stable.

### 4.7 Preview fabric tests [M3]

- Port discovery against fixture servers: Vite, Next.js, Rails, Django, plain `python -m http.server`, servers bound to `0.0.0.0` vs `127.0.0.1` vs `::1`, servers in Docker containers (published ports), servers started by agents in nested process trees.
- Proxy: HTTP/1.1, HTTP/2 upstreams, websockets (HMR for Vite/Next), SSE, large uploads, cookies with `Domain`/`SameSite`, absolute redirects to `localhost:PORT` rewritten to the preview origin, CSP-sensitive pages.
- Screenshot determinism: fixed fonts in the headless browser image; pixel-diff tolerance tests.
- Security tests from 09 §12.

### 4.8 Herdr plugin and automation conformance [M5; Windows M6]

The normative contract and baseline are in [07](07-api-cli-plugins.md) §7.7–8.4. Maintain a complete inventory from the pinned binary's API schema, CLI grammar and plugin manifest schema; every baseline entry requires positive/negative coverage. Run the reference Herdr and Vibeke with isolated homes, runtime dirs, repos and session names. Diff observable behavior, normalizing only declared nondeterministic values. Reference binaries, upstream source and real plugin fixtures are pinned by version/SHA/checksum.

- Test the full public surface, including client/UI methods and plugin-owned panes/popups, rather than just a few requests. Assert shapes, omitted fields, error codes, exit codes, async timing, events, ids, focus and process/file side effects.
- Run original plugin manifests and code unchanged. Cover build/install/link/reinstall/uninstall, startup/event hooks, actions/log polling, all pane placements, link/key actions, runtime context/env, file-based state, shared registration across sessions and offline operation. Stub external services only; fixture success must depend on real Vibeke callbacks.
- Exercise private PATH/`HERDR_BIN_PATH` routing with real Herdr also installed, raw socket callbacks, explicit session selection and remote execution. Kill/restart the Vibeke server during a long action and a plugin pane; verify identity recovery, expected hook dispatch and revocation. Verify restricted callers cannot acquire legacy authority through invocation or session routing.
- Retain recorded fixture replay and a pinned smoke test as one consumer regression suite. Native scoped-plugin capability tests also remain required. Neither suite substitutes for the full inventory.
- M5 blocks on missing or failing macOS/Linux entries; M6 adds Windows named pipes, argv/PATHEXT, paths and terminal behavior. A release exposes only tested baselines/platforms; new upstream versions create a drift report and must pass the same gate before support is advertised. Never waive missing entries by classifying baseline public APIs as private transport.

---

## 5. Crash, restart and chaos tests [M1 gate]

The durability promise ("the server is disposable; processes are not") is tested adversarially.

### 5.1 `vk-chaos` scenarios

The oracle follows the honest recovery contract in 01 §1.2: processes and input integrity are hard guarantees; screen equality is guaranteed only for apps that repaint on SIGWINCH; raw-shell fidelity is measured, not gated.

| Scenario | Method | Assertions |
|---|---|---|
| Kill server mid-output | 30 panes: 10 high-rate output (`yes`/`seq`-style), 10 alt-screen fixture apps that repaint on SIGWINCH, 10 idle shells; `kill -9` server at random times (100 iterations) | **0 processes lost**; holder offsets contiguous (no lost or duplicated journal bytes); alt-screen panes equal a reference VT cell-for-cell after the resize nudge; raw-shell panes scored by the visual-fidelity metric (§5.2) |
| Replay side effects | panes emitting bells, OSC 9/777 notifications, OSC 52 writes and DA/DSR queries during the replayed window | **0** duplicate notifications, clipboard writes or query replies after recovery |
| Server-absent queries | app sends DA1/DA2/XTVERSION and DSR 6 while no server is attached | DA/XTVERSION answered by the holder immediately; DSR 6 answered on reattach if ≤ 5 s old, otherwise dropped and recorded |
| Resize interleaving | resizes issued between output bursts, then kill -9 | replay applies `Resize` markers in journal order; reference comparison as above |
| Kill server mid-input | client streaming pasted input (with `input_id`s) when server dies | **no input applied twice** (holder dedupe); un-acked chunks reported to the client as `input_unconfirmed`, never auto-replayed; client reconnects automatically |
| Kill server with pending approval | Claude (hook shim waiting), Codex (app-server request), pi RPC (`extension_ui_request` pending) and pi TUI (uiContext-wrapped extension dialog open) each with an open Interaction; kill -9 after the decision is recorded but before delivery | on restart the Interaction is reconciled: delivered exactly once or marked `delivery_unknown`; never delivered twice |
| Headless (pipe mode) run | pi `--mode rpc` / `codex app-server` under a holder in pipe mode; kill -9 server mid-turn | process survives; adapter resumes from the last processed frame offset; turn completes; no duplicated items |
| Kill server during VT snapshot write | crash injected (failpoint) inside snapshot persistence | recovery uses the previous snapshot + longer replay; never corrupt |
| Ring overflow | server stopped while pane emits > journal size | `pane.recovered {method: ring_only}`, user notified once; process alive; TUI panes repaint correctly after the nudge |
| Two servers race | start a second server for the same session while the first is alive / just killed | lease epochs: exactly one server holds each holder; stale server's writes rejected (fencing test) |
| Upgrade with protocol skew | server N+1 attaching to holders started by N (and N-1) | works; holders never restarted |
| Holder crash | `kill -9` a holder | pane marked `exited` with reason `holder_lost`; agent resume offered; other panes unaffected |
| SQLite failure | disk full, read-only FS, corrupt WAL (failpoints / FUSE fault fs) | server enters degraded mode (02 §4a): failed mutations return `storage_unavailable` and emit no events; panes keep running; no Interaction answer is delivered without being recorded; automatic recovery when a probe write succeeds |
| DB restore | restore `state.db` from backup while clients hold cursors | `log_epoch` rotated; clients get `events.truncated` and resnapshot; no client misinterprets an old cursor |
| Client death | kill TUI mid-frame; stall client socket (no reads) | server unaffected; stalled client disconnected after 30 s; panes keep running at full speed |
| Adapter/plugin panic | inject panic in an adapter and a plugin | server stays up; adapter restarted; run marks `adapter_health: lost` and falls back to screen detection with `source: screen`; event `agent.adapter_failed` |
| Clock jumps | system time set back/forward | `seq` ordering unaffected; timers use the monotonic clock |
| Reboot simulation | kill all holders + server, restart | layout restored, resume offered for runs with resume handles, correct resume argv per harness |

Failpoints via the `fail` crate compiled in under `--features chaos` (never in release builds).

### 5.2 Gate

- Every PR: 10 iterations of "kill server mid-output", the input-dedupe test, and the fencing test.
- Nightly: full matrix, 500 iterations of randomized kill points.
- Release gate: zero failures of hard assertions over the last 7 nightlies.
- **Raw-shell visual fidelity** (tracked, not gated): % of raw-shell panes whose post-recovery screen equals the reference cell-for-cell. Target ≥ 95% without ring overflow; regressions > 2 points between nightlies open a P2.

> **As built.** `crates/vibeke/tests/chaos.rs` (`VIBEKE_CHAOS_ITER`, default 10 on a PR, 500 nightly via `nightly.yml`, 2000 weekly) and `crates/vibeke/tests/chaos_gaps.rs` drive a real server and holders. Built: ring overflow (the server is down while a pane emits 20 MB over the 16 MiB ring: `pane.recovered {method: ring_only}` once, process alive, pane usable; slow in a debug build, so `#[ignore]`d and run in release by the nightly: `cargo test --release -p vibeke --test chaos_gaps -- --ignored ring_overflow`); holder crash (`kill -9` a holder: the slot gets a fresh shell, the run ends `holder_lost`, a neighbouring pane keeps its process, holder and input path; in this build the pane is respawned rather than left `exited`); event-log identity (`session_uuid`/`log_epoch` survive `kill -9`, a cursor from another epoch is refused with `truncated` plus the current cursor); a render client that stops reading cannot slow the server or other panes. Not built, kept as ignored tests that state the gap: restoring `state.db` from a backup does not rotate `log_epoch` (`restore_rotates_log_epoch`), and a stalled client is not disconnected after 30 s (`stalled_client_is_disconnected_after_30_seconds`). The `fail`-crate failpoints, pending-approval, pipe-mode, SQLite-failure, adapter-panic, clock-jump and protocol-skew scenarios are not built.

---

## 6. Fuzzing [M1+, continuous]

`cargo-fuzz` targets (each with a seed corpus checked in under `fuzz/corpus/`, minimized weekly):

| Target | Input | Oracle |
|---|---|---|
| `vt_parse` | arbitrary bytes into `vk-term` | no panic, no OOM (> 256 MiB), grid invariants (cursor in bounds, row widths consistent), `serialize→deserialize` round-trip equality |
| `vt_resize_interleave` | bytes + resize ops | as above + reflow invariants |
| `render_frame_decode` | bytes as server→client and client→server frames | no panic; bounded allocations (max cols/rows/grapheme table) — remote input is untrusted (09 §7) |
| `holder_proto_decode` | holder frames | no panic; epoch/fencing invariants in a stateful fuzz harness |
| `jsonrpc_decode` | control API lines (incl. U+2028/2029, huge numbers, deep nesting) | no panic; depth limit; 16 MiB cap |
| `compat_socket` | Herdr-compat requests | no panic; one-shot connection semantics preserved |
| `hook_payloads` | Claude/Codex/Gemini hook JSON, pi/omp extension events, codex app-server messages, ACP messages | adapters never panic; malformed → ignored with counter |
| `transcript_parse` | harness transcript JSONL | bounded memory with huge lines |
| `key_grammar` | key strings | parse ⇄ print round-trip |
| `policy_match` | rules + actions | no catastrophic regex backtracking (regex crate is linear-time; rule compile limits enforced) |
| `osc_image` | kitty graphics / sixel / iTerm2 payloads | decoder limits (dimensions, total pixels) |

Nightly: each target 1 CPU-hour; crashes auto-filed as private issues. M6: submit to OSS-Fuzz.

> **As built.** 16 targets share one set of functions in `crates/vk-fuzz/src/targets.rs` and `targets_extra.rs`: every target in the table above, plus `socks5_handshake`, `mux_frame_decode`, `kitty_probe`, `compat_import` and `manifest_toml`. Added by lane 1D: `vt_resize_interleave` (cursor inside the grid, requested dimensions, visible row count after every feed and resize, snapshot → restore equality), `compat_socket` (first line parsed as one request; the reply is exactly one valid JSON line carrying the request id), `transcript_parse` (bounded rows on huge lines, `consumed` ends on a newline, a parse split at any cut resumes without losing a line) and `osc_image` (kitty APC, sixel DCS and iTerm2 OSC 1337 payloads with hostile sizes into the engine, the bounded tile inflater, the placeholder codec). `osc_image` exercises the engine's decoders and Vibeke's own limits, not a separate decoder crate. Each has a seed corpus in `fuzz/corpus/` and a `fuzz/fuzz_targets` entry; on stable they run in `mise run fuzz-smoke` and `cargo test -p vk-fuzz` (a few hundred cases each), under libFuzzer in `nightly.yml` (default 3600 s per target, one matrix job each; `fuzz/artifacts` uploaded on failure) and `weekly.yml` (longer, plus `cargo fuzz cmin`; the minimized corpus is an artifact, never pushed). Crashes are not auto-filed as issues, and OSS-Fuzz is not set up.

---

## 7. Soak test [weekly from late M1, release gate]

- **Setup**: one server, 50 panes: 30 fixture "agents" replaying recorded harness sessions in a loop (with real hook/extension traffic through `vibeke hook` shims, generating interactions answered by a scripted client at random delays), 10 real shells running a build/test loop (`cargo test` on a sample repo), 5 alt-screen apps (`htop`, `vim` scripted), 5 panes in task worktrees with preview servers (Vite) and periodic screenshots. A TUI client attached via virtual terminal; a second CLI client polling `session.snapshot` every 2 s and an events subscriber.
- **Duration**: 24 h.
- **Assertions**: RSS growth of server ≤ 10% after hour 2 (no leaks); fd count stable; CPU within §1.3 budgets (scaled); no Interaction decision delivered twice, `delivery_unknown` rate ≤ 0.5%; event log `seq` gapless; subscriber saw every event; no `overflow` without recovery; scrollback archive and retention compaction run without blocking (p99 input latency during compaction ≤ 5 ms); `state.db` size within §1.3; zero panics in logs.
- Variant (nightly, 2 h): same over the `wifi` netem remote profile.

> **As built.** `scripts/soak.py` is the scaffold: an isolated server with N panes (output, idle, burst, shell mix), a poller (`pane list` every 2 s plus an event reader that records any hole in `seq`), and RSS/fd sampling every 5 s. It asserts RSS growth ≤ 10% after warm-up (median of the first three against the last three post-warm-up samples), a stable fd count, gapless `seq`, every pane process alive and the pane list complete, and no `panicked at` in the logs, and prints a JSON summary (exit 1 on a failed assertion). `nightly.yml` runs it for 15 minutes with 50 panes and `weekly.yml` for 5.5 hours (the job cap). Not built: fixture agents through hook shims, scripted interaction answers, real build loops, htop/vim panes, Vite previews, the TUI client, `delivery_unknown` and compaction-latency assertions, the 24 h release-gate run (needs a dedicated host) and the netem variant.

---

## 8. Release process

### 8.1 Channels and versioning

- SemVer for the binary; API `vibeke/1` and `holder/1` versioned independently (07 §1.5, §4).
- **Channels**: `preview` (built from `main` after green nightly; 2–4 per week) and `stable` (promoted preview build, every 2–3 weeks). `vibeke channel set <stable|preview>`. Default `stable`.
- **Canary**: before promotion to stable, a preview build must have run ≥ 72 h on the dogfood fleet (§9) with no P0/P1 regressions and its crash-report rate (opt-in telemetry) ≤ the previous stable's.
- **Staged rollout**: the update manifest carries `rollout_percent`; clients hash `install-id` to decide. Stable releases go 10% → 50% → 100% over 72 h; the update server can halt the rollout. (Update checks remain privacy-preserving: the percentage decision is made client-side.)

### 8.2 Release checklist (automated in a `release` workflow)

1. All PR gates + last 7 nightlies green (chaos, fuzz no new crashes, soak passed within 7 days).
2. Keyboard matrix tier-1 green; real-terminal latency gate on the Mac mini.
3. Harness golden replay green for all supported versions; no open "harness drift" PR older than 7 days for a built-in harness.
4. From M5: full Herdr conformance inventory green for every advertised baseline/platform, unchanged plugin fixtures and fixture replay/smoke green; native plugin scopes and legacy trust/revocation tests green (§4.8). Windows joins at M6.
5. Schema diff: no breaking API changes within major. *(As built: `scripts/schema-diff.py`, run by `api-schema.yml`. It compares the generated `docs/api/vibeke-1.schema.json` with the base revision's (the pull request's base branch, or the last `v*` tag on pushes and nightly; before the first tag only the in-tree `vibeke-1.frozen.json` is checked) and fails on a removed method or event, a changed `mutating`/`scope`/`pane_scope` flag, a params shape that stopped accepting something (removed property, type change, removed enum value, newly required property) or a result/event shape that stopped producing something. Label a PR `api-break-approved` for an intended pre-1.0 break. The checker is self-tested by `scripts/tests/schema-diff-test.sh`; `api_docs.rs` keeps enforcing the in-tree freeze. The rest of this checklist belongs to the unbuilt `release` workflow.)*
6. Changelog generated from conventional commits + hand-written highlights; `release-notes.json` embedded for the TUI "what's new".
7. Build artifacts (macOS arm64/x64 universal + notarized helper app bundle, Linux x64/arm64 musl, Windows x64 from M6), minisign signatures, Sigstore provenance, sha256 sums.
8. Smoke-install on clean VMs (macOS, Ubuntu, Fedora, Arch, NixOS, Windows M6) via `install.sh`, Homebrew tap, Nix flake; run `vibeke doctor` and a 2-minute e2e.
9. Publish, start staged rollout.

### 8.3 Rollback

- Client: `vibeke update --rollback` swaps `current` back (previous version dir is retained); server restarted; holders untouched; DB migrations are forward-only but **every migration in a stable release must be readable by the previous stable** (expand/contract discipline: add columns/tables first, drop only two releases later). CI tests "migrate with N, run N-1 against the migrated DB" for each release.
- Server-side: halt rollout; mark release as `yanked` in the manifest → clients on it are offered an update to the fixed or previous version.
- Holder protocol never changes in a patch release.

### 8.4 Bug severity and response

| Severity | Examples | Response |
|---|---|---|
| P0 | process loss, data loss, security hole, unbootable | halt rollout immediately, hotfix within 24 h |
| P1 | keyboard regression in tier-1 cell, adapter mis-reports `needs_approval` as `idle` for a built-in harness, perf budget breach > 2× | fix in next preview, block stable promotion |
| P2 | other | normal cadence |

---

## 9. Dogfooding plan

- **From M1**: the core team runs Vibeke as their primary multiplexer. **The previous setup stays installed** as the fallback during dogfooding, and every fall-back use is logged with a reason; uninstalling them is not a quality metric. The two-week baseline for §1.7 is recorded before switching. The maintainer's setup is the reference workload: 5+ workspaces (samplehub, dashboard, backend, storefront, home), Claude Code + Codex side by side, sibling `*-todo` worktrees → migrated to `vibeke task`.
- **From M1**: pi and omp with custom extensions are daily drivers next to Claude and Codex on at least one machine (validates "bring your own harness").
- **From M2**: yolo runs default to `sandbox`/`container` in at least two repos; custom manifests (`espi`, Hermes) in daily use.
- **From M3**: laptop + Linux devbox; at least half of agent work runs remotely; previews used for all web work (samplehub, storefront).
- **From M5**: a representative set of unchanged Herdr plugins running against the compat layer on the dogfood fleet, including local/remote machines, state migration, restart, hooks and plugin terminal UI. The previous setup remains installed as fallback.
- **Instrumentation for dogfood builds**: opt-in local-only metrics file (`~/.local/state/vibeke/metrics.jsonl`): input latency histograms, CPU, recovery events, detector disagreements (adapter vs screen), interactions answered and channel used, keystroke-fallback failures. Weekly review → issues.
- **"Papercut Fridays"**: one day per week reserved for fixing dogfood friction reports; each milestone's exit criteria include "no open dogfood P1s".
- **Detector disagreement log**: whenever the structured adapter and the screen detector disagree for > 10 s, a redacted screen capture + event trace is saved locally for triage — this is the main source of new golden test cases.

---

## 10. Quality gates per milestone

| Milestone | Exit gate (in addition to feature completeness) |
|---|---|
| **M0 spikes** | VT engine (libghostty-vt, 03 §2) passing the C4 hard gate (serialize/restore incl. parser state at arbitrary cut points, 03 §2.3) with measured esctest pass rate and throughput ≥ 300 MB/s; holder prototype survives 100 kill -9 server iterations with zero process loss and zero duplicated input; key→screen added latency prototype ≤ 3 ms p99 |
| **M1 supervision slice** | §1.1–1.4 local budgets green; chaos PR gate green (incl. pending-approval and duplicate-input scenarios); esctest + corpus gate; keyboard tier-1 matrix green; API e2e for all M1 methods; golden replay for Claude, Codex, pi, omp; interaction delivery verified per tested capability (native where the capability table says native, keystroke fallback otherwise); red-team agent suite green (host = cooperative guardrails); agent CPU budgets; fuzz targets running nightly; **§1.7 product metrics met vs the baseline** |
| **M2 safe yolo + harnesses** | containment tests for `sandbox`/`container` (09 §12, 13 §14); egress proxy and fail-closed boundary Interactions; golden replay for every harness added (OpenCode, Gemini, ACP, custom manifests); live drift workflow running; soak test passing |
| **M3 remote + preview** | remote bandwidth/latency budgets on `lan`/`wifi`/`mobile`; reconnect scenarios; version-skew tests; discovery/proxy/screenshot budgets; framework fixture matrix green; preview security tests green; review-minutes metric baselined |
| **M4 VMs + polish** | VM containment + start-time budgets (13 §14); warm-pool/fork tests; parity features' e2e tests (groups, floating panes, palette, FTS archive search) |
| **M5 compatibility + plugins** | full pinned Herdr public contract inventory green on macOS/Linux; unchanged real plugin suite and socket-client replay/smoke green; installation/migration/lifecycle/routing tests; native scopes and legacy trust/revocation/no-escalation tests (§4.8) |
| **M6 hardening / Windows / 1.0** | full matrix incl. Windows Terminal and Herdr plugin/automation conformance on Windows; 7 consecutive green nightlies; external security review findings closed; reproducible Linux builds; OSS-Fuzz onboarding; 30 days of dogfood with zero P0; §1.7 product-metric gates met |


*Open (2026-10-07):* on GitHub's Ubuntu runner, the first CLI call after a chaos `kill -9` was once answered with `Connection reset by peer` while the auto-started server came up (macOS never shows it). `tests/chaos.rs` now retries transient `io` errors; the server-side cause needs a Linux host with the server log.
