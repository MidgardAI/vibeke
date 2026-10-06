//! The container box link (13 §4, §7): one `<runtime> exec -i <box> vibeke sandbox bridge`
//! stdio stream per box, multiplexed with [`Mux`]. It replaces bind-mounted unix sockets, which
//! the macOS VM-backed runtimes (OrbStack, Docker Desktop) do not forward.
//!
//! Channels:
//! - box → host `egress`: a TCP connection accepted on the box's `127.0.0.1:<port>` (where
//!   `HTTP(S)_PROXY` points) is bridged to the host egress proxy. The box itself runs with
//!   `--network none`; this channel is its only way out.
//! - host → box `listen:<id>`: the box binds `<broker_dir>/<id>.sock` for one pane and keeps it
//!   while the channel stays open; each local connection becomes a box → host `broker:<id>`
//!   channel, which the host bridges to that pane's broker socket.

use crate::mux::{Acceptor, Mux, Stream};
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::io::{AsyncReadExt, AsyncWrite};

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Host side acceptor: `egress` → `127.0.0.1:<proxy_port>`; `broker:<id>` → `<run_dir>/<id>.sock`.
pub fn host_acceptor(proxy_port: Option<u16>, run_dir: PathBuf) -> Acceptor {
    Arc::new(move |kind: String| {
        let run_dir = run_dir.clone();
        Box::pin(async move {
            if kind == "egress" {
                let Some(port) = proxy_port else {
                    bail!("this box has no network (profile none)");
                };
                let s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
                let _ = s.set_nodelay(true);
                return Ok(Box::new(s) as Box<dyn Stream>);
            }
            if let Some(id) = kind.strip_prefix("broker:")
                && valid_id(id)
            {
                let s = tokio::net::UnixStream::connect(run_dir.join(format!("{id}.sock"))).await?;
                return Ok(Box::new(s) as Box<dyn Stream>);
            }
            bail!("channel {kind} is not offered by the host")
        })
    })
}

async fn pump(mut a: Box<dyn Stream>, mut b: tokio::io::DuplexStream) {
    let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
}

/// Box side (`vibeke sandbox bridge`): serve the link over `rd`/`wr`.
pub async fn box_side<R, W>(
    rd: R,
    wr: W,
    tcp: Option<tokio::net::TcpListener>,
    broker_dir: PathBuf,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let cell: Arc<OnceLock<Mux>> = Arc::new(OnceLock::new());
    let c2 = cell.clone();
    let dir = broker_dir.clone();
    let acceptor: Acceptor = Arc::new(move |kind: String| {
        let cell = c2.clone();
        let dir = dir.clone();
        Box::pin(async move {
            let Some(id) = kind.strip_prefix("listen:").filter(|i| valid_id(i)) else {
                bail!("channel {kind} is not offered by the box");
            };
            let id = id.to_string();
            std::fs::create_dir_all(&dir)?;
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            }
            let path = dir.join(format!("{id}.sock"));
            let _ = std::fs::remove_file(&path);
            let l = tokio::net::UnixListener::bind(&path)?;
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
            let (ctl, mut ctl_far) = tokio::io::duplex(64);
            tokio::spawn(async move {
                let mut buf = [0u8; 16];
                loop {
                    tokio::select! {
                        // The host closed the listen channel: stop serving this pane.
                        n = ctl_far.read(&mut buf) => if n.map(|n| n == 0).unwrap_or(true) { break },
                        acc = l.accept() => {
                            let Ok((s, _)) = acc else { break };
                            let Some(m) = cell.get().cloned() else { continue };
                            let id = id.clone();
                            tokio::spawn(async move {
                                if let Ok(ch) = m.open(&format!("broker:{id}")).await {
                                    pump(Box::new(s), ch).await;
                                }
                            });
                        }
                    }
                }
                let _ = std::fs::remove_file(&path);
            });
            Ok(Box::new(ctl) as Box<dyn Stream>)
        })
    });
    let m = Mux::start(rd, wr, "client", Some(acceptor));
    let _ = cell.set(m.clone());
    if let Some(l) = tcp {
        let m2 = m.clone();
        tokio::spawn(async move {
            while let Ok((s, _)) = l.accept().await {
                let _ = s.set_nodelay(true);
                let m = m2.clone();
                tokio::spawn(async move {
                    if let Ok(ch) = m.open("egress").await {
                        pump(Box::new(s), ch).await;
                    }
                });
            }
        });
    }
    m.closed().await;
    Ok(())
}

/// Default in-box socket dir for pane brokers (writable by any box user).
pub fn default_broker_dir() -> &'static Path {
    Path::new("/tmp/vibeke-brokers")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn egress_and_broker_channels_cross_the_link() {
        let t = tempfile::tempdir().unwrap();
        let run_dir = t.path().join("run");
        std::fs::create_dir_all(&run_dir).unwrap();
        // Host: an "egress proxy" that answers, and a pane broker socket.
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pport = proxy.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = proxy.accept().await {
                tokio::spawn(async move {
                    let mut b = [0u8; 4];
                    s.read_exact(&mut b).await.unwrap();
                    s.write_all(b"PROXY").await.unwrap();
                });
            }
        });
        let broker = tokio::net::UnixListener::bind(run_dir.join("p1.sock")).unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = broker.accept().await {
                tokio::spawn(async move {
                    let mut b = [0u8; 4];
                    s.read_exact(&mut b).await.unwrap();
                    s.write_all(b"BROKER").await.unwrap();
                });
            }
        });
        let (a, b) = tokio::io::duplex(1 << 16);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let host = Mux::start(ar, aw, "bridge", Some(host_acceptor(Some(pport), run_dir)));
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let box_port = tcp.local_addr().unwrap().port();
        let bdir = t.path().join("brokers");
        let bd = bdir.clone();
        tokio::spawn(async move { box_side(br, bw, Some(tcp), bd).await });

        // Box process → its loopback proxy port → host proxy.
        let mut c = tokio::net::TcpStream::connect(("127.0.0.1", box_port))
            .await
            .unwrap();
        c.write_all(b"GET ").await.unwrap();
        // The proxy answers and closes: the box client must see EOF (HTTP/1.0 bodies end there).
        let mut out = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(5), c.read_to_end(&mut out))
            .await
            .expect("EOF propagates through the link")
            .unwrap();
        assert_eq!(out, b"PROXY");

        // Host asks the box to listen for pane p1; a hook in the box reaches the host broker.
        let ctl = host.open("listen:p1").await.unwrap();
        let sock = bdir.join("p1.sock");
        for _ in 0..100 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let mut u = tokio::net::UnixStream::connect(&sock).await.unwrap();
        u.write_all(b"ping").await.unwrap();
        let mut out = [0u8; 6];
        u.read_exact(&mut out).await.unwrap();
        assert_eq!(&out, b"BROKER");
        // Bad ids and unknown kinds are refused on both sides.
        assert!(host.open("listen:../x").await.is_err());
        assert!(host.open("tcp:127.0.0.1:22").await.is_err());
        // Closing the listen channel removes the in-box socket.
        drop(ctl);
        for _ in 0..100 {
            if !sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!sock.exists());
    }

    #[tokio::test]
    async fn no_proxy_means_no_egress() {
        let t = tempfile::tempdir().unwrap();
        let acc = host_acceptor(None, t.path().to_path_buf());
        assert!(acc("egress".into()).await.is_err());
        assert!(acc("broker:../../x".into()).await.is_err());
    }
}
