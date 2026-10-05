# 15 — Task outcomes, review packages and the attention inbox

**Status:** proposed product specification, based on the accepted UX direction of 2026-10-06. This specifies an additional product slice; it does not claim implementation or expand [milestones](11-milestones.md). The current SSH daily-driver goal remains first. Delivery stages in §13 are separate from the Phase 1 milestones.

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
- **Launching through Vibeke is optional.** A directly typed CLI inside a Vibeke pane has the same supervision capabilities as the same tested harness/version/mode launched with `agent.start`.
- **Tracking is optional.** Untracked agents retain status, interactions, peek/reply and basic observed changes/checks. The user can ignore task tracking indefinitely.
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
Baseline    Current captured state; earlier changes need selection

[Track task]  [Edit]  [Cancel]
```

One confirmation saves the intent. Suggested checks are not silently made mandatory; the user can select them or rely on an already trusted project recipe. The goal and constraints themselves are still criteria requiring appropriate evidence or human judgment. Users can always type the fields manually.

Tracking creates a record and run binding. It does not replay the original request, stop the process, relocate files, create a branch or mark existing work as owned by this agent. **New isolated task** is a separate command using 05's existing creation flow.

### 2.3 Clarify intentionally

Editing task intent opens a draft. **Save details** records a new revision. If the change affects what the agent should do, show **Save and send clarification**, with the exact message and recipient. Record-only edits show a persistent **Agent has not been told about revision N** label until an explicit message is delivered or the user links an already-sent instruction.

Saving remains useful if sending fails. The UI reports the saved intent and delivery result separately. No automatic retransmission occurs after an ambiguous result (§9).

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

A completed turn changes the agent row to **Turn finished**. Task details are available even while the agent works; an idle signal alone never says the task is ready.

```text
Fix login redirect                         Review available
Draft PR #123 · revision abc123 · checked 2m ago

Changes
  Preserves the destination through login and validates
  it before redirecting. [Source: diff]

Requirements
  PASS     Return to original page
           Redirect regression passed on this revision
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

## 3. Entry modes and graceful degradation

| Situation | Available experience | Limit or fallback |
|---|---|---|
| User types `claude`/`codex` inside a Vibeke pane | Detection, optional tracking and full supported integration | Actual capability is harness × version × mode × interaction kind, per 04 |
| `vibeke task new` launches a harness | Same task details/inbox; can record intent before launch | Confirmed initial prompt is sent once through the existing launch flow |
| User already has an existing worktree | Adopt it and optionally bind its run | Adoption does not move files or assume ownership of preexisting changes |
| Structured state, but no readable transcript | State and actual Interactions; manual intent | No synthesized task request or claimed complete conversation history |
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
  stop_at: implementation|draft_pr|reviewed_pr|merge|verified_deployment|custom,
  stop_detail?, source_refs, confirmed_by, confirmed_at
}
```

- `source_refs` identify machine/session/run/turn/item and, where applicable, source digests. Handwritten requirements have user provenance. A generated draft also stores its assistant result reference.
- `stop_at` records intent. It cannot grant or enforce merge/deploy permissions. Unsupported external outcome verification remains manual/unknown and the UI says so.
- Every criterion has a stable ID; changed meaning creates a new criterion version. Optional checks never block readiness. Constraints requiring judgment become explicit human criteria rather than disappearing into a summary.
- Extraction preserves scope and negatives such as "draft PR only". If the source is ambiguous, show an editable question; do not silently choose the more permissive interpretation.
- Changes create immutable intent revisions. Human acceptance of an older revision is retained as history and cannot satisfy the new one automatically.

### 4.2 A task and a session have different lifetimes

One task can involve several runs. One long-running CLI session can work on several successive tasks. New prompts usually refine the current task; they do not automatically create one task per turn.

**Start another task in this session** lets the user close the current binding and select the next request. A default binding switch takes effect at the next turn boundary. While a turn is running, a switch is queued and shown as pending; it does not reassign that turn's checks/interactions. Historical turn-range edits require explicit selection and invalidate affected derived reviews. Overlapping foreground-task bindings for the same turn are refused.

`/clear`, `/new`, `/resume` and forks are explicit context boundaries even where 04 preserves the same `AgentRun` ID. Preserve history, suspend automatic association for the new native conversation, and offer **Continue task** or **Track new work**. Pending interactions retain the task association they had when opened. A harness process replacement creates a new run and requires an explicit task link; matching cwd/title/prompt is insufficient.

Subagents can inherit the parent task binding only through a structured parent identity and the parent's binding at spawn. Ambiguous work remains unassigned. User-selected linking is always available.

### 4.3 Workspace ownership and lifecycle

Add `workspace_ownership = owned | attached`:

- `owned`: a workspace/checkout created for the task by `task new`; 05's park/archive/remove behavior applies, with its dirty/unpushed protections.
- `attached`: tracking work in an existing pane/workspace. Archiving/removing this task record never stops its agent, deletes its checkout, releases another task's ports, or closes the workspace. **Stop agent** is a separate explicit action. `task park` parks the record only; it shows that the process continues.

Several attached tasks can refer to one workspace over time. `Workspace.task_id` remains the optional owning task, not a universal task lookup. `AgentRun.task_id` becomes a compatibility projection of its current binding; the binding history is authoritative for task attribution.

Task lifecycle remains `active | parked | finished | archived`. Tracking status, review readiness and acceptance are independent. `task.finish` may finish without acceptance, but must record **Finished without review**; automation must not interpret lifecycle `finished` as verified success. Readiness or acceptance never triggers cleanup.

## 5. Change subjects and attribution

Tracking captures a baseline immediately: repository identity, resolved base/head, complete staged/unstaged/untracked change digest, observation time and selected source range. This baseline records what existed when tracking began; it does not prove who made it.

If work began earlier, show **Earlier changes not yet selected** and offer an explicit review base or user-selected patch. Do not imply a task-start baseline can recover the already-modified original state. Repositories without usable VCS can track intent and interactions; revision-based readiness is unavailable unless an explicit file snapshot is captured.

`ChangeSubject` is an immutable, content-addressed description of what is under review: repository identity, base SHA, head SHA, dirty digest when applicable, selected patch/snapshot digest, and capture metadata. Full dirty capture includes staged, unstaged, binary and untracked content; ignored runtime inputs relevant to verification belong in the environment manifest. Missing required capture data yields `unbound` evidence, not an empty/clean digest.

For a shared checkout, label the view **Changes in this checkout**. File events provide hints only. A user-selected patch defines review scope, but checks against the whole checkout retain that whole-checkout subject and are not presented as proof the selected patch passes independently. An isolated verification snapshot can establish that separately.

Never run a destructive Git operation to obtain a baseline. Background capture is bounded and off the terminal/state-actor hot path. If consistent capture cannot be established while writers are active, show **Workspace changing — verification subject unavailable** and let the user select a stable commit or request a snapshot.

## 6. Review package and verification

### 6.1 Package contents

A `ReviewPackage` contains the exact intent revision and change subject, links to the source request and diff, current criterion assessments, observed checks, artifacts, attributed findings, external outcome observations, and any previous human acceptance. It is a projection with a revision and source cursor vector, not a second source of truth.

The default view answers, in order: what was requested; what changed; what supports each requirement; what is missing or disputed; which action is available. Optional generated prose cites the underlying objects and follows 14. The deterministic view always works.

Criterion assessments are `supported | failed | missing | stale | needs_judgment | unknown`. A check mapping is configured/confirmed by the user or a trusted project recipe. A model may suggest a mapping but cannot certify semantic coverage. Human-reviewed criteria record the actor and inspected subject. Preserve conflicting results; a later successful retry does not hide a failure or flakiness. A configured aggregation rule determines the latest verdict for an identical check definition/environment; default is **needs judgment** after mixed outcomes.

A reviewer agent's finding contains source references and provenance. It cannot mark its own task accepted, waive required checks or override a failed observation. Running a reviewer is explicit or separately preauthorized; it consumes the selected provider's usage.

### 6.2 Observed versus independent checks

Use three visible categories:

- **Agent-reported claim:** prose or an unconfirmed report; never a passed check.
- **Observed agent check:** a bound tool/process execution, exact command/cwd, start/end, result, logs and subject/environment information. Missing native fields remain unknown. The harness adapter is an attribution source, not a trusted verifier against an adversarial agent.
- **Vibeke verification:** a configured check launched in a separate verification run against a stable captured subject. Record who selected the check and whether its definition was supplied by the task's changes or a trusted baseline/user recipe. Separate execution does not make a task-authored test independent in design.

### 6.3 Check authorization and execution

Checks are arbitrary code. **Run missing check** shows command, execution machine, environment, scope and expected side effects before first authorization. Reuse already-authorized matching project recipes; changed commands, runner permissions, endpoints or recipe digests require renewed authorization. A repo file or agent suggestion cannot authorize itself. User-level **Run these checks when a candidate is ready** is off by default and scoped to the exact recipe/policy.

Verification uses a disposable checkout/snapshot on the task's machine or an explicitly selected runner. It does not reset/stash the active checkout or silently run remotely requested commands on the laptop. Dependencies/fixtures and any reused services are recorded; shared mutable services prevent claims of hermetic or fully independent verification.

Snapshot capture requires a stable subject, including dirty/untracked content when selected. Capture must detect concurrent writes; checks performed against a changing live checkout are labeled unbound unless the runner can establish the complete subject for the execution interval. Matching start/end hashes alone cannot exclude intermediate changes. The UI offers a stable commit or isolated snapshot when it cannot bind evidence.

Check execution is explicit and bounded: concurrency, timeout, log size and output retention follow the existing runner limits. State is `queued -> running -> passed|failed|cancelled|interrupted|unknown`. Submission has caller-scoped idempotency. On restart reconcile the holder/runner before recording an outcome; never launch a duplicate external-side-effecting check automatically. Cancellation records partial output and never fabricates a failure/pass.

### 6.4 Browser and external outcome evidence

Screenshots retain 06's environment labels. Add runtime identity separately from checkout identity: launched server/build ID or digest, producing subject, process start, browser session/device, data fixture identity and capture time. A checkout SHA observed at screenshot time alone cannot bind an old server's response to new code. If runtime identity is unavailable, show **Build not verified**; the image is illustrative and cannot satisfy a required check for the current build.

PR evidence includes provider/repository/PR identity, target branch, head revision, draft state, observation time and authorization scope. A pasted URL or agent statement is not confirmation. Changed PR head invalidates the observation for the old subject. Failed/offline lookups remain unknown. Merge and deployment observation are extension points only; this slice has no release executor.

## 7. Readiness, acceptance and freshness

| Label | Meaning |
|---|---|
| Turn finished | A turn completed; no statement about the task outcome |
| Review available | A package exists; it may contain missing/failed/stale evidence |
| Ready for your review | The intent is confirmed, the subject is stable/current, all required machine-verifiable criteria have supporting evidence, and no failed/unknown required criterion or known blocking finding remains; required human judgment is presented for review |
| Reviewed | The user accepted this intent revision and subject, including any explicitly recorded exceptions |
| Review outdated | The accepted intent, subject, required environment/check definitions or relevant external outcome no longer matches current state |

Known blocking findings are explicit user-marked blockers or trusted check failures. Generated reviewer findings are visible concerns requiring classification; they cannot silently change permissions or task state. An unassessed potentially blocking review concern prevents the stronger readiness label until resolved, dismissed with attribution, or explicitly excepted by the user.

Freshness is evaluated from the complete evidence subject, relevant environment and check-definition digests, intent revision and live source coverage. A rebase invalidates SHA-bound evidence even if a model says the code is equivalent. No semantic-equivalence shortcut in the initial implementation. Dependency/fixture changes invalidate affected checks; unavailable environment identity yields unknown freshness.

**Mark reviewed** sends expected intent revision, package revision, subject digest and any exceptions. The owner server revalidates them atomically with recording acceptance. A competing edit returns `review_conflict` and refreshes the view. Subsequent changes invalidate current acceptance without deleting its history. Offline/gapped sources cannot produce a new acceptance; users may save a local review draft for later submission.

Missing/failed required criteria require explicit exceptions naming each criterion and a reason. The UI says **Reviewed with exceptions**, never turns them green. Acceptance is distinct from marking the package seen, answering an approval, finishing a task or granting merge permission.

## 8. Attention inbox

### 8.1 Items and ordering

Build a deterministic projection over authorized source objects: live Interactions, delivery failures/unknowns, actionable run/setup/check errors, and review packages with an unreviewed candidate. Routine working runs stay in the sidebar; the inbox has an optional **Also working** footer. A review candidate is created on an explicit user request or a settled-turn checkpoint with a changed subject; streaming output/file events do not create notification floods.

Use stable object IDs and revisions for deduplication. Group related items by task, while preserving each actual Interaction and action. A review item is keyed by task plus candidate subject; reading it marks seen, not accepted. Dismissed errors reopen only on a new occurrence. A task without confirmed intent can show **Changes to inspect**, never verified readiness.

Order by this precedence, with user pinning within a class and age as a stable tie-breaker:

1. Unconfirmed/failed decision delivery and actionable high-impact errors.
2. Open Interactions with a native deadline approaching; show the actual deadline.
3. Other blocking decisions, ordered by explicit task priority, confirmed dependent tasks and waiting time.
4. Review candidates, ordered by task priority and age.

Risk is displayed and raises prominence within a class; risk alone must not imply that approval is recommended. Dependency counts come only from user-confirmed task links; inferred links are suggestions and excluded from ranking. Reject cycles in confirmed dependency links. A tooltip explains ordering in plain language, for example "Waiting 12m; blocks two linked tasks."

Resort on meaningful changes with a short debounce. Preserve the selected object and scroll position while the user reads/types; newly urgent items get an indicator rather than replacing the selected card. A resolved Interaction disables its submit action immediately and shows who/what resolved it.

### 8.2 Five-minute view

**5-minute view** creates a suggested, stable working set; it is not a timer that dismisses unanswered work. Always surface urgent/unknown delivery items, even if their estimated effort exceeds the budget. Show **Urgent — may take longer**. Other candidates prefer high unblock impact and lower estimated review effort while retaining an **All items** count and access.

Effort starts as user-set `quick | a few minutes | deep review | unknown`. Optional model/history estimates are labeled estimates with a source; no precise countdown or guaranteed finish time. Unknown effort is eligible and is never silently hidden forever. Omitted tasks remain visible in All items and gain age priority. The UI states that the suggested set may exceed five minutes.

### 8.3 Snooze, batching and notifications

Snooze is per user and item revision: until a chosen time or review window. Never resolve/cancel the source or tell the agent an answer was given. A deadline, risk escalation, delivery uncertainty or material revision wakes the item with an explanation. Changes to unrelated logs do not. This single-user slice does not implement team snooze or ownership.

Respect existing presence and quiet-hour settings. A snoozed item may remain pending in a harness with a shorter native timeout; show that deadline before accepting the snooze. Offline machines have a persistent coverage indicator and last-observed time. Reconnect reconciles objects before enabling actions; cached cards never authorize queued automatic decisions.

Batching is limited to 04/08's supported native approval cases. Sharing a normalized command/fingerprint is insufficient: the effective policy scope, execution environment, operation and resource targets must be equivalent and each decision must still be live. Show the members and record/deliver individually, including partial failures. Never batch product questions, plan reviews, high-risk/unknown-risk actions, or human task acceptance.

## 9. Sending messages and answering safely

Opening a task, tracking it, editing intent, generating a draft and marking a review seen send no bytes/messages to any harness. Explicit Send/Answer actions re-resolve run identity, native conversation, current task binding and capability. If the run moved to another task or conversation, refuse stale send and offer target selection.

Interaction answers use the existing decision revision, idempotency key, lease and reconciliation semantics from 04. For freeform follow-ups, record a `TaskMessage` with caller idempotency key, intended binding/intent revision, exact text, actor, native request ID if any, and `prepared | sending | delivered | delivery_unknown | failed | cancelled`. Do not claim exactly-once native delivery where the harness cannot prove it.

When native follow-up/steer is supported, use it. Otherwise an explicitly authorized send can use the tested prompt-input path from 07 only at a verified idle prompt. If readiness or identity cannot be established, show the draft and **Open pane to send**. Never type into a running tool, permission dialog or arbitrary terminal screen. No automatic steering just because task details changed.

After an ambiguous send, preserve the draft/result and reconcile if supported; do not automatically resend. A manual retry warns that the earlier message may have arrived. A native delivered acknowledgment proves receipt, not that the agent understood or complied with the instruction.

## 10. Data, API and persistence additions

These are proposed schema extensions, not currently supported configuration or API. On implementation, merge canonical types into 02/07/08. Existing transport, ownership, authorization and outbox conventions remain authoritative.

### 10.1 Stored objects

| Object | Essential fields and ownership |
|---|---|
| Task extension | `owner_machine/session`, `workspace_ownership`, `current_intent_revision?`, explicit priority; existing lifecycle remains |
| TaskIntent | Immutable confirmed revisions per §4; drafts stored separately and never treated as authority |
| TaskRunBinding | Task/run/native-conversation identity, selected start/end turn or source cursors, effective boundary, role `implementation|review|verification`, actor; at most one foreground binding per run turn |
| ChangeSubject | Immutable content identity and selected review scope per §5 |
| CheckDefinition / CheckRun | Recipe revision/trust grant, argv or explicit shell command, cwd/env/runner scope, subject, lifecycle, exact observations and log references |
| ReviewPackage / Assessment | Intent/subject, projection revision, source cursors/coverage, criterion result and supporting references; regenerate only from authorized inputs |
| ReviewAcceptance | Task, exact intent/package/subject, actor/time, exceptions, current or outdated status derived from live state |
| TaskMessage | Intended recipient/binding, text reference, idempotency/delivery state per §9 |
| InboxPreference / Dependency | Per-user seen/snooze/pin state tied to subject revision; explicit confirmed dependency edges with provenance |

Extend 12's `EvidenceRecord` with check-definition identity, subject identity, collection category, runtime/build identity where applicable and source coverage. Existing evidence lacking these additions remains readable as **Legacy evidence — binding incomplete** and cannot automatically satisfy stronger readiness. No destructive rewrite of historical events.

### 10.2 Proposed API surface

All mutations accept caller-scoped idempotency keys and expected object revisions where relevant. Errors include `conflict`, `binding_changed`, `source_unavailable`, `unsupported_capability`, `verification_unbound`, `review_conflict`, and existing permission/storage failures.

| Method | Behavior |
|---|---|
| `task.track` | Atomically create an attached task, confirm intent and bind selected source range; no spawn/send/setup |
| `task.intent.get/update` | Read or create a confirmed intent revision; update is record-only |
| `task.bind` / `task.unbind` | Explicit association and boundary handling; never process relocation |
| `task.message.prepare/send/get` | Preview recipient/text, explicitly send, inspect delivery; no coupling of save success to send success |
| `task.review.get` | Deterministic package with source cursor vector and freshness; no model call or check execution |
| `task.review.accept` | Atomic acceptance of expected revisions/subject with explicit exceptions |
| `task.check.list/run/cancel` | Read defined checks and explicitly authorize/submit/cancel verification |
| `task.dependency.add/remove` | Confirm explicit dependency edges; cycle checks and normal mutation authorization |
| `attention.list` | Ranked items, coverage and optional effort budget; deterministic by default |
| `attention.update` | Set seen/snooze/pin for exact user/item revision; never answer or accept |

Optional extraction, explanation and review prose are named operations on 14's `assistant.generate`, governed by that feature's limits. Generated outputs cannot call mutation methods. Existing `interaction.answer` remains the only interaction-answer API. CLI and future mobile/web use the same methods; no new default keybindings beyond actions under the existing peek, palette and inbox.

### 10.3 Atomicity, remote ownership and recovery

The task's owner server is authoritative for intent, bindings, subjects and acceptance. Multi-machine views aggregate authorized projections with independent source cursors. Remote observations are never silently relabeled local. Cross-machine runs attach with explicit qualified identities; if sources cannot be revalidated, acceptance is unavailable rather than inferred from cached data.

Persist mutations and their events in the same SQLite transaction. New event families include `task.intent_updated`, `task.binding_changed`, `task.message_*`, `check.*`, `review.candidate_created`, `review.accepted`, `review.invalidated`, `attention.preference_changed`, `task.dependency_changed`. Events contain IDs, revisions and metadata rather than full prompts, generated text, secrets or logs. Blob content follows 09.

On reconnect, recover authoritative state before applying pending UI actions. `events.truncated` forces a fresh snapshot; do not continue ranking as if a missing interval were observed. On disk-full, refuse durable edits/answers/acceptance consistently with 02; terminal interaction continues. Transient generation failures do not affect task records or answering.

## 11. Privacy, authority and performance

- Source content is authorized before retrieval, projection, search or model use. A session-wide inbox reports excluded/offline scope without leaking its contents. A task link or dependency never widens access to another workspace's conversation.
- Agent/adapter tokens can report their own observations through existing APIs. They cannot confirm intent, accept reviews, waive checks, alter priorities/dependencies or authorize verification. These new mutations require an explicit human-client scope; future automation grants require a separate design.
- AI assistance needs 14's workspace/connection/context consent. No background extraction or model request occurs just because a CLI launched or a turn ended. User-configured automatic assistance, if later added, requires separate opt-in.
- Apply 09's storage permissions and operational-content rules. `forget` removes scoped drafts, messages, derived packages and cached excerpts as well as underlying sources; retained references become unavailable rather than resurrecting purged content. Required evidence purged from a package changes readiness to unknown. References may pin blobs only within the user's declared retention policy; the UI explains expiration before acceptance history loses inspectable evidence.
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
| Rebase or source edit during acceptance | Expected-version/subject conflict prevents stale acceptance; later changes invalidate prior acceptance visibly |
| Accept with missing required SSO check | Explicit criterion exception/reason; reviewed with exceptions; no fabricated pass/merge |
| Answer in native TUI while inbox card is open | Source resolution disables stale action; first-writer/reconciliation behavior from 04 applies |
| Server dies after sending a clarification/starting verification | Reconcile; no automatic duplicate send/check; uncertain outcome remains visible |
| Select five-minute view with a large urgent decision | Urgent item remains surfaced; estimate and omitted-item count visible; no automatic answer |
| Inbox reorders while typing, snooze meets deadline, batch partially fails | Draft/focus retained; material wake reason shown; each batch delivery has its own outcome |
| Remote offline, event cursor truncated, DB full, provider down | Honest source/coverage state; no stale remote action; normal terminal operation survives |
| Unauthorized client or agent requests review/intent mutation | Existing scopes enforced, no content leakage or privilege widening |
| Purge source/evidence or revoke model consent | Derived content/cache removed or invalidated; no new model call under revoked grant |

Test with recorded harness fixtures and isolated repositories before opt-in live Claude/Codex smoke tests. Include both directly typed and Vibeke-launched sessions; capability results apply only to tested versions/modes. No requirement that every harness natively answer every interaction.

Run product evaluation against the existing workflow using comparable tasks and blinded review where practical. Proposed targets, not measured results:

- Median active human minutes per accepted task reduced by at least 50%, including task setup, decisions, review and rework; report task category/size and sample counts.
- No increase in escaped defects or reverts within a defined follow-up window (initially 14 days); report uncertainty and small-sample limitations.
- Track time/steps to first tracked task, abandonment of tracking, incorrect requirement extraction, false readiness, missed urgent items, unwanted focus changes and unconfirmed sends separately.
- Zero unsupported automatic passes/acceptances, silent sends or focused-pane interference in the acceptance corpus.
- Structured-only workflow remains usable and meets the same correctness criteria with the assistant disabled.

These augment 00/10's metrics. They do not replace the current Goal 01 release gates or claim that model-generated summaries alone save time.

## 13. Delivery slices and integration with existing specs

| Stage | Deliverable | Exit condition |
|---|---|---|
| T1 — Optional tracking | Manual intent, attached-task ownership, explicit turn/conversation bindings, task detail view, clarification drafts/sending | Normal CLI journey and lifecycle tests pass; no LLM/browser dependency |
| T2 — Evidence-backed review | Stable subjects, observed check table, configured verification, criterion mappings, acceptance/conflicts/exceptions, basic review inbox items | No false readiness on stale/ambiguous evidence; complete local/SSH review loop |
| T3 — Attention workflow | Deterministic ranking/explanations, explicit dependencies, five-minute view, snooze, stable selection and conservative batching | Decision corpus and product attention measurements support wider rollout |
| T4 — Optional assistance and richer artifacts | Intent suggestions, review prose, effort estimates through 14; browser runtime evidence through 06; optional reviewer runs | Grounding/privacy/cost evaluation passes; deterministic paths remain available |

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
| 11/12 roadmap | Existing M1 interaction list remains; richer task/review/effort inbox follows this staged specification |
| 14 assistance | Optional operations using existing generation lifecycle and source grants; no autonomous sending/check execution |

Until these slices are implemented, 02/05/07/08 remain the canonical shipped-interface targets for the current goal. This document defines the proposed additions and the deliberate future changes, not silently effective API/config behavior.
