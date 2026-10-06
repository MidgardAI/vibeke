//! PTY allocation and child spawning with precise control over setsid / controlling tty.

use anyhow::{Context, Result};
use rustix::fd::{AsFd, AsRawFd, OwnedFd};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{Winsize, tcsetwinsize};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};

pub struct Pty {
    pub master: OwnedFd,
}

pub fn set_size(fd: impl AsFd, cols: u16, rows: u16, px_w: u16, px_h: u16) -> Result<()> {
    tcsetwinsize(
        fd,
        Winsize {
            ws_row: rows.max(1),
            ws_col: cols.max(1),
            ws_xpixel: px_w,
            ws_ypixel: px_h,
        },
    )?;
    Ok(())
}

/// Spawn `argv` on a new PTY as session leader with the PTY as its controlling terminal.
pub fn spawn(
    argv: &[String],
    cwd: &Path,
    env: &[(String, String)],
    cols: u16,
    rows: u16,
) -> Result<(Pty, Child)> {
    anyhow::ensure!(!argv.is_empty(), "empty argv");
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).context("openpt")?;
    rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC)?;
    grantpt(&master).context("grantpt")?;
    unlockpt(&master).context("unlockpt")?;
    let name = ptsname(&master, Vec::new()).context("ptsname")?;
    let slave = rustix::fs::open(
        name.as_c_str(),
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY,
        rustix::fs::Mode::empty(),
    )
    .context("open slave")?;
    set_size(&slave, cols, rows, 0, 0)?;

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).current_dir(cwd).env_clear();
    for (k, v) in env {
        cmd.env(k, v);
    }
    let s_in: OwnedFd = slave.try_clone()?;
    let s_out: OwnedFd = slave.try_clone()?;
    cmd.stdin(Stdio::from(s_in))
        .stdout(Stdio::from(s_out))
        .stderr(Stdio::from(slave));
    let um = crate::child_umask();
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::tcsetpgrp(0, libc::getpid());
            // Restore default signal dispositions the holder may have changed.
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            libc::signal(libc::SIGCHLD, libc::SIG_DFL);
            libc::signal(libc::SIGHUP, libc::SIG_DFL);
            // The user's own umask, not the holder's 077 (09 §3.1).
            if let Some(m) = um {
                libc::umask(m as libc::mode_t);
            }
            Ok(())
        });
    }
    let child = cmd
        .spawn()
        .with_context(|| format!("spawn {:?}", argv[0]))?;
    // Master is non-blocking for the poll loop.
    rustix::fs::fcntl_setfl(&master, rustix::fs::OFlags::NONBLOCK)?;
    let _ = master.as_raw_fd();
    Ok((Pty { master }, child))
}

/// Pipe mode (01 §1.2): the child's stdin, stdout and stderr, holder side, non-blocking.
pub struct Pipes {
    pub stdin: OwnedFd,
    pub stdout: OwnedFd,
    pub stderr: OwnedFd,
}

/// Spawn `argv` with pipes instead of a PTY (headless harness over stdio). The child becomes
/// a session and process-group leader without a controlling terminal, so `FgPgrp` signals
/// reach its whole group and it never competes for the holder's (absent) terminal.
pub fn spawn_pipe(argv: &[String], cwd: &Path, env: &[(String, String)]) -> Result<(Pipes, Child)> {
    anyhow::ensure!(!argv.is_empty(), "empty argv");
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).current_dir(cwd).env_clear();
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let um = crate::child_umask();
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            libc::signal(libc::SIGCHLD, libc::SIG_DFL);
            libc::signal(libc::SIGHUP, libc::SIG_DFL);
            // The user's own umask, not the holder's 077 (09 §3.1).
            if let Some(m) = um {
                libc::umask(m as libc::mode_t);
            }
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {:?}", argv[0]))?;
    let stdin: OwnedFd = child.stdin.take().context("stdin pipe")?.into();
    let stdout: OwnedFd = child.stdout.take().context("stdout pipe")?.into();
    let stderr: OwnedFd = child.stderr.take().context("stderr pipe")?.into();
    for fd in [&stdin, &stdout, &stderr] {
        rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)?;
        rustix::fs::fcntl_setfl(fd, rustix::fs::OFlags::NONBLOCK)?;
    }
    Ok((
        Pipes {
            stdin,
            stdout,
            stderr,
        },
        child,
    ))
}

/// Foreground process group of the terminal, if any.
pub fn fg_pgrp(master: impl AsFd) -> Option<u32> {
    rustix::termios::tcgetpgrp(master)
        .ok()
        .map(|p| p.as_raw_nonzero().get() as u32)
}
