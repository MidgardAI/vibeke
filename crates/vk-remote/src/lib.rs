//! Remote machines (06 Part A): the SSH stdio bridge with a channel multiplexer, the no-sudo
//! bootstrap that installs/upgrades the matching binary on the remote, and the remote side
//! (`vibeke bridge`).

pub mod bootstrap;
pub mod link;
pub mod mux;
pub mod ssh;

pub use link::Link;
pub use mux::Mux;
pub use ssh::Target;

use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Remote-side policy for channel kinds other than `socket`.
#[derive(Debug, Clone, Copy)]
pub struct BridgeOpts {
    /// Accept `egress:<host>:<port>` (06 B3.4 `profile_route = "remote"`): outbound TCP from
    /// this machine to any host. `preview.allow_remote_egress` on the remote (default true);
    /// the local server opens such channels only for profiles explicitly routed `remote`.
    pub allow_egress: bool,
}

impl Default for BridgeOpts {
    fn default() -> Self {
        BridgeOpts { allow_egress: true }
    }
}

/// A parsed bridge channel kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelKind {
    /// The remote server's control socket (control/render protocols run unchanged over it).
    Socket,
    /// `tcp:<host>:<port>` — preview forwarding (06 A4 `tcp_forward`); loopback hosts only.
    Tcp { host: String, port: u16 },
    /// `egress:<host>:<port>` — remote-routed browser egress to a non-loopback host.
    Egress { host: String, port: u16 },
}

/// `host:port` with an optional `[v6]` bracket. Port must be 1..=65535.
pub fn split_host_port(s: &str) -> Option<(String, u16)> {
    let (h, p) = s.rsplit_once(':')?;
    let port: u16 = p.parse().ok().filter(|p| *p != 0)?;
    let h = h
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .unwrap_or(h);
    if h.is_empty() || h.len() > 253 || h.chars().any(|c| c.is_whitespace() || c == '/') {
        return None;
    }
    Some((h.to_string(), port))
}

/// Loopback names a `tcp:` channel may reach: `localhost` (and `*.localhost`, RFC 6761),
/// `127.0.0.0/8` and `::1`.
pub fn is_loopback_host(h: &str) -> bool {
    let h = h.trim_end_matches('.').to_ascii_lowercase();
    if h == "localhost" || h.ends_with(".localhost") {
        return true;
    }
    match h.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}

impl ChannelKind {
    pub fn parse(kind: &str) -> Result<ChannelKind> {
        if kind == "socket" {
            return Ok(ChannelKind::Socket);
        }
        if let Some(rest) = kind.strip_prefix("tcp:") {
            let (host, port) = split_host_port(rest).context("tcp: wants <host>:<port>")?;
            if !is_loopback_host(&host) {
                bail!("tcp: channels may only reach loopback (got {host})");
            }
            return Ok(ChannelKind::Tcp { host, port });
        }
        if let Some(rest) = kind.strip_prefix("egress:") {
            let (host, port) = split_host_port(rest).context("egress: wants <host>:<port>")?;
            return Ok(ChannelKind::Egress { host, port });
        }
        bail!("unknown channel kind {kind}")
    }
}

/// Connect to a loopback destination on this machine. `localhost` tries 127.0.0.1 then ::1
/// (dev servers bound only to `::1` are common with Node ≥ 17).
pub async fn connect_loopback(host: &str, port: u16) -> Result<tokio::net::TcpStream> {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    let addrs: Vec<std::net::SocketAddr> = match h.parse::<std::net::IpAddr>() {
        Ok(ip) if ip.is_loopback() => vec![(ip, port).into()],
        Ok(_) => bail!("{host} is not loopback"),
        Err(_) if h == "localhost" || h.ends_with(".localhost") => vec![
            (std::net::Ipv4Addr::LOCALHOST, port).into(),
            (std::net::Ipv6Addr::LOCALHOST, port).into(),
        ],
        Err(_) => bail!("{host} is not loopback"),
    };
    let mut last = None;
    for a in addrs {
        match tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(a)).await
        {
            Ok(Ok(s)) => {
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Ok(Err(e)) => last = Some(anyhow::Error::from(e)),
            Err(_) => last = Some(anyhow::anyhow!("connect {a} timed out")),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no address")))
}

/// The bridge's acceptor: `socket` connects to the server's Unix socket (spawning the server
/// via `ensure_server` first), `tcp:` to a loopback port, `egress:` (if allowed) anywhere.
pub fn bridge_acceptor<F, Fut>(socket: PathBuf, ensure_server: F, opts: BridgeOpts) -> mux::Acceptor
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<()>> + Send + 'static,
{
    let ensure = Arc::new(ensure_server);
    Arc::new(move |kind: String| {
        let socket = socket.clone();
        let ensure = ensure.clone();
        Box::pin(async move {
            match ChannelKind::parse(&kind)? {
                ChannelKind::Socket => {
                    if tokio::net::UnixStream::connect(&socket).await.is_err() {
                        (ensure)().await?;
                    }
                    let s = tokio::net::UnixStream::connect(&socket).await?;
                    Ok(Box::new(s) as Box<dyn mux::Stream>)
                }
                ChannelKind::Tcp { host, port } => {
                    let s = connect_loopback(&host, port).await?;
                    Ok(Box::new(s) as Box<dyn mux::Stream>)
                }
                ChannelKind::Egress { host, port } => {
                    if !opts.allow_egress {
                        bail!("egress is disabled on this machine (preview.allow_remote_egress)");
                    }
                    let s = tokio::time::timeout(
                        Duration::from_secs(10),
                        tokio::net::TcpStream::connect((host.as_str(), port)),
                    )
                    .await
                    .map_err(|_| anyhow::anyhow!("connect {host}:{port} timed out"))??;
                    let _ = s.set_nodelay(true);
                    Ok(Box::new(s) as Box<dyn mux::Stream>)
                }
            }
        })
    })
}

/// Remote side: serve the mux on stdin/stdout (see [`bridge_acceptor`] for channel kinds).
pub async fn run_bridge<F, Fut>(socket: PathBuf, ensure_server: F, opts: BridgeOpts) -> Result<()>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<()>> + Send + 'static,
{
    let acceptor = bridge_acceptor(socket, ensure_server, opts);
    let m = Mux::start(
        tokio::io::stdin(),
        tokio::io::stdout(),
        "bridge",
        Some(acceptor),
    );
    m.closed().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn channel_kinds() {
        assert_eq!(ChannelKind::parse("socket").unwrap(), ChannelKind::Socket);
        assert_eq!(
            ChannelKind::parse("tcp:127.0.0.1:5173").unwrap(),
            ChannelKind::Tcp {
                host: "127.0.0.1".into(),
                port: 5173
            }
        );
        assert_eq!(
            ChannelKind::parse("tcp:[::1]:80").unwrap(),
            ChannelKind::Tcp {
                host: "::1".into(),
                port: 80
            }
        );
        assert_eq!(
            ChannelKind::parse("tcp:::1:80").unwrap(),
            ChannelKind::Tcp {
                host: "::1".into(),
                port: 80
            }
        );
        assert!(ChannelKind::parse("tcp:localhost:3000").is_ok());
        assert!(ChannelKind::parse("tcp:app.localhost:3000").is_ok());
        assert!(ChannelKind::parse("tcp:127.1.2.3:3000").is_ok());
        for bad in [
            "tcp:10.0.0.1:80",
            "tcp:example.com:80",
            "tcp:169.254.169.254:80",
            "tcp:0.0.0.0:80",
            "tcp:localhost.evil.com:80",
            "tcp:127.0.0.1:0",
            "tcp:127.0.0.1",
            "tcp::80",
            "nope",
        ] {
            assert!(ChannelKind::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(
            ChannelKind::parse("egress:example.com:443").unwrap(),
            ChannelKind::Egress {
                host: "example.com".into(),
                port: 443
            }
        );
    }

    async fn echo_listener() -> u16 {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        port
    }

    fn pair(opts: BridgeOpts) -> (Mux, Mux) {
        let (a, b) = tokio::io::duplex(1 << 20);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let acc = bridge_acceptor(
            PathBuf::from("/nonexistent/vibeke.sock"),
            || async { anyhow::bail!("no server in this test") },
            opts,
        );
        let bridge = Mux::start(br, bw, "bridge", Some(acc));
        let client = Mux::start(ar, aw, "client", None);
        (bridge, client)
    }

    #[tokio::test]
    async fn tcp_channel_reaches_loopback_and_refuses_others() {
        let port = echo_listener().await;
        let (_b, client) = pair(BridgeOpts::default());
        for host in ["127.0.0.1", "localhost"] {
            let mut s = client.open(&format!("tcp:{host}:{port}")).await.unwrap();
            s.write_all(b"hello").await.unwrap();
            let mut buf = [0u8; 5];
            s.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"hello");
        }
        // Non-loopback is refused before any connect.
        let e = client.open("tcp:10.255.255.1:80").await.unwrap_err();
        assert!(format!("{e:#}").contains("loopback"), "{e:#}");
        let e = client.open("tcp:example.com:80").await.unwrap_err();
        assert!(format!("{e:#}").contains("loopback"), "{e:#}");
        // A closed loopback port fails the open (the SOCKS side maps this to a failure reply).
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        assert!(
            client
                .open(&format!("tcp:127.0.0.1:{closed}"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn egress_is_gated() {
        let port = echo_listener().await;
        let (_b, client) = pair(BridgeOpts {
            allow_egress: false,
        });
        let e = client
            .open(&format!("egress:127.0.0.1:{port}"))
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("disabled"), "{e:#}");
        let (_b2, client2) = pair(BridgeOpts { allow_egress: true });
        let mut s = client2
            .open(&format!("egress:127.0.0.1:{port}"))
            .await
            .unwrap();
        s.write_all(b"x").await.unwrap();
        let mut b = [0u8; 1];
        s.read_exact(&mut b).await.unwrap();
    }
}
