//! One device connection (spec 16 §4–§5, §7): hello, Noise handshake, then JSON-RPC over the
//! encrypted channel with scope checks, operation ids and event streaming.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{Sink, SinkExt, Stream, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Mutex, Semaphore, mpsc};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use vk_e2e::hello::{Hello, Mode, version_error};
use vk_e2e::{Responder, Session, b64};

use crate::api::{self, ApiError, Call, OpCheck};
use crate::events::{Fanout, seq_of};
use crate::state::now_s;
use crate::{ConnCmd, Gateway};

pub const IDLE: Duration = Duration::from_secs(60);
/// The device's hello follows the splice at once; an announce that never sends one is dropped
/// quickly so it can't hold a pre-handshake slot.
pub const HELLO_DEADLINE: Duration = Duration::from_secs(3);
/// First Noise message after the hello.
pub const M1_DEADLINE: Duration = Duration::from_secs(5);
pub const MAX_AGE: Duration = Duration::from_secs(12 * 3600);

pub trait Ws:
    Stream<Item = Result<Message, WsError>> + Sink<Message, Error = WsError> + Unpin + Send + 'static
{
}
impl<T> Ws for T where
    T: Stream<Item = Result<Message, WsError>>
        + Sink<Message, Error = WsError>
        + Unpin
        + Send
        + 'static
{
}

async fn recv(ws: &mut impl Ws, wait: Duration) -> Option<Message> {
    let deadline = Instant::now() + wait;
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)))) => continue,
            Ok(Some(Ok(m))) => return Some(m),
            _ => return None,
        }
    }
}

async fn reject(mut ws: impl Ws, text: String) {
    let _ = ws.send(Message::Text(text.into())).await;
    let _ = ws.close().await;
}

/// Serve one spliced device connection to completion.
/// Counts a connection as "dialing" until its handshake finishes (spec 16 §6.4 gateway limits).
pub struct Dialing(Arc<Gateway>);

impl Dialing {
    /// Reserve a pre-handshake slot before dialing the relay (spec 16 §6.4 gateway limits), or
    /// `None` when `limits.max_pending` handshakes are already in progress.
    pub fn try_reserve(gw: &Arc<Gateway>) -> Option<Dialing> {
        reserve_slot(&gw.dialing, gw.limits.max_pending).then(|| Dialing(gw.clone()))
    }
}

/// Take one slot of `counter` if fewer than `max` are taken.
fn reserve_slot(counter: &std::sync::atomic::AtomicUsize, max: usize) -> bool {
    use std::sync::atomic::Ordering::SeqCst;
    counter
        .try_update(SeqCst, SeqCst, |n| (n < max).then_some(n + 1))
        .is_ok()
}

impl Drop for Dialing {
    fn drop(&mut self) {
        self.0
            .dialing
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

pub async fn serve(gw: Arc<Gateway>, mut ws: impl Ws, dialing: Dialing) {
    let Some(Message::Text(hello_raw)) = recv(&mut ws, HELLO_DEADLINE).await else {
        return;
    };
    let prologue = hello_raw.as_bytes().to_vec();
    let hello = match Hello::parse(&prologue) {
        Ok(h) => h,
        Err(_) => return reject(ws, version_error()).await,
    };
    let pairing = match &hello.mode {
        Mode::Pair => {
            // Look the pairing up first: only a claimable pairing spends a handshake budget, and
            // that budget is its own (spec 16 §4.3), so knowing the host id blocks nothing.
            match gw.state.pairing(hello.pid.as_deref().unwrap_or("")) {
                Ok(Some(p))
                    if p.exp > now_s() && p.status == crate::state::PairingStatus::Pending =>
                {
                    if !crate::pair::handshake_allowed(&p.pid, gw.limits.pair_handshakes_per_min) {
                        return reject(ws, json!({"error": "rate_limited"}).to_string()).await;
                    }
                    Some(p)
                }
                _ => {
                    // Unknown, expired or taken: refused without touching any pairing's budget;
                    // past their own budget such hellos get no answer at all.
                    if !crate::pair::unknown_allowed() {
                        return;
                    }
                    return reject(ws, json!({"error": "unauthorized"}).to_string()).await;
                }
            }
        }
        Mode::Device => None,
    };
    let psk = match &pairing {
        Some(p) => match b64::decode_array::<32>(&p.psk) {
            Ok(k) => Some(k),
            Err(_) => return,
        },
        None => None,
    };
    let Ok(mut responder) = Responder::new(&prologue, &gw.keys.noise_private, psk.as_ref()) else {
        return;
    };
    let Some(Message::Binary(m1)) = recv(&mut ws, M1_DEADLINE).await else {
        return;
    };
    let Ok((remote, _)) = responder.read_first(&m1) else {
        return;
    };
    let device = match &pairing {
        Some(_) => None,
        None => match gw.device_by_key(&remote) {
            Some(d) => Some(d),
            None => return reject(ws, json!({"error": "unauthorized"}).to_string()).await,
        },
    };
    let server_version = gw
        .server
        .info
        .lock()
        .unwrap()
        .get("server_version")
        .cloned()
        .unwrap_or(Value::Null);
    let payload = json!({"v": 1, "host_name": gw.host_name, "host_id": gw.keys.host_id(),
                         "gateway_version": env!("CARGO_PKG_VERSION"), "server_version": server_version});
    let Ok((m2, session)) = responder.write_second(payload.to_string().as_bytes()) else {
        return;
    };
    if ws.send(Message::Binary(m2.into())).await.is_err() {
        return;
    }
    drop(dialing);
    match (pairing, device) {
        (Some(p), _) => crate::pair::claim(gw, ws, session, p, remote).await,
        (None, Some(d)) => device_loop(gw, ws, session, d.id).await,
        _ => {}
    }
}

/// Encrypting sender shared by the request handlers and the event forwarder.
#[derive(Clone)]
pub struct Out {
    tx: mpsc::Sender<Value>,
}

impl Out {
    pub async fn send(&self, v: Value) -> bool {
        self.tx.send(v).await.is_ok()
    }
    pub async fn notify(&self, method: &str, params: Value) -> bool {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }
}

/// Split a socket into a decrypting reader half and an encrypting writer task.
pub fn spawn_writer<S>(
    mut sink: S,
    session: Arc<Mutex<Session>>,
) -> (Out, tokio::task::JoinHandle<()>)
where
    S: Sink<Message, Error = WsError> + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::channel::<Value>(1000);
    let h = tokio::spawn(async move {
        while let Some(v) = rx.recv().await {
            let frames = match session.lock().await.encrypt(v.to_string().as_bytes()) {
                Ok(f) => f,
                Err(_) => break,
            };
            for f in frames {
                if tokio::time::timeout(
                    Duration::from_secs(30),
                    sink.send(Message::Binary(f.into())),
                )
                .await
                .map_or(true, |r| r.is_err())
                {
                    return;
                }
            }
        }
        let _ = sink.close().await;
    });
    (Out { tx }, h)
}

async fn device_loop(gw: Arc<Gateway>, ws: impl Ws, session: Session, device_id: String) {
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<ConnCmd>(4);
    if gw.register_conn(&device_id, cmd_tx).is_err() {
        return;
    }
    let session = Arc::new(Mutex::new(session));
    let (sink, mut stream) = ws.split();
    let (out, writer) = spawn_writer(sink, session.clone());
    let inflight = Arc::new(Semaphore::new(gw.limits.max_inflight));
    let started = Instant::now();
    let mut events_task: Option<tokio::task::JoinHandle<()>> = None;
    tracing::info!(device = %device_id, "device connected");

    let mut tasks = tokio::task::JoinSet::new();
    // Liveness counts authenticated traffic only; WebSocket pings don't keep a session alive.
    let mut last_auth = Instant::now();
    loop {
        tasks.try_join_next();
        let remaining = IDLE.saturating_sub(last_auth.elapsed());
        let msg = tokio::select! {
            m = tokio::time::timeout(remaining, stream.next()) => m,
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    ConnCmd::Revoked => {
                        // Revoked: nothing this device queued may still run.
                        tasks.abort_all();
                        out.notify("device.revoked", json!({})).await;
                    }
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                break;
            }
        };
        let frame = match msg {
            Ok(Some(Ok(Message::Binary(b)))) => b,
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)))) => continue,
            _ => break, // idle, closed, error, or a text frame after the handshake
        };
        if started.elapsed() > MAX_AGE {
            break;
        }
        let plain = match session.lock().await.decrypt(&frame) {
            Ok(Some(p)) => {
                last_auth = Instant::now();
                p
            }
            Ok(None) => {
                last_auth = Instant::now();
                continue;
            }
            Err(_) => break,
        };
        // A request may carry a binary payload after a NUL byte (`handoff.write`, spec 16 §15.2).
        let (body, payload) = crate::handoff_peer::split_payload(&plain);
        let Ok(req) = serde_json::from_slice::<Value>(body) else {
            break;
        };
        let payload = payload.map(|b| Arc::new(b.to_vec()));
        let method = req
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let id = req.get("id").cloned();
        let params = req.get("params").cloned().unwrap_or(json!({}));
        let Some(device) = gw.device(&device_id) else {
            out.notify("device.revoked", json!({})).await;
            break;
        };
        gw.touch_visible(&device_id);

        // Connection-level methods obey the same kind and scope rules as everything else.
        if matches!(
            method.as_str(),
            "events.subscribe" | "hello" | "client.visibility"
        ) && !api::kind_allows(&device.kind, &method)
        {
            respond(
                &out,
                id,
                Err(ApiError::new(
                    "forbidden",
                    format!("{method} is not available to a {} device", device.kind),
                )),
            )
            .await;
            continue;
        }
        match method.as_str() {
            "events.subscribe" => {
                if let Some(t) = events_task.take() {
                    t.abort();
                }
                let after = params.get("after").and_then(|a| a.as_u64());
                events_task = Some(tokio::spawn(forward_events(
                    gw.clone(),
                    out.clone(),
                    id,
                    after,
                    api::Allowed::of(&device),
                )));
                continue;
            }
            "hello" | "client.visibility" => {
                if let Some(v) = params.get("visible").and_then(|v| v.as_bool()) {
                    gw.set_visible(&device_id, v);
                }
                if method == "client.visibility" {
                    respond(&out, id, Ok(json!({}))).await;
                    continue;
                }
                let info = gw.server.info.lock().unwrap().clone();
                let r = json!({"host_name": gw.host_name, "host_id": gw.keys.host_id(), "device_id": device.id,
                               "scope": device.scope, "kind": device.kind, "expires_at": device.expires_at, "limit": device.limit, "server_version": info.get("server_version"),
                               "gateway_version": env!("CARGO_PKG_VERSION"),
                               "features": features(&gw)});
                respond(&out, id, Ok(r)).await;
                continue;
            }
            _ => {}
        }

        if payload.is_some() && !crate::handoff_peer::takes_payload(&method) {
            respond(
                &out,
                id,
                Err(ApiError::invalid(format!(
                    "{method} takes no binary payload"
                ))),
            )
            .await;
            continue;
        }

        let Ok(permit) = inflight.clone().try_acquire_owned() else {
            respond(
                &out,
                id,
                Err(ApiError::new("rate_limited", "too many requests in flight")),
            )
            .await;
            continue;
        };
        let (gw, out) = (gw.clone(), out.clone());
        tasks.spawn(async move {
            let r = crate::handoff_peer::PAYLOAD
                .scope(payload, handle(&gw, &device, &method, params))
                .await;
            respond(&out, id, r).await;
            drop(permit);
        });
    }
    if let Some(t) = events_task {
        t.abort();
    }
    gw.set_visible(&device_id, false);
    // Closes this connection's entry in the live list before the report reads it.
    drop(cmd_rx);
    gw.conn_closed();
    // Its last connection gone, the device stops watching browser sessions.
    if !gw.has_live_conn(&device_id) {
        crate::screencast::device_gone(&gw, &device_id).await;
    }
    drop(out);
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
    tracing::info!(device = %device_id, "device disconnected");
}

fn features(gw: &Gateway) -> Vec<&'static str> {
    let mut f = vec![
        "inbox",
        "batch",
        "push",
        "git",
        "transcript",
        "workspace_views",
        // New agents in a worktree or folder, catch-up (`agent.turns`, the assistant), goals,
        // search, browser previews, `clear` pushes and prompt-cache notices.
        "agent_new_workspace",
        "catch_up",
        "goals",
        "search",
        "browser_preview",
        "push_clear",
        "cache_cold",
    ];
    if gw.cfg.stt.is_some() {
        f.push("stt");
    }
    f
}

async fn respond(out: &Out, id: Option<Value>, r: api::ApiResult) {
    let Some(id) = id else { return };
    let v = match r {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e.to_json()}),
    };
    out.send(v).await;
}

async fn handle(
    gw: &Arc<Gateway>,
    device: &crate::state::Device,
    method: &str,
    params: Value,
) -> api::ApiResult {
    if !api::kind_allows(&device.kind, method) {
        return Err(ApiError::new(
            "forbidden",
            format!("{method} is not available to a {} device", device.kind),
        ));
    }
    let Some(need) = api::required_scope(method) else {
        return Err(ApiError::new(
            "method_not_found",
            format!("unknown method {method}"),
        ));
    };
    if device.scope < need {
        return Err(ApiError::new(
            "forbidden",
            format!("{method} needs {} scope", need.as_str()),
        ));
    }
    let call = Call { gw, device };
    if !api::is_mutating(method) {
        return call.dispatch(method, params).await;
    }
    let op_id = params
        .get("op_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if op_id.is_empty() || op_id.len() > 64 {
        return Err(ApiError::invalid("op_id is required for mutating calls"));
    }
    let guard = match gw.ops.reserve(&device.id, &op_id, method, &params) {
        OpCheck::New(g) => g,
        OpCheck::Done(r) => return r,
        OpCheck::Conflict => {
            return Err(ApiError::invalid("op_id reused with different parameters"));
        }
        OpCheck::Wait(mut rx) => {
            return match rx.wait_for(|v| v.is_some()).await {
                Ok(v) => v
                    .clone()
                    .unwrap_or_else(|| Err(ApiError::unavailable("operation cancelled"))),
                Err(_) => Err(ApiError::unavailable(
                    "operation cancelled; refresh before retrying",
                )),
            };
        }
    };
    let r = call.dispatch(method, params.clone()).await;
    guard.complete(&r);
    let target = ["interaction", "pane", "target", "device"]
        .iter()
        .find_map(|k| params.get(*k).cloned());
    gw.state.audit(&json!({"ts": now_s(), "device": device.id, "method": method, "target": target, "op_id": op_id,
                           "outcome": if r.is_ok() { "ok".to_string() } else { r.as_ref().err().map(|e| e.kind.clone()).unwrap_or_default() }}));
    r
}

/// Stream events to one device: subscribe live first, then replay from `after`, dedupe by seq.
/// Which events a share device may see: subjects resolved to panes inside its limit. Rebuilt from
/// the snapshot when an unknown pane/run/interaction appears (at most once a second), unresolved
/// subjects are hidden (spec 16 §15.1).
struct EventScope {
    allowed: api::Allowed,
    panes: Vec<String>,
    runs: Vec<String>,
    interactions: Vec<String>,
    refreshed: Option<Instant>,
}

impl EventScope {
    async fn refresh(&mut self, gw: &Gateway) {
        if self
            .refreshed
            .is_some_and(|t| t.elapsed() < Duration::from_secs(1))
        {
            return;
        }
        self.refreshed = Some(Instant::now());
        let Ok(snap) = gw.server.call("session.snapshot", json!({})).await else {
            return;
        };
        let ids = |k: &str, f: &dyn Fn(&Value) -> bool| -> Vec<String> {
            snap.get(k)
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter(|x| f(x))
                .filter_map(|x| x.get("id").and_then(|i| i.as_str()).map(str::to_string))
                .collect()
        };
        let a = self.allowed.clone();
        self.panes = ids("panes", &|p| {
            a.pane_ok(
                p.get("id").and_then(|i| i.as_str()).unwrap_or(""),
                p.get("workspace").and_then(|w| w.as_str()),
            )
        });
        let panes = self.panes.clone();
        let in_panes = move |x: &Value| {
            x.get("pane")
                .and_then(|p| p.as_str())
                .is_some_and(|p| panes.contains(&p.to_string()))
        };
        self.runs = ids("runs", &in_panes);
        self.interactions = ids("interactions", &in_panes);
    }

    fn known(&self, subject: &Value) -> Option<bool> {
        let get = |k: &str| subject.get(k).and_then(|v| v.as_str());
        if let Some(p) = get("pane") {
            return self.panes.iter().any(|x| x == p).then_some(true);
        }
        if let Some(r) = get("run") {
            return self.runs.iter().any(|x| x == r).then_some(true);
        }
        if let Some(i) = get("interaction") {
            return self.interactions.iter().any(|x| x == i).then_some(true);
        }
        if let (Some(w), None) = (get("workspace"), &self.allowed.pane) {
            return Some(self.allowed.workspace.as_deref() == Some(w));
        }
        Some(false)
    }

    async fn visible(&mut self, gw: &Gateway, e: &Value) -> bool {
        let subject = e.get("subject").cloned().unwrap_or(Value::Null);
        if let Some(v) = self.known(&subject) {
            return v;
        }
        self.refresh(gw).await;
        self.known(&subject) == Some(true)
    }
}

/// Stream events to one device: subscribe live first, then replay from `after`, dedupe by seq.
async fn forward_events(
    gw: Arc<Gateway>,
    out: Out,
    id: Option<Value>,
    after: Option<u64>,
    allowed: Option<api::Allowed>,
) {
    let mut scope = match allowed {
        Some(a) => {
            let mut sc = EventScope {
                allowed: a,
                panes: vec![],
                runs: vec![],
                interactions: vec![],
                refreshed: None,
            };
            sc.refresh(&gw).await;
            Some(sc)
        }
        None => None,
    };
    let mut rx = gw.hub.subscribe();
    let mut last = 0u64;
    match after {
        Some(a) => match gw.hub.replay(a) {
            Some(evs) => {
                respond(&out, id, Ok(json!({"at": a}))).await;
                last = a;
                for e in evs {
                    last = seq_of(&e);
                    if let Some(sc) = scope.as_mut()
                        && !sc.visible(&gw, &e).await
                    {
                        continue;
                    }
                    if !out.notify("event", e).await {
                        return;
                    }
                }
            }
            None => {
                respond(&out, id, Ok(json!({"reset": true}))).await;
                return;
            }
        },
        None => {
            let at = gw.hub.last_seq();
            respond(&out, id, Ok(json!({"at": at}))).await;
        }
    }
    loop {
        match rx.recv().await {
            Ok(Fanout::Event(e)) => {
                if seq_of(&e) <= last {
                    continue;
                }
                last = seq_of(&e);
                if let Some(sc) = scope.as_mut()
                    && !sc.visible(&gw, &e).await
                {
                    continue;
                }
                if !out.notify("event", e).await {
                    return;
                }
            }
            Ok(Fanout::Reset) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                out.notify("events.reset", json!({})).await;
                return;
            }
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn pre_handshake_pool_is_bounded() {
        let c = AtomicUsize::new(0);
        for _ in 0..8 {
            assert!(reserve_slot(&c, 8));
        }
        assert!(!reserve_slot(&c, 8));
        assert_eq!(c.load(Ordering::SeqCst), 8);
        c.fetch_sub(1, Ordering::SeqCst);
        assert!(reserve_slot(&c, 8));
    }
}
