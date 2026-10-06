//! Forward WebSocket messages verbatim between a device and its host (spec 16 §6.3–§6.4).

use std::sync::atomic::Ordering;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};

use crate::Shared;
use crate::limits::Bucket;

/// One active-splice slot (global + per host); taken atomically when a pending connection is
/// accepted and released when the splice ends or the handoff to the client fails.
pub struct Counted {
    relay: Shared,
    host: String,
}

impl Counted {
    /// Take a slot. Call while holding the pending lock that removed the reservation.
    pub fn take(relay: &Shared, host: &str) -> Counted {
        relay.spliced.fetch_add(1, Ordering::SeqCst);
        *relay
            .per_host
            .lock()
            .unwrap()
            .entry(host.to_string())
            .or_default() += 1;
        Counted {
            relay: relay.clone(),
            host: host.to_string(),
        }
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.relay.spliced.fetch_sub(1, Ordering::SeqCst);
        let mut m = self.relay.per_host.lock().unwrap();
        if let Some(n) = m.get_mut(&self.host) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.host);
            }
        }
    }
}

pub async fn run(
    relay: Shared,
    client: WebSocket,
    host_ws: WebSocket,
    host: String,
    _slot: Counted,
) {
    let (mut c_tx, c_rx) = client.split();
    let (mut h_tx, h_rx) = host_ws.split();
    let l = &relay.cfg.limits;
    let (mut up_in, mut down_in) = (0u64, 0u64);
    let outcome = tokio::select! {
        r = forward(c_rx, &mut h_tx, l, &mut up_in) => (r, Side::Client),
        r = forward(h_rx, &mut c_tx, l, &mut down_in) => (r, Side::Host),
    };
    let frame = outcome.0.unwrap_or(CloseFrame {
        code: 1001,
        reason: "peer gone".into(),
    });
    // Propagate the close to whichever side is still open.
    let other = match outcome.1 {
        Side::Client => &mut h_tx,
        Side::Host => &mut c_tx,
    };
    let _ = tokio::time::timeout(l.write_timeout, other.send(Message::Close(Some(frame)))).await;
    relay.auth_usage(&host, up_in, down_in);
    tracing::debug!(
        host = &host[..8],
        up = up_in,
        down = down_in,
        "splice closed"
    );
}

enum Side {
    Client,
    Host,
}

/// Returns the close frame received from `rx`, or `None` on error/idle/limit.
async fn forward(
    mut rx: SplitStream<WebSocket>,
    tx: &mut SplitSink<WebSocket, Message>,
    l: &crate::Limits,
    bytes: &mut u64,
) -> Option<CloseFrame> {
    let mut byte_bucket = Bucket::new(l.conn_burst_bytes as f64, l.conn_bytes_per_sec as f64);
    let mut msg_bucket = Bucket::new(l.conn_msgs_per_sec as f64, l.conn_msgs_per_sec as f64);
    loop {
        let msg = match tokio::time::timeout(l.idle_timeout, rx.next()).await {
            Ok(Some(Ok(m))) => m,
            Ok(Some(Err(_))) | Ok(None) => return None,
            Err(_) => {
                return Some(CloseFrame {
                    code: 1001,
                    reason: "idle".into(),
                });
            }
        };
        let len = match &msg {
            Message::Text(t) => t.len(),
            Message::Binary(b) => b.len(),
            Message::Close(f) => return f.clone(),
            Message::Ping(_) | Message::Pong(_) => continue,
        };
        *bytes += len as u64;
        // Back-pressure: stop reading until the budget allows, instead of buffering.
        let wait = byte_bucket
            .take_wait(len as f64)
            .max(msg_bucket.take_wait(1.0));
        if wait > l.write_timeout {
            return Some(CloseFrame {
                code: vk_e2e::relay::close::RATE_LIMITED,
                reason: "rate limited".into(),
            });
        }
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        match tokio::time::timeout(l.write_timeout, tx.send(msg)).await {
            Ok(Ok(())) => {}
            _ => return None,
        }
    }
}
