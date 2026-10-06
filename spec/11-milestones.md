# 11 — Milestones and build plan (Phase 1)

The value proof comes first. M1 is a narrow but daily-usable runtime whose purpose is to show that structured agents and native interactions beat the current setup (tmux-style multiplexer) on the maintainer's real workload ([00](00-vision-and-scope.md) §Success metrics). Parity polish comes later. Each milestone ships to the `preview` channel and is dogfooded daily. Exit gates are in [10-quality-performance-testing.md](10-quality-performance-testing.md) §10; feature acceptance criteria live in each section.

```
M0 spikes ─► M1 supervision slice ─► M2 safe yolo + harnesses ─► M3 remote + preview ─► M4 VMs + polish/parity ─► M5 compatibility + plugins ─► M6 hardening / Windows / 1.0
                    │
                    └─ gate: product metrics vs baseline; fail → re-evaluate (sidecar fallback, 00 §Alternatives)
```

No calendar estimates are given: the earlier week numbers were unsupported placeholders. Sizing is tracked per milestone once M0 has produced real measurements. **The previous setup stays installed throughout dogfooding** as the fallback; uninstalling it is not a goal.

## Goal 01 track (current priority)

The first build goal ([milestones](11-milestones.md)) is a remote daily driver over SSH for the maintainer. It covers **M0 + M1 + the remote-machine half of M3** (06 Part A: machines, bootstrap, bridge, multi-machine view, reconnection, clipboard, image paste, dropped-path translation). M2 (safe yolo, extra harnesses) and the preview half of M3 come after it. Milestone numbering is unchanged; only the order of delivery differs.

**Next goals:** [task outcomes and review](15-task-outcomes-review-and-attention.md) (spec 15 T1–T3, plus pi/omp) and [remote and preview design](06-remote-and-preview.md) (the preview half of M3, with the in-terminal browser pane). M2 isolation follows.

The proposed follow-on product slice is [15 — Task outcomes, review and attention](15-task-outcomes-review-and-attention.md): optional tracking of normally launched CLIs, evidence-backed review, and a ranked decision inbox. Its T1–T4 stages preserve Goal 01's scope; they are not extra prerequisites for the SSH switch-over. The basic M1 inbox is the first version of the same surface, enriched in place by later stages.

## M0 — Spikes (de-risk the hard bets)

| Spike | Question | Output |
|---|---|---|
| VT engine (03 §2) | **Decided: libghostty-vt** (2026-10-06; replaces the alacritty_terminal binding built first). Remaining work: vendored build via Zig, binding behind `VtEngine`, C4 recovery gate passing on its native snapshot API, throughput and esctest baselines. | `vk-term` on libghostty-vt passing C4, `.adr/0001-vt-engine.md` |
| Holder durability (01 §1.2) | Do processes survive `kill -9` of the server; how good is screen recovery (checkpoint on ring half-full, safe cut points, forced redraw via resize nudge, holder-answered terminal queries); input with ids/acks so delivery is never duplicated; **pipe mode** so headless adapters (pi `--mode rpc`, Codex app-server) are also owned by a holder and survive server restarts | Holder prototype, chaos loop, measured failure envelope written into 01 §4 |
| Harness reality check (04 [verify] list) | Run every **[verify]** item against live binaries: Claude `PermissionRequest`/AskUserQuestion via hook and `updatedPermissions` shape; Codex hooks under `--disable daemon_auto_start` and under `-a never`; deterministic Codex thread binding; omp `tool_call` vs approval ordering; pi/omp shared-uiContext wrapper (writable methods, `signal` dismisses the native dialog) per version | Capability matrix (harness version × launch mode × interaction kind) checked into 04 |
| Approval-delivery transaction | Decision record → delivery lease → native delivery → ack, with idempotency key, `delivery_unknown` and reconciliation, under server kill at every step | Prototype + chaos test; final state machine in 02/04 |
| Input latency | Client-side keymap + server render stream ≤ 3 ms p99 added latency on Ghostty and Kitty | Measured prototype |

## M1 — Supervision slice (the value proof)

Minimal daily-use runtime:
- Sessions, workspaces, tabs (numbers + titles), panes, splits, zoom, resize, detach/attach, multiple clients (geometry controller lease, 03).
- Sidebar with execution state + attention markers, **needs-you section on top**, mark unread, isolation glyphs and `YOLO`/`YOLO·HOST` badges.
- Copy mode basics (vi keys, `/` search in the in-memory buffer), OSC 52 copy, bracketed paste, kitty keyboard, mouse.
- Notifications: OSC 9/99/777 in and out, `vibeke notify`, toasts, `prefix+a` next attention.
- Holders with process survival across server restart; best-effort screen recovery as measured in M0.
- SQLite state + transactional event outbox; JSON-RPC API and CLI for everything above; `vibeke --skill`.
- Config + hot reload; Herdr config and session importer.

Agents:
- **Claude Code** (hooks + transcript), **pi and omp** (`@vibeke/pi-extension`, observe-only — no Vibeke approval gate; dialogs of the user's own permission extension surfaced and answerable via the shared-uiContext wrapper where golden-tested, RPC Extension UI Protocol in headless mode, screen fallback), **Codex** via the PATH shim (per-pane embedded app-server) + hooks, with screen fallback.
- `AgentState` with source/confidence, `Interaction` objects with the delivery state machine, interaction cards for unfocused agents (the focused pane always shows the agent's own UI; gate-mode release-on-focus), inbox view, **peek and reply without attaching**, batch answer for identical native approvals.
- `agent start|prompt|wait|read|send-keys|get|list`, names, resume after reboot.
- `vibeke integration install|status|uninstall|doctor` for Claude, pi/omp, Codex (coexists with existing hooks).
- Yolo detection for user-typed flags (13 §3.1); `deny` policy rules via pre-tool hooks.
- Policy engine basics (allow/deny/ask rules; repo policy requires trust); decision log recorded.

Tasks:
- `vibeke task new` with **git worktree** isolation, env file copy, setup script, machine-wide port leases, async removal, branch status in the sidebar. Advisory collision warnings for shared-cwd agents (no automatic migration).

**Gate:** M1 exit gates in 10 **and** the product metrics in 00 (operator interventions −70%, blocked time −50%, approval delivery failures < 1%) over two weeks of the maintainer's real use versus the baseline. Failing materially → re-evaluate before M2.

## M2 — Safe yolo + more harnesses

**Status (2026-10-06):** built: the `sandbox` level on macOS (Seatbelt, real-`sandbox-exec` tests); the Linux bubblewrap/Landlock/seccomp chain (generated and unit-tested, not yet run on Linux); the egress proxy with `none`/`harness-apis`/`package-registries`/`dev`/`open` profiles and fail-closed egress Interactions; the per-pane broker; credential projection for Claude, Codex, pi and omp; `--yolo` / `--isolate` / `--network`; and local paste translation into the inbox for sandboxed panes. The `container` level is built: one box per task on OrbStack/Docker/Podman (Apple `container`: `open` network only; Docker Sandboxes: detection only). Proxy profiles get `--network none` plus a per-box `exec -i … vibeke sandbox bridge` link (the `vk-remote` mux) that tunnels egress to the host proxy and relays the pane brokers; this needs the static Linux `vibeke`. Also built: `clone` code isolation with `vibeke task sync` (host-side fetch through in-box git services; push to a side ref), devcontainer support (image/build/user/env/mounts; lifecycle commands only after repo trust), `vibeke sandbox start|stop|rm`, finish-time sync that never removes unsynced work, `/vibeke/inbox` paths for drops, and the doctor section. These are tested against a fake runtime, and against real OrbStack behind `VIBEKE_CONTAINER_TESTS=1`. Still open: the in-box bridge/holders (the holder stays on the host, so box ports are not forwarded as previews), templates/warm pools/idle suspend, Podman/Apple runs, and the harness and usage bullets below. Details: 13 §15.

- Execution isolation per [13](13-sandboxes-and-vms.md): `sandbox` level (Seatbelt on macOS; bubblewrap + Landlock + seccomp on Linux), `container` level (Apple `container`, OrbStack/Docker, Podman, Docker Sandboxes), host-side egress proxy with network profiles and egress Interactions, credential projection (Claude token, Codex auth, pi/omp env), `clone` code isolation + `task sync` via host-side fetch, devcontainer support, `--yolo` / `--isolate`. Dropped/pasted path translation into sandbox/container inboxes (06 A11, local namespaces; remote follows in M3).
- Harnesses: OpenCode, Gemini CLI, ACP-generic, custom harness manifests (`espi`, Hermes), Herdr-compatible self-report; screen detector manifests with golden corpus and nightly drift detection; signed manifest channel.
  - *Status (2026-10-06):* built and tested with fake harnesses only (04 §14.1) — declarative manifests (built-ins, user dir, trusted `repo:<id>`, `espi` example, Hermes screen+self-report), OpenCode plugin + Gemini hooks with installers (**unverified**: observe + keystrokes until golden-tested), ACP host (`agent start --acp`, native permission answers), Herdr `pane.report_agent*` on the main socket, manifest screen engine with a **synthetic** golden corpus and drift replay, `integration doctor` version-vs-range, signed-channel client (refuses until release keys exist). Open: live verification of every [verify M2] item, recorded corpus + nightly drift CI, Codex daemon observer, external adapters, §6.7 screen manifests, Hermes plugin.
- Turns/items/usage/rate-limit extraction from adapters and transcripts.
  - *Status (2026-10-06):* OpenCode/Gemini/ACP feed the same turn/item vocabulary as Claude (tracking works for them); `AgentRun.usage`/`rate_limit` from Claude/Codex transcripts, pi/OpenCode/Gemini/ACP reports and rate-limit signals (04 §10). Open: per-turn usage records, price table, Codex app-server events, limits status segment.

## M3 — Remote + preview

- Saved SSH machines, no-sudo bootstrap with checksum, bridge multiplexing over SSH stdio, unified multi-machine sidebar, `--machine` forwarding (never falls back to local), reconnect/offline states, bandwidth budgets and adaptive frame rate, remote clipboard, image paste local → remote, **dropped/pasted path translation over the bridge** (06 A11).
- Previews: discovery (process-tree listening sockets + URLs in output + `vibeke preview declare`); **a live browser pane in the layout** (laptop-side headless Chromium drawn with kitty graphics, 06 B3.2) and a one-key external window, both using a dedicated browser profile over SOCKS through the bridge so `localhost:5173` on the remote works unmodified (no Host rewriting); authenticated per-preview proxy origins as the alternative mode. Automatic forwarding of discovered ports is opt-in. Humans can watch and take over an agent's browser session.
- Remote **scriptable** headless Chromium over CDP (navigate, click, type, eval, screenshot, console/network logs, DOM snapshot) exposed as CLI + `vibeke mcp`; navigation restricted to declared previews (redirects, subresources and WebSockets included).
- Screenshots as blobs with `environment_label`; `EvidenceRecord` groundwork (12): base/head sha, checks observed, artifacts.
- Human review minutes per accepted change baselined.

## M4 — VMs + polish/parity

- `vm` level (Lima `vz`/Tart on macOS, Firecracker/Cloud Hypervisor on Linux), template snapshots, warm pools; previews and screenshots verified inside containers and VMs.
- Parity polish: groups, floating panes, status bar (built-in segments), command palette, goto improvements, sidebar left/right, bottom tab bar, copy-on-select/PRIMARY, configurable copy-mode keys, archived-scrollback FTS search and edit-scrollback, native OS notifications with click-to-focus, theme auto light/dark + propagation, layout export/apply, jj workspaces.
  - *Status (2026-10-06), server/API/CLI side:*
    - **Layout export/apply: built.** `layout.export {tab | workspace, format: toml}` → `LayoutSpec` (07 §2.14); `layout.apply {name | doc | layout, workspace? | new_workspace?}`; `layout.list/get`; named layouts in `[layouts.<name>]`; `workspace.create {layout, group}`, `tab.create {layout}`; `vibeke layout export|apply|list|get`.
    - **Archived-scrollback search: built.** `search.query` covers live screens and scrollback plus the archive (FTS5, or a segment scan with `regex`). Hits carry context and an archive position. `pane.read {source: archive, from, to}` pages a pane's whole history, closed panes included. `vibeke search <q>`. Read scope is enforced (09 §5.1 rule 4).
    - **Native notifications: built, with one caveat.** The pipeline runs rules (`notifications.on`), presence suppression, quiet hours and coalescing, then delivers to `notifications.channels`. Notifiers: `terminal-notifier`/`osascript` on macOS, `notify-send` on Linux, and a `log:` fake. Click-to-focus goes through `vibeke focus <url>` / `client.focus`, which focuses the pane in the most recently active client and raises its terminal by bundle id. Caveat: native delivery is automatic only once a client reports `host` metadata. **No signed helper bundle yet**: plain `osascript` notifications can't be clicked.
    - **Theme: server side built.** `theme.mode`, `client.appearance {dark}`, the `theme.changed` event and `SessionModel.appearance`. New panes get `COLORFGBG` and `VIBEKE_THEME[_NAME]`.
    - **jj workspaces: built.** `task new --isolation jj_workspace|worktree|none|auto`: `jj workspace add`, `jj workspace forget` plus directory removal, and bookmark/change status in `task.get`. Verified against a fake `jj` only; jj was not installed when this was written.
    - **Groups, floats and status segments: model/API built.** `group.*` (membership stored on `Group`), `Tab.floating`/`floats_hidden` with `pane.float`/`pane.embed`/`tab.floats` (floats are kept in layout export/apply), and `status.segments`.
  - *Status (2026-10-06), TUI side:*
    - **Groups: built.** Sidebar group level with aggregate badges, collapse/expand (`group.collapse`), navigate-mode keys (`enter`/`h`/`l`/`r`, `m` move picker, `G` new), drag a workspace onto a group, palette `group_new/move/rename/collapse` (08 §2.1).
    - **Floating panes: built.** Drawn over the tiling in z order with a frame and title, mouse move/resize/raise, resize-mode keys (`m` move), `prefix+f`/`prefix+shift+f`, palette `float_pane`/`embed_pane`; floats are in `pane_rects`/`ViewHint`, so their PTYs get real sizes and browser tiles clip (08 §5).
    - **Status bar: built.** Top/bottom row from `[ui.status_bar]`, data from `status.segments` (refresh ≥ 1 s after model changes, every 10 s otherwise), local fallback, local clock/mode/prefix, attention click (08 §4).
    - **Search: built.** Copy-mode `/` falls back to `search.query` + `pane.read` loading; archive paging with `pane.read` past memory; global search popup (`prefix+alt+/`) with jump-to (08 §13 copy mode). Edit-scrollback popup still open.
    - **Theme auto: built, with a limit.** Startup OSC 11 + `CSI ? 996 n`, `client.appearance` to every machine, `dark_name`/`light_name` switching; re-query on focus regain instead of mode 2031 (crossterm can't parse unsolicited 997 reports). Panes' OSC 10/11 queries are answered once, server-side, from the appearance's palette (small server change in `theme.rs`/`pane.rs`).
    - **Notifications: built.** `host` in `render.attach` (local machine only), no OSC forward when `delivered` has `native`, coalesced `(×n)` toasts. The signed helper bundle is still open.
    - **Layout save/apply: built** as palette entries; save prints/copies a `[layouts.<name>]` snippet (no config-write API).
    - Server change for paging: `pane.read {source: archive}` also returns `mem_first`.
    - Still open (TUI-only): sidebar left/right, bottom tab bar, copy-on-select/PRIMARY, configurable copy-mode keys, edit-scrollback popup, goto improvements (`ctrl+enter`), jj bookmark display. Verified with fake machines and the unit/draw tests only — not yet driven in a real terminal against a live server.

## M5 — Full Herdr plugin/automation compatibility + plugins

- `vk-compat`: full public CLI/socket contract for the pinned Herdr baseline (07 §8.0), including plugin-pane and UI-control APIs, exact result/error/event semantics, a private compatibility launcher and identity-bound callback brokers. Existing socket clients run unmodified, giving an early phone decision surface before the Phase 2 gateway.
- Unchanged Herdr plugins: complete manifests, build/startup/event hooks, async actions/logs, all terminal placements, link handlers, context/env, global per-user registry, offline installation, config/state migration and explicit legacy trust. No popular-plugin-only coverage shortcut (07 §7.7).
- Native additions: long-running plugin processes with capabilities, UI contributions (status segments, sidebar sections, palette commands), KV storage and `vibeke plugin link` hot reload.
- Exit evidence: exhaustive baseline inventory, differential tests against the pinned Herdr binary, unmodified real plugin fixtures and a real socket-client smoke test on macOS/Linux, including lifecycle, routing, trust/revocation and migration. Missing baseline support blocks full compatibility and M5 completion (07 §8.4, 10).
- **Status (2026-10-06): slices 1–3 built; support partial.** Inventory ([docs/herdr-compat-inventory.md](../docs/herdr-compat-inventory.md), not yet derived from the baseline binary's schema): 196 surfaces, **96 implemented, 89 partial, 11 missing** (slice 2: 91/88/17; slice 1: 77/61/56 of 194). Slice 1 built manifest parsing over the 99 real manifests; the per-user registry with explicit, digest-bound `herdr_legacy` trust; async actions with logs, `[[events]]` hooks and `[[startup]]`; private per-invocation brokers with revocation; the opt-in compat listener with Herdr wire semantics and the core workspace/tab/pane/agent/notify/report mapping; event projection; the `herdr` CLI shim and private launcher; and the gated differential harness (not run). Slice 2 (server/CLI) added 23 socket methods (`layout.export/apply/set_split_ratio`, `pane.process_info/move/swap`, `pane/workspace.report_metadata`, `client.window_title.set/clear`, `agent.start/prompt/wait/rename`, `worktree.create/open` with native counterparts, `plugin.link/unlink/enable/disable`, `plugin.pane.open/focus/close` for split/tab/zoomed/overlay); the `tab.moved`, `pane.moved`, `pane.output_matched` and `worktree.created/opened` events (the last two are declared 15 times by 8 corpus plugins); Herdr's named-session socket layout and `--session`/`HERDR_SESSION` in the shim; broker re-issue after a restart and the long-running rule; log persistence, redaction and metadata-only audit events; and copy-only migration with rollback (`vibeke plugin migrate --from`). Slice 3 (TUI, `vk-tui::plugins`) added popups (session-modal floating windows sized by `width`/`height`, hidden from the compat pane list and events, no `HERDR_PANE_ID`) and real overlays (a full-area layer over the tab) with `popup.close` and focus restore; plugin actions in the command palette (untrusted/disabled listed but disabled with the fixing command) and `[[keys.command]] type = "plugin_action"` bindings; link handlers on hint labels and Ctrl/Alt+click with `HERDR_PLUGIN_CLICKED_URL`/`HERDR_PLUGIN_LINK_HANDLER_ID`; the plugin window title (OSC 2 or the tab bar); and `pane.scroll_changed` from copy-mode scroll reports (`ClientFrame::ScrollView`). Open for full M5: baseline schema/CLI-help capture and an exhaustive inventory; the differential suite against the pinned binary; `agent.view.set/clear` (undefined until the schema is captured); manifest-declared default key bindings and action-context filtering in the palette; installs from repositories; cross-workspace pane moves; `HERDR_*` in ordinary panes (still stripped); and a real socket-client replay/smoke (07 §7.7, §8.3 "As built").

## M6 — Hardening, Windows, 1.0

- Windows host (ConPTY, named pipes, holder equivalent), Windows Terminal in the keyboard matrix; the full Herdr plugin/automation suite including Windows argv/PATHEXT and paths.
- External security review; reproducible Linux builds; OSS-Fuzz; docs site; migration guide from Herdr; 1.0 API freeze (`vibeke/1`).

## Deferred beyond 1.0 (unless real demand appears)

QUIC roaming transport + predictive echo (mosh-style) · plugin marketplace · policy learning (rule suggestions) · best-of-N comparison UI · automatic shared-cwd "split into task" migration · synchronized input · multiple graphics fallbacks beyond kitty graphics. Herdr's private TUI/binary transport is outside the public compatibility contract; the full public plugin/automation API is required in M5.

## Suggested first two weeks (M0 kickoff)

1. Repo scaffold: Cargo workspace per 01 §6, `mise.toml` (already in the repo) as the toolchain source, CI via `jdx/mise-action` running `mise run ci` (fmt, clippy `-D warnings`, nextest, cargo-deny), Renovate for toolchain bumps.
2. `vk-proto` v0: IDs, event envelope, holder protocol messages with input ids/acks.
3. `vk-hold` prototype (PTY and pipe modes) + chaos loop script.
4. VT engine bindings behind `VtEngine`, recovery-requirement test suite (mid-sequence cut, resize interleaving, terminal queries, alt screen).
5. Harness reality-check script that launches each installed agent (claude, codex, pi, omp) in a PTY and records hook/extension events to JSONL traces: the first golden fixtures and the capability matrix.
6. Baseline measurement: export two weeks of the current setup's `audit.log` and state timings from the current setup for the 00 metrics.

## Top risks

| Risk | Mitigation |
|---|---|
| VT engine can't meet the recovery requirement | libghostty-vt's native snapshot (with parser continuation) is gated by the C4 test; fallback is a weaker, honestly documented guarantee (process survival + forced redraw) |
| libghostty-vt C API / snapshot format churn | Vendored pinned source, patch log, all FFI behind `vk-term`'s wrapper; pin moves are deliberate and re-run C4 + corpus; snapshot version mismatch only degrades one recovery to ring-only replay |
| Harness vendors change hooks/UIs often | Structured channels first; capability matrix per version; golden + live drift CI; signed manifest channel |
| Codex shared daemon and vendor architecture shifts | PATH shim → per-pane embedded server; deterministic thread binding only (heuristic correlation never authorizes writes); screen fallback |
| The pinned compatibility baseline moves fast (weekly upstream releases) | Pin supported compatibility baselines, diff upstream schemas/CLI/behavior, and require full conformance before advertising an upgrade; preserve native differentiation in interactions, isolation, harnesses and evidence |
| Scope | M1 gate on product metrics; explicit post-1.0 list; sidecar fallback if the value proof fails |

## Milestone mapping (old → new)

For reconciling references in other sections written against the earlier plan:

| Old milestone | Content | New milestone |
|---|---|---|
| M0 spikes | VT engine, holder, latency, harness reality check | **M0** (+ approval-delivery transaction, holder pipe mode) |
| M1 local core | core runtime (sessions, panes, splits, holders, outbox, API, input fidelity, copy mode basics, config, importer) | **M1** |
| M1 local core | groups, floating panes, status bar, palette, sidebar side, bottom tabs, copy-on-select, FTS archive, edit scrollback, native notifications, theme propagation, layout export | **M4** |
| M1 local core | synchronized input | **post-1.0** |
| M2 agents/harnesses | Claude, pi/omp, Codex adapters; interactions; cards; integrations; resume; policy basics | **M1** |
| M2 agents/harnesses | OpenCode, Gemini, ACP, custom manifests, self-report, screen manifests + drift, signed manifest channel, usage extraction | **M2** |
| M3 tasks + remote | git worktree tasks, ports, setup, async removal, advisory collisions | **M1** |
| M3 tasks + remote | jj workspaces | **M4** |
| M3 tasks + remote | best-of-N compare, split-into-task migration | **post-1.0** (best-of-N *launch* may land in M2 with containers) |
| M3 tasks + remote | SSH machines, bridge, remote clipboard/images | **M3** |
| M3 (13) sandbox + container levels | execution isolation | **M2** |
| M4 preview fabric | discovery, browser profile over SOCKS / proxy, remote scriptable browser, screenshots, `vibeke mcp` | **M3** |
| M4 (13) VM level | VMs, templates, warm pools | **M4** |
| M5 plugins + compat + QUIC | plugins, full public Herdr plugin/automation compatibility | **M5** (Windows: M6) |
| M5 plugins + compat + QUIC | QUIC / predictive echo, marketplace | **post-1.0** |
| M6 hardening/Windows/1.0 | as before | **M6** |
