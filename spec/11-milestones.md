# 11 — Milestones and build plan (Phase 1)

The value proof comes first. M1 is a narrow but daily-usable runtime whose purpose is to show that structured agents and native interactions beat the current setup (tmux-style multiplexer) on the maintainer's real workload ([00](00-vision-and-scope.md) §Success metrics). Parity polish comes later. Each milestone ships to the `preview` channel and is dogfooded daily. Exit gates are in [10-quality-performance-testing.md](10-quality-performance-testing.md) §10; feature acceptance criteria live in each section.

```
M0 spikes ─► M1 supervision slice ─► M2 safe yolo + harnesses ─► M3 remote + preview ─► M4 VMs + polish/parity ─► M5 compatibility + plugins ─► M6 hardening / Windows / 1.0
                    │
                    └─ gate: product metrics vs baseline; fail → re-evaluate (sidecar fallback, 00 §Alternatives)
```

No calendar estimates are given: the earlier week numbers were unsupported placeholders. Sizing is tracked per milestone once M0 has produced real measurements. **The previous setup stays installed throughout dogfooding** as the fallback; uninstalling it is not a goal.

## M0 — Spikes (de-risk the hard bets)

| Spike | Question | Output |
|---|---|---|
| VT engine (03 §2) | Which of libghostty-vt / wezterm-term / alacritty_terminal meets the **recovery requirement**: serialize + restore incl. parser mid-sequence state, modes, alt screen; continue from a byte offset; reflow; kitty keyboard; graphics. One engine is chosen and pinned; no second engine is kept compiling. | Scored rubric, chosen engine, `VtEngine` trait v0 |
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

- Execution isolation per [13](13-sandboxes-and-vms.md): `sandbox` level (Seatbelt on macOS; bubblewrap + Landlock + seccomp on Linux), `container` level (Apple `container`, OrbStack/Docker, Podman, Docker Sandboxes), host-side egress proxy with network profiles and egress Interactions, credential projection (Claude token, Codex auth, pi/omp env), `clone` code isolation + `task sync` via host-side fetch, devcontainer support, `--yolo` / `--isolate`. Dropped/pasted path translation into sandbox/container inboxes (06 A11, local namespaces; remote follows in M3).
- Harnesses: OpenCode, Gemini CLI, ACP-generic, custom harness manifests (`espi`, Hermes), Herdr-compatible self-report; screen detector manifests with golden corpus and nightly drift detection; signed manifest channel.
- Turns/items/usage/rate-limit extraction from adapters and transcripts.

## M3 — Remote + preview

- Saved SSH machines, no-sudo bootstrap with checksum, bridge multiplexing over SSH stdio, unified multi-machine sidebar, `--machine` forwarding (never falls back to local), reconnect/offline states, bandwidth budgets and adaptive frame rate, remote clipboard, image paste local → remote, **dropped/pasted path translation over the bridge** (06 A11).
- Previews: discovery (process-tree listening sockets + URLs in output + `vibeke preview declare`); **access via a dedicated browser profile over SOCKS through the bridge** so `localhost:5173` on the remote works unmodified (no Host rewriting); authenticated per-preview proxy origins as the alternative mode. Automatic forwarding of discovered ports is opt-in.
- Remote **scriptable** headless Chromium over CDP (navigate, click, type, eval, screenshot, console/network logs, DOM snapshot) exposed as CLI + `vibeke mcp`; navigation restricted to declared previews (redirects, subresources and WebSockets included).
- Screenshots as blobs with `environment_label`; `EvidenceRecord` groundwork (12): base/head sha, checks observed, artifacts.
- Human review minutes per accepted change baselined.

## M4 — VMs + polish/parity

- `vm` level (Lima `vz`/Tart on macOS, Firecracker/Cloud Hypervisor on Linux), template snapshots, warm pools; previews and screenshots verified inside containers and VMs.
- Parity polish: groups, floating panes, status bar (built-in segments), command palette, goto improvements, sidebar left/right, bottom tab bar, copy-on-select/PRIMARY, configurable copy-mode keys, archived-scrollback FTS search and edit-scrollback, native OS notifications with click-to-focus, theme auto light/dark + propagation, layout export/apply, jj workspaces.

## M5 — compat subset + plugins

- `vk-compat`: the Herdr socket subset existing clients call → existing socket clients run unmodified, giving an early phone decision surface before the Phase 2 gateway.
- Plugins: Herdr-compatible argv actions + `herdr-plugin.toml` import; long-running plugin processes with capabilities, UI contributions (status segments, sidebar sections, palette commands), KV storage, install consent, `vibeke plugin link` hot reload.

## M6 — Hardening, Windows, 1.0

- Windows host (ConPTY, named pipes, holder equivalent), Windows Terminal in the keyboard matrix.
- External security review; reproducible Linux builds; OSS-Fuzz; docs site; migration guide from Herdr; 1.0 API freeze (`vibeke/1`).

## Deferred beyond 1.0 (unless real demand appears)

QUIC roaming transport + predictive echo (mosh-style) · plugin marketplace · full Herdr API compatibility beyond the initial subset · policy learning (rule suggestions) · best-of-N comparison UI · automatic shared-cwd "split into task" migration · synchronized input · multiple graphics fallbacks beyond kitty graphics.

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
| VT engine can't meet the recovery requirement | M0 selects by demonstrated recovery behaviour; fallback is a weaker, honestly documented guarantee (process survival + forced redraw) |
| Harness vendors change hooks/UIs often | Structured channels first; capability matrix per version; golden + live drift CI; signed manifest channel |
| Codex shared daemon and vendor architecture shifts | PATH shim → per-pane embedded server; deterministic thread binding only (heuristic correlation never authorizes writes); screen fallback |
| The pinned compatibility baseline moves fast (weekly upstream releases) | Don't chase parity; compete on interactions, safe yolo, BYO harnesses and evidence; early compat bounded to a small subset |
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
| M5 plugins + compat + QUIC | plugins, compat subset | **M5** |
| M5 plugins + compat + QUIC | QUIC / predictive echo, marketplace, full Herdr compat | **post-1.0** |
| M6 hardening/Windows/1.0 | as before | **M6** |
