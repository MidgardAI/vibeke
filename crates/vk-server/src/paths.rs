//! Directory layout (01 §5). Every path can be overridden by env for tests and side-by-side
//! sessions (`VIBEKE_RUNTIME_DIR`, `VIBEKE_STATE_DIR`).

use std::path::{Path, PathBuf};

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

/// `flock(LOCK_EX | LOCK_NB)` on `path` (created in `dir`); `Ok(None)` when held elsewhere.
fn try_flock(dir: &Path, path: &Path) -> std::io::Result<Option<StateLock>> {
    use std::os::fd::AsRawFd;
    std::fs::create_dir_all(dir)?;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
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
    /// Named after the last 12 characters of the pane id (60 random bits of a ULID) so the path
    /// fits `sun_path` (104 bytes on macOS) under a long `$TMPDIR`. The full path is recorded
    /// with the holder, so recovery never recomputes it and older sockets still reattach.
    pub fn holder_socket(&self, pane: &str) -> PathBuf {
        let start = pane.char_indices().rev().nth(11).map_or(0, |(i, _)| i);
        let short = &pane[start..];
        self.holders().join(format!("{short}.sock"))
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
        try_flock(&self.state, &self.state_lock())
    }
    /// The control socket's lock: `<runtime>/server.lock`, held by a serving server for its
    /// whole life. The state lock already keeps two servers off one state dir; this one keeps
    /// two servers off one socket even when their state dirs differ (an environment with
    /// another `XDG_STATE_HOME`), so a server never unlinks a live server's socket.
    pub fn runtime_lock(&self) -> PathBuf {
        self.runtime.join("server.lock")
    }
    /// [`Paths::runtime_lock`], retrying for up to `wait`; `Ok(None)` when another process holds it.
    pub fn lock_runtime(&self, wait: std::time::Duration) -> std::io::Result<Option<StateLock>> {
        let t0 = std::time::Instant::now();
        loop {
            if let Some(l) = try_flock(&self.runtime, &self.runtime_lock())? {
                return Ok(Some(l));
            }
            if t0.elapsed() >= wait {
                return Ok(None);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
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

    /// The append-only, hash-chained audit log (09 §11): `<state>/audit.jsonl`.
    pub fn audit_log(&self) -> PathBuf {
        self.state.join("audit.jsonl")
    }

    /// Create runtime/state dirs with 0700 permissions (09 §3.1), refusing a directory that
    /// another user owns or that is group/world-writable (socket squatting in a shared `/tmp`).
    pub fn ensure(&self) -> std::io::Result<()> {
        for d in [
            &runtime_root(),
            &self.runtime,
            &self.holders(),
            &state_root(),
            &self.state,
            &self.logs(),
        ] {
            ensure_private_dir(d)?;
        }
        // Logs are 0600 even when a spawner with another umask created them (09 §3.1).
        if let Ok(rd) = std::fs::read_dir(self.logs()) {
            for e in rd.flatten() {
                vk_store::restrict_file(&e.path());
            }
        }
        Ok(())
    }
}

fn unsafe_dir(d: &Path, why: String) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!(
            "refusing to use {}: {why} (09 §3.1). Remove it or fix its owner and mode (`chmod 700`), or point VIBEKE_RUNTIME_DIR / VIBEKE_STATE_DIR elsewhere",
            d.display()
        ),
    )
}

/// Why `d` is not a safe private directory for this user, if it isn't: it (or the symlink
/// naming it) belongs to another user, it is not a directory, or it is group/world-writable.
pub fn private_dir_problem(d: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let link = match std::fs::symlink_metadata(d) {
        Ok(m) => m,
        Err(e) => return Some(format!("cannot stat it: {e}")),
    };
    if link.file_type().is_symlink() && link.uid() != uid {
        return Some(format!(
            "it is a symlink owned by uid {}, not you (uid {uid})",
            link.uid()
        ));
    }
    let m = match std::fs::metadata(d) {
        Ok(m) => m,
        Err(e) => return Some(format!("cannot stat it: {e}")),
    };
    if !m.is_dir() {
        return Some("it is not a directory".into());
    }
    if m.uid() != uid {
        return Some(format!(
            "it is owned by uid {}, not you (uid {uid})",
            m.uid()
        ));
    }
    if m.mode() & 0o022 != 0 {
        return Some(format!(
            "it is group- or world-writable (mode {:03o})",
            m.mode() & 0o777
        ));
    }
    None
}

/// Create `d` (and missing parents) 0700, or verify an existing one with
/// [`private_dir_problem`] and tighten it to 0700.
pub fn ensure_private_dir(d: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if std::fs::symlink_metadata(d).is_err() {
        if let Some(parent) = d.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::DirBuilder::new().mode(0o700).create(d) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    if let Some(why) = private_dir_problem(d) {
        return Err(unsafe_dir(d, why));
    }
    std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))
}

/// The user's umask from before [`harden_umask`]; `u32::MAX` = not hardened.
static USER_UMASK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(u32::MAX);

/// `umask 077` for the server process (09 §3.1), so every file it creates (state.db, blobs,
/// logs, sockets) is private. Children that create the user's own files — the holders' pane
/// processes and git checkouts — get the user's previous umask back. Idempotent; returns the
/// user's umask.
pub fn harden_umask() -> u32 {
    // SAFETY: umask has no preconditions.
    let prev = unsafe { libc::umask(0o077) } as u32;
    let user = match USER_UMASK.compare_exchange(
        u32::MAX,
        prev,
        std::sync::atomic::Ordering::SeqCst,
        std::sync::atomic::Ordering::SeqCst,
    ) {
        Ok(_) => prev,
        Err(earlier) => earlier,
    };
    vk_hold::set_child_umask(user);
    vk_tasks::set_child_umask(user);
    vk_handoff::set_child_umask(user);
    user
}

/// The user's umask recorded by [`harden_umask`], if the process hardened its own.
pub fn user_umask() -> Option<u32> {
    let m = USER_UMASK.load(std::sync::atomic::Ordering::SeqCst);
    (m != u32::MAX).then_some(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 09 §3.1: a group/world-writable runtime dir (squatting in a shared `/tmp`) is refused,
    /// a merely readable one of ours is tightened to 0700, a missing one is created 0700, and a
    /// file in its place is refused.
    #[test]
    fn private_dirs_are_verified() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let fresh = d.path().join("a/b/fresh");
        ensure_private_dir(&fresh).unwrap();
        assert_eq!(mode(&fresh), 0o700);
        let open = d.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        let e = ensure_private_dir(&open).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(e.to_string().contains("world-writable"), "{e}");
        assert_eq!(mode(&open), 0o777, "a refused dir is left alone");
        let group = d.path().join("group");
        std::fs::create_dir(&group).unwrap();
        std::fs::set_permissions(&group, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(ensure_private_dir(&group).is_err());
        let readable = d.path().join("readable");
        std::fs::create_dir(&readable).unwrap();
        std::fs::set_permissions(&readable, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&readable).unwrap();
        assert_eq!(mode(&readable), 0o700);
        let file = d.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(
            ensure_private_dir(&file)
                .unwrap_err()
                .to_string()
                .contains("not a directory")
        );
        // A symlink of ours to a private dir of ours is fine.
        let link = d.path().join("link");
        std::os::unix::fs::symlink(&readable, &link).unwrap();
        ensure_private_dir(&link).unwrap();
        // Foreign owners can't be produced without root; check the message path directly.
        assert!(private_dir_problem(Path::new("/")).is_some());
    }

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
        // Released on drop. Another test in this binary may fork a child (pty spawn, a
        // `pre_exec` command) while our lock file is open; the child shares the open file
        // description until its `exec` closes it (close-on-exec), so allow that brief window.
        assert!(
            p.lock_state(std::time::Duration::from_secs(5))
                .unwrap()
                .is_some()
        );
    }

    /// The socket lock is independent of the state lock: two servers with different state dirs
    /// still cannot share a socket.
    #[test]
    fn runtime_lock_is_exclusive_across_state_dirs() {
        let d = tempfile::tempdir().unwrap();
        let p = |state: &str| Paths {
            session: "t".into(),
            runtime: d.path().join("run"),
            state: d.path().join(state),
        };
        let (a, b) = (p("state-a"), p("state-b"));
        let _sa = a.try_lock_state().unwrap().expect("free");
        let _sb = b.try_lock_state().unwrap().expect("other state dir");
        let held = a.lock_runtime(std::time::Duration::ZERO).unwrap();
        assert!(held.is_some());
        assert!(b.lock_runtime(std::time::Duration::ZERO).unwrap().is_none());
    }

    /// A holder socket under the longest macOS `$TMPDIR` shape fits `sun_path` (104 bytes).
    #[test]
    fn holder_socket_fits_macos_sun_path() {
        let p = Paths {
            session: "default".into(),
            runtime: PathBuf::from(
                "/var/folders/vp/bwsl4vyx2r30vjbd6hy07b6m0000gn/T/vibeke-501/default",
            ),
            state: PathBuf::new(),
        };
        let s = p.holder_socket("01M4AFG11S4Y0DZ7THC5P3TYTZ");
        assert_eq!(s, p.holders().join("Z7THC5P3TYTZ.sock"));
        assert!(s.as_os_str().len() < 104, "{}", s.display());
        assert_eq!(p.holder_socket("p1"), p.holders().join("p1.sock"));
    }
}
