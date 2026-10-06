//! Validated dirty-work snapshots (15 §5, §6.3, T4).
//!
//! A snapshot captures the checkout's complete uncommitted content — staged, unstaged and
//! untracked files, binary included (ignored files are not part of it; they belong in a
//! check's environment manifest) — into an immutable Git commit:
//!
//! 1. Digest the checkout ([`subject::observation_baseline`]: status, `diff --binary HEAD`,
//!    untracked file hashes).
//! 2. Copy the user's index to a private temporary file (keeping its mtime, so Git's racy-entry
//!    rules still apply), write its tree (the staged part), then `git add -A` **into the copy**
//!    and write the full working-tree tree. Objects go into the object store; the user's index,
//!    worktree and refs are untouched.
//! 3. Digest again. Only when both digests and HEAD agree is the capture consistent; otherwise
//!    retry, and after the last attempt report **Workspace changing — verification subject
//!    unavailable**.
//! 4. `commit-tree` with fixed author/committer/date (same content ⇒ same commit ⇒ same subject
//!    id) and keep it reachable under `refs/vibeke/snapshots/<commit>`.
//!
//! Matching digests narrow, but cannot rule out, a write that happened and was reverted inside
//! the window; the snapshot's content is nevertheless exactly what is stored and what checks
//! and acceptance name, so a later difference shows up as a new subject.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::gitcmd::{self, GitError};
use crate::subject::{self, ChangeSubject, DirtyState, SnapshotRef, SubjectError};
use crate::{new_id, now_ms};

/// The label shown when no consistent capture could be made (§5).
pub const WORKSPACE_CHANGING: &str = "Workspace changing — verification subject unavailable";

/// Private ref namespace for snapshot commits.
pub const REF_PREFIX: &str = "refs/vibeke/snapshots/";

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error(transparent)]
    Subject(#[from] SubjectError),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("{WORKSPACE_CHANGING} (the checkout changed during {attempts} capture attempt(s))")]
    WorkspaceChanging { attempts: u32 },
    #[error("nothing uncommitted to snapshot: the checkout is clean")]
    Clean,
    #[error("repository has no commits yet (unborn HEAD)")]
    UnbornHead,
    #[error("the index has unmerged paths; resolve the conflicts first")]
    Unmerged,
    #[error("snapshot capture failed: {0}")]
    Io(String),
}

impl SnapshotError {
    /// 07 `conflict` reason for API callers.
    pub fn reason(&self) -> &'static str {
        match self {
            SnapshotError::WorkspaceChanging { .. } => "workspace_changing",
            SnapshotError::Clean => "nothing_to_snapshot",
            SnapshotError::UnbornHead => "unborn_head",
            SnapshotError::Unmerged => "unmerged_paths",
            _ => "snapshot_failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotOptions {
    pub max_attempts: u32,
    pub retry_delay: Duration,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        SnapshotOptions {
            max_attempts: 3,
            retry_delay: Duration::from_millis(150),
        }
    }
}

/// Test/diagnostic interleaving point, called with the attempt number (1-based) after the
/// first digest and before the trees are written.
pub type CaptureHook<'a> = &'a mut dyn FnMut(u32);

/// Capture a validated dirty snapshot of the checkout containing `repo` against review base
/// `base_sha`. Blocking; run it off the state actor / render path.
pub fn capture_dirty_snapshot(
    repo: &Path,
    base_sha: &str,
    opts: &SnapshotOptions,
) -> Result<ChangeSubject, SnapshotError> {
    capture_dirty_snapshot_with(repo, base_sha, opts, &mut |_| {})
}

/// [`capture_dirty_snapshot`] with an interleaving hook (tests simulate concurrent writers).
pub fn capture_dirty_snapshot_with(
    repo: &Path,
    base_sha: &str,
    opts: &SnapshotOptions,
    hook: CaptureHook<'_>,
) -> Result<ChangeSubject, SnapshotError> {
    let identity = subject::repo_identity(repo)?;
    let root = PathBuf::from(&identity.root);
    let base = subject::rev_parse(&root, base_sha)?;
    let attempts = opts.max_attempts.max(1);
    for attempt in 1..=attempts {
        if attempt > 1 {
            std::thread::sleep(opts.retry_delay);
        }
        let before = subject::observation_baseline(&root)?;
        let Some(head) = before.head.clone() else {
            return Err(SnapshotError::UnbornHead);
        };
        match before.dirty_state {
            DirtyState::Clean => return Err(SnapshotError::Clean),
            DirtyState::Unknown => continue,
            DirtyState::Dirty => {}
        }
        hook(attempt);
        let trees = write_trees(&root);
        let after = subject::observation_baseline(&root)?;
        let consistent = after.head.as_deref() == Some(head.as_str())
            && after.change_digest.is_some()
            && after.change_digest == before.change_digest;
        let (staged_tree, tree) = match trees {
            // A file changing under `git add` fails it ("unable to index file"): that is a
            // concurrent writer, not a capture fault — retry. A failure on a quiet checkout is
            // a real error.
            Err(SnapshotError::Git(_)) if !consistent => continue,
            Err(e) => return Err(e),
            Ok(_) if !consistent => continue,
            Ok(t) => t,
        };
        let digest = before.change_digest.expect("dirty state has a digest");
        let commit = commit_snapshot(&root, &tree, &head, &base, staged_tree.as_deref())?;
        let ref_name = format!("{REF_PREFIX}{commit}");
        gitcmd::run(&root, &["update-ref", &ref_name, &commit])?;
        return Ok(ChangeSubject::dirty_snapshot(
            identity,
            base,
            head,
            digest,
            SnapshotRef {
                commit,
                tree,
                staged_tree,
                ref_name,
                attempts: attempt,
            },
            now_ms(),
        ));
    }
    Err(SnapshotError::WorkspaceChanging { attempts })
}

/// Removes the private index copy on drop.
struct TempIndex(PathBuf);

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let mut lock = self.0.clone().into_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(lock);
    }
}

/// `(staged tree, full working-tree tree)` written through a private copy of the index.
fn write_trees(root: &Path) -> Result<(Option<String>, String), SnapshotError> {
    let git_dir = PathBuf::from(gitcmd::run(root, &["rev-parse", "--absolute-git-dir"])?);
    let index = PathBuf::from(gitcmd::run(root, &["rev-parse", "--git-path", "index"])?);
    let index = if index.is_absolute() {
        index
    } else {
        root.join(index)
    };
    let tmp = TempIndex(git_dir.join(format!("vibeke-snapshot-{}.index", new_id())));
    let tmp_s = tmp.0.to_string_lossy().into_owned();
    let env = [("GIT_INDEX_FILE", tmp_s.as_str())];
    let staged = if index.exists() {
        std::fs::copy(&index, &tmp.0).map_err(|e| SnapshotError::Io(format!("index copy: {e}")))?;
        // Keep the original mtime: Git compares entry stat data against the index file's own
        // timestamp to decide which entries are racily clean and must be re-hashed.
        if let Ok(m) = std::fs::metadata(&index).and_then(|m| m.modified())
            && let Ok(f) = std::fs::OpenOptions::new().write(true).open(&tmp.0)
        {
            let _ = f.set_modified(m);
        }
        match gitcmd::run_env(root, &["write-tree"], &env) {
            Ok(t) => Some(t),
            Err(GitError::Failed { stderr, .. }) if stderr.contains("unmerged") => {
                return Err(SnapshotError::Unmerged);
            }
            Err(e) => return Err(e.into()),
        }
    } else {
        gitcmd::run_env(root, &["read-tree", "HEAD"], &env)?;
        None
    };
    gitcmd::run_env(root, &["add", "-A", "--", "."], &env)?;
    let tree = gitcmd::run_env(root, &["write-tree"], &env)?;
    Ok((staged, tree))
}

/// Deterministic snapshot commit: fixed identity and time, so identical content yields the
/// identical commit (and subject id).
fn commit_snapshot(
    root: &Path,
    tree: &str,
    head: &str,
    base: &str,
    staged: Option<&str>,
) -> Result<String, SnapshotError> {
    let msg = format!(
        "Vibeke dirty snapshot\n\nVibeke-Base: {base}\nVibeke-Head: {head}\nVibeke-Staged-Tree: {}\n",
        staged.unwrap_or("none")
    );
    let env = [
        ("GIT_AUTHOR_NAME", "Vibeke"),
        ("GIT_AUTHOR_EMAIL", "vibeke@localhost"),
        ("GIT_AUTHOR_DATE", "946684800 +0000"),
        ("GIT_COMMITTER_NAME", "Vibeke"),
        ("GIT_COMMITTER_EMAIL", "vibeke@localhost"),
        ("GIT_COMMITTER_DATE", "946684800 +0000"),
    ];
    let out = gitcmd::run_env(
        root,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit-tree",
            tree,
            "-p",
            head,
            "-m",
            &msg,
        ],
        &env,
    )?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subject::SubjectKind;
    use crate::subject::testrepo::TestRepo;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn fast() -> SnapshotOptions {
        SnapshotOptions {
            max_attempts: 3,
            retry_delay: Duration::from_millis(5),
        }
    }

    #[test]
    fn captures_staged_unstaged_untracked_and_binary_without_touching_the_checkout() {
        let r = TestRepo::new();
        r.write("a.txt", "base\n");
        r.write("keep.txt", "keep\n");
        r.write(".gitignore", "ignored.log\n");
        let base = r.commit("base");
        r.write("a.txt", "staged\n");
        r.git(&["add", "a.txt"]);
        r.write("a.txt", "staged\nunstaged\n");
        r.write("new/untracked.bin", "\0\x01\x02binary");
        r.write("ignored.log", "runtime noise");
        let status_before = r.git(&["status", "--porcelain=v2", "-z", "--untracked-files=all"]);
        let index_before = std::fs::read(r.root().join(".git/index")).unwrap();

        let s = capture_dirty_snapshot(r.root(), &base, &fast()).unwrap();
        assert_eq!(s.kind, SubjectKind::DirtySnapshot);
        assert!(s.is_immutable() && !s.is_committed() && s.verify_id());
        let sn = s.snapshot.clone().unwrap();
        assert_eq!(sn.attempts, 1);
        assert!(sn.ref_name.starts_with(REF_PREFIX));
        assert_eq!(s.head_sha, base);
        assert_eq!(s.content_sha(), sn.commit);
        // The snapshot holds the working-tree content (staged + unstaged + untracked + binary)…
        assert_eq!(
            r.git(&["show", &format!("{}:a.txt", sn.commit)]),
            "staged\nunstaged"
        );
        assert!(
            r.git(&["ls-tree", "-r", "--name-only", &sn.commit])
                .contains("new/untracked.bin")
        );
        let ignored = r.git(&["ls-tree", "-r", "--name-only", &sn.commit]);
        assert!(!ignored.contains("ignored.log"), "ignored files stay out");
        // …and the staged part separately.
        assert_eq!(
            r.git(&[
                "show",
                &format!("{}:a.txt", sn.staged_tree.clone().unwrap())
            ]),
            "staged"
        );
        // Diffs come from the immutable objects.
        let st = subject::diff_stat(&s).unwrap();
        assert!(
            st.files
                .iter()
                .any(|f| f.path == "new/untracked.bin" && f.added.is_none())
        );
        let d = subject::diff_text(&s, 1 << 20).unwrap();
        assert!(d.text.contains("+unstaged"));
        // The user's index, worktree and branch refs are untouched.
        assert_eq!(
            r.git(&["status", "--porcelain=v2", "-z", "--untracked-files=all"]),
            status_before
        );
        assert_eq!(
            std::fs::read(r.root().join(".git/index")).unwrap(),
            index_before
        );
        assert_eq!(r.git(&["rev-parse", "HEAD"]), base);
        assert_eq!(
            r.git(&["for-each-ref", "--format=%(refname)", "refs/heads"]),
            "refs/heads/main"
        );
        // No private index left behind.
        let leftovers: Vec<_> = std::fs::read_dir(r.root().join(".git"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("vibeke-snapshot")
            })
            .collect();
        assert!(leftovers.is_empty());

        // Content-addressed: the same content gives the same subject.
        let again = capture_dirty_snapshot(r.root(), &base, &fast()).unwrap();
        assert_eq!(again.id, s.id);
        // A different content gives a different one.
        r.write("new/untracked.bin", "changed");
        let other = capture_dirty_snapshot(r.root(), &base, &fast()).unwrap();
        assert_ne!(other.id, s.id);
        // A tampered snapshot record is refused.
        let mut t = s.clone();
        t.snapshot.as_mut().unwrap().commit = other.snapshot.clone().unwrap().commit;
        assert!(!t.verify_id());
        assert!(subject::diff_stat(&t).is_err());
    }

    #[test]
    fn clean_checkout_has_nothing_to_snapshot() {
        let r = TestRepo::new();
        r.write("a", "1");
        let base = r.commit("1");
        assert!(matches!(
            capture_dirty_snapshot(r.root(), &base, &fast()),
            Err(SnapshotError::Clean)
        ));
    }

    #[test]
    fn a_writer_during_capture_forces_a_retry() {
        let r = TestRepo::new();
        r.write("a.txt", "base\n");
        let base = r.commit("base");
        r.write("a.txt", "edit 1\n");
        let root = r.root().to_path_buf();
        let mut writes = 0;
        // The first attempt sees a write between its digests; the second is quiet.
        let s = capture_dirty_snapshot_with(&root, &base, &fast(), &mut |attempt| {
            if attempt == 1 {
                writes += 1;
                std::fs::write(root.join("a.txt"), "edit 2\n").unwrap();
            }
        })
        .unwrap();
        assert_eq!(writes, 1);
        let sn = s.snapshot.clone().unwrap();
        assert_eq!(sn.attempts, 2);
        assert_eq!(r.git(&["show", &format!("{}:a.txt", sn.commit)]), "edit 2");
        // The recorded digest is the one of the settled content.
        let now = subject::observation_baseline(r.root()).unwrap();
        assert_eq!(now.change_digest, s.dirty_digest);
    }

    #[test]
    fn a_continuous_writer_makes_the_subject_unavailable() {
        let r = TestRepo::new();
        r.write("a.txt", "base\n");
        let base = r.commit("base");
        r.write("a.txt", "dirty\n");
        let root = r.root().to_path_buf();
        // A hook-driven writer on every attempt.
        let mut n = 0;
        let e = capture_dirty_snapshot_with(&root, &base, &fast(), &mut |_| {
            n += 1;
            std::fs::write(root.join("a.txt"), format!("write {n}\n")).unwrap();
        })
        .unwrap_err();
        assert!(
            matches!(e, SnapshotError::WorkspaceChanging { attempts: 3 }),
            "{e}"
        );
        assert!(e.to_string().contains(WORKSPACE_CHANGING));
        assert_eq!(e.reason(), "workspace_changing");
        // No ref was created for an inconsistent capture.
        assert_eq!(r.git(&["for-each-ref", REF_PREFIX]), "");

        // A real concurrent writer thread (no hook): either a consistent snapshot of one of the
        // states it wrote, or "workspace changing" — never a mix.
        let stop = Arc::new(AtomicBool::new(false));
        let (stop2, root2) = (stop.clone(), root.clone());
        let writer = std::thread::spawn(move || {
            let mut i = 0u64;
            while !stop2.load(Ordering::Relaxed) {
                i += 1;
                std::fs::write(root2.join("a.txt"), format!("w {i}\n")).unwrap();
                std::fs::write(root2.join("b.txt"), format!("w {i}\n")).unwrap();
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let res = capture_dirty_snapshot(&root, &base, &fast());
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        match res {
            Err(SnapshotError::WorkspaceChanging { .. }) => {}
            Ok(s) => {
                let c = s.snapshot.unwrap().commit;
                let a = r.git(&["show", &format!("{c}:a.txt")]);
                let b = r.git(&["show", &format!("{c}:b.txt")]);
                assert_eq!(a, b, "a consistent snapshot holds one writer state");
            }
            Err(e) => panic!("unexpected {e}"),
        }
    }
}
