//! Schema registry entries for the Batch 4 orchestration methods and events (`orch.rs`), in
//! the shape language of `shape.rs`. `api_schema` loads [`SHAPES`] and [`EVENTS`] next to its
//! own tables. Records (families, goals, queue entries, VMs) are typed as `object` where their
//! full field list lives in `vk-orchestrate` / `vk-sandbox::vm`; the fields a client relies on
//! are spelled out.

pub const SHAPES: &str = r##"
# --- best-of-N (05 §12): all need [orchestrate.best_of_n] enabled ---
family.list :: {} => {families: [object]}
family.get :: {family: string} => {family: object, children: [{handle: string, harness: string, task: Task|null, run: object|null, discarded: bool}]}
# run the check command in each child (or one) and keep the outcome with the revision it ran against
family.check :: {family: string, child?: string} => {family: string, results: [{child: string, outcome: object}]}
# `task.create` with a string `agents` ("claude:2,codex:1") is the same call
task.best_of_n :: {title: string, agents: string|[string|object], repo?: string, base?: string, prompt?: string, prompt_file?: string, suffix?: string, check_command?: string, isolate?: string, yolo?: bool, network?: string, image?: string, setup?: bool, ports?: int, isolation?: string, checkout?: string, root?: string, fetch?: bool}
  => {family: object, tasks: [Task|null], runs: [AgentRun], warnings: [any]}
task.compare :: {family: string, pair?: [string]}
  => {family: string, state: string, picked: string|null, reports: [object], ranking: [{handle: string, score: int, reasons: [string]}], text: string, pair?: {a: string, b: string, summary: object}}
task.pick :: {family: string, child: string, merge?: bool, discard?: bool, force?: bool, target?: string}
  => {family: object, picked: string, merge: any, discarded: [{child: string, ok: bool, error?: string}]}

# --- split into task (05 §11): needs [orchestrate.split] enabled ---
task.split :: {run?: string, pane?: Target, workspace?: Target, paths?: [string], title?: string, slug?: string, dry_run?: bool = false, resume?: bool, keep_recovery?: bool}
  => {dry_run?: bool, source: string, changes?: [{path: string, staged: bool, unstaged: bool, untracked: bool, deleted: bool}], steps?: [{step: string, detail: string}], blocked_by?: [string]|null, runs: [object], task?: Task|null, moved?: [string], recovery_ref?: string, recovery_kept?: bool}

# --- merge orchestration (12): needs [orchestrate.merge] enabled; a pane may claim for its own task ---
task.claim :: {task?: Target, run?: Target, glob: string, note?: string, root?: string} => {claim: {id: string, task: string|null, glob: string, note: string|null, created_at_ms: int, run?: string, root?: string, kind?: run|task}, conflicts: [object], label?: string}
task.claim.list :: {task?: Target} => {claims: [object]}
task.claim.remove :: {claim: string} => {removed: string}
merge.predict :: {repo?: string, tasks?: [string]} => {conflicts: [{a: string, b: string, kind: overlap|textual|claim, severity: low|medium|high, paths: [string], detail: string}], tasks: [string], at_ms: int}
merge.queue.add :: {task: Target, target?: string, priority?: int = 0, note?: string, allow_dirty?: bool = false, run?: bool = false} => {entry: object, position: int|null, run?: object}
merge.queue.list :: {all?: bool = false} => {entries: [object], order: [string]}
merge.queue.cancel :: {entry: string} => {entry: object|null}
merge.queue.requeue :: {entry: string} => {entry: object|null}
# merges through an integration worktree; stops at the first entry that does not land
merge.queue.run :: {entry?: string, count?: int = 1, all?: bool = false} => {ran: int, results: [{entry: object, event: string, detail: object}]}

# --- goal planner (12): needs [orchestrate.planner] enabled ---
goal.create :: {title: string, text?: string, text_file?: string, repo?: string, base?: string, plan?: bool = true}
  => {goal: object, progress: {done: int, total: int}}
goal.list :: {} => {goals: [{goal: object, progress: {done: int, total: int}}]}
goal.get :: {goal: string} => {goal: object, progress: {done: int, total: int}}
# backend heuristic plans now; agent starts a planning run; external returns the prompt to give any planner
goal.plan :: {goal: string, backend?: heuristic|agent|external}
  => {goal: object, progress?: object, backend?: string, prompt?: string, submit_command?: string, plan_file?: string, planning_run?: string|null, agent_error?: string}
goal.plan_submit :: {goal: string, plan?: object|string, file?: string, planner?: string} => {goal: object, progress: {done: int, total: int}}
goal.approve :: {goal: string, by?: string, start?: bool = true} => {goal: object, progress: {done: int, total: int}}
goal.start :: {goal: string} => {goal: object, progress: {done: int, total: int}, started: [string]}
goal.step_done :: {goal: string, step: string, ok?: bool = true, error?: string} => {goal: object, progress: {done: int, total: int}}
goal.cancel :: {goal: string, stop_tasks?: bool = false} => {goal: object, progress: {done: int, total: int}, parked: [string]}
goal.briefing :: {since?: int|string, until?: int} => {briefing: {since_ms: int, until_ms: int, sections: [{title: string, items: [string]}], counts: object, text: string}}

# --- quota scheduling (12): needs [orchestrate.quota] enabled to tick ---
quota.status :: {} => {enabled: bool, accounts: [object], paused: [object], config: object}
quota.tick :: {dry_run?: bool = false} => {dry_run: bool, actions: [object]}
quota.route :: {harnesses?: [string]} => {ranking: [{harness: string, headroom: number, account: string}]}
quota.resume :: {key?: string, task?: string} => {resumed: string, handle: string}

# --- vm level (13 §2.1, §9): needs [isolation.vm] enabled ---
vm.status :: {} => {enabled: bool, provider: string, available: bool, detail: string, config: object, providers: [{provider: string, status: string, note: string}]}
vm.list :: {} => {vms: [{name: string, task: string|null, template: string|null, created_ms: int, state: string}], snapshots: [object]}
vm.create :: {name?: string, task?: string, checkout?: string, template?: bool} => {vm: object, fork_ms: int|null}
vm.start :: {vm: string} => {vm: string, state: string|null}
vm.stop :: {vm: string} => {vm: string, state: string|null}
vm.suspend :: {vm: string} => {vm: string, state: string|null}
vm.resume :: {vm: string} => {vm: string, state: string|null}
vm.destroy :: {vm: string} => {vm: string, state: string|null}
vm.snapshot :: {vm: string, label?: string} => {snapshot: object}
vm.snapshot.delete :: {snapshot: string, force?: bool = false} => {deleted: string}
vm.fork :: {snapshot: string, count?: int = 1, prefix?: string, tasks?: [string], checkouts?: [string]} => {vms: [object], ms: int}
vm.transport :: {vm: string} => {vm: string, offered: [string], chain: [string], configured: string, link: string}
vm.template.list :: {} => {templates: [object]}
vm.template.build :: {} => {template: object}
vm.template.delete :: {key: string} => {deleted: string}
"##;

pub const EVENTS: &str = r##"
family.created :: {family: string} => {title: string, children: [{handle: string, task: string, harness: string}], check_command: string|null}
family.checked :: {family: string, child: string} => {ok: bool, exit_code: int|null, timed_out: bool, duration_ms: int}
family.picked :: {family: string} => {picked: string, discarded: int}
task.split :: {task: string} => {source: string, moved: int, recovery_ref: string, recovery_kept: bool, runs: int}
task.claim_added :: {task: string, claim: string} => {glob: string, note: string|null}
task.claim_removed :: {task: string, claim: string} => {glob: string}
merge.conflict_predicted :: {a: string, b: string} => {a: string, b: string, kind: string, severity: string, paths: [string], detail: string}
merge.queued :: {entry: string, task: string} => {handle: string, target: string, priority: int}
merge.merged :: {entry: string, task: string} => {handle: string, target: string, commit: string, already: bool, via: string}
merge.conflict :: {entry: string, task: string} => {handle: string, target: string, paths: [string]}
merge.check_failed :: {entry: string, task: string} => {handle: string, target: string, exit_code: int|null, timed_out: bool}
merge.blocked :: {entry: string, task: string} => {handle: string, target: string, reason: string}
merge.failed :: {entry: string, task: string} => {handle: string, target: string, error: string}
merge.cancelled :: {entry?: string|null} => {handle: string|null}
merge.requeued :: {entry?: string|null} => {handle: string|null}
goal.created :: {goal: string} => {handle: string, title: string}
goal.planning_started :: {goal: string} => {harness: string, run: string|null}
goal.planned :: {goal: string} => {steps: int, planner: string, rev: int}
goal.approved :: {goal: string} => {by: string, rev: int}
goal.step_started :: {goal: string, step: string} => {task: string, harness: string, reasons: [string]}
goal.step_waiting :: {goal: string, step: string} => {reason: string}
goal.step_finished :: {goal: string, step: string} => {status: string, error: string|null}
goal.finished :: {goal: string} => {state: string}
goal.cancelled :: {goal: string} => {stop_tasks: bool}
quota.paused :: {key: string} => {handle: string, account: string, reason: string, resumes_at_ms: int|null, task: string|null}
quota.resumed :: {key: string} => {handle: string, reason: string, task: string|null}
vm.created :: {vm: string} => {provider: string, template: bool, fork_ms: int|null}
vm.started :: {vm: string} => {state: string|null}
vm.stopped :: {vm: string} => {state: string|null}
vm.suspended :: {vm: string} => {state: string|null}
vm.resumed :: {vm: string} => {state: string|null}
vm.destroyed :: {vm: string} => {state: string|null}
vm.snapshot_created :: {vm: string} => {snapshot: string, memory: bool}
vm.forked :: {snapshot: string} => {vms: [string], ms: int}
vm.template_built :: {template: string} => {snapshot: string, setup_digest: string}
"##;
