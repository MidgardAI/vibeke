# 05 — Tasks, isolation and worktrees

A **task workspace** is the default way to give an agent somewhere to work. One command produces an isolated checkout, a branch, a collision-free port range, an env, a finished setup script and a running agent. Running several agents in one shared cwd stays possible, because people do it (the maintainer's samplehub workspace has 2 Claude + 1 Codex in one directory). In that case Vibeke watches for collisions and offers a one-step migration into tasks.

Implemented in crate `vk-tasks`. Data model: `Task` in [02](02-data-model-and-event-log.md) §1.1. Milestone: **M3** (shared-cwd collision tracker: **M2**, since it only needs adapter events).

## 1. User-facing commands

```
vibeke task new "fix login redirect" [--agent claude] [--agents claude:2,codex:1]
                [--base <ref>] [--branch <name>] [--isolation worktree|jj|none]
                [--repo <path>] [--machine <m>] [--prompt <text>|--prompt-file <f>]
                [--no-setup] [--no-focus] [--group <g>]
vibeke task list [--all] [--json]
vibeke task show <k7>
vibeke task open <k7>                 # focus its workspace (creates panes if parked)
vibeke task park <k7>                 # stop agents (offer resume), keep worktree
vibeke task finish <k7> [--archive]   # mark finished; optional archive (see §8)
vibeke task rm <k7> [--force] [--keep-branch]
vibeke task adopt [--pane <id>|--run <a12>] [--title …]   # "split into task" (§10)
vibeke task setup rerun <k7>
vibeke task ports <k7>
```

TUI equivalents: `prefix+shift+g` opens "New task" (title, agent(s), base, isolation, prompt). There are task actions in the workspace context menu and the command palette.

`task new` resolves the repo from `--repo`, falling back to the focused pane's cwd. It fails clearly if there is no VCS and `--isolation` isn't `none`.

## 2. Lifecycle

```
            task new
               │
     ┌─────────▼──────────┐   failure → status=active, setup.status=failed,
     │ 1 resolve repo/base │             workspace opens with setup log pane
     │ 2 allocate slug     │
     │ 3 create checkout   │  (worktree | jj workspace | none)
     │ 4 materialize files │  (§5 env & untracked config)
     │ 5 lease ports       │  (§6)
     │ 6 create workspace  │  (root = checkout, task_id set, group = repo group)
     │ 7 run setup script  │  (§7, in a visible "setup" pane, logs → blob)
     │ 8 start agent(s)    │  (agent.start in root pane / split panes)
     │ 9 send prompt       │  (if provided; via adapter, not keystrokes when possible)
     └─────────┬──────────┘
          active ⇄ parked ──► finished ──► archived
                                  └──────► removed (checkout deleted, branch optional)
```

Each step emits events (`task.created`, `worktree.created`, `task.setup_started/finished/failed`, `task.run_attached`, `task.status_changed`). Steps 1–6 complete in under 1.5 s on a warm repo with no setup script; everything after that is async and visible.

The task is **idempotent and resumable**. If the server restarts mid-creation, the next start finds the `task.created` event without `setup_finished` and shows "setup interrupted — rerun?" rather than leaving a half-made checkout unaccounted for.

## 3. Naming

- **slug**: title lowercased, ASCII-folded (`ø→o`, `æ→ae`, `å→a`), non-alnum → `-`, truncated to 40 chars, plus `-<4 base32>` if it collides. Example: `fix-login-redirect`.
- **branch**: `config.tasks.branch_template`, default `{user}/{slug}` (user = `git config user.name` handle-ized, or `vk`). With `--branch`, the given name is used as-is. An existing branch is checked out rather than created, but only if no other worktree holds it.
- **workspace name**: `{repo}:{slug}`, and the sidebar shows it nested under the repo's group.
- **handle**: `k7`, never reused.

## 4. Isolation backends

```rust
trait IsolationBackend {
    fn detect(repo: &Path) -> Option<VcsInfo>;
    async fn create(&self, req: CheckoutRequest) -> Result<Checkout>;        // path, branch, base_ref
    async fn status(&self, co: &Checkout) -> Result<CheckoutStatus>;         // dirty, ahead/behind, conflicts
    async fn remove(&self, co: &Checkout, opts: RemoveOpts) -> Result<()>;   // async, see §9
    fn list(&self, repo: &Path) -> Result<Vec<Checkout>>;                    // reconcile with external changes
}
```

| Backend | Create | Notes |
|---|---|---|
| `worktree` (default for git) | `git worktree add -b <branch> <path> <base>` | `base` defaults to `origin/<default_branch>` after a `git fetch --quiet` (skippable with `tasks.fetch_before_create=false`, 5 s timeout, falls back to local). Sets `extensions.worktreeConfig` only if needed. Submodules: `git submodule update --init --recursive` if `.gitmodules` exists (configurable). |
| `jj` (default when `.jj/` exists) | `jj workspace add --name <slug> -r <base> <path>` | Branch → jj bookmark `<branch>` created on first commit (`jj bookmark create`). Status via `jj log -r @ --no-graph -T …`. Co-located git repos keep working. |
| `none` | Workspace rooted at the repo itself | Shared cwd. The collision tracker (§10) is active. |
| `clone` | private clone inside a container/VM (`git clone --reference`), synced back by host-side fetch | Default code isolation for `container`/`vm` execution — see [13](13-sandboxes-and-vms.md) §6. |

Execution isolation (`host` / `sandbox` / `container` / `vm`) is an orthogonal axis, specified in [13-sandboxes-and-vms.md](13-sandboxes-and-vms.md) and in Phase 1 scope (M3–M4).

**Worktree root**: `tasks.root = "~/.vibeke/worktrees"`, layout `<root>/<repo-name>-<hash6>/<slug>`. `tasks.root = "sibling"` gives the maintainer's current convention, `../<repo>-<slug>`, next to the repo (e.g. `~/code/samplehub-lk20-maths-grade-names`). Either way the path is stored on the Task, never recomputed.

**Reconciliation**: on start and every 60 s per repo with tasks, `list()` is compared with stored tasks. A checkout removed outside Vibeke marks the task `missing` (UI offers recreate or forget). A worktree created outside Vibeke inside the root can be adopted with `vibeke task adopt --path`.

## 5. Materializing untracked files (env, config, deps)

New worktrees lack untracked/ignored files that make a repo runnable. These are configured per repo in `.vibeke/task.toml`, committed; untrusted repos need a one-time trust prompt, like Claude/pi project trust. User-level overrides go in `config.toml [tasks.repos."<remote or path>"]`.

```toml
# .vibeke/task.toml
[files]
copy    = [".env", ".env.local", "apps/web/.env.local", ".vscode/settings.json"]
link    = ["data/fixtures-large"]                 # symlink to the source checkout
clone   = ["node_modules", "apps/*/node_modules"] # copy-on-write clone if possible, else fallback
ignore_missing = true

[deps]
strategy = "auto"        # auto | clone | install | none
install  = "pnpm install --frozen-lockfile --prefer-offline"

[setup]
script  = ".vibeke/setup.sh"   # or inline: run = ["pnpm db:migrate", "pnpm build:types"]
timeout = "10m"

[ports]
count = 10                     # size of the leased range (§6)
env   = { PORT = 0, API_PORT = 1, STORYBOOK_PORT = 2 }   # offsets into the range

[env]
NEXT_TELEMETRY_DISABLED = "1"
DATABASE_URL = "postgres://localhost:5432/app_{slug_underscored}"   # templated
```

**Clone strategy, chosen per filesystem at runtime:**

| Platform/FS | Mechanism | Fallback |
|---|---|---|
| macOS APFS | `clonefile(2)` (what `cp -c` does), recursive | copy |
| Linux btrfs/xfs/bcachefs | `ioctl(FICLONE)` per file (`cp --reflink=always`) | copy |
| Linux ext4 / other | — | `deps.strategy=auto` → run `install` (pnpm/bun with a shared store is fast and dedups); else copy |
| Cross-device | — | install |

`auto` decides as follows:

- Lockfile identical to the source checkout's and CoW available → clone `node_modules` (seconds, ~0 bytes).
- Otherwise → run `install`.
- Known package managers are detected for the default `install` command: pnpm, bun, yarn, npm, uv, poetry, cargo (no-op), go (no-op).

`copy` files containing secrets are copied, never symlinked, so an agent editing `.env` can't change the main checkout. Their contents are never logged or put in events: only file names and hashes.

Templating variables available in `env`, `install`, `setup`: `{slug}`, `{slug_underscored}`, `{branch}`, `{task}`, `{port}`, `{port_base}`, `{port_end}`, `{repo_root}`, `{worktree}`, `{source_root}`.

## 6. Port range allocator

Goal: N tasks of the same repo can all run `pnpm dev` without fighting over 3000/5173.

- Global pool `tasks.ports.pool = "20000-29999"`. Each task leases a contiguous range of `ports.count` (default 10), aligned to 10.
- Leases are stored in SQLite (`port_leases(task_id, start, end, machine_id)`) and are **machine-scoped**: tasks on devbox lease from devbox's server.
- At lease time each port is probed (`bind` on 127.0.0.1 and ::1). An occupied range is skipped. Leases are released on `task rm` and kept while parked.
- Injected env (in every pane of the task workspace, and in setup):
  - `VIBEKE_PORT_BASE`, `VIBEKE_PORT_END`, `VIBEKE_TASK`, `VIBEKE_TASK_SLUG`, `VIBEKE_WORKTREE`
  - `PORT = base + offset` (configurable mapping, as above)
- Framework helpers (documented, not magic): Vite reads `PORT` only when the config uses `process.env.PORT`, and Next uses `PORT` natively. `vibeke task doctor` warns when a known dev server starts on a port outside the task's range. Port discovery (06 §5) sees it either way, so previews still work.
- `vibeke task ports k7` prints the mapping. The sidebar task row shows the live previews.

## 7. Setup scripts

- Run in a dedicated pane titled `setup` in the task workspace, so output is live and interactive if needed. Env is as in §6 plus `VIBEKE_SETUP=1`.
- Order: materialize files → deps (if `install`) → `setup.script`/`run` → `task.setup_finished {duration, exit_code}`.
- Output is archived to a blob (`setup.log_ref`). On failure: the setup pane stays open, the task is marked, and agents are **not** started unless `setup.start_agents_on_failure = true`. A notification fires.
- `--no-setup` skips the whole step. `task setup rerun` reruns it.
- Agent start waits for setup by default. With `setup.parallel_agent = true` the agent starts immediately with a note in its prompt: "Setup is still running in pane X."

## 8. Archive and cleanup

| Policy (config `tasks.cleanup`) | Default | Behavior |
|---|---|---|
| `on_finish` | `keep` | `keep` \| `archive` \| `remove` |
| `archive` means | — | Stop agents, record resume handles, delete the worktree directory, keep the branch, keep the Task + events + transcripts, so `task open` can recreate the worktree from the branch and offer to resume the agent |
| `stale_after` | `14d` | Tasks with no agent activity and no unseen output for this long are listed in `vibeke task gc --dry-run`; never auto-removed unless `auto_gc = true` |
| `merged_branches` | `suggest` | If the task branch is merged into base (`git branch --merged`, or the PR is merged via `gh`), suggest finish+remove |
| `protect_dirty` | `true` | Never remove a checkout with uncommitted changes or unpushed commits without `--force`; show the diff stat in the confirmation |

## 9. Async, non-blocking removal

Removing a worktree of a large repo (node_modules, build dirs) can take ~10 s and freeze the UI. Vibeke:

1. Marks the task `removing`, closes its workspace immediately (UI returns at once), and kills agent processes via their holders (SIGTERM, then SIGKILL after 5 s).
2. Renames the checkout directory to `<root>/.trash/<slug>-<ulid>`. This is atomic and instant on the same filesystem.
3. Runs `git worktree prune` / `jj workspace forget`. These are now fast since the path is gone.
4. A background reaper deletes `.trash` entries at low IO priority (`ionice`/`setiopolicy_np`), one at a time, resumable after a restart.
5. Emits `worktree.removed`. A failure goes to `vibeke doctor` and leaves the entry in trash for manual inspection.

## 10. Shared cwd: collision tracker

Many users run several agents in one directory (`isolation = none`, or simply panes in the same repo). Vibeke makes this visible instead of forbidding it.

**Signals:**
1. **Adapter `file_change` items** (authoritative): Claude `PostToolUse` for Edit/Write/MultiEdit/NotebookEdit; pi/omp extension `tool_execution_end` for edit/write; Codex `file_change`/patch items (app-server mode) or the `apply_patch` hook; OpenCode plugin file events. These carry the path and the run.
2. **Filesystem watcher attribution** (fallback, for screen-only harnesses and Bash-made edits): `notify` (FSEvents/inotify) on each workspace root, ignoring `.git`, `node_modules` and gitignored dirs. On a change, attribution is attempted in this order:
   - (a) a run that reported an in-flight tool call touching that path;
   - (b) on Linux, fanotify/`/proc/*/fd` sampling, best-effort and off by default;
   - (c) the run(s) in `working` state in that cwd at that moment. With more than one candidate, attribution is `ambiguous`.
3. **Git index state**: `git status --porcelain` diffed every 5 s while any agent in the repo is working (cheap, and catches everything).

**Collision rules** (per repo root, rolling window `collision.window = 30m`):
- **Same file** touched by ≥2 different runs → `task.collision_detected {paths, runs, severity: high}`.
- **Same directory/module** (configurable glob depth) → `severity: low`, sidebar hint only.
- **One run modifying a file another run has read** (from Read tool events) in the last 10 min → `severity: medium` ("codex edited `api/auth.ts` that claude is relying on").

**UX:**
- The pane frame shows a ⚠ badge, and the sidebar shows "2 agents editing `src/auth.ts`".
- A notification fires (once per path-set per window).
- The popup lists paths, runs and timeline, with these actions:
  - **Split into task** (§11);
  - **Pause one agent** (sends interrupt via adapter);
  - **Tell the agents**: injects a short steering message via the adapter, e.g. "Note: another agent (codex, pane w5:p3) is also editing src/auth.ts — coordinate or avoid." Only for harnesses that support steer/follow-up natively (pi/omp `steer`, Claude hook `additionalContext` on next `UserPromptSubmit`/`PostToolUse`). Never by typing into a TUI mid-turn.
  - **Ignore for this path**.

**Advisory claims** (groundwork for Phase 2): `vibeke claim add --run a12 "src/auth/**"` and API `task.claim`. The collision tracker raises `severity: high` immediately when another run writes inside a claimed glob. Adapters may expose claims to agents (the Claude hook can deny an Edit inside a foreign claim when `collision.enforce_claims = true`, off by default).

## 11. "Split into task" migration

Takes a run working in a shared cwd and moves it into its own task without losing work:

1. Pick the run (and optionally the paths it owns, defaulting to the files attributed to it in the collision window).
2. Create a task with base = the current `HEAD` of the shared checkout.
3. Move the changes: `git diff -- <paths>` plus untracked files attributed to the run are applied into the new worktree, then reverted in the shared checkout. **Confirm with the diff first**, and keep a safety stash (`git stash push -m vibeke-split-<ulid> -- <paths>`) before reverting.
4. Move the agent:
   - If the harness supports resume: stop the run, then `resume.argv` with cwd = the new worktree. This works for Claude `--resume <id>`, Codex `resume <id>`, pi `--session <id>`, and omp `--resume`.
   - The adapter then injects a note: "Your working directory moved to <path>; continue there."
   - If the harness can't resume: start a fresh run with a generated hand-off prompt (last N turns summarized from the transcript).
5. Emit `task.created` + `task.run_attached` + `agent.resume_handle`. The old pane is closed or left as a shell (user choice).

This is transactional from the user's perspective: on failure at any step after 3, the stash is restored and a notification explains what happened.

## 12. Best-of-N launch (Phase 2 groundwork)

`vibeke task new "make the import 3x faster" --agents claude:2,codex:1 --prompt-file spec.md`

- Creates a **task family**: parent Task `k7` and child tasks `k7.1…k7.3`, each with its own worktree and branch (`{user}/{slug}-1…`) and port range, all from the same base ref.
- The same prompt is sent to all runs, plus a per-run suffix (`config.tasks.best_of_n.suffix`, optional).
- The sidebar groups the family. Each child shows state, diff stat (`+120 −34, 6 files`), test status if declared (`[tasks.check] command = "pnpm test"` run on finish), and previews.
- Phase 1 stops at **side-by-side info + `vibeke task compare k7`** (prints diff stats, check results, and `git diff k7.1..k7.2`). Phase 2 adds the comparison UI, evidence bundles and pick-and-merge.

## 13. Branch status in the sidebar

Per task/workspace, refreshed on fs events (debounced 500 ms) and at most every 10 s:

- branch name, `↑ahead ↓behind` vs upstream or base, `●` dirty count, `✗` conflicts;
- PR: if `gh` is installed and authenticated, `gh pr view --json number,state,isDraft,reviewDecision,statusCheckRollup,url` is cached for 60 s. It shows `#123 ✓` / `#123 ✗ checks` / `draft`. Clicking opens the URL locally (works for remote tasks too: the URL is opened on the client machine). jj: bookmark + `jj git` remote status.
- All status calls run with a 2 s timeout in a bounded worker pool (max 4 concurrent git processes per machine), so a huge repo can never stall the server.

## 14. Runner abstraction

Where a task's processes run. Phase 1 implements `local` and `ssh`, **and** `sandbox`, `container` and `vm` runners as specified in [13-sandboxes-and-vms.md](13-sandboxes-and-vms.md) (M3–M4). Cloud runners are Phase 2.

```rust
#[async_trait]
trait Runner {
    fn kind(&self) -> RunnerKind;                      // Local | Ssh{machine} | Container | MicroVm
    async fn prepare(&self, task: &TaskSpec) -> Result<RunnerHandle>;   // checkout, files, ports
    async fn spawn_pane(&self, h: &RunnerHandle, spec: PaneSpec) -> Result<HolderAddr>;
    async fn forward_port(&self, h: &RunnerHandle, port: u16) -> Result<ForwardHandle>;  // used by preview fabric
    async fn snapshot(&self, h: &RunnerHandle) -> Result<Option<SnapshotId>>;  // None for local/ssh
    async fn fork(&self, snap: SnapshotId) -> Result<RunnerHandle>;            // Unsupported for local/ssh
    async fn teardown(&self, h: &RunnerHandle, opts: TeardownOpts) -> Result<()>;
}
```

Container and microVM notes (detailed in 13):
- Docker Sandboxes / Apple `container` / Firecracker-based providers implement `Runner`.
- The holder runs *inside* the sandbox, reached via a vsock or exec stream.
- `snapshot`/`fork` enable Phase 2 "branch this agent at turn N".
- Policy kits (network allowlist, secrets) attach to the runner.

## 15. Config summary

```toml
[tasks]
root = "~/.vibeke/worktrees"          # or "sibling"
default_isolation = "auto"            # auto = jj if .jj else worktree; or none
branch_template = "{user}/{slug}"
fetch_before_create = true
default_agent = "claude"

[tasks.ports]
pool = "20000-29999"
count = 10

[tasks.cleanup]
on_finish = "keep"
stale_after = "14d"
auto_gc = false
protect_dirty = true

[collision]
enabled = true
window = "30m"
fs_attribution = "auto"               # auto | off | aggressive (fanotify)
enforce_claims = false
```

## 16. Acceptance criteria

- **M2 (collision tracker, shared cwd):**
  - Two Claude runs plus one Codex run in one repo.
  - Edits to the same file by two runs produce a `task.collision_detected` event within 2 s of the second write, a badge, and a notification. Attribution for Claude/pi/omp is via adapter events, verified in e2e tests with recorded hook traces.
- **M3:**
  - `vibeke task new "x" --agent claude` on a 50k-file pnpm repo on APFS:
    - the workspace opens in < 1.5 s;
    - `node_modules` is cloned via clonefile;
    - setup runs in its own pane;
    - the agent starts after setup with `PORT` from its lease;
    - `vibeke task ports` matches the injected env.
  - Three tasks of the same repo run `pnpm dev` concurrently without port conflicts.
  - jj repo: `task new` creates a jj workspace, and the sidebar shows the bookmark and status.
  - `task rm` on a 3 GB worktree returns control in < 200 ms, and the trash is reaped in the background. A server kill during reaping resumes on restart.
  - "Split into task" moves a Claude run with 3 modified files into a new worktree:
    - the files are present there and reverted in the source;
    - the agent resumes its session in the new cwd;
    - a safety stash exists.
  - `--agents claude:2,codex:1` creates 3 child tasks with distinct branches, ports and worktrees, and `task compare` prints diff stats.
  - Removing a checkout with uncommitted changes is refused without `--force` and shows the diff stat.
  - All operations work identically with `--machine devbox`, where the task lives on the remote and the sidebar shows the machine badge.
