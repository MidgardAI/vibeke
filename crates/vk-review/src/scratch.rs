//! Disposable reviewer checkouts (15 §6.1, T4).
//!
//! A reviewer run works in a detached checkout of the reviewed subject's content commit
//! instead of the task's own checkout. It is then not a known writer in the task's checkout
//! (Ready stays possible while it reviews), and whatever it edits cannot reach the user's work.
//! The checkout is a `git worktree add --detach` under a Vibeke-owned directory (or a
//! `git archive` export when worktrees are unavailable); removal only touches that path and
//! its own worktree metadata, never the user's checkout, index or branches.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::gitcmd::{self, GitError};

#[derive(Debug, thiserror::Error)]
pub enum ScratchError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("could not create the reviewer checkout: {0}")]
    Create(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// How a reviewer checkout was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScratchMethod {
    /// `git worktree add --detach` (shares the object store; metadata under `.git/worktrees`).
    Worktree,
    /// `git archive | tar -x` (no repository metadata at all).
    Archive,
}

/// A plain directory-name component (letters, digits, `-`, `_`).
fn safe_name(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Create a detached checkout of `sha` at `<root>/<id>`. Blocking.
pub fn create(
    repo: &Path,
    sha: &str,
    root: &Path,
    id: &str,
) -> Result<(PathBuf, ScratchMethod), ScratchError> {
    if !safe_name(id) {
        return Err(ScratchError::Create(format!("unsafe checkout name `{id}`")));
    }
    std::fs::create_dir_all(root)?;
    let path = root.join(id);
    if path.exists() {
        return Err(ScratchError::Create(format!(
            "{} already exists",
            path.display()
        )));
    }
    let p = path.to_string_lossy().into_owned();
    let out = gitcmd::run_raw(
        repo,
        &["worktree", "add", "--detach", "--quiet", &p, sha],
        Duration::from_secs(300),
    )?;
    if out.code == Some(0) {
        return Ok((path, ScratchMethod::Worktree));
    }
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path)?;
    let mut archive = gitcmd::command(repo)
        .args(["archive", "--format=tar", sha])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = archive.stdout.take().expect("piped");
    let tar = Command::new("tar")
        .arg("-xf")
        .arg("-")
        .arg("-C")
        .arg(&path)
        .stdin(Stdio::from(stdout))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    let a = archive.wait()?;
    if !a.success() || !tar.success() {
        let _ = std::fs::remove_dir_all(&path);
        return Err(ScratchError::Create(format!(
            "git archive {sha} | tar failed (archive {a}, tar {tar})"
        )));
    }
    Ok((path, ScratchMethod::Archive))
}

/// Remove a reviewer checkout created by [`create`] (idempotent). `root` must contain `path`:
/// nothing outside the Vibeke-owned directory is ever removed. Returns whether it is gone.
pub fn remove(repo: &Path, root: &Path, path: &Path, method: ScratchMethod) -> bool {
    if !path.starts_with(root) || path == root {
        return false;
    }
    if method == ScratchMethod::Worktree {
        let p = path.to_string_lossy().into_owned();
        let _ = gitcmd::run_raw(
            repo,
            &["worktree", "remove", "--force", "--force", &p],
            gitcmd::GIT_TIMEOUT,
        );
        let _ = gitcmd::run_raw(repo, &["worktree", "prune"], gitcmd::GIT_TIMEOUT);
    }
    if path.exists() {
        let _ = std::fs::remove_dir_all(path);
    }
    !path.exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subject::testrepo::TestRepo;

    #[test]
    fn creates_a_detached_checkout_and_removes_only_it() {
        let r = TestRepo::new();
        r.write("a.txt", "one\n");
        let sha = r.commit("base");
        r.write("a.txt", "user's uncommitted work\n");
        let status_before = r.git(&["status", "--porcelain=v2"]);
        let root = tempfile::tempdir().unwrap();
        let (path, method) = create(r.root(), &sha, root.path(), "rv_1").unwrap();
        assert_eq!(method, ScratchMethod::Worktree);
        assert_eq!(
            std::fs::read_to_string(path.join("a.txt")).unwrap(),
            "one\n"
        );
        // A reviewer editing its checkout never reaches the user's.
        std::fs::write(path.join("a.txt"), "reviewer edit\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(r.root().join("a.txt")).unwrap(),
            "user's uncommitted work\n"
        );
        assert_eq!(r.git(&["status", "--porcelain=v2"]), status_before);
        assert!(remove(r.root(), root.path(), &path, method));
        assert!(!path.exists());
        assert!(!r.git(&["worktree", "list"]).contains("rv_1"));
        // Paths outside the root are never removed.
        assert!(!remove(r.root(), root.path(), r.root(), method));
        assert!(r.root().exists());
    }

    #[test]
    fn refuses_unsafe_names() {
        let r = TestRepo::new();
        r.write("a.txt", "one\n");
        let sha = r.commit("base");
        let root = tempfile::tempdir().unwrap();
        assert!(create(r.root(), &sha, root.path(), "../x").is_err());
    }
}
