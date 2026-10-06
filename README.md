# Vibeke

A terminal workspace that understands agents **structurally** (hooks, extensions, RPC) instead of by reading their screens, keeps every process alive through server crashes and upgrades, isolates agents in task workspaces, and makes remote dev servers, previews and screenshots feel local.

**Status:** specification, Phase 1 (terminal runtime). Phase 2 (mobile/web supervision) follows.

**Language:** Rust on the latest stable toolchain (edition 2024), single static binary. The terminal engine is Ghostty's libghostty-vt, vendored and statically linked (built with Zig 0.16, pinned in `mise.toml`). Apart from that, the only non-Rust code is the in-agent integration glue that must live in each harness's ecosystem (e.g. the TypeScript extension for pi/omp, hook config templates).

## Spec

| # | Section |
|---|---|
| 00 | [Vision and scope](spec/00-vision-and-scope.md) — landscape and design bets, differentiation, alternatives considered, success metrics, goals, non-goals |
| 01 | [Architecture](spec/01-architecture.md) — processes (server, per-pane holders, clients), protocols, crates, key decisions |
| 02 | [Data model and event log](spec/02-data-model-and-event-log.md) — entities, AgentState, Interaction, events, SQLite, policy |
| 03 | [Terminal engine and TUI](spec/03-terminal-engine-and-tui.md) — VtEngine, M0 engine spike, render stream, input fidelity, graphics, copy mode |
| 04 | [Harness adapters](spec/04-harness-adapters.md) — Claude Code, Codex, pi, omp, OpenCode, Gemini, ACP, custom harness manifests, detection, approvals |
| 05 | [Tasks, isolation and worktrees](spec/05-tasks-isolation-and-worktrees.md) — task workspaces, ports, setup, collision tracking, best-of-N |
| 06 | [Remote and preview](spec/06-remote-and-preview.md) — machines, SSH/QUIC bridge, port forwarding, preview proxy, remote screenshots |
| 07 | [API, CLI and plugins](spec/07-api-cli-plugins.md) — JSON-RPC catalog, CLI, holder protocol, plugins, full versioned Herdr plugin/automation compatibility |
| 08 | [UX, config and keybindings](spec/08-ux-config-and-keybindings.md) — sidebar, peek-and-reply, interaction overlay, **canonical config reference** |
| 09 | [Security and privacy](spec/09-security-and-privacy.md) — threat model, agent containment, plugins, previews, updates |
| 10 | [Quality, performance and testing](spec/10-quality-performance-testing.md) — budgets, chaos, keyboard matrix, golden tests, release |
| 11 | [Milestones](spec/11-milestones.md) — value proof first: M0 spikes → M1 supervision slice → M2 safe yolo → M3 remote + preview → M4 VMs + parity → M5 compat + plugins → M6 1.0 |
| 12 | [Phase 2 outlook](spec/12-phase-2-outlook.md) — EvidenceRecord, inbox, merge, mobile; what Phase 1 must provide |
| 13 | [Sandboxes and VMs](spec/13-sandboxes-and-vms.md) — host / OS sandbox / container / VM execution, safe yolo, egress proxy, credentials, git boundary |
| 14 | [LLM assistance](spec/14-llm-assistance.md) — optional genai integration, provider/model selection, grounded briefings, context/privacy boundaries and staged rollout |
| 15 | [Task outcomes, review and attention](spec/15-task-outcomes-review-and-attention.md) — optional tracking for normally launched CLIs, explicit success criteria, evidence-backed review and a ranked decision inbox; proposed slice after Goal 01 |
| 16 | [Gateway, relay and apps](spec/16-gateway-relay-and-apps.md) — no-Tailscale E2E relay, QR pairing (Noise), gateway, PWA/Electron apps with inbox, quick approvals and push; staged SaaS, share/handoff and zero-knowledge services |

Also: [integrations/pi-extension/DESIGN.md](integrations/pi-extension/DESIGN.md) · design review · design review
