//! The filtering HTTP/CONNECT proxy in front of the agents' headless browser (spec 06 B5).
//!
//! One proxy per browser session, bound to `127.0.0.1:<ephemeral>`, used as that session's
//! browser-context proxy (`Target.createBrowserContext {proxyServer}`, with
//! `proxyBypassList: "<-loopback>"` so loopback goes through it too). Chromium sends plain
//! `http://` requests in absolute form and tunnels everything else (`https:`, `ws:`, `wss:`)
//! with `CONNECT`, so every request type passes here.
//!
//! For each request the proxy resolves the name **once** (`localhost`/`*.localhost` and IP
//! literals without DNS), asks the [`Policy`] about **every** resolved address, and connects
//! only to those addresses — there is no second lookup a rebinding DNS server could answer
//! differently. Denials get `403` with an `X-Vibeke-Denied: <reason>` header and are reported
//! through [`ProxyOptions::on_event`]. Plain HTTP is forwarded with `Connection: close` on both
//! legs, so a proxy connection never carries a second request to another host.

use crate::policy::{self, Decision, Kind, Policy, Reason};
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Name resolution for the proxy (tests inject answers to exercise rebinding).
pub trait Resolver: Send + Sync + 'static {
    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> BoxFuture<'a, io::Result<Vec<IpAddr>>>;
}

/// The system resolver (`getaddrinfo` via tokio).
pub struct SystemResolver;

impl Resolver for SystemResolver {
    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> BoxFuture<'a, io::Result<Vec<IpAddr>>> {
        Box::pin(async move {
            let mut v: Vec<IpAddr> = tokio::net::lookup_host((host, port))
                .await?
                .map(|a| a.ip())
                .collect();
            v.dedup();
            Ok(v)
        })
    }
}

/// IP literals and localhost names resolve without asking DNS; everything else goes to `r`.
pub async fn resolve_with(r: &dyn Resolver, host: &str, port: u16) -> io::Result<Vec<IpAddr>> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![ip]);
    }
    if policy::is_localhost_name(host) {
        return Ok(vec![
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ]);
    }
    r.resolve(host, port).await
}

/// One decision taken by the proxy.
#[derive(Debug, Clone)]
pub struct ProxyEvent {
    /// `http://host:port/path` for plain HTTP, `host:port` for CONNECT.
    pub target: String,
    pub method: String,
    pub host: String,
    pub port: u16,
    pub ips: Vec<IpAddr>,
    pub decision: Decision,
    /// Response status from upstream (plain HTTP only), or the status we answered with.
    pub status: Option<u16>,
}

/// Peer check: `(peer, local)` → may this connection use the proxy?
pub type PeerCheck = Arc<dyn Fn(SocketAddr, SocketAddr) -> BoxFuture<'static, bool> + Send + Sync>;

#[derive(Clone)]
pub struct ProxyOptions {
    /// Called per request, so preview declarations take effect immediately.
    pub policy: Arc<dyn Fn() -> Policy + Send + Sync>,
    pub resolver: Arc<dyn Resolver>,
    pub peer_check: Option<PeerCheck>,
    pub on_event: Arc<dyn Fn(ProxyEvent) + Send + Sync>,
    pub on_rejected_peer: Arc<dyn Fn(SocketAddr) + Send + Sync>,
    pub connect_timeout: Duration,
}

impl ProxyOptions {
    pub fn new(policy: Arc<dyn Fn() -> Policy + Send + Sync>) -> ProxyOptions {
        ProxyOptions {
            policy,
            resolver: Arc::new(SystemResolver),
            peer_check: None,
            on_event: Arc::new(|_| {}),
            on_rejected_peer: Arc::new(|_| {}),
            connect_timeout: Duration::from_secs(10),
        }
    }
}

/// A running proxy; dropping it stops the listener (open tunnels finish on their own).
pub struct ProxyHandle {
    pub port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl ProxyHandle {
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Bind `127.0.0.1:0` and serve. Must be called inside a tokio runtime.
pub async fn start(opts: ProxyOptions) -> io::Result<ProxyHandle> {
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let port = l.local_addr()?.port();
    let opts = Arc::new(opts);
    let task = tokio::spawn(async move {
        loop {
            let Ok((s, peer)) = l.accept().await else {
                continue;
            };
            let opts = opts.clone();
            tokio::spawn(async move {
                if let Some(check) = &opts.peer_check {
                    let local = s
                        .local_addr()
                        .unwrap_or(SocketAddr::from(([127, 0, 0, 1], 0)));
                    if !check(peer, local).await {
                        (opts.on_rejected_peer)(peer);
                        return;
                    }
                }
                let _ = s.set_nodelay(true);
                let _ = handle(s, &opts).await;
            });
        }
    });
    Ok(ProxyHandle { port, task })
}

const MAX_HEAD: usize = 64 * 1024;

/// Read until `\r\n\r\n`. Returns (head, bytes after it).
async fn read_head(s: &mut TcpStream) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 4096];
    loop {
        let n = s.read(&mut tmp).await?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            let rest = buf.split_off(i + 4);
            return Ok((buf, rest));
        }
        if buf.len() > MAX_HEAD {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "head too large"));
        }
    }
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

/// A parsed request head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub method: String,
    pub target: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
}

impl Head {
    pub fn parse(raw: &[u8]) -> Option<Head> {
        let text = std::str::from_utf8(raw).ok()?;
        let mut lines = text.split("\r\n");
        let mut first = lines.next()?.split(' ');
        let method = first.next()?.to_string();
        let target = first.next()?.to_string();
        let version = first.next()?.to_string();
        if first.next().is_some() || !version.starts_with("HTTP/1.") {
            return None;
        }
        let mut headers = Vec::new();
        for l in lines {
            if l.is_empty() {
                continue;
            }
            let (k, v) = l.split_once(':')?;
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
        Some(Head {
            method,
            target,
            version,
            headers,
        })
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn is_upgrade(&self) -> bool {
        self.header("upgrade").is_some()
            && self
                .header("connection")
                .is_some_and(|c| c.to_ascii_lowercase().contains("upgrade"))
    }

    /// The upstream head for a plain-HTTP request: origin-form target, no proxy headers,
    /// `Connection: close` (unless it's an upgrade).
    pub fn upstream(&self, path: &str) -> Vec<u8> {
        let upgrade = self.is_upgrade();
        let mut out = format!("{} {} {}\r\n", self.method, path, self.version);
        for (k, v) in &self.headers {
            let lk = k.to_ascii_lowercase();
            if lk == "proxy-connection" || lk == "proxy-authorization" || lk == "keep-alive" {
                continue;
            }
            if lk == "connection" && !upgrade {
                continue;
            }
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        if !upgrade {
            out.push_str("Connection: close\r\n");
        }
        out.push_str("\r\n");
        out.into_bytes()
    }
}

/// Rewrite an upstream response head to `Connection: close` (unless 101). Returns the status.
pub fn rewrite_response_head(raw: &[u8]) -> Option<(u16, Vec<u8>)> {
    let text = std::str::from_utf8(raw).ok()?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next()?;
    let status: u16 = status_line.split(' ').nth(1)?.parse().ok()?;
    if status == 101 {
        return Some((status, raw.to_vec()));
    }
    let mut out = format!("{status_line}\r\n");
    for l in lines {
        if l.is_empty() {
            continue;
        }
        let lk = l
            .split(':')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if lk == "connection" || lk == "keep-alive" || lk == "proxy-connection" {
            continue;
        }
        out.push_str(l);
        out.push_str("\r\n");
    }
    out.push_str("Connection: close\r\n\r\n");
    Some((status, out.into_bytes()))
}

async fn deny(s: &mut TcpStream, status: u16, reason: &str, what: &str) {
    let text = match status {
        403 => "Forbidden",
        400 => "Bad Request",
        502 => "Bad Gateway",
        _ => "Error",
    };
    let body = format!("vibeke: destination denied ({reason}): {what}\n");
    let resp = format!(
        "HTTP/1.1 {status} {text}\r\nX-Vibeke-Denied: {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = s.write_all(resp.as_bytes()).await;
    let _ = s.shutdown().await;
}

/// Resolve once, decide, and connect to exactly the decided addresses.
pub async fn resolve_decide(
    opts: &ProxyOptions,
    host: &str,
    port: u16,
    kind: Kind,
) -> (Vec<IpAddr>, Decision) {
    let ips = resolve_with(opts.resolver.as_ref(), host, port)
        .await
        .unwrap_or_default();
    let d = (opts.policy)().decide(host, port, &ips, kind);
    (ips, d)
}

async fn connect_pinned(ips: &[IpAddr], port: u16, timeout: Duration) -> io::Result<TcpStream> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, "no address");
    for ip in ips {
        match tokio::time::timeout(timeout, TcpStream::connect(SocketAddr::new(*ip, port))).await {
            Ok(Ok(s)) => {
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Ok(Err(e)) => last = e,
            Err(_) => last = io::Error::new(io::ErrorKind::TimedOut, "connect timed out"),
        }
    }
    Err(last)
}

async fn handle(mut s: TcpStream, opts: &ProxyOptions) -> io::Result<()> {
    let (raw, rest) = tokio::time::timeout(Duration::from_secs(30), read_head(&mut s))
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    let Some(head) = Head::parse(&raw) else {
        deny(&mut s, 400, Reason::BadTarget.as_str(), "malformed request").await;
        return Ok(());
    };
    let event = |target: String, host: String, port: u16, ips: Vec<IpAddr>, d: Decision, status| {
        (opts.on_event)(ProxyEvent {
            target,
            method: head.method.clone(),
            host,
            port,
            ips,
            decision: d,
            status,
        })
    };
    if head.method.eq_ignore_ascii_case("CONNECT") {
        let Some((host, port)) = policy::split_host_port(&head.target, 443) else {
            deny(&mut s, 400, Reason::BadTarget.as_str(), &head.target).await;
            return Ok(());
        };
        let (ips, d) = resolve_decide(opts, &host, port, Kind::Unknown).await;
        if !d.allow {
            event(head.target.clone(), host, port, ips, d, Some(403));
            deny(&mut s, 403, d.reason.as_str(), &head.target).await;
            return Ok(());
        }
        let mut up = match connect_pinned(&ips, port, opts.connect_timeout).await {
            Ok(u) => u,
            Err(_) => {
                event(head.target.clone(), host, port, ips, d, Some(502));
                deny(&mut s, 502, "upstream_unreachable", &head.target).await;
                return Ok(());
            }
        };
        event(head.target.clone(), host, port, ips, d, Some(200));
        s.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        if !rest.is_empty() {
            up.write_all(&rest).await?;
        }
        let _ = tokio::io::copy_bidirectional(&mut s, &mut up).await;
        return Ok(());
    }
    // Plain HTTP in absolute form.
    let Some(t) = policy::parse_target(&head.target).filter(|t| t.scheme == "http") else {
        deny(&mut s, 400, Reason::BadTarget.as_str(), &head.target).await;
        return Ok(());
    };
    let after_scheme = &head.target["http://".len()..];
    let path = match after_scheme.find(['/', '?']) {
        Some(i) if after_scheme.as_bytes()[i] == b'/' => after_scheme[i..].to_string(),
        Some(i) => format!("/{}", &after_scheme[i..]),
        None => "/".to_string(),
    };
    let path = path.split('#').next().unwrap_or("/").to_string();
    let (ips, d) = resolve_decide(opts, &t.host, t.port, Kind::Unknown).await;
    if !d.allow {
        event(head.target.clone(), t.host, t.port, ips, d, Some(403));
        deny(&mut s, 403, d.reason.as_str(), &head.target).await;
        return Ok(());
    }
    let mut up = match connect_pinned(&ips, t.port, opts.connect_timeout).await {
        Ok(u) => u,
        Err(_) => {
            event(head.target.clone(), t.host, t.port, ips, d, Some(502));
            deny(&mut s, 502, "upstream_unreachable", &head.target).await;
            return Ok(());
        }
    };
    up.write_all(&head.upstream(&path)).await?;
    if !rest.is_empty() {
        up.write_all(&rest).await?;
    }
    // Request body (if any) flows client → upstream while we read the response head.
    let (mut cr, mut cw) = s.into_split();
    let (mut ur, mut uw) = up.into_split();
    let to_up = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut cr, &mut uw).await;
        let _ = uw.shutdown().await;
    });
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 8192];
    let mut status = None;
    loop {
        let n = ur.read(&mut tmp).await.unwrap_or(0);
        if n == 0 {
            if !buf.is_empty() {
                let _ = cw.write_all(&buf).await;
            }
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            let body = buf.split_off(i + 4);
            match rewrite_response_head(&buf) {
                Some((st, h)) => {
                    status = Some(st);
                    cw.write_all(&h).await?;
                }
                None => cw.write_all(&buf).await?,
            }
            cw.write_all(&body).await?;
            let _ = tokio::io::copy(&mut ur, &mut cw).await;
            break;
        }
        if buf.len() > MAX_HEAD {
            cw.write_all(&buf).await?;
            let _ = tokio::io::copy(&mut ur, &mut cw).await;
            break;
        }
    }
    let _ = cw.shutdown().await;
    to_up.abort();
    event(head.target.clone(), t.host, t.port, ips, d, status);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::External;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Answers from a script, counting lookups.
    struct Scripted {
        answers: Mutex<Vec<Vec<IpAddr>>>,
        calls: AtomicUsize,
    }

    impl Resolver for Scripted {
        fn resolve<'a>(
            &'a self,
            _host: &'a str,
            _port: u16,
        ) -> BoxFuture<'a, io::Result<Vec<IpAddr>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut a = self.answers.lock().unwrap();
            let v = if a.len() > 1 {
                a.remove(0)
            } else {
                a[0].clone()
            };
            Box::pin(async move { Ok(v) })
        }
    }

    const WS101: &[u8] =
        b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n";

    async fn upstream() -> (u16, Arc<Mutex<Vec<String>>>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = l.accept().await.unwrap();
                let seen = seen2.clone();
                tokio::spawn(async move {
                    let (head, _) = read_head(&mut s).await.unwrap();
                    let text = String::from_utf8_lossy(&head).to_string();
                    seen.lock().unwrap().push(text.clone());
                    if text.starts_with("GET /ws") {
                        s.write_all(WS101).await.unwrap();
                        let mut b = [0u8; 4];
                        s.read_exact(&mut b).await.unwrap();
                        s.write_all(&b).await.unwrap();
                        return;
                    }
                    if text.starts_with("GET /redir") {
                        s.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:5432/x\r\nContent-Length: 0\r\n\r\n").await.unwrap();
                        return;
                    }
                    s.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok",
                    )
                    .await
                    .unwrap();
                });
            }
        });
        (port, seen)
    }

    struct Fixture {
        proxy: ProxyHandle,
        events: Arc<Mutex<Vec<ProxyEvent>>>,
    }

    async fn proxy_with(policy: Policy, resolver: Arc<dyn Resolver>) -> Fixture {
        let events = Arc::new(Mutex::new(Vec::new()));
        let ev2 = events.clone();
        let mut o = ProxyOptions::new(Arc::new(move || policy.clone()));
        o.resolver = resolver;
        o.on_event = Arc::new(move |e| ev2.lock().unwrap().push(e));
        Fixture {
            proxy: start(o).await.unwrap(),
            events,
        }
    }

    async fn roundtrip(port: u16, req: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
        String::from_utf8_lossy(&out).to_string()
    }

    fn sys() -> Arc<dyn Resolver> {
        Arc::new(SystemResolver)
    }

    #[tokio::test]
    async fn http_to_preview_port_is_forwarded_in_origin_form() {
        let (up, seen) = upstream().await;
        let f = proxy_with(
            Policy {
                preview_ports: [up].into(),
                ..Policy::default()
            },
            sys(),
        )
        .await;
        let r = roundtrip(
            f.proxy.port,
            &format!("GET http://localhost:{up}/app?x=1 HTTP/1.1\r\nHost: localhost:{up}\r\nProxy-Connection: keep-alive\r\n\r\n"),
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 200 OK"), "{r}");
        assert!(r.contains("Connection: close"), "{r}");
        assert!(!r.contains("keep-alive"), "{r}");
        assert!(r.ends_with("ok"));
        let head = seen.lock().unwrap()[0].clone();
        assert!(head.starts_with("GET /app?x=1 HTTP/1.1\r\n"), "{head}");
        assert!(head.contains("Connection: close"));
        assert!(!head.to_ascii_lowercase().contains("proxy-connection"));
        let e = f.events.lock().unwrap();
        assert_eq!(e.len(), 1);
        assert!(e[0].decision.allow);
        assert_eq!(e[0].status, Some(200));
    }

    #[tokio::test]
    async fn other_loopback_port_and_metadata_are_denied_without_connecting() {
        let (up, seen) = upstream().await;
        let f = proxy_with(Policy::default(), sys()).await;
        let r = roundtrip(
            f.proxy.port,
            &format!("GET http://127.0.0.1:{up}/ HTTP/1.1\r\nHost: x\r\n\r\n"),
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 403"), "{r}");
        assert!(
            r.contains("X-Vibeke-Denied: loopback_port_not_a_preview"),
            "{r}"
        );
        let r = roundtrip(
            f.proxy.port,
            "GET http://169.254.169.254/latest/meta-data/ HTTP/1.1\r\nHost: 169.254.169.254\r\n\r\n",
        )
        .await;
        assert!(r.contains("X-Vibeke-Denied: metadata_address"), "{r}");
        let r = roundtrip(f.proxy.port, "CONNECT 10.0.0.1:443 HTTP/1.1\r\n\r\n").await;
        assert!(r.contains("X-Vibeke-Denied: private_address"), "{r}");
        let r = roundtrip(
            f.proxy.port,
            "CONNECT [::ffff:127.0.0.1]:22 HTTP/1.1\r\n\r\n",
        )
        .await;
        assert!(
            r.contains("X-Vibeke-Denied: loopback_port_not_a_preview"),
            "{r}"
        );
        assert!(seen.lock().unwrap().is_empty(), "nothing reached upstream");
        let e = f.events.lock().unwrap();
        assert_eq!(e.len(), 4);
        assert!(e.iter().all(|e| !e.decision.allow));
    }

    #[tokio::test]
    async fn connect_tunnel_and_websocket_upgrade() {
        let (up, seen) = upstream().await;
        let f = proxy_with(
            Policy {
                preview_ports: [up].into(),
                ..Policy::default()
            },
            sys(),
        )
        .await;
        // CONNECT (what Chromium uses for ws:// and https:// through a proxy).
        let mut s = TcpStream::connect(("127.0.0.1", f.proxy.port))
            .await
            .unwrap();
        s.write_all(
            format!("CONNECT localhost:{up} HTTP/1.1\r\nHost: localhost:{up}\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
        let mut b = [0u8; 39];
        s.read_exact(&mut b).await.unwrap();
        assert_eq!(&b, b"HTTP/1.1 200 Connection Established\r\n\r\n");
        s.write_all(
            b"GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
        )
        .await
        .unwrap();
        let mut resp = vec![0u8; WS101.len()];
        s.read_exact(&mut resp).await.unwrap();
        assert!(String::from_utf8_lossy(&resp).starts_with("HTTP/1.1 101"));
        s.write_all(b"ping").await.unwrap();
        let mut echo = [0u8; 4];
        s.read_exact(&mut echo).await.unwrap();
        assert_eq!(&echo, b"ping");
        // Absolute-form upgrade keeps `Connection: Upgrade`.
        let mut s = TcpStream::connect(("127.0.0.1", f.proxy.port))
            .await
            .unwrap();
        s.write_all(format!("GET http://localhost:{up}/ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut resp = vec![0u8; WS101.len()];
        s.read_exact(&mut resp).await.unwrap();
        assert!(String::from_utf8_lossy(&resp).starts_with("HTTP/1.1 101"));
        s.write_all(b"pong").await.unwrap();
        s.read_exact(&mut echo).await.unwrap();
        assert_eq!(&echo, b"pong");
        let heads = seen.lock().unwrap().clone();
        assert!(heads.iter().all(|h| h.contains("Connection: Upgrade")));
        // A WebSocket to an undeclared loopback port is refused at CONNECT.
        let r = roundtrip(f.proxy.port, "CONNECT localhost:6001 HTTP/1.1\r\n\r\n").await;
        assert!(
            r.contains("X-Vibeke-Denied: loopback_port_not_a_preview"),
            "{r}"
        );
    }

    #[tokio::test]
    async fn redirect_hop_to_blocked_ip_is_denied() {
        // Chromium follows redirects itself; each hop is a new proxy request and is checked.
        let (up, _) = upstream().await;
        let f = proxy_with(
            Policy {
                preview_ports: [up].into(),
                ..Policy::default()
            },
            sys(),
        )
        .await;
        let ok = roundtrip(
            f.proxy.port,
            &format!("GET http://127.0.0.1:{up}/redir HTTP/1.1\r\nHost: x\r\n\r\n"),
        )
        .await;
        assert!(ok.starts_with("HTTP/1.1 302"), "{ok}");
        assert!(ok.contains("Location: http://127.0.0.1:5432/x"), "{ok}");
        let hop = roundtrip(
            f.proxy.port,
            "GET http://127.0.0.1:5432/x HTTP/1.1\r\nHost: x\r\n\r\n",
        )
        .await;
        assert!(
            hop.contains("X-Vibeke-Denied: loopback_port_not_a_preview"),
            "{hop}"
        );
    }

    #[tokio::test]
    async fn dns_rebinding_resolves_once_and_pins() {
        let (up, seen) = upstream().await;
        // First answer: loopback (a declared preview port) → allowed and pinned. A second
        // lookup would say 10.0.0.1, but the proxy never asks again.
        let r = Arc::new(Scripted {
            answers: Mutex::new(vec![
                vec!["127.0.0.1".parse().unwrap()],
                vec!["10.0.0.1".parse().unwrap()],
            ]),
            calls: AtomicUsize::new(0),
        });
        let f = proxy_with(
            Policy {
                preview_ports: [up].into(),
                ..Policy::default()
            },
            r.clone(),
        )
        .await;
        let out = roundtrip(
            f.proxy.port,
            &format!(
                "GET http://rebind.example:{up}/ HTTP/1.1\r\nHost: rebind.example:{up}\r\n\r\n"
            ),
        )
        .await;
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
        assert_eq!(r.calls.load(Ordering::SeqCst), 1);
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(
            f.events.lock().unwrap()[0].ips,
            vec!["127.0.0.1".parse::<IpAddr>().unwrap()]
        );
        // Mixed answer (public + internal) → denied as a whole.
        let r = Arc::new(Scripted {
            answers: Mutex::new(vec![vec![
                "93.184.216.34".parse().unwrap(),
                "169.254.169.254".parse().unwrap(),
            ]]),
            calls: AtomicUsize::new(0),
        });
        let f = proxy_with(
            Policy {
                external: External::Allow,
                ..Policy::default()
            },
            r.clone(),
        )
        .await;
        let out = roundtrip(f.proxy.port, "CONNECT evil.example:443 HTTP/1.1\r\n\r\n").await;
        assert!(out.contains("X-Vibeke-Denied: metadata_address"), "{out}");
        assert_eq!(r.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn localhost_never_reaches_dns_and_bad_requests_are_refused() {
        let r = Arc::new(Scripted {
            answers: Mutex::new(vec![vec!["93.184.216.34".parse().unwrap()]]),
            calls: AtomicUsize::new(0),
        });
        let f = proxy_with(Policy::default(), r.clone()).await;
        let out = roundtrip(f.proxy.port, "CONNECT app.localhost:3000 HTTP/1.1\r\n\r\n").await;
        assert!(out.contains("loopback_port_not_a_preview"), "{out}");
        assert_eq!(r.calls.load(Ordering::SeqCst), 0);
        let out = roundtrip(f.proxy.port, "GET / HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(out.starts_with("HTTP/1.1 400"), "{out}");
        let out = roundtrip(f.proxy.port, "GET ftp://x/ HTTP/1.1\r\n\r\n").await;
        assert!(out.starts_with("HTTP/1.1 400"), "{out}");
    }

    #[tokio::test]
    async fn peer_check_rejects() {
        let rejected = Arc::new(AtomicUsize::new(0));
        let r2 = rejected.clone();
        let mut o = ProxyOptions::new(Arc::new(Policy::default));
        o.peer_check = Some(Arc::new(|_, _| Box::pin(async { false })));
        o.on_rejected_peer = Arc::new(move |_| {
            r2.fetch_add(1, Ordering::SeqCst);
        });
        let p = start(o).await.unwrap();
        let out = roundtrip(p.port, "CONNECT localhost:1 HTTP/1.1\r\n\r\n").await;
        assert!(out.is_empty());
        assert_eq!(rejected.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn head_parsing_and_rewrites() {
        let h =
            Head::parse(b"GET http://a/b HTTP/1.1\r\nHost: a\r\nConnection: keep-alive\r\n\r\n")
                .unwrap();
        assert_eq!(h.method, "GET");
        assert_eq!(h.header("host"), Some("a"));
        let up = String::from_utf8(h.upstream("/b")).unwrap();
        assert_eq!(
            up,
            "GET /b HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n"
        );
        assert!(Head::parse(b"GET / FTP/1.0\r\n\r\n").is_none());
        assert!(Head::parse(b"garbage\r\n\r\n").is_none());
        let (st, r) = rewrite_response_head(
            b"HTTP/1.1 302 Found\r\nLocation: /x\r\nConnection: keep-alive\r\n\r\n",
        )
        .unwrap();
        assert_eq!(st, 302);
        assert_eq!(
            String::from_utf8(r).unwrap(),
            "HTTP/1.1 302 Found\r\nLocation: /x\r\nConnection: close\r\n\r\n"
        );
    }
}
