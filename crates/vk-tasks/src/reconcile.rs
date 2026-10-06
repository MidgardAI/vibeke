//! Reconcile stored tasks against the git worktrees on disk (05 §4).
//!
//! Read-only by construction: the report says what is missing or unknown,
//! and the caller (the server) only ever *marks* tasks and emits events.
//! Nothing here, or built on it, deletes a directory, branch or worktree.

use crate::Result;
use crate::git::same_path;
use crate::worktree::{WorktreeRoot, list_worktrees};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

/// A task's checkout as stored.
#[derive(Debug, Clone)]
pub struct TrackedCheckout {
    pub task_id: String,
    pub path: PathBuf,
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingReason {
    /// The directory no longer exists (removed outside Vibeke).
    DirectoryGone,
    /// The directory exists but git no longer lists it as a worktree of the repo.
    NotARegisteredWorktree,
    /// Git lists it but its directory is gone (`git worktree prune` would drop it).
    Prunable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MissingTask {
    pub task_id: String,
    pub path: PathBuf,
    pub reason: MissingReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BranchMoved {
    pub task_id: String,
    pub path: PathBuf,
    pub expected: Option<String>,
    pub actual: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OrphanKind {
    /// A git worktree of the repo inside the task root that no task owns:
    /// adoptable with `vibeke task adopt --path`.
    Worktree,
    /// A directory inside the task root that is not a registered worktree
    /// (leftover of a failed creation or removal). Inspect manually.
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Orphan {
    pub path: PathBuf,
    pub kind: OrphanKind,
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReconcileReport {
    pub missing: Vec<MissingTask>,
    pub branch_moved: Vec<BranchMoved>,
    pub orphans: Vec<Orphan>,
}

impl ReconcileReport {
    pub fn is_clean(&self) -> bool {
        self.missing.is_empty() && self.branch_moved.is_empty() && self.orphans.is_empty()
    }
}

/// Compare `tracked` (the tasks of one repo) with `git worktree list` of
/// `repo` and the contents of the task root.
pub fn reconcile(
    repo: &Path,
    root: &WorktreeRoot,
    tracked: &[TrackedCheckout],
) -> Result<ReconcileReport> {
    let listed = list_worktrees(repo)?;
    let mut rep = ReconcileReport::default();
    for t in tracked {
        let entry = listed.iter().find(|w| same_path(&w.path, &t.path));
        let exists = t.path.is_dir();
        match (entry, exists) {
            (_, false) => rep.missing.push(MissingTask {
                task_id: t.task_id.clone(),
                path: t.path.clone(),
                reason: if entry.is_some() {
                    MissingReason::Prunable
                } else {
                    MissingReason::DirectoryGone
                },
            }),
            (None, true) => rep.missing.push(MissingTask {
                task_id: t.task_id.clone(),
                path: t.path.clone(),
                reason: MissingReason::NotARegisteredWorktree,
            }),
            (Some(w), true) => {
                if t.branch.is_some() && w.branch != t.branch {
                    rep.branch_moved.push(BranchMoved {
                        task_id: t.task_id.clone(),
                        path: t.path.clone(),
                        expected: t.branch.clone(),
                        actual: w.branch.clone(),
                    });
                }
            }
        }
    }
    let owned = |p: &Path| tracked.iter().any(|t| same_path(&t.path, p));
    let in_root = |p: &Path| area_contains(root, repo, p);
    for w in listed.iter().filter(|w| !w.is_main) {
        if in_root(&w.path) && !owned(&w.path) && w.path.is_dir() {
            rep.orphans.push(Orphan {
                path: w.path.clone(),
                kind: OrphanKind::Worktree,
                branch: w.branch.clone(),
            });
        }
    }
    if let WorktreeRoot::Dir(d) = root {
        let name = repo
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Ok(rd) = fs::read_dir(d.join(&name)) {
            for e in rd.flatten() {
                let p = e.path();
                let fname = e.file_name().to_string_lossy().into_owned();
                if fname.starts_with('.') || !p.is_dir() {
                    continue;
                }
                let registered = listed.iter().any(|w| same_path(&w.path, &p));
                if !registered && !owned(&p) {
                    rep.orphans.push(Orphan {
                        path: p,
                        kind: OrphanKind::Directory,
                        branch: None,
                    });
                }
            }
        }
    }
    rep.orphans.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(rep)
}

/// Is `p` where this repo's task worktrees are created?
fn area_contains(root: &WorktreeRoot, repo: &Path, p: &Path) -> bool {
    let name = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let canon = |x: &Path| x.canonicalize().unwrap_or_else(|_| x.to_path_buf());
    let p = canon(p);
    match root {
        WorktreeRoot::Dir(d) => p.starts_with(canon(&d.join(&name))),
        WorktreeRoot::Sibling => {
            let parent = canon(repo.parent().unwrap_or(repo));
            p.parent() == Some(parent.as_path())
                && p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(&format!("{name}-")))
        }
    }
}
