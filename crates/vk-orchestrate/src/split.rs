//! "Split into task" (05 §11): move a *running* agent's uncommitted changes out of a shared
//! checkout into a new task worktree.
//!
//! Copying or reverting files while other writers are active is not a transaction, `git diff`
//! omits staged changes by default and `git stash` without `-u` omits untracked files, so the
//! migration is a fixed sequence, each step checked before the next:
//!
//! 1. **Quiesce** every writer in the shared checkout ([`quiesce_check`]; the server interrupts
//!    the runs and waits for `idle`, this function refuses while anything may still write).
//! 2. **Capture** the full state ([`capture`]): staged, unstaged and untracked paths, plus a
//!    recovery commit under `refs/vibeke/split/<id>` that is never applied to the tree, and a
//!    digest of every changed path.
//! 3. **Select** the paths to move ([`select`]): an explicit list (attribution only
//!    pre-selects); refuses when the source changed since the capture.
//! 4. **Validate** applicability in the new worktree ([`validate`]: `git apply --check`)
//!    before the source is touched.
//! 5. **Apply** ([`apply`]), **verify** ([`verify`]: the destination's per-path state digest
//!    equals the source's) and only then **revert the source** ([`revert_source`]). The
//!    recovery ref stays until the caller drops it ([`drop_recovery`]).
//! 6. Resuming the agent in the new cwd is the server's job.
//!
//! Nothing here deletes data that is not in the recovery commit. [`execute`] runs steps 3-5 and
//! rolls the destination back on any failure, leaving the source exactly as it was.

use crate::{Error, Result, gitx, now_ms};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub const RECOVERY_NS: &str = "refs/vibeke/split";

/// One path with changes in the source checkout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub staged: bool,
    pub unstaged: bool,
    pub untracked: bool,
    pub deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Captured {
    pub id: String,
    pub head: String,
    pub branch: Option<String>,
    pub changes: Vec<FileChange>,
    pub recovery_ref: String,
    pub recovery_commit: String,
    /// State digest over every changed path at capture time.
    pub digest: String,
    pub captured_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    pub paths: Vec<String>,
    /// Digest of the selected paths in the source when they were selected.
    pub digest: String,
}

/// Something that may still write in the shared checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Writer {
    pub label: String,
    /// An agent run (interrupted by the caller) or another process.
    pub agent: bool,
    /// For an agent: its execution state is idle. For a process: not known to be writing.
    pub quiet: bool,
}

/// Step 1: refuse while a writer is not quiet or the source changed within `quiet_for_ms`.
/// `newest_change_ms` is the newest modification time among the changed paths.
pub fn quiesce_check(
    writers: &[Writer],
    newest_change_ms: Option<i64>,
    now_ms: i64,
    quiet_for_ms: i64,
) -> std::result::Result<(), Vec<String>> {
    let mut why = vec![];
    for w in writers.iter().filter(|w| !w.quiet) {
        why.push(if w.agent {
            format!(
                "{} is still working; interrupt it and wait for idle",
                w.label
            )
        } else {
            format!("{} may be writing files in the checkout", w.label)
        });
    }
    if let Some(t) = newest_change_ms
        && now_ms - t < quiet_for_ms
    {
        why.push(format!(
            "files changed {} ms ago (the checkout must be quiet for {} ms)",
            (now_ms - t).max(0),
            quiet_for_ms
        ));
    }
    if why.is_empty() { Ok(()) } else { Err(why) }
}

/// The newest modification time (ms) among `paths` that still exist.
pub fn newest_mtime_ms(root: &Path, paths: &[String]) -> Option<i64> {
    paths
        .iter()
        .filter_map(|p| std::fs::symlink_metadata(root.join(p)).ok())
        .filter_map(|m| m.modified().ok())
        .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .max()
}

/// Parse `git status --porcelain=v1 -z` output.
fn parse_status(raw: &[u8]) -> Result<Vec<FileChange>> {
    let mut out: Vec<FileChange> = vec![];
    for rec in gitx::split_nul(raw) {
        if rec.len() < 4 {
            continue;
        }
        let b = rec.as_bytes();
        let (x, y) = (b[0] as char, b[1] as char);
        let path = rec[3..].to_string();
        if x == '!' && y == '!' {
            continue;
        }
        if x == 'U' || y == 'U' || (x == 'A' && y == 'A') || (x == 'D' && y == 'D') {
            return Err(Error::Refused(format!(
                "{path} has unresolved merge conflicts; resolve them before splitting"
            )));
        }
        let untracked = x == '?' && y == '?';
        out.push(FileChange {
            path,
            staged: !untracked && x != ' ',
            unstaged: !untracked && y != ' ',
            untracked,
            deleted: x == 'D' || y == 'D',
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.dedup_by(|a, b| a.path == b.path);
    Ok(out)
}

/// Every changed path of `dir` (staged, unstaged, untracked; renames appear as delete + add).
pub fn changed_paths(dir: &Path) -> Result<Vec<FileChange>> {
    let raw = gitx::run_bytes(
        dir,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ],
    )?;
    parse_status(&raw)
}

/// Digest of the index entry and worktree content of each path (sorted), plus nothing else:
/// two checkouts at the same commit with equal digests have the same staged and working state
/// for those paths.
pub fn state_digest(dir: &Path, paths: &[String]) -> Result<String> {
    let mut sorted: Vec<&String> = paths.iter().collect();
    sorted.sort();
    sorted.dedup();
    let mut h = blake3::Hasher::new();
    for p in sorted {
        h.update(p.as_bytes());
        h.update(b"\0");
        let idx = gitx::run_bytes(dir, &["ls-files", "-s", "-z", "--", p])?;
        h.update(&idx);
        h.update(b"\0");
        let full = dir.join(p);
        match std::fs::symlink_metadata(&full) {
            Err(_) => {
                h.update(b"absent");
            }
            Ok(m) if m.file_type().is_symlink() => {
                h.update(b"link:");
                if let Ok(t) = std::fs::read_link(&full) {
                    h.update(t.to_string_lossy().as_bytes());
                }
            }
            Ok(m) if m.is_file() => {
                use std::os::unix::fs::PermissionsExt;
                h.update(if m.permissions().mode() & 0o111 != 0 {
                    b"x:"
                } else {
                    b"f:"
                });
                h.update(&std::fs::read(&full)?);
            }
            Ok(_) => {
                h.update(b"other");
            }
        }
        h.update(b"\n");
    }
    Ok(h.finalize().to_hex().to_string())
}

fn git_path(dir: &Path, name: &str) -> Result<PathBuf> {
    let p = gitx::run(dir, &["rev-parse", "--git-path", name])?;
    let p = PathBuf::from(p);
    Ok(if p.is_absolute() { p } else { dir.join(p) })
}

/// A commit holding the whole current state (tracked, staged and untracked non-ignored files)
/// on top of `HEAD`, kept under `refs/vibeke/split/<id>`. Built with a scratch index, so the
/// source's index, working tree and branch are untouched.
fn recovery_commit(source: &Path, id: &str) -> Result<(String, String)> {
    let idx = git_path(source, &format!("vibeke-split-{id}.idx"))?;
    let idx_s = idx.to_string_lossy().into_owned();
    let env = [("GIT_INDEX_FILE", idx_s.as_str())];
    let build = || -> Result<String> {
        gitx::run_env(source, &["read-tree", "HEAD"], &env)?;
        gitx::run_env(source, &["add", "-A"], &env)?;
        let tree = gitx::run_env(source, &["write-tree"], &env)?;
        gitx::run_env(
            source,
            &[
                "-c",
                "user.name=Vibeke",
                "-c",
                "user.email=vibeke@localhost",
                "-c",
                "commit.gpgsign=false",
                "commit-tree",
                &tree,
                "-p",
                "HEAD",
                "-m",
                &format!("vibeke split recovery point {id}"),
            ],
            &env,
        )
    };
    let commit = build();
    let _ = std::fs::remove_file(&idx);
    let commit = commit?;
    let r = format!("{RECOVERY_NS}/{id}");
    gitx::run(source, &["update-ref", &r, &commit])?;
    Ok((r, commit))
}

/// Step 2. `id` names the recovery ref; the digest covers every changed path.
pub fn capture(source: &Path, id: &str) -> Result<Captured> {
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(Error::invalid("split id must be alphanumeric"));
    }
    let head = gitx::rev_parse(source, "HEAD")?;
    let changes = changed_paths(source)?;
    if changes.is_empty() {
        return Err(Error::Refused(
            "nothing to split: the checkout has no uncommitted changes".into(),
        ));
    }
    let branch = gitx::run(source, &["symbolic-ref", "--short", "-q", "HEAD"]).ok();
    let paths: Vec<String> = changes.iter().map(|c| c.path.clone()).collect();
    let digest = state_digest(source, &paths)?;
    let (recovery_ref, recovery_commit) = recovery_commit(source, id)?;
    Ok(Captured {
        id: id.to_string(),
        head,
        branch,
        changes,
        recovery_ref,
        recovery_commit,
        digest,
        captured_at_ms: now_ms(),
    })
}

/// Step 3: choose the paths to move (`None` = everything). Refuses when the source changed
/// since the capture, so what is moved is exactly what was shown.
pub fn select(source: &Path, cap: &Captured, paths: Option<&[String]>) -> Result<Selection> {
    let all: Vec<String> = cap.changes.iter().map(|c| c.path.clone()).collect();
    if state_digest(source, &all)? != cap.digest
        || changed_paths(source)?
            .iter()
            .map(|c| &c.path)
            .collect::<Vec<_>>()
            != cap.changes.iter().map(|c| &c.path).collect::<Vec<_>>()
    {
        return Err(Error::Refused(
            "the checkout changed since the capture; quiesce the writers and capture again".into(),
        ));
    }
    let chosen: Vec<String> = match paths {
        None => all,
        Some(p) => {
            let known: BTreeSet<&String> = cap.changes.iter().map(|c| &c.path).collect();
            let mut v = vec![];
            for x in p {
                if !known.contains(x) {
                    return Err(Error::invalid(format!("{x} has no uncommitted changes")));
                }
                if !v.contains(x) {
                    v.push(x.clone());
                }
            }
            if v.is_empty() {
                return Err(Error::invalid("no paths selected"));
            }
            v
        }
    };
    let digest = state_digest(source, &chosen)?;
    Ok(Selection {
        paths: chosen,
        digest,
    })
}

struct Patches {
    /// Index versus `HEAD` for the selected tracked paths.
    staged: Vec<u8>,
    /// Working tree versus index.
    unstaged: Vec<u8>,
    /// Working tree versus `HEAD` (the combined result, used to validate).
    combined: Vec<u8>,
    untracked: Vec<String>,
}

fn pathspec_args<'a>(base: &[&'a str], paths: &'a [String]) -> Vec<&'a str> {
    let mut v: Vec<&str> = base.to_vec();
    v.push("--");
    v.extend(paths.iter().map(String::as_str));
    v
}

fn patches(source: &Path, cap: &Captured, sel: &Selection) -> Result<Patches> {
    let tracked: Vec<String> = cap
        .changes
        .iter()
        .filter(|c| !c.untracked && sel.paths.contains(&c.path))
        .map(|c| c.path.clone())
        .collect();
    let untracked: Vec<String> = cap
        .changes
        .iter()
        .filter(|c| c.untracked && sel.paths.contains(&c.path))
        .map(|c| c.path.clone())
        .collect();
    let (staged, unstaged, combined) = if tracked.is_empty() {
        (vec![], vec![], vec![])
    } else {
        (
            gitx::run_bytes(
                source,
                &pathspec_args(
                    &[
                        "diff",
                        "--cached",
                        "--binary",
                        "--no-renames",
                        "--no-ext-diff",
                    ],
                    &tracked,
                ),
            )?,
            gitx::run_bytes(
                source,
                &pathspec_args(
                    &["diff", "--binary", "--no-renames", "--no-ext-diff"],
                    &tracked,
                ),
            )?,
            gitx::run_bytes(
                source,
                &pathspec_args(
                    &["diff", "HEAD", "--binary", "--no-renames", "--no-ext-diff"],
                    &tracked,
                ),
            )?,
        )
    };
    Ok(Patches {
        staged,
        unstaged,
        combined,
        untracked,
    })
}

fn same_commit(a: &Path, b: &Path) -> Result<()> {
    let (x, y) = (gitx::rev_parse(a, "HEAD")?, gitx::rev_parse(b, "HEAD")?);
    if x == y {
        Ok(())
    } else {
        Err(Error::Refused(format!(
            "the new worktree is at {} but the source is at {}; create the task from the source's HEAD",
            &y[..y.len().min(12)],
            &x[..x.len().min(12)]
        )))
    }
}

/// Step 4: can the selection be applied in `dest`? Nothing is modified.
pub fn validate(source: &Path, dest: &Path, cap: &Captured, sel: &Selection) -> Result<()> {
    same_commit(source, dest)?;
    if !changed_paths(dest)?.is_empty() {
        return Err(Error::Refused(
            "the new worktree is not clean; it must be a fresh checkout".into(),
        ));
    }
    let p = patches(source, cap, sel)?;
    if !p.staged.is_empty() {
        let o = gitx::run_raw(
            dest,
            &["apply", "--check", "--index", "--binary", "-"],
            Some(&p.staged),
            &[],
        )?;
        if !o.ok {
            return Err(Error::Refused(format!(
                "staged changes do not apply: {}",
                o.stderr
            )));
        }
    }
    if !p.combined.is_empty() {
        let o = gitx::run_raw(
            dest,
            &["apply", "--check", "--binary", "-"],
            Some(&p.combined),
            &[],
        )?;
        if !o.ok {
            return Err(Error::Refused(format!(
                "working-tree changes do not apply: {}",
                o.stderr
            )));
        }
    }
    for u in &p.untracked {
        if std::fs::symlink_metadata(dest.join(u)).is_ok() {
            return Err(Error::Refused(format!(
                "{u} already exists in the new worktree"
            )));
        }
        safe_rel(u)?;
    }
    Ok(())
}

fn safe_rel(p: &str) -> Result<()> {
    let path = Path::new(p);
    if path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        })
    {
        return Err(Error::Refused(format!("{p}: path escapes the checkout")));
    }
    Ok(())
}

fn copy_untracked(source: &Path, dest: &Path, rel: &str) -> Result<()> {
    safe_rel(rel)?;
    let from = source.join(rel);
    let to = dest.join(rel);
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let md = std::fs::symlink_metadata(&from)?;
    if md.file_type().is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(&from)?, &to)?;
    } else {
        std::fs::copy(&from, &to)?;
    }
    Ok(())
}

/// Step 5a: apply the selection in `dest` (staged with `--index`, then the working-tree part,
/// then the untracked files). On any failure the destination is rolled back and the error
/// returned; the source is never modified here.
pub fn apply(source: &Path, dest: &Path, cap: &Captured, sel: &Selection) -> Result<()> {
    let p = patches(source, cap, sel)?;
    let mut created: Vec<String> = vec![];
    let result = (|| -> Result<()> {
        if !p.staged.is_empty() {
            gitx::run_in(dest, &["apply", "--index", "--binary", "-"], &p.staged)?;
        }
        if !p.unstaged.is_empty() {
            gitx::run_in(dest, &["apply", "--binary", "-"], &p.unstaged)?;
        }
        for u in &p.untracked {
            copy_untracked(source, dest, u)?;
            created.push(u.clone());
        }
        Ok(())
    })();
    if result.is_err() {
        // The destination was clean (validate), so resetting it loses nothing.
        let _ = gitx::run(dest, &["reset", "-q", "--hard", "HEAD"]);
        for u in created {
            let _ = std::fs::remove_file(dest.join(u));
        }
    }
    result
}

/// Step 5b: the destination now has the same staged and working state for the selected paths.
pub fn verify(dest: &Path, sel: &Selection) -> Result<()> {
    let got = state_digest(dest, &sel.paths)?;
    if got == sel.digest {
        Ok(())
    } else {
        Err(Error::Refused(
            "the new worktree does not match the selected state after applying".into(),
        ))
    }
}

/// Step 5c: restore the selected paths in the source to `HEAD` (index and working tree) and
/// delete the moved untracked files. Refuses when the selected state changed since step 3.
pub fn revert_source(source: &Path, cap: &Captured, sel: &Selection) -> Result<()> {
    if state_digest(source, &sel.paths)? != sel.digest {
        return Err(Error::Refused(
            "the source changed since the selection; it was not reverted".into(),
        ));
    }
    for c in cap.changes.iter().filter(|c| sel.paths.contains(&c.path)) {
        if c.untracked {
            std::fs::remove_file(source.join(&c.path))?;
            // Remove emptied parent directories the untracked file lived in (never the root).
            let mut d = source.join(&c.path);
            while d.pop() && d != source {
                if std::fs::remove_dir(&d).is_err() {
                    break;
                }
            }
            continue;
        }
        let in_head = gitx::ok(source, &["cat-file", "-e", &format!("HEAD:{}", c.path)]);
        if in_head {
            gitx::run(source, &["checkout", "-q", "HEAD", "--", &c.path])?;
        } else {
            gitx::run(
                source,
                &[
                    "rm",
                    "-q",
                    "-f",
                    "--cached",
                    "--ignore-unmatch",
                    "--",
                    &c.path,
                ],
            )?;
            let _ = std::fs::remove_file(source.join(&c.path));
        }
    }
    let left = gitx::run(
        source,
        &pathspec_args(
            &["status", "--porcelain", "--untracked-files=all"],
            &sel.paths,
        ),
    )?;
    if !left.trim().is_empty() {
        return Err(Error::Refused(format!(
            "paths still changed in the source after reverting: {left}"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitResult {
    pub moved: Vec<String>,
    pub recovery_ref: String,
    pub source_reverted: bool,
}

/// Steps 4-5: validate, apply, verify, then revert the source. Any failure before the revert
/// leaves the source exactly as it was and the destination rolled back. A failure of the
/// revert itself keeps the destination (the work exists in both places and in the recovery
/// ref) and is reported as an error naming that state.
pub fn execute(source: &Path, dest: &Path, cap: &Captured, sel: &Selection) -> Result<SplitResult> {
    validate(source, dest, cap, sel)?;
    apply(source, dest, cap, sel)?;
    if let Err(e) = verify(dest, sel) {
        let _ = gitx::run(dest, &["reset", "-q", "--hard", "HEAD"]);
        for c in cap
            .changes
            .iter()
            .filter(|c| c.untracked && sel.paths.contains(&c.path))
        {
            let _ = std::fs::remove_file(dest.join(&c.path));
        }
        return Err(e);
    }
    revert_source(source, cap, sel).map_err(|e| {
        Error::Refused(format!(
            "{e}; the changes are applied in the new worktree and also still in the source (recovery: {})",
            cap.recovery_ref
        ))
    })?;
    Ok(SplitResult {
        moved: sel.paths.clone(),
        recovery_ref: cap.recovery_ref.clone(),
        source_reverted: true,
    })
}

/// Delete a recovery ref once the destination is verified and the agent resumed there.
pub fn drop_recovery(source: &Path, id: &str) -> Result<()> {
    let r = format!("{RECOVERY_NS}/{id}");
    if gitx::ok(source, &["rev-parse", "--verify", "-q", &r]) {
        gitx::run(source, &["update-ref", "-d", &r])?;
    }
    Ok(())
}

/// The recovery refs in a repository: (id, commit).
pub fn recovery_refs(repo: &Path) -> Result<Vec<(String, String)>> {
    let out = gitx::run(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            RECOVERY_NS,
        ],
    )?;
    Ok(out
        .lines()
        .filter_map(|l| {
            let (r, c) = l.split_once(' ')?;
            Some((
                r.strip_prefix(&format!("{RECOVERY_NS}/"))?.to_string(),
                c.to_string(),
            ))
        })
        .collect())
}

/// The ordered steps shown by a dry run.
pub fn plan_steps(cap: &Captured, sel: &Selection) -> Vec<(String, String)> {
    let staged = cap
        .changes
        .iter()
        .filter(|c| sel.paths.contains(&c.path) && c.staged)
        .count();
    let unstaged = cap
        .changes
        .iter()
        .filter(|c| sel.paths.contains(&c.path) && c.unstaged)
        .count();
    let untracked = cap
        .changes
        .iter()
        .filter(|c| sel.paths.contains(&c.path) && c.untracked)
        .count();
    vec![
        (
            "quiesce".into(),
            "interrupt every run in the checkout and wait for idle".into(),
        ),
        (
            "capture".into(),
            format!(
                "{} changed path(s); recovery ref {}",
                cap.changes.len(),
                cap.recovery_ref
            ),
        ),
        (
            "select".into(),
            format!(
                "{} path(s): {staged} staged, {unstaged} unstaged, {untracked} untracked",
                sel.paths.len()
            ),
        ),
        (
            "validate".into(),
            "git apply --check in the new worktree".into(),
        ),
        (
            "apply".into(),
            "apply staged, then unstaged, then copy untracked files".into(),
        ),
        (
            "verify".into(),
            "the new worktree's state digest equals the selection's".into(),
        ),
        (
            "revert_source".into(),
            "restore the moved paths in the source".into(),
        ),
        (
            "resume".into(),
            "resume the agent (or hand it off) in the new worktree".into(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gitx::testutil::*;

    /// A source checkout with a staged edit, an unstaged edit, a file that is both, a staged
    /// new file, a deleted file and untracked files; and a fresh worktree of the same commit.
    fn setup() -> (Repo, PathBuf) {
        let r = repo(&[
            ("a.txt", "a1\na2\na3\n"),
            ("b.txt", "b1\nb2\n"),
            ("c.txt", "c1\n"),
            ("gone.txt", "bye\n"),
            ("bin.dat", "\u{1}\u{2}"),
        ]);
        write(&r.root, "a.txt", "a1\nSTAGED\na3\n");
        sh(&r.root, &["add", "a.txt"]);
        write(&r.root, "b.txt", "b1\nb2\nUNSTAGED\n");
        // both staged and unstaged
        write(&r.root, "c.txt", "c1\nc-staged\n");
        sh(&r.root, &["add", "c.txt"]);
        write(&r.root, "c.txt", "c1\nc-staged\nc-unstaged\n");
        write(&r.root, "new.txt", "brand new\n");
        sh(&r.root, &["add", "new.txt"]);
        std::fs::remove_file(r.root.join("gone.txt")).unwrap();
        write(&r.root, "dir/untracked.txt", "u1\nu2\n");
        write(&r.root, "top-untracked.txt", "t\n");
        let wt = worktree(&r, "split/dest");
        (r, wt)
    }

    #[test]
    fn quiesce_refuses_active_writers_and_recent_changes() {
        let ok = [Writer {
            label: "a1".into(),
            agent: true,
            quiet: true,
        }];
        assert!(quiesce_check(&ok, Some(1_000), 10_000, 3_000).is_ok());
        assert!(quiesce_check(&ok, None, 10_000, 3_000).is_ok());
        let busy = [
            Writer {
                label: "a1".into(),
                agent: true,
                quiet: false,
            },
            Writer {
                label: "w3:p2 (make)".into(),
                agent: false,
                quiet: false,
            },
        ];
        let e = quiesce_check(&busy, Some(9_000), 10_000, 3_000).unwrap_err();
        assert_eq!(e.len(), 3);
        assert!(e[0].contains("interrupt"));
        assert!(e[1].contains("may be writing"));
        assert!(e[2].contains("changed"));
    }

    #[test]
    fn status_parsing_classifies_changes() {
        let (r, _wt) = setup();
        let ch = changed_paths(&r.root).unwrap();
        let get = |p: &str| ch.iter().find(|c| c.path == p).unwrap().clone();
        assert!(get("a.txt").staged && !get("a.txt").unstaged);
        assert!(!get("b.txt").staged && get("b.txt").unstaged);
        assert!(get("c.txt").staged && get("c.txt").unstaged);
        assert!(get("new.txt").staged);
        assert!(get("gone.txt").deleted);
        assert!(get("dir/untracked.txt").untracked);
        assert!(get("top-untracked.txt").untracked);
    }

    #[test]
    fn unresolved_conflicts_refuse() {
        let r = repo(&[("f.txt", "base\n")]);
        let w = worktree(&r, "x/side");
        write(&w, "f.txt", "side\n");
        commit_all(&w, "side");
        write(&r.root, "f.txt", "main\n");
        commit_all(&r.root, "main");
        let o = gitx::run_raw(&r.root, &["merge", "x/side"], None, &[]).unwrap();
        assert!(!o.ok);
        let e = capture(&r.root, "s1").unwrap_err();
        assert!(e.to_string().contains("conflicts"), "{e}");
    }

    #[test]
    fn capture_makes_a_recovery_commit_without_touching_the_tree() {
        let (r, _wt) = setup();
        let before = gitx::run(&r.root, &["status", "--porcelain"]).unwrap();
        let cap = capture(&r.root, "s1").unwrap();
        assert_eq!(
            gitx::run(&r.root, &["status", "--porcelain"]).unwrap(),
            before
        );
        assert_eq!(cap.changes.len(), 7, "{:?}", cap.changes);
        // The recovery commit has the untracked files and the unstaged content.
        let show =
            |p: &str| gitx::run(&r.root, &["show", &format!("{}:{p}", cap.recovery_ref)]).unwrap();
        assert_eq!(show("dir/untracked.txt"), "u1\nu2");
        assert!(show("c.txt").contains("c-unstaged"));
        assert!(
            gitx::run(
                &r.root,
                &["cat-file", "-e", &format!("{}:gone.txt", cap.recovery_ref)]
            )
            .is_err()
        );
        assert_eq!(recovery_refs(&r.root).unwrap().len(), 1);
        // No stray scratch index.
        let idx = git_path(&r.root, "vibeke-split-s1.idx").unwrap();
        assert!(!idx.exists());
        assert!(capture(&r.root, "bad id").is_err());
    }

    #[test]
    fn execute_moves_everything_and_reverts_the_source() {
        let (r, wt) = setup();
        let cap = capture(&r.root, "s1").unwrap();
        let sel = select(&r.root, &cap, None).unwrap();
        assert_eq!(sel.paths.len(), cap.changes.len());
        validate(&r.root, &wt, &cap, &sel).unwrap();
        let res = execute(&r.root, &wt, &cap, &sel).unwrap();
        assert!(res.source_reverted);
        // Destination has the staged/unstaged split preserved.
        let st = gitx::run(&wt, &["status", "--porcelain", "-uall"]).unwrap();
        assert!(st.contains("M  a.txt"), "{st}");
        assert!(st.contains(" M b.txt"), "{st}");
        assert!(st.contains("MM c.txt"), "{st}");
        assert!(st.contains("A  new.txt"), "{st}");
        assert!(
            st.contains(" D gone.txt") || st.contains("D  gone.txt"),
            "{st}"
        );
        assert!(st.contains("?? dir/untracked.txt"), "{st}");
        assert_eq!(
            std::fs::read_to_string(wt.join("c.txt")).unwrap(),
            "c1\nc-staged\nc-unstaged\n"
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("dir/untracked.txt")).unwrap(),
            "u1\nu2\n"
        );
        // Source is clean again.
        assert_eq!(gitx::run(&r.root, &["status", "--porcelain"]).unwrap(), "");
        assert!(!r.root.join("dir").exists(), "emptied directory removed");
        // The recovery ref still holds the original state until dropped.
        assert_eq!(recovery_refs(&r.root).unwrap().len(), 1);
        drop_recovery(&r.root, "s1").unwrap();
        assert!(recovery_refs(&r.root).unwrap().is_empty());
        drop_recovery(&r.root, "s1").unwrap();
    }

    #[test]
    fn a_selection_moves_only_the_chosen_paths() {
        let (r, wt) = setup();
        let cap = capture(&r.root, "s2").unwrap();
        let sel = select(
            &r.root,
            &cap,
            Some(&["b.txt".into(), "dir/untracked.txt".into()]),
        )
        .unwrap();
        execute(&r.root, &wt, &cap, &sel).unwrap();
        let st = gitx::run(&wt, &["status", "--porcelain", "-uall"]).unwrap();
        assert!(
            st.contains(" M b.txt") && st.contains("?? dir/untracked.txt"),
            "{st}"
        );
        assert!(!st.contains("a.txt") && !st.contains("c.txt"));
        let src = gitx::run(&r.root, &["status", "--porcelain"]).unwrap();
        assert!(
            src.contains("a.txt") && src.contains("c.txt") && src.contains("top-untracked.txt"),
            "{src}"
        );
        assert!(!src.contains("b.txt") && !src.contains("dir/"));
        assert!(select(&r.root, &cap, Some(&["nope.txt".into()])).is_err());
    }

    #[test]
    fn select_refuses_when_the_source_changed_after_capture() {
        let (r, _wt) = setup();
        let cap = capture(&r.root, "s3").unwrap();
        write(&r.root, "b.txt", "b1\nb2\nUNSTAGED\nLATE\n");
        let e = select(&r.root, &cap, None).unwrap_err();
        assert!(e.to_string().contains("changed since the capture"));
        let cap2 = capture(&r.root, "s4").unwrap();
        write(&r.root, "extra-untracked.txt", "x");
        assert!(select(&r.root, &cap2, None).is_err());
    }

    #[test]
    fn validate_refuses_before_touching_anything() {
        let (r, wt) = setup();
        let cap = capture(&r.root, "s5").unwrap();
        let sel = select(&r.root, &cap, None).unwrap();
        // Destination not clean.
        write(&wt, "stray.txt", "x");
        assert!(
            validate(&r.root, &wt, &cap, &sel)
                .unwrap_err()
                .to_string()
                .contains("not clean")
        );
        std::fs::remove_file(wt.join("stray.txt")).unwrap();
        // Destination at another commit.
        write(&wt, "other.txt", "o");
        commit_all(&wt, "moved on");
        assert!(
            validate(&r.root, &wt, &cap, &sel)
                .unwrap_err()
                .to_string()
                .contains("HEAD")
        );
        // Failed execute leaves the source untouched.
        let before = gitx::run(&r.root, &["status", "--porcelain"]).unwrap();
        assert!(execute(&r.root, &wt, &cap, &sel).is_err());
        assert_eq!(
            gitx::run(&r.root, &["status", "--porcelain"]).unwrap(),
            before
        );
    }

    #[test]
    fn a_patch_that_does_not_apply_rolls_the_destination_back() {
        let (r, wt) = setup();
        let cap = capture(&r.root, "s6").unwrap();
        let sel = select(
            &r.root,
            &cap,
            Some(&["a.txt".into(), "dir/untracked.txt".into()]),
        )
        .unwrap();
        // Break the destination's copy of a.txt in the same commit as the source... by making
        // the untracked path exist there (validate catches it first).
        write(&wt, "dir/untracked.txt", "already here\n");
        let e = validate(&r.root, &wt, &cap, &sel).unwrap_err();
        assert!(
            e.to_string().contains("not clean") || e.to_string().contains("already exists"),
            "{e}"
        );
        // Direct apply with a conflicting untracked path still rolls back the tracked part.
        std::fs::remove_file(wt.join("dir/untracked.txt")).unwrap();
        std::fs::create_dir_all(wt.join("dir")).unwrap();
        // make copy fail: a *directory* where the file must go
        std::fs::create_dir_all(wt.join("dir/untracked.txt")).unwrap();
        let e = apply(&r.root, &wt, &cap, &sel);
        assert!(e.is_err());
        let st = gitx::run(&wt, &["status", "--porcelain", "-uall"]).unwrap();
        assert!(!st.contains("a.txt"), "tracked part rolled back: {st}");
    }

    #[test]
    fn verify_detects_a_mismatch() {
        let (r, wt) = setup();
        let cap = capture(&r.root, "s7").unwrap();
        let sel = select(&r.root, &cap, Some(&["b.txt".into()])).unwrap();
        assert!(verify(&wt, &sel).is_err(), "nothing applied yet");
        apply(&r.root, &wt, &cap, &sel).unwrap();
        verify(&wt, &sel).unwrap();
        write(&wt, "b.txt", "tampered\n");
        assert!(verify(&wt, &sel).is_err());
    }

    #[test]
    fn revert_refuses_when_the_source_moved_on() {
        let (r, wt) = setup();
        let cap = capture(&r.root, "s8").unwrap();
        let sel = select(&r.root, &cap, Some(&["b.txt".into()])).unwrap();
        apply(&r.root, &wt, &cap, &sel).unwrap();
        write(&r.root, "b.txt", "b1\nb2\nUNSTAGED\nMORE\n");
        let e = revert_source(&r.root, &cap, &sel).unwrap_err();
        assert!(e.to_string().contains("changed since the selection"));
        assert!(
            std::fs::read_to_string(r.root.join("b.txt"))
                .unwrap()
                .contains("MORE")
        );
    }

    #[test]
    fn binary_and_executable_files_survive() {
        let r = repo(&[("bin.dat", "x"), ("run.sh", "#!/bin/sh\n")]);
        std::fs::write(r.root.join("bin.dat"), [0u8, 159, 146, 150, 0, 1]).unwrap();
        std::fs::write(r.root.join("new.bin"), [0u8, 1, 2, 3, 255]).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                r.root.join("new.bin"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        sh(&r.root, &["add", "new.bin"]);
        let wt = worktree(&r, "split/bin");
        let cap = capture(&r.root, "s9").unwrap();
        let sel = select(&r.root, &cap, None).unwrap();
        execute(&r.root, &wt, &cap, &sel).unwrap();
        assert_eq!(
            std::fs::read(wt.join("bin.dat")).unwrap(),
            vec![0u8, 159, 146, 150, 0, 1]
        );
        assert_eq!(
            std::fs::read(wt.join("new.bin")).unwrap(),
            vec![0u8, 1, 2, 3, 255]
        );
        let ls = gitx::run(&wt, &["ls-files", "-s", "new.bin"]).unwrap();
        assert!(ls.starts_with("100755"), "{ls}");
    }

    #[test]
    fn nothing_to_split_is_refused() {
        let r = repo(&[("a", "1")]);
        assert!(
            capture(&r.root, "e")
                .unwrap_err()
                .to_string()
                .contains("nothing to split")
        );
    }

    #[test]
    fn plan_steps_describe_the_sequence() {
        let (r, _wt) = setup();
        let cap = capture(&r.root, "s10").unwrap();
        let sel = select(&r.root, &cap, Some(&["a.txt".into()])).unwrap();
        let steps = plan_steps(&cap, &sel);
        let names: Vec<_> = steps.iter().map(|s| s.0.as_str()).collect();
        assert_eq!(
            names,
            [
                "quiesce",
                "capture",
                "select",
                "validate",
                "apply",
                "verify",
                "revert_source",
                "resume"
            ]
        );
        assert!(steps[2].1.contains("1 staged"));
    }
}
