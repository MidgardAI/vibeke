//! The repository-side import: commit → new worktree on a new branch → uncommitted changes →
//! untracked files → transcript. Validated before anything is created; rolled back on failure.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{
    Error, Manifest, Result, git, git_line, install_transcript, safe_relative, safe_tree_path,
    write_new_file,
};

/// A file the import could not write; the rest of the import went ahead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NotWritten {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Imported {
    pub worktree: PathBuf,
    pub branch: String,
    /// The agent's working directory inside the worktree.
    pub cwd: PathBuf,
    /// A transcript was installed where the harness resumes from.
    pub resumed: bool,
    /// Rebuilt locally from the harness and the (possibly fresh) session id; never the manifest's.
    pub resume_args: Option<Vec<String>>,
    pub not_written: Vec<NotWritten>,
}

/// The manifest someone reviewed must be exactly the one inside the bundle, and well-formed.
pub fn verify(packed: &Manifest, reviewed: &Manifest) -> Result<()> {
    if serde_json::to_value(packed).ok() != serde_json::to_value(reviewed).ok() {
        return Err(Error::new("conflict", "manifest does not match the bundle"));
    }
    check(packed)
}

fn check(m: &Manifest) -> Result<()> {
    if m.head.len() != 40 || !m.head.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::new(
            "invalid_params",
            "head must be a full commit id",
        ));
    }
    if !m.cwd_rel.is_empty() && !safe_relative(&m.cwd_rel) {
        return Err(Error::new(
            "invalid_params",
            "manifest cwd is not a relative path inside the repository",
        ));
    }
    // A crafted bundle must never write repository metadata (`.git/config`, hooks, ...): refuse
    // the whole bundle before anything is created.
    if let Some(bad) = m.untracked.iter().find(|rel| !safe_tree_path(rel)) {
        return Err(Error::new(
            "invalid_params",
            format!(
                "the bundle lists an untracked file outside the working tree: {}",
                crate::clean(bad, 200)
            ),
        ));
    }
    if !m.cwd_rel.is_empty() && !safe_tree_path(&m.cwd_rel) {
        return Err(Error::new(
            "invalid_params",
            "manifest cwd is inside the git directory",
        ));
    }
    Ok(())
}

fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

async fn branch_exists(root: &Path, br: &str) -> bool {
    git(
        root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{br}"),
        ],
    )
    .await
    .is_ok()
}

async fn has_commit(root: &Path, commit: &str) -> bool {
    git(root, &["cat-file", "-e", &format!("{commit}^{{commit}}")])
        .await
        .is_ok()
}

async fn valid_branch(root: &Path, br: &str) -> bool {
    !br.starts_with('-')
        && git(root, &["check-ref-format", "--branch", br])
            .await
            .is_ok()
}

/// Import the unpacked bundle in `work` (described by `m`) into the repository at `root`, as a new
/// worktree on a new branch. `worktree` and `branch` default to `<repo>-handoff-<branch>` next to
/// the repository and `handoff/<branch>`, with `-2`, `-3`, ... on collisions; chosen ones must not
/// exist yet. Nothing this import did not create is ever removed, and nothing it created is left
/// behind on failure.
pub async fn import(
    work: &Path,
    m: &Manifest,
    root: &Path,
    worktree: Option<&Path>,
    branch: Option<&str>,
) -> Result<Imported> {
    check(m)?;
    let bundle = work.join("repo.bundle");
    let bs = bundle.to_str().unwrap_or_default();
    if m.bundle != "none" {
        // The bundle's advertised HEAD must be that commit.
        let heads = git(work, &["bundle", "list-heads", bs]).await?;
        if !String::from_utf8_lossy(&heads)
            .lines()
            .any(|l| l.starts_with(&m.head))
        {
            return Err(Error::new(
                "conflict",
                "bundle HEAD does not match the manifest",
            ));
        }
    }
    if let Some(wt) = worktree {
        if !wt.is_absolute() {
            return Err(Error::new(
                "invalid_params",
                "the worktree path must be absolute",
            ));
        }
        if exists(wt) {
            return Err(Error::new(
                "conflict",
                format!("{} already exists", wt.display()),
            ));
        }
    }
    if let Some(br) = branch {
        if !valid_branch(root, br).await {
            return Err(Error::new(
                "invalid_params",
                format!("{br} is not a valid branch name"),
            ));
        }
        if branch_exists(root, br).await {
            return Err(Error::new(
                "conflict",
                format!("branch {br} already exists"),
            ));
        }
    }

    // Objects → commit.
    let commit = m.head.as_str();
    if m.bundle != "none" {
        if git(root, &["fetch", "--no-tags", bs, "HEAD"])
            .await
            .is_err()
        {
            git(root, &["fetch", "--no-tags", "origin"]).await?;
            git(root, &["fetch", "--no-tags", bs, "HEAD"]).await?;
        }
    } else if !has_commit(root, commit).await {
        git(root, &["fetch", "--no-tags", "origin"]).await?;
    }
    if !has_commit(root, commit).await {
        return Err(Error::new(
            "conflict",
            "the handed-off commit is not available",
        ));
    }

    // Placement.
    let base = m.branch.clone().unwrap_or_else(|| "detached".into());
    let slug: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let base_branch = if valid_branch(root, &format!("handoff/{base}")).await {
        format!("handoff/{base}")
    } else {
        format!("handoff/{slug}")
    };
    let parent = root.parent().unwrap_or(root).to_path_buf();
    let repo_name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".into());
    // Each candidate is claimed atomically: the directory with `mkdir` and the branch with
    // `git branch` (a ref lock), so an import running at the same time that picked the same
    // name simply moves on, and a failure removes only what this import created.
    let mut placed = None;
    for n in 1..100 {
        let suffix = if n == 1 {
            String::new()
        } else {
            format!("-{n}")
        };
        let wt = worktree
            .map(Path::to_path_buf)
            .unwrap_or_else(|| parent.join(format!("{repo_name}-handoff-{slug}{suffix}")));
        let br = branch
            .map(str::to_string)
            .unwrap_or_else(|| format!("{base_branch}{suffix}"));
        if !valid_branch(root, &br).await {
            return Err(Error::new(
                "invalid_params",
                format!("{br} is not a valid branch name"),
            ));
        }
        match claim(root, &wt, &br, commit).await? {
            Claim::Placed => {
                placed = Some((wt, br));
                break;
            }
            // A chosen path or branch has no alternative.
            Claim::PathTaken if worktree.is_some() => {
                return Err(Error::new(
                    "conflict",
                    format!("{} already exists", wt.display()),
                ));
            }
            Claim::BranchTaken if branch.is_some() => {
                return Err(Error::new(
                    "conflict",
                    format!("branch {br} already exists"),
                ));
            }
            Claim::PathTaken | Claim::BranchTaken => {}
        }
    }
    let Some((wt, br)) = placed else {
        return Err(Error::new(
            "conflict",
            format!("no free worktree and branch name for {base_branch}"),
        ));
    };

    match fill(work, m, &wt).await {
        Ok((cwd, not_written, installed)) => Ok(Imported {
            worktree: wt,
            branch: br,
            cwd,
            resumed: installed.is_some(),
            resume_args: installed,
            not_written,
        }),
        Err(e) => {
            rollback(root, &wt, &br).await;
            Err(e)
        }
    }
}

/// Import the unpacked bundle into the existing checkout `wt` in place (spec 17 §7): its tracked
/// files must be clean and its HEAD must be the bundle's commit or an ancestor of it. HEAD (and
/// the branch it is on) fast-forwards to the commit, then the uncommitted changes, the untracked
/// files and the transcript follow. A failure puts the checkout back at its old commit.
pub async fn import_in_place(work: &Path, m: &Manifest, wt: &Path) -> Result<Imported> {
    check(m)?;
    let old = git_line(wt, &["rev-parse", "HEAD"])
        .await
        .ok_or_else(|| Error::new("conflict", format!("{} has no commits", wt.display())))?;
    let dirty = git(wt, &["status", "--porcelain", "--untracked-files=no"]).await?;
    if !dirty.iter().all(|b| b.is_ascii_whitespace()) {
        return Err(Error::new(
            "conflict",
            format!(
                "{} has uncommitted changes; commit or sync them first",
                wt.display()
            ),
        ));
    }
    if m.bundle != "none" {
        let bundle = work.join("repo.bundle");
        let bs = bundle.to_str().unwrap_or_default();
        let heads = git(work, &["bundle", "list-heads", bs]).await?;
        if !String::from_utf8_lossy(&heads)
            .lines()
            .any(|l| l.starts_with(&m.head))
        {
            return Err(Error::new(
                "conflict",
                "bundle HEAD does not match the manifest",
            ));
        }
        git(wt, &["fetch", "--no-tags", bs, "HEAD"]).await?;
    }
    if !has_commit(wt, &m.head).await {
        return Err(Error::new(
            "conflict",
            "the handed-off commit is not available",
        ));
    }
    if old != m.head {
        if git(wt, &["merge-base", "--is-ancestor", &old, &m.head])
            .await
            .is_err()
        {
            return Err(Error::new(
                "conflict",
                format!(
                    "{} has commits the handed-off work does not; sync them first",
                    wt.display()
                ),
            ));
        }
        git(wt, &["merge", "--ff-only", "-q", &m.head]).await?;
    }
    let branch = git_line(wt, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .unwrap_or_else(|| "HEAD".into());
    match fill(work, m, wt).await {
        Ok((cwd, not_written, installed)) => Ok(Imported {
            worktree: wt.to_path_buf(),
            branch,
            cwd,
            resumed: installed.is_some(),
            resume_args: installed,
            not_written,
        }),
        Err(e) => {
            if old != m.head {
                let _ = git(wt, &["reset", "-q", "--hard", &old]).await;
            }
            Err(e)
        }
    }
}

type Filled = (PathBuf, Vec<NotWritten>, Option<Vec<String>>);

/// Changes, untracked files and the transcript in the new worktree.
async fn fill(work: &Path, m: &Manifest, wt: &Path) -> Result<Filled> {
    let patch = work.join("changes.patch");
    if std::fs::metadata(&patch).map(|m| m.len()).unwrap_or(0) > 0 {
        git(
            wt,
            &[
                "apply",
                "--binary",
                "--whitespace=nowarn",
                patch.to_str().unwrap_or_default(),
            ],
        )
        .await?;
    }
    let (work, m, wt) = (work.to_path_buf(), m.clone(), wt.to_path_buf());
    tokio::task::spawn_blocking(move || {
        let mut not_written = Vec::new();
        for rel in &m.untracked {
            if !safe_tree_path(rel) {
                not_written.push(NotWritten {
                    path: rel.clone(),
                    reason: "unsafe path".into(),
                });
                continue;
            }
            // The patch may have created symlinks: never write through one.
            if let Err(e) = write_new_file(&wt, rel, &work.join("untracked").join(rel)) {
                not_written.push(NotWritten {
                    path: rel.clone(),
                    reason: e.to_string(),
                });
            }
        }
        // The cwd must resolve inside the worktree (the patch could have made it a symlink).
        let cwd = match wt.join(&m.cwd_rel).canonicalize() {
            Ok(c)
                if !m.cwd_rel.is_empty()
                    && c.is_dir()
                    && wt.canonicalize().is_ok_and(|w| c.starts_with(&w)) =>
            {
                c
            }
            _ => wt.clone(),
        };
        let installed = match install_transcript(&m, &work, &cwd, &wt) {
            Ok(Some(i)) => {
                not_written.extend(i.not_written);
                Some(i.resume_args)
            }
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("handoff transcript: {e}");
                not_written.push(NotWritten {
                    path: m.transcript_rel.clone().unwrap_or_default(),
                    reason: e.to_string(),
                });
                None
            }
        };
        (cwd, not_written, installed)
    })
    .await
    .map_err(|e| Error::new("internal", e.to_string()))
}

enum Claim {
    Placed,
    /// The path or the branch exists; nothing was created.
    PathTaken,
    BranchTaken,
}

/// Create the worktree directory, the branch at `commit` and the worktree in it. Either all
/// three exist afterwards, or only what was there before.
async fn claim(root: &Path, wt: &Path, br: &str, commit: &str) -> Result<Claim> {
    if let Some(p) = wt.parent() {
        std::fs::create_dir_all(p)
            .map_err(|e| Error::new("conflict", format!("{}: {e}", p.display())))?;
    }
    match std::fs::create_dir(wt) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Ok(Claim::PathTaken);
        }
        Err(e) => return Err(Error::new("conflict", format!("{}: {e}", wt.display()))),
    }
    // `git branch` refuses an existing branch under the ref lock, so the branch is ours iff it
    // succeeded.
    if let Err(e) = git(root, &["branch", "--no-track", br, commit]).await {
        let _ = std::fs::remove_dir(wt);
        return if branch_exists(root, br).await {
            Ok(Claim::BranchTaken)
        } else {
            Err(e)
        };
    }
    let wts = wt.to_str().unwrap_or_default();
    if let Err(e) = git(root, &["worktree", "add", wts, br]).await {
        rollback(root, wt, br).await;
        return Err(e);
    }
    Ok(Claim::Placed)
}

/// Remove the worktree, its directory and the branch: all created by [`claim`] for this import.
async fn rollback(root: &Path, wt: &Path, br: &str) {
    let wts = wt.to_str().unwrap_or_default();
    let _ = git(root, &["worktree", "remove", "--force", wts]).await;
    if std::fs::symlink_metadata(wt).is_ok_and(|m| m.is_dir()) {
        let _ = std::fs::remove_dir_all(wt);
    }
    let _ = git(root, &["worktree", "prune"]).await;
    let _ = git(root, &["branch", "-D", br]).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} in {}", dir.display());
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repository with one commit and an unpacked bundle (`bundle: none`) for that commit.
    fn setup(t: &Path, patch: &str) -> (PathBuf, PathBuf, Manifest) {
        let repo = t.join("repo");
        std::fs::create_dir_all(repo.join("app")).unwrap();
        sh(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("app/a.txt"), "one\n").unwrap();
        sh(&repo, &["add", "-A"]);
        sh(&repo, &["commit", "-qm", "base"]);
        let head = sh(&repo, &["rev-parse", "HEAD"]);
        let work = t.join("work");
        std::fs::create_dir_all(work.join("untracked/app")).unwrap();
        std::fs::write(work.join("changes.patch"), patch).unwrap();
        std::fs::write(work.join("untracked/app/new.txt"), "new\n").unwrap();
        std::fs::write(work.join("untracked/app/a.txt"), "clobber\n").unwrap();
        let m = Manifest {
            v: 1,
            head,
            branch: Some("feature".into()),
            bundle: "none".into(),
            cwd_rel: "app".into(),
            untracked: vec!["app/new.txt".into(), "app/a.txt".into()],
            ..Default::default()
        };
        (repo.canonicalize().unwrap(), work, m)
    }

    const PATCH: &str = "diff --git a/app/a.txt b/app/a.txt\n--- a/app/a.txt\n+++ b/app/a.txt\n@@ -1 +1,2 @@\n one\n+two\n";

    #[tokio::test]
    async fn imports_into_a_new_worktree() {
        let t = tempfile::tempdir().unwrap();
        let (repo, work, m) = setup(t.path(), PATCH);
        let r = import(&work, &m, &repo, None, None).await.unwrap();
        let parent = repo.parent().unwrap();
        assert_eq!(r.worktree, parent.join("repo-handoff-feature"));
        assert_eq!(r.branch, "handoff/feature");
        assert_eq!(r.cwd, r.worktree.canonicalize().unwrap().join("app"));
        assert!(!r.resumed);
        assert_eq!(
            std::fs::read_to_string(r.worktree.join("app/a.txt")).unwrap(),
            "one\ntwo\n"
        );
        assert_eq!(
            std::fs::read_to_string(r.worktree.join("app/new.txt")).unwrap(),
            "new\n"
        );
        // A tracked file is never replaced by an untracked one; the failure is reported.
        assert_eq!(r.not_written.len(), 1);
        assert_eq!(r.not_written[0].path, "app/a.txt");

        // A second import of the same work lands next to the first.
        let again = import(&work, &m, &repo, None, None).await.unwrap();
        assert_eq!(again.worktree, parent.join("repo-handoff-feature-2"));
        assert_eq!(again.branch, "handoff/feature-2");
    }

    #[tokio::test]
    async fn failed_patch_leaves_nothing_behind() {
        let t = tempfile::tempdir().unwrap();
        let (repo, work, m) = setup(t.path(), "diff --git a/x b/x\ngarbage\n@@ nope\n");
        let e = import(&work, &m, &repo, None, None).await.unwrap_err();
        assert_eq!(e.kind, "conflict");
        assert!(!repo.parent().unwrap().join("repo-handoff-feature").exists());
        assert!(!branch_exists(&repo, "handoff/feature").await);
        let list = sh(&repo, &["worktree", "list", "--porcelain"]);
        assert_eq!(list.matches("worktree ").count(), 1, "{list}");
    }

    #[tokio::test]
    async fn invalid_manifest_rejected_before_anything() {
        let t = tempfile::tempdir().unwrap();
        let (repo, work, mut m) = setup(t.path(), "");
        m.cwd_rel = "../outside".into();
        assert_eq!(
            import(&work, &m, &repo, None, None).await.unwrap_err().kind,
            "invalid_params"
        );
        m.cwd_rel = "app".into();
        m.head = "abc".into();
        assert_eq!(
            import(&work, &m, &repo, None, None).await.unwrap_err().kind,
            "invalid_params"
        );
        assert!(!repo.parent().unwrap().join("repo-handoff-feature").exists());
        let mut other = m.clone();
        other.branch = Some("x".into());
        assert!(verify(&m, &other).is_err());
    }

    #[tokio::test]
    async fn bundles_never_write_git_metadata() {
        for bad in [
            ".git/hooks/post-checkout",
            ".GIT/config",
            "./.git/config",
            "app/../.git/config",
            "app/.git/config",
        ] {
            let t = tempfile::tempdir().unwrap();
            let (repo, work, mut m) = setup(t.path(), "");
            std::fs::create_dir_all(work.join("untracked/.git/hooks")).unwrap();
            std::fs::write(work.join("untracked/.git/hooks/post-checkout"), "pwned").unwrap();
            m.untracked.push(bad.into());
            let e = import(&work, &m, &repo, None, None).await.unwrap_err();
            assert_eq!(e.kind, "invalid_params", "{bad}");
            assert!(!repo.parent().unwrap().join("repo-handoff-feature").exists());
            let e = import_in_place(&work, &m, &repo).await.unwrap_err();
            assert_eq!(e.kind, "invalid_params", "{bad}");
            assert!(!repo.join(".git/hooks/post-checkout").exists(), "{bad}");
            assert!(
                !repo.join("app/new.txt").exists(),
                "nothing imported for {bad}"
            );
        }
    }

    #[tokio::test]
    async fn untracked_files_never_follow_a_symlink_into_git() {
        let t = tempfile::tempdir().unwrap();
        let (repo, work, mut m) = setup(t.path(), "");
        std::os::unix::fs::symlink(".git", repo.join("lnk")).unwrap();
        sh(&repo, &["add", "lnk"]);
        sh(&repo, &["commit", "-qm", "link"]);
        m.head = sh(&repo, &["rev-parse", "HEAD"]);
        std::fs::create_dir_all(work.join("untracked/lnk/hooks")).unwrap();
        std::fs::write(work.join("untracked/lnk/hooks/post-checkout"), "pwned").unwrap();
        m.untracked = vec!["lnk/hooks/post-checkout".into()];
        let r = import_in_place(&work, &m, &repo).await.unwrap();
        assert_eq!(r.not_written.len(), 1);
        assert!(!repo.join(".git/hooks/post-checkout").exists());
    }

    #[tokio::test]
    async fn chosen_worktree_and_branch() {
        let t = tempfile::tempdir().unwrap();
        let (repo, work, m) = setup(t.path(), "");
        let wt = t.path().canonicalize().unwrap().join("elsewhere/mine");
        let r = import(&work, &m, &repo, Some(wt.as_path()), Some("me/topic"))
            .await
            .unwrap();
        assert_eq!(r.worktree, wt);
        assert_eq!(r.branch, "me/topic");
        assert!(wt.join("app/a.txt").exists());
        // Taken path, taken branch, bad branch, relative path.
        let other = t.path().join("other");
        let kind = |r: Result<Imported>| r.unwrap_err().kind;
        assert_eq!(
            kind(import(&work, &m, &repo, Some(wt.as_path()), None).await),
            "conflict"
        );
        assert_eq!(
            kind(import(&work, &m, &repo, Some(other.as_path()), Some("me/topic")).await),
            "conflict"
        );
        assert_eq!(
            kind(import(&work, &m, &repo, Some(other.as_path()), Some("a..b")).await),
            "invalid_params"
        );
        assert_eq!(
            kind(import(&work, &m, &repo, Some(Path::new("rel/wt")), None).await),
            "invalid_params"
        );
        assert!(!other.exists());
    }

    #[tokio::test]
    async fn names_taken_by_someone_else_are_skipped_and_kept() {
        let t = tempfile::tempdir().unwrap();
        let (repo, work, m) = setup(t.path(), "");
        let parent = repo.parent().unwrap();
        // Another import got here first: its worktree directory (no branch yet) and, for the
        // next name, its branch (no directory yet).
        let theirs = parent.join("repo-handoff-feature");
        std::fs::create_dir(&theirs).unwrap();
        std::fs::write(theirs.join("keep.txt"), "theirs\n").unwrap();
        sh(&repo, &["branch", "handoff/feature-2", "HEAD"]);
        let r = import(&work, &m, &repo, None, None).await.unwrap();
        assert_eq!(r.worktree, parent.join("repo-handoff-feature-3"));
        assert_eq!(r.branch, "handoff/feature-3");
        assert_eq!(
            std::fs::read_to_string(theirs.join("keep.txt")).unwrap(),
            "theirs\n"
        );
        assert!(branch_exists(&repo, "handoff/feature-2").await);
        assert!(!parent.join("repo-handoff-feature-2").exists());
        // A chosen branch someone else holds is a conflict that leaves it and makes nothing.
        let mine = parent.join("mine");
        let e = import(&work, &m, &repo, Some(&mine), Some("handoff/feature-2"))
            .await
            .unwrap_err();
        assert_eq!(e.kind, "conflict");
        assert!(branch_exists(&repo, "handoff/feature-2").await);
        assert!(!mine.exists());
    }

    #[tokio::test]
    async fn concurrent_imports_get_their_own_names() {
        let t = tempfile::tempdir().unwrap();
        let (repo, work, m) = setup(t.path(), PATCH);
        let (a, b, c) = tokio::join!(
            import(&work, &m, &repo, None, None),
            import(&work, &m, &repo, None, None),
            import(&work, &m, &repo, None, None)
        );
        let mut got: Vec<String> = [a, b, c]
            .into_iter()
            .map(|r| {
                let r = r.unwrap();
                assert_eq!(
                    std::fs::read_to_string(r.worktree.join("app/a.txt")).unwrap(),
                    "one\ntwo\n"
                );
                r.branch
            })
            .collect();
        got.sort();
        assert_eq!(
            got,
            ["handoff/feature", "handoff/feature-2", "handoff/feature-3"]
        );
        let list = sh(&repo, &["worktree", "list", "--porcelain"]);
        assert_eq!(list.matches("worktree ").count(), 4, "{list}");
    }

    #[tokio::test]
    async fn collisions_give_up_after_99() {
        let t = tempfile::tempdir().unwrap();
        let (repo, work, m) = setup(t.path(), "");
        let parent = repo.parent().unwrap();
        std::fs::create_dir(parent.join("repo-handoff-feature")).unwrap();
        for n in 2..100 {
            std::fs::create_dir(parent.join(format!("repo-handoff-feature-{n}"))).unwrap();
        }
        let e = import(&work, &m, &repo, None, None).await.unwrap_err();
        assert_eq!(e.kind, "conflict");
        assert!(!branch_exists(&repo, "handoff/feature-99").await);
    }
}
