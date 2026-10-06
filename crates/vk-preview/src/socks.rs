//! SOCKS5 (RFC 1928) CONNECT with no authentication, for the Vibeke-managed browser
//! (06 B3.1). Chromium can't do SOCKS username/password auth, so callers are authenticated by
//! peer lookup instead ([`Router::authorize`]); everyone else gets reply `0x02`.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// Reply codes (RFC 1928 §6).
pub mod reply {
    pub const SUCCEEDED: u8 = 0x00;
    pub const GENERAL_FAILURE: u8 = 0x01;
    pub const NOT_ALLOWED: u8 = 0x02;
    pub const NETWORK_UNREACHABLE: u8 = 0x03;
    pub const HOST_UNREACHABLE: u8 = 0x04;
    pub const CONNECTION_REFUSED: u8 = 0x05;
    pub const COMMAND_NOT_SUPPORTED: u8 = 0x07;
    pub const ADDRESS_TYPE_NOT_SUPPORTED: u8 = 0x08;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Addr {
    V4(Ipv4Addr),
    V6(Ipv6Addr),
    Domain(String),
}

/// A CONNECT destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dest {
    pub addr: Addr,
    pub port: u16,
}

impl Dest {
    /// Host as text (`::1` without brackets).
    pub fn host(&self) -> String {
        match &self.addr {
            Addr::V4(a) => a.to_string(),
            Addr::V6(a) => a.to_string(),
            Addr::Domain(d) => d.clone(),
        }
    }

    /// `localhost` / `*.localhost`, 127/8, `::1` (and v4-mapped loopback).
    pub fn is_loopback(&self) -> bool {
        match &self.addr {
            Addr::V4(a) => a.is_loopback(),
            Addr::V6(a) => a.is_loopback() || a.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
            Addr::Domain(d) => {
                let d = d.trim_end_matches('.').to_ascii_lowercase();
                d == "localhost"
                    || d.ends_with(".localhost")
                    || d.parse::<IpAddr>()
                        .is_ok_and(|ip| ip.to_canonical().is_loopback())
            }
        }
    }

    /// `host:port` for a bridge channel kind (`tcp:` / `egress:`), v6 bracketed.
    pub fn channel_target(&self) -> String {
        match &self.addr {
            Addr::V6(a) => format!("[{a}]:{}", self.port),
            _ => format!("{}:{}", self.host(), self.port),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum HandshakeError {
    Io(String),
    /// Not SOCKS5.
    Version(u8),
    /// The client offered no acceptable method (we only do "no authentication").
    NoAcceptableMethod,
    /// Command other than CONNECT (reply 0x07 was sent).
    Command(u8),
    /// Unknown address type (reply 0x08 was sent).
    AddressType(u8),
}

impl From<std::io::Error> for HandshakeError {
    fn from(e: std::io::Error) -> Self {
        HandshakeError::Io(e.to_string())
    }
}

/// Write a reply with a zero `BND.ADDR`.
pub async fn send_reply<S: AsyncWrite + Unpin>(s: &mut S, code: u8) -> std::io::Result<()> {
    s.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
    s.flush().await
}

/// Server side of the greeting and request. On success the caller must send a reply.
pub async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(
    s: &mut S,
) -> Result<Dest, HandshakeError> {
    let mut h = [0u8; 2];
    s.read_exact(&mut h).await?;
    if h[0] != 5 {
        return Err(HandshakeError::Version(h[0]));
    }
    let mut methods = vec![0u8; h[1] as usize];
    s.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        s.write_all(&[5, 0xff]).await?;
        s.flush().await?;
        return Err(HandshakeError::NoAcceptableMethod);
    }
    s.write_all(&[5, 0]).await?;
    s.flush().await?;
    let mut r = [0u8; 4];
    s.read_exact(&mut r).await?;
    if r[0] != 5 {
        return Err(HandshakeError::Version(r[0]));
    }
    let addr = match r[3] {
        1 => {
            let mut a = [0u8; 4];
            s.read_exact(&mut a).await?;
            Addr::V4(Ipv4Addr::from(a))
        }
        3 => {
            let mut n = [0u8; 1];
            s.read_exact(&mut n).await?;
            let mut d = vec![0u8; n[0] as usize];
            s.read_exact(&mut d).await?;
            match String::from_utf8(d) {
                Ok(d) if !d.is_empty() => Addr::Domain(d),
                _ => {
                    send_reply(s, reply::ADDRESS_TYPE_NOT_SUPPORTED).await?;
                    return Err(HandshakeError::AddressType(3));
                }
            }
        }
        4 => {
            let mut a = [0u8; 16];
            s.read_exact(&mut a).await?;
            Addr::V6(Ipv6Addr::from(a))
        }
        t => {
            send_reply(s, reply::ADDRESS_TYPE_NOT_SUPPORTED).await?;
            return Err(HandshakeError::AddressType(t));
        }
    };
    let mut p = [0u8; 2];
    s.read_exact(&mut p).await?;
    if r[1] != 1 {
        send_reply(s, reply::COMMAND_NOT_SUPPORTED).await?;
        return Err(HandshakeError::Command(r[1]));
    }
    Ok(Dest {
        addr,
        port: u16::from_be_bytes(p),
    })
}

/// Client side (tests, tools): no-auth greeting + CONNECT. Returns the reply code.
pub async fn client_connect<S: AsyncRead + AsyncWrite + Unpin>(
    s: &mut S,
    dest: &Dest,
) -> std::io::Result<u8> {
    s.write_all(&[5, 1, 0]).await?;
    let mut m = [0u8; 2];
    s.read_exact(&mut m).await?;
    if m != [5, 0] {
        return Ok(reply::GENERAL_FAILURE);
    }
    let mut req = vec![5, 1, 0];
    match &dest.addr {
        Addr::V4(a) => {
            req.push(1);
            req.extend_from_slice(&a.octets());
        }
        Addr::V6(a) => {
            req.push(4);
            req.extend_from_slice(&a.octets());
        }
        Addr::Domain(d) => {
            req.push(3);
            req.push(d.len() as u8);
            req.extend_from_slice(d.as_bytes());
        }
    }
    req.extend_from_slice(&dest.port.to_be_bytes());
    s.write_all(&req).await?;
    let mut r = [0u8; 4];
    s.read_exact(&mut r).await?;
    let skip = match r[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut n = [0u8; 1];
            s.read_exact(&mut n).await?;
            n[0] as usize
        }
        _ => 0,
    };
    let mut rest = vec![0u8; skip + 2];
    s.read_exact(&mut rest).await?;
    Ok(r[1])
}

/// Map an I/O error from connecting to a SOCKS reply code.
pub fn reply_for(e: &std::io::Error) -> u8 {
    use std::io::ErrorKind::*;
    match e.kind() {
        ConnectionRefused => reply::CONNECTION_REFUSED,
        HostUnreachable => reply::HOST_UNREACHABLE,
        NetworkUnreachable => reply::NETWORK_UNREACHABLE,
        TimedOut => reply::HOST_UNREACHABLE,
        _ => reply::GENERAL_FAILURE,
    }
}

/// Policy for the listener: who may connect and where their traffic goes.
pub trait Router: Send + Sync + 'static {
    /// What a successful peer check yields (e.g. the browser profile).
    type Grant: Clone + Send + Sync + 'static;
    /// Peer check for an accepted connection (`peer` = the client's address, `local` = ours).
    fn authorize(&self, peer: SocketAddr, local: SocketAddr) -> BoxFuture<Option<Self::Grant>>;
    /// Open the upstream stream, or a reply code.
    fn connect(&self, grant: Self::Grant, dest: Dest) -> BoxFuture<Result<Box<dyn Stream>, u8>>;
    /// Observability hook.
    fn rejected(&self, _peer: SocketAddr) {}
}

/// Handle one accepted connection.
pub async fn serve_conn<R: Router, S: AsyncRead + AsyncWrite + Unpin + Send>(
    router: Arc<R>,
    mut s: S,
    peer: SocketAddr,
    local: SocketAddr,
) {
    let dest = match tokio::time::timeout(Duration::from_secs(10), handshake(&mut s)).await {
        Ok(Ok(d)) => d,
        _ => return,
    };
    let Some(grant) = router.authorize(peer, local).await else {
        router.rejected(peer);
        let _ = send_reply(&mut s, reply::NOT_ALLOWED).await;
        return;
    };
    match router.connect(grant, dest).await {
        Ok(mut up) => {
            if send_reply(&mut s, reply::SUCCEEDED).await.is_err() {
                return;
            }
            let _ = tokio::io::copy_bidirectional(&mut s, &mut up).await;
        }
        Err(code) => {
            let _ = send_reply(&mut s, code).await;
        }
    }
}

/// Accept loop. The listener must be bound to loopback (callers use [`bind_loopback`]).
pub async fn serve<R: Router>(listener: TcpListener, router: Arc<R>) {
    let local = listener.local_addr().ok();
    loop {
        let Ok((s, peer)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        if !peer.ip().is_loopback() {
            continue; // unreachable for a loopback bind; defensive
        }
        let _ = s.set_nodelay(true);
        let r = router.clone();
        let local = local.unwrap_or(peer);
        tokio::spawn(serve_conn(r, s, peer, local));
    }
}

/// Bind `127.0.0.1:<port>` (0 = ephemeral). Never a wildcard address (09 §7).
pub async fn bind_loopback(port: u16) -> std::io::Result<TcpListener> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Fake {
        allow: bool,
        seen: Mutex<Vec<Dest>>,
        upstream: u16,
    }

    impl Router for Fake {
        type Grant = ();
        fn authorize(&self, _: SocketAddr, _: SocketAddr) -> BoxFuture<Option<()>> {
            let a = self.allow;
            Box::pin(async move { a.then_some(()) })
        }
        fn connect(&self, _: (), dest: Dest) -> BoxFuture<Result<Box<dyn Stream>, u8>> {
            self.seen.lock().unwrap().push(dest.clone());
            let up = self.upstream;
            Box::pin(async move {
                if dest.port == 1 {
                    return Err(reply::CONNECTION_REFUSED);
                }
                let s = tokio::net::TcpStream::connect(("127.0.0.1", up))
                    .await
                    .map_err(|e| reply_for(&e))?;
                Ok(Box::new(s) as Box<dyn Stream>)
            })
        }
    }

    async fn echo() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        p
    }

    async fn start(allow: bool) -> (u16, Arc<Fake>) {
        let up = echo().await;
        let router = Arc::new(Fake {
            allow,
            seen: Mutex::new(vec![]),
            upstream: up,
        });
        let l = bind_loopback(0).await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(serve(l, router.clone()));
        (port, router)
    }

    #[tokio::test]
    async fn connect_with_each_address_type() {
        let (port, router) = start(true).await;
        let dests = [
            Dest {
                addr: Addr::Domain("localhost".into()),
                port: 5173,
            },
            Dest {
                addr: Addr::V4(Ipv4Addr::LOCALHOST),
                port: 3000,
            },
            Dest {
                addr: Addr::V6(Ipv6Addr::LOCALHOST),
                port: 8080,
            },
        ];
        for d in &dests {
            let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            assert_eq!(client_connect(&mut s, d).await.unwrap(), reply::SUCCEEDED);
            s.write_all(b"ping").await.unwrap();
            let mut b = [0u8; 4];
            s.read_exact(&mut b).await.unwrap();
            assert_eq!(&b, b"ping");
        }
        assert_eq!(*router.seen.lock().unwrap(), dests.to_vec());
        // Upstream failure maps to a failure reply.
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let d = Dest {
            addr: Addr::Domain("localhost".into()),
            port: 1,
        };
        assert_eq!(
            client_connect(&mut s, &d).await.unwrap(),
            reply::CONNECTION_REFUSED
        );
    }

    #[tokio::test]
    async fn peer_check_failure_is_not_allowed() {
        let (port, router) = start(false).await;
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let d = Dest {
            addr: Addr::Domain("localhost".into()),
            port: 5173,
        };
        assert_eq!(
            client_connect(&mut s, &d).await.unwrap(),
            reply::NOT_ALLOWED
        );
        assert!(
            router.seen.lock().unwrap().is_empty(),
            "never connected upstream"
        );
    }

    #[tokio::test]
    async fn protocol_errors() {
        // No acceptable method.
        let (a, mut b) = tokio::io::duplex(1024);
        let h = tokio::spawn(async move {
            let mut a = a;
            handshake(&mut a).await
        });
        b.write_all(&[5, 1, 2]).await.unwrap();
        let mut r = [0u8; 2];
        b.read_exact(&mut r).await.unwrap();
        assert_eq!(r, [5, 0xff]);
        assert_eq!(h.await.unwrap(), Err(HandshakeError::NoAcceptableMethod));
        // BIND is not supported.
        let (a, mut b) = tokio::io::duplex(1024);
        let h = tokio::spawn(async move {
            let mut a = a;
            handshake(&mut a).await
        });
        b.write_all(&[5, 1, 0]).await.unwrap();
        b.read_exact(&mut r).await.unwrap();
        b.write_all(&[5, 2, 0, 1, 127, 0, 0, 1, 0, 80])
            .await
            .unwrap();
        let mut rep = [0u8; 10];
        b.read_exact(&mut rep).await.unwrap();
        assert_eq!(rep[1], reply::COMMAND_NOT_SUPPORTED);
        assert_eq!(h.await.unwrap(), Err(HandshakeError::Command(2)));
        // Unknown address type.
        let (a, mut b) = tokio::io::duplex(1024);
        let h = tokio::spawn(async move {
            let mut a = a;
            handshake(&mut a).await
        });
        b.write_all(&[5, 1, 0]).await.unwrap();
        b.read_exact(&mut r).await.unwrap();
        b.write_all(&[5, 1, 0, 9]).await.unwrap();
        b.read_exact(&mut rep).await.unwrap();
        assert_eq!(rep[1], reply::ADDRESS_TYPE_NOT_SUPPORTED);
        assert_eq!(h.await.unwrap(), Err(HandshakeError::AddressType(9)));
        // SOCKS4 is refused.
        let (a, mut b) = tokio::io::duplex(1024);
        let h = tokio::spawn(async move {
            let mut a = a;
            handshake(&mut a).await
        });
        b.write_all(&[4, 1]).await.unwrap();
        assert_eq!(h.await.unwrap(), Err(HandshakeError::Version(4)));
    }

    #[test]
    fn loopback_classification() {
        let d = |s: &str| Dest {
            addr: Addr::Domain(s.into()),
            port: 1,
        };
        assert!(d("localhost").is_loopback());
        assert!(d("LOCALHOST.").is_loopback());
        assert!(d("app.localhost").is_loopback());
        assert!(d("127.0.0.1").is_loopback());
        assert!(d("::ffff:127.0.0.1").is_loopback());
        assert!(!d("::ffff:10.0.0.1").is_loopback());
        assert!(!d("localhost.example.com").is_loopback());
        assert!(!d("example.com").is_loopback());
        assert!(
            Dest {
                addr: Addr::V6("::ffff:127.0.0.1".parse().unwrap()),
                port: 1
            }
            .is_loopback()
        );
        assert!(
            !Dest {
                addr: Addr::V4("10.0.0.1".parse().unwrap()),
                port: 1
            }
            .is_loopback()
        );
        assert_eq!(
            Dest {
                addr: Addr::V6(Ipv6Addr::LOCALHOST),
                port: 80
            }
            .channel_target(),
            "[::1]:80"
        );
    }
}
