# 11 — Milestones and build plan (Phase 1)

Each milestone is shippable to the `preview` channel and dogfooded daily. Exit gates are defined in [10-quality-performance-testing.md](10-quality-performance-testing.md) §10; feature acceptance criteria live in each section. Estimates assume 2 strong Rust engineers plus heavy agent-assisted development; treat them as relative sizing, not commitments.

```
M0 spikes ──► M1 local core ──► M2 agents/harnesses ──► M3 tasks + remote ──► M4 preview fabric ──► M5 plugins + compat + QUIC ──► M6 hardening / Windows / 1.0
  ~2 wk          ~6 wk              ~5 wk                    ~5 wk                  ~4 wk                    ~5 wk                            ~5 wk
```
Dogfooding starts at the end of M1 (Vibeke is used on the maintainer's machine for non-agent work) and becomes total at the end of M2.

## M0 — Spikes (de-risk the three hard bets)

| Spike | Question | Output |
|---|---|---|
| VT engine (03 §2) | libghostty-vt vs wezterm-term vs alacritty_terminal | Scored rubric, chosen engine, `VtEngine` trait v0 merged |
| Holder durability (01 §1.2) | Can a holder + snapshot/replay restore panes exactly after `kill -9` of the server? | `vk-hold` prototype, 100× chaos loop with zero process loss |
| Input latency | Can client-side keymap + server render stream stay ≤ 3 ms p99 added latency? | Measured prototype on Ghostty + Kitty |
| Harness reality check (04 [verify M2] list) | Run each **[verify M2]** item against live binaries: AskUserQuestion via hook, Codex `--disable daemon_auto_start`, omp tool_call vs approval ordering, OpenCode `permission.ask`, Gemini confirmations, omp pre-assigned session ids | Verified/adjusted table in 04 |

## M1 — Local core (a better tmux without agent smarts)

- `vk-proto`, `vk-hold`, `vk-term`, `vk-store`, `vk-server`, `vk-tui`, `vk-cli`, `vibeke` binary.
- Sessions (named), workspaces, groups, tabs (numbers + titles), panes, splits, floating panes, zoom, resize mode, navigate mode, command palette, fuzzy goto, sidebar (left/right), tab bar (top/bottom), status bar.
- Holders + snapshot/replay recovery; server auto-restart; `vibeke update` is a server restart, not a handoff.
- Event log + subscriptions with cursors; JSON-RPC API for all M1 methods; CLI mirror; `--skill` v1.
- Input fidelity: kitty keyboard / CSI-u / modifyOtherKeys, bracketed paste, mouse, focus; OSC 7/8/52/133/9/777; kitty graphics passthrough; theme propagation.
- Copy mode with `/` search, copy-on-select option; unlimited scrollback archive + FTS search; edit scrollback in `$EDITOR`.
- Notifications (bell/OSC 9/777 → OS notification, click-to-focus), mark unread, pin.
- Config + hot reload; Herdr config importer.
- macOS signed helper identity for the server (TCC).

## M2 — Agents and harnesses (the core differentiator)

- `vk-agents`: adapter trait, harness manifests, process detection (nix/node/bun shims), screen detector engine + manifests, self-report protocol.
- First-class adapters: **Claude Code** (full hooks + transcript), **Codex** (hooks + app-server observer; shared-daemon handling), **pi** and **omp** (`@vibeke/pi-extension`, plus RPC headless mode), **OpenCode**, **Gemini CLI**, **ACP-generic**, **custom manifest** (e.g. `espi`, Hermes).
- `AgentState` with source/confidence; `Interaction` objects; native answer delivery + verified-keystroke fallback; interaction overlay in the TUI; batch answer.
- Policy engine (allow/deny/ask rules, repo policy requires trust); decision log for Phase 2 learning.
- `agent start|prompt|wait|read|send-keys|get|list`, names, resume after reboot via resume argv.
- `vibeke integration install|status|uninstall|doctor` (coexists with Herdr hooks).
- Turns/items/usage/rate-limit extraction from adapters and transcripts.
- Golden corpus per harness version + nightly drift detection; signed manifest channel.

## M3 — Tasks and remote

- `vk-tasks`: `task new` with git worktree / jj workspace, env copy/clone strategies, setup scripts, port leases, async removal, cleanup policies, branch/PR status in sidebar.
- Collision tracker for shared-cwd agents; advisory claims; "split into task"; best-of-N task families (`--agents claude:2,codex:1`) + `task compare` (text-level).
- `Runner` trait (local + ssh implementations) **plus** execution isolation per [13](13-sandboxes-and-vms.md): `sandbox` level (Seatbelt / bwrap+Landlock), egress proxy + network profiles, `container` level (Apple `container`, OrbStack/Docker, Podman), `clone` code isolation + `task sync`, devcontainers, credential projection, `--yolo`.
- `vk-remote`: saved machines, no-sudo bootstrap with checksum, bridge multiplexing over SSH stdio, unified multi-machine sidebar, `--machine` forwarding (never falls back to local), reconnect/offline states, bandwidth budgets + adaptive frame rate, OSC 52 clipboard, image paste local→remote.

## M4 — Preview fabric

- `vk-preview`: port discovery (process tree sockets + output URL detection + declare), forwarding through the bridge, local reverse proxy with per-preview `*.vibeke.localhost` origins, header rewriting, WebSocket/HMR/SSE, HTTPS upstreams, mirror mode for hard-coded ports, DNS-rebinding protection.
- Headless Chromium over CDP on the dev-server machine: screenshots, console/network errors, DOM snapshot, visual diff.
- `vm` execution level (Lima vz/Tart, Firecracker/Cloud Hypervisor), template snapshots, warm pools, forked best-of-N VMs (13 §9).
- `vibeke preview …`, `vibeke browser …`, and `vibeke mcp` so agents screenshot their own work; inline screenshots via kitty graphics; screenshots stored as blobs with events.

## M5 — Plugins, Herdr compatibility, QUIC

- `vk-plugins`: Herdr-compatible argv actions + `herdr-plugin.toml` import; long-running plugin processes with capabilities, UI contributions, KV storage; install consent; `vibeke plugin link` hot reload; marketplace index from GitHub topic `vibeke-plugin`.
- `vk-compat`: Herdr compat socket covering everything existing socket clients call → **existing socket clients run unmodified on Vibeke** (bridge to Phase 2).
- QUIC roaming transport with predictive local echo (mosh-style).

## M6 — Hardening, Windows, 1.0

- Windows host (ConPTY, named pipes, holder equivalent), Windows Terminal in the keyboard matrix.
- External security review; reproducible Linux builds; OSS-Fuzz; docs site; migration guide from Herdr; 1.0 API freeze (`vibeke/1`).

## Suggested first two weeks (M0 kickoff)

1. Repo scaffold: Cargo workspace per 01 §6, `rust-toolchain.toml` on latest stable, CI (fmt, clippy `-D warnings`, nextest, cargo-deny), Renovate for toolchain bumps.
2. `vk-proto` v0: IDs, event envelope, holder protocol messages.
3. `vk-hold` prototype + chaos loop script.
4. Three VT engine bindings behind `VtEngine`, benchmark + esctest harness.
5. Harness reality-check script that launches each installed agent (claude, codex, pi, omp) in a PTY and records hooks/extension events to a JSONL trace, which becomes the first golden fixtures.

## Top risks

| Risk | Mitigation |
|---|---|
| libghostty-vt API instability / wezterm-term not on crates.io | `VtEngine` trait; vendored pin; spike decides; keep a second engine compiling in CI |
| Harness vendors change hooks/UIs often | Structured channels first; golden + live drift CI; signed manifest channel ships fixes without a release |
| Codex shared daemon and other vendor architecture shifts | Observer adapter on app-server socket; per-pane server option; screen fallback |
| The pinned compatibility baseline moves fast (weekly upstream releases) | Don't chase parity forever: compat layer is bounded; compete on durability, structured agents, tasks, remote preview, and Phase 2 |
| Scope (this is a lot) | Ruthless milestone gating; M2 is the value proof — if structured adapters don't beat screen scraping clearly, revisit |
