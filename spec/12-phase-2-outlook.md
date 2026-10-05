# 12 — Phase 2+ outlook (not in Phase 1 scope)

Phase 1 builds the runtime. This file records what comes next so Phase 1 decisions stay compatible. Nothing here is a Phase 1 requirement **except** the "Phase 1 must provide" column.

| Phase 2 capability | What it is | Phase 1 must provide |
|---|---|---|
| **Vibeke Gateway + mobile/web app** | `vibeke gateway` serves an installable PWA (later native shells) over Tailscale or an end-to-end-encrypted relay (QR key exchange, self-hostable relay, zero-knowledge — vendors' relays are not E2E). | Complete JSON-RPC API; durable event log with cursors; `Interaction` objects with native answer delivery; blob store; per-client capability tokens. |
| **Attention inbox** | One ranked list of everything that needs a human, across machines and vendors: ranked by blocking-ness × risk × wait time × dependents; batch identical approvals ("4 agents want `pnpm test`"); presence-aware notifications (desk → terminal, away → phone/watch). | `Interaction` with `risk`, fingerprints, `answered_by`; `notification` events with urgency; client presence (`client.attached`). |
| **Learned policy** | Suggest rules from repeated decisions; per-repo policy files reviewed like code. | Decision log (interaction fingerprint → decision) and the Phase 1 policy engine. |
| **Evidence bundles** | Each finished task arrives with diff summary, tests run, screenshots/preview link, reviewer-agent verdict and risk score → review by exception, phone-friendly. | `file_change` items, turn usage, preview screenshots as blobs, task ↔ run links. |
| **Merge orchestration** | Claims on files/areas, conflict prediction across live worktrees, merge queue, best-of-N comparison view. | Task workspaces with branches; collision tracker; best-of-N launch. |
| **Goal → plan → tasks** | Spec/plan approval gate; planner fans out to Claude/Codex/pi based on fit, cost and quota; "what happened while you were away" briefings. | Headless adapters (RPC/app-server/ACP) that can be driven programmatically; usage/rate-limit extraction. |
| **Quota & cost scheduling** | Pause low-priority runs near 5-hour/weekly limits, route work to the subscription with headroom. | `rate_limited` state with `resets_at`; per-turn usage. |
| **Sandboxed runners** | Run tasks in Docker Sandboxes / Apple `container` / microVMs; snapshot & fork a session at turn N; move a session laptop → cloud so it survives a closed lid. | `Runner` abstraction in tasks (05); event-sourced sessions; resume handles. |
| **Team mode** | Shared inbox, hand an agent to a colleague, ownership of stuck agents, audit. | Actor attribution on every event; per-user tokens. |
| **Native GUI client** (Phase 3) | Optional desktop app rendering the same render stream with rich previews. | Render stream is client-agnostic (cell grids + images), not tied to the TUI. |

**Strategic notes from the research (Oct 2026):**
- Basic remote control is commoditized by vendors (Claude Remote Control, Codex in ChatGPT mobile). Orchestration-only startups died (Terragon Feb 2026, Vibe Kanban Apr 2026); Omnara pivoted. Differentiation must be: vendor-neutral + custom harnesses, multi-machine, E2E-encrypted, and the supervision layer (inbox, evidence, merge) — not "chat with your agent from your phone".
- Human review time, not agent count, is the bottleneck. Measure *human minutes per merged change* as the north-star metric from Phase 2 on.
