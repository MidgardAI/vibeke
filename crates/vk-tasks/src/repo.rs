//! Repo / VCS detection (05 §4 `detect`).

use crate::git::{git, ref_exists};
use crate::worktree::list_worktrees;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Vcs {
    Git,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInfo {
    /// Root of the *main* working tree (never a linked worktree).
    pub root: PathBuf,
    /// Top level of the worktree containing the queried cwd. Equals `root`
    /// unless `is_linked_worktree`.
    pub worktree_root: PathBuf,
    pub is_linked_worktree: bool,
    pub vcs: Vcs,
    /// Default branch name without remote prefix (`main`).
    pub default_branch: Option<String>,
    /// URL of remote `origin`.
    pub remote_url: Option<String>,
    /// Branch checked out in `worktree_root` (None when detached).
    pub current_branch: Option<String>,
}

/// Detect the git repository containing `cwd`. `None` if it is not in a
/// (non-bare) git repository or git is unavailable.
pub fn repo_root(cwd: &Path) -> Option<RepoInfo> {
    let out = git(
        cwd,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--absolute-git-dir",
            "--git-common-dir",
        ],
    )
    .ok()?;
    let mut lines = out.lines();
    let top = PathBuf::from(lines.next()?);
    let git_dir = PathBuf::from(lines.next()?);
    let common = PathBuf::from(lines.next()?);
    let worktree_root = top.canonicalize().unwrap_or(top);
    let linked = git_dir.canonicalize().ok() != common.canonicalize().ok();
    let root = if linked {
        match common.parent() {
            Some(p) if common.file_name().is_some_and(|n| n == ".git") => {
                p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
            }
            _ => list_worktrees(&worktree_root)
                .ok()
                .and_then(|l| l.into_iter().next())
                .map(|w| w.path)
                .unwrap_or_else(|| worktree_root.clone()),
        }
    } else {
        worktree_root.clone()
    };
    let current_branch = git(&worktree_root, &["symbolic-ref", "--short", "-q", "HEAD"])
        .ok()
        .filter(|s| !s.is_empty());
    Some(RepoInfo {
        default_branch: default_branch(&root, current_branch.as_deref()),
        remote_url: git(&root, &["config", "--get", "remote.origin.url"])
            .ok()
            .filter(|s| !s.is_empty()),
        root,
        worktree_root,
        is_linked_worktree: linked,
        vcs: Vcs::Git,
        current_branch,
    })
}

/// Like [`repo_root`] but always returns something: for non-repos
/// `vcs == Vcs::None` and `root == cwd` (the `none` isolation backend).
pub fn detect(cwd: &Path) -> RepoInfo {
    repo_root(cwd).unwrap_or_else(|| {
        let p = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        RepoInfo {
            root: p.clone(),
            worktree_root: p,
            is_linked_worktree: false,
            vcs: Vcs::None,
            default_branch: None,
            remote_url: None,
            current_branch: None,
        }
    })
}

fn default_branch(root: &Path, current: Option<&str>) -> Option<String> {
    if let Ok(s) = git(
        root,
        &["symbolic-ref", "--short", "-q", "refs/remotes/origin/HEAD"],
    ) && let Some(b) = s.strip_prefix("origin/")
    {
        return Some(b.to_string());
    }
    for cand in ["main", "master"] {
        if ref_exists(root, &format!("refs/heads/{cand}")) {
            return Some(cand.to_string());
        }
    }
    current.map(str::to_string)
}
