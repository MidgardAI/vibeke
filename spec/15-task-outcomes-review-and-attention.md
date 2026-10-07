# 15 — Task outcomes, review packages and the attention inbox

**Status:** proposed product specification, based on the accepted UX direction of 2026-10-06. This specifies an additional product slice; it does not expand [milestones](11-milestones.md). The current SSH daily-driver goal remains first. Delivery stages in §13 are separate from the Phase 1 milestones. *Implementation:* T1–T3 are built ([task outcomes and review](15-task-outcomes-review-and-attention.md)); T4's server/API/CLI parts are built with the limits in the *As built* notes and the T4 status under §13.

**Product promise:** launch `claude`, `codex` or another supported harness normally, optionally track the work, and return to a clear account of what needs a decision and what has been demonstrated.

Related: [02 data model](02-data-model-and-event-log.md), [04 adapters](04-harness-adapters.md), [05 tasks](05-tasks-isolation-and-worktrees.md), [06 previews](06-remote-and-preview.md), [07 API](07-api-cli-plugins.md), [08 UX](08-ux-config-and-keybindings.md), [09 security](09-security-and-privacy.md), [10 quality](10-quality-performance-testing.md), [12 Phase 2](12-phase-2-outlook.md), [14 LLM assistance](14-llm-assistance.md).

## 1. Scope and principles

Three improvements form one workflow:

1. **Task intent:** record the outcome, constraints, acceptance criteria and requested stopping point independently of a harness session.
2. **Review package:** connect the current change to that intent and to observed, revision-bound evidence; expose missing verification and decisions.
3. **Attention inbox:** help the user choose which decisions to handle across tasks, harnesses and machines.

The default desktop experience has three surfaces: the harness's own pane, task details, and the inbox. Task details and the inbox are explicitly opened views, not another permanent layer of chrome.

### 1.1 Invariants

- **The focused pane belongs to the harness.** Tracking, suggestions, state changes, check results and inbox arrivals never take focus, overlay its TUI, inject a prompt or write PTY bytes. Existing prefix keys and user-invoked surfaces follow 08.
- **Launching through Vibeke is optional.** A directly typed CLI inside a Vibeke pane has the same supervision capabilities as `agent.start` when its identity is deterministically bound and the same tested integration is active. A guessed/shared-daemon association does not provide that parity.
- **Tracking is optional.** Untracked agents retain status, interactions, peek/reply, branch/diff status and an observed-command panel derived from their existing Turn/Item records. These observations can lack a bound code subject and never imply criterion satisfaction. The user can ignore task tracking indefinitely.
- **Intent is explicit.** Extracted requirements and suggested checks are drafts until the user confirms them. Saving a task is not sending a message, granting permissions, changing a checkout, or authorizing checks.
- **A finished turn is not a finished task.** Process state, turn completion, read state, verification and human acceptance are separate.
- **Evidence has limits.** An observed successful command demonstrates that command's result on its recorded subject; it does not prove every requirement. Agent and reviewer opinions remain attributed opinions.
- **No cloud-model dependency.** Manual intent entry, deterministic review contents, ranking and interaction answering work with AI assistance disabled. Optional generation follows all grants, provenance and budgets in 14.
- **One control path.** Interactions reuse 04's delivery transaction. Follow-ups reuse the adapter capability model. The assistant has no new authority to answer, send, execute, accept or merge.

### 1.2 Out of scope

This slice does not add an autonomous planner, automatic harness switching, merge/deployment orchestration, team assignment, a mobile client, a new permission system, or migration of a running process into a new worktree. It prepares stable objects/APIs for future clients. Browser evidence uses 06 when available; browser support is not a prerequisite for basic review.

## 2. Primary journey: a manually launched CLI

### 2.1 Work normally

Inside a Vibeke pane the user runs:

```sh
cd my-project
claude
```

They send this directly to the harness:

> Fix the login redirect. After login, users should return to their original page. Preserve SSO. Open a draft PR when ready.

With the integration installed and identity bound per 04, the sidebar shows the normal agent row. Detection does not create a task or call an LLM. Integrations are installed through the existing setup flow; task tracking never edits harness configuration implicitly.

### 2.2 Track when useful

The agent's sidebar context menu and peek expose **Track this work**. The user selects the request or a bounded range of turns; the latest user message is initially selected when available. Optional **Suggest task details** invokes 14 against that selection, not the entire conversation.

```text
Track work in this session

Title       Fix login redirect
Goal        Return users to their original page after login

From your selected request:
  • Preserve SSO behavior
  • Deliver a draft PR

Suggested verification — select what to require:
  [ ] Redirect regression test
  [ ] SSO smoke test

Workspace   Current checkout · existing files and permissions
Review base Proposed merge-base · inspect full diff before confirming

[Track task]  [Edit]  [Cancel]
```

One confirmation saves the intent. Suggested checks are not silently made mandatory; the user can select them or rely on an already trusted project recipe. The goal and constraints themselves are still criteria requiring appropriate evidence or human judgment. Users can always type the fields manually.

The example above requires **Suggest task details**. T1's default form instead shows the selected request verbatim, uses its first line as an editable title, and provides optional criteria fields plus trusted recipe choices. It performs no semantic extraction. The stopping-point field begins **Not specified** unless the user explicitly sets it; the verbatim request remains visible, including any instruction such as "draft only". Structured readiness requires confirmed criterion/stop mappings; incomplete mappings stay **Needs task details**. This avoids a default value contradicting the user's original request.

Tracking creates a record and run binding. It does not replay the original request, stop the process, relocate files, create a branch or mark existing work as owned by this agent. **New isolated task** is a separate command using 05's existing creation flow.

### 2.3 Clarify intentionally

Editing task intent opens a draft. **Save details** records a new revision. If the change affects what the agent should do, show **Save and send clarification**, with the exact message and recipient. Record-only edits show a persistent **Agent has not been told about revision N** label until an explicit message is delivered or the user links an already-sent instruction.

Saving remains useful if sending fails. The UI reports the saved intent and delivery result separately. No automatic retransmission occurs after an ambiguous result (§9).

For revision 1, requirements quoted from a user message already delivered to this exact conversation count as already communicated. Manually added requirements and suggested checks do not. Store communication coverage per requirement/version; a task-level label summarizes any uncovered requirements. A linked old conversation or an adapter report of receipt is not proof of understanding.

### 2.4 Handle a decision while working elsewhere

The user focuses a Codex pane in another project. Claude opens a question. A sidebar badge appears; Codex keeps its focus. `prefix+i` opens the inbox, replacing the pane area until dismissed:

```text
Needs you                                        [5-minute view]

1. Fix login redirect · Claude
   Product decision · waiting 8m · blocks this run

2. Fix invoice export · Codex
   Review available · one required check missing

Selected: Fix login redirect
Claude asks:
  If the destination is no longer accessible, use the
  dashboard or show an error?

Task context: Return to the original page; preserve SSO.
[Dashboard] [Show an error] [Write reply] [Open pane]
```

Options come from the live Interaction, not generated suggestions. Native answering is offered only for a tested capability. Observe-only targets offer **Open pane**. Explicitly enabled keystroke fallback stays labeled best-effort. Answering shows recorded, delivering, delivered or unconfirmed status; it never equates a click with confirmed delivery.

### 2.5 Review a result

A completed turn keeps 08's familiar **✓ done** attention marker (tooltip: **Turn finished**). Tracked task readiness appears as a separate label. Task details are available even while the agent works; an idle signal alone never says the task is ready.

```text
Fix login redirect                         Review available
Draft PR #123 · revision abc123 · checked 2m ago

Changes
  Preserves the destination through login and validates
  it before redirecting. [Source: diff]

Requirements
  PASS     Return to original page
           Vibeke verification passed on commit abc123
  MISSING  Preserve SSO
           Agent claims compatibility; no matching SSO check
  PASS     Deliver a draft PR
           PR #123 is a draft for this branch and revision

[Inspect diff] [Run missing check] [Ask Claude] [Mark reviewed]
```

This is an example of content, not a promise that a model can infer a valid regression test. Requirements can be linked to project checks or manually reviewed evidence. **Run missing check** is available only for a defined, authorized check; otherwise offer **Choose verification** or **Ask agent to verify**.

### 2.6 Request changes or accept

**Ask Claude** opens an editable message, for example "Add coverage for an expired SSO session before I review this." It targets the original run after a live identity/capability check. The user's send action is required.

New code makes old evidence and acceptance stale as described in §7. **Mark reviewed** records an acceptance for the exact displayed intent revision and change subject. Missing required checks are shown explicitly; a user may accept with a recorded exception rather than fabricate a pass. This neither merges a PR nor finishes/removes its workspace. **Finish task** is a separate lifecycle action.

In initial T2, formal acceptance requires a committed candidate. A dirty checkout remains inspectable and annotatable; the action explains **Select a committed revision to record acceptance**. Vibeke never commits the user's work automatically. Later stable dirty-subject capture can extend acceptance without changing its meaning.

## 3. Entry modes and graceful degradation

| Situation | Available experience | Limit or fallback |
|---|---|---|
| User types `claude`/`codex` inside a Vibeke pane | Detection, optional tracking and full supported integration | Actual capability is harness × version × mode × interaction kind, per 04 |
| `vibeke task new` launches a harness | Same task details/inbox; can record intent before launch | Confirmed initial prompt is sent once through the existing launch flow |
| User already has an existing worktree | Adopt it and optionally bind its run | Adoption does not move files or assume ownership of preexisting changes |
| Structured state, but no readable transcript | State and actual Interactions; manual intent | No synthesized task request or claimed complete conversation history |
| Suggested/unlinked run identity | Inferred display and manual workspace notes | **Link run** first; `task.track`/`task.bind` refuse run attachment until the deterministic identity is verified per 04 |
| Shared checkout with multiple active writers | Checkout diff or explicitly selected commit-range review | No live-task Ready label; offer **Start a fresh task from here** for isolated work; stable commit verification remains inspectable |
| Screen-only or unvalidated integration | Inferred status, manual task details, user-triggered filesystem review | No automatic readiness from prose/screens; answer capabilities remain those of 04 |
| AI disabled, unavailable or not permitted for this workspace | Manual intent, exact diff/check tables, deterministic inbox | No automatic summary/extraction; terminal and answering remain usable |
| Remote machine offline | Last observed snapshot with age and offline badge | No sends, checks, acceptance or finishing against unverifiable live state; local drafts remain editable |
| CLI already running outside Vibeke | Document supported integration/import/resume options | No promise to seize an arbitrary process's PTY or retroactively recover unobserved evidence |

## 4. Task intent and session boundaries

### 4.1 Intent revision

A confirmed `TaskIntent` contains:

```text
TaskIntent {
  task_id, revision, title, objective,
  constraints: [{id, text, source_refs}],
  criteria: [{id, text, required, evaluation: check|human|external,
              check_definition_ids?, source_refs}],
  stop_at: unspecified|implementation|draft_pr|reviewed_pr|merge|verified_deployment|custom,
  stop_detail?, source_refs, confirmed_by, confirmed_at
}
```

- `source_refs` identify machine/session/run/turn/item and, where applicable, source digests. Handwritten requirements have user provenance. A generated draft also stores its assistant result reference.
- Keep a bounded copy of the user-selected source excerpt with the intent, under the same content scope and purge policy. This preserves inspectability after ordinary transcript/item compaction without retaining unrelated conversation text. A purged excerpt becomes unavailable; it is never restored from a cache.
- `stop_at` records intent. It cannot grant or enforce merge/deploy permissions. Unsupported external outcome verification remains manual/unknown and the UI says so.
- `unspecified` initially means the stopping point has not been mapped. The user can explicitly choose **No separate delivery outcome to verify**, confirming that value while leaving the original request and its constraints intact; final outcome assessment then requires human judgment. An unconfirmed default cannot make readiness pass.
- Every criterion has a stable ID; changed meaning creates a new criterion version. Optional checks never block readiness. Constraints requiring judgment become explicit human criteria rather than disappearing into a summary.
- Extraction preserves scope and negatives such as "draft PR only". If the source is ambiguous, show an editable question; do not silently choose the more permissive interpretation.
- Changes create immutable intent revisions. Human acceptance of an older revision is retained as history and cannot satisfy the new one automatically.

### 4.2 A task and a session have different lifetimes

One task can involve several runs. One long-running CLI session can work on several successive tasks. New prompts usually refine the current task; they do not automatically create one task per turn.

**Start another task in this session** lets the user close the current binding and select the next request. A default binding switch takes effect at the next turn boundary. While a turn is running, a switch is queued and shown as pending; it does not reassign that turn's checks/interactions. Historical turn-range edits require explicit selection and invalidate affected derived reviews. Overlapping foreground-task bindings for the same turn are refused.

Closing a binding pins its consistently captured end-boundary candidate, if one exists. In T2 this is a committed candidate; if only dirty/unbound work exists, record **No bound end candidate** and let the user choose an explicit committed range later. Once the run moves to task B, task A's review never follows the checkout's live diff automatically or absorbs B's subsequent edits. Historical observations remain available with their original limits.

Boundaries follow native conversation identity, not the command's name. `/clear`, `/new`, a fork, or `/resume` to a different conversation suspends automatic association even where 04 preserves the `AgentRun` ID; offer **Continue task** or **Track new work**. Compaction and resume of the same conversation are not new boundaries. Pending interactions retain the association they had when opened.

Vibeke-controlled reboot/task resume transfers the binding to a new run when the verified resume handle, harness/native session identity, repository and owner match and the old run is no longer active. An observed same-session restart can do so under the same checks. Record a continuation edge rather than rewriting history. Concurrent resumes or multiple possible predecessors require explicit resolution; cwd/title/prompt similarity is insufficient. Imported sessions without a recorded matching binding need an explicit link.

Subagents can inherit the parent task binding only through a structured parent identity and the parent's binding at spawn. Ambiguous work remains unassigned. User-selected linking is always available.

### 4.3 Workspace ownership and lifecycle

Add `workspace_ownership = owned | attached`:

- `owned`: a workspace/checkout created for the task by `task new`; 05's park/archive/remove behavior applies, with its dirty/unpushed protections.
- `attached`: tracking work in an existing pane/workspace. Archiving/removing this task record never stops its agent, deletes its checkout, releases another task's ports, or closes the workspace. **Stop agent** is a separate explicit action. `task park` parks the record only; it shows that the process continues.

Several attached tasks can refer to one workspace over time. `Workspace.task_id` remains the optional owning task, not a universal task lookup. `AgentRun.task_id` becomes a compatibility projection of its current binding; the binding history is authoritative for task attribution.

Execution-resource identity remains separate and stable: inherited `VIBEKE_TASK_ID`, pane-token/broker scope, ports, preview ownership and existing screenshot `CodeState.task` refer to the workspace's owning execution task, if any. Switching an attached binding changes none of these. The server associates observations with a tracking task through the event's verified run/turn binding and stores both identities; an agent cannot widen resource scope by supplying another tracking task ID.

Task lifecycle remains `active | parked | finished | archived`. Tracking status, review readiness and acceptance are independent. `task.finish` may finish without acceptance, but must record **Finished without review**; automation must not interpret lifecycle `finished` as verified success. Readiness or acceptance never triggers cleanup.

When an owned workspace is archived/removed under 05, enumerate attached tasks that reference it in the cleanup impact/confirmation. After deletion, mark those tasks' live source unavailable; their retained immutable candidates remain historical and follow normal retention. An attached record does not secretly prevent explicitly authorized owner cleanup, and cleanup cannot present its dependent live reviews as current afterward.

## 5. Change subjects and attribution

Keep **review base** and **observation baseline** separate. The review base defines the diff the user wants to inspect; the observation baseline records what existed when tracking began and does not prove who made it.

For an owned task, propose its recorded resolved base. For an attached task, propose the merge-base with the user-selected target branch, or with `Workspace.repo.default_branch` when configured and locally resolvable; otherwise fall back to current `HEAD`. Show the choice and full diff before confirmation. This slice does not add a historical turn-start commit recorder. Do not reconstruct a historical commit from timestamps/file-change claims or silently fetch to choose a different base. Tracking-time dirty content is not the default review base: edits already made after the prompt must remain visible.

Capture an observation baseline immediately: repository identity, resolved head, complete staged/unstaged/untracked change digest, observation time and selected source range. If earlier authorship is unknown, label **May include preexisting changes** and offer an explicit base or selected patch. Warn that the HEAD fallback omits earlier committed work. Repositories without usable VCS can track intent/interactions and manual review; automatic revision-based readiness requires a supported captured subject.

`ChangeSubject` is an immutable, content-addressed description of what is under review: repository identity, base SHA, head SHA, dirty digest when applicable, selected patch/snapshot digest, and capture metadata. Full dirty capture includes staged, unstaged, binary and untracked content; ignored runtime inputs relevant to verification belong in the environment manifest. Missing required capture data yields `unbound` evidence, not an empty/clean digest.

For a shared checkout, label the view **Changes in this checkout**. File events provide hints only. A user-selected patch defines review scope, but checks against the whole checkout retain that whole-checkout subject and are not presented as proof the selected patch passes independently. An isolated verification snapshot can establish that separately. With multiple active writers, live-task readiness is unavailable; users can inspect/accept an explicit immutable commit-range candidate as historical work or start a fresh isolated task through 05. Acceptance of selected commits never claims the moving checkout is reviewed.

Never run a destructive Git operation to obtain a baseline. Background capture is bounded and off the terminal/state-actor hot path. If consistent capture cannot be established while writers are active, show **Workspace changing — verification subject unavailable** and let the user select a stable commit or request a snapshot.

*As built (T4, `vk-review::snapshot`, `task.review.snapshot`):* a **dirty snapshot** captures the checkout's uncommitted content — staged, unstaged and untracked files, binary included, with the executable bit; ignored files are excluded — into an immutable Git commit. **Submodules and nested repositories are not captured**: Git records a submodule only as its commit id, so when any submodule (or a nested repository Git lists as untracked) has a moved gitlink or modified/untracked content, capture is refused with `conflict{reason: unsupported_capture, details: {unsupported: "submodule", paths, offers}}` instead of producing a snapshot that silently omits that work. The user's index is copied to a private temporary file (its mtime preserved so Git's racy-entry rules still apply); the staged tree is written from the copy, then `git add -A` into the copy and `write-tree` give the full working-tree tree; `commit-tree` with a fixed author/committer/date (identical content ⇒ identical commit ⇒ identical subject id) records it with HEAD as parent, and `refs/vibeke/snapshots/<commit>` keeps it reachable. The user's index, worktree and branch refs are never touched (tested byte-for-byte); the snapshot refs are the one thing capture adds to the repository. They are collected when nothing references them any more (kept: accepted snapshots, an open task's latest snapshot, snapshots under an active reviewer request or a running check): after each capture, in a background pass at server start, and on demand with `task.review.snapshot.gc {task | repo, include_unrecorded?, dry_run?}` (full scope; CLI `vibeke task snapshot-gc [task] | --repo <path> [--dry-run] [--include-unrecorded]`; refs no record names are only removed with `include_unrecorded`). The capture is accepted only when the change digest (`vk-review/baseline/v2`: status, `diff --binary HEAD`, untracked file hashes including the executable bit, and each changed submodule's HEAD and change digest, recursively) and HEAD agree before and after; a `git add` failure while files change counts as a change. Up to three attempts, then `conflict{reason: workspace_changing}` with this label. Matching digests narrow but cannot rule out a write reverted inside the window; the stored content is nevertheless exactly what checks and acceptance name. The `dirty_snapshot` `ChangeSubject` (its id also covers the commit and tree; the commit's tree and parent are re-verified before use) is the **current, accept-capable candidate while the checkout still has the same HEAD and change digest**; otherwise it stays an inspectable earlier candidate. "Current" is only as strong as the digest: the first build's digest ignored untracked files' executable bit and submodule contents, so a `chmod +x` or an edit inside an already-dirty submodule left an old snapshot "current" (latest-areas review); both are in the digest now, and snapshots taken before the upgrade show as not current. Not built: execution-interval binding for observed agent commands.

*As built (lane 2C, `vk-review::selection`, `review::patch`):* **selected-patch snapshots** — `task.review.snapshot {task, paths: [..]}` (whole files, deletions included) or `{task, patch: "<unified diff>"}` (hunks applied to HEAD with `git apply --cached`) writes HEAD plus only the selection through a private index and stores it like a dirty snapshot (`refs/vibeke/snapshots/`, fixed identity, so the same selection on the same HEAD is the same subject). Subject kind `selected_patch` with `selection {mode: paths|patch, paths, patch_digest?, excludes_other_changes}` in its identity. Capture is consistent only when HEAD and the whole checkout's change digest agree before and after (whole files: the selection's tree is also identical when written twice); otherwise `workspace_changing`. Refusals: `nothing_selected` (the selected files hold no change), `invalid_selection` (a path outside the repository, a patch that does not apply), `unsupported_capture` only when a changed submodule is part of the selection. The latest selected patch is the current, accept-capable candidate while it still describes the checkout (same HEAD; whole files: the selected files' content is unchanged, edits elsewhere don't matter; a patch: the whole digest is unchanged) and it is newer than the latest dirty snapshot; acceptance re-checks this right before its transaction (`review_changed`, `detail: selection_outdated`). The package warns "Selected changes … the rest of the checkout is not part of this review". Checks against it run in a disposable checkout of the snapshot commit, so they verify the selection alone; checks against other subjects keep their own subject. CLI `vibeke task select <task> --paths f …|--patch d`; TUI `P` in task details. **Dirty end candidates** — when a binding closes (unbind, switch, suspension) with uncommitted work in the checkout, a dirty snapshot is captured right then (two attempts) and pinned as the binding's end candidate (it includes the commits since the base); snapshot GC keeps pinned snapshots. If no consistent capture can be made the committed candidate (if any) is pinned and the note says the uncommitted part was not captured. Limitation: the capture runs as the binding closes, after HEAD was read; a write landing in that instant by the next writer is captured with it (the digest check narrows, but cannot exclude, this).

## 6. Review package and verification

### 6.1 Package contents

A `ReviewPackage` contains the exact intent revision and change subject, links to the source request and diff, current criterion assessments, observed checks, artifacts, attributed findings, external outcome observations, and any previous human acceptance. It is a projection with a revision and source cursor vector, not a second source of truth.

The accept-capable view renders its diff/files from that immutable subject's stored content or verified immutable Git objects, never from a fresh unqualified `git diff` or mutable PR head. Acceptance names the same digest shown to the user. A separate **Live checkout** view is labeled live and cannot submit acceptance for another candidate. If the inspected subject becomes unavailable or the view switches candidates, invalidate the acceptance form and require a refreshed inspection.

The default view answers, in order: what was requested; what changed; what supports each requirement; what is missing or disputed; which action is available. Optional generated prose cites the underlying objects and follows 14. The deterministic view always works.

Criterion assessments are `supported | failed | missing | stale | needs_judgment | unknown`. A check mapping is configured/confirmed by the user or a trusted project recipe. A model may suggest a mapping but cannot certify semantic coverage. Human-reviewed criteria record the actor and inspected subject. Aggregate by **check definition + environment + subject**. Failures on an older subject remain visible history and do not invalidate a fixed revision. Mixed outcomes on the same subject default to **needs judgment** for possible flakiness; a successful retry never silently erases the failure.

A reviewer agent's finding contains source references and provenance. It cannot mark its own task accepted, waive required checks or override a failed observation. Running a reviewer is explicit or separately preauthorized; it consumes the selected provider's usage.

*As built (T4, `vk-review::reviewer`, `task.review.request_reviewer/start_reviewer`, `task.review.notes/note.classify`):* reviewer runs are explicit only (no preauthorization). `request_reviewer` builds a deterministic, review-only prompt from the package (intent, criterion statuses, stopping point, `git diff <base> <content commit>` of an immutable subject, file list, recorded checks, a `FINDING [blocking|concern|nit] …` / `NO FINDINGS` reply format) — or takes the user's edited text — and launches nothing. A re-prepared (edited) prompt carries the original `expected_subject`; if the task's current candidate has moved, the request is refused with `conflict{reason: subject_changed, details: {expected, current}}` and the TUI shows a fresh prompt for the new subject for renewed confirmation (never auto-started). `start_reviewer` requires that prompt's digest (`prompt_mismatch` otherwise, nothing launched) and moves the request `prepared → starting` atomically with those checks, so concurrent confirmations launch at most one run (a second caller gets `reviewer_starting`, the same idempotency key replays the first result from its operation receipt); success records `started`, the binding and the receipt together, a failed launch returns the request to `prepared`, and a request found in `starting` after a restart becomes `unknown` (event `review.reviewer_unknown`; never relaunched automatically). It starts the harness through the `agent.start` path (the prompt is its initial prompt, sent once; no focus change; it runs in a split of the task's pane or a pane the user names) and binds the run with role `review` from its first turn, so a reviewer turn that settles before the launch call returns is still collected (exactly once). Review-role bindings never contribute observed commands or claims to the implementation's evidence and never become `AgentRun.task`. Each settled reviewer turn becomes **review notes** (one per `FINDING` line, else one `unstructured` note), attributed to the run (`agent`), `category: agent_claim`, tied to the reviewed subject. Unassessed `blocking`/`concern`/`unstructured` notes and user-marked blockers on the reviewed subject are `blocking_concern` blockers (§7); the user classifies notes (`blocking | not_blocking | dismissed`, a dismissal needs a reason; attributed). Notes cannot accept, waive or change evidence. *As built (lane 2C, `vk-review::scratch`, `review::scratch`):* when `start_reviewer` splits a new pane (the default), the reviewer works in a **disposable checkout** — `git worktree add --detach` of the reviewed subject's content commit under `<state>/reviewers/<request>` (a `git archive` export when worktrees are unavailable) — so it is not a known writer in the task's checkout and nothing it edits reaches the user's work. The request records `checkout {path, content_sha, removed_at_ms}`. The checkout is removed (only that directory and its worktree metadata) once the reviewer run ended or its start failed/became unknown, by the attention watcher, after a failed launch and at server start (orphans of a crashed launch too); event `review.reviewer_checkout_removed`. `checkout: "task"` keeps the old behaviour; a reviewer started in a pane the user names works in that pane's directory (`checkout: "disposable"` with `pane` is refused). The default falls back to the task checkout when the subject is not immutable or the checkout can't be created. Remaining limitation of the task-checkout mode: the reviewer counts as a known writer there.

### 6.2 Observed versus independent checks

Use three visible categories:

- **Agent-reported claim:** prose or an unconfirmed report; never a passed check.
- **Observed agent check:** a bound tool/process execution, exact command/cwd, start/end, result, logs and subject/environment information. Missing native fields remain unknown. The harness adapter is an attribution source, not a trusted verifier against an adversarial agent.
- **Vibeke verification:** a configured check launched in a separate verification run against a stable captured subject. Record who selected the check and whether its definition was supplied by the task's changes or a trusted baseline/user recipe. Separate execution does not make a task-authored test independent in design.

An unbound observed command may say **Command passed · code binding unverified**, while its requirement remains unknown. A quiet filesystem watcher or matching start/end hashes is not enough to upgrade it. Only evidence whose runner/collector establishes the stable execution subject can support a machine-verifiable criterion. Users may accept unbound evidence as an explicit review exception, never as a fabricated pass. The §2.5 example uses a separate committed-subject verification for this reason.

### 6.3 Check authorization and execution

Checks are arbitrary code, including source/tests modified by the agent. **Run missing check** shows command, execution machine, environment, scope and expected side effects. On the host, require a per-candidate explicit action labeled **Runs code modified by this task**; a remembered command/recipe cannot authorize future task code. Auto-run is allowed only in a contained sandbox/container/VM, with an explicit recipe/policy grant and a broker/profile that does not silently widen the originating task's filesystem, network or credential authority. It is off by default.

Changed commands, runner permissions, endpoints or recipe digests require renewed authorization. Record resolved script definitions/test configuration (for example package scripts and referenced Makefile targets) alongside the full subject/environment identity, and flag task-modified definitions. Unchanged argv does not mean unchanged executable code. A repo file or agent suggestion cannot authorize itself. Separately authorizing verification never grants new permissions to the implementation agent.

Verification uses a disposable checkout/snapshot on the task's machine or an explicitly selected runner, defaulting to the originating implementation's containment level and permitted resource profile (or a verified stricter profile). For several implementation runs, choose a profile that does not silently broaden their applicable permissions; if none is available, ask the user to select one. Missing containment support never silently falls back to host.

Running a contained task's verification on the host requires a distinct **Outside this task's containment** confirmation for that candidate, showing the containment being lost and newly reachable filesystem, network and credential classes without exposing secret values. The ordinary host-run click alone is insufficient. Verification does not reset/stash the active checkout or silently execute remote requests on the laptop. Dependencies/fixtures and reused services are recorded; shared mutable services prevent hermetic/fully independent verification claims.

Snapshot capture requires a stable subject, including dirty/untracked content when selected. Capture must detect concurrent writes; checks performed against a changing live checkout are labeled unbound unless the runner can establish the complete subject for the execution interval. Matching start/end hashes alone cannot exclude intermediate changes. The UI offers a stable commit or isolated snapshot when it cannot bind evidence.

**Initial T2 scope:** verify and formally accept committed revisions, using disposable checkouts and an explicit environment manifest for checks. Dirty work remains inspectable/annotatable but cannot receive formal acceptance until the user selects a committed candidate. Stable dirty/selected-patch content capture for acceptance and execution-interval binding for verification are separate T4 capabilities; neither is assumed available in T2. Host verification always uses the per-candidate action above; contained automatic verification additionally depends on 13's containment workstream. Neither flow may be described as hermetic if shared mutable services affect its result.


*As built (3F, execution-interval binding; `vk-review::interval`, `vk-server::review::interval`):* an observed shell command of a run bound to a task is bound to a subject only when the collector can show the subject was stable for the **whole execution interval**. For each such command the server (a) makes sure a file watcher covers the checkout (`notify`; a journal of writes, armed before the command starts), (b) captures the checkout state (HEAD, change digest, as for screenshots) and the other runs writing in the checkout at the synchronous `PreToolUse`, (c) after `PostToolUse` waits `settle_ms` for late events, captures the state again, asks Git (`check-ignore`) which written paths are ignored and decides. A command is `bound` only when: the journal covers the interval completely (watcher armed before the start, no overflow/restart gap, nothing evicted); no non-ignored write (Git internals excepted, their effect is in the captured states) happened between the start and `settle_ms` after the end; no other known writer was active; and the start and end states are equal and neither is `unknown`. Matching start and end hashes alone never bind: a write-and-revert shows up in the journal. Anything else is `unbound` with named reasons (`writes_observed`, `watcher_late`, `watcher_gap`, `watcher_not_armed`, `journal_evicted`, `subject_changed`, `other_writer`, `state_unknown`, `no_start_state`, `no_end_state`, `missing_times`) and the offer "Select a stable commit or take an isolated snapshot to bind evidence". A bound interval names a *state*; the package names a subject on the command only for the selected subject that state shows exactly (a committed subject = clean tree at its head; a dirty snapshot = same head and change digest), so the row reads **Command passed** (not "code binding unverified") and, when it is the mapped check's command, supports the check criterion: the tool versions of the command are probed when it ends and recorded as the environment identity a host run of the check would have (an unavailable identity leaves the evidence stale/unknown, never fresh). `task.review.get` adds an `interval` object to every observed command (`bound|unbound|not_collected`, reasons, explanation, window) and `task.review.intervals` / `task.review.interval_status` list the records and watchers. Only harnesses whose pre-tool hook runs *before* the tool are collected (`[review.interval] harnesses`, default `["claude"]`; the others need that verified per harness, see the audit); `watcher = "none"` is the fake backend that binds nothing. Unbuilt: attribution of writes to a writer (3A), selected-patch subjects, and binding of commands run in pane shells rather than by the agent.
*As built (T4):* a validated dirty snapshot (§5) is verifiable and accept-capable like a committed candidate: check definitions resolve at the snapshot commit, the disposable checkout is materialized from it, and the same per-candidate **Runs code modified by this task** grant (naming the snapshot subject and definition digest) is required. Acceptance of a current snapshot additionally re-reads the checkout's change digest right before its transaction; a difference is `conflict{reason: review_changed}`. A later edit makes the snapshot non-current, so an acceptance of it becomes **Review outdated** (code changed since acceptance). Selected-patch subjects (lane 2C, §5) verify and accept like dirty snapshots. Execution-interval binding is built (below).

Check execution is explicit and bounded: concurrency, timeout, log size and output retention follow the existing runner limits. State is `queued -> running -> passed|failed|cancelled|interrupted|unknown`. Submission has caller-scoped idempotency. On restart reconcile the holder/runner before recording an outcome; never launch a duplicate external-side-effecting check automatically. Cancellation records partial output and never fabricates a failure/pass.

### 6.4 Browser and external outcome evidence

Screenshots retain 06's environment labels. Add runtime identity separately from checkout identity: launched server/build ID or digest, producing subject, process start, browser session/device, data fixture identity and capture time. A checkout SHA observed at screenshot time alone cannot bind an old server's response to new code. If runtime identity is unavailable, show **Build not verified**; the image is illustrative and cannot satisfy a required check for the current build.

*As built (Goal 03 Stage 4, 06 B6):* every screenshot record carries checkout identity (`code {repo, head_sha, dirty_digest, dirty_state, captured_at_ms}`, the observation-baseline digest) and, separately, running-build identity (`runtime {status, source, build_id, head_sha, dirty_digest, started_at_ms, fixture}` from the app's `/__vibeke_build` endpoint or `X-Vibeke-Build` header — typically the output of `vibeke screenshot code-state` captured by the dev script at launch), plus browser session/device in `environment` and capture time. `binding` is `bound` only when the running build reports exactly the captured checkout state; otherwise `illustrative` (**Build not verified** when no identity is reported). Review packages (`task.review.get`) list the task's screenshots (recorded for the task, or taken by a run bound to it) under `screenshots` and as `browser` evidence (`EvidenceCategory::Browser`, outcome always unknown): a screenshot is linked to the reviewed subject only when it is `bound` **and** its code state equals that subject (a committed subject = clean tree at its head); only then is it referenced from the intent's *human* criteria ("Screenshot of this revision available for your review (not a pass)"), which stay `needs_judgment` until an explicit human review. Browser evidence never counts for a check criterion, whatever its binding or claimed outcome, and never changes readiness; a bound screenshot of an older revision shows as illustrative for the current subject. Not built: process start time from the preview's pid, data fixture identity beyond what the app reports.

*As built (lane 2C, `review::human`):* `task.review.human_review {task, criterion, verdict: supported|failed|withdrawn, subject?, expected_subject?, note?, screenshots?}` (full human-client scope; pane tokens refused) records the user's judgment of one **human** criterion on one immutable subject as `HumanReview` evidence with the actor and subject. A failure needs a note; a check criterion is refused (`not_a_human_criterion`); a moved subject is `review_changed`; every named screenshot must be `bound` to that subject (`screenshot_not_bound` otherwise). The readiness engine reads it like other evidence: it decides that criterion on that subject only (an older subject's review shows as reviewed on an older revision), `withdrawn` returns it to **needs judgment**, and it never satisfies a check criterion or accepts the task. A recorded review is a known competing update for acceptance (state token). The package lists `human_reviews`; event `review.human_reviewed` (history tier). CLI `vibeke task human-review <task> <criterion> <verdict> [--note]`; TUI `H` in task details.

PR evidence includes provider/repository/PR identity, target branch, head revision, draft state, observation time and authorization scope. A pasted URL or agent statement is not confirmation. Changed PR head invalidates the observation for the old subject. Failed/offline lookups remain unknown. Merge and deployment observation are extension points only; this slice has no release executor.

*As built (3F, PR evidence; `vk-review::pr_evidence`, `vk-server::review::pr`, `vk-tasks::ghpr::fetch_pr_evidence_json`):* `task.pr.observe {task, pr?, criteria?}` is an explicit, full-scope lookup through the user's authenticated `gh` CLI (never automatic, never from a pane token; no prompt, 5 s timeout, refused for sandboxed checkouts). It stores an immutable `pr_observation` row: provider, host and repository, PR number and URL, target branch, head branch, **head revision**, state, draft flag, review decision, checks rollup, observation time, `authorization_scope` (`gh_cli`: the user's own login, read-only use), the actor and the external criteria it is offered for (default all). A failed, offline or unauthenticated lookup and "no pull request" are recorded as such and stay **unknown**. `task.pr.claim {task, url}` records a pasted URL (or, from a pane, an agent statement) as a **claim**: shown as "Claimed … (not confirmed)", agent-claim evidence that never supports anything. In `task.review.get` (`pr` section plus `external_observation` evidence) an observation supports an external criterion only for the **committed subject whose head equals the observed PR head, in the same repository** (origin remote normalized and compared with the PR's host/owner/repo); a dirty snapshot is never in a PR; a changed PR head leaves the old observation bound to the old subject only and a re-observation is a new row (`review.pr_head_changed`). Outcome for a bound observation: open, not draft, no failing checks, no requested changes: `passed`; closed unmerged, failing checks or requested changes: `failed`; draft, checks still running and **merged** are unbound (judge manually; merge observation is an extension point and nothing here merges, deploys or comments); observations older than `[review.pr] max_age_secs` (default 900) are demoted to unknown. Events: `review.pr_observed`, `review.pr_head_changed`, `review.pr_claimed`. Only GitHub through `gh` is implemented; other providers need an adapter producing the same `PrFacts`.

## 7. Readiness, acceptance and freshness

| Label | Meaning |
|---|---|
| Turn finished | A turn completed; no statement about the task outcome |
| Needs task details | Required intent/criterion/stopping-point mappings remain unconfirmed; show the original request and an edit action |
| Changes to inspect | Untracked work or work without confirmed intent has an inspectable diff/observations; no criterion-completion claim |
| Review available | A package exists; it may contain missing/failed/stale evidence |
| Ready for your review | Intent and criterion/stop mappings are confirmed; subject stable/current; all required machine-verifiable criteria supported; no failed/unknown required criterion or unresolved blocking concern; bound implementation runs idle, no known active writer in the same checkout (including other tasks, untracked runs and writing shell processes), no open task Interactions, pending binding switches or unresolved message delivery; required human judgment is presented for review |
| Reviewed | The user accepted this intent revision and subject, including any explicitly recorded exceptions |
| Review outdated | The accepted intent, subject, required environment/check definitions or relevant external outcome no longer matches current state |

Known blocking findings are explicit user-marked blockers or trusted check failures. Generated reviewer findings are visible concerns requiring classification; they cannot silently change permissions or task state. An unassessed potentially blocking review concern prevents the stronger readiness label until resolved, dismissed with attribution, or explicitly excepted by the user.

Freshness is evaluated from the complete evidence subject, relevant environment and check-definition digests, intent revision and live source coverage. A rebase invalidates SHA-bound evidence even if a model says the code is equivalent. No semantic-equivalence shortcut in the initial implementation. Dependency/fixture changes invalidate affected checks; unavailable environment identity yields unknown freshness.

An active, disconnected or otherwise unverifiable bound implementation run, or another known writer in the checkout, prevents Ready; use **Review available — agent/writer active or state unavailable**. Absence of a known writer is a necessary condition, not proof of a stable filesystem; immutable-subject and execution-binding requirements still apply. Historical immutable candidates remain inspectable while work continues, with their exact subject and age displayed. Reading an old candidate is never labeled review of the current moving checkout.

**Mark reviewed** sends expected intent revision, package revision, subject digest and any exceptions. The owner revalidates source observations, serializes the expected-version check with acceptance in its state transaction, and accepts the named immutable subject only. A known competing update returns `conflict` with `reason=review_changed`. This is not an atomic transaction with an external filesystem: a later observed change invalidates current acceptance without deleting its history. If a live subject cannot be captured consistently, require an explicit immutable candidate or keep the action unavailable. Offline/gapped sources cannot produce a new acceptance; users may save a local review draft for later submission.

Missing/failed required criteria require explicit exceptions naming each criterion and a reason. The UI says **Reviewed with exceptions**, never turns them green. Acceptance is distinct from marking the package seen, answering an approval, finishing a task or granting merge permission.

## 8. Attention inbox

### 8.1 Items and ordering

Build a deterministic projection over authorized source objects: live Interactions, delivery failures/unknowns, actionable run/setup/check errors, and review packages with an unreviewed candidate. Routine working runs stay in the sidebar; the inbox has an optional **Also working** footer (*as built, lane 2C:* `attention.list` → `also_working [{run, pane, name, task, working_for_ms}]`, busy runs without an open interaction, scoped like the list; drawn under the items, derived from the session model for older servers; `ui.inbox.also_working`, default on). A review candidate is created on an explicit user request or a settled-turn checkpoint with a changed subject; streaming output/file events do not create notification floods.

This evolves 08 §6.6's M1 inbox in place: one surface, the same `prefix+i`, progressively adding task/review items. At T3, `next_attention` (`prefix+a`) follows this ordering, opening the first actionable item using the same snooze/expiry filters. Reading an unresolved Interaction does not remove it from attention; seen review candidates can move below unseen candidates within their class. Before T3 the current M1 ordering remains. There are never two inboxes competing for a keybinding.

Use stable object IDs and revisions for deduplication. Group related items by task, while preserving each actual Interaction and action. A review item is keyed by task plus candidate subject; reading it marks seen, not accepted. Dismissed errors reopen only on a new occurrence. A task without confirmed intent can show **Changes to inspect**, never verified readiness.

Order by this precedence, with user pinning within a class and age as a stable tie-breaker:

1. Unconfirmed/failed decision delivery, or source errors that make continued action unsafe/uncertain: durable storage unavailable, lost verification runner with uncertain outcome, or lost integration identity during a pending send. Ordinary setup/test failures remain with their blocking-decision/review item and do not outrank expiring approvals.
2. Open Interactions with a native deadline approaching (default: at most 60 seconds remaining, configurable); show the actual deadline. Expired native requests are reconciled and do not remain answerable merely because their card is cached. *As built (lane 2C):* deadlines live in a side table (`interaction_deadline`): `native` from the adapter payload (`deadline_ms` / `expires_at_ms`, or `timeout_ms` / `timeout` relative) or `record_native_deadline`, and `gate` (Vibeke's 30-minute hook gate; after it the harness shows its own dialog), which counts only while the gate is held. The window is `ui.interactions.deadline_window` (default `60s`). `attention.list` items carry `deadline_ms`, `deadline_source`, `deadline_in_ms`; the inbox detail says "The agent's request expires in …" or "Answer here within …; after that the question shows in the agent's own pane". A passed native deadline closes the interaction as `expired` (watcher). No tested adapter reports a native deadline yet, so only the gate deadline appears in practice.
3. Other blocking decisions, ordered by explicit task priority, confirmed dependent tasks and waiting time.
4. Review candidates, ordered by task priority and age.

After ranked actionable items are exhausted, `next_attention` retains the M1 fallback: focus the oldest unseen `✓ done` run, including untracked runs. These can appear in the inbox's **Finished turns** footer without creating tasks or review packages. Users who never track work keep their existing navigation behavior.

Risk is displayed and raises prominence within a class; risk alone must not imply that approval is recommended. T3 starts with run-blocking status, explicit priority and age. Confirmed dependency links/counts are a later optional enhancement, not required setup: inferred links are excluded from ranking, cycles are rejected when that enhancement ships. Explanations reflect available data, for example "Waiting 12m; blocks this run"; counts such as "two linked tasks" require real confirmed edges.

*As built (T4, `vk-review::dependency`, `task.dependency.add/remove/list`):* a user confirms an edge `task → depends_on` of kind `blocks` (default) or `related`; the confirming user is recorded (agents/pane scope cannot add or remove edges). `blocks` edges are cycle-checked under the state lock (`conflict{reason: dependency_cycle, path}`; self-links and duplicates are refused too); removal closes the edge row as history; `task.dependency_changed` is a history-tier event. An item's `blocks_tasks` counts the open (active/parked) tasks transitively waiting for its task through confirmed `blocks` edges; within classes 3 and 4 it orders after explicit priority, the five-minute view prefers it among non-urgent items, and the explanation says "blocks N linked task(s)". `related` edges never rank. There is no inferred linking. Reads (`task.dependency.list`, the review package of `task.review.get`) are filtered by the caller's access: a pane-scoped caller sees a linked task outside its workspace only as `{hidden: true, title: "hidden task", edge: {kind}}` — no title, handle or ids.

Resort on meaningful changes with a short debounce. Preserve the selected object and scroll position while the user reads/types; newly urgent items get an indicator rather than replacing the selected card. A resolved Interaction disables its submit action immediately and shows who/what resolved it.

### 8.2 Five-minute view

**5-minute view** creates a suggested, stable working set; it is not a timer that dismisses unanswered work. Always surface urgent/unknown delivery items, even if their estimated effort exceeds the budget. Show **Urgent — may take longer**. Other candidates prefer high unblock impact and lower estimated review effort while retaining an **All items** count and access.

Effort starts as user-set `quick | a few minutes | deep review | unknown`; users need not label every task. T3 provides a stable shortlist using these coarse values plus blocking/age, with unknown-effort items eligible. Optional model/history estimates belong to T4 and are labeled estimates with a source; no precise countdown or guaranteed finish time. Omitted tasks remain visible in All items and gain age priority. The UI states that the suggested set may exceed five minutes.

*As built (T4):* two optional estimates, both labelled with their source and never applied: a deterministic **heuristic** (`vk-review::effort`: changed lines and files, binary files, failing/unknown checks on the subject, criteria needing judgment → `quick | minutes | deep`, `unknown` without a diff) in `task.review.get` (`effort.heuristic`), `task.effort.estimate`, and as `effort_estimate {effort, source: heuristic}` on `attention.list` items whose effort the user hasn't set; and a **model estimate**, the 14 operation `effort_estimate` (class `review_package`; preview + confirm; output `{effort, rationale, source_refs, estimate_source: assistant, applied: false, apply_with}`). The user applies either with `task.set {effort, effort_source}`; ranking and the five-minute view use only the user's value. No history-based estimate.

### 8.3 Snooze, batching and notifications

Snooze is per user and item revision: until a chosen time or review window. Never resolve/cancel the source or tell the agent an answer was given. A deadline, risk escalation, delivery uncertainty or material revision wakes the item with an explanation. Changes to unrelated logs do not. This single-user slice does not implement team snooze or ownership.

Respect existing presence and quiet-hour settings. *As built (lane 2C, `review::attention_ext`):* an attention watcher (event-driven on model changes, plus a timer only while deadlines or snoozes are pending) notifies items that newly became urgent (delivery problems, deadline approaching), failed checks and tasks that became **Ready for your review**, once per item revision and class, through the ordinary notification pipeline (`Server::notify`: `notifications.on.deadline` / `.review` / `.error`, presence = the item's pane focused in an attached client, quiet hours for everything below urgency high; only class-1 delivery problems are high). Snoozed items stay quiet unless a material change woke them; open interactions are not re-notified (they were when they opened); the first pass after a start only records what is there. A snoozed item may remain pending in a harness with a shorter native timeout; show that deadline before accepting the snooze. Offline machines have a persistent coverage indicator and last-observed time. Reconnect reconciles objects before enabling actions; cached cards never authorize queued automatic decisions.

*As built (lane 2C):* `attention.list` items carry `batch {id, size}` and the result lists `batches`, computed on the server with `vk_review::attention::batchable` from server facts (harness, tool, the raw command byte for byte, resource paths, the pane's isolation level and network profile, the workspace root as policy scope; only natively answerable, plain, low/medium-risk approvals). `attention.batch {interaction}` returns the current group with each member's `decision_rev`; members are still answered one by one with `interaction.answer` (`expected_decision_rev`), so there is no second answer API and each member records and delivers on its own. The inbox shows `⧉N` and `A` opens the batch view seeded with the item. Batching is limited to 04/08's supported native approval cases. Sharing a normalized command/fingerprint is insufficient: the effective policy scope, execution environment, operation and resource targets must be equivalent and each decision must still be live. Show the members and record/deliver individually, including partial failures. Never batch product questions, plan reviews, high-risk/unknown-risk actions, or human task acceptance.

## 9. Sending messages and answering safely

Opening a task, tracking it, editing intent, generating a draft and marking a review seen send no bytes/messages to any harness. Explicit Send/Answer actions re-resolve run identity, native conversation, current task binding and capability. If the run moved to another task or conversation, refuse stale send and offer target selection.

Interaction answers use the existing decision revision, idempotency key, lease and reconciliation semantics from 04. For freeform follow-ups, record a `TaskMessage` with caller idempotency key, intended binding/intent revision, exact text, actor, native request ID if any, and `prepared | sending | delivered | delivery_unknown | failed | cancelled`. Do not claim exactly-once native delivery where the harness cannot prove it.

When native follow-up/steer is supported, use it. Otherwise an explicitly authorized send can use 07's prompt-input path only where the tested adapter can establish an idle, **empty** input and no attached client currently focuses that pane. Acquire the existing input lease and recheck immediately before delivery; focus/input changes invalidate the attempt. If emptiness, identity or exclusion cannot be established, show the draft and **Open pane to send**, with zero PTY bytes. Never append to a partial user draft or type into a running tool/dialog. Confirm fallback delivery only from a bound submit/turn event matching the sent content and target; an ambiguous association remains `delivery_unknown`. No automatic steering just because task details changed.

After an ambiguous send, preserve the draft/result and reconcile if supported; do not automatically resend. A manual retry warns that the earlier message may have arrived. A native delivered acknowledgment proves receipt, not that the agent understood or complied with the instruction.

## 10. Data, API and persistence additions

These are proposed schema extensions, not currently supported configuration or API. On implementation, merge canonical types into 02/07/08. Existing transport, ownership, authorization and outbox conventions remain authoritative.

### 10.1 Stored objects

| Object | Essential fields and ownership |
|---|---|
| Task extension | `owner_machine/session`, `workspace_ownership`, `current_intent_revision?`, explicit priority; existing lifecycle remains |
| TaskIntent | Immutable confirmed revisions per §4; drafts stored separately and never treated as authority |
| TaskRunBinding | Task/run/native-conversation identity, selected start/end turn or source cursors, effective boundary, role `implementation`, `review` or `verification`, actor; at most one foreground binding per run turn |
| ChangeSubject | Immutable content identity and selected review scope per §5 |
| CheckDefinition / CheckRun | Recipe revision/trust grant, argv or explicit shell command, cwd/env/runner scope, subject, lifecycle, exact observations and log references |
| ReviewPackage / Assessment | Intent/subject, projection revision, source cursors/coverage, criterion result and supporting references; regenerate only from authorized inputs |
| ReviewAcceptance | Task, exact intent/package/subject, actor/time, exceptions, current or outdated status derived from live state |
| TaskMessage | Intended recipient/binding, text reference, idempotency/delivery state per §9 |
| InboxPreference / Dependency | Per-user seen/snooze/pin state tied to subject revision; explicit confirmed dependency edges with provenance |
| PendingClientOperation | Durable local caller/owner identity, idempotency key, operation kind, expected revisions/payload digest and outcome; persisted before dispatch, reconciled after restart |

Extend 12's `EvidenceRecord` with check-definition identity, subject identity, collection category, runtime/build identity where applicable and source coverage. Existing evidence lacking these additions remains readable as **Legacy evidence — binding incomplete** and cannot automatically satisfy stronger readiness. No destructive rewrite of historical events.

### 10.2 Proposed API surface

All mutations accept caller-scoped idempotency keys and expected object revisions where relevant. Reuse 07's canonical errors: `conflict` with a structured reason such as `binding_unverified`, `binding_changed`, `verification_unbound` or `review_changed`; `unsupported` with capability details; and existing remote/permission/storage failures. These reasons are not new incompatible top-level JSON-RPC codes.

| Method | Behavior |
|---|---|
| `task.track` | Atomically create an attached task, confirm intent and bind selected source range; run identity must be deterministically bound per 04; no spawn/send/setup |
| `task.intent.get/update` | Read or create a confirmed intent revision; update is record-only |
| `task.bind` / `task.unbind` | Explicit verified association and boundary handling; reject suggested/unlinked run identities; never process relocation |
| `task.message.prepare/send/get` | Preview recipient/text, explicitly send, inspect delivery; no coupling of save success to send success |
| `task.review.get` | Deterministic package with source cursor vector and freshness; no model call or check execution |
| `task.review.accept` | Atomic acceptance of expected revisions/subject with explicit exceptions |
| `task.operation.get` | Authorized caller queries a durable mutation receipt by idempotency key; unknown/expired receipt never means safe to repeat blindly |
| `task.check.list/run/cancel` | Read defined checks and explicitly authorize/submit/cancel verification |
| `task.dependency.add/remove` (T4, built) | Confirm explicit dependency edges; cycle checks and normal mutation authorization |
| `task.review.snapshot` (T4, built) | Capture a validated, immutable dirty snapshot subject; `workspace_changing` when no consistent capture |
| `task.review.request_reviewer/start_reviewer`, `task.review.notes/note.classify` (T4, built) | Reviewable reviewer prompt, explicit start with its digest, `review` binding; findings as attributed notes the user classifies |
| `task.review.human_review` (lane 2C, built) | Record the user's judgment of a human criterion on one immutable subject; full human-client scope |
| `task.review.forget` (lane 2C, built) | Purge derived review content in a scope (also run by `scrollback.forget`); full scope |
| `task.link.status` (lane 2C, built) | Why a run's identity is unverified, remedies and verified runs nearby; read-only |
| `attention.batch` (lane 2C, built) | The current batch of equivalent native approvals with member decision revisions; read-only (answers still go through `interaction.answer`) |
| `attention.list` | Ranked items, coverage and optional effort budget; deterministic by default |
| `attention.update` | Set seen/snooze/pin for exact user/item revision; never answer or accept |

Optional extraction, explanation and review prose are named operations on 14's `assistant.generate`, governed by that feature's limits. Generated outputs cannot call mutation methods. Existing `interaction.answer` remains the only interaction-answer API. CLI and future mobile/web use the same methods; no new default keybindings beyond actions under the existing peek, palette and inbox.

### 10.3 Atomicity, remote ownership and recovery

The owner is the server hosting the task's primary workspace/checkout. Tracking a devbox run from a local client creates the task on devbox; the client forwards machine-qualified `task.*` requests through 07. This is the same owner in plain-SSH and local-bridge topologies. The local coordinator stores only its per-user inbox preferences, cached projections and unsent drafts; it does not mirror authority for acceptance. Cross-machine run bindings are later extensions; T1–T3 binds only runs on that owner and explicitly rejects other-owner attachment as unsupported.

The task's owner server is authoritative for intent, bindings, subjects and acceptance. Multi-machine inboxes aggregate authorized projections with independent source cursors. Remote observations are never silently relabeled local. If sources cannot be revalidated, acceptance is unavailable. If the link drops after an acceptance request, the result may be unknown: reconcile using the same idempotency key on reconnect, never claim no acceptance occurred merely because its acknowledgment was lost.

Persist the pending operation and its idempotency key in the client's local state before dispatch, alongside its draft/expected revisions, so a client crash does not create a new key. On reconnect/restart query the owner's receipt with `task.operation.get`; do not automatically resubmit a mutating request whose outcome is unknown. Owners retain deduplication receipts for the supported reconciliation window (initially 30 days, advertised to clients). Expired/missing receipts require a fresh inspection and explicit user action; they cannot justify a silent retry.

Persist mutations and their events in the same SQLite transaction. New event families include `task.intent_updated`, `task.binding_changed`, `task.message_*`, `check.*`, `review.candidate_created`, `review.accepted`, `review.invalidated`, `attention.preference_changed`, `task.dependency_changed`. Events contain IDs, revisions and metadata rather than full prompts, generated text, secrets or logs. Blob content follows 09.

Assign intent/binding changes, message decisions/outcomes, terminal check outcomes, acceptance/invalidation and dependency changes to 02's **history** tier. Queue/progress notifications, candidate projections and per-user inbox preferences use **sync**; their current state survives sync pruning in state tables. History has the existing configured retention, not indefinite storage. The bounded source excerpt in §4.1 survives routine item compaction but obeys explicit purge and content retention.

On reconnect, recover authoritative state before applying pending UI actions. `events.truncated` forces a fresh snapshot; do not continue ranking as if a missing interval were observed. On disk-full, refuse durable edits/answers/acceptance consistently with 02; terminal interaction continues. Transient generation failures do not affect task records or answering.

## 11. Privacy, authority and performance

- Source content is authorized before retrieval, projection, search or model use. A session-wide inbox reports excluded/offline scope without leaking its contents. A task link or dependency never widens access to another workspace's conversation.
- Agent/adapter tokens can report their own observations through existing APIs. They cannot confirm intent, accept reviews, waive checks, alter priorities/dependencies or authorize verification. These new mutations require an explicit human-client scope; future automation grants require a separate design.
- AI assistance needs 14's workspace/connection/context consent. No background extraction or model request occurs just because a CLI launched or a turn ended. User-configured automatic assistance, if later added, requires separate opt-in.
- Apply 09's storage permissions and operational-content rules. `forget` removes scoped drafts, messages, derived packages and cached excerpts as well as underlying sources; retained references become unavailable rather than resurrecting purged content. Required evidence purged from a package changes readiness to unknown. *As built (lane 2C, `review::purge`):* `task.review.forget {task | pane | workspace | before | all, dry_run?}` (full scope; CLI `vibeke task forget` with scope flags and no positional task — `vibeke task forget <task>` is `task.forget`, 07) and every `scrollback.forget` (`vibeke forget`, same scope, reported under `review`) purge, for the tasks and runs in scope: task message text (delivery records stay), intent source excerpts, turn prompt copies and last messages, observed tool-item command/claim text, reviewer prompts, review-note and human-review note text, check-run log files, and cached projections. Each purged object gets a tombstone (`review_purged`) so packages show the content as unavailable instead of rebuilding it; evidence whose content was purged (check runs whose logs were purged, observed commands of purged runs) counts as `unknown`, so a required criterion it supported is no longer supported. `review.purged` carries scope and counts only. Not purged here: assistant records (`assistant.purge`), screenshots (`screenshot.delete`), R2 drafts (`draft.delete`), events (no tombstoning yet). References may pin blobs only within the user's declared retention policy; the UI explains expiration before acceptance history loses inspectable evidence.
- Host-mode permissions remain cooperative guardrails; contained execution uses 13's broker. Intent text never grants credentials or containment, and reviewers/verifiers get their own explicit scopes.
- Reuse 10's terminal/input budgets. Inbox projections, hashing, Git commands, provider lookups and verification run outside the state actor/render path with bounded queues. Cache by authorized scope, source revisions and digests, not task title. Large diffs/logs load on demand.
- Proposed UI budgets on the existing reference machines: cached task/inbox opens ≤ 100 ms p95 for 100 tracked tasks/20 live runs; a durable source change appears in an attached local inbox ≤ 500 ms p95, remote within transport latency plus that budget. Show loading/coverage when uncached; never invent a current result to meet a budget.

## 12. Acceptance scenarios and product evaluation

| Scenario | Required result |
|---|---|
| Type Claude/Codex directly in a focused pane | Tracking is offered on user invocation; no modal, task record, cloud call or PTY input occurs automatically |
| Track a selected request with AI disabled | Manual task/criteria save works; original prompt is not resent |
| Suggest intent from a request saying "draft only" | Source instruction remains visible; suggested checks are separate; no authority broadening |
| Track a dirty, shared checkout halfway through work | Baseline limitations and checkout-wide attribution shown; no stash/reset/move; selection cannot fabricate independent verification |
| Archive/remove an attached task | Existing process/workspace/files/ports remain; owned-task cleanup still follows 05 |
| One CLI session, two tasks, `/clear`, and a subagent | Explicit boundaries; no reassignment of in-flight turns or old questions; uncertain links require user selection |
| Agent idle after a failed check or missing criterion | Turn finished/review available; never ready or reviewed solely from idleness |
| Agent says "tests pass" without observed execution | Claim shown as such; criterion remains unsupported |
| Check runs while files change and later revert | Start/end equality is insufficient; evidence unbound unless a stable execution subject was established |
| Browser still serves an old build | Current checkout SHA does not certify the screenshot; runtime binding incomplete |
| Agent adds a convenient test or changes check recipe | Test provenance visible; changed executable recipe requires authorization; no automatic independent-verifier label |
| Rebase or source edit during acceptance | Known version/subject conflict prevents current acceptance; acceptance always names immutable content; later observed changes invalidate current acceptance visibly |
| Accept with missing required SSO check | Explicit criterion exception/reason; reviewed with exceptions; no fabricated pass/merge |
| Answer in native TUI while inbox card is open | Source resolution disables stale action; first-writer/reconciliation behavior from 04 applies |
| Server dies after sending a clarification/starting verification | Reconcile; no automatic duplicate send/check; uncertain outcome remains visible |
| Select five-minute view with a large urgent decision | Urgent item remains surfaced; estimate and omitted-item count visible; no automatic answer |
| Inbox reorders while typing, snooze meets deadline, batch partially fails | Draft/focus retained; material wake reason shown; each batch delivery has its own outcome |
| Remote offline, event cursor truncated, DB full, provider down | Honest source/coverage state; no stale remote action; normal terminal operation survives |
| Unauthorized client or agent requests review/intent mutation | Existing scopes enforced, no content leakage or privilege widening |
| Purge source/evidence or revoke model consent | Derived content/cache removed or invalidated; no new model call under revoked grant |
| Alias bypasses Codex identity integration | Suggested binding cannot attach observations/actions to a task; verified Link required |
| Local client tracks a remote run; link drops during acceptance | Owner stays remote; client reconciles unknown result by idempotency key, never creates a second acceptance locally |
| Same-session reboot resume, compaction, different-session resume | Verified same-session continuation preserves binding; compaction has no effect; new identity prompts explicit association |
| Track after three files were already edited | Proposed review diff still includes them; tracking-time snapshot is attribution metadata; preexisting content is labeled |
| Auto-check recipe on host or task edits package scripts | No host auto-execution; per-candidate authorization and changed-definition provenance required |
| Checks pass while a run works, question is open, or send is uncertain | Review available with state; Ready remains unavailable |
| Red then green on different subjects versus identical subject | Fixed revision can pass; same-subject inconsistency remains visible and needs judgment |
| Switch attached task; send with another client focused or draft present | Resource/broker/env scope unchanged; unsafe paste refused with zero bytes |
| Prune sync/item history after seven days | Acceptance history and bounded intent source remain per their policies; purged content is not reconstructed |
| T2 dirty checkout; inspect subject X while live checkout becomes Y | Inspect-only until a committed candidate is selected; accept-capable diff and acceptance both name X, never the mutable live diff |
| Task A ends and task B or an untracked shell writes in the same checkout | A pins its end candidate or reports none; no absorption of B's edits and no live Ready while a known writer is active |
| No tracked tasks and no pending Interactions | Next attention still reaches the oldest unseen done run |
| Contained task requests host verification | Default retains containment; explicit host escape confirmation lists newly reachable resource classes |
| Owned checkout cleanup and client crash after acceptance dispatch | Attached records lose live availability but preserve history; restarted client reconciles the original durable idempotency key |

Test with recorded harness fixtures and isolated repositories before opt-in live Claude/Codex smoke tests. Include both directly typed and Vibeke-launched sessions; capability results apply only to tested versions/modes. No requirement that every harness natively answer every interaction.

Run product evaluation against the existing workflow using comparable tasks and blinded review where practical. Proposed targets, not measured results:

- Median active human minutes per accepted task reduced by at least 50%, including task setup, decisions, review and rework; report task category/size and sample counts. For this slice, **accepted** means a `ReviewAcceptance`, not merge; report exceptions separately. This is the canonical whole-task metric for T1–T4, distinct from 00/10's existing review-only/runtime gates.
- No increase in escaped defects or reverts within a defined follow-up window (initially 14 days); report uncertainty and small-sample limitations.
- Track time/steps to first tracked task, abandonment of tracking, incorrect requirement extraction, false readiness, missed urgent items, unwanted focus changes and unconfirmed sends separately.
- Zero unsupported automatic passes/acceptances, silent sends or focused-pane interference in the acceptance corpus.
- Structured-only workflow remains usable and meets the same correctness criteria with the assistant disabled.

These augment 00/10's metrics. They do not replace the current Goal 01 release gates or claim that model-generated summaries alone save time.

## 13. Delivery slices and integration with existing specs

| Stage | Deliverable | Exit condition |
|---|---|---|
| T1 — Optional tracking | Manual intent, attached-task ownership, explicit turn/conversation bindings, task detail view, clarification drafts/sending | Normal CLI journey and lifecycle tests pass; no LLM/browser dependency |
| T2 — Evidence-backed review | Committed subjects, observed command table, per-candidate verification, criterion mappings, acceptance/conflicts/exceptions, basic review items in the existing inbox | No false readiness on stale/ambiguous evidence; complete local/SSH review loop; dirty work remains reviewable with limitations |
| T3 — Attention workflow | Deterministic ranking/explanations, coarse five-minute shortlist, snooze, stable selection and conservative batching; no dependency graph required | Decision corpus and product attention measurements support wider rollout |
| T4 — Optional assistance and richer artifacts | Intent suggestions, review prose, effort estimates through 14; validated dirty/patch snapshots; browser runtime evidence through 06; optional reviewer runs and confirmed dependency links | Grounding/privacy/cost/capture evaluation passes; deterministic paths remain available |

*T4 status (2026-10-06), server/API/CLI:* **built** — intent suggestions and review prose (14 `suggest_task_details`, `review_summary`), effort estimates (14 `effort_estimate` + deterministic heuristic, §8.2), validated dirty snapshots with acceptance and verification (§5, §6.3), browser runtime evidence (§6.4, Goal 03 Stage 4), reviewer runs with notes (§6.1), confirmed dependency links in ranking (§8.1). **Built by lane 3F** — PR evidence (observations bound to the exact head, claims, unknown on failure) and execution-interval binding of observed agent commands (§6.3, §6.4 as-built notes). **Not built** — history-based estimates (selected-patch snapshots, end-candidate snapshots at binding close and a disposable reviewer checkout were added by lane 2C). **TUI** — built in task details (`vk-tui::tasks_t4`; see the *As built (T4 TUI)* note below). **Exit condition** — the grounding/privacy/cost/capture evaluation has not been run; only fixture tests with a fake provider and temp repositories exist.
*Lane 2C status (2026-10-06):* **built** with fakes only — selected-patch snapshots and dirty end candidates (§5), disposable reviewer checkouts (§6.1), human review recording (§6.4), forget of derived objects (§11), native deadlines with a configurable window, server-side batching, the Also working footer and inbox notifications respecting presence and quiet hours (§8), the Link run step (§3: `task.link.status {run | pane}` explains why a run is not verified, lists remedies — install the integration through setup, or start the agent through Vibeke — and verified runs nearby; the Track form shows it and `enter` tracks a chosen verified run; nothing is bound or installed implicitly). Tests: `crates/vk-server/src/review/lane2c_tests.rs`, unit tests in `vk-review::{selection, scratch, attention}` and `review::attention_ext`, TUI `tasks_2c_tests.rs` and inbox tests. Still open: execution-interval binding (3F), PR evidence (3F), history-based effort estimates (product decision), contained auto-verification and the host-escape confirmation (spec 13), the "Start a fresh task from here" offer, owned-cleanup enumeration of attached tasks (§4.3), live verification.
*As built (T4 TUI, `vk-tui::tasks_t4`, task details):* keys appear only when the review package carries the T4 fields (an older server answers each key with "needs a newer server"). **`s` Snapshot** sends `task.review.snapshot` (idempotency key persisted first; one in flight); success shows the server's label and refreshes; `workspace_changing` opens a screen headed **Workspace changing — verification subject unavailable** ("Nothing was recorded", `s` snapshot again); `nothing_to_snapshot` and `snapshot.available = false` say so without sending. A `dirty_snapshot` subject shows "Snapshot of uncommitted work · accept-capable while the checkout still matches it" (or "Earlier snapshot … inspect only" when not current) with its commit, and **Mark reviewed** accepts it like a committed candidate. **`R` Request reviewer** calls `task.review.request_reviewer`, then shows the harness, the provider-usage warning and the **exact prompt** in an editor; `ctrl+s` starts it with `task.review.start_reviewer {request, prompt_digest}`. An edited prompt is first recorded as a new request carrying the user's text and is started only if the server's recorded prompt equals what was shown (otherwise it is shown again); `ctrl+r` resets, `esc` launches nothing; `prompt_mismatch` launches nothing. **`N` Review notes** (`task.review.notes`) lists findings as "Reviewer finding · agent opinion, not evidence" with severity, classification and the reviewer run/turn; `b` blocking, `n` not blocking, `x` dismiss each ask for a reason (required for a dismissal, refused locally when empty) and send `task.review.note.classify`. **`d` Dependencies** (`task.dependency.list`) lists "waits for #h title (kind)" and "#h title waits for this (kind)" with "blocks N linked tasks"; `a` (this task waits for…) / `A` (…waits for this task) open a filterable picker over the machine's open tasks (`tab` blocks/related) and send `task.dependency.add`; a refused cycle shows its path as task names; `x` then `y` sends `task.dependency.remove`. **Effort**: the detail shows the user's value ("ranking uses only this"), the heuristic estimate and any model estimate, each with its source and "not applied"; **`f`** chooses a value (`enter`, source `user`), `h` applies the heuristic (`effort_source: heuristic`), `m` the model estimate (`assistant:<request>`); **`E` Estimate effort** runs 14 `effort_estimate` through the assist flow (preview → `y` → result), keeps the result on the task view unapplied, and `enter` in the result applies it with `task.set`. Inbox rows append "· blocks N linked tasks" (unless the explanation already says it) and the detail shows the heuristic estimate with its source when effort is unset. Verified with fake control-stream replies (state machines and draw tests); not yet driven against a live server.

T1–T3 form the first complete product slice after Goal 01; they do not wait for a new mobile client or an autonomous planner. Selected components may ship incrementally once their dependencies are ready. Browser/runtime verification in T4 depends on the preview workstream, not on an assumed reordered M3.

On implementation, update canonical specs together:

| Existing specification | Required integration |
|---|---|
| 02 Task/Workspace/AgentRun | Ownership versus attachment, versioned intent, binding history, review objects; keep execution/read state separate |
| 04 adapters | Tested source visibility, native conversation boundaries, supported send/reconcile paths; no new inferred authorization |
| 05 task lifecycle | Owned versus attached cleanup/park semantics; selected baseline and shared-checkout limits |
| 06 preview evidence | Separate running-build and checkout identity; unbound artifacts remain illustrative |
| 07 API | Add the proposed methods/scopes/idempotency/preconditions and generated schemas |
| 08 UX/config | Three surfaces, optional tracking, task-aware labels, stable inbox selection, accessible textual status and action availability |
| 09 privacy | Derived-object retention/purge and verifier/assistant access without widening grants |
| 10 quality | Acceptance fixtures and proposed human-time/defect evaluation, preserving current runtime budgets |
| 11/12 roadmap | Existing M1 interaction list evolves in place into the task/review/effort inbox under this staged specification |
| 14 assistance | Optional operations using existing generation lifecycle and source grants; no autonomous sending/check execution |

Until these slices are implemented, 02/05/07/08 remain the canonical shipped-interface targets for the current goal. This document defines the proposed additions and the deliberate future changes, not silently effective API/config behavior.
