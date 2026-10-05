//! Branch status (05 §13) and removal blockers (05 §8 `protect_dirty`).

use crate::Result;
use crate::git::{git, git_timeout};
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BranchStatus {
    /// None when detached.
    pub branch: Option<String>,
    pub detached: bool,
    pub oid: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// What `ahead`/`behind` are measured against: the upstream, or the
    /// `base` passed to [`branch_status`].
    pub compared_to: Option<String>,
    /// Changed + untracked + conflicted entries.
    pub dirty_files: u32,
    pub untracked: u32,
    pub conflicts: u32,
}

/// Cheap status via `git status --porcelain=v2 --branch` (2 s timeout).
/// If the branch has no upstream and `base` is given, ahead/behind are
/// computed against `base`.
pub fn branch_status(path: &Path, base: Option<&str>) -> Result<BranchStatus> {
    let out = git_timeout(
        path,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=normal",
        ],
        Some(Duration::from_secs(2)),
    )?;
    let mut s = parse_status(&out);
    if s.upstream.is_some() {
        s.compared_to = s.upstream.clone();
    } else if let Some(b) = base
        && let Ok(o) = git_timeout(
            path,
            &[
                "rev-list",
                "--left-right",
                "--count",
                &format!("{b}...HEAD"),
            ],
            Some(Duration::from_secs(2)),
        )
    {
        let mut it = o.split_whitespace().filter_map(|x| x.parse::<u32>().ok());
        if let (Some(behind), Some(ahead)) = (it.next(), it.next()) {
            s.behind = behind;
            s.ahead = ahead;
            s.compared_to = Some(b.to_string());
        }
    }
    Ok(s)
}

pub(crate) fn parse_status(out: &str) -> BranchStatus {
    let mut s = BranchStatus::default();
    for line in out.lines() {
        if let Some(h) = line.strip_prefix("# branch.head ") {
            if h == "(detached)" {
                s.detached = true;
            } else {
                s.branch = Some(h.to_string());
            }
        } else if let Some(o) = line.strip_prefix("# branch.oid ") {
            s.oid = (o != "(initial)").then(|| o.to_string());
        } else if let Some(u) = line.strip_prefix("# branch.upstream ") {
            s.upstream = Some(u.to_string());
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for p in ab.split_whitespace() {
                if let Some(n) = p.strip_prefix('+') {
                    s.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = p.strip_prefix('-') {
                    s.behind = n.parse().unwrap_or(0);
                }
            }
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            s.dirty_files += 1;
        } else if line.starts_with("u ") {
            s.dirty_files += 1;
            s.conflicts += 1;
        } else if line.starts_with("? ") {
            s.dirty_files += 1;
            s.untracked += 1;
        }
    }
    s
}

/// What would be lost by deleting a checkout.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Blockers {
    pub dirty_files: u32,
    /// Commits on this checkout's HEAD not pushed to its upstream or, without
    /// an upstream, not reachable from any other branch or remote.
    pub unpushed_commits: u32,
}

impl Blockers {
    pub fn is_empty(&self) -> bool {
        self.dirty_files == 0 && self.unpushed_commits == 0
    }
}

pub fn removal_blockers(path: &Path) -> Result<Blockers> {
    let s = branch_status(path, None)?;
    let unpushed = if s.upstream.is_some() {
        s.ahead
    } else {
        let mut args: Vec<String> = vec!["rev-list".into(), "--count".into(), "HEAD".into()];
        args.push("--not".into());
        if let Some(b) = &s.branch {
            args.push(format!("--exclude={b}"));
        }
        args.push("--branches".into());
        args.push("--remotes".into());
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        git(path, &refs)
            .ok()
            .and_then(|o| o.trim().parse().ok())
            .unwrap_or(0)
    };
    Ok(Blockers {
        dirty_files: s.dirty_files,
        unpushed_commits: unpushed,
    })
}
