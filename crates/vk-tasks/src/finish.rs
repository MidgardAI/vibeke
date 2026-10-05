//! Finish / archive helpers (05 §8) that are pure git/fs operations.

use crate::Result;
use crate::git::{exec, git};
use crate::remove::{RemovalJob, RemoveOptions, start_remove};
use std::path::Path;

/// Is `branch` fully merged into `base`?
pub fn is_merged(repo: &Path, branch: &str, base: &str) -> Result<bool> {
    match exec(repo, &["merge-base", "--is-ancestor", branch, base], None) {
        Ok(_) => Ok(true),
        Err(crate::Error::Git { code: Some(1), .. }) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Local branches merged into `base` (`git branch --merged`), for the
/// `merged_branches = "suggest"` policy. `base` itself is excluded.
pub fn merged_branches(repo: &Path, base: &str) -> Result<Vec<String>> {
    let out = git(
        repo,
        &["branch", "--merged", base, "--format=%(refname:short)"],
    )?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|b| !b.is_empty() && *b != base)
        .map(str::to_string)
        .collect())
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffStat {
    pub files: u32,
    pub insertions: u32,
    pub deletions: u32,
}

/// Diff stat of the working tree (staged + unstaged) against `HEAD`, or
/// against the merge-base with `base` if given (what the task would add).
pub fn diff_stat(path: &Path, base: Option<&str>) -> Result<DiffStat> {
    let from = match base {
        Some(b) => git(path, &["merge-base", b, "HEAD"])?,
        None => "HEAD".to_string(),
    };
    let out = git(path, &["diff", "--numstat", &from])?;
    let mut s = DiffStat::default();
    for l in out.lines() {
        let mut it = l.split('\t');
        s.files += 1;
        // Binary files show "-".
        s.insertions += it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        s.deletions += it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    }
    Ok(s)
}

/// Archive a task checkout: delete the worktree directory asynchronously but
/// keep the branch, so [`crate::restore_worktree`] can recreate it.
/// Refuses dirty / unpushed checkouts unless `force`.
pub fn archive_worktree(path: &Path, mut opts: RemoveOptions) -> RemovalJob {
    opts.delete_branch = false;
    start_remove(path, opts)
}
