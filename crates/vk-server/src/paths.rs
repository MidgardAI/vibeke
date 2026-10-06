//! Directory layout (01 §5). Every path can be overridden by env for tests and side-by-side
//! sessions (`VIBEKE_RUNTIME_DIR`, `VIBEKE_STATE_DIR`).

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Paths {
    pub session: String,
    /// `$XDG_RUNTIME_DIR/vibeke/<session>` (macOS: `$TMPDIR/vibeke-$UID/<session>`).
    pub runtime: PathBuf,
    /// `~/.local/state/vibeke/<session>`.
    pub state: PathBuf,
}

/// The exclusive-writer lock of a session's state dir: `flock(LOCK_EX)` on
/// `<state>/state.lock`, held by a running server for its whole life and by
/// `vibeke doctor --rebuild-index` while it rebuilds, so the two never write the archive and its
/// index at the same time (02). Released when dropped or when the process exits (the
/// descriptor is close-on-exec, so holders and plugins never inherit it).
#[derive(Debug)]
pub struct StateLock {
    _file: std::fs::File,
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn runtime_root() -> PathBuf {
    if let Some(d) = std::env::var_os("VIBEKE_RUNTIME_DIR") {
        return PathBuf::from(d);
    }
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(d).join("vibeke");
    }
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let tmp = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    tmp.join(format!("vibeke-{uid}"))
}

pub fn state_root() -> PathBuf {
    if let Some(d) = std::env::var_os("VIBEKE_STATE_DIR") {
        return PathBuf::from(d);
    }
    if let Some(d) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(d).join("vibeke");
    }
    home().join(".local/state/vibeke")
}

pub fn data_root() -> PathBuf {
    if let Some(d) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(d).join("vibeke");
    }
    home().join(".local/share/vibeke")
}

impl Paths {
    pub fn new(session: &str) -> Self {
        Paths {
            session: session.to_string(),
            runtime: runtime_root().join(session),
            state: state_root().join(session),
        }
    }
    pub fn socket(&self) -> PathBuf {
        self.runtime.join("vibeke.sock")
    }
    pub fn holders(&self) -> PathBuf {
        self.runtime.join("holders")
    }
    pub fn holder_socket(&self, pane: &str) -> PathBuf {
        self.holders().join(format!("{pane}.sock"))
    }
    pub fn db(&self) -> PathBuf {
        self.state.join("state.db")
    }
    pub fn logs(&self) -> PathBuf {
        self.state.join("logs")
    }
    pub fn scrollback(&self) -> PathBuf {
        self.state.join("scrollback")
    }
    pub fn blobs(&self) -> PathBuf {
        self.state.join("blobs")
    }
    pub fn pidfile(&self) -> PathBuf {
        self.runtime.join("server.pid")
    }
    /// Exclusive-writer lock of the session's state dir (see [`StateLock`]).
    pub fn state_lock(&self) -> PathBuf {
        self.state.join("state.lock")
    }
    /// Take the state dir's exclusive-writer lock without waiting; `Ok(None)` when another
    /// process holds it.
    pub fn try_lock_state(&self) -> std::io::Result<Option<StateLock>> {
        use std::os::fd::AsRawFd;
        std::fs::create_dir_all(&self.state)?;
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.state_lock())?;
        // SAFETY: flock on a descriptor we own; the lock lives as long as the file is open.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(StateLock { _file: f }));
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(None)
        } else {
            Err(e)
        }
    }
    /// [`Paths::try_lock_state`], retrying for up to `wait`.
    pub fn lock_state(&self, wait: std::time::Duration) -> std::io::Result<Option<StateLock>> {
        let t0 = std::time::Instant::now();
        loop {
            if let Some(l) = self.try_lock_state()? {
                return Ok(Some(l));
            }
            if t0.elapsed() >= wait {
                return Ok(None);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    /// Pane inbox for translated drops/pastes (06 A11.4): `$XDG_STATE_HOME/vibeke/inbox`.
    pub fn inbox() -> PathBuf {
        state_root().join("inbox")
    }
    /// Directory of PATH shims prepended in panes (04 §6.2).
    pub fn shims() -> PathBuf {
        data_root().join("shims")
    }

    /// Create runtime/state dirs with 0700 permissions (09 §3.1).
    pub fn ensure(&self) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        for d in [
            &runtime_root(),
            &self.runtime,
            &self.holders(),
            &state_root(),
            &self.state,
            &self.logs(),
        ] {
            std::fs::create_dir_all(d)?;
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One holder at a time; released on drop.
    #[test]
    fn state_lock_is_exclusive() {
        let d = tempfile::tempdir().unwrap();
        let p = Paths {
            session: "t".into(),
            runtime: d.path().join("run"),
            state: d.path().join("state"),
        };
        let held = p.try_lock_state().unwrap().expect("free");
        assert!(p.try_lock_state().unwrap().is_none());
        let t0 = std::time::Instant::now();
        assert!(
            p.lock_state(std::time::Duration::from_millis(200))
                .unwrap()
                .is_none()
        );
        assert!(t0.elapsed() >= std::time::Duration::from_millis(200));
        drop(held);
        // Not `try_lock_state`: a process forked by a concurrent test holds a duplicate of the
        // descriptor (and so the flock) until its exec closes it (O_CLOEXEC), briefly.
        assert!(
            p.lock_state(std::time::Duration::from_secs(5))
                .unwrap()
                .is_some()
        );
    }
}
