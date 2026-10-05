# Vibeke

A terminal workspace that understands agents **structurally** (hooks, extensions, RPC) instead of by reading their screens, keeps every process alive through server crashes and upgrades, isolates agents in task workspaces, and makes remote dev servers, previews and screenshots feel local.

**Status:** specification, Phase 1 (terminal runtime). Phase 2 (mobile/web supervision) follows.

**Language:** Rust on the latest stable toolchain (edition 2024), single static binary. The only non-Rust code is the in-agent integration glue that must live in each harness's ecosystem (e.g. the TypeScript extension for pi/omp, hook config templates).

## Spec

| # | Section |
|---|---|
| 00 | [Vision and scope](spec/00-vision-and-scope.md) — landscape and design bets, goals, non-goals, glossary |
| 01 | [Architecture](spec/01-architecture.md) — processes (server, per-pane holders, clients), protocols, crates, key decisions |
| 02 | [Data model and event log](spec/02-data-model-and-event-log.md) — entities, AgentState, Interaction, events, SQLite, policy |
| 03 | [Terminal engine and TUI](spec/03-terminal-engine-and-tui.md) — VtEngine, M0 engine spike, render stream, input fidelity, graphics, copy mode |
| 04 | [Harness adapters](spec/04-harness-adapters.md) — Claude Code, Codex, pi, omp, OpenCode, Gemini, ACP, custom harness manifests, detection, approvals |
| 05 | [Tasks, isolation and worktrees](spec/05-tasks-isolation-and-worktrees.md) — task workspaces, ports, setup, collision tracking, best-of-N |
| 06 | [Remote and preview](spec/06-remote-and-preview.md) — machines, SSH/QUIC bridge, port forwarding, preview proxy, remote screenshots |
| 07 | [API, CLI and plugins](spec/07-api-cli-plugins.md) — JSON-RPC catalog, CLI, holder protocol, plugins, Herdr compatibility |
| 08 | [UX, config and keybindings](spec/08-ux-config-and-keybindings.md) — sidebar, palette, interaction overlay, config schema |
| 09 | [Security and privacy](spec/09-security-and-privacy.md) — threat model, agent containment, plugins, previews, updates |
| 10 | [Quality, performance and testing](spec/10-quality-performance-testing.md) — budgets, chaos, keyboard matrix, golden tests, release |
| 11 | [Milestones](spec/11-milestones.md) — M0–M6 build plan and risks |
| 13 | [Sandboxes and VMs](spec/13-sandboxes-and-vms.md) — host / OS sandbox / container / VM execution, safe yolo, egress proxy, credentials, git boundary |
| 12 | [Phase 2 outlook](spec/12-phase-2-outlook.md) — inbox, evidence, merge, mobile; what Phase 1 must provide |

Also: [integrations/pi-extension/DESIGN.md](integrations/pi-extension/DESIGN.md) · design review
