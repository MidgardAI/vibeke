//! Disk-bounded invocation output (07 §7.7 limits, 09 §6).
//!
//! A plugin process never gets a plain file as its stdout/stderr. Its streams are pipes read by
//! a small capture helper (`vibeke compat plugin-output <budget> <out> <err>`, the stdout pipe on
//! fd 0 and the stderr pipe on fd 3) that appends to the invocation's 0600 output files until the
//! invocation's byte budget (`[plugins] output_max_bytes`, both streams together) is spent, then
//! writes one marker line per stream and discards the rest while still draining the pipes (so
//! the plugin never blocks or gets `SIGPIPE`). The helper is its own process, not the server: a
//! long-running invocation keeps writing across a server restart exactly as before, and the
//! server keeps tailing the (now bounded) files.

use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The line written once to a stream when the invocation's output budget is spent.
pub fn marker(budget: u64) -> String {
    format!(
        "\n[vibeke: output budget of {budget} bytes for this invocation exhausted; further output discarded]\n"
    )
}

/// Copy `r` to `w` while `left` (shared by both streams) lasts; past it write `marker` once and
/// discard the rest until EOF. Returns the bytes discarded.
pub fn copy_capped(
    mut r: impl Read,
    mut w: impl Write,
    left: &AtomicU64,
    marker: &str,
) -> io::Result<u64> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut marked = false;
    let mut discarded = 0u64;
    let mut writable = true;
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        // Claim up to `n` bytes of the shared budget.
        let mut take = 0u64;
        let _ = left.try_update(Ordering::SeqCst, Ordering::SeqCst, |l| {
            take = l.min(n as u64);
            Some(l - take)
        });
        let take = take as usize;
        if take > 0 && writable && w.write_all(&buf[..take]).is_err() {
            // The file is gone or the disk is full: keep draining, write nothing more.
            writable = false;
        }
        if take < n {
            discarded += (n - take) as u64;
            if !marked {
                marked = true;
                if writable {
                    let _ = w.write_all(marker.as_bytes());
                }
            }
        }
    }
    let _ = w.flush();
    Ok(discarded)
}

fn append(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)
}

/// `vibeke compat plugin-output <budget> <out file> <err file>`: stdout pipe on fd 0, stderr
/// pipe on fd 3. Exits when both pipes are closed.
pub fn main(args: &[String]) -> i32 {
    let [budget, out, err] = args else {
        eprintln!("vibeke compat plugin-output <budget> <out> <err>");
        return 2;
    };
    let Ok(budget) = budget.parse::<u64>() else {
        return 2;
    };
    use std::os::fd::FromRawFd;
    // SAFETY: the server passes the stderr pipe as fd 3 and nothing else in this process owns
    // it; fd 0 is the stdout pipe.
    let err_pipe = unsafe { std::fs::File::from_raw_fd(3) };
    let left = Arc::new(AtomicU64::new(budget));
    let m = marker(budget);
    let (out_f, err_f) = (append(Path::new(out)), append(Path::new(err)));
    let t = {
        let left = left.clone();
        let m = m.clone();
        std::thread::spawn(move || match err_f {
            Ok(f) => copy_capped(err_pipe, f, &left, &m),
            Err(_) => copy_capped(err_pipe, io::sink(), &left, &m),
        })
    };
    let stdin = io::stdin().lock();
    let _ = match out_f {
        Ok(f) => copy_capped(stdin, f, &left, &m),
        Err(_) => copy_capped(stdin, io::sink(), &left, &m),
    };
    let _ = t.join();
    0
}

/// The capture helper of one invocation: the write ends to hand to the plugin as its stdout
/// and stderr, and a receiver that completes when the helper has written everything (both
/// pipes closed by every writer).
pub struct Capture {
    pub stdout: std::io::PipeWriter,
    pub stderr: std::io::PipeWriter,
    pub done: tokio::sync::oneshot::Receiver<()>,
}

/// Start the capture helper for one invocation.
pub fn spawn(bin: &Path, budget: u64, out: &Path, err: &Path) -> io::Result<Capture> {
    use std::os::fd::AsRawFd;
    let (out_r, out_w) = std::io::pipe()?;
    let (err_r, err_w) = std::io::pipe()?;
    let err_fd = err_r.as_raw_fd();
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(["compat", "plugin-output", &budget.to_string()])
        .arg(out)
        .arg(err)
        .env_clear()
        .stdin(std::process::Stdio::from(out_r))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0);
    // SAFETY: only async-signal-safe calls (dup2/fcntl) between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            if err_fd == 3 {
                // Already in place: just let it survive the exec.
                if libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
            } else if libc::dup2(err_fd, 3) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    drop(err_r);
    drop(cmd);
    let (tx, done) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _ = child.wait().await;
        let _ = tx.send(());
    });
    Ok(Capture {
        stdout: out_w,
        stderr: err_w,
        done,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_is_shared_and_the_rest_is_drained() {
        let left = AtomicU64::new(1000);
        let m = marker(1000);
        let mut out = Vec::new();
        let d = copy_capped(&vec![b'a'; 600][..], &mut out, &left, &m).unwrap();
        assert_eq!((d, out.len()), (0, 600));
        let mut err = Vec::new();
        let d = copy_capped(&vec![b'b'; 5000][..], &mut err, &left, &m).unwrap();
        assert_eq!(d, 4600, "everything past the shared budget is discarded");
        assert_eq!(err.len(), 400 + m.len());
        assert!(String::from_utf8_lossy(&err).ends_with(&m));
        // Once spent, a stream gets the marker and nothing else.
        let mut more = Vec::new();
        copy_capped(&b"xyz"[..], &mut more, &left, &m).unwrap();
        assert_eq!(more, m.as_bytes());
    }

    #[test]
    fn sustained_output_through_a_pipe_stays_bounded() {
        // A writer producing far more than the budget is fully drained (it never blocks) while
        // the file stays at the budget plus one marker.
        let (r, mut w) = std::io::pipe().unwrap();
        let writer = std::thread::spawn(move || {
            let chunk = vec![b'x'; 64 * 1024];
            for _ in 0..(64 * 16) {
                w.write_all(&chunk).unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out");
        let f = append(&path).unwrap();
        let budget = 256 * 1024;
        let left = AtomicU64::new(budget);
        let d = copy_capped(r, f, &left, &marker(budget)).unwrap();
        writer.join().unwrap();
        assert_eq!(d, 64 * 1024 * 64 * 16 - budget);
        let len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(len, budget + marker(budget).len() as u64);
    }
}
