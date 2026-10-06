//! `clone` code isolation and `vibeke task sync` (05 §4, 13 §6).
//!
//! A container box never sees the user's `.git`: it gets a private repo whose object store
//! borrows the host's objects read-only through `objects/info/alternates` (the host objects
//! dir is bind-mounted read-only at the same path), with the task branch created at the base
//! commit ([`box_clone_script`]). Results come back by a **host-side fetch** ([`sync_pull`]):
//! the host runs `git fetch` in its own repo against the box repo, so the box never writes host
//! refs, hooks or config. Host commits go in by a push to a side ref ([`sync_push`]) that the
//! box fast-forwards itself ([`box_ff_script`]).
//!
//! The git *services* for the box repo (`upload-pack`, `receive-pack`) are a parameter
//! ([`BoxRemote`]): for a running container they run **inside** the box via `<runtime> exec`,
//! so repo-controlled config or hooks only ever execute there. [`BoxRemote::local`] runs them on
//! the host against a local path with hooks, fsmonitor, alternate-ref commands and auto-gc
//! disabled (used when the box is stopped, and by the tests).

use crate::git::{exec, git, git_ok};
use crate::{Error, Result};
use serde::Serialize;
use std::path::Path;

/// `-c` flags for every host-side git command that touches a task branch from a box (13 §6):
/// no hooks, no fsmonitor, no alternate-refs command, no auto-gc.
pub const HARDEN: &[&str] = &[
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.alternateRefsCommand=true",
    "-c",
    "gc.auto=0",
    "-c",
    "receive.autogc=false",
    "-c",
    "protocol.file.allow=always",
];

/// Where the box repo is and how to run git services against it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoxRemote {
    /// Path of the repo as the service command sees it (`/workspace` inside a box).
    pub url: String,
    /// Shell command git runs as `--upload-pack` (the path is appended by git).
    pub upload_pack: String,
    /// Shell command git runs as `--receive-pack`.
    pub receive_pack: String,
}

fn q(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn hardened_service(service: &str) -> String {
    let mut v = vec!["git".to_string()];
    v.extend(HARDEN.iter().map(|s| q(s)));
    v.push(service.to_string());
    v.join(" ")
}

impl BoxRemote {
    /// Services run on the host against a local repo path, hardened (no hooks etc.).
    pub fn local(path: &Path) -> BoxRemote {
        BoxRemote {
            url: path.to_string_lossy().into_owned(),
            upload_pack: hardened_service("upload-pack"),
            receive_pack: hardened_service("receive-pack"),
        }
    }
}

/// Script run **inside the box** (as the box user) to create the private repo: `git init`,
/// alternates to the read-only host objects, the task branch at `base_sha`, checkout.
/// Idempotent: does nothing when `<workspace>/.git` exists.
pub fn box_clone_script(
    host_objects: &Path,
    workspace: &str,
    branch: &str,
    base_sha: &str,
    user_name: Option<&str>,
    user_email: Option<&str>,
) -> String {
    let w = q(workspace);
    let mut s = vec![
        "set -e".to_string(),
        format!("if [ -e {w}/.git ]; then exit 0; fi"),
        format!("mkdir -p {w}"),
        format!("git init -q {w}"),
        format!(
            "git -C {w} symbolic-ref HEAD {}",
            q(&format!("refs/heads/{branch}"))
        ),
        format!(
            "printf '%s\\n' {} > {w}/.git/objects/info/alternates",
            q(&host_objects.to_string_lossy())
        ),
        format!(
            "git -C {w} update-ref {} {}",
            q(&format!("refs/heads/{branch}")),
            q(base_sha)
        ),
        format!("git -C {w} reset -q --hard"),
    ];
    if let Some(n) = user_name {
        s.push(format!("git -C {w} config user.name {}", q(n)));
    }
    if let Some(e) = user_email {
        s.push(format!("git -C {w} config user.email {}", q(e)));
    }
    s.join("\n")
}

/// Script run **inside the box** to fast-forward its branch to the side ref a host push wrote.
/// Exit 3 = the box worktree has uncommitted changes (nothing done).
pub fn box_ff_script(workspace: &str, branch: &str) -> String {
    let w = q(workspace);
    format!(
        "set -e\ncd {w}\nif [ -n \"$(git status --porcelain --untracked-files=no)\" ]; then echo dirty; exit 3; fi\ngit merge -q --ff-only {}\necho merged",
        q(&host_ref(branch))
    )
}

/// Side ref in the box repo that host pushes write.
pub fn host_ref(branch: &str) -> String {
    format!("refs/vibeke/host/{branch}")
}

/// Mirror ref in the host repo that pulls write (always updated, even when the task branch
/// can't be moved).
pub fn mirror_ref(ns: &str, branch: &str) -> String {
    format!("refs/vibeke/box/{ns}/{branch}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    UpToDate,
    /// The host task branch moved (ref update or `merge --ff-only` in its clean worktree).
    FastForwarded,
    /// Box and host histories diverged; only the mirror ref was updated.
    Diverged,
    /// The task branch is checked out in a worktree with uncommitted changes; only the mirror
    /// ref was updated.
    CheckedOutDirty,
    /// Host commits written to the box's side ref.
    Pushed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyncOutcome {
    pub direction: &'static str,
    pub status: SyncStatus,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Commits newly reachable on the target side.
    pub commits: u32,
    /// The ref that was written (mirror ref for pulls, side ref for pushes).
    #[serde(rename = "ref")]
    pub reference: String,
}

fn rev(repo: &Path, r: &str) -> Option<String> {
    git(
        repo,
        &["rev-parse", "--verify", "-q", &format!("{r}^{{commit}}")],
    )
    .ok()
}

fn count(repo: &Path, range: &str) -> u32 {
    git(repo, &["rev-list", "--count", range])
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn hardened(repo: &Path, args: &[&str]) -> Result<String> {
    let mut v: Vec<&str> = HARDEN.to_vec();
    v.extend_from_slice(args);
    let out = exec(repo, &v, Some(std::time::Duration::from_secs(300)))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// Pull the box's `box_branch` into the host: fetch into [`mirror_ref`]`(ns, box_branch)`, then
/// fast-forward `refs/heads/<task_branch>` (in its worktree with `merge --ff-only` if it is
/// checked out and clean). `force` allows a non-fast-forward update of the task branch when it
/// is not checked out.
pub fn sync_pull(
    repo: &Path,
    remote: &BoxRemote,
    box_branch: &str,
    task_branch: &str,
    ns: &str,
    force: bool,
) -> Result<SyncOutcome> {
    let mirror = mirror_ref(ns, box_branch);
    let old = rev(repo, &format!("refs/heads/{task_branch}"));
    hardened(
        repo,
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--upload-pack",
            &remote.upload_pack,
            &remote.url,
            &format!("+refs/heads/{box_branch}:{mirror}"),
        ],
    )?;
    let new = rev(repo, &mirror)
        .ok_or_else(|| Error::Refused(format!("box branch {box_branch} not found")))?;
    let mut out = SyncOutcome {
        direction: "pull",
        status: SyncStatus::UpToDate,
        from: old.clone(),
        to: Some(new.clone()),
        commits: 0,
        reference: mirror,
    };
    if old.as_deref() == Some(new.as_str()) {
        return Ok(out);
    }
    out.commits = match &old {
        Some(o) => count(repo, &format!("{o}..{new}")),
        None => count(repo, &new),
    };
    let ff = old
        .as_ref()
        .is_none_or(|o| git_ok(repo, &["merge-base", "--is-ancestor", o, &new]));
    let checked_out = crate::worktree::find_worktree_by_branch(repo, task_branch).ok();
    if !ff && (!force || checked_out.is_some()) {
        out.status = SyncStatus::Diverged;
        return Ok(out);
    }
    match checked_out {
        Some(w) => {
            let dirty = git(&w.path, &["status", "--porcelain", "--untracked-files=no"])
                .map(|s| !s.trim().is_empty())
                .unwrap_or(true);
            if dirty {
                out.status = SyncStatus::CheckedOutDirty;
                return Ok(out);
            }
            hardened(&w.path, &["merge", "--quiet", "--ff-only", &new])?;
        }
        None => {
            let r = format!("refs/heads/{task_branch}");
            let mut args = vec!["update-ref", "-m", "vibeke task sync", r.as_str(), &new];
            if let Some(o) = &old {
                args.push(o);
            }
            hardened(repo, &args)?;
        }
    }
    out.status = SyncStatus::FastForwarded;
    Ok(out)
}

/// Push the host's `task_branch` into the box repo's [`host_ref`]`(box_branch)`.
pub fn sync_push(
    repo: &Path,
    remote: &BoxRemote,
    task_branch: &str,
    box_branch: &str,
) -> Result<SyncOutcome> {
    let head = rev(repo, &format!("refs/heads/{task_branch}"))
        .ok_or_else(|| Error::Refused(format!("host branch {task_branch} not found")))?;
    let target = host_ref(box_branch);
    hardened(
        repo,
        &[
            "push",
            "--quiet",
            "--no-verify",
            "--receive-pack",
            &remote.receive_pack,
            &remote.url,
            &format!("+refs/heads/{task_branch}:{target}"),
        ],
    )?;
    Ok(SyncOutcome {
        direction: "push",
        status: SyncStatus::Pushed,
        from: None,
        to: Some(head),
        commits: 0,
        reference: target,
    })
}

/// The host repo's object store (`<common-dir>/objects`), for the box's read-only alternates.
pub fn objects_dir(repo: &Path) -> Result<std::path::PathBuf> {
    let common = git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    Ok(Path::new(&common).join("objects"))
}

/// Commit id of `r` in `repo`.
pub fn resolve_commit(repo: &Path, r: &str) -> Result<String> {
    rev(repo, r).ok_or_else(|| Error::Refused(format!("{r} is not a commit")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    fn g(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    struct Fixture {
        _t: tempfile::TempDir,
        root: PathBuf,
        repo: PathBuf,
        wt: PathBuf,
        boxrepo: PathBuf,
        base: String,
    }

    /// Host repo + task worktree + a "box" repo created by the real clone script (run with
    /// `sh` on the host; the alternates path is the host objects dir, exactly as the box mount
    /// would present it).
    fn fixture() -> Fixture {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        g(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        g(&repo, &["add", "-A"]);
        g(&repo, &["commit", "-q", "-m", "base"]);
        let base = g(&repo, &["rev-parse", "HEAD"]);
        let wt = root.join("wt");
        g(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "u/task",
                wt.to_str().unwrap(),
                "main",
            ],
        );
        let boxrepo = root.join("box/workspace");
        let script = box_clone_script(
            &objects_dir(&repo).unwrap(),
            boxrepo.to_str().unwrap(),
            "u/task",
            &base,
            Some("Box Agent"),
            Some("box@example.invalid"),
        );
        let out = Command::new("sh")
            .args(["-c", &script])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // Idempotent.
        assert!(
            Command::new("sh")
                .args(["-c", &script])
                .status()
                .unwrap()
                .success()
        );
        Fixture {
            _t: t,
            root,
            repo,
            wt,
            boxrepo,
            base,
        }
    }

    fn box_commit(f: &Fixture, name: &str) -> String {
        std::fs::write(f.boxrepo.join(name), name).unwrap();
        g(&f.boxrepo, &["add", "-A"]);
        g(&f.boxrepo, &["commit", "-q", "-m", name]);
        g(&f.boxrepo, &["rev-parse", "HEAD"])
    }

    #[test]
    fn clone_script_borrows_host_objects_without_copying() {
        let f = fixture();
        assert_eq!(g(&f.boxrepo, &["rev-parse", "HEAD"]), f.base);
        assert_eq!(
            g(&f.boxrepo, &["symbolic-ref", "--short", "HEAD"]),
            "u/task"
        );
        assert_eq!(
            std::fs::read_to_string(f.boxrepo.join("a.txt")).unwrap(),
            "one\n"
        );
        let alt = std::fs::read_to_string(f.boxrepo.join(".git/objects/info/alternates")).unwrap();
        assert_eq!(alt.trim(), objects_dir(&f.repo).unwrap().to_string_lossy());
        // The box's own `.git` is separate from the host's.
        assert!(!f.boxrepo.join(".git").is_symlink());
        assert_eq!(
            g(&f.boxrepo, &["config", "--local", "user.name"]),
            "Box Agent"
        );
    }

    #[test]
    fn pull_fast_forwards_the_checked_out_task_branch() {
        let f = fixture();
        let c1 = box_commit(&f, "b.txt");
        let c2 = box_commit(&f, "c.txt");
        let o = sync_pull(
            &f.repo,
            &BoxRemote::local(&f.boxrepo),
            "u/task",
            "u/task",
            "T1",
            false,
        )
        .unwrap();
        assert_eq!(o.status, SyncStatus::FastForwarded);
        assert_eq!(o.commits, 2);
        assert_eq!(o.from.as_deref(), Some(f.base.as_str()));
        assert_eq!(o.to.as_deref(), Some(c2.as_str()));
        assert_eq!(g(&f.wt, &["rev-parse", "HEAD"]), c2);
        assert!(f.wt.join("b.txt").is_file());
        assert_eq!(g(&f.repo, &["rev-parse", "refs/vibeke/box/T1/u/task"]), c2);
        let _ = c1;
        // Again: nothing new.
        let o = sync_pull(
            &f.repo,
            &BoxRemote::local(&f.boxrepo),
            "u/task",
            "u/task",
            "T1",
            false,
        )
        .unwrap();
        assert_eq!(o.status, SyncStatus::UpToDate);
    }

    #[test]
    fn pull_never_imports_box_hooks_or_runs_box_config() {
        let f = fixture();
        // The agent plants a hook and hostile config in its own repo.
        let marker = f.root.join("pwned");
        let hook = f.boxrepo.join(".git/hooks/post-merge");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        std::fs::write(
            f.boxrepo.join(".git/hooks/pre-commit"),
            format!("#!/bin/sh\ntouch {}\n", marker.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        for h in ["post-merge", "pre-commit"] {
            std::fs::set_permissions(
                f.boxrepo.join(".git/hooks").join(h),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        let touch = format!("touch {}", marker.display());
        for (k, v) in [
            ("core.fsmonitor", touch.as_str()),
            ("core.alternateRefsCommand", touch.as_str()),
            ("uploadpack.packObjectsHook", touch.as_str()),
            (
                "core.hooksPath",
                f.boxrepo.join(".git/hooks").to_str().unwrap(),
            ),
        ] {
            g(&f.boxrepo, &["config", k, v]);
        }
        // Also commit a hooks dir into the tree (only a file, never installed by git).
        std::fs::create_dir_all(f.boxrepo.join(".githooks")).unwrap();
        std::fs::write(
            f.boxrepo.join(".githooks/pre-commit"),
            "#!/bin/sh\nexit 1\n",
        )
        .unwrap();
        g(&f.boxrepo, &["-c", "core.hooksPath=/dev/null", "add", "-A"]);
        g(
            &f.boxrepo,
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "-q",
                "-m",
                "evil",
            ],
        );
        let _ = std::fs::remove_file(&marker);
        let o = sync_pull(
            &f.repo,
            &BoxRemote::local(&f.boxrepo),
            "u/task",
            "u/task",
            "T1",
            false,
        )
        .unwrap();
        assert_eq!(o.status, SyncStatus::FastForwarded);
        assert!(!marker.exists(), "box-controlled code ran on the host");
        let hooks = f.repo.join(".git/hooks");
        let installed: Vec<_> = std::fs::read_dir(&hooks)
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| !n.ends_with(".sample"))
                    .collect()
            })
            .unwrap_or_default();
        assert!(installed.is_empty(), "hooks on the host: {installed:?}");
        assert!(
            g(&f.repo, &["config", "--get-regexp", "core\\."])
                .find("fsmonitor")
                .is_none()
        );
    }

    #[test]
    fn pull_reports_divergence_and_dirty_checkouts() {
        let f = fixture();
        box_commit(&f, "b.txt");
        // Host commits on the task branch in the meantime.
        std::fs::write(f.wt.join("h.txt"), "host").unwrap();
        g(&f.wt, &["add", "-A"]);
        g(&f.wt, &["commit", "-q", "-m", "host"]);
        let host_head = g(&f.wt, &["rev-parse", "HEAD"]);
        let o = sync_pull(
            &f.repo,
            &BoxRemote::local(&f.boxrepo),
            "u/task",
            "u/task",
            "T1",
            true,
        )
        .unwrap();
        // Checked out: even force never rewrites a worktree's branch.
        assert_eq!(o.status, SyncStatus::Diverged);
        assert_eq!(g(&f.wt, &["rev-parse", "HEAD"]), host_head);
        // The box's work is still available on the mirror ref.
        assert!(rev(&f.repo, &o.reference).is_some());

        // Dirty checkout + fast-forwardable box: untouched.
        let f = fixture();
        box_commit(&f, "b.txt");
        std::fs::write(f.wt.join("a.txt"), "edited\n").unwrap();
        let o = sync_pull(
            &f.repo,
            &BoxRemote::local(&f.boxrepo),
            "u/task",
            "u/task",
            "T1",
            false,
        )
        .unwrap();
        assert_eq!(o.status, SyncStatus::CheckedOutDirty);
        assert_eq!(g(&f.wt, &["rev-parse", "HEAD"]), f.base);
    }

    #[test]
    fn pull_into_a_branch_without_worktree_and_force() {
        let f = fixture();
        g(
            &f.repo,
            &["worktree", "remove", "--force", f.wt.to_str().unwrap()],
        );
        let c = box_commit(&f, "b.txt");
        let o = sync_pull(
            &f.repo,
            &BoxRemote::local(&f.boxrepo),
            "u/task",
            "u/task",
            "T1",
            false,
        )
        .unwrap();
        assert_eq!(o.status, SyncStatus::FastForwarded);
        assert_eq!(g(&f.repo, &["rev-parse", "refs/heads/u/task"]), c);
        // The box rewrites history: refused without force, applied with it.
        g(&f.boxrepo, &["reset", "-q", "--hard", &f.base]);
        let c2 = box_commit(&f, "z.txt");
        let o = sync_pull(
            &f.repo,
            &BoxRemote::local(&f.boxrepo),
            "u/task",
            "u/task",
            "T1",
            false,
        )
        .unwrap();
        assert_eq!(o.status, SyncStatus::Diverged);
        assert_eq!(g(&f.repo, &["rev-parse", "refs/heads/u/task"]), c);
        let o = sync_pull(
            &f.repo,
            &BoxRemote::local(&f.boxrepo),
            "u/task",
            "u/task",
            "T1",
            true,
        )
        .unwrap();
        assert_eq!(o.status, SyncStatus::FastForwarded);
        assert_eq!(g(&f.repo, &["rev-parse", "refs/heads/u/task"]), c2);
    }

    #[test]
    fn push_writes_a_side_ref_without_running_box_hooks() {
        let f = fixture();
        let marker = f.root.join("pwned");
        for h in ["pre-receive", "update", "post-receive", "post-update"] {
            let p = f.boxrepo.join(".git/hooks").join(h);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(f.wt.join("h.txt"), "host").unwrap();
        g(&f.wt, &["add", "-A"]);
        g(&f.wt, &["commit", "-q", "-m", "host fix"]);
        let head = g(&f.wt, &["rev-parse", "HEAD"]);
        let o = sync_push(&f.repo, &BoxRemote::local(&f.boxrepo), "u/task", "u/task").unwrap();
        assert_eq!(o.status, SyncStatus::Pushed);
        assert_eq!(o.to.as_deref(), Some(head.as_str()));
        assert!(!marker.exists(), "box hooks ran on the host");
        assert_eq!(
            g(&f.boxrepo, &["rev-parse", "refs/vibeke/host/u/task"]),
            head
        );
        // The box fast-forwards itself (this script runs inside the box).
        let out = Command::new("sh")
            .args(["-c", &box_ff_script(f.boxrepo.to_str().unwrap(), "u/task")])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(g(&f.boxrepo, &["rev-parse", "HEAD"]), head);
        assert!(f.boxrepo.join("h.txt").is_file());
        // Dirty box worktree: exit 3, nothing changes.
        std::fs::write(f.boxrepo.join("a.txt"), "dirty").unwrap();
        let st = Command::new("sh")
            .args(["-c", &box_ff_script(f.boxrepo.to_str().unwrap(), "u/task")])
            .status()
            .unwrap();
        assert_eq!(st.code(), Some(3));
    }
}
