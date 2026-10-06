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
//! so repo-controlled config or hooks only ever execute there. [`BoxRemote::local`] (a stopped
//! box, and the tests) never runs `receive-pack` on the host: `receive-pack` honours the box's
//! `receive.denyCurrentBranch=updateInstead`, `core.worktree` and attributes/filters even with
//! hooks off. A push to a stopped box writes the side ref **itself** ([`write_box_ref`],
//! symlink-safe); the objects are already visible to the box through its alternates.
//! `upload-pack` (pulls) runs hardened and only reads objects and refs.
//!
//! [`box_leftovers`] inspects a stopped box's repo from the host (hardened, filters
//! neutralized) for anything not on the host yet: index/worktree changes, untracked files,
//! other branches, a detached HEAD, stashes.

use crate::git::{HOST_HARDEN, exec, git, git_ok};
use crate::{Error, Result};
use serde::Serialize;
use std::path::{Component, Path, PathBuf};

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
    /// Host path of the box repo when the services would run on the host ([`BoxRemote::local`]):
    /// pushes then write the side ref directly instead of running `receive-pack`.
    pub local: Option<PathBuf>,
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
            local: Some(path.to_path_buf()),
        }
    }
}

/// `<box>/.git` must be a plain directory (not a symlink to some host repo, not a `gitdir:`
/// link file) before Vibeke touches a box repo from the host.
fn box_git_dir(box_dir: &Path) -> Result<PathBuf> {
    let g = box_dir.join(".git");
    match std::fs::symlink_metadata(&g) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => Ok(g),
        _ => Err(Error::Refused(format!(
            "{} is not a plain git directory; not touching it from the host",
            g.display()
        ))),
    }
}

/// Write `full_ref` = `sha` as a loose ref in the box repo at `box_dir`, from the host, without
/// running git there and without following any symlink the box planted under `.git`.
pub fn write_box_ref(box_dir: &Path, full_ref: &str, sha: &str) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    if sha.len() < 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::Refused(format!("{sha} is not an object id")));
    }
    let rel = Path::new(full_ref);
    let comps: Vec<&std::ffi::OsStr> = rel
        .components()
        .map(|c| match c {
            Component::Normal(n) => Ok(n),
            _ => Err(Error::Refused(format!("bad ref name {full_ref}"))),
        })
        .collect::<Result<_>>()?;
    if comps.first().is_none_or(|c| *c != "refs") || comps.len() < 2 {
        return Err(Error::Refused(format!("bad ref name {full_ref}")));
    }
    let mut cur = box_git_dir(box_dir)?;
    for c in &comps[..comps.len() - 1] {
        cur.push(c);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(Error::Refused(format!(
                    "{} is not a plain directory",
                    cur.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir(&cur)?,
            Err(e) => return Err(e.into()),
        }
    }
    let file = cur.join(comps[comps.len() - 1]);
    if let Ok(m) = std::fs::symlink_metadata(&file) {
        if m.is_dir() {
            return Err(Error::Refused(format!("{} is a directory", file.display())));
        }
        std::fs::remove_file(&file)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o644)
        .open(&file)?;
    f.write_all(format!("{sha}\n").as_bytes())?;
    Ok(())
}

/// Hardened git in a box repo seen from the host: explicit `GIT_DIR`/`GIT_WORK_TREE` (the box's
/// `core.worktree` is ignored), [`HOST_HARDEN`], every filter driver neutralized, no optional
/// locks, no system config.
fn box_git(box_dir: &Path, args: &[&str]) -> Result<String> {
    let gd = box_git_dir(box_dir)?;
    let base = |c: &mut std::process::Command| {
        c.env("GIT_DIR", &gd)
            .env("GIT_WORK_TREE", box_dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .current_dir(box_dir)
            .stdin(std::process::Stdio::null());
    };
    let mut cfg = std::process::Command::new("git");
    base(&mut cfg);
    let names = cfg
        .args([
            "config",
            "--null",
            "--name-only",
            "--get-regexp",
            r"^filter\.",
        ])
        .output()
        .map(|o| o.stdout)
        .unwrap_or_default();
    let mut filters: Vec<String> = Vec::new();
    for k in names.split(|&b| b == 0) {
        let k = String::from_utf8_lossy(k);
        if let Some((n, _)) = k.strip_prefix("filter.").and_then(|r| r.rsplit_once('.')) {
            for f in ["clean", "smudge", "process"] {
                filters.extend(["-c".to_string(), format!("filter.{n}.{f}=")]);
            }
        }
    }
    let mut c = std::process::Command::new("git");
    base(&mut c);
    let out = c.args(HOST_HARDEN).args(&filters).args(args).output()?;
    if !out.status.success() {
        return Err(Error::Git {
            args: args.join(" "),
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// What a (stopped) box repo holds that the host's `task_branch` does not (13 §6, task
/// finish): uncommitted or staged changes, untracked files, a detached HEAD, other branches or
/// stashes whose commits are not reachable from the host branch. Empty = safe to delete.
/// Ignored files (build output, dependencies) are not work and not reported.
pub fn box_leftovers(box_dir: &Path, host_repo: &Path, task_branch: &str) -> Vec<String> {
    let mut why = Vec::new();
    if let Err(e) = box_git_dir(box_dir) {
        return vec![e.to_string()];
    }
    match box_git(box_dir, &["status", "--porcelain", "--untracked-files=all"]) {
        Ok(s) if !s.trim().is_empty() => {
            let n = s.lines().count();
            let untracked = s.lines().filter(|l| l.starts_with("??")).count();
            why.push(format!(
                "{n} uncommitted change(s) in the box ({untracked} untracked)"
            ));
        }
        Ok(_) => {}
        Err(e) => why.push(format!("box status failed: {e}")),
    }
    let on_host = |sha: &str| {
        git_ok(
            host_repo,
            &[
                "merge-base",
                "--is-ancestor",
                sha,
                &format!("refs/heads/{task_branch}"),
            ],
        )
    };
    if box_git(box_dir, &["symbolic-ref", "-q", "HEAD"]).is_err() {
        match box_git(box_dir, &["rev-parse", "--verify", "-q", "HEAD"]) {
            Ok(sha) if !on_host(&sha) => {
                why.push(format!("detached HEAD at {sha} is not on the host"))
            }
            _ => {}
        }
    }
    match box_git(
        box_dir,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads",
            "refs/stash",
            "refs/notes",
            "refs/tags",
        ],
    ) {
        Ok(list) => {
            for l in list.lines() {
                let Some((r, sha)) = l.split_once(' ') else {
                    continue;
                };
                if r == "refs/stash" {
                    why.push("the box has stashed changes".into());
                } else if !on_host(sha) {
                    why.push(format!(
                        "{r} ({}) is not on the host",
                        &sha[..sha.len().min(12)]
                    ));
                }
            }
        }
        Err(e) => why.push(format!("box refs unreadable: {e}")),
    }
    why
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
    if let Some(dir) = &remote.local {
        box_git_dir(dir)?;
    }
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
    match &remote.local {
        // Stopped box: no `receive-pack` on the host (it would honour the box's
        // `receive.denyCurrentBranch=updateInstead`, `core.worktree` and filters). The commits
        // are in the host object store the box borrows through its alternates; only the side
        // ref needs writing.
        Some(dir) => write_box_ref(dir, &target, &head)?,
        None => {
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
        }
    }
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

    fn evil_script(f: &Fixture, marker: &Path) -> String {
        let p = f.root.join("evil.sh");
        std::fs::write(&p, format!("#!/bin/sh\ntouch {}\ncat\n", marker.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.to_string_lossy().into_owned()
    }

    /// Review finding 3: a stopped box's repo is hostile input. Pushing a host commit into it
    /// must not run updateInstead, filters or a foreign `core.worktree` on the host.
    #[test]
    fn stopped_box_push_never_runs_receive_pack_machinery() {
        let f = fixture();
        let marker = f.root.join("pwned");
        let evil = evil_script(&f, &marker);
        // The agent points HEAD at the side ref, asks receive-pack to update the worktree,
        // installs a clean/smudge filter for every path and redirects the worktree to a host
        // directory.
        let victim = f.root.join("victim-dir");
        std::fs::create_dir_all(&victim).unwrap();
        g(&f.boxrepo, &["symbolic-ref", "HEAD", &host_ref("u/task")]);
        for (k, v) in [
            ("receive.denyCurrentBranch", "updateInstead"),
            ("filter.evil.clean", evil.as_str()),
            ("filter.evil.smudge", evil.as_str()),
            ("filter.evil.required", "true"),
            ("core.worktree", victim.to_str().unwrap()),
        ] {
            g(&f.boxrepo, &["config", k, v]);
        }
        std::fs::create_dir_all(f.boxrepo.join(".git/info")).unwrap();
        std::fs::write(f.boxrepo.join(".git/info/attributes"), "* filter=evil\n").unwrap();
        // A host commit to push.
        std::fs::write(f.wt.join("h.txt"), "host").unwrap();
        g(&f.wt, &["add", "-A"]);
        g(&f.wt, &["commit", "-q", "-m", "host fix"]);
        let head = g(&f.wt, &["rev-parse", "HEAD"]);
        let o = sync_push(&f.repo, &BoxRemote::local(&f.boxrepo), "u/task", "u/task").unwrap();
        assert_eq!(o.status, SyncStatus::Pushed);
        assert!(!marker.exists(), "box-controlled code ran on the host");
        assert_eq!(std::fs::read_dir(&victim).unwrap().count(), 0);
        assert!(
            !f.boxrepo.join("h.txt").exists(),
            "worktree updated on the host"
        );
        assert_eq!(
            std::fs::read_to_string(f.boxrepo.join(".git/refs/vibeke/host/u/task"))
                .unwrap()
                .trim(),
            head
        );
        // A pull from the same hostile repo is just as inert.
        let _ = sync_pull(
            &f.repo,
            &BoxRemote::local(&f.boxrepo),
            "u/task",
            "u/task",
            "T1",
            false,
        );
        assert!(!marker.exists());
    }

    #[test]
    fn stopped_box_ref_write_never_follows_symlinks() {
        let f = fixture();
        let outside = f.root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(f.boxrepo.join(".git/refs")).unwrap();
        std::os::unix::fs::symlink(&outside, f.boxrepo.join(".git/refs/vibeke")).unwrap();
        let e = sync_push(&f.repo, &BoxRemote::local(&f.boxrepo), "u/task", "u/task");
        assert!(e.is_err());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        // The final component as a symlink: replaced, the target is untouched.
        std::fs::remove_file(f.boxrepo.join(".git/refs/vibeke")).unwrap();
        std::fs::create_dir_all(f.boxrepo.join(".git/refs/vibeke/host/u")).unwrap();
        let target = outside.join("file");
        std::fs::write(&target, "keep").unwrap();
        std::os::unix::fs::symlink(&target, f.boxrepo.join(".git/refs/vibeke/host/u/task"))
            .unwrap();
        sync_push(&f.repo, &BoxRemote::local(&f.boxrepo), "u/task", "u/task").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
        // A box whose `.git` is a link to some other repo is refused outright.
        let other = f.root.join("other");
        std::fs::rename(f.boxrepo.join(".git"), &other).unwrap();
        std::os::unix::fs::symlink(&other, f.boxrepo.join(".git")).unwrap();
        assert!(sync_push(&f.repo, &BoxRemote::local(&f.boxrepo), "u/task", "u/task").is_err());
        assert!(!box_leftovers(&f.boxrepo, &f.repo, "u/task").is_empty());
    }

    /// Review finding 13: work that is only in the box (index, worktree, untracked files, other
    /// branches, a detached HEAD, stashes) is reported, so finish keeps the box.
    #[test]
    fn leftovers_report_everything_not_on_the_host() {
        let f = fixture();
        let synced = |f: &Fixture| {
            sync_pull(
                &f.repo,
                &BoxRemote::local(&f.boxrepo),
                "u/task",
                "u/task",
                "T1",
                false,
            )
            .unwrap()
        };
        box_commit(&f, "b.txt");
        synced(&f);
        assert_eq!(
            box_leftovers(&f.boxrepo, &f.repo, "u/task"),
            Vec::<String>::new()
        );
        // Ignored files are not work.
        std::fs::write(f.boxrepo.join(".gitignore"), "target/\n").unwrap();
        g(&f.boxrepo, &["add", ".gitignore"]);
        g(&f.boxrepo, &["commit", "-q", "-m", "ignore"]);
        synced(&f);
        std::fs::create_dir_all(f.boxrepo.join("target")).unwrap();
        std::fs::write(f.boxrepo.join("target/out"), "bin").unwrap();
        assert!(box_leftovers(&f.boxrepo, &f.repo, "u/task").is_empty());

        // Unstaged, staged and untracked.
        std::fs::write(f.boxrepo.join("a.txt"), "edited\n").unwrap();
        let l = box_leftovers(&f.boxrepo, &f.repo, "u/task");
        assert!(l.iter().any(|x| x.contains("uncommitted")), "{l:?}");
        g(&f.boxrepo, &["add", "a.txt"]);
        assert!(!box_leftovers(&f.boxrepo, &f.repo, "u/task").is_empty());
        g(&f.boxrepo, &["reset", "-q", "--hard"]);
        std::fs::write(f.boxrepo.join("new.txt"), "untracked\n").unwrap();
        let l = box_leftovers(&f.boxrepo, &f.repo, "u/task");
        assert!(l.iter().any(|x| x.contains("1 untracked")), "{l:?}");
        std::fs::remove_file(f.boxrepo.join("new.txt")).unwrap();
        assert!(box_leftovers(&f.boxrepo, &f.repo, "u/task").is_empty());

        // Another branch with its own commit.
        g(&f.boxrepo, &["checkout", "-q", "-b", "side"]);
        box_commit(&f, "side.txt");
        g(&f.boxrepo, &["checkout", "-q", "u/task"]);
        let l = box_leftovers(&f.boxrepo, &f.repo, "u/task");
        assert!(l.iter().any(|x| x.contains("refs/heads/side")), "{l:?}");
        g(&f.boxrepo, &["branch", "-q", "-D", "side"]);
        assert!(box_leftovers(&f.boxrepo, &f.repo, "u/task").is_empty());

        // A detached HEAD with a new commit.
        g(&f.boxrepo, &["checkout", "-q", "--detach"]);
        box_commit(&f, "detached.txt");
        let l = box_leftovers(&f.boxrepo, &f.repo, "u/task");
        assert!(l.iter().any(|x| x.contains("detached")), "{l:?}");
        g(&f.boxrepo, &["checkout", "-q", "u/task"]);

        // A stash.
        std::fs::write(f.boxrepo.join("a.txt"), "stash me\n").unwrap();
        g(&f.boxrepo, &["stash", "-q"]);
        let l = box_leftovers(&f.boxrepo, &f.repo, "u/task");
        assert!(l.iter().any(|x| x.contains("stash")), "{l:?}");
    }

    #[test]
    fn leftovers_check_runs_no_box_code() {
        let f = fixture();
        let marker = f.root.join("pwned");
        let evil = evil_script(&f, &marker);
        g(&f.boxrepo, &["config", "filter.evil.clean", &evil]);
        g(&f.boxrepo, &["config", "core.fsmonitor", &evil]);
        g(
            &f.boxrepo,
            &["config", "core.worktree", f.root.to_str().unwrap()],
        );
        std::fs::write(f.boxrepo.join(".gitattributes"), "* filter=evil\n").unwrap();
        std::fs::write(f.boxrepo.join("a.txt"), "dirty\n").unwrap();
        let l = box_leftovers(&f.boxrepo, &f.repo, "u/task");
        assert!(!l.is_empty());
        assert!(!marker.exists(), "box filter/fsmonitor ran on the host");
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
