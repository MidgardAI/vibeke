//! Handoff export and the incoming-handoff passthrough (spec 16 §15.2). The source side exports
//! an agent's work at a turn boundary as a bundle ([`export_bundle`]); `handoff_send.rs` carries
//! it to the peer host, whose `handoff_peer.rs` delivers it to its server as an incoming handoff,
//! which the receiver accepts (or the server imports automatically) as a new worktree with the
//! agent resumed there.
//!
//! Bundle format, unpacking and the repository-side import live in `vk-handoff`; the import and
//! the incoming records live in the server (`handoff.incoming.*`, `handoff.accept`). This module
//! does the export and forwards the receiver's own methods to the server.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use vk_handoff::{
    MAX_BUNDLE, MAX_UNTRACKED, Manifest, Skipped, git, git_line, hash_file, regular_under,
    safe_relative, secret_path,
};

pub use vk_handoff::claude_project_dir;

use crate::Gateway;
use crate::api::{ApiError, ApiResult, normalize};
use crate::state::{Device, now_s};

impl From<vk_handoff::Error> for ApiError {
    fn from(e: vk_handoff::Error) -> Self {
        ApiError::new(e.kind, e.message)
    }
}

/// Remove everything in the handoffs directory. Uploads live in memory, so at startup all that
/// is there is left over from before a restart.
pub fn sweep(gw: &Gateway) {
    let Ok(d) = dir(gw) else { return };
    for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
        let p = e.path();
        let r = match e.file_type() {
            Ok(t) if t.is_dir() => std::fs::remove_dir_all(&p),
            _ => std::fs::remove_file(&p),
        };
        if let Err(x) = r {
            tracing::warn!("handoff sweep: {}: {x}", p.display());
        }
    }
}

/// Side effects re-check that the device is still authorized (spec 16 §4.6).
fn still_authorized(gw: &Gateway, dev: &Device) -> Result<(), ApiError> {
    let _ = gw.reload_devices();
    match gw.device(&dev.id) {
        Some(d) if !d.expired() => Ok(()),
        _ => Err(err("forbidden", "this device is no longer authorized")),
    }
}

fn err(kind: &str, m: impl Into<String>) -> ApiError {
    ApiError::new(kind, m)
}

fn s<'a>(p: &'a Value, k: &str) -> Option<&'a str> {
    p.get(k).and_then(|v| v.as_str())
}

/// Disk work off the async threads.
pub(crate) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| err("internal", e.to_string()))?
        .map_err(|e| err("internal", e.to_string()))
}

pub async fn dispatch(gw: &Arc<Gateway>, dev: &Device, method: &str, p: &Value) -> ApiResult {
    match method {
        "handoff.incoming.list"
        | "handoff.incoming.get"
        | "handoff.accept"
        | "handoff.decline"
        | "handoff.resume"
        | "handoff.prefs" => incoming(gw, dev, method, p).await,
        _ => Err(err("method_not_found", method)),
    }
}

pub(crate) fn dir(gw: &Gateway) -> Result<PathBuf, ApiError> {
    let d = gw.state.dir.join("handoffs");
    std::fs::create_dir_all(&d).map_err(|e| err("internal", e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700));
    }
    Ok(d)
}

// ---------------------------------------------------------------------------------------------
// export

/// A packed bundle in the handoffs directory (`<id>.out.tar.zst`); the caller owns the file.
#[derive(Debug, Clone)]
pub struct Exported {
    pub id: String,
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
    pub manifest: Manifest,
}

/// Export the work in `pane` at a turn boundary. `actor` names who asks (server audit);
/// `auth`, when a device asks, is re-authorized before its agent is interrupted. Without a
/// device (a server job, `handoff.send`) the server already authorized the request. A job's id
/// goes into the manifest as `source_job`.
///
/// Every git command runs against the commit read at the start (or, with `expect`, the commit
/// the user approved, which must be the one checked out), never a later `HEAD`; the working-tree
/// changes and untracked files are live by nature, so after packing HEAD and the branch are read
/// again and the export fails with `repo_moved` if they changed meanwhile.
#[allow(clippy::too_many_arguments)]
pub async fn export_bundle(
    gw: &Arc<Gateway>,
    actor: &str,
    auth: Option<&Device>,
    pane: &str,
    interrupt: bool,
    full: bool,
    source_job: Option<&str>,
    expect: Option<&Expected>,
) -> Result<Exported, ApiError> {
    let info = gw.server.call("pane.get", json!({"pane": pane})).await?;
    let cwd = s(&info, "cwd")
        .map(PathBuf::from)
        .ok_or_else(|| err("not_found", "pane has no working directory"))?;
    let mut run = info.get("run").cloned().unwrap_or(Value::Null);
    normalize(&mut run);

    // Turn boundary: the agent must be idle.
    if run.is_object() {
        let state = run
            .pointer("/execution/value")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        if matches!(state, "working" | "starting") {
            if !interrupt {
                return Err(err(
                    "busy",
                    "the agent is working; wait for it to finish or pass interrupt: true",
                ));
            }
            let id = s(&run, "id").unwrap_or("").to_string();
            if let Some(dev) = auth {
                still_authorized(gw, dev)?;
            }
            gw.server
                .call_as(actor, "agent.interrupt", json!({"target": id}))
                .await?;
            let r = gw.server.call("agent.wait", json!({"target": id, "until": ["idle", "exited", "error"], "timeout_ms": 30_000})).await;
            if r.is_err() {
                return Err(err("timeout", "the agent did not stop within 30 s"));
            }
        }
    }

    let root = git_line(&cwd, &["rev-parse", "--show-toplevel"])
        .await
        .map(PathBuf::from)
        .ok_or_else(|| err("not_found", "not_a_repo"))?;
    let head = git_line(&root, &["rev-parse", "HEAD"])
        .await
        .ok_or_else(|| err("conflict", "repository has no commits"))?;
    let branch = git_line(&root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .filter(|b| b != "HEAD");
    // An approved send: the repository must still be where the user approved it.
    if let Some(e) = expect {
        e.check(&root.display().to_string(), branch.as_deref(), &head)?;
    }
    // Every checkout appends to HEAD's reflog, even one that later returns to `head`.
    let head_log = head_log_len(&root).await;
    let origin = git_line(&root, &["remote", "get-url", "origin"]).await;
    let has_remotes = git_line(&root, &["remote"]).await.is_some();

    let id = ulid::Ulid::new().to_string().to_lowercase();
    let work = tempfile::Builder::new()
        .prefix("handoff-")
        .tempdir_in(dir(gw)?)
        .map_err(|e| err("internal", e.to_string()))?;
    let w = work.path().to_path_buf();

    // Repository objects, then uncommitted tracked changes, both pinned to `head`.
    let bundle_path = w.join("repo.bundle");
    let bundle_kind = repo_objects(&root, &head, has_remotes && !full, &bundle_path).await?;
    if (std::fs::metadata(&bundle_path)
        .map(|m| m.len())
        .unwrap_or(0))
        > MAX_BUNDLE
    {
        return Err(err("too_large", "repository bundle exceeds 200 MiB"));
    }
    let patch = tracked_changes(&root, &head).await?;
    std::fs::write(w.join("changes.patch"), &patch).map_err(|e| err("internal", e.to_string()))?;

    // Untracked files.
    let listed = git(&root, &["ls-files", "--others", "--exclude-standard", "-z"]).await?;
    let mut untracked = Vec::new();
    let mut skipped = Vec::new();
    for raw in listed.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let rel = String::from_utf8_lossy(raw).to_string();
        let reason = if !safe_relative(&rel) {
            Some("unsafe path")
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
    let (harness, session_id) = (
        s(&run, "harness").map(str::to_string),
        s(&run, "harness_session_id").map(str::to_string),
    );
    let resume_args: Vec<String> = run
        .get("resume_argv")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .skip(1)
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let mut redactions = 0;
    let mut transcript_rel = None;
    if let Some(tp) = s(&run, "transcript_path") {
        let (h, tp, w2) = (
            harness.clone().unwrap_or_default(),
            PathBuf::from(tp),
            w.clone(),
        );
        if let Some((rel, n)) =
            blocking(move || vk_handoff::export_transcript(&h, &tp, &w2)).await?
        {
            transcript_rel = Some(rel);
            redactions = n;
        }
    }

    let cwd_rel = cwd
        .strip_prefix(&root)
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let manifest = Manifest {
        v: 1,
        source_host: gw.host_name.clone(),
        repo_name: root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".into()),
        origin,
        branch,
        head,
        bundle: bundle_kind.into(),
        cwd_rel,
        source_cwd: cwd.display().to_string(),
        source_root: root.display().to_string(),
        harness,
        session_id,
        resume_args,
        transcript_rel,
        last_message: s(&run, "last_message").map(|m| m.chars().take(500).collect()),
        untracked: untracked.clone(),
        skipped,
        redactions,
        created_at: now_s(),
        source_job: source_job.map(str::to_string),
    };

    // Pack.
    let out_path = dir(gw)?.join(format!("{id}.out.tar.zst"));
    let (root2, w2, m2, out2) = (root.clone(), w.clone(), manifest.clone(), out_path.clone());
    let bundle_present = bundle_kind != "none";
    let (size, sha) = blocking(move || {
        vk_handoff::pack(&out2, &w2, &root2, &m2, bundle_present)?;
        hash_file(&out2)
    })
    .await?;
    if size > MAX_BUNDLE {
        let _ = std::fs::remove_file(&out_path);
        return Err(err("too_large", "handoff bundle exceeds 200 MiB"));
    }
    // The working-tree diff and untracked files were read live: HEAD and the branch must not
    // have moved while they were.
    // A checkout to elsewhere and back leaves HEAD as it was, but not its reflog.
    let moved = match still_at(&root, manifest.branch.as_deref(), &manifest.head).await {
        Err(e) => Some(e),
        Ok(()) if head_log_len(&root).await != head_log => Some(err(
            "conflict",
            "repo_moved: the repository was checked out during the export; nothing was sent, try again",
        )),
        Ok(()) => None,
    };
    if let Some(e) = moved {
        let _ = std::fs::remove_file(&out_path);
        return Err(e);
    }
    Ok(Exported {
        id,
        path: out_path,
        size,
        sha256: sha,
        manifest,
    })
}

/// What the user approved for a send from a pane (`auth.approve`), recorded at request time
/// (the job's `expect`): the export must come from this repository, branch and commit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expected {
    pub repo_root: String,
    pub branch: Option<String>,
    pub head: String,
}

impl Expected {
    /// From a job's `expect` object; `None` when the job has none.
    pub fn from_json(v: &Value) -> Option<Self> {
        if !v.is_object() {
            return None;
        }
        Some(Self {
            repo_root: s(v, "repo_root").unwrap_or_default().to_string(),
            branch: s(v, "branch").map(str::to_string),
            head: s(v, "head").unwrap_or_default().to_string(),
        })
    }

    /// `repo_moved` unless the repository is at `root` on `branch` at `head`.
    pub fn check(&self, root: &str, branch: Option<&str>, head: &str) -> Result<(), ApiError> {
        if self.repo_root == root && self.branch.as_deref() == branch && self.head == head {
            return Ok(());
        }
        Err(err(
            "conflict",
            format!(
                "repo_moved: the pane's repository changed since the handoff was approved (approved: {}; now: {}); ask again",
                at(&self.repo_root, self.branch.as_deref(), &self.head),
                at(root, branch, head)
            ),
        ))
    }
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

fn moved_during_export(
    root: &Path,
    was: (Option<&str>, &str),
    now: (Option<&str>, &str),
) -> ApiError {
    let r = root.display().to_string();
    err(
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
async fn repo_objects(
    root: &Path,
    head: &str,
    thin: bool,
    bundle_path: &Path,
) -> Result<&'static str, ApiError> {
    let was = |now: &str| {
        let short = |h: &str| h.chars().take(12).collect::<String>();
        err(
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
            Err(e) => return Err(e.into()),
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
async fn tracked_changes(root: &Path, head: &str) -> Result<Vec<u8>, ApiError> {
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
    .map_err(ApiError::from)
}

/// The length of HEAD's reflog (`None` without one, e.g. `core.logAllRefUpdates=false`).
async fn head_log_len(root: &Path) -> Option<u64> {
    let rel = git_line(root, &["rev-parse", "--git-path", "logs/HEAD"]).await?;
    std::fs::metadata(root.join(rel)).ok().map(|m| m.len())
}

/// `repo_moved` unless the repository is still on `branch` at `head`.
async fn still_at(root: &Path, branch: Option<&str>, head: &str) -> Result<(), ApiError> {
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

// ---------------------------------------------------------------------------------------------
// incoming handoffs on this host (full-scope devices)

/// The server's incoming-handoff methods, with only the params they take.
async fn incoming(gw: &Arc<Gateway>, dev: &Device, method: &str, p: &Value) -> ApiResult {
    let keys: &[&str] = match method {
        "handoff.incoming.list" => &[],
        "handoff.incoming.get" | "handoff.decline" | "handoff.resume" => &["id"],
        // Read, or set whether the user's own handoffs may import without asking.
        "handoff.prefs" => &["always_ask"],
        "handoff.accept" => &[
            "id",
            "repo",
            "worktree_path",
            "branch",
            "start_agent",
            "trust",
        ],
        _ => return Err(err("method_not_found", method)),
    };
    let mut params = json!({});
    for k in keys {
        if let Some(v) = p.get(*k).filter(|v| !v.is_null()) {
            params[*k] = v.clone();
        }
    }
    if matches!(method, "handoff.incoming.list" | "handoff.incoming.get") {
        return gw.server.call(method, params).await;
    }
    still_authorized(gw, dev)?;
    gw.server
        .call_as(&format!("gateway:{}", dev.name), method, params)
        .await
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
        let before = head_log_len(&repo).await;
        assert!(before.is_some());
        assert_eq!(head_log_len(&repo).await, before);
        sh(&repo, &["checkout", "-q", "main"]);
        sh(&repo, &["checkout", "-q", "feature"]);
        assert!(still_at(&repo, Some("feature"), &b).await.is_ok());
        assert_ne!(head_log_len(&repo).await, before);
    }

    #[test]
    fn an_expectation_matches_only_the_approved_repository_branch_and_commit() {
        let e = Expected::from_json(
            &json!({"repo_root": "/src/app", "branch": "main", "head": "a".repeat(40)}),
        )
        .unwrap();
        assert!(e.check("/src/app", Some("main"), &"a".repeat(40)).is_ok());
        for (root, branch, head) in [
            ("/src/other", Some("main"), "a".repeat(40)),
            ("/src/app", Some("feature"), "a".repeat(40)),
            ("/src/app", None, "a".repeat(40)),
            ("/src/app", Some("main"), "b".repeat(40)),
        ] {
            let x = e.check(root, branch, &head).unwrap_err();
            assert!(x.message.starts_with("repo_moved"), "{}", x.message);
        }
    }
}
