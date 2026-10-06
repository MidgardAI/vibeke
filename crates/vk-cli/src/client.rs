//! JSON-RPC client over the server's Unix socket (or any stream), plus server auto-spawn.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use vk_proto::rpc::{Request, Response, RpcError};
use vk_server::paths::Paths;

pub struct Client<S> {
    rd: BufReader<tokio::io::ReadHalf<S>>,
    wr: tokio::io::WriteHalf<S>,
    next: u64,
    pub notifications: Vec<Value>,
}

#[derive(Debug)]
pub enum CallError {
    Rpc(RpcError),
    Io(anyhow::Error),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Rpc(e) => write!(f, "{e}"),
            CallError::Io(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for CallError {}

impl<S: AsyncRead + AsyncWrite + Unpin> Client<S> {
    pub fn new(stream: S) -> Self {
        let (rd, wr) = tokio::io::split(stream);
        Client {
            rd: BufReader::new(rd),
            wr,
            next: 1,
            notifications: Vec::new(),
        }
    }

    pub async fn send(&mut self, method: &str, params: Value) -> Result<u64> {
        let id = self.next;
        self.next += 1;
        let mut line = serde_json::to_string(&Request::new(id, method, params))?;
        line.push('\n');
        self.wr.write_all(line.as_bytes()).await?;
        self.wr.flush().await?;
        Ok(id)
    }

    /// Next message from the server (response or notification).
    pub async fn recv(&mut self) -> Result<Value> {
        let mut line = String::new();
        if self.rd.read_line(&mut line).await? == 0 {
            bail!("server closed the connection");
        }
        Ok(serde_json::from_str(&line)?)
    }

    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, CallError> {
        let id = self.send(method, params).await.map_err(CallError::Io)?;
        loop {
            let v = self.recv().await.map_err(CallError::Io)?;
            if v.get("id").and_then(Value::as_u64) == Some(id) {
                let r: Response = serde_json::from_value(v).map_err(|e| CallError::Io(e.into()))?;
                return match (r.result, r.error) {
                    (_, Some(e)) => Err(CallError::Rpc(e)),
                    (Some(v), None) => Ok(v),
                    (None, None) => Ok(Value::Null),
                };
            }
            if v.get("method").is_some() {
                self.notifications.push(v);
            }
        }
    }

    pub async fn hello(&mut self, kind: &str) -> Result<Value, CallError> {
        let token = std::env::var("VIBEKE_PANE_TOKEN").unwrap_or_default();
        self.call("client.hello", json!({"client": "vibeke-cli", "version": vk_proto::VERSION, "api": vk_proto::API_VERSION, "kind": kind, "token": token}))
            .await
    }

    pub fn into_inner(self) -> (BufReader<tokio::io::ReadHalf<S>>, tokio::io::WriteHalf<S>) {
        (self.rd, self.wr)
    }
}

/// Socket for a session: `--socket`, else `$VIBEKE_SOCKET` when it belongs to the same session,
/// else the session's runtime dir.
pub fn socket_path(session: &str, explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Ok(s) = std::env::var("VIBEKE_SOCKET")
        && std::env::var("VIBEKE_SESSION").ok().as_deref() == Some(session)
    {
        return PathBuf::from(s);
    }
    Paths::new(session).socket()
}

/// Refuse to talk to a socket whose directory chain or file could have been planted by
/// another user (e.g. an attacker-created `/tmp/vibeke-<uid>` symlink or directory).
///
/// For a socket under the runtime root (`runtime_root()/<session>/vibeke.sock`) both the
/// session dir and the root must be real directories (checked with `symlink_metadata`, so
/// symlinks are never followed) owned by the current uid with mode 0700. For an explicit
/// socket elsewhere only its parent directory is checked (real directory, ours, not group- or
/// world-writable). The socket itself must be a socket owned by the current uid. Missing
/// pieces are fine here (nothing to trust yet); `connect` then fails on its own.
pub fn check_socket_trust(socket: &Path) -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    check_socket_trust_for(socket, &vk_server::paths::runtime_root(), uid)
}

fn check_socket_trust_for(socket: &Path, root: &Path, uid: u32) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let Some(parent) = socket.parent() else {
        bail!("socket path {} has no parent directory", socket.display());
    };
    let mut dirs: Vec<(&Path, bool)> = Vec::new();
    if socket.starts_with(root) {
        let mut d = Some(parent);
        while let Some(p) = d {
            dirs.push((p, true));
            if p == root {
                break;
            }
            d = p.parent();
        }
    } else {
        dirs.push((parent, false));
    }
    for (d, strict) in dirs {
        let md = match std::fs::symlink_metadata(d) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => bail!("cannot inspect {}: {e}", d.display()),
        };
        if md.file_type().is_symlink() || !md.is_dir() {
            bail!(
                "refusing {}: {} is not a plain directory (symlink or other file)",
                socket.display(),
                d.display()
            );
        }
        if md.uid() != uid {
            bail!(
                "refusing {}: {} is owned by uid {}, not {uid}",
                socket.display(),
                d.display(),
                md.uid()
            );
        }
        let mode = md.mode() & 0o7777;
        if (strict && mode != 0o700) || (!strict && mode & 0o022 != 0) {
            bail!(
                "refusing {}: {} has mode {mode:o}{}",
                socket.display(),
                d.display(),
                if strict { " (need 700)" } else { "" }
            );
        }
    }
    match std::fs::symlink_metadata(socket) {
        Ok(md) => {
            if !md.file_type().is_socket() {
                bail!("refusing {}: not a socket", socket.display());
            }
            if md.uid() != uid {
                bail!(
                    "refusing {}: socket is owned by uid {}, not {uid}",
                    socket.display(),
                    md.uid()
                );
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => bail!("cannot inspect {}: {e}", socket.display()),
    }
    Ok(())
}

pub async fn connect(path: &Path) -> Result<UnixStream> {
    check_socket_trust(path)?;
    UnixStream::connect(path)
        .await
        .with_context(|| format!("connect {}", path.display()))
}

/// Connect, spawning the server in the background if it isn't running (unless `no_spawn`).
pub async fn connect_or_spawn(session: &str, socket: &Path, no_spawn: bool) -> Result<UnixStream> {
    // Before connecting, and before `spawn_server` creates/chmods anything in the chain.
    check_socket_trust(socket)?;
    if let Ok(s) = UnixStream::connect(socket).await {
        return Ok(s);
    }
    if no_spawn {
        bail!("server not running (session {session})");
    }
    spawn_server(session)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        check_socket_trust(socket)?;
        if let Ok(s) = UnixStream::connect(socket).await {
            return Ok(s);
        }
        if Instant::now() > deadline {
            let log = Paths::new(session).logs().join("server.log");
            bail!("server did not start within 5 s (see {})", log.display());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Start `vibeke server --session <s>` detached (own session, stdio to the server log).
pub fn spawn_server(session: &str) -> Result<()> {
    let paths = Paths::new(session);
    paths.ensure()?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.logs().join("server.log"))?;
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.args(["server", "--session", session])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid between fork and exec is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("spawn server")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    fn uid() -> u32 {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() }
    }

    fn mkdir(p: &Path, mode: u32) {
        std::fs::create_dir_all(p).unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// root/s/vibeke.sock with a live listener.
    fn layout() -> (tempfile::TempDir, PathBuf, PathBuf, UnixListener) {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("r");
        mkdir(&root, 0o700);
        mkdir(&root.join("s"), 0o700);
        let sock = root.join("s").join("v.sock");
        let l = UnixListener::bind(&sock).unwrap();
        (t, root, sock, l)
    }

    #[test]
    fn correct_layout_accepted() {
        let (_t, root, sock, _l) = layout();
        check_socket_trust_for(&sock, &root, uid()).unwrap();
        // Nothing there yet (first run): not an error, connect fails on its own.
        check_socket_trust_for(&root.join("new").join("v.sock"), &root, uid()).unwrap();
    }

    #[test]
    fn symlinked_dirs_refused() {
        let (t, root, _sock, _l) = layout();
        // Attacker-style: the session dir is a symlink to a directory we do own.
        let real = t.path().join("elsewhere");
        mkdir(&real, 0o700);
        std::os::unix::fs::symlink(&real, root.join("evil")).unwrap();
        let e = check_socket_trust_for(&root.join("evil").join("v.sock"), &root, uid());
        assert!(format!("{:#}", e.unwrap_err()).contains("not a plain directory"));
        // The root itself being a symlink is refused too.
        let link_root = t.path().join("linkroot");
        std::os::unix::fs::symlink(&root, &link_root).unwrap();
        assert!(
            check_socket_trust_for(&link_root.join("s").join("v.sock"), &link_root, uid()).is_err()
        );
    }

    #[test]
    fn wrong_mode_refused() {
        let (_t, root, sock, _l) = layout();
        std::fs::set_permissions(root.join("s"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(check_socket_trust_for(&sock, &root, uid()).is_err());
        mkdir(&root.join("s"), 0o700);
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o750)).unwrap();
        assert!(check_socket_trust_for(&sock, &root, uid()).is_err());
    }

    #[test]
    fn wrong_owner_and_non_socket_refused() {
        let (_t, root, sock, _l) = layout();
        assert!(check_socket_trust_for(&sock, &root, uid() + 1).is_err());
        let plain = root.join("s").join("plain.sock");
        std::fs::write(&plain, b"x").unwrap();
        assert!(check_socket_trust_for(&plain, &root, uid()).is_err());
        let link = root.join("s").join("link.sock");
        std::os::unix::fs::symlink(&sock, &link).unwrap();
        assert!(check_socket_trust_for(&link, &root, uid()).is_err());
    }

    #[test]
    fn explicit_socket_outside_root_checks_parent() {
        let (t, root, _sock, _l) = layout();
        let other = t.path().join("o");
        mkdir(&other, 0o700);
        let sock = other.join("x.sock");
        let _l2 = UnixListener::bind(&sock).unwrap();
        check_socket_trust_for(&sock, &root, uid()).unwrap();
        std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(check_socket_trust_for(&sock, &root, uid()).is_err());
    }
}
