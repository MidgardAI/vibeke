//! Read-only `git` CLI wrapper with a timeout. Never used for destructive operations on the
//! user's checkout (no stash/reset/checkout/clean).

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Default timeout for a single git invocation.
pub(crate) const GIT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git {args}: {source}")]
    Spawn {
        args: String,
        #[source]
        source: std::io::Error,
    },
    #[error("git {args} timed out after {secs:.1}s")]
    Timeout { args: String, secs: f32 },
    #[error("git {args} exited with {code:?}: {stderr}")]
    Failed {
        args: String,
        code: Option<i32>,
        stderr: String,
    },
}

pub(crate) struct GitOutput {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub(crate) fn command(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C")
        .arg(dir)
        // Never run the user's hooks or prompt for credentials from a background capture.
        .args(["-c", "core.hooksPath=/dev/null"])
        .env("GIT_TERMINAL_PROMPT", "0")
        // Avoid taking index.lock for opportunistic refreshes during `git status`.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C");
    c
}

/// Run git and return its output whatever the exit status.
pub(crate) fn run_raw(dir: &Path, args: &[&str], timeout: Duration) -> Result<GitOutput, GitError> {
    let joined = args.join(" ");
    let mut child = command(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| GitError::Spawn {
            args: joined.clone(),
            source,
        })?;
    let pid = child.id();
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let err_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let status = child.wait();
        let _ = tx.send((buf, status));
    });
    match rx.recv_timeout(timeout) {
        Ok((out, status)) => {
            let status = status.map_err(|source| GitError::Spawn {
                args: joined.clone(),
                source,
            })?;
            let err = err_thread.join().unwrap_or_default();
            Ok(GitOutput {
                code: status.code(),
                stdout: out,
                stderr: err,
            })
        }
        Err(_) => {
            // SAFETY: plain kill(2) on a child we spawned and have not reaped.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            Err(GitError::Timeout {
                args: joined,
                secs: timeout.as_secs_f32(),
            })
        }
    }
}

/// Run git, require success, return raw stdout.
pub(crate) fn run_bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>, GitError> {
    let out = run_raw(dir, args, GIT_TIMEOUT)?;
    if out.code == Some(0) {
        Ok(out.stdout)
    } else {
        Err(GitError::Failed {
            args: args.join(" "),
            code: out.code,
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}

/// Run git, require success, return stdout trimmed of trailing newlines.
pub(crate) fn run(dir: &Path, args: &[&str]) -> Result<String, GitError> {
    let out = run_bytes(dir, args)?;
    Ok(String::from_utf8_lossy(&out)
        .trim_end_matches(['\n', '\r'])
        .to_string())
}
