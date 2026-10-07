//! Merge orchestration (12 "Merge orchestration", 05 §10): claims on files and areas, conflict
//! prediction across live worktrees, and a merge queue.
//!
//! - **Claims** ([`Claim`]) are advisory globs a task declares (`task.claim`). They are input to
//!   [`predict`]: another task changing a claimed path is a `high` conflict.
//! - **Prediction** ([`predict`]) compares the changed-file sets of live worktrees (committed,
//!   staged, unstaged and untracked) and, for pairs that overlap and both have commits, asks
//!   `git merge-tree --write-tree` whether their commits would conflict textually. Uncommitted
//!   work can only be reported as an overlap. Prediction is advisory and never blocks anything.
//! - **The queue** ([`MergeQueue`], [`merge_branch`], [`run_next`]) lands task branches one at
//!   a time. Each merge is built in a **throwaway integration worktree** from the target's
//!   current tip, optionally gated by a check command, and only then is the target branch
//!   advanced: by compare-and-swap `update-ref` when the target is not checked out anywhere, or
//!   `merge --ff-only` inside the one clean worktree that has it. A target checked out with
//!   uncommitted changes blocks the entry instead of touching that checkout.

use crate::family::{CheckOutcome, run_check};
use crate::{Error, Result, gitx, glob_match, now_ms};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub id: String,
    /// Task id the claim belongs to.
    pub task: String,
    pub glob: String,
    #[serde(default)]
    pub note: Option<String>,
    pub created_at_ms: i64,
}

/// A claim is a repository-relative glob: not absolute, no `..`, not empty.
pub fn validate_claim_glob(g: &str) -> Result<()> {
    let g = g.trim();
    if g.is_empty() {
        return Err(Error::invalid("a claim needs a path glob"));
    }
    if g.starts_with('/') || g.split('/').any(|c| c == "..") {
        return Err(Error::invalid(
            "a claim glob is relative to the repository (no leading `/`, no `..`)",
        ));
    }
    Ok(())
}

pub fn claims_covering<'a>(claims: &'a [Claim], path: &str) -> Vec<&'a Claim> {
    claims
        .iter()
        .filter(|c| glob_match(&c.glob, path))
        .collect()
}

/// What one live worktree has changed relative to its base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changed {
    pub task: String,
    pub handle: String,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub committed: bool,
    pub files: BTreeSet<String>,
    /// The subset of `files` with uncommitted state in the worktree (staged, unstaged or
    /// untracked): the merge check cannot see those.
    #[serde(default)]
    pub dirty: BTreeSet<String>,
}

/// Paths with uncommitted state in `worktree` (staged, unstaged and untracked).
pub fn dirty_files(worktree: &Path) -> Result<BTreeSet<String>> {
    let mut files = BTreeSet::new();
    let diff = gitx::run_bytes(
        worktree,
        &["diff", "--name-only", "--no-renames", "-z", "HEAD"],
    )?;
    files.extend(gitx::split_nul(&diff));
    let untracked = gitx::run_bytes(
        worktree,
        &["ls-files", "-z", "--others", "--exclude-standard"],
    )?;
    files.extend(gitx::split_nul(&untracked));
    Ok(files)
}

/// Every path `worktree` changes against `base` (merge-base): committed work, staged,
/// unstaged and untracked.
pub fn changed_files(worktree: &Path, base: Option<&str>) -> Result<(BTreeSet<String>, bool)> {
    let from = match base {
        Some(b) => {
            let b = gitx::rev_parse(worktree, b).unwrap_or_else(|_| b.to_string());
            gitx::run(worktree, &["merge-base", &b, "HEAD"]).unwrap_or(b)
        }
        None => "HEAD".to_string(),
    };
    let mut files = BTreeSet::new();
    let diff = gitx::run_bytes(
        worktree,
        &["diff", "--name-only", "--no-renames", "-z", &from],
    )?;
    files.extend(gitx::split_nul(&diff));
    let untracked = gitx::run_bytes(
        worktree,
        &["ls-files", "-z", "--others", "--exclude-standard"],
    )?;
    files.extend(gitx::split_nul(&untracked));
    let committed = gitx::run(worktree, &["rev-list", "--count", &format!("{from}..HEAD")])
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0)
        > 0;
    Ok((files, committed))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// Both changed the same paths; the commits merge cleanly (or one side is uncommitted).
    Overlap,
    /// `git merge-tree` reports a content conflict between the two commits.
    Textual,
    /// One task changed a path another task claimed.
    Claim,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    /// Handles of the two tasks (for a claim: `a` changed, `b` owns the claim).
    pub a: String,
    pub b: String,
    pub kind: ConflictKind,
    pub severity: Severity,
    pub paths: Vec<String>,
    pub detail: String,
}

/// `Some(conflicted paths)` when the two commits conflict, `None` when they merge cleanly or
/// the question cannot be answered (git without `merge-tree --write-tree`).
fn textual_conflicts(repo: &Path, a: &str, b: &str) -> Option<Vec<String>> {
    let o = gitx::run_raw(
        repo,
        &[
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            a,
            b,
        ],
        None,
        &[],
    )
    .ok()?;
    if o.ok {
        return None;
    }
    if o.code != Some(1) {
        return None;
    }
    // First line: the (partial) tree id; then the conflicted paths until a blank line.
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    let mut lines = text.lines();
    lines.next()?;
    let paths: Vec<String> = lines
        .take_while(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    Some(paths)
}

/// Conflicts between every pair of live changes, plus claim violations.
pub fn predict(repo: &Path, changes: &[Changed], claims: &[Claim]) -> Vec<Conflict> {
    let mut out = vec![];
    for (i, a) in changes.iter().enumerate() {
        for b in changes.iter().skip(i + 1) {
            let both: Vec<String> = a.files.intersection(&b.files).cloned().collect();
            if both.is_empty() {
                continue;
            }
            let textual = match (a.committed && b.committed, &a.head, &b.head) {
                (true, Some(x), Some(y)) => textual_conflicts(repo, x, y),
                _ => None,
            };
            match textual {
                Some(paths) if !paths.is_empty() => out.push(Conflict {
                    a: a.handle.clone(),
                    b: b.handle.clone(),
                    kind: ConflictKind::Textual,
                    severity: Severity::High,
                    detail: format!(
                        "merging {} and {} conflicts in {} file(s)",
                        a.handle,
                        b.handle,
                        paths.len()
                    ),
                    paths,
                }),
                _ => {
                    // Per path: anything uncommitted on either side cannot be merge-checked.
                    let both_committed = a.committed && b.committed;
                    let (unchecked, clean): (Vec<String>, Vec<String>) =
                        both.into_iter().partition(|f| {
                            !both_committed || a.dirty.contains(f) || b.dirty.contains(f)
                        });
                    if !clean.is_empty() {
                        out.push(Conflict {
                            a: a.handle.clone(),
                            b: b.handle.clone(),
                            kind: ConflictKind::Overlap,
                            severity: Severity::Low,
                            detail: format!(
                                "both change {} file(s); the commits merge cleanly",
                                clean.len()
                            ),
                            paths: clean,
                        });
                    }
                    if !unchecked.is_empty() {
                        out.push(Conflict {
                            a: a.handle.clone(),
                            b: b.handle.clone(),
                            kind: ConflictKind::Overlap,
                            severity: Severity::Medium,
                            detail: format!(
                                "both change {} file(s); uncommitted work cannot be merge-checked yet",
                                unchecked.len()
                            ),
                            paths: unchecked,
                        });
                    }
                }
            }
        }
    }
    for c in changes {
        let mut by_owner: std::collections::BTreeMap<&str, Vec<String>> = Default::default();
        for f in &c.files {
            for claim in claims_covering(claims, f) {
                if claim.task != c.task {
                    by_owner
                        .entry(claim.task.as_str())
                        .or_default()
                        .push(f.clone());
                }
            }
        }
        for (owner, paths) in by_owner {
            let owner_handle = changes
                .iter()
                .find(|x| x.task == owner)
                .map(|x| x.handle.clone())
                .unwrap_or_else(|| owner.to_string());
            out.push(Conflict {
                a: c.handle.clone(),
                b: owner_handle.clone(),
                kind: ConflictKind::Claim,
                severity: Severity::High,
                detail: format!(
                    "{} changes {} path(s) claimed by {}",
                    c.handle,
                    paths.len(),
                    owner_handle
                ),
                paths,
            });
        }
    }
    out.sort_by(|x, y| {
        y.severity
            .cmp(&x.severity)
            .then(x.a.cmp(&y.a))
            .then(x.b.cmp(&y.b))
    });
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryState {
    Queued,
    Merged,
    /// The merge conflicts with the target; fix the branch and requeue.
    Conflict,
    /// The check command failed in the integration worktree.
    CheckFailed,
    /// The target cannot be advanced safely right now (dirty checkout).
    Blocked,
    Failed,
    Cancelled,
}

impl EntryState {
    pub fn as_str(self) -> &'static str {
        match self {
            EntryState::Queued => "queued",
            EntryState::Merged => "merged",
            EntryState::Conflict => "conflict",
            EntryState::CheckFailed => "check_failed",
            EntryState::Blocked => "blocked",
            EntryState::Failed => "failed",
            EntryState::Cancelled => "cancelled",
        }
    }
    pub fn is_open(self) -> bool {
        matches!(self, EntryState::Queued)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueEntry {
    pub id: String,
    pub task: String,
    pub handle: String,
    pub repo: String,
    pub branch: String,
    pub target: String,
    pub priority: i32,
    pub state: EntryState,
    pub added_at_ms: i64,
    pub updated_at_ms: i64,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub merge_commit: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub conflict_paths: Vec<String>,
    #[serde(default)]
    pub check: Option<CheckOutcome>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MergeQueue {
    pub entries: Vec<QueueEntry>,
}

impl MergeQueue {
    /// Queue a task branch. A task with an open entry cannot be queued twice.
    pub fn add(&mut self, mut e: QueueEntry) -> Result<&QueueEntry> {
        if self
            .entries
            .iter()
            .any(|x| x.task == e.task && x.state.is_open())
        {
            return Err(Error::Refused(format!(
                "{} is already in the merge queue",
                e.handle
            )));
        }
        e.state = EntryState::Queued;
        e.added_at_ms = e.added_at_ms.max(0);
        self.entries.push(e);
        Ok(self.entries.last().unwrap())
    }

    pub fn get(&self, id_or_task: &str) -> Option<&QueueEntry> {
        self.entries
            .iter()
            .rev()
            .find(|e| e.id == id_or_task || e.task == id_or_task || e.handle == id_or_task)
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut QueueEntry> {
        self.entries.iter_mut().find(|e| e.id == id)
    }

    /// Cancel an open entry.
    pub fn cancel(&mut self, id_or_task: &str) -> Result<()> {
        let id = self
            .entries
            .iter()
            .rev()
            .find(|e| {
                (e.id == id_or_task || e.task == id_or_task || e.handle == id_or_task)
                    && e.state.is_open()
            })
            .map(|e| e.id.clone())
            .ok_or_else(|| Error::invalid(format!("{id_or_task} has no open queue entry")))?;
        let e = self.get_mut(&id).unwrap();
        e.state = EntryState::Cancelled;
        e.updated_at_ms = now_ms();
        Ok(())
    }

    /// Put a conflict/check-failed/blocked/failed entry back in line.
    pub fn requeue(&mut self, id_or_task: &str) -> Result<()> {
        let id = self
            .entries
            .iter()
            .rev()
            .find(|e| e.id == id_or_task || e.task == id_or_task || e.handle == id_or_task)
            .map(|e| e.id.clone())
            .ok_or_else(|| Error::invalid(format!("{id_or_task} is not in the queue")))?;
        let task = self.get(&id).unwrap().task.clone();
        if self
            .entries
            .iter()
            .any(|x| x.task == task && x.state.is_open())
        {
            return Err(Error::Refused("already queued".into()));
        }
        let e = self.get_mut(&id).unwrap();
        if matches!(e.state, EntryState::Merged) {
            return Err(Error::Refused("already merged".into()));
        }
        e.state = EntryState::Queued;
        e.error = None;
        e.conflict_paths.clear();
        e.check = None;
        e.updated_at_ms = now_ms();
        Ok(())
    }

    /// Open entries in merge order: priority (high first), then arrival.
    pub fn order(&self) -> Vec<&QueueEntry> {
        let mut v: Vec<&QueueEntry> = self.entries.iter().filter(|e| e.state.is_open()).collect();
        v.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then(a.added_at_ms.cmp(&b.added_at_ms))
        });
        v
    }

    pub fn next(&self) -> Option<&QueueEntry> {
        self.order().into_iter().next()
    }

    /// Keep the log of closed entries bounded.
    pub fn trim(&mut self, keep_closed: usize) {
        let mut closed = 0usize;
        let mut keep: Vec<QueueEntry> = vec![];
        for e in self.entries.drain(..).rev() {
            if e.state.is_open() {
                keep.push(e);
            } else if closed < keep_closed {
                closed += 1;
                keep.push(e);
            }
        }
        keep.reverse();
        self.entries = keep;
    }
}

#[derive(Debug, Clone)]
pub struct MergeOptions {
    /// Branch to advance (short name).
    pub target: String,
    pub squash: bool,
    pub check: Option<(String, Duration)>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MergeOutcome {
    Merged {
        commit: String,
        /// The branch was already contained in the target.
        already: bool,
        /// How the target moved: `update_ref` | `fast_forward` | `none`.
        via: &'static str,
    },
    Conflict {
        paths: Vec<String>,
    },
    CheckFailed(CheckOutcome),
    Blocked {
        reason: String,
    },
}

fn worktrees_with_branch(repo: &Path, branch: &str) -> Result<Vec<PathBuf>> {
    let out = gitx::run(repo, &["worktree", "list", "--porcelain"])?;
    let mut cur: Option<PathBuf> = None;
    let mut hits = vec![];
    for l in out.lines() {
        if let Some(p) = l.strip_prefix("worktree ") {
            cur = Some(PathBuf::from(p));
        } else if l.strip_prefix("branch ") == Some(&format!("refs/heads/{branch}"))
            && let Some(p) = &cur
        {
            hits.push(p.clone());
        }
    }
    Ok(hits)
}

struct Integration {
    repo: PathBuf,
    path: PathBuf,
}
impl Drop for Integration {
    fn drop(&mut self) {
        let _ = gitx::run(
            &self.repo,
            &[
                "worktree",
                "remove",
                "--force",
                &self.path.to_string_lossy(),
            ],
        );
        let _ = std::fs::remove_dir_all(&self.path);
        let _ = gitx::run(&self.repo, &["worktree", "prune"]);
    }
}

fn identity_args(dir: &Path) -> Vec<&'static str> {
    let has = |k: &str| {
        gitx::run(dir, &["config", "--get", k])
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    };
    if has("user.name") && has("user.email") {
        vec![]
    } else {
        vec![
            "-c",
            "user.name=Vibeke",
            "-c",
            "user.email=vibeke@localhost",
        ]
    }
}

/// Merge `branch` into `opts.target` through an integration worktree (see the module docs).
pub fn merge_branch(repo: &Path, branch: &str, opts: &MergeOptions) -> Result<MergeOutcome> {
    let target_ref = format!("refs/heads/{}", opts.target);
    let tip = gitx::rev_parse(repo, &target_ref)
        .map_err(|_| Error::invalid(format!("target branch {} does not exist", opts.target)))?;
    let b = gitx::rev_parse(repo, &format!("refs/heads/{branch}"))
        .map_err(|_| Error::invalid(format!("branch {branch} does not exist")))?;
    if gitx::ok(repo, &["merge-base", "--is-ancestor", &b, &tip]) {
        return Ok(MergeOutcome::Merged {
            commit: tip,
            already: true,
            via: "none",
        });
    }
    let common = gitx::run(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let id = format!("{}-{}", std::process::id(), now_ms());
    let path = PathBuf::from(common).join("vibeke-merge").join(id);
    std::fs::create_dir_all(path.parent().unwrap())?;
    gitx::run(
        repo,
        &[
            "worktree",
            "add",
            "--detach",
            "-q",
            &path.to_string_lossy(),
            &tip,
        ],
    )?;
    let _guard = Integration {
        repo: repo.to_path_buf(),
        path: path.clone(),
    };
    let ident = identity_args(&path);
    let msg = opts
        .message
        .clone()
        .unwrap_or_else(|| format!("Merge branch '{branch}' into {}", opts.target));
    let mut args: Vec<&str> = ident.clone();
    if opts.squash {
        args.extend(["merge", "--squash", "--no-edit", &b]);
    } else {
        args.extend(["merge", "--no-ff", "--no-edit", "-m", &msg, &b]);
    }
    let o = gitx::run_raw(&path, &args, None, &[])?;
    if !o.ok {
        let unmerged = gitx::run_bytes(&path, &["diff", "--name-only", "--diff-filter=U", "-z"])
            .unwrap_or_default();
        let paths = gitx::split_nul(&unmerged);
        if paths.is_empty() {
            return Err(Error::Git {
                args: args.join(" "),
                code: o.code,
                stderr: o.stderr,
            });
        }
        let _ = gitx::run(&path, &["merge", "--abort"]);
        return Ok(MergeOutcome::Conflict { paths });
    }
    if opts.squash {
        let mut c: Vec<&str> = ident.clone();
        c.extend(["commit", "-q", "-m", &msg]);
        // An empty squash (nothing new) has nothing to commit.
        let staged = gitx::run(&path, &["diff", "--cached", "--name-only"])?;
        if staged.trim().is_empty() {
            return Ok(MergeOutcome::Merged {
                commit: tip,
                already: true,
                via: "none",
            });
        }
        gitx::run(&path, &c)?;
    }
    if let Some((cmd, timeout)) = &opts.check {
        let r = run_check(&path, cmd, *timeout);
        if !r.ok {
            return Ok(MergeOutcome::CheckFailed(r));
        }
    }
    let new = gitx::rev_parse(&path, "HEAD")?;
    let checkouts = worktrees_with_branch(repo, &opts.target)?;
    match checkouts.as_slice() {
        [] => {
            gitx::run(repo, &["update-ref", &target_ref, &new, &tip]).map_err(|_| {
                Error::Refused(format!("{} moved while merging; retry", opts.target))
            })?;
            Ok(MergeOutcome::Merged {
                commit: new,
                already: false,
                via: "update_ref",
            })
        }
        [w] => {
            let dirty = gitx::run(w, &["status", "--porcelain", "--untracked-files=no"])?;
            if !dirty.trim().is_empty() {
                return Ok(MergeOutcome::Blocked {
                    reason: format!(
                        "{} is checked out in {} with uncommitted changes",
                        opts.target,
                        w.display()
                    ),
                });
            }
            let now = gitx::rev_parse(w, "HEAD")?;
            if now != tip {
                return Err(Error::Refused(format!(
                    "{} moved while merging; retry",
                    opts.target
                )));
            }
            gitx::run(w, &["merge", "--ff-only", "-q", &new]).map_err(|e| {
                Error::Refused(format!("could not fast-forward {}: {e}", opts.target))
            })?;
            Ok(MergeOutcome::Merged {
                commit: new,
                already: false,
                via: "fast_forward",
            })
        }
        many => Ok(MergeOutcome::Blocked {
            reason: format!(
                "{} is checked out in {} worktrees ({})",
                opts.target,
                many.len(),
                many.iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }),
    }
}

/// Merge the next open entry (or the open entry named by `only`) and record the outcome on it.
/// Returns the entry id and outcome, or `None` when there is nothing to run.
pub fn run_next(
    queue: &mut MergeQueue,
    only: Option<&str>,
    check: Option<(String, Duration)>,
    squash: bool,
) -> Option<(String, Result<MergeOutcome>)> {
    let e = match only {
        Some(o) => queue
            .order()
            .into_iter()
            .find(|e| e.id == o || e.task == o || e.handle == o)?
            .clone(),
        None => queue.next()?.clone(),
    };
    let opts = MergeOptions {
        target: e.target.clone(),
        squash,
        check,
        message: None,
    };
    let r = merge_branch(Path::new(&e.repo), &e.branch, &opts);
    let slot = queue.entries.iter_mut().find(|x| x.id == e.id)?;
    slot.updated_at_ms = now_ms();
    match &r {
        Ok(MergeOutcome::Merged { commit, .. }) => {
            slot.state = EntryState::Merged;
            slot.merge_commit = Some(commit.clone());
            slot.error = None;
        }
        Ok(MergeOutcome::Conflict { paths }) => {
            slot.state = EntryState::Conflict;
            slot.conflict_paths = paths.clone();
            slot.error = Some(format!("conflicts in {} file(s)", paths.len()));
        }
        Ok(MergeOutcome::CheckFailed(c)) => {
            slot.state = EntryState::CheckFailed;
            slot.check = Some(c.clone());
            slot.error = Some(if c.timed_out {
                "check timed out".into()
            } else {
                "check failed".into()
            });
        }
        Ok(MergeOutcome::Blocked { reason }) => {
            slot.state = EntryState::Blocked;
            slot.error = Some(reason.clone());
        }
        Err(e) => {
            slot.state = EntryState::Failed;
            slot.error = Some(e.to_string());
        }
    }
    Some((e.id, r))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gitx::testutil::*;

    fn entry(r: &Repo, task: &str, branch: &str, prio: i32, at: i64) -> QueueEntry {
        QueueEntry {
            id: format!("q-{task}"),
            task: task.into(),
            handle: task.into(),
            repo: r.root.to_string_lossy().into_owned(),
            branch: branch.into(),
            target: "main".into(),
            priority: prio,
            state: EntryState::Queued,
            added_at_ms: at,
            updated_at_ms: at,
            note: None,
            merge_commit: None,
            error: None,
            conflict_paths: vec![],
            check: None,
        }
    }

    fn opts() -> MergeOptions {
        MergeOptions {
            target: "main".into(),
            squash: false,
            check: None,
            message: None,
        }
    }

    /// Two worktrees off main editing different and same files.
    fn two() -> (Repo, PathBuf, PathBuf) {
        let r = repo(&[
            ("a.txt", "a1\na2\na3\n"),
            ("b.txt", "b1\n"),
            ("c.txt", "c1\n"),
        ]);
        let w1 = worktree(&r, "t/one");
        let w2 = worktree(&r, "t/two");
        (r, w1, w2)
    }

    fn ch(task: &str, wt: &Path, base: &str, branch: &str) -> Changed {
        let (files, committed) = changed_files(wt, Some(base)).unwrap();
        Changed {
            task: task.into(),
            handle: task.into(),
            branch: Some(branch.into()),
            head: gitx::rev_parse(wt, "HEAD").ok(),
            committed,
            files,
            dirty: dirty_files(wt).unwrap(),
        }
    }

    #[test]
    fn claim_globs_are_validated_and_match() {
        assert!(validate_claim_glob("src/auth/**").is_ok());
        assert!(validate_claim_glob("").is_err());
        assert!(validate_claim_glob("/etc/**").is_err());
        assert!(validate_claim_glob("../x").is_err());
        let c = Claim {
            id: "c1".into(),
            task: "t1".into(),
            glob: "src/auth/**".into(),
            note: None,
            created_at_ms: 0,
        };
        assert_eq!(
            claims_covering(std::slice::from_ref(&c), "src/auth/login.rs").len(),
            1
        );
        assert!(claims_covering(&[c], "src/ui/x.rs").is_empty());
    }

    #[test]
    fn changed_files_cover_committed_staged_unstaged_and_untracked() {
        let (_r, w1, _w2) = two();
        write(&w1, "a.txt", "a1\nX\na3\n");
        commit_all(&w1, "c");
        write(&w1, "b.txt", "b1\nY\n");
        write(&w1, "new/u.txt", "u");
        let (f, committed) = changed_files(&w1, Some("main")).unwrap();
        assert!(committed);
        assert_eq!(
            f.into_iter().collect::<Vec<_>>(),
            vec!["a.txt", "b.txt", "new/u.txt"]
        );
        let (_r2, _a, w2) = two();
        let (f, committed) = changed_files(&w2, Some("main")).unwrap();
        assert!(f.is_empty() && !committed);
    }

    #[test]
    fn prediction_finds_textual_overlap_and_claim_conflicts() {
        let (r, w1, w2) = two();
        // conflicting edits to a.txt line 2, disjoint edits elsewhere
        write(&w1, "a.txt", "a1\nONE\na3\n");
        write(&w1, "b.txt", "b1\nmore\n");
        commit_all(&w1, "one");
        write(&w2, "a.txt", "a1\nTWO\na3\n");
        write(&w2, "c.txt", "c1\nmore\n");
        commit_all(&w2, "two");
        let changes = [
            ch("k1", &w1, "main", "t/one"),
            ch("k2", &w2, "main", "t/two"),
        ];
        let claims = [Claim {
            id: "c1".into(),
            task: "k2".into(),
            glob: "b.txt".into(),
            note: None,
            created_at_ms: 0,
        }];
        let cs = predict(&r.root, &changes, &claims);
        let textual = cs
            .iter()
            .find(|c| c.kind == ConflictKind::Textual)
            .expect("textual conflict");
        assert_eq!(textual.severity, Severity::High);
        assert_eq!(textual.paths, vec!["a.txt"]);
        let claim = cs
            .iter()
            .find(|c| c.kind == ConflictKind::Claim)
            .expect("claim conflict");
        assert_eq!((claim.a.as_str(), claim.b.as_str()), ("k1", "k2"));
        assert_eq!(claim.paths, vec!["b.txt"]);
        // High severity sorts first.
        assert_eq!(cs[0].severity, Severity::High);
    }

    #[test]
    fn overlap_without_conflict_and_uncommitted_work_are_lower_severity() {
        let (r, w1, w2) = two();
        // same file, different regions: merges cleanly
        write(&w1, "a.txt", "ONE\na2\na3\n");
        commit_all(&w1, "one");
        write(&w2, "a.txt", "a1\na2\nTWO\n");
        commit_all(&w2, "two");
        let cs = predict(
            &r.root,
            &[
                ch("k1", &w1, "main", "t/one"),
                ch("k2", &w2, "main", "t/two"),
            ],
            &[],
        );
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].kind, ConflictKind::Overlap);
        assert_eq!(cs[0].severity, Severity::Low);
        // uncommitted on one side: medium
        write(&w2, "b.txt", "x\n");
        write(&w1, "b.txt", "y\n");
        let cs = predict(
            &r.root,
            &[
                ch("k1", &w1, "main", "t/one"),
                ch("k2", &w2, "main", "t/two"),
            ],
            &[],
        );
        assert!(
            cs.iter()
                .any(|c| c.severity == Severity::Medium && c.paths == vec!["b.txt"])
        );
        // no overlap at all
        let (r3, a, b) = two();
        write(&a, "a.txt", "z\n");
        write(&b, "c.txt", "z\n");
        assert!(
            predict(
                &r3.root,
                &[ch("k1", &a, "main", "t/one"), ch("k2", &b, "main", "t/two")],
                &[]
            )
            .is_empty()
        );
        // a task's own claim is not a conflict with itself
        let own = [Claim {
            id: "c".into(),
            task: "k1".into(),
            glob: "a.txt".into(),
            note: None,
            created_at_ms: 0,
        }];
        assert!(predict(&r3.root, &[ch("k1", &a, "main", "t/one")], &own).is_empty());
    }

    #[test]
    fn queue_ordering_cancel_requeue_and_trim() {
        let r = repo(&[("a", "1")]);
        let mut q = MergeQueue::default();
        q.add(entry(&r, "t1", "b1", 0, 10)).unwrap();
        q.add(entry(&r, "t2", "b2", 5, 20)).unwrap();
        q.add(entry(&r, "t3", "b3", 5, 15)).unwrap();
        assert!(
            q.add(entry(&r, "t1", "b1", 0, 30)).is_err(),
            "duplicate open entry"
        );
        let order: Vec<_> = q.order().iter().map(|e| e.task.clone()).collect();
        assert_eq!(order, vec!["t3", "t2", "t1"]);
        assert_eq!(q.next().unwrap().task, "t3");
        q.cancel("t3").unwrap();
        assert!(q.cancel("t3").is_err());
        assert_eq!(q.next().unwrap().task, "t2");
        q.requeue("t3").unwrap();
        assert_eq!(q.next().unwrap().task, "t3");
        assert!(q.requeue("t3").is_err(), "already queued");
        assert!(q.requeue("nope").is_err());
        assert_eq!(q.get("b2").map(|e| e.task.as_str()), None);
        assert_eq!(q.get("t2").unwrap().id, "q-t2");
        // trim keeps open entries and the newest closed ones
        q.cancel("t1").unwrap();
        q.cancel("t2").unwrap();
        q.trim(1);
        assert_eq!(q.entries.iter().filter(|e| e.state.is_open()).count(), 1);
        assert_eq!(q.entries.len(), 2);
        let j = serde_json::to_string(&q).unwrap();
        assert_eq!(serde_json::from_str::<MergeQueue>(&j).unwrap(), q);
    }

    #[test]
    fn merges_into_a_checked_out_clean_target_by_fast_forward() {
        let (r, w1, _w2) = two();
        write(&w1, "a.txt", "a1\nONE\na3\n");
        commit_all(&w1, "one");
        let before = gitx::rev_parse(&r.root, "main").unwrap();
        let out = merge_branch(&r.root, "t/one", &opts()).unwrap();
        let MergeOutcome::Merged {
            commit,
            already,
            via,
        } = out
        else {
            panic!("{out:?}")
        };
        assert!(!already);
        assert_eq!(via, "fast_forward");
        assert_ne!(commit, before);
        assert_eq!(gitx::rev_parse(&r.root, "main").unwrap(), commit);
        // the checkout was updated too
        assert!(
            std::fs::read_to_string(r.root.join("a.txt"))
                .unwrap()
                .contains("ONE")
        );
        assert_eq!(gitx::run(&r.root, &["status", "--porcelain"]).unwrap(), "");
        // it is a real merge commit
        assert_eq!(
            gitx::run(&r.root, &["rev-list", "--parents", "-n1", "main"])
                .unwrap()
                .split(' ')
                .count(),
            3
        );
        // no integration worktree left behind
        let list = gitx::run(&r.root, &["worktree", "list", "--porcelain"]).unwrap();
        assert!(!list.contains("vibeke-merge"), "{list}");
        // merging again is a no-op
        let again = merge_branch(&r.root, "t/one", &opts()).unwrap();
        assert!(matches!(
            again,
            MergeOutcome::Merged {
                already: true,
                via: "none",
                ..
            }
        ));
    }

    #[test]
    fn merges_by_update_ref_when_the_target_is_not_checked_out() {
        let (r, w1, _w2) = two();
        write(&w1, "a.txt", "a1\nONE\na3\n");
        commit_all(&w1, "one");
        sh(&r.root, &["checkout", "-q", "-b", "elsewhere"]);
        let out = merge_branch(&r.root, "t/one", &opts()).unwrap();
        assert!(
            matches!(
                out,
                MergeOutcome::Merged {
                    via: "update_ref",
                    already: false,
                    ..
                }
            ),
            "{out:?}"
        );
        assert!(gitx::ok(
            &r.root,
            &["merge-base", "--is-ancestor", "t/one", "main"]
        ));
    }

    #[test]
    fn a_dirty_target_checkout_blocks_instead_of_being_touched() {
        let (r, w1, _w2) = two();
        write(&w1, "b.txt", "b1\nONE\n");
        commit_all(&w1, "one");
        write(&r.root, "c.txt", "c1\nlocal edit\n");
        let before = gitx::rev_parse(&r.root, "main").unwrap();
        let out = merge_branch(&r.root, "t/one", &opts()).unwrap();
        let MergeOutcome::Blocked { reason } = out else {
            panic!("{out:?}")
        };
        assert!(reason.contains("uncommitted"));
        assert_eq!(gitx::rev_parse(&r.root, "main").unwrap(), before);
        assert!(
            std::fs::read_to_string(r.root.join("c.txt"))
                .unwrap()
                .contains("local edit")
        );
    }

    #[test]
    fn conflicts_are_reported_and_leave_everything_alone() {
        let (r, w1, _w2) = two();
        write(&w1, "a.txt", "a1\nBRANCH\na3\n");
        commit_all(&w1, "branch");
        write(&r.root, "a.txt", "a1\nMAIN\na3\n");
        commit_all(&r.root, "main edit");
        let before = gitx::rev_parse(&r.root, "main").unwrap();
        let out = merge_branch(&r.root, "t/one", &opts()).unwrap();
        assert_eq!(
            out,
            MergeOutcome::Conflict {
                paths: vec!["a.txt".into()]
            }
        );
        assert_eq!(gitx::rev_parse(&r.root, "main").unwrap(), before);
        assert_eq!(gitx::run(&r.root, &["status", "--porcelain"]).unwrap(), "");
        assert!(
            !gitx::run(&r.root, &["worktree", "list", "--porcelain"])
                .unwrap()
                .contains("vibeke-merge")
        );
    }

    #[test]
    fn a_failing_check_stops_the_merge() {
        let (r, w1, _w2) = two();
        write(&w1, "b.txt", "b1\nONE\n");
        commit_all(&w1, "one");
        let before = gitx::rev_parse(&r.root, "main").unwrap();
        let mut o = opts();
        o.check = Some((
            "test -f b.txt && grep -q ONE b.txt && exit 7".into(),
            Duration::from_secs(20),
        ));
        let out = merge_branch(&r.root, "t/one", &o).unwrap();
        let MergeOutcome::CheckFailed(c) = out else {
            panic!("{out:?}")
        };
        assert_eq!(c.exit_code, Some(7));
        assert_eq!(gitx::rev_parse(&r.root, "main").unwrap(), before);
        // and a passing check lets it through, running against the merged tree
        o.check = Some(("grep -q ONE b.txt".into(), Duration::from_secs(20)));
        assert!(matches!(
            merge_branch(&r.root, "t/one", &o).unwrap(),
            MergeOutcome::Merged { .. }
        ));
    }

    #[test]
    fn squash_makes_one_commit_with_a_single_parent() {
        let (r, w1, _w2) = two();
        write(&w1, "b.txt", "b1\nONE\n");
        commit_all(&w1, "one");
        write(&w1, "b.txt", "b1\nONE\nTWO\n");
        commit_all(&w1, "two");
        let mut o = opts();
        o.squash = true;
        o.message = Some("feat: squashed".into());
        let out = merge_branch(&r.root, "t/one", &o).unwrap();
        assert!(matches!(out, MergeOutcome::Merged { .. }), "{out:?}");
        assert_eq!(
            gitx::run(&r.root, &["rev-list", "--parents", "-n1", "main"])
                .unwrap()
                .split(' ')
                .count(),
            2
        );
        assert_eq!(
            gitx::run(&r.root, &["log", "-1", "--format=%s", "main"]).unwrap(),
            "feat: squashed"
        );
        assert!(
            std::fs::read_to_string(r.root.join("b.txt"))
                .unwrap()
                .contains("TWO")
        );
    }

    #[test]
    fn missing_branches_are_errors() {
        let r = repo(&[("a", "1")]);
        assert!(merge_branch(&r.root, "nope", &opts()).is_err());
        let mut o = opts();
        o.target = "nope".into();
        assert!(merge_branch(&r.root, "main", &o).is_err());
    }

    #[test]
    fn run_next_lands_entries_in_order_and_records_outcomes() {
        let (r, w1, w2) = two();
        write(&w1, "a.txt", "a1\nONE\na3\n");
        commit_all(&w1, "one");
        write(&w2, "a.txt", "a1\nTWO\na3\n");
        commit_all(&w2, "two");
        let mut q = MergeQueue::default();
        q.add(entry(&r, "k1", "t/one", 0, 1)).unwrap();
        q.add(entry(&r, "k2", "t/two", 0, 2)).unwrap();
        let (id, out) = run_next(&mut q, None, None, false).unwrap();
        assert_eq!(id, "q-k1");
        assert!(matches!(out.unwrap(), MergeOutcome::Merged { .. }));
        assert_eq!(q.get("k1").unwrap().state, EntryState::Merged);
        assert!(q.get("k1").unwrap().merge_commit.is_some());
        // k2 now conflicts with what k1 landed
        let (id, out) = run_next(&mut q, None, None, false).unwrap();
        assert_eq!(id, "q-k2");
        assert!(matches!(out.unwrap(), MergeOutcome::Conflict { .. }));
        let e = q.get("k2").unwrap();
        assert_eq!(e.state, EntryState::Conflict);
        assert_eq!(e.conflict_paths, vec!["a.txt"]);
        assert!(
            run_next(&mut q, None, None, false).is_none(),
            "nothing left in line"
        );
        // fixing the branch and requeueing lets it land
        write(&w2, "a.txt", "a1\nONE\na3\n");
        sh(&w2, &["add", "-A"]);
        sh(&w2, &["commit", "-q", "--amend", "-m", "two fixed"]);
        sh(&w2, &["merge", "-q", "-s", "ours", "main", "-m", "sync"]);
        q.requeue("k2").unwrap();
        let (_, out) = run_next(&mut q, None, None, false).unwrap();
        assert!(matches!(out.unwrap(), MergeOutcome::Merged { .. }));
        assert_eq!(q.get("k2").unwrap().state, EntryState::Merged);
    }
}
