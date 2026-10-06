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
//! * **Files** – [`copy_files`] copies `copy_files` entries into the new
//!   worktree, preserving mode, never overwriting.
//! * **Setup** – [`run_setup`] (blocking) / [`spawn_setup`] (background,
//!   cancellable via [`SetupHandle::cancel`]) with timeout; env from
//!   [`setup_env`]; combined output goes to a log file.
//! * **Ports** – [`PortLeases`] over a state dir: `flock`ed lock file plus
//!   JSON lease table shared by all processes; leases expire when their
//!   owner pid dies; ports are bind-tested on 127.0.0.1 before being handed
//!   out.
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
//! * Backends: worktree, jj workspace ([`Jj`], M4) and none; `copy` files and
//!   `setup.script` (no `link`/deps). `clone` for container boxes lives in [`sync`]:
//!   the in-box clone script plus host-side fetch/push (`vibeke task sync`).

mod error;
mod files;
mod finish;
mod git;
mod jj;
mod naming;
mod ports;
mod remove;
mod repo;
mod setup;
mod status;
pub mod sync;
mod worktree;

pub use error::{Error, Result};
pub use files::{CopyOutcome, CopyResult, copy_files, default_copy_files};
pub use finish::{DiffStat, archive_worktree, diff_stat, is_merged, merged_branches};
pub use jj::{Jj, JjStatus, JjWorkspace, find_root as jj_root, is_colocated as jj_colocated};
pub use naming::{DEFAULT_SLUG_MAX, render_branch, slugify, slugify_raw, unique_slug, user_handle};
pub use ports::{Lease, LeaseRequest, PortLeases, PortPool};
pub use remove::{RemovalEvent, RemovalJob, RemovalState, RemoveOptions, reap_trash, start_remove};
pub use repo::{RepoInfo, Vcs, detect, repo_root};
pub use setup::{
    CancelToken, DEFAULT_SETUP_SCRIPT, SetupHandle, SetupOptions, SetupOutcome, SetupStatus,
    run_setup, setup_env, spawn_setup,
};
pub use status::{Blockers, BranchStatus, branch_status, removal_blockers};
pub use worktree::{
    Checkout, CreateRequest, FetchOutcome, WorktreeConfig, WorktreeEntry, WorktreeRoot,
    create_worktree, default_base, find_worktree, find_worktree_by_branch, list_worktrees,
    open_worktree, restore_worktree, worktree_path,
};
