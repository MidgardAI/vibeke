//! The `fake` provider (spec 17 §3.3), for tests only. It is **not** an isolation boundary:
//! commands run on the host as the current user.
//!
//! - A box is the directory `<dir>/boxes/<id>/` with a `meta.json` (name, tags, created_at,
//!   state). Its id is its name. `HOME` is `<box>/home`, and absolute paths in `cwd` and
//!   [`Provider::write_file`] are mapped into the box directory (`/workspace` is
//!   `<box>/workspace`). Commands also get `VIBEKE_FAKE_BOX=<box>`.
//! - The only valid credential is [`TOKEN`]; anything else is `needs_auth`.
//! - Pipe sessions are host child processes.
//! - Terminal sessions run under a small per-session daemon, `vibeke cloud exec --fake-daemon
//!   <session dir>` ([`session_daemon_main`]), so they survive the client and can be attached
//!   again. The daemon owns a real PTY, keeps the last 64 KiB of output for replay, and serves
//!   one client at a time on `<session dir>/sock` (a newer client replaces the older one).
//!
//! Daemon socket frames: 1 type byte, a big-endian u32 length, then the payload
//! ([`frame`]).

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::{
    Account, AuthMethod, BoxFut, BoxState, Caps, CloudError, CreateSpec, ErrorKind, ExecReq, In,
    Out, Provider, RemoteBox, Result, Secret, Session, SessionInfo, naming,
};

/// Set to a directory to enable the `fake` provider (tests).
pub const DIR_ENV: &str = "VIBEKE_CLOUD_FAKE_DIR";
/// The one credential the fake accepts.
pub const TOKEN: &str = "fake-token";
/// Env var the fake reads its credential from ([`AuthMethod::Env`]).
pub const TOKEN_ENV: &str = "VIBEKE_CLOUD_FAKE_TOKEN";
/// Binary that runs session daemons (default: the current executable, which must be `vibeke`).
pub const EXE_ENV: &str = "VIBEKE_CLOUD_FAKE_EXE";
/// Set in every command's environment: the box directory.
pub const BOX_ENV: &str = "VIBEKE_FAKE_BOX";

const RING: usize = 64 * 1024;
const MAX_FRAME: usize = 4 * 1024 * 1024;

/// Daemon socket frame types.
pub mod frame {
    /// Terminal bytes, both directions.
    pub const DATA: u8 = 0;
    /// Client to daemon: cols u16 BE, rows u16 BE.
    pub const RESIZE: u8 = 1;
    /// Client to daemon: signal name (`TERM`, `INT`, …).
    pub const SIGNAL: u8 = 2;
    /// Daemon to client: exit code i32 BE.
    pub const EXIT: u8 = 3;
}

pub struct Fake {
    pub dir: PathBuf,
}

impl Fake {
    pub fn new(dir: PathBuf) -> Self {
        Fake { dir }
    }

    fn box_dir(&self, id: &str) -> Result<PathBuf> {
        let ok = !id.is_empty()
            && id != "."
            && id != ".."
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
        if !ok {
            return Err(CloudError::new(
                ErrorKind::InvalidParams,
                format!("bad box id {id:?}"),
            ));
        }
        Ok(self.dir.join("boxes").join(id))
    }

    /// The box directory of an existing box.
    fn existing(&self, id: &str) -> Result<PathBuf> {
        let d = self.box_dir(id)?;
        if !d.join("meta.json").is_file() {
            return Err(CloudError::not_found(format!("no box {id}")));
        }
        Ok(d)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Meta {
    name: String,
    #[serde(default)]
    tags: Option<naming::Tags>,
    #[serde(default)]
    created_at: u64,
    #[serde(default)]
    state: BoxState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DaemonSpec {
    argv: Vec<String>,
    env: Vec<(String, String)>,
    cwd: PathBuf,
    cols: u16,
    rows: u16,
    detachable: bool,
}

fn check(cred: &Secret) -> Result<()> {
    if cred.expose() == TOKEN {
        Ok(())
    } else {
        Err(CloudError::needs_auth("fake"))
    }
}

fn io_err(e: std::io::Error) -> CloudError {
    CloudError::internal(format!("fake provider: {e}"))
}

fn read_meta(bdir: &Path) -> Result<Meta> {
    let text =
        std::fs::read(bdir.join("meta.json")).map_err(|_| CloudError::not_found("no such box"))?;
    serde_json::from_slice(&text).map_err(CloudError::internal)
}

fn write_meta(bdir: &Path, m: &Meta) -> Result<()> {
    let tmp = bdir.join("meta.json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(m).map_err(CloudError::internal)?,
    )
    .map_err(io_err)?;
    std::fs::rename(&tmp, bdir.join("meta.json")).map_err(io_err)
}

fn to_box(id: &str, m: Meta) -> RemoteBox {
    RemoteBox {
        provider: "fake".into(),
        id: id.to_string(),
        tags: m.tags.or_else(|| naming::parse_name(&m.name)),
        name: m.name,
        state: m.state,
        created_at: m.created_at,
        last_active_at: m.created_at,
        url: None,
    }
}

/// A path in the box: absolute paths are rooted at the box directory, relative ones too.
fn map_path(bdir: &Path, p: &str) -> Result<PathBuf> {
    // Paths the server built from `box_root` are already inside the box.
    let p = Path::new(p).strip_prefix(bdir).unwrap_or(Path::new(p));
    let mut out = bdir.to_path_buf();
    for c in p.components() {
        match c {
            Component::Normal(s) => out.push(s),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => {
                return Err(CloudError::new(
                    ErrorKind::InvalidParams,
                    "paths may not contain ..",
                ));
            }
        }
    }
    Ok(out)
}

fn new_id() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seed = format!(
        "{nanos}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    );
    blake3::hash(seed.as_bytes()).to_hex()[..8].to_string()
}

/// The daemon's socket: `<session dir>/sock`, or a hashed name in `/tmp` when that path is too
/// long for a unix socket.
fn sock_path(sdir: &Path) -> PathBuf {
    let p = sdir.join("sock");
    if p.as_os_str().len() < 100 {
        return p;
    }
    let h = blake3::hash(sdir.as_os_str().as_bytes()).to_hex();
    PathBuf::from(format!("/tmp/vkfake-{}.sock", &h[..16]))
}

pub(crate) fn sig_num(name: &str) -> Option<i32> {
    let n = name.trim().to_ascii_uppercase();
    Some(match n.strip_prefix("SIG").unwrap_or(&n) {
        "HUP" => libc::SIGHUP,
        "INT" => libc::SIGINT,
        "QUIT" => libc::SIGQUIT,
        "KILL" => libc::SIGKILL,
        "TERM" => libc::SIGTERM,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        "WINCH" => libc::SIGWINCH,
        "CONT" => libc::SIGCONT,
        "STOP" => libc::SIGSTOP,
        other => other.parse().ok()?,
    })
}

fn exit_code(st: std::process::ExitStatus) -> i32 {
    st.code().unwrap_or_else(|| 128 + st.signal().unwrap_or(0))
}

fn read_pid(p: &Path) -> Option<i32> {
    std::fs::read_to_string(p).ok()?.trim().parse().ok()
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

/// A daemon that is still running (pid alive and no exit recorded).
fn session_alive(sdir: &Path) -> bool {
    !sdir.join("exit").exists() && read_pid(&sdir.join("pid")).is_some_and(alive)
}

fn kill_session(sdir: &Path) {
    // SAFETY: plain kill(2) calls; the child is its own process group.
    unsafe {
        if let Some(c) = read_pid(&sdir.join("child.pid")).filter(|p| alive(*p)) {
            libc::kill(-c, libc::SIGKILL);
            libc::kill(c, libc::SIGKILL);
        }
        if let Some(d) = read_pid(&sdir.join("pid")).filter(|p| alive(*p)) {
            libc::kill(d, libc::SIGKILL);
        }
    }
    let _ = std::fs::remove_file(sock_path(sdir));
}

// ---- framing ----

async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<(u8, Vec<u8>)> {
    let mut h = [0u8; 5];
    r.read_exact(&mut h).await?;
    let n = u32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize;
    if n > MAX_FRAME {
        return Err(std::io::Error::other("frame too large"));
    }
    let mut p = vec![0u8; n];
    r.read_exact(&mut p).await?;
    Ok((h[0], p))
}

async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, t: u8, p: &[u8]) -> std::io::Result<()> {
    let mut b = Vec::with_capacity(5 + p.len());
    b.push(t);
    b.extend_from_slice(&(p.len() as u32).to_be_bytes());
    b.extend_from_slice(p);
    w.write_all(&b).await
}

fn read_frame_sync(r: &mut impl Read) -> std::io::Result<(u8, Vec<u8>)> {
    let mut h = [0u8; 5];
    r.read_exact(&mut h)?;
    let n = u32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize;
    if n > MAX_FRAME {
        return Err(std::io::Error::other("frame too large"));
    }
    let mut p = vec![0u8; n];
    r.read_exact(&mut p)?;
    Ok((h[0], p))
}

fn write_frame_sync(w: &mut impl Write, t: u8, p: &[u8]) -> std::io::Result<()> {
    let mut b = Vec::with_capacity(5 + p.len());
    b.push(t);
    b.extend_from_slice(&(p.len() as u32).to_be_bytes());
    b.extend_from_slice(p);
    w.write_all(&b)
}

// ---- sessions (client side) ----

/// A session over a daemon socket.
fn daemon_client(id: String, stream: tokio::net::UnixStream) -> Session {
    let (mut rd, mut wr) = stream.into_split();
    let (in_tx, mut in_rx) = mpsc::channel::<In>(64);
    let (out_tx, out_rx) = mpsc::channel::<Out>(256);
    tokio::spawn(async move {
        while let Some(i) = in_rx.recv().await {
            let (t, p) = match i {
                In::Data(d) => (frame::DATA, d),
                In::Resize { cols, rows } => {
                    let mut p = cols.to_be_bytes().to_vec();
                    p.extend_from_slice(&rows.to_be_bytes());
                    (frame::RESIZE, p)
                }
                In::Signal(s) => (frame::SIGNAL, s.into_bytes()),
                In::Eof => continue,
            };
            if write_frame(&mut wr, t, &p).await.is_err() {
                return;
            }
        }
        // Input dropped: detach.
        let _ = wr.shutdown().await;
    });
    tokio::spawn(async move {
        loop {
            let o = match read_frame(&mut rd).await {
                Ok((frame::DATA, p)) => Out::Stdout(p),
                Ok((frame::EXIT, p)) if p.len() == 4 => {
                    let _ = out_tx
                        .send(Out::Exit(i32::from_be_bytes([p[0], p[1], p[2], p[3]])))
                        .await;
                    return;
                }
                Ok(_) => continue,
                Err(_) => {
                    let _ = out_tx
                        .send(Out::Lost("fake session connection closed".into()))
                        .await;
                    return;
                }
            };
            if out_tx.send(o).await.is_err() {
                return;
            }
        }
    });
    Session {
        id,
        tty: true,
        input: in_tx,
        output: out_rx,
    }
}

async fn connect(sdir: &Path) -> std::io::Result<tokio::net::UnixStream> {
    tokio::net::UnixStream::connect(sock_path(sdir)).await
}

/// A session that only reports `msg` on stderr and exits with `code` (like a shell would).
fn failed_session(msg: String, code: i32) -> Session {
    let (in_tx, _in_rx) = mpsc::channel(1);
    let (out_tx, out_rx) = mpsc::channel(4);
    let _ = out_tx.try_send(Out::Stderr(msg.into_bytes()));
    let _ = out_tx.try_send(Out::Exit(code));
    Session {
        id: String::new(),
        tty: false,
        input: in_tx,
        output: out_rx,
    }
}

async fn copy_out<R: AsyncRead + Unpin>(r: Option<R>, tx: mpsc::Sender<Out>, stdout: bool) {
    let Some(mut r) = r else { return };
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let d = buf[..n].to_vec();
                let o = if stdout {
                    Out::Stdout(d)
                } else {
                    Out::Stderr(d)
                };
                if tx.send(o).await.is_err() {
                    return;
                }
            }
        }
    }
}

fn base_env(bdir: &Path) -> Vec<(String, String)> {
    vec![
        (
            "HOME".into(),
            bdir.join("home").to_string_lossy().into_owned(),
        ),
        (BOX_ENV.into(), bdir.to_string_lossy().into_owned()),
    ]
}

async fn exec_pipe(bdir: PathBuf, cwd: PathBuf, req: ExecReq) -> Result<Session> {
    let mut cmd = tokio::process::Command::new(&req.argv[0]);
    cmd.args(&req.argv[1..])
        .current_dir(&cwd)
        .envs(base_env(&bdir))
        .envs(req.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Ok(failed_session(
                format!("{}: {e}\n", req.argv[0]),
                if e.kind() == std::io::ErrorKind::NotFound {
                    127
                } else {
                    126
                },
            ));
        }
    };
    let pid = child.id().map(|p| p as i32);
    let mut stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (in_tx, mut in_rx) = mpsc::channel::<In>(64);
    let (out_tx, out_rx) = mpsc::channel::<Out>(256);
    tokio::spawn(async move {
        while let Some(i) = in_rx.recv().await {
            match i {
                In::Data(d) => {
                    let failed = match stdin.as_mut() {
                        Some(w) => w.write_all(&d).await.is_err(),
                        None => false,
                    };
                    if failed {
                        stdin = None;
                    }
                }
                In::Eof => stdin = None,
                In::Resize { .. } => {}
                In::Signal(s) => {
                    if let (Some(p), Some(n)) = (pid, sig_num(&s)) {
                        // SAFETY: kill(2) on our own child.
                        unsafe {
                            libc::kill(p, n);
                        }
                    }
                }
            }
        }
    });
    tokio::spawn(async move {
        let copy = async {
            tokio::join!(
                copy_out(stdout, out_tx.clone(), true),
                copy_out(stderr, out_tx.clone(), false)
            )
        };
        tokio::select! {
            _ = copy => {}
            _ = out_tx.closed() => {
                let _ = child.start_kill();
                return;
            }
        }
        let code = match child.wait().await {
            Ok(st) => exit_code(st),
            Err(_) => 255,
        };
        let _ = out_tx.send(Out::Exit(code)).await;
    });
    Ok(Session {
        id: String::new(),
        tty: false,
        input: in_tx,
        output: out_rx,
    })
}

async fn exec_tty(bdir: PathBuf, cwd: PathBuf, req: ExecReq) -> Result<Session> {
    let id = new_id();
    let sdir = bdir.join("sessions").join(&id);
    std::fs::create_dir_all(&sdir).map_err(io_err)?;
    let mut env = base_env(&bdir);
    env.extend(req.env.iter().cloned());
    let spec = DaemonSpec {
        argv: req.argv.clone(),
        env,
        cwd,
        cols: if req.cols == 0 { 80 } else { req.cols },
        rows: if req.rows == 0 { 24 } else { req.rows },
        detachable: req.detachable,
    };
    std::fs::write(
        sdir.join("spec.json"),
        serde_json::to_vec(&spec).map_err(CloudError::internal)?,
    )
    .map_err(io_err)?;
    let exe = match std::env::var_os(EXE_ENV).filter(|v| !v.is_empty()) {
        Some(e) => PathBuf::from(e),
        None => std::env::current_exe().map_err(io_err)?,
    };
    let st = tokio::process::Command::new(exe)
        .args(["cloud", "exec", "--fake-daemon"])
        .arg(&sdir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(io_err)?;
    if !st.success() {
        return Err(CloudError::internal("fake session daemon failed to start"));
    }
    for _ in 0..250 {
        if let Ok(e) = std::fs::read_to_string(sdir.join("error")) {
            return Err(CloudError::new(
                ErrorKind::InvalidParams,
                format!("fake session failed: {}", e.trim()),
            ));
        }
        if let Ok(s) = connect(&sdir).await {
            return Ok(daemon_client(id, s));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Err(CloudError::unavailable("fake session daemon did not start"))
}

impl Provider for Fake {
    fn id(&self) -> &'static str {
        "fake"
    }
    fn label(&self) -> &'static str {
        "Fake (tests)"
    }
    fn caps(&self) -> Caps {
        Caps {
            resize: true,
            reattach: true,
            explicit_suspend: true,
            ..Caps::default()
        }
    }
    fn auth_methods(&self) -> Vec<AuthMethod> {
        vec![
            AuthMethod::PasteToken {
                label: "Fake token".into(),
                help_url: "https://vibeke.dev".into(),
                hint: format!("Use {TOKEN}"),
            },
            AuthMethod::Env {
                var: TOKEN_ENV.into(),
            },
        ]
    }

    fn verify<'a>(&'a self, cred: &'a Secret) -> BoxFut<'a, Result<Account>> {
        Box::pin(async move {
            check(cred)?;
            Ok(Account {
                label: "fake".into(),
                details: Default::default(),
            })
        })
    }

    fn import<'a>(&'a self, _source: &'a str) -> BoxFut<'a, Result<Option<Secret>>> {
        Box::pin(async { Ok(None) })
    }

    fn create<'a>(
        &'a self,
        cred: &'a Secret,
        spec: &'a CreateSpec,
    ) -> BoxFut<'a, Result<RemoteBox>> {
        Box::pin(async move {
            check(cred)?;
            let bdir = self.box_dir(&spec.name)?;
            if bdir.join("meta.json").exists() {
                return Err(CloudError::new(
                    ErrorKind::Conflict,
                    format!("box {} exists", spec.name),
                ));
            }
            for d in ["home", "workspace", "sessions"] {
                std::fs::create_dir_all(bdir.join(d)).map_err(io_err)?;
            }
            let m = Meta {
                name: spec.name.clone(),
                tags: Some(spec.tags.clone()),
                created_at: crate::now_s(),
                state: BoxState::Running,
            };
            write_meta(&bdir, &m)?;
            Ok(to_box(&spec.name, m))
        })
    }

    fn get<'a>(&'a self, cred: &'a Secret, id: &'a str) -> BoxFut<'a, Result<RemoteBox>> {
        Box::pin(async move {
            check(cred)?;
            let bdir = self.existing(id)?;
            Ok(to_box(id, read_meta(&bdir)?))
        })
    }

    fn list<'a>(&'a self, cred: &'a Secret) -> BoxFut<'a, Result<Vec<RemoteBox>>> {
        Box::pin(async move {
            check(cred)?;
            let Ok(rd) = std::fs::read_dir(self.dir.join("boxes")) else {
                return Ok(vec![]);
            };
            let mut v: Vec<RemoteBox> = rd
                .flatten()
                .filter_map(|e| {
                    let id = e.file_name().to_str()?.to_string();
                    let m = read_meta(&e.path()).ok()?;
                    Some(to_box(&id, m))
                })
                .filter(|b| b.tags.is_some())
                .collect();
            v.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(v)
        })
    }

    fn destroy<'a>(&'a self, cred: &'a Secret, id: &'a str) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            check(cred)?;
            let bdir = self.box_dir(id)?;
            if !bdir.exists() {
                return Ok(());
            }
            if let Ok(rd) = std::fs::read_dir(bdir.join("sessions")) {
                for e in rd.flatten() {
                    kill_session(&e.path());
                }
            }
            match std::fs::remove_dir_all(&bdir) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(io_err(e)),
            }
        })
    }

    fn suspend<'a>(&'a self, cred: &'a Secret, id: &'a str) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            check(cred)?;
            let bdir = self.existing(id)?;
            let mut m = read_meta(&bdir)?;
            m.state = BoxState::Paused;
            write_meta(&bdir, &m)
        })
    }

    fn resume<'a>(&'a self, cred: &'a Secret, id: &'a str) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            check(cred)?;
            let bdir = self.existing(id)?;
            let mut m = read_meta(&bdir)?;
            m.state = BoxState::Running;
            write_meta(&bdir, &m)
        })
    }

    fn checkpoint<'a>(
        &'a self,
        cred: &'a Secret,
        _id: &'a str,
        _note: &'a str,
    ) -> BoxFut<'a, Result<String>> {
        Box::pin(async move {
            check(cred)?;
            Err(CloudError::unsupported(
                "the fake provider has no checkpoints",
            ))
        })
    }

    fn exec<'a>(
        &'a self,
        cred: &'a Secret,
        id: &'a str,
        req: ExecReq,
    ) -> BoxFut<'a, Result<Session>> {
        Box::pin(async move {
            check(cred)?;
            if req.argv.is_empty() || req.argv[0].is_empty() {
                return Err(CloudError::new(ErrorKind::InvalidParams, "empty command"));
            }
            let bdir = self.existing(id)?;
            let cwd = match req.cwd.as_deref() {
                Some(c) if !c.is_empty() => map_path(&bdir, c)?,
                _ => bdir.clone(),
            };
            std::fs::create_dir_all(&cwd).map_err(io_err)?;
            if req.tty {
                exec_tty(bdir, cwd, req).await
            } else {
                exec_pipe(bdir, cwd, req).await
            }
        })
    }

    fn attach<'a>(
        &'a self,
        cred: &'a Secret,
        id: &'a str,
        session: &'a str,
        cols: u16,
        rows: u16,
    ) -> BoxFut<'a, Result<Session>> {
        Box::pin(async move {
            check(cred)?;
            let bdir = self.existing(id)?;
            if session.is_empty() || !session.bytes().all(|b| b.is_ascii_alphanumeric()) {
                return Err(CloudError::not_found("no such session"));
            }
            let sdir = bdir.join("sessions").join(session);
            if !session_alive(&sdir) {
                return Err(CloudError::not_found("the session has ended"));
            }
            let s = connect(&sdir)
                .await
                .map_err(|_| CloudError::not_found("the session has ended"))?;
            let sess = daemon_client(session.to_string(), s);
            if cols > 0 && rows > 0 {
                let _ = sess.input.send(In::Resize { cols, rows }).await;
            }
            Ok(sess)
        })
    }

    fn sessions<'a>(
        &'a self,
        cred: &'a Secret,
        id: &'a str,
    ) -> BoxFut<'a, Result<Vec<SessionInfo>>> {
        Box::pin(async move {
            check(cred)?;
            let bdir = self.existing(id)?;
            let Ok(rd) = std::fs::read_dir(bdir.join("sessions")) else {
                return Ok(vec![]);
            };
            let mut v: Vec<SessionInfo> = rd
                .flatten()
                .filter_map(|e| {
                    let sdir = e.path();
                    let spec: DaemonSpec =
                        serde_json::from_slice(&std::fs::read(sdir.join("spec.json")).ok()?)
                            .ok()?;
                    let last = std::fs::metadata(sdir.join("spec.json"))
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    Some(SessionInfo {
                        id: e.file_name().to_str()?.to_string(),
                        command: spec.argv.join(" "),
                        tty: true,
                        active: session_alive(&sdir),
                        last_activity_at: last,
                    })
                })
                .collect();
            v.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(v)
        })
    }

    fn write_file<'a>(
        &'a self,
        cred: &'a Secret,
        id: &'a str,
        path: &'a str,
        data: Vec<u8>,
        mode: u32,
    ) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            use std::os::unix::fs::PermissionsExt;
            check(cred)?;
            let bdir = self.existing(id)?;
            let p = map_path(&bdir, path)?;
            if p == bdir {
                return Err(CloudError::new(ErrorKind::InvalidParams, "empty path"));
            }
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).map_err(io_err)?;
            }
            std::fs::write(&p, data).map_err(io_err)?;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode & 0o7777))
                .map_err(io_err)
        })
    }

    fn port_url<'a>(
        &'a self,
        cred: &'a Secret,
        _id: &'a str,
        _port: u16,
    ) -> BoxFut<'a, Result<Option<String>>> {
        Box::pin(async move {
            check(cred)?;
            Ok(None)
        })
    }

    fn box_root(&self, id: &str) -> String {
        self.box_dir(id)
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

// ---- session daemon ----

/// `vibeke cloud exec --fake-daemon <session dir>`: detach, run the session's command on a PTY
/// and serve it on the session socket until the command exits. Must run before any other
/// thread exists (it forks). Returns the launcher's exit code; the daemon itself exits from
/// its own threads.
pub fn session_daemon_main(args: &[String]) -> i32 {
    let Some(sdir) = args.first().map(PathBuf::from) else {
        eprintln!("vibeke cloud exec --fake-daemon <session dir>");
        return 125;
    };
    let spec: DaemonSpec = match std::fs::read(sdir.join("spec.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
    {
        Some(s) => s,
        None => {
            let _ = std::fs::write(sdir.join("error"), "unreadable spec.json");
            return 125;
        }
    };
    // SAFETY: single-threaded here (dispatched before any runtime); the child only continues
    // with ordinary Rust code after setsid.
    match unsafe { libc::fork() } {
        -1 => return 125,
        0 => {}
        _ => return 0,
    }
    // SAFETY: setsid in the new child detaches it from the launcher's session.
    unsafe {
        libc::setsid();
    }
    if let Err(e) = run_daemon(&sdir, &spec) {
        let _ = std::fs::write(sdir.join("error"), e.to_string());
        let _ = std::fs::remove_file(sock_path(&sdir));
        return 1;
    }
    0
}

struct Shared {
    ring: VecDeque<u8>,
    client: Option<(u64, std::os::unix::net::UnixStream)>,
    generation: u64,
}

impl Shared {
    fn push(&mut self, b: &[u8]) {
        self.ring.extend(b.iter().copied());
        if self.ring.len() > RING {
            let over = self.ring.len() - RING;
            self.ring.drain(..over);
        }
    }
}

fn spawn_pty(spec: &DaemonSpec) -> std::io::Result<(std::fs::File, std::process::Child)> {
    use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
    use std::os::fd::OwnedFd;
    use std::os::unix::process::CommandExt;
    if spec.argv.is_empty() {
        return Err(std::io::Error::other("empty argv"));
    }
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY)?;
    rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC)?;
    grantpt(&master)?;
    unlockpt(&master)?;
    let name = ptsname(&master, Vec::new())?;
    let slave = rustix::fs::open(
        name.as_c_str(),
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY,
        rustix::fs::Mode::empty(),
    )?;
    set_winsize(&slave, spec.cols, spec.rows);
    let mut cmd = std::process::Command::new(&spec.argv[0]);
    cmd.args(&spec.argv[1..]).current_dir(&spec.cwd);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    let s_in: OwnedFd = slave.try_clone()?;
    let s_out: OwnedFd = slave.try_clone()?;
    cmd.stdin(Stdio::from(s_in))
        .stdout(Stdio::from(s_out))
        .stderr(Stdio::from(slave));
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    drop(cmd);
    Ok((std::fs::File::from(master), child))
}

fn set_winsize(fd: impl rustix::fd::AsFd, cols: u16, rows: u16) {
    let _ = rustix::termios::tcsetwinsize(
        fd,
        rustix::termios::Winsize {
            ws_row: rows.max(1),
            ws_col: cols.max(1),
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    );
}

fn run_daemon(sdir: &Path, spec: &DaemonSpec) -> std::io::Result<()> {
    let (master, mut child) = spawn_pty(spec)?;
    let child_pid = child.id() as i32;
    std::fs::write(sdir.join("child.pid"), child_pid.to_string())?;
    std::fs::write(sdir.join("pid"), std::process::id().to_string())?;
    let sock = sock_path(sdir);
    let _ = std::fs::remove_file(&sock);
    let listener = std::os::unix::net::UnixListener::bind(&sock)?;
    let master = Arc::new(master);
    let shared = Arc::new(Mutex::new(Shared {
        ring: VecDeque::new(),
        client: None,
        generation: 0,
    }));

    // PTY output: into the ring and to the current client; at EOF the command is done.
    {
        let (master, shared, sdir, sock) = (
            master.clone(),
            shared.clone(),
            sdir.to_path_buf(),
            sock.clone(),
        );
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                let n = match (&*master).read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                let mut s = shared.lock().unwrap_or_else(|p| p.into_inner());
                s.push(&buf[..n]);
                let failed = match s.client.as_mut() {
                    Some((_, c)) => write_frame_sync(c, frame::DATA, &buf[..n]).is_err(),
                    None => false,
                };
                if failed {
                    s.client = None;
                }
            }
            let code = child.wait().map(exit_code).unwrap_or(255);
            let _ = std::fs::write(sdir.join("exit"), code.to_string());
            let mut s = shared.lock().unwrap_or_else(|p| p.into_inner());
            if let Some((_, c)) = s.client.as_mut() {
                let _ = write_frame_sync(c, frame::EXIT, &code.to_be_bytes());
                let _ = c.flush();
            }
            let _ = std::fs::remove_file(&sock);
            std::process::exit(0);
        });
    }

    // The box is gone (destroyed, or a test removed its directory): end the command and stop,
    // so no daemon outlives its box.
    {
        let (sdir, sock) = (sdir.to_path_buf(), sock.clone());
        std::thread::spawn(move || {
            while sdir.join("spec.json").exists() {
                std::thread::sleep(Duration::from_secs(2));
            }
            // SAFETY: kill(2) on the command's process group.
            unsafe {
                libc::kill(-child_pid, libc::SIGHUP);
            }
            let _ = std::fs::remove_file(&sock);
            std::process::exit(0);
        });
    }

    for conn in listener.incoming() {
        let Ok(mut c) = conn else { continue };
        let _ = c.set_write_timeout(Some(Duration::from_secs(5)));
        let Ok(reader) = c.try_clone() else { continue };
        let generation = {
            let mut s = shared.lock().unwrap_or_else(|p| p.into_inner());
            let replay: Vec<u8> = s.ring.iter().copied().collect();
            if !replay.is_empty() && write_frame_sync(&mut c, frame::DATA, &replay).is_err() {
                continue;
            }
            s.generation += 1;
            let g = s.generation;
            // One client at a time: the newer one wins.
            if let Some((_, old)) = s.client.take() {
                let _ = old.shutdown(std::net::Shutdown::Both);
            }
            s.client = Some((g, c));
            g
        };
        let (master, shared) = (master.clone(), shared.clone());
        let detachable = spec.detachable;
        std::thread::spawn(move || {
            client_loop(reader, generation, &master, &shared, child_pid, detachable)
        });
    }
    Ok(())
}

fn client_loop(
    mut r: std::os::unix::net::UnixStream,
    generation: u64,
    master: &std::fs::File,
    shared: &Mutex<Shared>,
    child_pid: i32,
    detachable: bool,
) {
    loop {
        match read_frame_sync(&mut r) {
            Ok((frame::DATA, p)) => {
                let _ = (&*master).write_all(&p);
            }
            Ok((frame::RESIZE, p)) if p.len() == 4 => {
                let cols = u16::from_be_bytes([p[0], p[1]]);
                let rows = u16::from_be_bytes([p[2], p[3]]);
                set_winsize(master, cols, rows);
            }
            Ok((frame::SIGNAL, p)) => {
                if let Some(n) = sig_num(&String::from_utf8_lossy(&p)) {
                    // SAFETY: kill(2) on the command's process group.
                    unsafe {
                        libc::kill(-child_pid, n);
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let mut s = shared.lock().unwrap_or_else(|p| p.into_inner());
    if s.client.as_ref().is_some_and(|(g, _)| *g == generation) {
        s.client = None;
        if !detachable {
            // SAFETY: kill(2) on the command's process group.
            unsafe {
                libc::kill(-child_pid, libc::SIGHUP);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake() -> (tempfile::TempDir, Fake) {
        let d = tempfile::Builder::new()
            .prefix("vkf")
            .tempdir_in("/tmp")
            .unwrap();
        let f = Fake::new(d.path().to_path_buf());
        (d, f)
    }

    fn spec() -> CreateSpec {
        let tags = naming::Tags::new("host", "task");
        CreateSpec {
            name: naming::box_name(&tags),
            tags,
            ..Default::default()
        }
    }

    async fn collect(mut s: Session) -> (Vec<u8>, Vec<u8>, i32) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        loop {
            let o = tokio::time::timeout(Duration::from_secs(10), s.output.recv())
                .await
                .expect("timed out")
                .expect("closed without exit");
            match o {
                Out::Stdout(d) => out.extend(d),
                Out::Stderr(d) => err.extend(d),
                Out::Exit(c) => return (out, err, c),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn pipe_exec_runs_in_the_box() {
        let (_d, f) = fake();
        let tok = Secret::new(TOKEN);
        let b = f.create(&tok, &spec()).await.unwrap();
        assert_eq!(f.verify(&tok).await.unwrap().label, "fake");
        let s = f
            .exec(
                &tok,
                &b.id,
                ExecReq {
                    argv: vec!["echo".into(), "hi".into()],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let (out, _, code) = collect(s).await;
        assert_eq!(out, b"hi\n");
        assert_eq!(code, 0);

        // stdin, cwd mapping, HOME and a non-zero exit.
        f.write_file(&tok, &b.id, "/workspace/f.txt", b"data".to_vec(), 0o644)
            .await
            .unwrap();
        let s = f
            .exec(
                &tok,
                &b.id,
                ExecReq {
                    argv: vec![
                        "sh".into(),
                        "-c".into(),
                        "cat; cat f.txt; echo \" $HOME\" >&2; exit 3".into(),
                    ],
                    cwd: Some("/workspace".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        s.input.send(In::Data(b"in:".to_vec())).await.unwrap();
        s.input.send(In::Eof).await.unwrap();
        let (out, err, code) = collect(s).await;
        assert_eq!(out, b"in:data");
        assert!(String::from_utf8_lossy(&err).trim().ends_with("/home"));
        assert_eq!(code, 3);

        let listed = f.list(&tok).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].tags, Some(spec().tags));
        f.suspend(&tok, &b.id).await.unwrap();
        assert_eq!(f.get(&tok, &b.id).await.unwrap().state, BoxState::Paused);
        f.destroy(&tok, &b.id).await.unwrap();
        f.destroy(&tok, &b.id).await.unwrap();
        assert_eq!(
            f.get(&tok, &b.id).await.unwrap_err().kind,
            ErrorKind::NotFound
        );
    }

    #[tokio::test]
    async fn bad_token_needs_auth() {
        let (_d, f) = fake();
        let bad = Secret::new("nope");
        assert_eq!(f.verify(&bad).await.unwrap_err().kind, ErrorKind::NeedsAuth);
        assert_eq!(
            f.create(&bad, &spec()).await.unwrap_err().kind,
            ErrorKind::NeedsAuth
        );
        assert_eq!(f.list(&bad).await.unwrap_err().kind, ErrorKind::NeedsAuth);
    }

    #[tokio::test]
    async fn missing_command_exits_127() {
        let (_d, f) = fake();
        let tok = Secret::new(TOKEN);
        let b = f.create(&tok, &spec()).await.unwrap();
        let s = f
            .exec(
                &tok,
                &b.id,
                ExecReq {
                    argv: vec!["vk-no-such-command-xyz".into()],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(collect(s).await.2, 127);
    }

    #[test]
    fn paths_stay_in_the_box() {
        let b = Path::new("/b");
        assert_eq!(
            map_path(b, "/workspace/x").unwrap(),
            Path::new("/b/workspace/x")
        );
        assert_eq!(map_path(b, "rel").unwrap(), Path::new("/b/rel"));
        assert!(map_path(b, "/../etc").is_err());
        assert_eq!(sig_num("SIGTERM"), Some(libc::SIGTERM));
        assert_eq!(sig_num("int"), Some(libc::SIGINT));
    }
}
