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

/// A machine connection shared by every channel opened through it; reconnects on demand.
#[derive(Clone)]
pub struct Link {
    label: String,
    connector: Connector,
    mux: Arc<Mutex<Option<Conn>>>,
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
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    async fn mux(&self) -> Result<Mux> {
        let mut g = self.mux.lock().await;
        if g.as_ref().is_none_or(|(m, _)| m.is_closed()) {
            *g = Some((self.connector)().await?);
        }
        Ok(g.as_ref().map(|(m, _)| m.clone()).unwrap())
    }

    /// Open a channel of `kind` (`socket`, `tcp:<host>:<port>`, `egress:<host>:<port>`).
    /// A timeout drops the link so the next call reconnects; a refused open (e.g. nothing
    /// listening on the port) keeps it.
    pub async fn open_kind(&self, kind: &str) -> Result<tokio::io::DuplexStream> {
        let m = self.mux().await?;
        match tokio::time::timeout(Duration::from_secs(10), m.open(kind)).await {
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
