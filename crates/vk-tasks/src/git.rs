//! Thin wrapper around the `git` CLI.
//!
//! Every command runs with `core.fsmonitor=false` (Vibeke's own queries never need it, and it is
//! a program git would run). Checkouts that a contained process can write to (13 §6: sandboxed
//! task worktrees, container worktree-mode binds) are registered with
//! [`register_contained_checkout`]; git commands in them additionally run **hardened**
//! ([`HOST_HARDEN`] plus every configured filter driver neutralized), and refuse to run at all
//! when the checkout's `.git` no longer points at the git dir recorded at registration (a box
//! that swapped its `.git` file for one naming a repository it controls).
//!
//! Other crates that run git on a task checkout (review, orchestration) go through
//! [`harden_command`], so every hardening rule (and any environment a contained checkout needs)
//! lives here, in one place.

use crate::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
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
    // Never enter submodules: git would run commands with the submodule's own configuration.
    "-c",
    "diff.ignoreSubmodules=all",
    "-c",
    "status.submoduleSummary=false",
    "-c",
    "fetch.recurseSubmodules=false",
    "-c",
    "push.recurseSubmodules=no",
    "-c",
    "core.askPass=",
    "-c",
    "commit.gpgSign=false",
    "-c",
    "tag.gpgSign=false",
    "-c",
    "log.showSignature=false",
    "-c",
    "gpg.program=gpg",
    "-c",
    "gpg.ssh.program=ssh-keygen",
    "-c",
    "gpg.x509.program=gpgsm",
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

/// `-c` overrides that neutralize every filter, diff and merge driver configured for `dir`.
/// Git runs clean filters during `status`/`diff`, textconv and external diff drivers during
/// `diff`/`log -p`, and merge drivers during merges. Reading config executes nothing.
fn driver_overrides(dir: &Path) -> Vec<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "config",
            "--null",
            "--name-only",
            "--get-regexp",
            r"^(filter|diff|merge)\.",
        ])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(out) = out else { return vec![] };
    driver_overrides_for(&out.stdout)
}

/// [`driver_overrides`] for the NUL-separated key names `git config --name-only` printed.
fn driver_overrides_for(names: &[u8]) -> Vec<String> {
    let mut drivers: Vec<(String, String)> = names
        .split(|&b| b == 0)
        .filter_map(|k| {
            let k = String::from_utf8_lossy(k);
            let (section, rest) = k.split_once('.')?;
            // `diff.external` and other two-part keys name no driver.
            let (name, _) = rest.rsplit_once('.')?;
            Some((section.to_ascii_lowercase(), name.to_string()))
        })
        .collect();
    drivers.sort();
    drivers.dedup();
    let mut v = Vec::new();
    let mut set = |k: String| {
        v.push("-c".to_string());
        v.push(k);
    };
    for (section, n) in &drivers {
        match section.as_str() {
            "filter" => {
                for k in ["clean", "smudge", "process"] {
                    set(format!("filter.{n}.{k}="));
                }
                set(format!("filter.{n}.required=false"));
            }
            "diff" => {
                // `cat` prints the file unchanged: the same text as having no textconv.
                set(format!("diff.{n}.textconv=cat"));
                set(format!("diff.{n}.command="));
            }
            "merge" => {
                // `false` reports a conflict: the merge stops for a person to resolve.
                set(format!("merge.{n}.driver=false"));
            }
            _ => {}
        }
    }
    v
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
    v.extend(driver_overrides(dir));
    Ok(v)
}

/// Add the safety flags for `dir` to a `git` command (before its subcommand), plus whatever
/// a git command on that checkout needs. Every crate that runs git on a task checkout calls
/// this, so the rules stay in one place. Errors when a contained checkout's `.git` was swapped
/// (see [`safety_args`]).
///
/// When a sandboxed box works in the same repository, the objects it wrote to its private
/// object store are first imported into the repository's own store
/// ([`crate::register_box_objects`]), so the command sees the box's commits and anything it
/// writes (a merge, a pushed pack) never depends on the box's directory.
pub fn harden_command(cmd: &mut Command, dir: &Path) -> Result<()> {
    cmd.args(safety_args(dir)?);
    crate::box_objects::import_box_objects(dir);
    Ok(())
}

/// The user's umask for git commands that create checkout files (09 §3.1): the server runs
/// with `umask 077`, but worktree files belong to the user and keep the user's own mode bits.
/// `u32::MAX` = unset (inherit).
static CHILD_UMASK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(u32::MAX);

/// Set the umask git children run with (see [`CHILD_UMASK`]).
pub fn set_child_umask(mask: u32) {
    CHILD_UMASK.store(mask & 0o777, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn command(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C")
        .arg(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C");
    let m = CHILD_UMASK.load(std::sync::atomic::Ordering::Relaxed);
    if m != u32::MAX {
        use std::os::unix::process::CommandExt;
        // SAFETY: umask is async-signal-safe.
        unsafe {
            c.pre_exec(move || {
                libc::umask(m as libc::mode_t);
                Ok(())
            });
        }
    }
    c
}

pub(crate) fn exec(dir: &Path, args: &[&str], timeout: Option<Duration>) -> Result<Output> {
    let mut cmd = command(dir);
    harden_command(&mut cmd, dir)?;
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let out = match timeout {
        None => child.wait_with_output()?,
        Some(t) => wait_bounded(&mut child, t)?.ok_or_else(|| Error::Timeout {
            args: args.join(" "),
            secs: t.as_secs_f32(),
        })?,
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

/// Wait for `child` and collect its piped stdout and stderr, all within `t`. `None` on timeout.
///
/// The child stays owned (and unreaped) here until it exits or is killed, so the kill can never
/// hit a recycled pid: `Child::kill` only signals an unwaited child. The deadline also covers
/// output collection: a background descendant that inherited stdout/stderr can hold the pipes
/// open after the child exits; the reader threads are then left behind and end when the pipes
/// close.
fn wait_bounded(child: &mut std::process::Child, t: Duration) -> std::io::Result<Option<Output>> {
    let deadline = std::time::Instant::now() + t;
    let (tx, rx) = std::sync::mpsc::channel::<(bool, Vec<u8>)>();
    let drain = |r: Option<Box<dyn std::io::Read + Send>>, is_err: bool| {
        let tx = tx.clone();
        thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut r) = r {
                let _ = r.read_to_end(&mut buf);
            }
            let _ = tx.send((is_err, buf));
        });
    };
    drain(
        child
            .stdout
            .take()
            .map(|x| Box::new(x) as Box<dyn std::io::Read + Send>),
        false,
    );
    drain(
        child
            .stderr
            .take()
            .map(|x| Box::new(x) as Box<dyn std::io::Read + Send>),
        true,
    );
    drop(tx);
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(10));
    };
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    for _ in 0..2 {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok((false, b)) => stdout = b,
            Ok((true, b)) => stderr = b,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return Ok(None),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(Some(Output {
        status,
        stdout,
        stderr,
    }))
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

/// [`driver_overrides`] for a box repo whose config `config` lists (see `sync::box_git`).
pub(crate) fn driver_overrides_from_names(names: &[u8]) -> Vec<String> {
    driver_overrides_for(names)
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

    #[test]
    fn bounded_wait_collects_output_and_times_out_on_held_pipes() {
        let spawn = |script: &str| {
            Command::new("sh")
                .args(["-c", script])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        };
        let mut c = spawn("echo out; echo err >&2");
        let o = wait_bounded(&mut c, Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert!(o.status.success());
        assert_eq!(o.stdout, b"out\n");
        assert_eq!(o.stderr, b"err\n");
        // The child exits at once, but a background descendant keeps its pipes open.
        let mut c = spawn("sleep 5 & echo hi");
        let t0 = std::time::Instant::now();
        assert!(
            wait_bounded(&mut c, Duration::from_millis(300))
                .unwrap()
                .is_none()
        );
        assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
    }

    #[test]
    fn driver_overrides_neutralize_filters_diff_and_merge_drivers() {
        let names = b"filter.lfs.clean\0filter.lfs.smudge\0diff.evil.textconv\0diff.evil.command\0merge.ours.driver\0diff.external\0";
        let v = driver_overrides_for(names);
        let has = |k: &str| v.iter().any(|x| x == k);
        assert!(has("filter.lfs.clean="));
        assert!(has("filter.lfs.process="));
        assert!(has("filter.lfs.required=false"));
        assert!(has("diff.evil.textconv=cat"));
        assert!(has("diff.evil.command="));
        assert!(has("merge.ours.driver=false"));
        // `diff.external` is not a driver (HOST_HARDEN clears it).
        assert!(!v.iter().any(|x| x.starts_with("diff.external.")));
        // Each override is preceded by `-c`, once per driver.
        assert_eq!(
            v.iter().filter(|x| *x == "diff.evil.textconv=cat").count(),
            1
        );
        assert_eq!(v.iter().filter(|x| *x == "-c").count(), v.len() / 2);
    }

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
        g(&repo, &["config", "diff.evil.textconv", &t_str]);
        g(&repo, &["config", "diff.evil.command", &t_str]);
        let attrs = "*.txt filter=evil diff=evil\n";
        std::fs::write(repo.join(".gitattributes"), attrs).unwrap();
        std::fs::write(wt.join(".gitattributes"), attrs).unwrap();
        std::fs::write(repo.join("a.txt"), "changed\n").unwrap();
        std::fs::write(wt.join("a.txt"), "changed too\n").unwrap();
        for d in [&repo, &wt] {
            let s = crate::branch_status(d, Some("main")).unwrap();
            assert!(s.dirty_files > 0);
            let _ = crate::removal_blockers(d).unwrap();
            // Textconv and external diff drivers are neutralized too.
            let _ = git(d, &["diff"]);
            let _ = git(d, &["log", "-p", "-1"]);
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
