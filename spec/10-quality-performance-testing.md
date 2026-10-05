# 10 — Quality, performance and testing

Terminal multiplexers for agents tend to suffer from three classes of problems: keyboard/terminal fidelity (Kitty protocol, AltGr, Shift+Enter, undercurl, emoji width), performance with many agents (CPU/fans on macOS, 3.6 GB/h remote bandwidth per animating pane, 570 ms tab close, Windows freezes), and detection fragility. This section makes each of those a **measured budget with a CI gate**, not a hope.

Milestones (plan in [11](11-milestones.md)): **M0** spikes, **M1** supervision slice, **M2** safe yolo + more harnesses, **M3** remote + preview, **M4** VMs + polish/parity, **M5** compat + plugins, **M6** hardening / Windows / 1.0; **post-1.0** deferred (QUIC, marketplace, …).

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
| Compat | socket-client fixture replay + pinned smoke | every PR touching `vk-compat`, nightly |
| Security | §12 of 09 red-team agent | every PR |
| Perf | §2 | every PR (subset), nightly (full), release gate (real terminals) |

**Toolchain**: CI builds and tests with the latest stable Rust pinned in `rust-toolchain.toml` (01 §2), bumped within a week of each stable release by an automated PR that must pass the full PR gate; a nightly job also builds with the upcoming beta to catch breakage early. No MSRV older than the pinned stable is tested or supported.

Coverage target: ≥ 80% line coverage on `vk-proto`, `vk-hold`, `vk-store`, `vk-agents` (adapter logic), `vk-compat`; the TUI is covered by snapshot and e2e tests instead.

---

## 4. Test suites in detail

### 4.1 VT conformance [M0 selection, M1 gate]

- **esctest2** (George Nachman's suite, xterm reference) run headless against `vk-term` through a PTY: target ≥ the chosen engine's upstream pass rate; any regression fails CI. Known deviations are listed in `tests/vt/expected-failures.toml` with justification.
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
| Kill server with pending approval | Claude (hook shim waiting), Codex (app-server request) and pi (extension gate) each blocked on an Interaction; kill -9 after the decision is recorded but before delivery | on restart the Interaction is reconciled: delivered exactly once or marked `delivery_unknown`; never delivered twice |
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

---

## 7. Soak test [weekly from late M1, release gate]

- **Setup**: one server, 50 panes: 30 fixture "agents" replaying recorded harness sessions in a loop (with real hook/extension traffic through `vibeke hook` shims, generating interactions answered by a scripted client at random delays), 10 real shells running a build/test loop (`cargo test` on a sample repo), 5 alt-screen apps (`htop`, `vim` scripted), 5 panes in task worktrees with preview servers (Vite) and periodic screenshots. A TUI client attached via virtual terminal; a second CLI client polling `session.snapshot` every 2 s and an events subscriber.
- **Duration**: 24 h.
- **Assertions**: RSS growth of server ≤ 10% after hour 2 (no leaks); fd count stable; CPU within §1.3 budgets (scaled); no Interaction decision delivered twice, `delivery_unknown` rate ≤ 0.5%; event log `seq` gapless; subscriber saw every event; no `overflow` without recovery; scrollback archive and retention compaction run without blocking (p99 input latency during compaction ≤ 5 ms); `state.db` size within §1.3; zero panics in logs.
- Variant (nightly, 2 h): same over the `wifi` netem remote profile.

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
4. Compat suite green (fixture replay + pinned socket-client smoke).
5. Schema diff: no breaking API changes within major.
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

- **From M1**: the core team runs Vibeke as their primary multiplexer. **The previous setup stays installed** as the fallback during dogfooding, and every fall-back use is logged with a reason; uninstalling them is not a quality metric. The two-week baseline for §1.7 is recorded before switching. the maintainer's setup is the reference workload: 5+ workspaces (samplehub, dashboard, backend, storefront, home), Claude Code + Codex side by side, sibling `*-todo` worktrees → migrated to `vibeke task`.
- **From M1**: pi and omp with custom extensions are daily drivers next to Claude and Codex on at least one machine (validates "bring your own harness").
- **From M2**: yolo runs default to `sandbox`/`container` in at least two repos; custom manifests (`espi`, Hermes) in daily use.
- **From M3**: laptop + Linux devbox; at least half of agent work runs remotely; previews used for all web work (samplehub, storefront).
- **From M5**: existing socket clients running unmodified against the compat socket on the dogfood fleet (the bridge to Phase 2).
- **Instrumentation for dogfood builds**: opt-in local-only metrics file (`~/.local/state/vibeke/metrics.jsonl`): input latency histograms, CPU, recovery events, detector disagreements (adapter vs screen), interactions answered and channel used, keystroke-fallback failures. Weekly review → issues.
- **"Papercut Fridays"**: one day per week reserved for fixing dogfood friction reports; each milestone's exit criteria include "no open dogfood P1s".
- **Detector disagreement log**: whenever the structured adapter and the screen detector disagree for > 10 s, a redacted screen capture + event trace is saved locally for triage — this is the main source of new golden test cases.

---

## 10. Quality gates per milestone

| Milestone | Exit gate (in addition to feature completeness) |
|---|---|
| **M0 spikes** | VT engine chosen and passing the C4 hard gate (serialize/restore incl. parser state at arbitrary cut points, 03 §2.3) with measured esctest pass rate and throughput ≥ 300 MB/s; holder prototype survives 100 kill -9 server iterations with zero process loss and zero duplicated input; key→screen added latency prototype ≤ 3 ms p99 |
| **M1 supervision slice** | §1.1–1.4 local budgets green; chaos PR gate green (incl. pending-approval and duplicate-input scenarios); esctest + corpus gate; keyboard tier-1 matrix green; API e2e for all M1 methods; golden replay for Claude, Codex, pi, omp; interaction delivery verified per tested capability (native where the capability table says native, keystroke fallback otherwise); red-team agent suite green (host = cooperative guardrails); agent CPU budgets; fuzz targets running nightly; **§1.7 product metrics met vs the baseline** |
| **M2 safe yolo + harnesses** | containment tests for `sandbox`/`container` (09 §12, 13 §14); egress proxy and fail-closed boundary Interactions; golden replay for every harness added (OpenCode, Gemini, ACP, custom manifests); live drift workflow running; soak test passing |
| **M3 remote + preview** | remote bandwidth/latency budgets on `lan`/`wifi`/`mobile`; reconnect scenarios; version-skew tests; discovery/proxy/screenshot budgets; framework fixture matrix green; preview security tests green; review-minutes metric baselined |
| **M4 VMs + polish** | VM containment + start-time budgets (13 §14); warm-pool/fork tests; parity features' e2e tests (groups, floating panes, palette, FTS archive search); jj task tests |
| **M5 compat + plugins** | socket-client fixture replay + pinned smoke green; plugin capability enforcement tests |
| **M6 hardening / Windows / 1.0** | full matrix incl. Windows Terminal; 7 consecutive green nightlies; external security review findings closed; reproducible Linux builds; OSS-Fuzz onboarding; 30 days of dogfood with zero P0; §1.7 product-metric gates met |
