//! Recreating a task's checkout at its recorded path (05 §4, `task recreate` for a task the
//! reconcile loop marked `missing`).
//!
//! The directory was removed outside Vibeke, so git usually still lists the worktree as
//! prunable and refuses to check its branch out again. Recreation therefore first prunes the
//! stale worktree metadata of directories that no longer exist (`git worktree prune`, which
//! never touches a directory that exists), then adds the worktree back at the same path from
//! the kept branch. Nothing is created when the path exists or the branch is gone.

use crate::error::{Error, Result};
use crate::git::{git, ref_exists};
use crate::repo::repo_root;
use crate::worktree::{Checkout, FetchOutcome, list_worktrees};
use std::path::Path;

/// Recreate the worktree of `branch` at `path` in `repo`.
pub fn recreate_worktree(repo: &Path, path: &Path, branch: &str) -> Result<Checkout> {
    let info = repo_root(repo).ok_or_else(|| Error::NotARepo(repo.to_path_buf()))?;
    if path.exists() {
        return Err(Error::PathExists(path.to_path_buf()));
    }
    if !ref_exists(&info.root, &format!("refs/heads/{branch}")) {
        return Err(Error::Refused(format!(
            "branch {branch} no longer exists; the checkout cannot be recreated from it"
        )));
    }
    git(&info.root, &["worktree", "prune"])?;
    if let Some(w) = list_worktrees(&info.root)?
        .into_iter()
        .find(|w| w.branch.as_deref() == Some(branch))
    {
        return Err(Error::BranchInUse {
            branch: branch.to_string(),
            path: w.path,
        });
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let p = path.to_string_lossy().into_owned();
    git(&info.root, &["worktree", "add", &p, branch])?;
    let slug = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Checkout {
        path: path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
        branch: Some(branch.to_string()),
        base_ref: None,
        slug,
        repo_root: info.root,
        created_branch: false,
        fetch: FetchOutcome::Skipped,
        warnings: vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn sh(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn recreates_a_removed_worktree_from_its_branch() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        sh(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "a").unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-qm", "init"]);
        let wt = d.path().join("repo-fix");
        sh(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "vk/fix",
                wt.to_str().unwrap(),
            ],
        );
        std::fs::remove_dir_all(&wt).unwrap();
        let co = recreate_worktree(&repo, &wt, "vk/fix").unwrap();
        assert!(wt.join("a.txt").exists());
        assert_eq!(co.branch.as_deref(), Some("vk/fix"));
        // Exists now: refused.
        assert!(matches!(
            recreate_worktree(&repo, &wt, "vk/fix"),
            Err(Error::PathExists(_))
        ));
        // A missing branch is refused, nothing created.
        let other = d.path().join("repo-gone");
        assert!(recreate_worktree(&repo, &other, "vk/gone").is_err());
        assert!(!other.exists());
    }
}
