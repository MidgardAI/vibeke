//! Setup script runner (05 §7).

use crate::Result;
use crate::ports::Lease;
use std::fs::File;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
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

/// One setup step as the host would run it: argv and the full environment.
pub type Step = (Vec<String>, Vec<(String, String)>);

/// Turns a setup step into the command that runs it inside the task's isolation (the same
/// runner as the task's panes). The returned argv is spawned with exactly the returned
/// environment (nothing inherited).
#[derive(Clone)]
pub struct StepWrapper(pub Arc<dyn Fn(Step) -> std::io::Result<Step> + Send + Sync>);

impl std::fmt::Debug for StepWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StepWrapper")
    }
}

#[derive(Debug, Clone)]
pub struct SetupOptions {
    /// Run with this cwd.
    pub worktree: PathBuf,
    /// Script path, relative to `worktree`. Run as `sh <script>` (relative, from the
    /// worktree) after `commands`; an empty path or a missing file means no script.
    pub script: PathBuf,
    /// Shell commands (`sh -c`) run first, in order: the dependency install,
    /// then `setup.run`. The first failing step ends setup.
    pub commands: Vec<String>,
    pub task_id: String,
    pub lease: Option<Lease>,
    /// Extra environment (e.g. the `[env]` table), applied last.
    pub extra_env: Vec<(String, String)>,
    /// stdout+stderr are written here. The file is replaced, never followed: a symlink or
    /// hard link at this path is removed first and a new file is created.
    pub log_path: PathBuf,
    pub timeout: Option<Duration>,
    /// Run every step through this wrapper (a contained task's runner). `None` runs the steps
    /// on the host with the server's environment.
    pub wrap: Option<StepWrapper>,
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
    let mut log = create_log(&opts.log_path)?;
    let env: Vec<(String, String)> = setup_env(&opts.task_id, &opts.worktree, opts.lease.as_ref())
        .into_iter()
        .chain(opts.extra_env.iter().cloned())
        .collect();
    let mut steps: Vec<(String, Vec<String>)> = opts
        .commands
        .iter()
        .map(|c| (c.clone(), vec!["-c".to_string(), c.clone()]))
        .collect();
    if script.is_some() {
        // Relative to the worktree (the cwd): the same path inside a box that mounts the
        // checkout somewhere else.
        steps.push((
            format!("sh {}", opts.script.display()),
            vec![opts.script.to_string_lossy().into_owned()],
        ));
    }
    for (label, args) in steps {
        use std::io::Write;
        let _ = writeln!(log, "$ {label}");
        let mut cmd = match &opts.wrap {
            None => {
                let mut c = Command::new("sh");
                c.args(&args);
                for (k, v) in &env {
                    c.env(k, v);
                }
                c
            }
            Some(w) => {
                let argv: Vec<String> = std::iter::once("sh".to_string())
                    .chain(args.iter().cloned())
                    .collect();
                // The runner scrubs the host environment the same way it does for panes.
                let mut full: Vec<(String, String)> = std::env::vars()
                    .filter(|(k, _)| !env.iter().any(|(n, _)| n == k))
                    .collect();
                full.extend(env.iter().cloned());
                let (argv, wenv) = match (w.0)((argv, full)) {
                    Ok(x) => x,
                    Err(e) => {
                        let _ = writeln!(log, "setup cannot run in the task's isolation: {e}");
                        return Ok(done(SetupStatus::Failed { exit_code: None }));
                    }
                };
                let Some((prog, rest)) = argv.split_first() else {
                    return Ok(done(SetupStatus::Failed { exit_code: None }));
                };
                let mut c = Command::new(prog);
                c.args(rest).env_clear().envs(wenv);
                c
            }
        };
        cmd.current_dir(&opts.worktree)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log.try_clone()?))
            .process_group(0);
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

/// Create the setup log at `path` without following links: the parent directories are
/// created private (`0700`), an existing entry (file, symlink or hard link) is removed, and a
/// new `0600` file is created exclusively with `O_NOFOLLOW`.
pub fn create_log(path: &Path) -> Result<File> {
    if let Some(p) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(p)?;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(path)?)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(dir: &Path, log: PathBuf) -> SetupOptions {
        SetupOptions {
            worktree: dir.to_path_buf(),
            script: PathBuf::new(),
            commands: vec!["echo hi".into()],
            task_id: "t1".into(),
            lease: None,
            extra_env: vec![("EXTRA".into(), "yes".into())],
            log_path: log,
            timeout: Some(Duration::from_secs(20)),
            wrap: None,
        }
    }

    /// Review finding D1.5: a symlink planted at the log path is replaced, not followed.
    #[test]
    fn log_never_follows_a_planted_symlink() {
        let t = tempfile::tempdir().unwrap();
        let victim = t.path().join("victim");
        std::fs::write(&victim, "keep me").unwrap();
        let log = t.path().join("logs/setup.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&victim, &log).unwrap();
        let out = run_setup(&opts(t.path(), log.clone()), &CancelToken::new()).unwrap();
        assert_eq!(out.status, SetupStatus::Succeeded);
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me");
        assert!(
            !std::fs::symlink_metadata(&log)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(std::fs::read_to_string(&log).unwrap().contains("hi"));
    }

    /// Review finding D1.3: with a wrapper every step runs through it, with exactly the
    /// environment the wrapper returns.
    #[test]
    fn wrapped_steps_run_through_the_wrapper_with_its_env_only() {
        let t = tempfile::tempdir().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let s2 = seen.clone();
        let mut o = opts(t.path(), t.path().join("setup.log"));
        o.commands = vec!["echo \"extra=$EXTRA wrapped=$WRAPPED home=${HOME:-none}\"".into()];
        o.wrap = Some(StepWrapper(Arc::new(move |(argv, env): Step| {
            s2.lock().unwrap().push(argv.clone());
            assert!(env.iter().any(|(k, v)| k == "EXTRA" && v == "yes"));
            Ok((argv, vec![("WRAPPED".into(), "1".into())]))
        })));
        let out = run_setup(&o, &CancelToken::new()).unwrap();
        assert_eq!(out.status, SetupStatus::Succeeded);
        let log = std::fs::read_to_string(t.path().join("setup.log")).unwrap();
        assert!(log.contains("extra= wrapped=1 home=none"), "{log}");
        assert_eq!(seen.lock().unwrap()[0][0], "sh");
    }

    #[test]
    fn a_failing_wrapper_fails_setup_without_running_anything() {
        let t = tempfile::tempdir().unwrap();
        let mut o = opts(t.path(), t.path().join("setup.log"));
        o.commands = vec!["touch ran".into()];
        o.wrap = Some(StepWrapper(Arc::new(|_| {
            Err(std::io::Error::other("box is gone"))
        })));
        let out = run_setup(&o, &CancelToken::new()).unwrap();
        assert_eq!(out.status, SetupStatus::Failed { exit_code: None });
        assert!(!t.path().join("ran").exists());
        let log = std::fs::read_to_string(t.path().join("setup.log")).unwrap();
        assert!(log.contains("box is gone"));
    }
}
