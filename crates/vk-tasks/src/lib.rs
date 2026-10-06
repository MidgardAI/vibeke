//! Task workspaces for Vibeke (spec 05): git worktrees, untracked-file
//! materialization, setup scripts, machine-wide port leases, non-blocking
//! removal and branch status. Synchronous library; depends on no other `vk-*`
//! crate and shells out to the `git` CLI.
//!
//! # API overview
//!
//! * **Detection** – [`repo_root`]`(cwd) -> Option<`[`RepoInfo`]`>` (main repo
//!   root, the worktree containing `cwd`, default branch, `origin` URL,
//!   current branch); [`detect`] always answers (`Vcs::None` for non-repos).
//! * **Naming** – [`slugify`], [`unique_slug`], [`render_branch`],
//!   [`user_handle`].
//! * **Worktrees** – [`create_worktree`]`(&`[`CreateRequest`]`, &`[`WorktreeConfig`]`)
//!   -> `[`Checkout`], [`list_worktrees`] (porcelain parse), [`open_worktree`],
//!   [`find_worktree`], [`restore_worktree`]. Roots: [`WorktreeRoot::Dir`]
//!   (`<root>/<repo>/<slug>`) or [`WorktreeRoot::Sibling`]
//!   (`<repo parent>/<repo>-<slug>`).
//! * **Task file** – [`TaskFile`] (`.vibeke/task.toml`: files, deps, setup,
//!   ports, env), user overrides via [`TaskFile::apply`], `{variable}`
//!   templating via [`TemplateVars`], [`repo_key_matches`] for
//!   `[tasks.repos."<remote or path>"]`.
//! * **Files** – [`materialize_files`] copies, symlinks (`link`) and
//!   copy-on-write clones (`clone`: `clonefile(2)`, `FICLONE`, else copy)
//!   with glob entries, never overwriting; [`copy_files`] is the `copy` part.
//! * **Deps** – [`plan_deps`] / [`run_deps_clone`]: auto | clone | install |
//!   none, lockfile comparison, package manager detection.
//! * **Reconcile** – [`reconcile`] compares tasks with `git worktree list`
//!   and the task root (missing, branch moved, orphans); read-only.
//! * **PR status** – [`PrCache`] over `gh pr view` (60 s cache, only when
//!   `gh` is installed and authenticated, never prompts).
//! * **Setup** – [`run_setup`] (blocking) / [`spawn_setup`] (background,
//!   cancellable via [`SetupHandle::cancel`]) with timeout; env from
//!   [`setup_env`]; steps (`commands`, then the script) share one log file.
//! * **Ports** – [`PortLeases`] over a state dir: `flock`ed lock file plus
//!   JSON lease table shared by all processes; leases expire when their
//!   owner pid dies; ports are bind-tested on 127.0.0.1 before being handed
//!   out.
//! * **Task previews** – [`parse_previews`] / [`resolve_previews`]: `[previews]`
//!   entries (`port_env` / `offset` into the lease, 06 B2) resolved to ports.
//! * **Removal** – [`start_remove`]`(path, `[`RemoveOptions`]`) -> `[`RemovalJob`]
//!   (events channel + pollable [`RemovalState`]); [`reap_trash`].
//! * **Status** – [`branch_status`], [`removal_blockers`].
//! * **Finish/archive** – [`is_merged`], [`merged_branches`], [`diff_stat`],
//!   [`archive_worktree`].
//!
//! # Deviations from spec 05
//!
//! * Port leases use a JSON file + `flock` instead of SQLite `ports.db`; the
//!   cross-process exclusion guarantee is the same.
//! * Slug collisions get `-2`, `-3` suffixes (not `-<base32>`); `[tasks]
//!   root` directory layout is `<root>/<repo-name>/<slug>` (no hash).
//! * New task branches are created with `--no-track`, so they never silently
//!   track (and push to) the base branch; status compares against the base.
//! * Removal follows 05 §9 (rename into `.trash`, `git worktree prune`,
//!   delete in background) and falls back to `git worktree remove` when the
//!   rename fails. Reaping does not lower IO priority yet.
//! * Backends: worktree and none. `clone` for container boxes lives in [`sync`]:
//!   the in-box clone script plus host-side fetch/push (`vibeke task sync`).

mod clone;
mod deps;
mod error;
mod files;
mod finish;
mod ghpr;
mod git;
mod naming;
mod ports;
mod previews;
mod reconcile;
mod recreate;
mod remove;
mod repo;
mod setup;
mod status;
pub mod sync;
mod taskfile;
mod worktree;

pub use clone::{CloneMethod, clone_path, cow_available};
pub use deps::{
    DepsAction, DepsPlan, PackageManager, detect_package_manager, plan_deps, run_deps_clone,
};
pub use error::{Error, Result};
pub use files::{
    CopyOutcome, CopyResult, copy_files, default_copy_files, expand_glob, materialize_files,
};
pub use finish::{DiffStat, archive_worktree, diff_stat, is_merged, merged_branches};
pub use ghpr::{
    ChecksState, PR_CACHE_TTL, PR_EVIDENCE_FIELDS, PrCache, PrJson, PrLookup, PrStatus, fetch_pr,
    fetch_pr_evidence_json, gh_binary, gh_ready, parse_pr, valid_pr_ref,
};
pub use git::set_child_umask;
pub use git::{
    HOST_HARDEN, is_contained, register_contained_checkout, safety_args,
    unregister_contained_checkout,
};
pub use naming::{DEFAULT_SLUG_MAX, render_branch, slugify, slugify_raw, unique_slug, user_handle};
pub use ports::{
    Lease, LeaseRequest, PoolHealth, PortLeases, PortPool, ephemeral_range, pool_health,
};
pub use previews::{
    MAX_TASK_PREVIEWS, PreviewSpec, ResolvedPreview, normalize_preview_path, offset_of_env,
    parse_previews, port_env_offsets, previews_tls_default, resolve_previews,
};
pub use reconcile::{
    BranchMoved, MissingReason, MissingTask, Orphan, OrphanKind, ReconcileReport, TrackedCheckout,
    reconcile,
};
pub use recreate::recreate_worktree;
pub use remove::{RemovalEvent, RemovalJob, RemovalState, RemoveOptions, reap_trash, start_remove};
pub use repo::{RepoInfo, Vcs, detect, repo_root};
pub use setup::{
    CancelToken, DEFAULT_SETUP_SCRIPT, SetupHandle, SetupOptions, SetupOutcome, SetupStatus,
    run_setup, setup_env, spawn_setup,
};
pub use status::{Blockers, BranchStatus, branch_status, removal_blockers};
pub use taskfile::{
    DepsSpec, DepsStrategy, FilesSpec, PlannedCommand, PortsSpec, SetupSpec, TASK_FILE, TaskFile,
    TemplateVars, denied_port_env_name, filter_port_env, parse_duration, parse_remote,
    repo_key_matches, trusted_port_env_name, untrusted_port_env_name, valid_env_name,
};
pub use worktree::{
    Checkout, CreateRequest, FetchOutcome, WorktreeConfig, WorktreeEntry, WorktreeRoot,
    create_worktree, default_base, find_worktree, find_worktree_by_branch, list_worktrees,
    open_worktree, restore_worktree, worktree_path,
};
