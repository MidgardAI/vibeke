//! Read-only `git` CLI wrapper with a timeout. Never used for destructive operations on the
//! user's checkout (no stash/reset/checkout/clean).

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

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
    /// Git was not run: the checkout is not safe to run it in (see `vk_tasks::safety_args`).
    #[error("{0}")]
    Refused(String),
}

pub(crate) struct GitOutput {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// A `git -C <dir>` command with Vibeke's hardening applied.
///
/// Hooks never run. In a checkout a contained process can write (sandbox, container, VM or
/// cloud task), the shared hardening from `vk_tasks::harden_command` also applies: no fsmonitor,
/// filter/diff/merge drivers neutralized, no submodule recursion, and git refuses to run when
/// the checkout's `.git` was swapped. Host checkouts keep the user's own configuration (LFS
/// filters, fsmonitor speedups).
///
/// The command starts in its own process group, so a timeout can stop git and every helper it
/// started.
pub(crate) fn command(dir: &Path) -> Result<Command, GitError> {
    use std::os::unix::process::CommandExt;
    let mut c = Command::new("git");
    c.arg("-C").arg(dir);
    vk_tasks::harden_command(&mut c, dir).map_err(|e| GitError::Refused(e.to_string()))?;
    // Never run the user's hooks or prompt for credentials from a background capture.
    c.args(["-c", "core.hooksPath=/dev/null"])
        .env("GIT_TERMINAL_PROMPT", "0")
        // Avoid taking index.lock for opportunistic refreshes during `git status`.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .process_group(0);
    Ok(c)
}

/// Run git and return its output whatever the exit status.
pub(crate) fn run_raw(dir: &Path, args: &[&str], timeout: Duration) -> Result<GitOutput, GitError> {
    run_raw_env(dir, args, &[], timeout)
}

/// [`run_raw`] with extra environment variables (e.g. a private `GIT_INDEX_FILE`).
pub(crate) fn run_raw_env(
    dir: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    timeout: Duration,
) -> Result<GitOutput, GitError> {
    let joined = args.join(" ");
    let mut child = command(dir)?
        .args(args)
        .envs(envs.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| GitError::Spawn {
            args: joined.clone(),
            source,
        })?;
    let spawn_err = |source| GitError::Spawn {
        args: joined.clone(),
        source,
    };
    let (tx, rx) = mpsc::channel::<(bool, Vec<u8>)>();
    for (is_err, r) in [
        (
            false,
            child
                .stdout
                .take()
                .map(|x| Box::new(x) as Box<dyn Read + Send>),
        ),
        (
            true,
            child
                .stderr
                .take()
                .map(|x| Box::new(x) as Box<dyn Read + Send>),
        ),
    ] {
        let tx = tx.clone();
        thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut r) = r {
                let _ = r.read_to_end(&mut buf);
            }
            let _ = tx.send((is_err, buf));
        });
    }
    drop(tx);
    let deadline = Instant::now() + timeout;
    let timed_out = || GitError::Timeout {
        args: joined.clone(),
        secs: timeout.as_secs_f32(),
    };
    // The child stays owned and unreaped here until it exits or is killed: the group kill
    // below can never hit a recycled pid.
    let status = loop {
        if let Some(st) = child.try_wait().map_err(spawn_err)? {
            break st;
        }
        if Instant::now() >= deadline {
            kill_group(&mut child);
            return Err(timed_out());
        }
        thread::sleep(Duration::from_millis(5));
    };
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    for _ in 0..2 {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok((false, b)) => stdout = b,
            Ok((true, b)) => stderr = b,
            // A helper git started still holds the pipes open after git exited. Git is reaped,
            // so its pid (and group id) may be reused: nothing is killed. The reader threads
            // end when the pipes close.
            Err(mpsc::RecvTimeoutError::Timeout) => return Err(timed_out()),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(GitOutput {
        code: status.code(),
        stdout,
        stderr,
    })
}

/// Kill the process group of a child that is still unreaped, then reap it. The group id is the
/// child's pid (`process_group(0)`), and that id cannot be reused while the child is unreaped.
pub(crate) fn kill_group(child: &mut std::process::Child) {
    let pid = child.id() as libc::pid_t;
    // SAFETY: plain killpg(2) on the group of a child we spawned and have not reaped.
    unsafe { libc::killpg(pid, libc::SIGKILL) };
    let _ = child.kill();
    let _ = child.wait();
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

/// Run git with extra environment, require success, return trimmed stdout.
pub(crate) fn run_env(
    dir: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
) -> Result<String, GitError> {
    let out = run_raw_env(dir, args, envs, Duration::from_secs(300))?;
    if out.code == Some(0) {
        Ok(String::from_utf8_lossy(&out.stdout)
            .trim_end_matches(['\n', '\r'])
            .to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_are_always_off_and_host_checkouts_keep_their_config() {
        let t = tempfile::tempdir().unwrap();
        let c = command(t.path()).unwrap();
        let args: Vec<String> = c
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|a| a == "core.hooksPath=/dev/null"));
        // Not a contained checkout: only the always-on fsmonitor override, no driver overrides.
        assert!(args.iter().any(|a| a == "core.fsmonitor=false"));
        assert!(!args.iter().any(|a| a == "diff.ignoreSubmodules=all"));
    }

    #[test]
    fn timeout_kills_the_whole_process_group() {
        // A git alias that starts a long-running helper: the timeout must stop the helper too.
        let t = tempfile::tempdir().unwrap();
        let dir = t.path();
        let marker = dir.join("survived");
        let alias = format!(
            "alias.slow=!sh -c 'sleep 3; touch {}' && sleep 30",
            marker.display()
        );
        let t0 = Instant::now();
        let r = run_raw(dir, &["-c", &alias, "slow"], Duration::from_millis(300));
        assert!(matches!(r, Err(GitError::Timeout { .. })), "{:?}", r.err());
        assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
        // Wait past the helper's own sleep: it was killed with the group, so no marker.
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until {
            thread::sleep(Duration::from_millis(100));
        }
        assert!(!marker.exists(), "a helper outlived the timeout");
    }
}
