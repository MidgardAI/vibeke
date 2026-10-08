//! Client of the Vibeke server's JSON-RPC socket (spec 16 §7.1): one pipelined RPC connection and a
//! dedicated event connection (`events.subscribe` only works on a raw connection).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, mpsc, oneshot};
use vk_proto::rpc::{Request, Response};

use crate::api::ApiError;
use crate::events::Hub;

/// Socket precedence (spec 16 §7.1): explicit, `$VIBEKE_SOCKET` when `$VIBEKE_SESSION` matches,
/// then the server's runtime-dir rule.
pub fn socket_path(explicit: Option<PathBuf>, session: &str) -> PathBuf {
    if let Some(p) = explicit {
        return p;
    }
    if let (Some(sock), Ok(sess)) = (
        std::env::var_os("VIBEKE_SOCKET"),
        std::env::var("VIBEKE_SESSION"),
    ) && sess == session
    {
        return PathBuf::from(sock);
    }
    let root = if let Some(d) = std::env::var_os("VIBEKE_RUNTIME_DIR") {
        PathBuf::from(d)
    } else if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR") {
        PathBuf::from(d).join("vibeke")
    } else {
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        std::env::temp_dir().join(format!("vibeke-{uid}"))
    };
    root.join(session).join("vibeke.sock")
}

type Pending = Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Response>>>>;

struct Conn {
    tx: mpsc::Sender<String>,
    pending: Pending,
    alive: Arc<AtomicBool>,
}

pub struct Server {
    path: PathBuf,
    conn: Mutex<Option<Conn>>,
    next_id: AtomicU64,
    pub info: std::sync::Mutex<Value>,
}

const HELLO: &str = "vibeke-gateway";

impl Server {
    pub fn new(path: PathBuf) -> Arc<Self> {
        Arc::new(Server {
            path,
            conn: Mutex::new(None),
            next_id: AtomicU64::new(1),
            info: std::sync::Mutex::new(Value::Null),
        })
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    async fn connect(&self) -> Result<Conn, ApiError> {
        let stream = UnixStream::connect(&self.path)
            .await
            .map_err(|e| ApiError::unavailable(format!("server: {e}")))?;
        let (r, mut w) = stream.into_split();
        let (tx, mut rx) = mpsc::channel::<String>(256);
        let pending: Pending = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        tokio::spawn({
            let alive = alive.clone();
            async move {
                while let Some(line) = rx.recv().await {
                    if w.write_all(line.as_bytes()).await.is_err()
                        || w.write_all(b"\n").await.is_err()
                    {
                        break;
                    }
                }
                alive.store(false, Ordering::SeqCst);
            }
        });
        tokio::spawn({
            let (pending, alive) = (pending.clone(), alive.clone());
            async move {
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(resp) = serde_json::from_str::<Response>(&line) else {
                        continue;
                    };
                    if let Some(id) = resp.id.as_u64()
                        && let Some(tx) = pending.lock().unwrap().remove(&id)
                    {
                        let _ = tx.send(resp);
                    }
                }
                alive.store(false, Ordering::SeqCst);
                pending.lock().unwrap().clear();
            }
        });
        let conn = Conn { tx, pending, alive };
        let hello = self.call_on(&conn, "client.hello", hello_params()).await?;
        check_full(&hello)?;
        *self.info.lock().unwrap() = hello;
        Ok(conn)
    }

    async fn call_on(&self, conn: &Conn, method: &str, params: Value) -> Result<Value, ApiError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        conn.pending.lock().unwrap().insert(id, tx);
        let line =
            serde_json::to_string(&Request::new(id, method, params)).expect("request serializes");
        if conn.tx.send(line).await.is_err() {
            return Err(ApiError::unavailable("server connection closed"));
        }
        match tokio::time::timeout(Duration::from_secs(60), rx).await {
            Ok(Ok(resp)) => match (resp.result, resp.error) {
                (_, Some(e)) => Err(ApiError::from_server(e)),
                (Some(v), None) => Ok(v),
                (None, None) => Ok(Value::Null),
            },
            Ok(Err(_)) => Err(ApiError::unavailable("server connection closed")),
            Err(_) => {
                conn.pending.lock().unwrap().remove(&id);
                Err(ApiError::unavailable("server timeout"))
            }
        }
    }

    /// Call on behalf of `actor` (e.g. `gateway:Alice's iPhone`): gateway clients must name one on
    /// every mutation (server X4), and the server audits it as `client.action`.
    pub async fn call_as(
        &self,
        actor: &str,
        method: &str,
        mut params: Value,
    ) -> Result<Value, ApiError> {
        if let Some(m) = params.as_object_mut() {
            m.entry("actor").or_insert_with(|| Value::from(actor));
        }
        self.call(method, params).await
    }

    /// Call a server method, (re)connecting if needed.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        let mut guard = self.conn.lock().await;
        if guard
            .as_ref()
            .is_none_or(|c| !c.alive.load(Ordering::SeqCst))
        {
            *guard = Some(self.connect().await?);
        }
        let conn = guard.as_ref().expect("connected");
        // Don't hold the lock across the call: requests are pipelined.
        let c = Conn {
            tx: conn.tx.clone(),
            pending: conn.pending.clone(),
            alive: conn.alive.clone(),
        };
        drop(guard);
        self.call_on(&c, method, params).await
    }
}

fn hello_params() -> Value {
    json!({"client": HELLO, "version": env!("CARGO_PKG_VERSION"), "api": "vibeke/1", "kind": "gateway"})
}

fn check_full(hello: &Value) -> Result<(), ApiError> {
    let caps = hello
        .get("capabilities")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    if caps.iter().any(|c| c == "*") {
        Ok(())
    } else {
        Err(ApiError::new(
            "forbidden",
            "the server gave the gateway pane scope; start vibeke-gateway from a terminal outside Vibeke panes",
        ))
    }
}

/// Keep one `events.subscribe` stream open and feed it into the hub (spec 16 §7.5).
pub async fn run_events(path: PathBuf, hub: Arc<Hub>) {
    let mut backoff = Duration::from_millis(500);
    loop {
        match events_once(&path, &hub).await {
            Ok(()) => backoff = Duration::from_millis(500),
            Err(e) => tracing::debug!("event stream: {e}"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

async fn events_once(path: &PathBuf, hub: &Hub) -> anyhow::Result<()> {
    let stream = UnixStream::connect(path).await?;
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    let hello = serde_json::to_string(&Request::new(1, "client.hello", hello_params()))?;
    w.write_all(format!("{hello}\n").as_bytes()).await?;
    let mut params = json!({});
    if let Some(after) = hub.resume_cursor() {
        params["after"] = after;
    }
    let sub = serde_json::to_string(&Request::new(2, "events.subscribe", params))?;
    w.write_all(format!("{sub}\n").as_bytes()).await?;
    while let Some(line) = lines.next_line().await? {
        let v: Value = serde_json::from_str(&line)?;
        if v.get("id").and_then(|i| i.as_u64()) == Some(2) {
            if let Some(err) = v.get("error") {
                let kind = err
                    .pointer("/data/kind")
                    .and_then(|k| k.as_str())
                    .unwrap_or("");
                if kind == "truncated" {
                    hub.reset();
                }
                anyhow::bail!("subscribe failed: {err}");
            }
            hub.subscribed(v.pointer("/result/at").cloned().unwrap_or(Value::Null));
            continue;
        }
        match v.get("method").and_then(|m| m.as_str()) {
            Some("events.event") => {
                if let Some(ev) = v.pointer("/params/event") {
                    // Not a fact for devices: the server asks this gateway to do something.
                    if ev.get("type").and_then(|t| t.as_str()) == Some("gateway.request") {
                        hub.push_request(ev.get("data").cloned().unwrap_or(Value::Null));
                        continue;
                    }
                    hub.push(ev.clone());
                }
            }
            Some("events.overflow") => anyhow::bail!("overflow; resubscribing"),
            _ => {}
        }
    }
    Ok(())
}
