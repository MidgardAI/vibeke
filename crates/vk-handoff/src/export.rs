//! The export: an agent's work in a repository as a bundle (spec 16 §15.2, spec 17 §7). The
//! caller gathers the facts (which checkout, which agent session) and waits for the turn
//! boundary; this module reads the repository and packs.
//!
//! Every git command runs against the commit pinned at the start ([`pin`]), never a later `HEAD`.
//! The working-tree changes and untracked files are live by nature, so after packing HEAD and the
//! branch are read again and the export fails with `repo_moved` if they changed meanwhile.

use std::path::{Path, PathBuf};

use crate::{
    Error, MAX_BUNDLE, MAX_UNTRACKED, Manifest, Result, Skipped, git, git_line, hash_file,
    regular_under, safe_relative, secret_path,
};

/// The repository an export reads, read once at the start: its root, the commit and branch, and
/// HEAD's fingerprint (see [`head_fingerprint`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub root: PathBuf,
    pub head: String,
    /// `None` on a detached HEAD.
    pub branch: Option<String>,
    fingerprint: Fingerprint,
}

type Fingerprint = (Option<u64>, Option<std::time::SystemTime>);

/// The facts an export needs. Nothing here is read from a server: the gateway takes them from
/// its pane, `vibeke sandbox export-bundle` from its flags.
#[derive(Debug, Clone, Default)]
pub struct ExportInput {
    /// The agent's working directory.
    pub cwd: PathBuf,
    /// The repository read before (to check it against an approval); `None` pins it now.
    pub pin: Option<Pin>,
    pub harness: Option<String>,
    pub session_id: Option<String>,
    /// The harness transcript to carry (redacted), if any.
    pub transcript: Option<PathBuf>,
    /// `resume_argv` without the program name.
    pub resume_args: Vec<String>,
    pub last_message: Option<String>,
    pub source_host: String,
    pub source_job: Option<String>,
    /// Bundle the whole history, even what the remotes have.
    pub full: bool,
}

/// A packed bundle.
#[derive(Debug, Clone)]
pub struct Packed {
    pub manifest: Manifest,
    pub size: u64,
    pub sha256: String,
}

/// Read the repository `cwd` is in: `not_found` outside a repository, `conflict` without commits.
pub async fn pin(cwd: &Path) -> Result<Pin> {
    let root = git_line(cwd, &["rev-parse", "--show-toplevel"])
        .await
        .map(PathBuf::from)
        .ok_or_else(|| Error::new("not_found", "not_a_repo"))?;
    let head = git_line(&root, &["rev-parse", "HEAD"])
        .await
        .ok_or_else(|| Error::new("conflict", "repository has no commits"))?;
    let branch = git_line(&root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .filter(|b| b != "HEAD");
    // Every checkout appends to HEAD's reflog, even one that later returns to `head`.
    let fingerprint = head_fingerprint(&root).await;
    Ok(Pin {
        root,
        head,
        branch,
        fingerprint,
    })
}

fn now_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Export the work described by `input` into the new file `out` (its directory also holds the
/// temporary work directory). On failure `out` does not exist.
pub async fn export(input: &ExportInput, out: &Path) -> Result<Packed> {
    if std::fs::symlink_metadata(out).is_ok() {
        return Err(Error::new(
            "conflict",
            format!("{} already exists", out.display()),
        ));
    }
    let pinned = match &input.pin {
        Some(p) => p.clone(),
        None => pin(&input.cwd).await?,
    };
    let Pin {
        root, head, branch, ..
    } = pinned.clone();
    let origin = git_line(&root, &["remote", "get-url", "origin"]).await;
    let has_remotes = git_line(&root, &["remote"]).await.is_some();

    let parent = out.parent().unwrap_or(Path::new("."));
    let work = tempfile::Builder::new()
        .prefix("handoff-")
        .tempdir_in(parent)
        .map_err(|e| Error::new("internal", e.to_string()))?;
    let w = work.path().to_path_buf();

    // Repository objects, then uncommitted tracked changes, both pinned to `head`.
    let bundle_path = w.join("repo.bundle");
    let bundle_kind = repo_objects(&root, &head, has_remotes && !input.full, &bundle_path).await?;
    if (std::fs::metadata(&bundle_path)
        .map(|m| m.len())
        .unwrap_or(0))
        > MAX_BUNDLE
    {
        return Err(Error::new("too_large", "repository bundle exceeds 200 MiB"));
    }
    let patch = tracked_changes(&root, &head).await?;
    std::fs::write(w.join("changes.patch"), &patch)
        .map_err(|e| Error::new("internal", e.to_string()))?;

    // Untracked files.
    let listed = git(&root, &["ls-files", "--others", "--exclude-standard", "-z"]).await?;
    let mut untracked = Vec::new();
    let mut skipped = Vec::new();
    for raw in listed.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let rel = String::from_utf8_lossy(raw).to_string();
        let reason = if !safe_relative(&rel) {
            Some("unsafe path")
        } else if !crate::safe_tree_path(&rel) {
            Some("git metadata")
        } else if secret_path(&rel) {
            Some("secret")
        } else {
            match regular_under(&root, &rel) {
                None => Some("not a regular file"),
                Some(md) if md.len() > MAX_UNTRACKED => Some("larger than 5 MiB"),
                Some(_) => None,
            }
        };
        match reason {
            Some(r) => skipped.push(Skipped {
                path: rel,
                reason: r.into(),
            }),
            None => untracked.push(rel),
        }
    }

    // Transcript (and Claude's sidechain files), redacted line by line.
    let mut redactions = 0;
    let mut transcript_rel = None;
    if let Some(tp) = &input.transcript {
        let (h, tp, w2) = (
            input.harness.clone().unwrap_or_default(),
            tp.clone(),
            w.clone(),
        );
        if let Some((rel, n)) = blocking(move || crate::export_transcript(&h, &tp, &w2)).await? {
            transcript_rel = Some(rel);
            redactions = n;
        }
    }

    let cwd_rel = input
        .cwd
        .strip_prefix(&root)
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let manifest = Manifest {
        v: 1,
        source_host: input.source_host.clone(),
        repo_name: root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".into()),
        origin,
        branch,
        head,
        bundle: bundle_kind.into(),
        cwd_rel,
        source_cwd: input.cwd.display().to_string(),
        source_root: root.display().to_string(),
        harness: input.harness.clone(),
        session_id: input.session_id.clone(),
        resume_args: input.resume_args.clone(),
        transcript_rel,
        last_message: input
            .last_message
            .as_ref()
            .map(|m| m.chars().take(500).collect()),
        untracked: untracked.clone(),
        skipped,
        redactions,
        created_at: now_s(),
        source_job: input.source_job.clone(),
    };

    // Pack.
    let (root2, w2, m2, out2) = (root.clone(), w.clone(), manifest.clone(), out.to_path_buf());
    let bundle_present = bundle_kind != "none";
    let (size, sha256) = blocking(move || {
        crate::pack(&out2, &w2, &root2, &m2, bundle_present)?;
        hash_file(&out2)
    })
    .await
    .inspect_err(|_| {
        let _ = std::fs::remove_file(out);
    })?;
    if size > MAX_BUNDLE {
        let _ = std::fs::remove_file(out);
        return Err(Error::new("too_large", "handoff bundle exceeds 200 MiB"));
    }
    // The working-tree diff and untracked files were read live: HEAD and the branch must not
    // have moved while they were.
    // A checkout to elsewhere and back leaves HEAD as it was, but not its reflog.
    let moved = match still_at(&root, manifest.branch.as_deref(), &manifest.head).await {
        Err(e) => Some(e),
        Ok(()) if head_fingerprint(&root).await != pinned.fingerprint => Some(Error::new(
            "conflict",
            "repo_moved: the repository was checked out during the export; nothing was sent, try again",
        )),
        Ok(()) => None,
    };
    if let Some(e) = moved {
        let _ = std::fs::remove_file(out);
        return Err(e);
    }
    drop(work);
    Ok(Packed {
        manifest,
        size,
        sha256,
    })
}

/// Disk work off the async threads.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::new("internal", e.to_string()))?
        .map_err(|e| Error::new("internal", e.to_string()))
}

fn at(root: &str, branch: Option<&str>, head: &str) -> String {
    let short: String = head.chars().take(12).collect();
    format!(
        "{root} on {} at {}",
        branch.unwrap_or("a detached HEAD"),
        if short.is_empty() {
            "no commit"
        } else {
            short.as_str()
        }
    )
}

fn moved_during_export(root: &Path, was: (Option<&str>, &str), now: (Option<&str>, &str)) -> Error {
    let r = root.display().to_string();
    Error::new(
        "conflict",
        format!(
            "repo_moved: the repository's HEAD changed during the export (was: {}; now: {}); nothing was sent, try again",
            at(&r, was.0, was.1),
            at(&r, now.0, now.1)
        ),
    )
}

/// The repository's objects for `head` in `bundle_path`: `thin` (only what the remotes lack),
/// `none` (the remotes have it all; no file) or `full`. `git bundle` can only bundle the named
/// `HEAD`, so the bundle's recorded HEAD is verified to be `head`: a HEAD that moved since it
/// was read fails the export (`repo_moved`) instead of bundling another commit.
pub(crate) async fn repo_objects(
    root: &Path,
    head: &str,
    thin: bool,
    bundle_path: &Path,
) -> Result<&'static str> {
    let was = |now: &str| {
        let short = |h: &str| h.chars().take(12).collect::<String>();
        Error::new(
            "conflict",
            format!(
                "repo_moved: {}'s HEAD moved from {} to {} during the export; nothing was sent, try again",
                root.display(),
                short(head),
                if now.is_empty() {
                    "nothing".to_string()
                } else {
                    short(now)
                }
            ),
        )
    };
    let bp = bundle_path.to_str().unwrap_or_default();
    let kind = if thin {
        // Decided for the pinned commit, not for whatever HEAD is when the bundle is made.
        let unpushed = git(
            root,
            &[
                "rev-list",
                "--max-count=1",
                head,
                "--not",
                "--remotes",
                "--",
            ],
        )
        .await?;
        if unpushed.iter().all(|b| b.is_ascii_whitespace()) {
            return Ok("none");
        }
        match git(
            root,
            &["bundle", "create", bp, "HEAD", "--not", "--remotes"],
        )
        .await
        {
            Ok(_) => "thin",
            // HEAD moved onto pushed history since it was read.
            Err(e) if e.message.contains("empty bundle") => {
                let now = git_line(root, &["rev-parse", "HEAD"])
                    .await
                    .unwrap_or_default();
                return Err(was(&now));
            }
            Err(e) => return Err(e),
        }
    } else {
        git(root, &["bundle", "create", bp, "HEAD"]).await?;
        "full"
    };
    let heads = git(root, &["bundle", "list-heads", bp]).await?;
    let heads = String::from_utf8_lossy(&heads);
    let bundled = heads
        .lines()
        .filter_map(|l| l.split_once(' '))
        .find(|(_, r)| *r == "HEAD")
        .map(|(sha, _)| sha.to_string())
        .unwrap_or_default();
    if bundled != head {
        let _ = std::fs::remove_file(bundle_path);
        return Err(was(&bundled));
    }
    Ok(kind)
}

/// Uncommitted tracked changes against `head` (not `HEAD`).
pub(crate) async fn tracked_changes(root: &Path, head: &str) -> Result<Vec<u8>> {
    git(
        root,
        &[
            "diff",
            "--binary",
            "--no-ext-diff",
            "--no-textconv",
            head,
            "--",
        ],
    )
    .await
}

/// What a checkout always changes, even one that returns to the same commit: the length of
/// HEAD's reflog (absent with `core.logAllRefUpdates=false`) and the HEAD file's modification
/// time (a checkout rewrites it).
pub(crate) async fn head_fingerprint(root: &Path) -> Fingerprint {
    let meta = |name: &'static str| async move {
        let rel = git_line(root, &["rev-parse", "--git-path", name]).await?;
        std::fs::metadata(root.join(rel)).ok()
    };
    (
        meta("logs/HEAD").await.map(|m| m.len()),
        meta("HEAD").await.and_then(|m| m.modified().ok()),
    )
}

/// `repo_moved` unless the repository is still on `branch` at `head`.
pub(crate) async fn still_at(root: &Path, branch: Option<&str>, head: &str) -> Result<()> {
    let now = git_line(root, &["rev-parse", "HEAD"])
        .await
        .unwrap_or_default();
    let now_branch = git_line(root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .filter(|b| b != "HEAD");
    if now == head && now_branch.as_deref() == branch {
        return Ok(());
    }
    Err(moved_during_export(
        root,
        (branch, head),
        (now_branch.as_deref(), &now),
    ))
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

    /// A repository with `main` at commit A and `feature` at B (adds `b.txt`), on `feature`.
    fn setup(t: &Path) -> (PathBuf, String, String) {
        let repo = t.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        sh(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        sh(&repo, &["add", "-A"]);
        sh(&repo, &["commit", "-qm", "a"]);
        let a = sh(&repo, &["rev-parse", "HEAD"]);
        sh(&repo, &["checkout", "-qb", "feature"]);
        std::fs::write(repo.join("b.txt"), "two\n").unwrap();
        sh(&repo, &["add", "-A"]);
        sh(&repo, &["commit", "-qm", "b"]);
        let b = sh(&repo, &["rev-parse", "HEAD"]);
        (repo.canonicalize().unwrap(), a, b)
    }

    #[tokio::test]
    async fn a_pinned_export_uses_the_given_commit() {
        let t = tempfile::tempdir().unwrap();
        let (repo, a, b) = setup(t.path());
        let bp = t.path().join("repo.bundle");

        // HEAD is B: a bundle pinned to B carries B.
        assert_eq!(repo_objects(&repo, &b, false, &bp).await.unwrap(), "full");
        let heads = sh(&repo, &["bundle", "list-heads", bp.to_str().unwrap()]);
        assert!(heads.lines().any(|l| l == format!("{b} HEAD")), "{heads}");

        // HEAD moved off the pinned commit: the export fails rather than bundle B for A.
        std::fs::remove_file(&bp).unwrap();
        let e = repo_objects(&repo, &a, false, &bp).await.unwrap_err();
        assert_eq!(e.kind, "conflict");
        assert!(e.message.starts_with("repo_moved"), "{}", e.message);
        assert!(!bp.exists());

        // The working-tree diff is taken against the given commit, not HEAD.
        assert!(tracked_changes(&repo, &b).await.unwrap().is_empty());
        let vs_a = String::from_utf8(tracked_changes(&repo, &a).await.unwrap()).unwrap();
        assert!(vs_a.contains("b.txt"), "{vs_a}");
    }

    #[tokio::test]
    async fn a_thin_export_is_decided_for_the_pinned_commit() {
        let t = tempfile::tempdir().unwrap();
        let (repo, a, b) = setup(t.path());
        sh(&repo, &["update-ref", "refs/remotes/origin/main", &a]);
        let bp = t.path().join("repo.bundle");
        // A is pushed: nothing to bundle, whatever HEAD is.
        assert_eq!(repo_objects(&repo, &a, true, &bp).await.unwrap(), "none");
        assert!(!bp.exists());
        // B is not: a thin bundle with B as HEAD.
        assert_eq!(repo_objects(&repo, &b, true, &bp).await.unwrap(), "thin");
        // B pinned but HEAD moved to the pushed A: refused, not "none".
        std::fs::remove_file(&bp).unwrap();
        sh(&repo, &["checkout", "-q", "main"]);
        let e = repo_objects(&repo, &b, true, &bp).await.unwrap_err();
        assert!(e.message.starts_with("repo_moved"), "{}", e.message);
    }

    #[tokio::test]
    async fn the_recheck_after_export_catches_a_branch_switch() {
        let t = tempfile::tempdir().unwrap();
        let (repo, a, b) = setup(t.path());
        assert!(still_at(&repo, Some("feature"), &b).await.is_ok());
        sh(&repo, &["checkout", "-q", "main"]);
        let e = still_at(&repo, Some("feature"), &b).await.unwrap_err();
        assert_eq!(e.kind, "conflict");
        assert!(e.message.starts_with("repo_moved"), "{}", e.message);
        // Same commit, other branch name: still moved.
        sh(&repo, &["checkout", "-qb", "other", &a]);
        assert!(still_at(&repo, Some("main"), &a).await.is_err());
        // Detached at the same commit: moved too.
        sh(&repo, &["checkout", "-q", "--detach", &a]);
        assert!(still_at(&repo, Some("other"), &a).await.is_err());
        assert!(still_at(&repo, None, &a).await.is_ok());
    }

    #[tokio::test]
    async fn a_checkout_elsewhere_and_back_changes_the_head_reflog() {
        let t = tempfile::tempdir().unwrap();
        let (repo, _a, b) = setup(t.path());
        let before = head_fingerprint(&repo).await;
        assert!(before.0.is_some() && before.1.is_some());
        assert_eq!(head_fingerprint(&repo).await, before);
        sh(&repo, &["checkout", "-q", "main"]);
        sh(&repo, &["checkout", "-q", "feature"]);
        assert!(still_at(&repo, Some("feature"), &b).await.is_ok());
        assert_ne!(head_fingerprint(&repo).await, before);
        // Without a reflog the HEAD file's rewrite still shows.
        sh(&repo, &["config", "core.logAllRefUpdates", "false"]);
        std::fs::remove_file(repo.join(".git/logs/HEAD")).unwrap();
        let before = head_fingerprint(&repo).await;
        assert!(before.0.is_none());
        std::thread::sleep(std::time::Duration::from_millis(20));
        sh(&repo, &["checkout", "-q", "main"]);
        sh(&repo, &["checkout", "-q", "feature"]);
        assert_ne!(head_fingerprint(&repo).await, before);
    }

    #[tokio::test]
    async fn pin_reads_root_head_and_branch() {
        let t = tempfile::tempdir().unwrap();
        let (repo, _a, b) = setup(t.path());
        let p = pin(&repo).await.unwrap();
        assert_eq!(p.root, repo);
        assert_eq!(p.head, b);
        assert_eq!(p.branch.as_deref(), Some("feature"));
        let outside = t.path().join("plain");
        std::fs::create_dir_all(&outside).unwrap();
        assert_eq!(pin(&outside).await.unwrap_err().kind, "not_found");
    }

    /// Committed, uncommitted and untracked work survives export → unpack → import into a
    /// clone of the repository.
    #[tokio::test]
    async fn export_then_import_round_trip() {
        let t = tempfile::tempdir().unwrap();
        let (repo, _a, b) = setup(t.path());
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("a.txt"), "one\nchanged\n").unwrap();
        std::fs::write(repo.join("src/new.txt"), "untracked\n").unwrap();
        std::fs::write(repo.join(".env"), "SECRET=1\n").unwrap();
        let out = t.path().join("out.tar.zst");
        let input = ExportInput {
            cwd: repo.join("src"),
            source_host: "host-a".into(),
            harness: Some("claude".into()),
            session_id: Some("s1".into()),
            ..Default::default()
        };
        let packed = export(&input, &out).await.unwrap();
        let m = &packed.manifest;
        assert_eq!(m.head, b);
        assert_eq!(m.branch.as_deref(), Some("feature"));
        // No remotes: the whole history travels.
        assert_eq!(m.bundle, "full");
        assert_eq!(m.cwd_rel, "src");
        assert_eq!(m.source_host, "host-a");
        assert_eq!(m.untracked, vec!["src/new.txt".to_string()]);
        assert!(m.skipped.iter().any(|s| s.path == ".env"));
        assert_eq!(packed.size, std::fs::metadata(&out).unwrap().len());
        assert_eq!(packed.sha256, hash_file(&out).unwrap().1);

        // Elsewhere: a repository that has only the first commit.
        let other = t.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        sh(&other, &["init", "-q", "-b", "main"]);
        std::fs::write(other.join("a.txt"), "one\n").unwrap();
        sh(&other, &["add", "-A"]);
        sh(&other, &["commit", "-qm", "a"]);
        let other = other.canonicalize().unwrap();
        let work = t.path().join("unpacked");
        std::fs::create_dir_all(&work).unwrap();
        let got = crate::unpack(&out, &work).unwrap();
        crate::verify(&got, m).unwrap();
        let imp = crate::import(&work, &got, &other, None, None)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(imp.worktree.join("a.txt")).unwrap(),
            "one\nchanged\n"
        );
        assert_eq!(
            std::fs::read_to_string(imp.worktree.join("b.txt")).unwrap(),
            "two\n"
        );
        assert_eq!(
            std::fs::read_to_string(imp.worktree.join("src/new.txt")).unwrap(),
            "untracked\n"
        );
        assert!(!imp.worktree.join(".env").exists());
        assert_eq!(imp.cwd, imp.worktree.canonicalize().unwrap().join("src"));
    }

    /// The same work imported in place into a clean checkout at the exported commit's ancestor.
    #[tokio::test]
    async fn export_then_import_in_place() {
        let t = tempfile::tempdir().unwrap();
        let (repo, a, b) = setup(t.path());
        // A clone at A, on the same branch name.
        let clone = t.path().join("clone");
        sh(
            t.path(),
            &[
                "clone",
                "-q",
                repo.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        sh(&clone, &["checkout", "-q", "-B", "feature", &a]);
        sh(&clone, &["remote", "remove", "origin"]);
        let clone = clone.canonicalize().unwrap();

        std::fs::write(repo.join("a.txt"), "one\nmore\n").unwrap();
        std::fs::write(repo.join("notes.md"), "n\n").unwrap();
        let out = t.path().join("out.tar.zst");
        let packed = export(
            &ExportInput {
                cwd: repo.clone(),
                ..Default::default()
            },
            &out,
        )
        .await
        .unwrap();
        let work = t.path().join("unpacked");
        std::fs::create_dir_all(&work).unwrap();
        let m = crate::unpack(&out, &work).unwrap();
        assert_eq!(m.head, packed.manifest.head);
        let imp = crate::import_in_place(&work, &m, &clone).await.unwrap();
        assert_eq!(imp.worktree, clone);
        assert_eq!(imp.branch, "feature");
        assert_eq!(sh(&clone, &["rev-parse", "HEAD"]), b);
        assert_eq!(
            std::fs::read_to_string(clone.join("a.txt")).unwrap(),
            "one\nmore\n"
        );
        assert_eq!(
            std::fs::read_to_string(clone.join("notes.md")).unwrap(),
            "n\n"
        );

        // Now dirty: a second in-place import is refused and changes nothing.
        let e = crate::import_in_place(&work, &m, &clone).await.unwrap_err();
        assert_eq!(e.kind, "conflict");
        assert_eq!(
            std::fs::read_to_string(clone.join("a.txt")).unwrap(),
            "one\nmore\n"
        );
    }
}
