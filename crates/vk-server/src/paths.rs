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
