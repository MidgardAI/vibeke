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
        Client { rd: BufReader::new(rd), wr, next: 1, notifications: Vec::new() }
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

pub async fn connect(path: &Path) -> Result<UnixStream> {
    UnixStream::connect(path).await.with_context(|| format!("connect {}", path.display()))
}

/// Connect, spawning the server in the background if it isn't running (unless `no_spawn`).
pub async fn connect_or_spawn(session: &str, socket: &Path, no_spawn: bool) -> Result<UnixStream> {
    if let Ok(s) = UnixStream::connect(socket).await {
        return Ok(s);
    }
    if no_spawn {
        bail!("server not running (session {session})");
    }
    spawn_server(session)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
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
    let log = std::fs::OpenOptions::new().create(true).append(true).open(paths.logs().join("server.log"))?;
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.args(["server", "--session", session]).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
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
