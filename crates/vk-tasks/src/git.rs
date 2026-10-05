//! Thin wrapper around the `git` CLI.

use crate::{Error, Result};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

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
    cmd.args(args)
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
