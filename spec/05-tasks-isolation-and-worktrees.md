# 05 — Tasks, isolation and worktrees

A **task workspace** is the default way to give an agent somewhere to work. One command produces an isolated checkout, a branch, a collision-free port range, an env, a finished setup script and a running agent. Running several agents in one shared cwd stays possible, because people do it (the maintainer's samplehub workspace has 2 Claude + 1 Codex in one directory). In that case Vibeke watches for collisions and warns (advisory only); moving a running agent out of a shared cwd is a Phase 2 feature (§11).

**Scope of the isolation a task gives you.** A worktree is **checkout isolation**: each agent edits its own files and branch. It is **not execution isolation** — every task still runs as you, on the same machine, sharing databases, caches, credentials, `~`, Docker, and anything else reachable from your user. Linked/cloned directories (§5) and shared services (a local Postgres) are shared too. For execution isolation (OS sandbox, container, VM — and safe "yolo") see [13](13-sandboxes-and-vms.md); a task combines one checkout mode with one execution level.

Implemented in crate `vk-tasks`. Data model: `Task` in [02](02-data-model-and-event-log.md) §1.1. Milestones (see [11](11-milestones.md)): git worktree tasks, env/setup, port leases, async removal and advisory collision warnings **M1**; jj workspaces were dropped for v1 (2026-10-06); tasks on remote machines **M3**; best-of-N **post-1.0** (launch may land in M2 together with containers); split-into-task: Phase 2.

Proposed next slice: [15](15-task-outcomes-review-and-attention.md) adds **Track this work** for an already-running CLI, without relocating or restarting it. Its `attached` task records have separate park/archive/remove semantics (§4.3 there); they must not inherit the owned-workspace cleanup below. This does not change Goal 01's current implementation scope.

## 1. User-facing commands

```
vibeke task new "fix login redirect" [--agent claude] [--agents claude:2,codex:1]
                [--base <ref>] [--branch <name>] [--checkout worktree|clone|none]
                [--isolate host|sandbox|container|vm] [--yolo]          # execution level: 13
                [--repo <path>] [--machine <m>] [--prompt <text>|--prompt-file <f>]
                [--no-setup] [--no-focus] [--group <g>]
vibeke task list [--all] [--json]
vibeke task show <k7>
vibeke task open <k7>                 # focus its workspace (creates panes if parked)
vibeke task park <k7>                 # stop agents (offer resume), keep worktree
vibeke task finish <k7> [--archive]   # mark finished; optional archive (see §8)
vibeke task rm <k7> [--force] [--keep-branch]
vibeke task adopt [--path <worktree>|--pane <id>] [--title …]   # record an existing worktree/pane as a task; moves nothing (§4)
vibeke task setup rerun <k7>
vibeke task ports <k7>
```

TUI equivalents: `prefix+shift+g` opens "New task" (title, agent(s), base, checkout mode, execution level, prompt). There are task actions in the workspace context menu and the command palette.

`task new` resolves the repo from `--repo`, falling back to the focused pane's cwd. It fails clearly if there is no VCS and `--checkout` isn't `none`.

## 2. Lifecycle

```
            task new
               │
     ┌─────────▼──────────┐   failure → status=active, setup.status=failed,
     │ 1 resolve repo/base │             workspace opens with setup log pane
     │ 2 allocate slug     │
     │ 3 create checkout   │  (worktree | none)
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
- **branch**: `tasks.branch_template`, default `{user}/{slug}` (user = `git config user.name` handle-ized, or `vk`). With `--branch`, the given name is used as-is. An existing branch is checked out rather than created, but only if no other worktree holds it.
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
| ~~`jj`~~ | removed | jj support was removed for v1 (2026-10-06); git worktrees only. `--isolation jj_workspace`, `tasks.checkout = "jj"` and `tasks.vcs = "jj"` are refused. |
| `none` | Workspace rooted at the repo itself | Shared cwd. The collision tracker (§10) is active. **Implemented (M4)** as `task new --isolation none`; `task finish --remove-worktree` never deletes anything for it. |
| `clone` | private repo inside a container/VM whose objects borrow the host's read-only (alternates), synced back by host-side fetch | Default code isolation for `container` execution (`--code clone`, `[isolation.container] code`) — see [13](13-sandboxes-and-vms.md) §6. **Implemented for containers (2026-10-06):** the task still gets its host worktree, which is the review surface and is never mounted. The box gets `/workspace` with the task branch at the worktree's HEAD. `vibeke task sync <task> [--direction pull\|push\|both] [--force]` fetches the box branch from the host side with in-box git services into `refs/vibeke/box/…`, then fast-forwards the task branch (in its worktree if it is checked out and clean). Push writes host commits to the box's `refs/vibeke/host/<branch>`, and the box fast-forwards itself. Events: `task.synced`. `task finish` syncs first and keeps (stops) a box whose work isn't on the host. Code: `vk_tasks::sync`. `vm` is M4. |

Execution isolation (`host` / `sandbox` / `container` / `vm`) is an orthogonal axis, specified in [13-sandboxes-and-vms.md](13-sandboxes-and-vms.md) and in Phase 1 scope (M2–M4). **Status (2026-10-06):** `vibeke task new --isolate sandbox|container [--yolo] [--network p]` records the execution level as `Task.isolation`, and every pane in the task's workspace inherits it. `sandbox` uses `worktree` code isolation, and a sandboxed worktree gets the git write rules of 13 §6. `container` defaults to `clone` (above) and can bind the worktree instead (`--code worktree`). The box image comes from `--image`, `.vibeke/sandbox.toml`, the repo's devcontainer, or `[isolation.container] image`. A container task's setup script and devcontainer lifecycle commands run inside the box after repo trust. See 13 §15.

**Worktree root**: `tasks.root = "~/.vibeke/worktrees"`, layout `<root>/<repo-name>-<hash6>/<slug>`. `tasks.root = "sibling"` gives the maintainer's current convention, `../<repo>-<slug>`, next to the repo (e.g. `~/code/samplehub-lk20-maths-grade-names`). Either way the path is stored on the Task, never recomputed.

**Reconciliation**: on start and every 60 s per repo with tasks, `list()` is compared with stored tasks. A checkout removed outside Vibeke marks the task `missing` (UI offers recreate or forget). A worktree created outside Vibeke inside the root can be adopted with `vibeke task adopt --path`.

**Implemented (2026-10-06):** `vk_tasks::reconcile(repo, root, tracked)` compares the owned worktree tasks of a repo with `git worktree list` and the task root (read-only), and `vk-server/src/task_workspace.rs` runs it on start and every 60 s while tasks exist (`task.reconcile {repo?}` runs it now; `vibeke task reconcile`). Findings: a task whose directory is gone, or is no longer a registered worktree, gets `status = missing` and a `task.missing {path, reason: directory_gone|not_a_registered_worktree|prunable}` event (back to `active` with `task.recovered` if the checkout reappears); a task whose worktree switched branches gets `task.branch_changed {expected, actual}` once; worktrees of the repo inside the task root that no task owns, and directories there that are not worktrees (leftovers of a failed creation or removal; `.trash` is the reaper's and skipped), get `worktree.orphan_found {path, kind: worktree|directory, branch, hint}` once per path. **Nothing is ever deleted, pruned or removed automatically:** no directory, branch or worktree. `task adopt` (`task.adopt {path|pane}`), `task recreate` and `task forget` for missing tasks are built (07 §2.10 as-built, v1 remainder); the TUI calls the same methods.

## 5. Materializing untracked files (env, config, deps)

New worktrees lack untracked/ignored files that make a repo runnable. These are configured per repo in `.vibeke/task.toml`, committed; untrusted repos need a one-time trust prompt, like Claude/pi project trust. User-level overrides go in `config.toml [tasks.repos."<remote or path>"]`.

**Implemented (2026-10-06)** in `vk_tasks::{taskfile, files, clone, deps}` and `vk-server/src/task_workspace.rs`. `.vibeke/task.toml` is read from the **new worktree** (the tree the trust digest covers; an uncommitted file in the source checkout is not read). Parsing is lenient: unknown tables such as `[previews]` (read by the preview fabric, 06 B2) are ignored, and problems (a bad `setup.timeout`, an env name that is not a shell identifier, a `[ports] env` offset outside `count`) come back as `warnings` in the `task.create` result. All paths are repo-relative; absolute paths and `..` are rejected. Nothing is overwritten, ever: an existing destination is reported as `exists`.

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

**As built:**

- **Files.** `copy` entries are real files with their mode (the destination is created `create_new`; a symlink is never made for a secret), reported with a blake3 hash; `link` makes a symlink to the source path; `clone` is a copy-on-write clone of a file or a whole tree (`clonefile(2)` with `CLONE_NOFOLLOW` on macOS, `ioctl(FICLONE)` per file on Linux, a plain copy elsewhere or on a filesystem without reflinks; the result says `copy_on_write`, `mixed` or `copy`). Entries may be globs (`*`, `?` per path segment, never descending into `.git`, hidden entries only when the segment starts with `.`), expanded against the source; a wildcard that matches nothing yields nothing. A missing literal source is `missing`, or a `failed` entry with `ignore_missing = false` (default `true`). `tasks.copy_files` from config and `files.copy` are merged (the `task.create {copy_files}` param replaces both). The `task.files_materialized {files: [{path, outcome: copied|linked|cloned|missing|exists|rejected|failed, method?, hash?}], deps}` event and the `files` array of the `task.create` result carry names, outcomes and hashes only, never contents.
- **Deps.** The strategy applies only when the repo file or the user's override has a `[deps]` table (a repo without one keeps today's behaviour: no automatic install). `none` does nothing; `install` runs `deps.install` or the detected manager's command (pnpm `install --frozen-lockfile --prefer-offline`, bun `install --frozen-lockfile`, yarn `install --frozen-lockfile`, npm `ci --prefer-offline`, uv `sync`, poetry `install`; cargo and go need none); `clone` clones every `node_modules` of the source (the root one and `*/*/node_modules`, never nested ones) and falls back to install when the source has none; `auto` clones only when the lockfile in the worktree is byte-identical to the source's **and** the filesystem can clone (probed by cloning the lockfile into the worktree and removing the copy), otherwise installs. An install is a setup step (below), so it is subject to trust. Cross-device or ext4 therefore installs, as the table says. The plan and the reason are in the `task.create` result (`deps`).
- **Env.** `[env]` values are templated and exported to setup and **every pane of the task**, along with `VIBEKE_TASK_SLUG` and the `[ports] env` names (`PORT` etc., offsets into the lease; names that are not shell identifiers or offsets outside the lease are dropped). **`[ports] env` names are allowlisted** (as built after the batch-1 review): an untrusted repo may set only `PORT` and `*_PORT`; a trusted repo any upper-case name containing `PORT` (`^[A-Z][A-Z0-9_]*PORT[A-Z0-9_]*$`); names the user declares in their own `[tasks.repos]` override pass as written. Shell, loader and interpreter variables are never settable this way, trusted or not (`ZDOTDIR`, `BASH_ENV`, `ENV`, `PATH`, `PROMPT_COMMAND`, `PS1/PS2/PS4`, `IFS`, `HOME`, `SHELL`, `PYTHONSTARTUP`, `NODE_OPTIONS`, ... and the prefixes `LD_`, `DYLD_`, `BASH_FUNC_`, `GIT_`, `VIBEKE_`, `ZSH_`): the value is a port number, which a repo can name a directory after (`ZDOTDIR=20000` would run `20000/.zshenv` in the task's first pane before trust). Other names are dropped and listed in `warnings`. The env is saved with the task (state kv `task_env`) so panes created after a server restart still get it. Variables unknown to the template (and a `{` after a `$`) are left as written, so shell `${VAR}` survives. **In commands (`install`, `run`) values are never pasted into the shell text**: `{branch}` becomes a reference to an exported variable, written for the quoting context it sits in (`"${VIBEKE_BRANCH}"` unquoted, `${VIBEKE_BRANCH}` inside `"…"`, `'"${VIBEKE_BRANCH}"'` inside `'…'`; `$(…)` and backticks open a fresh context). The variables are `VIBEKE_TASK_SLUG`, `VIBEKE_TASK_SLUG_UNDERSCORED`, `VIBEKE_BRANCH`, `VIBEKE_TASK_ID`, `VIBEKE_PORT_BASE` (`{port}`), `VIBEKE_PORT_END`, `VIBEKE_REPO_ROOT`, `VIBEKE_WORKTREE` and `VIBEKE_SOURCE_ROOT`; they are exported to the setup commands after `[env]`, so `[env]` cannot redefine them. A shell expands a variable once and never re-parses the value, so a branch like `feature/$(cmd)` prints literally in every quoting context (the old single-quoting broke inside `"{branch}"`). The commands shown in `task.setup_untrusted`/`task.setup_started` carry the references.
- **Ports.** `[ports] count` (or `task.create {ports}`) sizes the lease: a block of that many ports aligned to its own size, from `tasks.port_pool`; `tasks.port_block` is the default size. A task that cannot get a block is still created, with a `no ports leased` warning (the old code swallowed this). `VIBEKE_TASK_SLUG` and `PORT` (offset 0 unless mapped) are injected.
- **Repo overrides.** `config.toml [tasks.repos."<key>"]` takes the same tables (`files`, `deps`, `setup`, `ports`, `env`). The key is the repo path (`~/` expanded, canonical comparison) or the `origin` URL (compared ignoring scheme, user, port, trailing `.git`, case and `host:path` vs `host/path`). URLs are **parsed** (`vk_tasks::parse_remote`): `scheme://[userinfo@]host[:port]/path`, scp-like `[user@]host:path`, or `host/path`; userinfo is only what precedes the host, and the host must be a plain hostname, so `https://evil.example/x@github.com/org/repo` is host `evil.example` and never picks up the `github.com/org/repo` override (whose commands run without trust). Lists and scalars set there replace the repo file's, `env` and `ports.env` merge per key with the user's value winning; unknown keys warn, and `ports.count` / `setup.timeout` are validated. The shipped default config carries a commented example.
- **Trust (09 §4).** What runs repo code is gated on `vibeke policy trust <repo>` for the exact `.vibeke/` tree (which includes `task.toml`): `setup.script`, `setup.run`, `deps.install` (including the auto-detected install, because package lifecycle scripts are repo code), and the repo's own `[env]`. Commands the user wrote in `[tasks.repos]` are theirs and run without the prompt; a script file is always repo content. Cloning, linking, copying and ports need no trust (no code runs). Until trusted, `task.setup_untrusted {repo, digest, script, commands: [{source, command}], hint}` lists every command that would run (so they are shown before anything runs), `setup_status = untrusted`, nothing runs, and the repo's `[env]` is withheld (the user's own `env` still applies). `policy.trust` prints the same list as `task_file` (commands, env, warnings). After trusting, `vibeke task setup <task>` (`task.setup`) runs the setup; a changed tree is untrusted again.

## 6. Port range allocator

Goal: N tasks of the same repo can all run `pnpm dev` without fighting over 3000/5173.

**Leases are machine-wide, not per session.** Several Vibeke sessions (`default`, `work`, a test session) on one machine share one lease table:

- `~/.local/state/vibeke/machine/ports.db` (SQLite, WAL) — under state, not `$XDG_RUNTIME_DIR`, because leases of parked tasks must survive reboots.
- Allocation runs inside `BEGIN IMMEDIATE` (a write lock shared by all sessions on the machine), so two sessions can never hand out the same range.
- Schema: `port_leases(start, end, session_uuid, task_id, machine_uuid, created_at, released_at)`.
- Remote tasks lease from the remote machine's table (the remote server allocates).

**Pool and ranges.** `tasks.port_pool = "20000-29999"`; each task leases a contiguous block of `tasks.port_block` ports (default 10), aligned to the block size. The default pool lies **below the OS ephemeral range** (macOS 49152–65535, Linux 32768–60999 by default), so the kernel never hands these ports out to outgoing connections. `vibeke doctor` warns if the machine's ephemeral range overlaps the pool.

**What a lease does and doesn't reserve.** Probing a port (bind, then close) proves nothing: another process can bind it a millisecond later, and holding the socket open would block the dev server itself. So:
- A lease is a **reservation among Vibeke sessions** (enforced by the lock above), not an OS-level reservation.
- Protection against non-Vibeke processes is statistical (pool outside the ephemeral range, rarely used by other software) plus **detection**: at lease time ports are probed and occupied blocks skipped; when a known dev server in the task fails with `EADDRINUSE` or starts outside the block (06 §B2 discovery), `task doctor` reports it and offers `vibeke task ports k7 --re-lease`.
- Leases are released on `task rm`, kept while parked, and garbage-collected when their session or task no longer exists (checked by `machine_uuid` + `session_uuid`).

**Injected env** (every pane of the task workspace, and setup):
- `VIBEKE_PORT_BASE`, `VIBEKE_PORT_END`, `VIBEKE_TASK`, `VIBEKE_TASK_SLUG`, `VIBEKE_WORKTREE`
- `PORT = base + offset` (configurable mapping, as above)

**Doctor (implemented 2026-10-06).** `vibeke doctor` has a `tasks` section: the pool's free and leased blocks, and a warning for each of: the pool overlapping the OS ephemeral port range (read from `/proc/sys/net/ipv4/ip_local_port_range` or `sysctl net.inet.ip.portrange.*`), the pool exhausted (no free block; new tasks then start without ports) or nearly (a tenth left), no aligned block fitting the pool, a lease outside the pool (the pool was changed), and overlapping leases. Warnings never fail the doctor. `vibeke task ports <t> [--re-lease]` is built (`task.ports`, `task.ports.re_lease`: another block of the same size, never the old one); `task doctor` is not built.

Framework helpers (documented, not magic): Vite reads `PORT` only when the config uses `process.env.PORT`; Next uses `PORT` natively. Ports from the lease are pre-declared as previews when `[previews]` names them (06 §B2). `vibeke task ports k7` prints the mapping.

## 7. Setup scripts

- Run in a dedicated pane titled `setup` in the task workspace, so output is live and interactive if needed. Env is as in §6 plus `VIBEKE_SETUP=1`.
- Order: materialize files → deps (if `install`) → `setup.script`/`run` → `task.setup_finished {duration, exit_code}`.
- Output is archived to a blob (`setup.log_ref`). On failure: the setup pane stays open, the task is marked, and agents are **not** started unless `setup.start_agents_on_failure = true`. A notification fires.
- `--no-setup` skips the whole step. `task setup rerun` reruns it.
- Agent start waits for setup by default. With `setup.parallel_agent = true` the agent starts immediately with a note in its prompt: "Setup is still running in pane X."

- **As built (2026-10-06):** the steps run on a background thread (`vk_tasks::run_setup`) in this order: `deps.install` (when the plan installs), each `setup.run` command (`sh -c`, one after the other, the first failure ends setup), then `setup.script` if the file exists; one log file (`<worktree>/.vibeke/setup.log`, with a `$ <command>` line per step) and one overall `setup.timeout` (default 10 min; the process group gets SIGTERM, then SIGKILL after 2 s). The **`setup` pane** is a split below the task's first pane showing the live log (`tail -F`): it is read-only, not an interactive shell running the steps, and it stays open after a failure. Events: `task.setup_started {commands, pane}`, `task.setup_finished {status, exit_code, duration_ms, log, pane}`, and on failure also `task.setup_failed` plus a `high` notification. `setup_status` is `running`, then the final status (`Succeeded`, `Failed { exit_code: Some(n) }`, `TimedOut`, ...). Agents wait for setup: with agents in the request, `task.create` returns at once with `setup.agents_pending = true` and the agents start when setup succeeds. **`setup.start_agents_on_failure = true`** starts them anyway; otherwise a failed setup emits `task.agents_withheld`. **`setup.parallel_agent = true`** starts them immediately and appends "(Setup is still running in pane wN:pM.)" to each agent's prompt (when it has one). `--no-setup` (`setup: false`) skips the step; container tasks run theirs in the box (13 §9). `task setup <task>` reruns it (after trust or a fix), reading the task file again; agents are not touched. **For a container task, `task.setup` goes through the container runner** (`sandbox::container::rerun_setup`): the box's trusted lifecycle commands and `.vibeke/setup.sh` (trust checked again, so a rerun after `policy trust` picks it up) are `exec`ed into the box, starting it if stopped; the result has `in_container: true`, and completion is `sandbox.setup_finished` with the log in the box root. If the task's box is gone or is not a container, `task.setup` is refused (`conflict`); it never falls back to the host.
- **Trust first (09 §4, implemented):** setup runs only when `(canonical repo path, blake3 of the .vibeke/ tree)` is recorded by `vibeke policy trust <repo>` (which prints the script). An untrusted or changed tree marks the task `setup_status = untrusted` and emits `task.setup_untrusted` with the digest and a hint; nothing runs.

## 7a. Attached (tracked) tasks — spec 15 §4.3

`vibeke task track` records an **attached** task for work already running in an existing pane/checkout. It never creates a worktree, branch, port lease or setup run. Park/finish/archive of an attached task change only the record: no agent is stopped, no workspace closed, no files deleted, no ports released (`task.finish` returns a note saying so). The review base for an attached task is the merge-base with the selected target branch (else the default branch, else `HEAD`), and the observation baseline (head + staged/unstaged/untracked digest) is captured at tracking time, labelled "may include preexisting changes".

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
3. Runs `git worktree prune`. These are now fast since the path is gone.
4. A background reaper deletes `.trash` entries at low IO priority (`ionice`/`setiopolicy_np`), one at a time, resumable after a restart.
5. Emits `worktree.removed`. A failure goes to `vibeke doctor` and leaves the entry in trash for manual inspection.

## 10. Shared cwd: collision tracker

Many users run several agents in one directory (`checkout = "none"`, or simply panes in the same repo). Vibeke makes this visible instead of forbidding it.

**Collision detection is advisory.** A `file_change` item is evidence that a tool *attempted or reported* an edit, not proof of who owns a file's current content: Bash commands, formatters, git operations and non-integrated harnesses edit files without reporting, and two runs can make overlapping edits that attribution can't untangle. Vibeke warns; it never blocks, reverts or reassigns changes on the basis of attribution.

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
  - **Start a fresh task from here**: creates a task from the shared checkout's `HEAD` and starts a *new* run there with a hand-off prompt; the original run keeps working where it is (nothing is moved);
  - **Pause one agent** (sends interrupt via adapter);
  - **Tell the agents**: injects a short steering message via the adapter, e.g. "Note: another agent (codex, pane w5:p3) is also editing src/auth.ts — coordinate or avoid." Only for harnesses that support steer/follow-up natively (pi/omp `steer`, Claude hook `additionalContext` on next `UserPromptSubmit`/`PostToolUse`). Never by typing into a TUI mid-turn.
  - **Ignore for this path**.

**Advisory claims** (groundwork for Phase 2): `vibeke claim add --run a12 "src/auth/**"` and API `task.claim`. The collision tracker raises `severity: high` immediately when another run writes inside a claimed glob. With `collision.enforce_claims = true` (off by default), cooperating adapters deny *reported* edit tools inside a foreign claim (Claude `PreToolUse` Edit/Write, pi/omp `tool_call`); this is a courtesy guardrail, not enforcement — shell commands and non-integrated harnesses are unaffected.

**As built (3A, 2026-10-07; pure rules in `vk_tasks::collision`, state, signals and API in `vk-server::collision`, the typed `[collision]` section in `vk_config::Collision`, UI in `vk-tui::collision`, API in 07 §2.10a):**
- **Roots.** A collision is per *checkout*: the nearest ancestor of a run's cwd holding `.git` (a task worktree is its own root, so isolated tasks never collide). A run's cwd is its reported cwd, else its pane's, else its task's.
- **Signals.** (1) Adapter reports end in the hook vocabulary, so one extractor serves every harness: `PostToolUse` of Edit/Write/MultiEdit/NotebookEdit (Claude), pi/omp `edit`/`write` (extension `ToolEnded`), Codex `apply_patch` (every `*** Add|Update|Delete File:` / `Move to:` path), ACP `edit`, OpenCode `tool.execute.after` and `file.edited`; `Read` events feed the read-then-edited rule; a `PreToolUse` of an edit tool marks that path *in flight* for the run until its `PostToolUse` plus 3 s. (2) A `notify` watcher on each *shared* checkout, events settled `[collision] settle` (150 ms), batched through `git check-ignore`, ignoring `.git`, `node_modules`, gitignored paths and `[collision] ignore`. Attribution in order: (a) the run with an in-flight reported tool call on that path; (b) with `fs_attribution = "aggressive"`, the run whose process tree held the file open for writing (Linux `/proc/*/fd` + `fdinfo` flags, best effort — there is no fanotify path: it needs privileges, and the writer has usually closed the file by the time the event arrives); (c) the runs `working` in that checkout, **`ambiguous`** when more than one. A change no run was working on is not an agent's collision. A change an adapter reported in the last 3 s explains its own event and adds nothing (two different writers of one file inside that window are not told apart — a documented limit). (3) `git status --porcelain=v1 -z` of each shared checkout every `[collision] poll_interval` (5 s) while a run in it is working: the first snapshot is the baseline, a path is a write when its entry is new or its (status, size, mtime) changed, attributed like watcher events. The watcher and the poll run **only** for a checkout with two or more live runs or a claim of a live run (a lone agent cannot collide; an idle machine pays nothing). `fs_attribution = "off"` disables both; adapter reports stay.
- **Rules** (`window` 30 min, `read_window` 10 min; touches older than the window are forgotten): the same file written by two runs → `high`; a write inside another run's claim → `high` at once (even a single write); a run editing a file another run read → `medium`; two runs writing different files below the same `dir_depth` (2) directory components → `low`. **Ambiguous** touches never collide with each other (two writers cannot be proven); an ambiguous touch collides with a certain one when it cannot be the same run, and the record says `ambiguous` ("possibly editing"). Findings merge into one record per checkout and run set (a record whose run set contains, or is contained in, the finding's accepts it), keeping the strongest hit per path; severity only rises while open.
- **Records and events.** `collision` entities (open while live, closed as history): `task.collision_detected` when a record is created, gains a path or a run, or its severity rises; `task.collision_cleared {reason}` when the window passes quiet, fewer than two of its runs are alive, every path was ignored, or the server restarted (records do not survive a restart: the touch window is memory). A notification (`kind: collision`, `high` normal urgency, `medium` low) fires for `high` and `medium` once per path set per window (`[collision] notify`); `low` is a sidebar hint only. Collision records are `sync`-tier events (not `history`).
- **UX.** Pane-frame `⚠` (top-left; `high`/`medium` only), agent-row `⚠`/`~`, a sidebar line "2 agents editing src/auth.ts" per workspace, and the collision view (`collisions` palette action): agents with how each can be told, paths, timeline, and **p**ause (the adapter's interrupt), **t**ell, **f**resh task (shows the plan and hand-off prompt and asks before creating), **i**gnore path, **o**pen the pane.
- **Tell** reaches a run only through a native channel: a headless run's steer (`agent.prompt {mode: steer}`: pi/omp `steer`, Codex `turn/steer`, Claude stream-json queue) or Claude's hook `additionalContext` on the next `UserPromptSubmit`/`PostToolUse` (queued up to 10 minutes). A pi/omp, Codex, Gemini, OpenCode or other TUI run has no channel and is reported `unsupported`; nothing is ever typed into a TUI mid-turn.
- **Fresh task** is `task.create` from the shared checkout's `HEAD` (no fetch) with the source run's harness (or a named one) and a hand-off prompt (the colliding paths, the checkout's branch and `HEAD`, the source run's last message redacted and cut at 600 characters); the shared checkout and its runs are untouched.
- **Claims.** Two kinds, one tracker. **Run claims** (`collision.claim|claims|claim_release`; `vibeke collision claim|claims|unclaim`; `vibeke claim add --run r` and `task.claim {run}` of a run that belongs to no task reach the same call): a repo-relative glob (`*` stays in a segment, `**` crosses, a directory name covers its contents), idempotent per run, root and glob, overlaps reported as `conflicts`, released when the run ends. **Task claims** (`task.claim`, merge orchestration, 12 / Batch 4, stored as `orch_claim`) are read by the tracker too: a task's claim binds, in each checkout where a run of the task works, every run that is not of that task. With `enforce_claims = true` the `vibeke hook` shim prints the server's `permissionDecision: deny` for a Claude `PreToolUse` of an edit tool inside another live run's claim.
- **Still open / needs the user.** Claim enforcement for pi/omp `tool_call` (the server answers an `adapter.signal` reply; the extension in `integrations/pi-extension` does not await replies, so it needs a release that does, and the committed `dist/vibeke.js` is rebuilt with it), Codex (its `PreToolUse` only covers Bash) and OpenCode; steering of TUI runs of pi/omp/Codex/OpenCode (no native channel); a live check of the Claude `additionalContext` and `deny` hook outputs against the installed Claude version; real-watcher timing (`settle`, FSEvents latency, large repos); fanotify attribution; the decision whether the watcher and the `git status` poll should stay on by default for every shared checkout (they are on: `[collision] enabled = true`).

## 11. "Split into task" migration — deferred to Phase 2

Moving a *running* agent and its uncommitted changes out of a shared checkout is not in Phase 1. Copying or reverting files while other writers are active is not a transaction, `git diff` omits staged changes by default, `git stash` without `-u` omits untracked files, and file-level attribution can't separate overlapping edits. Phase 1 offers "start a fresh task from here" (§10) instead and makes isolated tasks the default.

Outline for the Phase 2 design (requirements, not a spec):
1. **Quiesce all writers** in the shared checkout: interrupt every run via its adapter (not only the one being moved), wait for `idle`, and refuse if any pane in that cwd has a non-agent foreground process writing files (watcher quiet for N seconds).
2. **Capture full state** atomically: staged (`git diff --cached --binary`), unstaged (`git diff --binary`), untracked (`git ls-files --others --exclude-standard -z` + contents), plus a `git stash create`-style snapshot commit as a recovery point that is not applied to the working tree.
3. **Select** the changes to move with an explicit user-reviewed patch (attribution only pre-selects).
4. **Validate** applicability in the new worktree (`git apply --check --index`) before touching the source.
5. **Apply, then verify, then revert the source** — keep the recovery point until the destination is verified and the agent has resumed there.
6. Resume the agent with its resume handle in the new cwd (or hand-off prompt), then release the other writers.

**As built (Batch 4, lane 4; `[orchestrate.split] enabled`, off by default).** `task.split {run|pane|workspace, paths?, title?, dry_run?, resume?, keep_recovery?}` (`vibeke task split`) follows the outline: it interrupts **every** run in the checkout and waits for idle, refuses while a non-agent foreground process runs there or any changed file is younger than `quiet_for` (3 s), then captures staged, unstaged and untracked state, builds a recovery commit under `refs/vibeke/split/<id>` with a scratch index (never applied to the tree) and a per-path state digest, refuses if the checkout changed since the capture, validates with `git apply --check --index` in the new worktree (created from the source's exact `HEAD`, without setup or file copies, so it is clean), applies staged then unstaged then copies untracked files, verifies the destination's digest equals the selection's, and only then reverts the source for the moved paths. A failure before the revert rolls the destination back and leaves the source untouched. The moved agent is handed off in the new task (a prompt naming the carried changes and its last message); `resume = true` tries the harness resume handle in the new cwd first (unverified: harnesses key sessions by cwd). The recovery ref is dropped once the hand-off succeeded unless `keep_recovery`. `dry_run` lists the changes, the steps and what blocks. Code: `vk-orchestrate::split`, `vk-server/src/orch_split.rs`. Not built: attribution-based pre-selection of paths (the selection is explicit or everything).

## 12. Best-of-N launch (Phase 2 groundwork)

`vibeke task new "make the import 3x faster" --agents claude:2,codex:1 --prompt-file spec.md`

- Creates a **task family**: parent Task `k7` and child tasks `k7.1…k7.3`, each with its own worktree and branch (`{user}/{slug}-1…`) and port range, all from the same base ref.
- The same prompt is sent to all runs, plus a per-run suffix (`tasks.best_of_n.suffix`, optional; 08 §11).
- The sidebar groups the family. Each child shows state, diff stat (`+120 −34, 6 files`), test status if declared (`[tasks.check] command = "pnpm test"` run on finish), and previews.
- **As built (Batch 4, lane 4; `[orchestrate.best_of_n] enabled`, off by default).** `vibeke task new "..." --agents claude:2,codex:1 [--prompt-file f]` (that is `task.create` with a string `agents`, also `task.best_of_n`) creates a family record `k7` (no phantom parent task) and child tasks renamed `k7.1` to `k7.3`, each with its own worktree, branch (`{slug}-N`) and ports, the same base and prompt plus the per-run suffix (`tasks.best_of_n.suffix`, with `{n}`, `{total}`, `{harness}`, `{ordinal}`); creation is all or nothing. `task.compare k7 [--pair a --pair b]` gives diff stats, commits, dirty state, stored check outcome (flagged stale when the head or dirty state changed) and a deterministic ranking (a passing check dominates, then a smaller diff; a sort aid, not an acceptance decision). The check command comes from the request, `check_command` in config, or a trusted repo `.vibeke/task.toml` `[check] command`; `family.check` runs it in each child, and a background pass runs it when a child goes idle at a revision without a fresh check (`check = true`). `task.pick k7 k7.2 [--merge] [--discard]` records the winner once, optionally queues its branch in the merge queue and archives the losers (`protect_dirty` unless `--force`). Sidebar grouping is not built (the handles `k7.N` carry the family). Code: `vk-orchestrate::family`, `orch_family.rs`.
- Phase 1 stops at **side-by-side info + `vibeke task compare k7`** (prints diff stats, check results, and `git diff k7.1..k7.2`). Phase 2 adds the comparison UI, evidence bundles and pick-and-merge. Best-of-N launch is **post-1.0**, possibly earlier in M2 alongside containers (see 11).

## 13. Branch status in the sidebar

Per task/workspace, refreshed on fs events (debounced 500 ms) and at most every 10 s:

- branch name, `↑ahead ↓behind` vs upstream or base, `●` dirty count, `✗` conflicts;
- PR: if `gh` is installed and authenticated, `gh pr view --json number,state,isDraft,reviewDecision,statusCheckRollup,url` is cached for 60 s. It shows `#123 ✓` / `#123 ✗ checks` / `draft`. Clicking opens the URL locally (works for remote tasks too: the URL is opened on the client machine).
- **PR status as built (2026-10-06):** `task.pr {task, refresh?}` (`vibeke task pr <task>`) runs `gh pr view --json number,state,isDraft,reviewDecision,statusCheckRollup,url` in the task's worktree, but only after `gh auth status` succeeds (`gh` missing or logged out gives `{kind: unavailable, reason}` and nothing else runs; "no pull requests found" gives `{kind: no_pr}`). It never prompts (stdin null, `GH_PROMPT_DISABLED=1`, `GIT_TERMINAL_PROMPT=0`), each call has a 5 s timeout, and every answer, including unavailable and no-PR, is cached for 60 s per worktree (`refresh` bypasses it). `task.get` returns the cached `pr` (null when none is cached) and never runs `gh`. The label is `#123 ✓`, `#123 ✗ checks`, `#123 …` (checks pending), `#123 draft`, `#123 merged`/`closed`, or `#123`. Sandboxed checkouts are skipped (a box can rewrite their git config, and `gh` runs git). Tests use a fake `gh` through `VIBEKE_GH_BIN=<absolute path>`, honoured only under `VIBEKE_TEST_HOOKS=1`. The sidebar display and click-to-open are not built. The "merged via gh" suggestion is wired: the first time a task's cached PR is `MERGED`, `task.cleanup_suggested {reason: pr_merged, hint}` and a low notification (never automatic cleanup). `task.archive` and `task.setup_log` are API methods (07 §2.10).
- All status calls run with a 2 s timeout in a bounded worker pool (max 4 concurrent git processes per machine), so a huge repo can never stall the server.

## 14. Runner abstraction

Where a task's processes run. Phase 1 implements `local` and `ssh`, **and** `sandbox`, `container` and `vm` runners as specified in [13-sandboxes-and-vms.md](13-sandboxes-and-vms.md) (M2 sandbox/container, M4 VM). Cloud runners are Phase 2.

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

**Implemented (2026-10-06):** `vk_sandbox::runner::Runner`, with `level()`, `provider()`, `check()` and a synchronous `prepare(SpawnRequest) -> PreparedSpawn` (argv wrapper, scrubbed env, cwd, mounts, generated profile, broker socket, visible roots). It differs from the sketch above: holders keep owning the PTY on the host and the runner only wraps the holder's child, so there is no `spawn_pane` returning a holder address yet. Checkout preparation stays in `task.create`. `forward_port`, `snapshot`, `fork` and `teardown` are not part of the trait yet; contexts are torn down by the server's sandbox module. Implementations: `HostRunner`, `SandboxRunner` (Seatbelt on macOS; bubblewrap/Landlock/seccomp chain on Linux, unverified), `ContainerRunner` (per-task box: `prepare` returns `<runtime> exec -it <box> …`; the server's `sandbox_container.rs` creates, starts, stops and removes the box and runs the clone/lifecycle steps) and `VmRunner` (placeholder). The SSH runner remains the bridge stack of 06. Details and caveats: 13 §15.

Container and microVM notes (detailed in 13):
- Docker Sandboxes / Apple `container` / Firecracker-based providers implement `Runner`.
- The holder runs *inside* the sandbox, reached via a vsock or exec stream. (2026-10-06: not yet for containers. The holder stays on the host and owns the `exec -it` PTY; see the deviations in 13 §15.)
- `snapshot`/`fork` enable Phase 2 "branch this agent at turn N".
- Policy kits (network allowlist, secrets) attach to the runner.

## 15. Config summary

Canonical schema: [08](08-ux-config-and-keybindings.md) §11. Keys used here (`root`, `vcs`, `default_agent`, `port_block`, `setup_script`, `copy_files` are defined there; the rest are introduced by this section):

```toml
[tasks]
root = "~/.vibeke/worktrees"          # or "sibling"
vcs = "auto"                          # auto | git
checkout = "auto"                     # auto (worktree) | worktree | clone | none
branch_template = "{user}/{slug}"
fetch_before_create = true
default_agent = "claude"
port_pool = "20000-29999"
port_block = 10
setup_script = ".vibeke/setup.sh"
copy_files = [".env", ".env.local"]

# Per-repo overrides of .vibeke/task.toml (§5), keyed by origin URL or repo path:
# [tasks.repos."github.com/acme/app"]
# files = { clone = ["node_modules"] }
# deps  = { strategy = "auto" }
# setup = { run = ["pnpm db:migrate"], timeout = "10m" }
# env   = { DATABASE_URL = "postgres://localhost:5432/app_{slug_underscored}" }

[tasks.cleanup]
on_finish = "keep"
stale_after = "14d"
auto_gc = false
protect_dirty = true

[collision]
enabled = true
window = "30m"
fs_attribution = "auto"               # auto | off | aggressive (fanotify)
enforce_claims = false                # courtesy guardrail for cooperating adapters only
```

## 16. Acceptance criteria

- **M1 (collision tracker, shared cwd):**
  - Two Claude runs plus one Codex run in one repo.
  - Edits to the same file by two runs produce a `task.collision_detected` event within 2 s of the second write, a badge, and a notification. Attribution for Claude/pi/omp is via adapter events, verified in e2e tests with recorded hook traces.
- **M1 (tasks):**
  - `vibeke task new "x" --agent claude` on a 50k-file pnpm repo on APFS:
    - the workspace opens in < 1.5 s;
    - `node_modules` is cloned via clonefile;
    - setup runs in its own pane;
    - the agent starts after setup with `PORT` from its lease;
    - `vibeke task ports` matches the injected env.
  - Three tasks of the same repo run `pnpm dev` concurrently without port conflicts.
  - `task rm` on a 3 GB worktree returns control in < 200 ms, and the trash is reaped in the background. A server kill during reaping resumes on restart.
  - "Start a fresh task from here" on a collision creates a task from the shared `HEAD` and starts a new run with a hand-off prompt, without touching the shared checkout or the original run.
  - Two Vibeke sessions on one machine creating tasks concurrently never receive overlapping port blocks (stress test: 50 parallel `task new` across 2 sessions).
  - (post-1.0, or M2 with containers) `--agents claude:2,codex:1` creates 3 child tasks with distinct branches, ports and worktrees, and `task compare` prints diff stats.
  - Removing a checkout with uncommitted changes is refused without `--force` and shows the diff stat.
  - (M3) All operations work identically with `--machine devbox`, where the task lives on the remote and the sidebar shows the machine badge.
