//! `vibeke hold`: the per-pane holder process (01 §1.2). It owns the PTY master and is the
//! parent of the pane's child process, journals output in a ring with markers, and lets a
//! (re)started server re-acquire the pane. Deliberately small, with no async runtime.

pub mod holder;
pub mod procinfo;
pub mod pty;
pub mod ring;
pub mod scan;

use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::Path;
use std::process::{Command, Stdio};
use vk_proto::holder::SpawnSpec;

/// Entry point for `vibeke hold --spec <file>`: double-fork, `setsid`, spawn the child,
/// report `ready <holder_pid> <child_pid>` on stdout from the intermediate process, then run.
pub fn main_daemon(spec_path: &Path, log_path: Option<&Path>) -> Result<()> {
    close_inherited_fds();
    let (rd, wr) = rustix::pipe::pipe()?;
    // SAFETY: called at process start before any threads exist.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        bail!("fork failed");
    }
    if pid > 0 {
        // Intermediate: relay the readiness line and exit so the server can reap us.
        drop(wr);
        let f = std::fs::File::from(rd);
        let mut line = String::new();
        BufReader::new(f).read_line(&mut line)?;
        if line.starts_with("ready ") {
            print!("{line}");
            std::io::stdout().flush()?;
            std::process::exit(0);
        }
        eprintln!("holder failed: {line}");
        std::process::exit(1);
    }
    drop(rd);
    // SAFETY: new session so the holder outlives the server's process group and terminal.
    unsafe { libc::setsid() };
    redirect_stdio(log_path);
    let mut ready = std::fs::File::from(wr);
    let result = (|| -> Result<holder::Holder> {
        let spec = holder::read_spec(spec_path)?;
        let listener = holder::bind_socket(Path::new(&spec.socket))?;
        holder::Holder::new(spec, listener)
    })();
    match result {
        Ok(h) => {
            writeln!(ready, "ready {} {}", std::process::id(), h.child_pid())?;
            drop(ready);
            h.run()
        }
        Err(e) => {
            writeln!(ready, "error {e:#}")?;
            Err(e)
        }
    }
}

/// A long-lived holder must not keep descriptors it inherited by accident. On macOS a
/// spawning process creates pipes non-atomically (`pipe` then `FD_CLOEXEC`), so a holder
/// launched concurrently from another thread can inherit the write end of some unrelated
/// `Command::output()` pipe and keep that caller waiting for EOF forever.
fn close_inherited_fds() {
    // SAFETY: getrlimit writes to the out-param; closing fds we don't own is the point and
    // happens at process start before any are opened.
    let mut rl = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let max = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } == 0 {
        rl.rlim_cur.min(8192) as i32
    } else {
        1024
    };
    for fd in 3..max {
        unsafe { libc::close(fd) };
    }
}

fn redirect_stdio(log: Option<&Path>) {
    let devnull = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null");
    let logf = log.and_then(|p| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .ok()
    });
    if let Ok(n) = &devnull {
        // SAFETY: dup2 onto the standard fds.
        unsafe { libc::dup2(n.as_raw_fd(), 0) };
    }
    let out = logf
        .as_ref()
        .map(|f| f.as_raw_fd())
        .or(devnull.as_ref().ok().map(|f| f.as_raw_fd()));
    if let Some(fd) = out {
        // SAFETY: as above.
        unsafe {
            libc::dup2(fd, 1);
            libc::dup2(fd, 2);
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Launched {
    pub holder_pid: u32,
    pub child_pid: u32,
}

/// Server side: write the spec file and run `<bin> hold --spec <file>`, returning once the
/// holder is listening. `hold_args` lets tests and the binary choose the subcommand spelling.
pub fn launch(
    bin: &Path,
    hold_args: &[&str],
    spec: &SpawnSpec,
    spec_dir: &Path,
    log: Option<&Path>,
) -> Result<Launched> {
    std::fs::create_dir_all(spec_dir)?;
    let spec_path = spec_dir.join(format!(
        "spawn-{}-{}.bin",
        spec.pane_id,
        rand::random::<u32>()
    ));
    holder::write_spec(&spec_path, spec)?;
    let mut cmd = Command::new(bin);
    cmd.args(hold_args).arg("--spec").arg(&spec_path);
    if let Some(l) = log {
        cmd.arg("--log").arg(l);
    }
    let out = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("spawn holder")?;
    let _ = std::fs::remove_file(&spec_path);
    let line = String::from_utf8_lossy(&out.stdout);
    let mut it = line.split_whitespace();
    if it.next() != Some("ready") {
        bail!(
            "holder did not start: {}{}",
            line.trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let holder_pid = it
        .next()
        .and_then(|s| s.parse().ok())
        .context("holder pid")?;
    let child_pid = it
        .next()
        .and_then(|s| s.parse().ok())
        .context("child pid")?;
    Ok(Launched {
        holder_pid,
        child_pid,
    })
}

/// Is the holder behind `socket` still running? Its socket accepting a connection is proof;
/// otherwise the recorded holder pid must exist *and* still look like a holder (`--spec` in
/// its argv), so a pid reused after a reboot isn't mistaken for a live holder.
pub fn holder_alive(socket: &Path, holder_pid: Option<u32>) -> bool {
    if std::os::unix::net::UnixStream::connect(socket).is_ok() {
        return true;
    }
    let Some(pid) = holder_pid.filter(|p| *p > 0) else {
        return false;
    };
    // SAFETY: signal 0 only checks existence.
    if unsafe { libc::kill(pid as i32, 0) } != 0 {
        return false;
    }
    procinfo::argv(pid).iter().any(|a| a == "--spec")
}

/// Used by tests: a std::fs::File from a raw fd we own.
#[doc(hidden)]
pub fn file_from_fd(fd: i32) -> std::fs::File {
    // SAFETY: caller passes ownership of a valid fd.
    unsafe { std::fs::File::from_raw_fd(fd) }
}
