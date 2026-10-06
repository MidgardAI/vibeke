//! Setup script runner (05 §7).

use crate::Result;
use crate::ports::Lease;
use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Default `tasks.setup_script`.
pub const DEFAULT_SETUP_SCRIPT: &str = ".vibeke/setup.sh";

/// Cooperative cancellation flag shared with a running setup.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone)]
pub struct SetupOptions {
    /// Run with this cwd.
    pub worktree: PathBuf,
    /// Script path, relative to `worktree` (or absolute). Run as `sh <script>`
    /// after `commands`; an empty path or a missing file means no script.
    pub script: PathBuf,
    /// Shell commands (`sh -c`) run first, in order: the dependency install,
    /// then `setup.run`. The first failing step ends setup.
    pub commands: Vec<String>,
    pub task_id: String,
    pub lease: Option<Lease>,
    /// Extra environment (e.g. the `[env]` table), applied last.
    pub extra_env: Vec<(String, String)>,
    /// stdout+stderr are written here (truncated first).
    pub log_path: PathBuf,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupStatus {
    /// There is nothing to run (no commands, no script); nothing ran.
    Skipped,
    Succeeded,
    Failed {
        exit_code: Option<i32>,
    },
    TimedOut,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct SetupOutcome {
    pub status: SetupStatus,
    pub duration: Duration,
    pub log_path: PathBuf,
}

/// Environment injected into setup (and, by the caller, into task panes).
pub fn setup_env(
    task_id: &str,
    worktree: &std::path::Path,
    lease: Option<&Lease>,
) -> Vec<(String, String)> {
    let mut env = vec![
        ("VIBEKE_SETUP".to_string(), "1".to_string()),
        ("VIBEKE_TASK".to_string(), task_id.to_string()),
        ("VIBEKE_TASK_ID".to_string(), task_id.to_string()),
        (
            "VIBEKE_WORKTREE".to_string(),
            worktree.to_string_lossy().into_owned(),
        ),
    ];
    if let Some(l) = lease {
        env.push(("VIBEKE_PORT_BASE".into(), l.start.to_string()));
        env.push(("VIBEKE_PORT_END".into(), l.end.to_string()));
        env.push(("VIBEKE_PORT_COUNT".into(), l.count().to_string()));
        for i in 0..l.count() {
            env.push((format!("VIBEKE_PORT_{i}"), (l.start + i).to_string()));
        }
        env.push(("PORT".into(), l.start.to_string()));
    }
    env
}

fn signal_group(pid: u32, sig: libc::c_int) {
    // SAFETY: signalling the process group we created via process_group(0).
    unsafe { libc::kill(-(pid as libc::pid_t), sig) };
}

/// Run the setup script to completion (blocking), honouring the timeout and
/// `cancel`. The whole process group is terminated (SIGTERM, then SIGKILL
/// after 2 s) on timeout or cancellation.
pub fn run_setup(opts: &SetupOptions, cancel: &CancelToken) -> Result<SetupOutcome> {
    let script = (!opts.script.as_os_str().is_empty())
        .then(|| opts.worktree.join(&opts.script))
        .filter(|p| p.is_file());
    let start = Instant::now();
    let done = |status| SetupOutcome {
        status,
        duration: start.elapsed(),
        log_path: opts.log_path.clone(),
    };
    if script.is_none() && opts.commands.is_empty() {
        return Ok(done(SetupStatus::Skipped));
    }
    if let Some(p) = opts.log_path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let mut log = File::create(&opts.log_path)?;
    let env: Vec<(String, String)> = setup_env(&opts.task_id, &opts.worktree, opts.lease.as_ref())
        .into_iter()
        .chain(opts.extra_env.iter().cloned())
        .collect();
    let mut steps: Vec<(String, Vec<String>)> = opts
        .commands
        .iter()
        .map(|c| (c.clone(), vec!["-c".to_string(), c.clone()]))
        .collect();
    if let Some(sc) = &script {
        steps.push((
            format!("sh {}", opts.script.display()),
            vec![sc.to_string_lossy().into_owned()],
        ));
    }
    for (label, args) in steps {
        use std::io::Write;
        let _ = writeln!(log, "$ {label}");
        let mut cmd = Command::new("sh");
        cmd.args(&args)
            .current_dir(&opts.worktree)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log.try_clone()?))
            .process_group(0);
        for (k, v) in &env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn()?;
        let pid = child.id();
        loop {
            if let Some(st) = child.try_wait()? {
                if st.success() {
                    break;
                }
                return Ok(done(SetupStatus::Failed {
                    exit_code: st.code(),
                }));
            }
            let timed_out = opts.timeout.is_some_and(|t| start.elapsed() >= t);
            if timed_out || cancel.is_cancelled() {
                signal_group(pid, libc::SIGTERM);
                let grace = Instant::now();
                while child.try_wait()?.is_none() {
                    if grace.elapsed() > Duration::from_secs(2) {
                        signal_group(pid, libc::SIGKILL);
                        child.wait()?;
                        break;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                // Reap any stragglers in the group.
                signal_group(pid, libc::SIGKILL);
                return Ok(done(if cancel.is_cancelled() && !timed_out {
                    SetupStatus::Cancelled
                } else {
                    SetupStatus::TimedOut
                }));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
    Ok(done(SetupStatus::Succeeded))
}

/// A setup running on a background thread.
pub struct SetupHandle {
    cancel: CancelToken,
    join: JoinHandle<Result<SetupOutcome>>,
}

impl SetupHandle {
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
    pub fn is_finished(&self) -> bool {
        self.join.is_finished()
    }
    pub fn wait(self) -> Result<SetupOutcome> {
        self.join
            .join()
            .unwrap_or_else(|_| Err(std::io::Error::other("setup thread panicked").into()))
    }
}

/// Run setup on a background thread.
pub fn spawn_setup(opts: SetupOptions) -> SetupHandle {
    let cancel = CancelToken::new();
    let c = cancel.clone();
    SetupHandle {
        cancel,
        join: thread::spawn(move || run_setup(&opts, &c)),
    }
}
