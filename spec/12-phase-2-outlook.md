# 12 — Phase 2+ outlook (not in Phase 1 scope)

Phase 1 builds the runtime. This file records what comes next so Phase 1 decisions stay compatible. Nothing here is a Phase 1 requirement **except** the "Phase 1 must provide" column.

[15 — Task outcomes, review and attention](15-task-outcomes-review-and-attention.md) now specifies a proposed desktop/API slice for the attention and evidence capabilities below, including optional tracking of manually launched harnesses. It can follow Goal 01 without waiting for the mobile gateway. Its stronger evidence binding, readiness and acceptance rules extend the groundwork here; the existing M1 interaction list evolves in place into this richer inbox, with delivery still staged.

| Phase 2 capability | What it is | Phase 1 must provide |
|---|---|---|
| **Vibeke Gateway + mobile/web app** (specified in [16](16-gateway-relay-and-apps.md)) | `vibeke gateway` serves an installable PWA (later native shells) over Tailscale or an end-to-end-encrypted relay (QR key exchange, self-hostable relay, zero-knowledge — vendors' relays are not E2E). | Complete JSON-RPC API; event outbox with cursors; `Interaction` objects with native answer delivery and delivery states; blob store; per-client capability tokens. **May be pulled forward:** a minimal phone decision surface (see attention, answer, peek) by running an existing phone companion against the M5 Herdr compatibility layer, before the gateway exists. |
| **Attention inbox** | One ranked list of everything that needs a human, across machines and vendors: ranked by blocking-ness × risk × wait time × dependents; batch identical approvals ("4 agents want `pnpm test`"); presence-aware notifications (desk → terminal, away → phone/watch). | `Interaction` with `risk`, fingerprints, `answered_by`; `notification` events with urgency; client presence (`client.attached`). |
| **Learned policy** | Suggest rules from repeated decisions; per-repo policy files reviewed like code. | Decision log (interaction fingerprint → decision) and the Phase 1 policy engine. |
| **Evidence bundles** (the Phase 2 core) | Each finished task arrives with one or more `EvidenceRecord`s (below) plus a reviewer-agent verdict and risk score → review by exception, phone-friendly. | `file_change` items, turn usage, screenshots as blobs **with environment labels**, task ↔ run links, recorded check commands; `EvidenceRecord` groundwork in M3. |
| **Merge orchestration** | Claims on files/areas, conflict prediction across live worktrees, merge queue, best-of-N comparison view, automatic shared-cwd "split into task" migration. | Task workspaces with branches; advisory collision tracker. (best-of-N comparison and split-into-task are post-1.0.) |
| **Goal → plan → tasks** | Spec/plan approval gate; planner fans out to Claude/Codex/pi based on fit, cost and quota; "what happened while you were away" briefings. | Headless adapters (RPC/app-server/ACP) that can be driven programmatically; usage/rate-limit extraction. |
| **Quota & cost scheduling** | Pause low-priority runs near 5-hour/weekly limits, route work to the subscription with headroom. | `rate_limited` state with `resets_at`; per-turn usage. |
| **Cloud runners & session mobility** | Local sandbox/container/VM levels ship in Phase 1 (13). Phase 2 adds cloud providers (E2B, Daytona, Modal, Morph, Docker Cloud Sandboxes), snapshot & fork at turn N, and moving a session laptop → cloud so it survives a closed lid. | `Runner` provider trait (05/13); resume handles; per-run history in the event outbox. |
| **Team mode** | Shared inbox, hand an agent to a colleague, ownership of stuck agents, audit. | Actor attribution on every event; per-user tokens. |
| **Native GUI client** (Phase 3) | Optional desktop app rendering the same render stream with rich previews. | Render stream is client-agnostic (cell grids + images), not tied to the TUI. |

## EvidenceRecord (Phase 2 core; groundwork in M3)

A screenshot linked to a turn doesn't prove which code it shows. Evidence is tied to an exact revision and environment:

```
EvidenceRecord {
  id, task_id, run_id?, created_at,
  base_sha,                    # merge-base / task base_ref resolved to a commit
  head_sha,                    # commit the evidence was produced against
  dirty_digest?,               # blake3 over `git diff HEAD` + untracked file list/contents when the tree was dirty; null when clean
  checks: [ { cmd, cwd, env_digest, runner: {level: host|sandbox|container|vm, machine}, exit_code, duration_ms,
              started_at, stdout_ref?, stderr_ref?, junit_ref? } ],
  artifacts: [ { kind: screenshot|dom_snapshot|console_log|network_log|coverage|diff_summary|other, blob, meta } ],
  environment_label,           # e.g. "remote headless chromium @devbox, origin http://localhost:5173, fresh profile" vs "local browser profile via SOCKS"
  provenance: { produced_by: agent|vibeke|user|reviewer, harness?, model?, tool_call_id?, signed_by? },
  supersedes?: evidence_id
}
```

Rules: evidence whose `head_sha`/`dirty_digest` doesn't match the task's current state is shown as **stale**; checks are only "run" if Vibeke observed the process (pane/runner) or the adapter reported the exact tool call — an agent claiming "tests pass" in prose is not evidence. Screenshots always carry `environment_label` so a remote headless browser is never mistaken for the human's own view.

**Strategic notes from the research (Oct 2026):**
- Basic remote control is commoditized by vendors (Claude Remote Control, Codex in ChatGPT mobile). Orchestration-only startups died (Terragon Feb 2026, Vibe Kanban Apr 2026); Omnara pivoted. Differentiation must be: vendor-neutral + custom harnesses, E2E-encrypted, safe-yolo isolation, and the supervision layer (multi-machine alone is table stakes) (inbox, evidence, merge) — not "chat with your agent from your phone".
- Human review time, not agent count, is the bottleneck. *Human review minutes per accepted change* is the north-star metric; it is baselined from M3 (00 §Success metrics).
- Other tools' roadmaps (E2E relays, cross-machine agent collaboration, session mobility) overlap the gateway/relay; our edge must be interactions, isolation, BYO harnesses and evidence, not the relay itself.
