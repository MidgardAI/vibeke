//! Thin wrapper around the `git` CLI.
//!
//! Every command runs with `core.fsmonitor=false` (Vibeke's own queries never need it, and it is
//! a program git would run). Checkouts that a contained process can write to (13 §6: sandboxed
//! task worktrees, container worktree-mode binds) are registered with
//! [`register_contained_checkout`]; git commands in them additionally run **hardened**
//! ([`HOST_HARDEN`] plus every configured filter driver neutralized), and refuse to run at all
//! when the checkout's `.git` no longer points at the git dir recorded at registration (a box
//! that swapped its `.git` file for one naming a repository it controls).

use crate::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

/// `-c` overrides for git commands in a contained checkout: nothing the repo's (box-writable)
/// configuration names gets executed by a host-side git command.
pub const HOST_HARDEN: &[&str] = &[
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.sshCommand=ssh",
    "-c",
    "core.alternateRefsCommand=true",
    "-c",
    "core.pager=cat",
    "-c",
    "core.editor=true",
    "-c",
    "sequence.editor=true",
    "-c",
    "core.untrackedCache=false",
    "-c",
    "diff.external=",
    "-c",
    "credential.helper=",
    "-c",
    "protocol.ext.allow=never",
    "-c",
    "submodule.recurse=false",
    "-c",
    "gc.auto=0",
];

/// A registered contained checkout and the git dir its `.git` must keep pointing at.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Contained {
    checkout: PathBuf,
    git_dir: Option<PathBuf>,
}

fn registry() -> &'static Mutex<Vec<Contained>> {
    static R: OnceLock<Mutex<Vec<Contained>>> = OnceLock::new();
    R.get_or_init(Mutex::default)
}

fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Where `<checkout>/.git` points right now: the dir itself, or the `gitdir:` of a link file
/// (resolved against the checkout). `None` when there is no `.git` or it is unreadable.
fn current_git_dir(checkout: &Path) -> Option<PathBuf> {
    let dot = checkout.join(".git");
    let md = std::fs::symlink_metadata(&dot).ok()?;
    if md.file_type().is_symlink() {
        return Some(PathBuf::from("<symlink>"));
    }
    if md.is_dir() {
        return Some(canon(&dot));
    }
    let text = std::fs::read_to_string(&dot).ok()?;
    let target = text.lines().next()?.strip_prefix("gitdir:")?.trim();
    let p = Path::new(target);
    Some(canon(&if p.is_absolute() {
        p.to_path_buf()
    } else {
        checkout.join(p)
    }))
}

/// Mark `checkout` as writable by a contained process: from now on git commands there run
/// hardened and verify its `.git`. Call while the checkout is still trusted (before the box
/// first runs).
pub fn register_contained_checkout(checkout: &Path) {
    let checkout = canon(checkout);
    let git_dir = current_git_dir(&checkout);
    let mut r = registry().lock().unwrap();
    r.retain(|c| c.checkout != checkout);
    r.push(Contained { checkout, git_dir });
}

/// Forget a checkout (task finished and its box gone).
pub fn unregister_contained_checkout(checkout: &Path) {
    let checkout = canon(checkout);
    registry()
        .lock()
        .unwrap()
        .retain(|c| c.checkout != checkout);
}

/// Is `dir` inside a registered contained checkout?
pub fn is_contained(dir: &Path) -> bool {
    contained_for(dir).is_some()
}

fn contained_for(dir: &Path) -> Option<Contained> {
    let d = canon(dir);
    registry()
        .lock()
        .unwrap()
        .iter()
        .find(|c| d.starts_with(&c.checkout))
        .cloned()
}

/// `-c filter.<name>.{clean,smudge,process}=` for every filter driver configured for `dir`
/// (git runs clean filters during `status`/`diff`). Reading config executes nothing.
fn filter_overrides(dir: &Path) -> Vec<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "config",
            "--null",
            "--name-only",
            "--get-regexp",
            r"^filter\.",
        ])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(out) = out else { return vec![] };
    let mut names: Vec<String> = out
        .stdout
        .split(|&b| b == 0)
        .filter_map(|k| {
            let k = String::from_utf8_lossy(k);
            let rest = k.strip_prefix("filter.")?;
            rest.rsplit_once('.').map(|(n, _)| n.to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    names
        .iter()
        .flat_map(|n| {
            ["clean", "smudge", "process"]
                .iter()
                .flat_map(move |k| ["-c".to_string(), format!("filter.{n}.{k}=")])
                .chain(["-c".to_string(), format!("filter.{n}.required=false")])
        })
        .collect()
}

/// The `-c` flags a git command in `dir` needs (plain, or hardened for a contained checkout),
/// or an error when the contained checkout's `.git` was swapped.
pub fn safety_args(dir: &Path) -> Result<Vec<String>> {
    let Some(c) = contained_for(dir) else {
        return Ok(vec!["-c".into(), "core.fsmonitor=false".into()]);
    };
    let now = current_git_dir(&c.checkout);
    if now != c.git_dir {
        return Err(Error::Refused(format!(
            "{}/.git no longer points at {} (now {}); a contained process may have replaced it — not running git there",
            c.checkout.display(),
            c.git_dir
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "nothing".into()),
            now.as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "nothing".into()),
        )));
    }
    let mut v: Vec<String> = HOST_HARDEN.iter().map(|s| s.to_string()).collect();
    v.extend(filter_overrides(dir));
    Ok(v)
}

pub(crate) fn command(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C")
        .arg(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C");
    c
}

pub(crate) fn exec(dir: &Path, args: &[&str], timeout: Option<Duration>) -> Result<Output> {
    let mut cmd = command(dir);
    cmd.args(safety_args(dir)?)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd.spawn()?;
    let pid = child.id();
    let out = match timeout {
        None => child.wait_with_output()?,
        Some(t) => {
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                let _ = tx.send(child.wait_with_output());
            });
            match rx.recv_timeout(t) {
                Ok(r) => r?,
                Err(_) => {
                    // SAFETY: plain kill(2) on a child we spawned.
                    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                    return Err(Error::Timeout {
                        args: args.join(" "),
                        secs: t.as_secs_f32(),
                    });
                }
            }
        }
    };
    if out.status.success() {
        Ok(out)
    } else {
        Err(Error::Git {
            args: args.join(" "),
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}

/// Run git, return stdout with trailing newline(s) trimmed.
pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<String> {
    git_timeout(dir, args, None)
}

pub(crate) fn git_timeout(dir: &Path, args: &[&str], t: Option<Duration>) -> Result<String> {
    let out = exec(dir, args, t)?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .trim_end_matches(['\n', '\r'])
        .to_string())
}

pub(crate) fn git_ok(dir: &Path, args: &[&str]) -> bool {
    exec(dir, args, None).is_ok()
}

pub(crate) fn ref_exists(dir: &Path, full_ref: &str) -> bool {
    git_ok(dir, &["rev-parse", "--verify", "-q", full_ref])
}

pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn g(dir: &Path, args: &[&str]) {
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
                "-c",
                "core.hooksPath=/dev/null",
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
    }

    fn script(path: &Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// 13 §6 / review findings 2 and 9: hostile configuration and a swapped `.git` in a
    /// contained checkout never make a host-side status run code.
    #[test]
    fn contained_checkouts_run_git_hardened_in_both_checkout_forms() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let marker = root.join("pwned");
        // Form 1: a regular checkout (`.git` is a directory the box could write in container
        // worktree mode): fsmonitor, a hooks dir inside the checkout and a clean filter.
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        g(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        g(&repo, &["add", "-A"]);
        g(&repo, &["commit", "-q", "-m", "base"]);
        // Form 2: a linked worktree (the task checkout).
        let wt = root.join("wt");
        g(
            &repo,
            &["worktree", "add", "-q", "-b", "task", wt.to_str().unwrap()],
        );
        register_contained_checkout(&repo);
        register_contained_checkout(&wt);
        let touch = root.join("touch.sh");
        script(&touch, &format!("touch {}", marker.display()));
        let t_str = touch.to_string_lossy().into_owned();
        g(&repo, &["config", "core.fsmonitor", &t_str]);
        g(&repo, &["config", "core.hooksPath", ".githooks"]);
        g(&repo, &["config", "filter.evil.clean", &t_str]);
        std::fs::write(repo.join(".gitattributes"), "*.txt filter=evil\n").unwrap();
        std::fs::write(wt.join(".gitattributes"), "*.txt filter=evil\n").unwrap();
        std::fs::write(repo.join("a.txt"), "changed\n").unwrap();
        std::fs::write(wt.join("a.txt"), "changed too\n").unwrap();
        for d in [&repo, &wt] {
            let s = crate::branch_status(d, Some("main")).unwrap();
            assert!(s.dirty_files > 0);
            let _ = crate::removal_blockers(d).unwrap();
        }
        assert!(!marker.exists(), "host status ran repo-configured code");

        // The box replaces the worktree's `.git` link with one naming a repo it controls
        // (inside the checkout), with an fsmonitor of its choosing.
        let evil = wt.join("evil-repo");
        std::fs::create_dir_all(&evil).unwrap();
        g(&evil, &["init", "-q"]);
        g(&evil, &["config", "core.fsmonitor", &t_str]);
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", evil.join(".git").display()),
        )
        .unwrap();
        let e = crate::branch_status(&wt, None).unwrap_err();
        assert!(e.to_string().contains("no longer points"), "{e}");
        assert!(!marker.exists());
        // A plain checkout replaced by a symlinked `.git` is refused the same way.
        std::fs::rename(repo.join(".git"), root.join("moved-git")).unwrap();
        std::os::unix::fs::symlink(evil.join(".git"), repo.join(".git")).unwrap();
        assert!(crate::branch_status(&repo, None).is_err());
        assert!(!marker.exists());
        // Unregistered checkouts keep running git normally (hooks etc. are the user's).
        unregister_contained_checkout(&repo);
        unregister_contained_checkout(&wt);
        assert!(!is_contained(&wt));
        assert_eq!(
            safety_args(&wt).unwrap(),
            ["-c".to_string(), "core.fsmonitor=false".to_string()]
        );
    }
}
