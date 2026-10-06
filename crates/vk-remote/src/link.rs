//! One reconnecting bridge link per machine (06 A1/A4). Used by the TUI/CLI for control and
//! render channels and by the local *server* for preview `tcp:` /
//! `egress:` channels (06 B3.1), each holding its own ControlMaster-backed `ssh` bridge.

use crate::mux::{Mux, futures_util::BoxFuture};
use crate::ssh::Target;
use anyhow::{Result, anyhow, bail};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Session names reach remote shell commands and file names: `[A-Za-z0-9_.-]{1,64}`.
pub fn valid_session_name(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
}

pub fn check_session(s: &str) -> Result<()> {
    if !valid_session_name(s) {
        bail!("invalid session name {s:?}: use 1-64 characters from [A-Za-z0-9_.-]");
    }
    Ok(())
}

/// A live mux plus the transport process (the `ssh` child) that must outlive it.
pub type Conn = (Mux, Option<tokio::process::Child>);

/// Establishes a fresh mux to the machine. The optional child is kept alive with the mux
/// (the `ssh` process; dropping it kills the link).
pub type Connector = Arc<dyn Fn() -> BoxFuture<Result<Conn>> + Send + Sync>;

/// RTT above which a live link counts as degraded (06 A7).
pub const DEGRADED_RTT_MS: u64 = 400;
/// A ping unanswered this long counts as loss (pings go every 5 s).
pub const LOSS_AFTER_MS: u64 = 12_000;

/// Link states (06 A7): `connected → degraded (RTT > 400 ms or loss) → reconnecting → offline`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    Connected,
    Degraded,
    Reconnecting,
    Offline,
}

impl LinkState {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkState::Connected => "connected",
            LinkState::Degraded => "degraded",
            LinkState::Reconnecting => "reconnecting",
            LinkState::Offline => "offline",
        }
    }
}

/// A point-in-time view of a link for the sidebar, `machine show` and `preview.status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkStatus {
    pub state: LinkState,
    pub rtt_ms: Option<u64>,
    /// Wall clock (unix ms) of the last frame received ("last seen 4m ago" while offline).
    pub last_seen_ms: Option<u64>,
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// Channel payload bytes before compression, and how many frames went out compressed.
    pub payload_out: u64,
    pub zstd_frames_out: u64,
    pub remote_version: Option<String>,
}

/// Classify a link (pure, so the thresholds are testable): `live` = a mux is up,
/// `connecting` = a (re)connect attempt is running, `ping_age_ms` = age of the oldest
/// unanswered ping.
pub fn classify(live: bool, connecting: bool, rtt_ms: Option<u64>, ping_age_ms: u64) -> LinkState {
    if live {
        if rtt_ms.is_some_and(|r| r > DEGRADED_RTT_MS) || ping_age_ms > LOSS_AFTER_MS {
            LinkState::Degraded
        } else {
            LinkState::Connected
        }
    } else if connecting {
        LinkState::Reconnecting
    } else {
        LinkState::Offline
    }
}

/// "just now", "42s ago", "4m ago", "3h ago", "2d ago".
pub fn ago(now_ms: u64, then_ms: u64) -> String {
    let s = now_ms.saturating_sub(then_ms) / 1000;
    match s {
        0..=4 => "just now".into(),
        5..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86_399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}

/// A machine connection shared by every channel opened through it; reconnects on demand.
#[derive(Clone)]
pub struct Link {
    label: String,
    connector: Connector,
    mux: Arc<Mutex<Option<Conn>>>,
    /// Lock-free view for status queries while a connect holds `mux`.
    observe: Arc<std::sync::Mutex<Observed>>,
}

#[derive(Default)]
struct Observed {
    mux: Option<Mux>,
    connecting: bool,
    last_seen_ms: Option<u64>,
}

impl Link {
    /// `ssh -T <target> vibeke bridge --session <session>` (with ControlMaster).
    pub fn new(target: Target, session: &str) -> Self {
        let label = target.label.clone();
        let session = session.to_string();
        let connector: Connector = Arc::new(move || {
            let target = target.clone();
            let session = session.clone();
            Box::pin(async move {
                check_session(&session)?;
                let (m, child) = tokio::time::timeout(
                    Duration::from_secs(15),
                    target.bridge(crate::bootstrap::REMOTE_BIN, &session),
                )
                .await
                .map_err(|_| anyhow!("ssh {} timed out", target.address))??;
                Ok((m, Some(child)))
            })
        });
        Link::with_connector(&label, connector)
    }

    /// A link over any transport (tests use an in-process or piped `vibeke bridge`).
    pub fn with_connector(label: &str, connector: Connector) -> Self {
        Link {
            label: label.to_string(),
            connector,
            mux: Arc::new(Mutex::new(None)),
            observe: Arc::default(),
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    async fn mux(&self) -> Result<Mux> {
        let mut g = self.mux.lock().await;
        if g.as_ref().is_none_or(|(m, _)| m.is_closed()) {
            if let Some((old, _)) = g.take() {
                self.remember(&old);
            }
            self.observe.lock().unwrap().connecting = true;
            let r = (self.connector)().await;
            let mut o = self.observe.lock().unwrap();
            o.connecting = false;
            let conn = r?;
            o.mux = Some(conn.0.clone());
            *g = Some(conn);
        }
        Ok(g.as_ref().map(|(m, _)| m.clone()).unwrap())
    }

    /// Keep the last-seen time of a mux that is going away.
    fn remember(&self, m: &Mux) {
        let seen = m
            .stats()
            .last_rx_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        let mut o = self.observe.lock().unwrap();
        if seen > 0 {
            o.last_seen_ms = Some(seen);
        }
        if o.mux.as_ref().is_some_and(|x| x.is_closed()) {
            o.mux = None;
        }
    }

    /// Connect now (without opening a channel) unless already connected.
    pub async fn connect(&self) -> Result<()> {
        self.mux().await.map(|_| ())
    }

    /// Drop the link (`machine disconnect`): every channel through it ends; the next open
    /// reconnects.
    pub async fn disconnect(&self) {
        if let Some((m, child)) = self.mux.lock().await.take() {
            self.remember(&m);
            m.shutdown();
            drop(child); // kill_on_drop ends the ssh process
        }
        self.observe.lock().unwrap().mux = None;
    }

    /// Current state, RTT, last seen and byte counters (never blocks on a running connect).
    pub fn status(&self) -> LinkStatus {
        use std::sync::atomic::Ordering::Relaxed;
        let o = self.observe.lock().unwrap();
        let live = o.mux.as_ref().filter(|m| !m.is_closed());
        let now = crate::mux::now_unix_ms();
        match live {
            Some(m) => {
                let s = m.stats();
                let rtt_us = s.rtt_us.load(Relaxed);
                let rtt_ms = (rtt_us > 0).then_some(rtt_us / 1000);
                let ping = s.ping_outstanding_ms.load(Relaxed);
                let ping_age = if ping == 0 {
                    0
                } else {
                    now.saturating_sub(ping)
                };
                LinkStatus {
                    state: classify(true, o.connecting, rtt_ms, ping_age),
                    rtt_ms,
                    last_seen_ms: Some(s.last_rx_ms.load(Relaxed)),
                    bytes_in: s.bytes_in.load(Relaxed),
                    bytes_out: s.bytes_out.load(Relaxed),
                    payload_out: s.payload_out.load(Relaxed),
                    zstd_frames_out: s.zstd_frames_out.load(Relaxed),
                    remote_version: m.remote_version.lock().unwrap().clone(),
                }
            }
            None => {
                let last = o
                    .mux
                    .as_ref()
                    .map(|m| m.stats().last_rx_ms.load(Relaxed))
                    .filter(|t| *t > 0)
                    .or(o.last_seen_ms);
                LinkStatus {
                    state: classify(false, o.connecting, None, 0),
                    rtt_ms: None,
                    last_seen_ms: last,
                    bytes_in: 0,
                    bytes_out: 0,
                    payload_out: 0,
                    zstd_frames_out: 0,
                    remote_version: None,
                }
            }
        }
    }

    /// Open a channel of `kind` (`socket`, `tcp:<host>:<port>`, `egress:<host>:<port>`).
    /// A timeout drops the link so the next call reconnects; a refused open (e.g. nothing
    /// listening on the port) keeps it.
    pub async fn open_kind(&self, kind: &str) -> Result<tokio::io::DuplexStream> {
        self.open_kind_class(kind, crate::mux::Class::for_kind(kind))
            .await
    }

    /// A `socket` channel scheduled as `class` (render streams, bulk uploads; 06 A4).
    pub async fn open_class(&self, class: crate::mux::Class) -> Result<tokio::io::DuplexStream> {
        self.open_kind_class("socket", class).await
    }

    async fn open_kind_class(
        &self,
        kind: &str,
        class: crate::mux::Class,
    ) -> Result<tokio::io::DuplexStream> {
        let m = self.mux().await?;
        match tokio::time::timeout(Duration::from_secs(10), m.open_class(kind, class)).await {
            Ok(Ok(s)) => Ok(s),
            Ok(Err(e)) => {
                if m.is_closed() {
                    *self.mux.lock().await = None;
                }
                Err(e)
            }
            Err(_) => {
                *self.mux.lock().await = None;
                bail!("machine {} did not answer (offline?)", self.label)
            }
        }
    }

    /// A channel to the remote server's control socket.
    pub async fn open(&self) -> Result<tokio::io::DuplexStream> {
        self.open_kind("socket").await
    }

    pub async fn rtt_ms(&self) -> Option<u64> {
        let g = self.mux.lock().await;
        g.as_ref()
            .map(|(m, _)| m.stats().rtt_us.load(std::sync::atomic::Ordering::Relaxed) / 1000)
    }

    /// Bytes carried so far (in, out), for bandwidth checks.
    pub async fn bytes(&self) -> Option<(u64, u64)> {
        use std::sync::atomic::Ordering::Relaxed;
        let g = self.mux.lock().await;
        g.as_ref().map(|(m, _)| {
            let s = m.stats();
            (s.bytes_in.load(Relaxed), s.bytes_out.load(Relaxed))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_state_thresholds() {
        assert_eq!(classify(true, false, Some(23), 0), LinkState::Connected);
        assert_eq!(classify(true, false, None, 0), LinkState::Connected);
        assert_eq!(classify(true, false, Some(400), 0), LinkState::Connected);
        assert_eq!(classify(true, false, Some(401), 0), LinkState::Degraded);
        // Loss: a ping unanswered for more than two intervals.
        assert_eq!(classify(true, false, Some(20), 13_000), LinkState::Degraded);
        assert_eq!(classify(false, true, None, 0), LinkState::Reconnecting);
        assert_eq!(classify(false, false, None, 0), LinkState::Offline);
        assert_eq!(ago(1_000_000, 1_000_000), "just now");
        assert_eq!(ago(1_000_000, 958_000), "42s ago");
        assert_eq!(ago(1_000_000, 1_000_000 - 4 * 60_000), "4m ago");
        assert_eq!(ago(10_000_000, 0), "2h ago");
    }

    fn fake_connector(count: Arc<std::sync::atomic::AtomicU32>) -> Connector {
        Arc::new(move || {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                let (a, b) = tokio::io::duplex(1 << 16);
                let (ar, aw) = tokio::io::split(a);
                let (br, bw) = tokio::io::split(b);
                let acc = crate::bridge_acceptor(
                    "/nonexistent".into(),
                    || async { anyhow::bail!("none") },
                    crate::BridgeOpts::default(),
                );
                let bridge = Mux::start(br, bw, "bridge", Some(acc));
                tokio::spawn(async move {
                    bridge.closed().await;
                });
                Ok((Mux::start(ar, aw, "client", None), None))
            })
        })
    }

    #[tokio::test]
    async fn status_connect_disconnect_last_seen() {
        let n = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let link = Link::with_connector("fake", fake_connector(n.clone()));
        let st = link.status();
        assert_eq!(st.state, LinkState::Offline);
        assert_eq!(st.last_seen_ms, None);
        link.connect().await.unwrap();
        // The peer's Hello arrives right away.
        for _ in 0..50 {
            if link.status().remote_version.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let st = link.status();
        assert_eq!(st.state, LinkState::Connected);
        assert_eq!(st.remote_version.as_deref(), Some(vk_proto::VERSION));
        assert!(st.bytes_in > 0 && st.last_seen_ms.is_some());
        link.disconnect().await;
        let st = link.status();
        assert_eq!(st.state, LinkState::Offline);
        assert!(st.last_seen_ms.is_some(), "last seen kept after disconnect");
        // Opening again reconnects.
        link.connect().await.unwrap();
        assert_eq!(n.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(link.status().state, LinkState::Connected);
    }

    #[test]
    fn session_names() {
        assert!(valid_session_name("default"));
        assert!(!valid_session_name("a b"));
        assert!(check_session("$(id)").is_err());
    }

    #[tokio::test]
    async fn reconnects_after_link_loss() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let n = Arc::new(AtomicU32::new(0));
        let n2 = n.clone();
        let connector: Connector = Arc::new(move || {
            n2.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let (a, b) = tokio::io::duplex(1 << 16);
                let (ar, aw) = tokio::io::split(a);
                let (br, bw) = tokio::io::split(b);
                let acc = crate::bridge_acceptor(
                    "/nonexistent".into(),
                    || async { anyhow::bail!("none") },
                    crate::BridgeOpts::default(),
                );
                // The bridge side ends when its task is dropped with the duplex.
                let bridge = Mux::start(br, bw, "bridge", Some(acc));
                tokio::spawn(async move {
                    bridge.closed().await;
                });
                Ok((Mux::start(ar, aw, "client", None), None))
            })
        });
        let link = Link::with_connector("fake", connector);
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move { while l.accept().await.is_ok() {} });
        link.open_kind(&format!("tcp:127.0.0.1:{port}"))
            .await
            .unwrap();
        link.open_kind(&format!("tcp:127.0.0.1:{port}"))
            .await
            .unwrap();
        assert_eq!(n.load(Ordering::SeqCst), 1, "one mux for both channels");
        assert!(link.open_kind("tcp:8.8.8.8:53").await.is_err());
        assert_eq!(n.load(Ordering::SeqCst), 1, "a refused open keeps the link");
    }
}
