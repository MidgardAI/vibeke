//! TCP streams to preview destinations for the desktop app's embedded preview (spec 06 B3.5,
//! spec 16 §7.4 `tunnel.*`).
//!
//! Each stream is a server connection that sent `preview.connect` and now carries raw bytes to a
//! loopback port on the preview machine. Streams belong to one encrypted device connection: they
//! close when it ends, when the device closes them, or when the device loses its authorization
//! (checked every second, as for `tui.*`). Data flows to the device as `tunnel.data`
//! notifications, bounded by a per-stream window of unacknowledged bytes.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use futures::FutureExt;
use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, watch};
use vk_e2e::b64;

use crate::Gateway;
use crate::api::{ApiError, ApiResult};
use crate::session::Out;
use crate::state::{Device, Scope};

/// Bytes per `tunnel.data` notification.
pub const CHUNK: usize = 48 * 1024;
/// Decoded bytes per `tunnel.send`.
pub const MAX_SEND: usize = 64 * 1024;
/// Unacknowledged bytes per stream before the gateway stops reading from the server.
pub const WINDOW: u64 = 512 * 1024;
/// Open (and opening) streams per device connection.
pub const MAX_STREAMS: usize = 64;
/// The same for a review device (spec 20 §3.6).
pub const MAX_REVIEW_STREAMS: usize = 32;
/// Queued writes per stream; a client that waits for each `tunnel.send` response never fills it.
const WRITE_QUEUE: usize = 8;
/// Longest server response line read before the stream starts.
const MAX_RESPONSE: u64 = 64 * 1024;

/// Full-scope devices without a limit, and review devices (for their one preview, see
/// [`open_params`]); never other shares, peers or limited devices.
pub fn authorize(device: &Device) -> Result<(), ApiError> {
    if device.is_review() && device.kind == "share" && !device.expired() {
        return Ok(());
    }
    if device.kind != "device"
        || device.scope != Scope::Full
        || device.limit.is_some()
        || device.expired()
    {
        return Err(ApiError::new(
            "forbidden",
            "Preview streams need a paired device with full access",
        ));
    }
    Ok(())
}

/// Bytes sent to the device and acknowledged by it, for one stream.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub sent: u64,
    pub acked: u64,
}

impl Window {
    /// Whether the gateway may read more from the server.
    pub fn can_read(&self) -> bool {
        self.sent.saturating_sub(self.acked) < WINDOW
    }
    /// The device consumed `bytes` more; never past what was sent.
    pub fn ack(&mut self, bytes: u64) {
        self.acked = self.acked.saturating_add(bytes).min(self.sent);
    }
}

/// One queued `tunnel.send`, answered after the write.
struct Write {
    data: Vec<u8>,
    eof: bool,
    done: oneshot::Sender<ApiResult>,
}

struct Handle {
    writes: mpsc::Sender<Write>,
    window: Arc<watch::Sender<Window>>,
    /// The device half-closed its side; later sends are refused.
    write_closed: bool,
    task: tokio::task::AbortHandle,
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Default)]
struct State {
    streams: HashMap<String, Handle>,
    /// `tunnel.open` calls still connecting (they count against [`MAX_STREAMS`]).
    pending: usize,
    next: u64,
    closed: bool,
}

struct Inner {
    gw: Arc<Gateway>,
    device_id: String,
    out: Out,
    state: Mutex<State>,
}

impl Inner {
    fn remove(&self, id: &str) {
        let h = self.state.lock().unwrap().streams.remove(id);
        drop(h);
    }
}

/// Releases a pending-open slot however the open ends (success, error, abort).
struct PendingSlot(Weak<Inner>);

impl Drop for PendingSlot {
    fn drop(&mut self) {
        if let Some(i) = self.0.upgrade() {
            let mut st = i.state.lock().unwrap();
            st.pending = st.pending.saturating_sub(1);
        }
    }
}

/// The streams of one device connection. Dropping it closes them all.
pub struct Tunnels {
    inner: Arc<Inner>,
}

impl Drop for Tunnels {
    fn drop(&mut self) {
        self.close_all();
    }
}

impl Tunnels {
    pub fn new(gw: Arc<Gateway>, device_id: String, out: Out) -> Self {
        Tunnels {
            inner: Arc::new(Inner {
                gw,
                device_id,
                out,
                state: Mutex::new(State::default()),
            }),
        }
    }

    /// Close every stream (disconnect, revocation). Opens still connecting are discarded.
    pub fn close_all(&self) {
        let streams = {
            let mut st = self.inner.state.lock().unwrap();
            st.closed = true;
            std::mem::take(&mut st.streams)
        };
        drop(streams);
    }

    /// Handle one `tunnel.*` request. Work that must keep request order (queueing a write)
    /// happens before this returns; the returned future answers the request and may run
    /// concurrently with later requests.
    pub fn handle(
        &self,
        device: &Device,
        method: &str,
        params: Value,
        id: Option<Value>,
    ) -> BoxFuture<'static, ()> {
        let max = if device.is_review() {
            MAX_REVIEW_STREAMS
        } else {
            MAX_STREAMS
        };
        let out = self.inner.out.clone();
        if method == "tunnel.close" {
            if let Some(s) = params.get("stream").and_then(Value::as_str) {
                self.inner.remove(s);
            }
            return reply(out, id, Ok(json!({})));
        }
        if let Err(e) = authorize(device) {
            return reply(out, id, Err(e));
        }
        match method {
            "tunnel.open" => match open_params(&params, device) {
                Ok(target) => self.open(target, id, max),
                Err(e) => reply(out, id, Err(e)),
            },
            "tunnel.send" => match self.send(&params) {
                Ok(rx) => async move {
                    let r = match rx {
                        Some(rx) => rx
                            .await
                            .unwrap_or_else(|_| Err(ApiError::unavailable("Stream closed"))),
                        None => Ok(json!({})),
                    };
                    crate::session::respond(&out, id, r).await
                }
                .boxed(),
                Err(e) => reply(out, id, Err(e)),
            },
            "tunnel.ack" => reply(out, id, self.ack(&params)),
            _ => reply(
                out,
                id,
                Err(ApiError::new("method_not_found", "Unknown tunnel method")),
            ),
        }
    }

    fn open(&self, target: Value, id: Option<Value>, max: usize) -> BoxFuture<'static, ()> {
        let out = self.inner.out.clone();
        let stream_id = {
            let mut st = self.inner.state.lock().unwrap();
            if st.closed {
                return reply(out, id, Err(ApiError::unavailable("Connection closed")));
            }
            if st.streams.len() + st.pending >= max {
                return reply(
                    out,
                    id,
                    Err(ApiError::new("busy", "Too many open preview streams")),
                );
            }
            st.pending += 1;
            st.next += 1;
            format!("t{}", st.next)
        };
        let slot = PendingSlot(Arc::downgrade(&self.inner));
        let inner = self.inner.clone();
        async move {
            let r = connect(inner.gw.server.path(), target).await;
            let (rd, wr) = match r {
                Ok(s) => s,
                Err(e) => {
                    drop(slot);
                    return crate::session::respond(&out, id, Err(e)).await;
                }
            };
            // The device may have been revoked while the server connected.
            let allowed = inner
                .gw
                .device(&inner.device_id)
                .is_some_and(|d| authorize(&d).is_ok());
            let (start_tx, start_rx) = oneshot::channel();
            let registered = {
                let mut st = inner.state.lock().unwrap();
                if st.closed || !allowed {
                    None
                } else {
                    let (wtx, wrx) = mpsc::channel(WRITE_QUEUE);
                    let window = Arc::new(watch::Sender::new(Window::default()));
                    let task = tokio::spawn(run_stream(
                        Arc::downgrade(&inner),
                        stream_id.clone(),
                        rd,
                        wr,
                        wrx,
                        window.clone(),
                        start_rx,
                    ));
                    st.streams.insert(
                        stream_id.clone(),
                        Handle {
                            writes: wtx,
                            window,
                            write_closed: false,
                            task: task.abort_handle(),
                        },
                    );
                    Some(())
                }
            };
            drop(slot);
            let r = match registered {
                Some(()) => Ok(json!({"stream": stream_id})),
                None if !allowed => Err(ApiError::new(
                    "forbidden",
                    "Preview streams need a paired device with full access",
                )),
                None => Err(ApiError::unavailable("Connection closed")),
            };
            crate::session::respond(&out, id, r).await;
            // Data notifications start only after the device has the stream id.
            let _ = start_tx.send(());
        }
        .boxed()
    }

    /// Queue a write. `Ok(None)`: nothing to write.
    fn send(&self, params: &Value) -> Result<Option<oneshot::Receiver<ApiResult>>, ApiError> {
        let id = params
            .get("stream")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::invalid("Missing stream"))?;
        let data = decode_send(params.get("data_b64").and_then(Value::as_str))?;
        let eof = params.get("eof").and_then(Value::as_bool) == Some(true);
        let mut st = self.inner.state.lock().unwrap();
        let h = st
            .streams
            .get_mut(id)
            .ok_or_else(|| ApiError::new("not_found", "Unknown stream"))?;
        if h.write_closed {
            return Err(ApiError::invalid("The stream's write side is closed"));
        }
        if data.is_empty() && !eof {
            return Ok(None);
        }
        let (done, rx) = oneshot::channel();
        h.writes
            .try_send(Write { data, eof, done })
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => {
                    ApiError::new("busy", "Too many writes queued on this stream")
                }
                mpsc::error::TrySendError::Closed(_) => ApiError::unavailable("Stream closed"),
            })?;
        if eof {
            h.write_closed = true;
        }
        Ok(Some(rx))
    }

    fn ack(&self, params: &Value) -> ApiResult {
        let id = params
            .get("stream")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::invalid("Missing stream"))?;
        let bytes = params
            .get("bytes")
            .and_then(Value::as_u64)
            .ok_or_else(|| ApiError::invalid("Missing bytes"))?;
        // An ack racing a close is harmless.
        if let Some(h) = self.inner.state.lock().unwrap().streams.get(id) {
            h.window.send_modify(|w| w.ack(bytes));
        }
        Ok(json!({}))
    }
}

fn reply(out: Out, id: Option<Value>, r: ApiResult) -> BoxFuture<'static, ()> {
    async move { crate::session::respond(&out, id, r).await }.boxed()
}

/// The `preview.connect` params of a `tunnel.open`: `{preview}` (the server resolves its machine
/// and port, spec 20 §3.6), or `machine`, `host` and `port`. A review device may only use the
/// `{preview}` form, for its own preview.
fn open_params(p: &Value, device: &Device) -> Result<Value, ApiError> {
    let preview = p.get("preview").filter(|v| !v.is_null());
    if let Some(rv) = &device.review {
        if ["host", "port", "machine"]
            .iter()
            .any(|k| p.get(*k).is_some_and(|v| !v.is_null()))
        {
            return Err(ApiError::new(
                "forbidden",
                "A review opens streams to its preview only (tunnel.open {preview})",
            ));
        }
        return match preview.and_then(Value::as_str) {
            Some(v) if v == rv.preview => Ok(json!({"preview": v})),
            _ => Err(ApiError::new(
                "forbidden",
                "A review opens streams to its preview only",
            )),
        };
    }
    if let Some(v) = preview {
        return match v.as_str().filter(|v| !v.is_empty() && v.len() <= 255) {
            Some(v) => Ok(json!({"preview": v})),
            None => Err(ApiError::invalid("Invalid preview")),
        };
    }
    let host = p
        .get("host")
        .and_then(Value::as_str)
        .filter(|h| !h.is_empty() && h.len() <= 255)
        .ok_or_else(|| ApiError::invalid("Missing host"))?;
    let port = p
        .get("port")
        .and_then(Value::as_u64)
        .filter(|n| (1..=65535).contains(n))
        .ok_or_else(|| ApiError::invalid("port must be from 1 to 65535"))?;
    let mut t = json!({"host": host, "port": port});
    match p.get("machine") {
        None | Some(Value::Null) => {}
        Some(Value::String(m)) if m.len() <= 255 => t["machine"] = json!(m),
        Some(_) => return Err(ApiError::invalid("Invalid machine")),
    }
    Ok(t)
}

/// The bytes of one `tunnel.send` (none without `data_b64`), at most [`MAX_SEND`].
fn decode_send(data: Option<&str>) -> Result<Vec<u8>, ApiError> {
    let Some(data) = data else {
        return Ok(vec![]);
    };
    if data.len() > MAX_SEND * 4 / 3 + 4 {
        return Err(ApiError::invalid("Stream data is too large"));
    }
    let data = b64::decode(data).map_err(|_| ApiError::invalid("Invalid stream data encoding"))?;
    if data.len() > MAX_SEND {
        return Err(ApiError::invalid("Stream data is too large"));
    }
    Ok(data)
}

/// A server error from `preview.connect`, passed on in the gateway's error kinds.
fn server_error(e: &Value) -> ApiError {
    let kind = e["data"]["kind"].as_str().unwrap_or("unavailable");
    let kind = match kind {
        "remote_unavailable" | "timeout" => "unavailable",
        "permission_denied" => "forbidden",
        k => k,
    };
    ApiError {
        kind: kind.to_string(),
        message: e["message"]
            .as_str()
            .unwrap_or("Preview stream refused")
            .to_string(),
        details: e["data"]["details"].clone(),
    }
}

type ServerRead = BufReader<tokio::net::unix::OwnedReadHalf>;
type ServerWrite = tokio::net::unix::OwnedWriteHalf;

/// Open a server connection and turn it into a stream with `preview.connect`.
async fn connect(
    path: &std::path::Path,
    target: Value,
) -> Result<(ServerRead, ServerWrite), ApiError> {
    let stream = tokio::time::timeout(Duration::from_secs(5), UnixStream::connect(path))
        .await
        .map_err(|_| ApiError::unavailable("Server connection timed out"))?
        .map_err(|e| ApiError::unavailable(e.to_string()))?;
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);
    let hello = json!({"jsonrpc": "2.0", "id": 1, "method": "client.hello", "params": {
        "client": "vibeke-gateway-tunnel", "kind": "gateway-tunnel", "remote": true, "api": "vibeke/1"}});
    request(&mut rd, &mut wr, hello, Duration::from_secs(5)).await?;
    let connect = json!({"jsonrpc": "2.0", "id": 2, "method": "preview.connect", "params": target});
    // A remote machine's `tcp:` open waits up to 10 s, a local connect up to 5 s per address.
    request(&mut rd, &mut wr, connect, Duration::from_secs(30)).await?;
    Ok((rd, wr))
}

/// One request and its response line. Bytes after the line stay in `rd` (stream data that
/// arrived with the response).
async fn request(
    rd: &mut ServerRead,
    wr: &mut ServerWrite,
    request: Value,
    wait: Duration,
) -> Result<Value, ApiError> {
    let operation = async {
        wr.write_all(format!("{request}\n").as_bytes()).await?;
        wr.flush().await?;
        let mut line = String::new();
        (&mut *rd).take(MAX_RESPONSE).read_line(&mut line).await?;
        Ok::<_, std::io::Error>(line)
    };
    let line = tokio::time::timeout(wait, operation)
        .await
        .map_err(|_| ApiError::unavailable("The host did not answer in time"))?
        .map_err(|e| ApiError::unavailable(e.to_string()))?;
    if !line.ends_with('\n') {
        return Err(ApiError::unavailable("Invalid server response"));
    }
    let v: Value = serde_json::from_str(&line)
        .map_err(|_| ApiError::unavailable("Invalid server response"))?;
    if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
        return Err(server_error(e));
    }
    v.get("result")
        .cloned()
        .ok_or_else(|| ApiError::unavailable("Missing server response"))
}

enum End {
    Eof,
    Error,
    /// The device connection or the stream handle is gone; nothing to tell.
    Gone,
}

async fn read_side<R: AsyncRead + Unpin>(
    mut rd: R,
    window: Arc<watch::Sender<Window>>,
    out: Out,
    stream: String,
) -> End {
    let mut rx = window.subscribe();
    let mut buf = vec![0u8; CHUNK];
    loop {
        if rx.wait_for(Window::can_read).await.is_err() {
            return End::Gone;
        }
        match rd.read(&mut buf).await {
            Ok(0) => return End::Eof,
            Ok(n) => {
                window.send_modify(|w| w.sent += n as u64);
                let data = b64::encode(&buf[..n]);
                if !out
                    .notify("tunnel.data", json!({"stream": stream, "data_b64": data}))
                    .await
                {
                    return End::Gone;
                }
            }
            Err(_) => return End::Error,
        }
    }
}

async fn write_side<W: AsyncWrite + Unpin>(mut wr: W, mut rx: mpsc::Receiver<Write>) -> End {
    while let Some(w) = rx.recv().await {
        let r = async {
            if !w.data.is_empty() {
                wr.write_all(&w.data).await?;
                wr.flush().await?;
            }
            if w.eof {
                wr.shutdown().await?;
            }
            Ok::<_, std::io::Error>(())
        }
        .await;
        match r {
            Ok(()) => {
                let _ = w.done.send(Ok(json!({})));
                if w.eof {
                    return End::Eof;
                }
            }
            Err(e) => {
                let _ = w.done.send(Err(ApiError::unavailable(format!(
                    "Stream write failed: {e}"
                ))));
                return End::Error;
            }
        }
    }
    End::Gone
}

/// One stream: both directions run concurrently (a blocked write never stops reads), with
/// the authorization check every second. Ends with one `tunnel.closed` unless the device
/// connection or the handle is gone; a server-side EOF alone is reported as `eof`.
async fn run_stream(
    inner: Weak<Inner>,
    stream: String,
    rd: ServerRead,
    wr: ServerWrite,
    writes: mpsc::Receiver<Write>,
    window: Arc<watch::Sender<Window>>,
    start: oneshot::Receiver<()>,
) {
    let Some((gw, device_id, out)) = inner
        .upgrade()
        .map(|i| (i.gw.clone(), i.device_id.clone(), i.out.clone()))
    else {
        return;
    };
    if start.await.is_err() {
        return;
    }
    let reader = read_side(rd, window, out.clone(), stream.clone());
    let writer = write_side(wr, writes);
    tokio::pin!(reader, writer);
    let mut check = tokio::time::interval(Duration::from_secs(1));
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let (mut read_done, mut write_done) = (false, false);
    let reason = loop {
        tokio::select! {
            _ = check.tick() => {
                if !gw.device(&device_id).is_some_and(|d| authorize(&d).is_ok()) {
                    break "revoked";
                }
            }
            r = &mut reader, if !read_done => {
                read_done = true;
                match r {
                    End::Eof if write_done => break "closed",
                    End::Eof => {
                        out.notify("tunnel.closed", json!({"stream": stream, "reason": "eof"})).await;
                    }
                    End::Error => break "error",
                    End::Gone => return,
                }
            }
            w = &mut writer, if !write_done => {
                write_done = true;
                match w {
                    End::Eof if read_done => break "closed",
                    End::Eof => {}
                    End::Error => break "error",
                    End::Gone => return,
                }
            }
        }
    };
    out.notify("tunnel.closed", json!({"stream": stream, "reason": reason}))
        .await;
    if let Some(i) = inner.upgrade() {
        i.remove(&stream);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> Device {
        serde_json::from_value(
            json!({"id":"d1", "name":"Desktop", "public":"", "scope":"full", "paired_at":0}),
        )
        .unwrap()
    }

    #[test]
    fn only_full_unlimited_devices_may_open_streams() {
        let mut d = device();
        assert!(authorize(&d).is_ok());
        for scope in [Scope::View, Scope::Approve] {
            d.scope = scope;
            assert_eq!(authorize(&d).unwrap_err().kind, "forbidden");
        }
        d.scope = Scope::Full;
        for kind in ["share", "peer"] {
            d.kind = kind.into();
            assert!(authorize(&d).is_err(), "{kind}");
        }
        // A share with a valid limit and expiry is still refused.
        d.kind = "share".into();
        d.expires_at = Some(u64::MAX);
        d.limit = Some(crate::state::Limit {
            pane: Some("p1".into()),
            workspace: None,
            preview: None,
        });
        assert!(authorize(&d).is_err());
        // A limited device is refused.
        d.kind = "device".into();
        d.expires_at = None;
        assert!(authorize(&d).is_err());
        d.limit = None;
        assert!(authorize(&d).is_ok());
        // An expired device is refused.
        d.expires_at = Some(1);
        assert!(authorize(&d).is_err());
    }

    #[test]
    fn window_stops_reading_at_512_kib_unacknowledged() {
        let mut w = Window::default();
        assert!(w.can_read());
        w.sent = WINDOW - 1;
        assert!(w.can_read());
        w.sent = WINDOW;
        assert!(!w.can_read());
        w.ack(1);
        assert!(w.can_read());
        // Acks never pass what was sent: an over-ack cannot open an unbounded window.
        w.ack(u64::MAX);
        assert_eq!(w.acked, w.sent);
        w.sent += WINDOW;
        assert!(!w.can_read());
    }

    #[test]
    fn send_data_is_bounded() {
        assert!(decode_send(None).unwrap().is_empty());
        let max = vec![7u8; MAX_SEND];
        assert_eq!(decode_send(Some(&b64::encode(&max))).unwrap(), max);
        let over = vec![7u8; MAX_SEND + 1];
        assert!(decode_send(Some(&b64::encode(&over))).is_err());
        assert!(decode_send(Some("not base64!")).is_err());
    }

    fn review_device() -> Device {
        let mut d = device();
        d.kind = "share".into();
        d.scope = Scope::View;
        d.expires_at = Some(u64::MAX);
        d.limit = Some(crate::state::Limit {
            pane: None,
            workspace: None,
            preview: Some("pv1".into()),
        });
        d.review = Some(crate::state::Review {
            preview: "pv1".into(),
            sid: "s".into(),
            ..Default::default()
        });
        d
    }

    #[test]
    fn review_devices_open_streams_to_their_preview_only() {
        let r = review_device();
        assert!(authorize(&r).is_ok());
        assert_eq!(
            open_params(&json!({"preview": "pv1", "x": 1}), &r).unwrap(),
            json!({"preview": "pv1"})
        );
        for bad in [
            json!({"preview": "pv2"}),
            json!({}),
            json!({"preview": 1}),
            json!({"preview": "pv1", "port": 22}),
            json!({"preview": "pv1", "host": "localhost"}),
            json!({"preview": "pv1", "machine": "m2"}),
            json!({"host": "localhost", "port": 5173}),
        ] {
            assert_eq!(
                open_params(&bad, &r).unwrap_err().kind,
                "forbidden",
                "{bad}"
            );
        }
        // Expired: refused like any device.
        let mut old = review_device();
        old.expires_at = Some(1);
        assert!(authorize(&old).is_err());
        // Owners may use the preview form too, unchanged otherwise.
        assert_eq!(
            open_params(&json!({"preview": "v4"}), &device()).unwrap(),
            json!({"preview": "v4"})
        );
        assert!(open_params(&json!({"preview": ""}), &device()).is_err());
    }

    #[test]
    fn open_passes_only_destination_params() {
        let d = device();
        let open_params = |p: &Value| open_params(p, &d);
        let t = open_params(
            &json!({"host": "localhost", "port": 5173, "machine": "devbox",
                                     "client_id": "x", "share": {}}),
        )
        .unwrap();
        assert_eq!(
            t,
            json!({"host": "localhost", "port": 5173, "machine": "devbox"})
        );
        assert!(open_params(&json!({"host": "localhost", "port": 0})).is_err());
        assert!(open_params(&json!({"host": "localhost", "port": 65536})).is_err());
        assert!(open_params(&json!({"port": 80})).is_err());
        assert!(open_params(&json!({"host": "localhost", "port": 80, "machine": 3})).is_err());
    }

    #[test]
    fn server_errors_keep_their_reason() {
        let e = server_error(&json!({"code": -32008, "message": "connect refused",
            "data": {"kind": "remote_unavailable", "details": {"reason": "refused"}}}));
        assert_eq!(e.kind, "unavailable");
        assert_eq!(e.details["reason"], "refused");
        let e = server_error(&json!({"message": "no", "data": {"kind": "invalid_params"}}));
        assert_eq!(e.kind, "invalid_params");
        let e = server_error(&json!({"message": "no", "data": {"kind": "permission_denied"}}));
        assert_eq!(e.kind, "forbidden");
    }
}
