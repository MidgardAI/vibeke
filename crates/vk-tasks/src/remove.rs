//! Async, non-blocking worktree removal (05 §9).
//!
//! The job renames the checkout into a trash directory on the same
//! filesystem (atomic, instant), prunes git's metadata, then deletes the
//! trash on a background thread. [`RemovalEvent::Detached`] is the point at
//! which the checkout is gone from its path and the UI may return.

use crate::git::{git, same_path};
use crate::repo::repo_root;
use crate::status::removal_blockers;
use crate::worktree::{WorktreeEntry, find_worktree};
use crate::{Error, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use ulid::Ulid;

#[derive(Debug, Clone)]
pub struct RemoveOptions {
    /// Remove even if dirty / has unpushed commits / is locked.
    pub force: bool,
    /// Refuse dirty checkouts unless `force` (`tasks.cleanup.protect_dirty`).
    pub protect_dirty: bool,
    /// Where to move the checkout before deleting. Default: `.trash` next to
    /// the worktree directory. Must be on the same filesystem.
    pub trash_root: Option<PathBuf>,
    /// Also delete the branch (`git branch -d`, `-D` with `force`).
    pub delete_branch: bool,
}

impl Default for RemoveOptions {
    fn default() -> Self {
        Self {
            force: false,
            protect_dirty: true,
            trash_root: None,
            delete_branch: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemovalEvent {
    Started,
    /// Refused; nothing was touched.
    Refused {
        reason: String,
        dirty_files: u32,
        unpushed_commits: u32,
    },
    /// The checkout no longer exists at its path. `trash` is where the data
    /// went (None if `git worktree remove` was used as fallback).
    Detached {
        trash: Option<PathBuf>,
    },
    BranchDeleted(String),
    BranchKept {
        branch: String,
        reason: String,
    },
    /// Trash deleted; the job is complete.
    Removed,
    /// Failed; any trash entry is left for inspection.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemovalState {
    Running,
    /// Checkout is gone from its path; trash deletion continues.
    Detached,
    Done,
    Refused(String),
    Failed(String),
}

impl RemovalState {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Running | Self::Detached)
    }
}

/// Handle on a removal running on a background thread.
pub struct RemovalJob {
    pub id: Ulid,
    pub path: PathBuf,
    /// Progress events; ends (disconnects) after the terminal event.
    pub events: Receiver<RemovalEvent>,
    state: Arc<Mutex<RemovalState>>,
    handle: JoinHandle<()>,
}

impl RemovalJob {
    /// Poll the current state without blocking.
    pub fn state(&self) -> RemovalState {
        self.state
            .lock()
            .map(|s| s.clone())
            .unwrap_or(RemovalState::Running)
    }
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }
    /// Block until the job ends and return its final state.
    pub fn wait(self) -> RemovalState {
        let _ = self.handle.join();
        self.state
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }
}

/// Start removing the linked worktree at `path` on a background thread.
pub fn start_remove(path: &Path, opts: RemoveOptions) -> RemovalJob {
    let (tx, rx) = mpsc::channel();
    let state = Arc::new(Mutex::new(RemovalState::Running));
    let st = state.clone();
    let p = path.to_path_buf();
    let handle = thread::spawn(move || {
        let emit = |ev: RemovalEvent| {
            if let Ok(mut s) = st.lock() {
                match &ev {
                    RemovalEvent::Detached { .. } => *s = RemovalState::Detached,
                    RemovalEvent::Removed => *s = RemovalState::Done,
                    RemovalEvent::Refused { reason, .. } => {
                        *s = RemovalState::Refused(reason.clone())
                    }
                    RemovalEvent::Failed(e) => *s = RemovalState::Failed(e.clone()),
                    _ => {}
                }
            }
            let _ = tx.send(ev);
        };
        emit(RemovalEvent::Started);
        match run(&p, &opts, &emit) {
            Ok(()) => emit(RemovalEvent::Removed),
            Err(Error::Refused(reason)) => {
                let (d, u) = removal_blockers(&p)
                    .map(|b| (b.dirty_files, b.unpushed_commits))
                    .unwrap_or((0, 0));
                emit(RemovalEvent::Refused {
                    reason,
                    dirty_files: d,
                    unpushed_commits: u,
                });
            }
            Err(e) => emit(RemovalEvent::Failed(e.to_string())),
        }
    });
    RemovalJob {
        id: Ulid::new(),
        path: path.to_path_buf(),
        events: rx,
        state,
        handle,
    }
}

fn run(path: &Path, opts: &RemoveOptions, emit: &dyn Fn(RemovalEvent)) -> Result<()> {
    if !path.exists() {
        return Err(Error::WorktreeNotFound(format!(
            "{} (does not exist)",
            path.display()
        )));
    }
    let info = repo_root(path).ok_or_else(|| Error::NotARepo(path.to_path_buf()))?;
    if !info.is_linked_worktree || !same_path(&info.worktree_root, path) {
        return Err(Error::NotLinkedWorktree(path.to_path_buf()));
    }
    let main = info.root.clone();
    let entry: WorktreeEntry = find_worktree(&main, path)?;

    if entry.locked && !opts.force {
        return Err(Error::Refused(format!(
            "worktree is locked{}",
            entry
                .lock_reason
                .as_deref()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default()
        )));
    }
    if opts.protect_dirty && !opts.force {
        let b = removal_blockers(path)?;
        if !b.is_empty() {
            return Err(Error::Refused(format!(
                "checkout has {} uncommitted file(s) and {} unpushed commit(s); use force",
                b.dirty_files, b.unpushed_commits
            )));
        }
    }
    if entry.locked {
        let _ = git(&main, &["worktree", "unlock", &path.to_string_lossy()]);
    }

    let canon = path.canonicalize()?;
    let name = canon
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "worktree".into());
    let trash_root = opts
        .trash_root
        .clone()
        .or_else(|| canon.parent().map(|p| p.join(".trash")))
        .ok_or_else(|| Error::Config("cannot determine trash dir".into()))?;
    fs::create_dir_all(&trash_root)?;
    let trash = trash_root.join(format!("{name}-{}", Ulid::new()));

    let mut leftover = None;
    match fs::rename(&canon, &trash) {
        Ok(()) => {
            emit(RemovalEvent::Detached {
                trash: Some(trash.clone()),
            });
            git(&main, &["worktree", "prune"])?;
            leftover = Some(trash);
        }
        Err(_) => {
            // Cross-device or permissions: let git do the (slow) removal.
            git(
                &main,
                &[
                    "worktree",
                    "remove",
                    "--force",
                    "--force",
                    &canon.to_string_lossy(),
                ],
            )?;
            emit(RemovalEvent::Detached { trash: None });
        }
    }

    if opts.delete_branch
        && let Some(b) = &entry.branch
    {
        let flag = if opts.force { "-D" } else { "-d" };
        match git(&main, &["branch", flag, b]) {
            Ok(_) => emit(RemovalEvent::BranchDeleted(b.clone())),
            Err(e) => emit(RemovalEvent::BranchKept {
                branch: b.clone(),
                reason: e.to_string(),
            }),
        }
    }
    if let Some(t) = leftover {
        fs::remove_dir_all(&t)?;
    }
    Ok(())
}

/// Delete everything in a trash directory, one entry at a time (resumable
/// after a crash). Returns the entries that could not be deleted.
pub fn reap_trash(trash_root: &Path) -> Vec<(PathBuf, String)> {
    let mut failed = Vec::new();
    let Ok(rd) = fs::read_dir(trash_root) else {
        return failed;
    };
    for e in rd.flatten() {
        let p = e.path();
        let r = if p.is_dir() && !p.is_symlink() {
            fs::remove_dir_all(&p)
        } else {
            fs::remove_file(&p)
        };
        if let Err(err) = r {
            failed.push((p, err.to_string()));
        }
    }
    failed
}
