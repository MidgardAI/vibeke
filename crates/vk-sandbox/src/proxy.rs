//! Host-side egress proxy (13 §7): an HTTP CONNECT + absolute-form forward proxy and a SOCKS5
//! (`CONNECT`, no auth) listener on the same `127.0.0.1:<port>` (and optionally a unix socket for
//! Linux net namespaces), one per contained task. The protocol is told apart by the first byte. Every destination is checked against the task's [`EgressPolicy`]; names are resolved
//! **by the proxy** and each resolved address is checked again (loopback only for declared
//! ports, never link-local/metadata, private ranges only when allowed), and the proxy connects
//! to exactly the address it checked (no second resolution, so no DNS rebinding window).
//!
//! Destinations on no list are asked about through an [`Asker`] (the server turns that into an
//! egress Interaction). No asker, a timeout, or any error → **deny** (fail closed, 13 §7).
//! The proxy never logs request bodies or headers; events carry host, port and rule only.
//!
//! Domain fronting (13 §7, SNI/Host cross-check): a tunnel's TLS ClientHello must name the host
//! that was checked (or another host the policy allows), and an absolute-form request's `Host`
//! header must match its target. Non-TLS tunnels are relayed unchanged; server-first protocols
//! are never delayed (the server → client direction starts at once).

use crate::net::{EgressPolicy, HostVerdict};
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

pub type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Answer to "agent wants to reach `host:port`".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskDecision {
    AllowOnce,
    AllowTask,
    /// Allow this endpoint for every contained task, persistently (the server stores it).
    AllowAlways,
    Deny,
}

/// Asks the user (egress Interaction) about a destination on no list.
pub trait Asker: Send + Sync + 'static {
    fn ask(&self, host: String, port: u16) -> BoxFut<AskDecision>;
}

/// Name resolution (system DNS by default; tests inject fixed answers).
pub trait Resolver: Send + Sync + 'static {
    fn resolve(&self, host: String, port: u16) -> BoxFut<std::io::Result<Vec<SocketAddr>>>;
}

pub struct SystemResolver;

impl Resolver for SystemResolver {
    fn resolve(&self, host: String, port: u16) -> BoxFut<std::io::Result<Vec<SocketAddr>>> {
        Box::pin(async move {
            Ok(tokio::net::lookup_host((host.as_str(), port))
                .await?
                .collect())
        })
    }
}

/// What happened to one connection attempt (becomes `sandbox.egress_*` events).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressEvent {
    Allowed {
        host: String,
        port: u16,
        rule: String,
    },
    Denied {
        host: String,
        port: u16,
        reason: String,
    },
}

pub type Observer = Arc<dyn Fn(EgressEvent) + Send + Sync>;

#[derive(Clone)]
pub struct ProxyConfig {
    pub policy: Arc<RwLock<EgressPolicy>>,
    pub asker: Option<Arc<dyn Asker>>,
    pub resolver: Arc<dyn Resolver>,
    pub observer: Option<Observer>,
    /// How long a connection is held while the user decides (13 §7: ≤ 30 s).
    pub ask_timeout: Duration,
    pub connect_timeout: Duration,
    /// Check a tunnel's TLS SNI and a forwarded request's `Host` against the checked host.
    pub sni_check: bool,
    /// How long a tunnel waits for the client's first bytes to be classified (a whole
    /// ClientHello, or non-TLS). Undecided at the deadline, the tunnel closes.
    pub sni_timeout: Duration,
}

impl ProxyConfig {
    pub fn new(policy: EgressPolicy) -> Self {
        ProxyConfig {
            policy: Arc::new(RwLock::new(policy)),
            asker: None,
            resolver: Arc::new(SystemResolver),
            observer: None,
            ask_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            sni_check: true,
            sni_timeout: Duration::from_secs(30),
        }
    }
}

/// A running proxy. Dropping it stops the listeners (open tunnels finish on their own).
pub struct EgressProxy {
    pub port: u16,
    pub unix_socket: Option<PathBuf>,
    pub policy: Arc<RwLock<EgressPolicy>>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for EgressProxy {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
        if let Some(u) = &self.unix_socket {
            let _ = std::fs::remove_file(u);
        }
    }
}

impl EgressProxy {
    /// Listen on `127.0.0.1:port` (0 = ephemeral) and, if given, a unix socket.
    pub async fn start(
        cfg: ProxyConfig,
        port: u16,
        unix: Option<PathBuf>,
    ) -> std::io::Result<EgressProxy> {
        let tcp = TcpListener::bind(("127.0.0.1", port)).await?;
        let port = tcp.local_addr()?.port();
        let mut tasks = Vec::new();
        let c = cfg.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                let Ok((s, _)) = tcp.accept().await else {
                    continue;
                };
                let c = c.clone();
                tokio::spawn(async move {
                    let _ = handle(c, s).await;
                });
            }
        }));
        if let Some(path) = &unix {
            let _ = std::fs::remove_file(path);
            let l = tokio::net::UnixListener::bind(path)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            let c = cfg.clone();
            tasks.push(tokio::spawn(async move {
                loop {
                    let Ok((s, _)) = l.accept().await else {
                        continue;
                    };
                    let c = c.clone();
                    tokio::spawn(async move {
                        let _ = handle(c, s).await;
                    });
                }
            }));
        }
        Ok(EgressProxy {
            port,
            unix_socket: unix,
            policy: cfg.policy.clone(),
            tasks,
        })
    }

    /// "Allow for task": add the approved endpoint (`host:port`) to the task allowlist.
    pub fn allow_for_task(&self, host: &str, port: u16) {
        if let Ok(mut p) = self.policy.write() {
            p.task_allow
                .insert(format!("{}:{port}", host.to_ascii_lowercase()));
        }
    }

    /// "Allow always": add a global entry (`host` or `host:port`) to this proxy's policy.
    pub fn allow_global(&self, entry: &str) {
        if let Ok(mut p) = self.policy.write() {
            p.global_allow.insert(entry.to_ascii_lowercase());
        }
    }

    /// Drop a global entry (`sandbox.disallow {global}`).
    pub fn remove_global(&self, entry: &str) {
        if let Ok(mut p) = self.policy.write() {
            p.global_allow.remove(&entry.to_ascii_lowercase());
        }
    }
}

const MAX_HEAD: usize = 32 * 1024;

struct Head {
    method: String,
    host: String,
    port: u16,
    /// Rewritten request head for absolute-form requests (None for CONNECT).
    forward: Option<Vec<u8>>,
    /// Bytes read past the head (request body start, or early TLS bytes).
    rest: Vec<u8>,
    /// Request body length of an absolute-form request (`Content-Length`, 0 without one).
    body_len: u64,
    /// The `Host` header of an absolute-form request (host part, lowercased), if any.
    host_header: Option<String>,
}

fn split_host_port(s: &str, default_port: u16) -> Option<(String, u16)> {
    if let Some(rest) = s.strip_prefix('[') {
        let (h, tail) = rest.split_once(']')?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None if tail.is_empty() => default_port,
            None => return None,
        };
        return Some((h.to_string(), port));
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => Some((h.to_string(), p.parse().ok()?)),
        Some(_) => None, // bare IPv6 without brackets
        None => Some((s.to_string(), default_port)),
    }
}

fn parse_head(buf: &[u8], end: usize) -> Result<Head, &'static str> {
    let text = std::str::from_utf8(&buf[..end]).map_err(|_| "non-utf8 request head")?;
    let mut lines = text.split("\r\n");
    let first = lines.next().ok_or("empty request")?;
    let mut parts = first.split(' ');
    let method = parts.next().ok_or("bad request line")?.to_string();
    let target = parts.next().ok_or("bad request line")?;
    let version = parts.next().unwrap_or("HTTP/1.1");
    let rest = buf[end + 4..].to_vec();
    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_host_port(target, 443).ok_or("bad CONNECT target")?;
        return Ok(Head {
            method,
            host,
            port,
            forward: None,
            rest,
            body_len: 0,
            host_header: None,
        });
    }
    let after = target
        .strip_prefix("http://")
        .ok_or("only CONNECT and absolute http:// requests are proxied")?;
    let (authority, path) = match after.find('/') {
        Some(i) => (&after[..i], &after[i..]),
        None => (after, "/"),
    };
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let (host, port) = split_host_port(authority, 80).ok_or("bad request target")?;
    let mut out = format!("{method} {path} {version}\r\n");
    let mut body_len: Option<u64> = None;
    let mut host_header = None;
    for l in lines {
        if l.is_empty() {
            continue;
        }
        let name = l
            .split(':')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if name == "transfer-encoding" {
            // Exactly one request is forwarded per connection; that needs a known body length.
            return Err("chunked/encoded request bodies are not proxied (send Content-Length)");
        }
        if name == "host" {
            let v = l.split_once(':').map(|(_, v)| v.trim()).unwrap_or("");
            host_header = split_host_port(v, port).map(|(h, _)| h.to_ascii_lowercase());
        }
        if name == "content-length" {
            let n: u64 = l
                .split_once(':')
                .map(|(_, v)| v.trim())
                .and_then(|v| v.parse().ok())
                .ok_or("bad Content-Length")?;
            if body_len.is_some_and(|b| b != n) {
                return Err("conflicting Content-Length headers");
            }
            body_len = Some(n);
        }
        if matches!(
            name.as_str(),
            "proxy-authorization" | "proxy-connection" | "connection" | "keep-alive"
        ) {
            continue;
        }
        out.push_str(l);
        out.push_str("\r\n");
    }
    // One request per upstream connection: a keep-alive connection must not carry a second
    // request to a different host past the policy check. `Connection: close` asks the origin to
    // end after its response; [`handle`] enforces it by forwarding exactly this head and
    // `body_len` body bytes, never anything the client sends after them.
    out.push_str("Connection: close\r\n\r\n");
    Ok(Head {
        method,
        host,
        port,
        forward: Some(out.into_bytes()),
        rest,
        body_len: body_len.unwrap_or(0),
        host_header,
    })
}

#[cfg(test)]
async fn read_head<S: AsyncRead + Unpin>(s: &mut S) -> Result<(Vec<u8>, usize), &'static str> {
    read_head_from(s, Vec::with_capacity(4096)).await
}

/// [`read_head`] with bytes already read from the stream (the protocol sniff).
async fn read_head_from<S: AsyncRead + Unpin>(
    s: &mut S,
    mut buf: Vec<u8>,
) -> Result<(Vec<u8>, usize), &'static str> {
    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
        return Ok((buf, i));
    }
    let mut chunk = [0u8; 4096];
    loop {
        let n = s.read(&mut chunk).await.map_err(|_| "read error")?;
        if n == 0 {
            return Err("connection closed");
        }
        let from = buf.len().saturating_sub(3);
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf[from..].windows(4).position(|w| w == b"\r\n\r\n") {
            return Ok((buf, from + i));
        }
        if buf.len() > MAX_HEAD {
            return Err("request head too large");
        }
    }
}

async fn respond<S: AsyncWrite + Unpin>(s: &mut S, status: &str, body: &str) {
    let msg = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nX-Vibeke-Egress: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
        if status.starts_with('2') {
            "allowed"
        } else {
            "denied"
        }
    );
    let _ = s.write_all(msg.as_bytes()).await;
    let _ = s.shutdown().await;
}

/// Decide and resolve: the addresses the proxy may connect to, or the denial reason.
pub async fn decide(
    cfg: &ProxyConfig,
    host: &str,
    port: u16,
) -> Result<(Vec<SocketAddr>, String), String> {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let verdict = cfg
        .policy
        .read()
        .map(|p| p.check_host(&host, port))
        .unwrap_or(HostVerdict::Deny {
            reason: "policy unavailable".into(),
        });
    let rule = match verdict {
        HostVerdict::Allow { rule } => rule,
        HostVerdict::Deny { reason } => return Err(reason),
        HostVerdict::Ask => {
            let Some(asker) = &cfg.asker else {
                return Err("not on the allowlist".into());
            };
            match tokio::time::timeout(cfg.ask_timeout, asker.ask(host.clone(), port)).await {
                Ok(AskDecision::AllowOnce) => "approved once".to_string(),
                Ok(AskDecision::AllowTask) => {
                    // The approval names one endpoint: `host:port`, never the whole host.
                    let entry = format!("{host}:{port}");
                    if let Ok(mut p) = cfg.policy.write() {
                        p.task_allow.insert(entry.clone());
                    }
                    format!("task:{entry}")
                }
                Ok(AskDecision::AllowAlways) => {
                    let entry = format!("{host}:{port}");
                    if let Ok(mut p) = cfg.policy.write() {
                        p.global_allow.insert(entry.clone());
                    }
                    format!("global:{entry}")
                }
                Ok(AskDecision::Deny) => return Err("denied by user".into()),
                Err(_) => return Err("no decision in time (fail closed)".into()),
            }
        }
    };
    let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => cfg
            .resolver
            .resolve(host.clone(), port)
            .await
            .map_err(|e| format!("resolve {host}: {e}"))?,
    };
    let policy = cfg
        .policy
        .read()
        .map_err(|_| "policy unavailable".to_string())?
        .clone();
    let mut refused = None;
    let ok: Vec<SocketAddr> = addrs
        .into_iter()
        .filter(|a| match policy.check_ip(a.ip(), port) {
            Ok(()) => true,
            Err(c) => {
                refused.get_or_insert(c);
                false
            }
        })
        .collect();
    if ok.is_empty() {
        return Err(match refused {
            Some(c) => format!("{host} resolves to a {} address", c.as_str()),
            None => format!("{host} did not resolve"),
        });
    }
    Ok((ok, rule))
}

async fn handle<S>(cfg: ProxyConfig, mut client: S) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Protocol sniff: SOCKS5 starts with its version byte, HTTP with a method name.
    let mut first = [0u8; 1];
    match tokio::time::timeout(Duration::from_secs(15), client.read(&mut first)).await {
        Ok(Ok(1)) => {}
        _ => return Ok(()),
    }
    if first[0] == 0x05 {
        return socks5(cfg, client).await;
    }
    let head = match tokio::time::timeout(
        Duration::from_secs(15),
        read_head_from(&mut client, first.to_vec()),
    )
    .await
    {
        Ok(Ok((buf, end))) => parse_head(&buf, end),
        _ => return Ok(()),
    };
    let head = match head {
        Ok(h) => h,
        Err(e) => {
            respond(&mut client, "400 Bad Request", e).await;
            return Ok(());
        }
    };
    let observe = |e: EgressEvent| {
        if let Some(o) = &cfg.observer {
            o(e);
        }
    };
    // Host/target cross-check for forwarded requests (domain fronting through an allowed name).
    if cfg.sni_check
        && head.forward.is_some()
        && let Some(hh) = &head.host_header
        && !same_host(hh, &head.host)
    {
        let reason = format!("Host header {hh} does not match the request target");
        observe(EgressEvent::Denied {
            host: head.host.clone(),
            port: head.port,
            reason: reason.clone(),
        });
        respond(
            &mut client,
            "403 Forbidden",
            &format!("vibeke egress: {reason}\n"),
        )
        .await;
        return Ok(());
    }
    let (addrs, rule) = match decide(&cfg, &head.host, head.port).await {
        Ok(x) => x,
        Err(reason) => {
            observe(EgressEvent::Denied {
                host: head.host.clone(),
                port: head.port,
                reason: reason.clone(),
            });
            respond(
                &mut client,
                "403 Forbidden",
                &format!(
                    "vibeke egress: {}:{} denied ({reason})\n",
                    head.host, head.port
                ),
            )
            .await;
            return Ok(());
        }
    };
    let Some(mut up) = connect_any(&cfg, &addrs).await else {
        respond(
            &mut client,
            "502 Bad Gateway",
            "vibeke egress: upstream unreachable\n",
        )
        .await;
        return Ok(());
    };
    observe(EgressEvent::Allowed {
        host: head.host.clone(),
        port: head.port,
        rule,
    });
    let _ = head.method;
    let Some(fwd) = &head.forward else {
        // CONNECT: an opaque tunnel to the one checked endpoint.
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        return tunnel(&cfg, client, up, head.rest, &head.host, head.port).await;
    };
    // Absolute-form request: forward this head and exactly `body_len` body bytes, then only
    // relay the response. Pipelined or keep-alive follow-up requests are never forwarded
    // (they would bypass the policy check made for this request's host).
    up.write_all(fwd).await?;
    let first = (head.rest.len() as u64).min(head.body_len) as usize;
    up.write_all(&head.rest[..first]).await?;
    let remaining = head.body_len - first as u64;
    let (mut ur, mut uw) = up.into_split();
    let (mut cr, mut cw) = tokio::io::split(client);
    {
        let body = async {
            let mut limited = (&mut cr).take(remaining);
            let _ = tokio::io::copy(&mut limited, &mut uw).await;
            // Keep the upstream write half open (half-close aborts some origins); nothing more
            // from the client goes upstream.
            std::future::pending::<()>().await;
        };
        let response = async {
            let _ = tokio::io::copy(&mut ur, &mut cw).await;
            let _ = cw.shutdown().await;
        };
        tokio::select! {
            _ = body => {}
            _ = response => {}
        }
    }
    drop(uw);
    // Lingering close: discard whatever the client still sends (a follow-up request) for a
    // moment, so closing with unread input doesn't reset the connection before the client has
    // read the response.
    let mut sink = [0u8; 4096];
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while matches!(cr.read(&mut sink).await, Ok(n) if n > 0) {}
    })
    .await;
    Ok(())
}

fn same_host(a: &str, b: &str) -> bool {
    let n = |s: &str| {
        s.trim_end_matches('.')
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase()
    };
    n(a) == n(b)
}

async fn connect_any(cfg: &ProxyConfig, addrs: &[SocketAddr]) -> Option<TcpStream> {
    for a in addrs {
        if let Ok(Ok(s)) = tokio::time::timeout(cfg.connect_timeout, TcpStream::connect(a)).await {
            return Some(s);
        }
    }
    None
}

/// The server name of a TLS ClientHello, as far as the buffered bytes hold it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sni {
    /// Not a TLS handshake record: relayed without a check.
    NotTls,
    /// A ClientHello, but not all of it yet.
    Incomplete,
    /// A complete ClientHello without a server_name extension.
    Absent,
    Name(String),
}

/// Parse the SNI from the first TLS record(s) of a client stream (13 §7 domain-fronting check).
pub fn parse_sni(buf: &[u8]) -> Sni {
    if buf.is_empty() {
        return Sni::Incomplete;
    }
    if buf[0] != 0x16 {
        return Sni::NotTls;
    }
    if buf.len() < 5 {
        return Sni::Incomplete;
    }
    if buf[1] != 0x03 {
        return Sni::NotTls;
    }
    // Reassemble handshake bytes across records (a large ClientHello may span two).
    let mut hs = Vec::new();
    let mut i = 0;
    while i + 5 <= buf.len() && buf[i] == 0x16 {
        let len = u16::from_be_bytes([buf[i + 3], buf[i + 4]]) as usize;
        let end = (i + 5 + len).min(buf.len());
        hs.extend_from_slice(&buf[i + 5..end]);
        if end < i + 5 + len {
            break;
        }
        i += 5 + len;
        if hs.len() >= 4 {
            let need = 4 + (u32::from_be_bytes([0, hs[1], hs[2], hs[3]]) as usize);
            if hs.len() >= need {
                break;
            }
        }
    }
    if hs.len() < 4 {
        return Sni::Incomplete;
    }
    if hs[0] != 0x01 {
        return Sni::NotTls;
    }
    let body_len = u32::from_be_bytes([0, hs[1], hs[2], hs[3]]) as usize;
    if hs.len() < 4 + body_len {
        // A ClientHello is at most a few KB; give up on absurd lengths instead of buffering.
        return if body_len > 64 * 1024 {
            Sni::NotTls
        } else {
            Sni::Incomplete
        };
    }
    let b = &hs[4..4 + body_len];
    // version(2) random(32) session_id(1+n) cipher_suites(2+n) compression(1+n) extensions(2+n)
    let mut p = 34usize;
    fn take(b: &[u8], p: &mut usize, n: usize) -> Option<usize> {
        let at = *p;
        if at + n > b.len() {
            return None;
        }
        *p += n;
        Some(at)
    }
    let Some(at) = take(b, &mut p, 1) else {
        return Sni::Absent;
    };
    let sid = b[at] as usize;
    if take(b, &mut p, sid).is_none() {
        return Sni::Absent;
    }
    let Some(at) = take(b, &mut p, 2) else {
        return Sni::Absent;
    };
    let cs = u16::from_be_bytes([b[at], b[at + 1]]) as usize;
    if take(b, &mut p, cs).is_none() {
        return Sni::Absent;
    }
    let Some(at) = take(b, &mut p, 1) else {
        return Sni::Absent;
    };
    let cm = b[at] as usize;
    if take(b, &mut p, cm).is_none() {
        return Sni::Absent;
    }
    let Some(at) = take(b, &mut p, 2) else {
        return Sni::Absent;
    };
    let ext_end = (p + u16::from_be_bytes([b[at], b[at + 1]]) as usize).min(b.len());
    while p + 4 <= ext_end {
        let ty = u16::from_be_bytes([b[p], b[p + 1]]);
        let len = u16::from_be_bytes([b[p + 2], b[p + 3]]) as usize;
        let data = &b[(p + 4).min(ext_end)..(p + 4 + len).min(ext_end)];
        p += 4 + len;
        if ty != 0 {
            continue;
        }
        // server_name_list: len(2), then entries of type(1) len(2) name.
        let mut q = 2;
        while q + 3 <= data.len() {
            let nt = data[q];
            let nl = u16::from_be_bytes([data[q + 1], data[q + 2]]) as usize;
            let name = &data[(q + 3).min(data.len())..(q + 3 + nl).min(data.len())];
            q += 3 + nl;
            if nt == 0 {
                return match std::str::from_utf8(name) {
                    Ok(n) => Sni::Name(n.to_ascii_lowercase()),
                    Err(_) => Sni::Absent,
                };
            }
        }
    }
    Sni::Absent
}

/// Is a ClientHello naming `sni` acceptable for a tunnel checked against `host:port`? The same
/// name, or (IP-literal targets, other names) a name the policy allows on its own.
pub fn sni_ok(cfg: &ProxyConfig, sni: &str, host: &str, port: u16) -> Result<(), String> {
    if same_host(sni, host) {
        return Ok(());
    }
    let verdict = cfg
        .policy
        .read()
        .map(|p| p.check_host(sni, port))
        .unwrap_or(HostVerdict::Deny {
            reason: "policy unavailable".into(),
        });
    match verdict {
        HostVerdict::Allow { .. } => Ok(()),
        _ => Err(format!(
            "TLS server name {sni} does not match {host} (domain fronting)"
        )),
    }
}

/// Relay a checked tunnel. The server → client direction starts at once (server-first
/// protocols are never delayed); client → server bytes wait for the SNI check when the client
/// opens with a TLS ClientHello.
async fn tunnel<S>(
    cfg: &ProxyConfig,
    client: S,
    up: TcpStream,
    mut pending: Vec<u8>,
    host: &str,
    port: u16,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut cr, mut cw) = tokio::io::split(client);
    let (mut ur, mut uw) = up.into_split();
    let down = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut ur, &mut cw).await;
        let _ = cw.shutdown().await;
    });
    if cfg.sni_check {
        let mut chunk = [0u8; 4096];
        // One deadline for the whole inspection (a client trickling bytes can't extend it).
        let deadline = tokio::time::Instant::now() + cfg.sni_timeout;
        let verdict = loop {
            match parse_sni(&pending) {
                Sni::Incomplete => {}
                Sni::NotTls | Sni::Absent => break Ok(()),
                Sni::Name(n) => break sni_ok(cfg, &n, host, port),
            }
            match tokio::time::timeout_at(deadline, cr.read(&mut chunk)).await {
                Ok(Ok(n)) if n > 0 => pending.extend_from_slice(&chunk[..n]),
                // Closed, failed or quiet before the first bytes could be classified: the
                // tunnel closes. Uninspected bytes are never forwarded (a delayed ClientHello
                // would otherwise name any host).
                Ok(_) => break Err("connection closed before a complete TLS ClientHello".into()),
                Err(_) => {
                    break Err(format!(
                        "no complete TLS ClientHello within {} s",
                        cfg.sni_timeout.as_secs()
                    ));
                }
            }
        };
        if let Err(reason) = verdict {
            down.abort();
            if let Some(o) = &cfg.observer {
                o(EgressEvent::Denied {
                    host: host.to_string(),
                    port,
                    reason,
                });
            }
            return Ok(());
        }
    }
    if !pending.is_empty() {
        uw.write_all(&pending).await?;
    }
    let _ = tokio::io::copy(&mut cr, &mut uw).await;
    let _ = uw.shutdown().await;
    let _ = down.await;
    Ok(())
}

/// SOCKS5 reply codes (RFC 1928 §6).
const SOCKS_OK: u8 = 0x00;
const SOCKS_NOT_ALLOWED: u8 = 0x02;
const SOCKS_HOST_UNREACHABLE: u8 = 0x04;
const SOCKS_CMD_UNSUPPORTED: u8 = 0x07;
const SOCKS_ATYP_UNSUPPORTED: u8 = 0x08;

async fn socks_reply<S: AsyncWrite + Unpin>(s: &mut S, code: u8) {
    let _ = s
        .write_all(&[0x05, code, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await;
}

/// SOCKS5 `CONNECT` without authentication (the version byte was already read). A domain-type
/// destination is resolved by the proxy, like an HTTP CONNECT target.
async fn socks5<S>(cfg: ProxyConfig, mut client: S) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let t = Duration::from_secs(15);
    let methods = match tokio::time::timeout(t, async {
        let n = client.read_u8().await? as usize;
        let mut methods = vec![0u8; n];
        client.read_exact(&mut methods).await?;
        Ok::<_, std::io::Error>(methods)
    })
    .await
    {
        Ok(Ok(m)) => m,
        _ => return Ok(()),
    };
    if !methods.contains(&0x00) {
        let _ = client.write_all(&[0x05, 0xff]).await;
        return Ok(());
    }
    client.write_all(&[0x05, 0x00]).await?;
    let (h, host, port) = match tokio::time::timeout(t, async {
        let mut h = [0u8; 4];
        client.read_exact(&mut h).await?;
        let host = match h[3] {
            0x01 => {
                let mut a = [0u8; 4];
                client.read_exact(&mut a).await?;
                Some(std::net::Ipv4Addr::from(a).to_string())
            }
            0x03 => {
                let n = client.read_u8().await? as usize;
                let mut d = vec![0u8; n];
                client.read_exact(&mut d).await?;
                String::from_utf8(d).ok()
            }
            0x04 => {
                let mut a = [0u8; 16];
                client.read_exact(&mut a).await?;
                Some(std::net::Ipv6Addr::from(a).to_string())
            }
            _ => None,
        };
        let port = client.read_u16().await?;
        Ok::<_, std::io::Error>((h, host, port))
    })
    .await
    {
        Ok(Ok(x)) => x,
        _ => return Ok(()),
    };
    if h[0] != 0x05 || h[1] != 0x01 {
        socks_reply(&mut client, SOCKS_CMD_UNSUPPORTED).await;
        return Ok(());
    }
    let Some(host) = host.filter(|x| !x.is_empty()) else {
        socks_reply(&mut client, SOCKS_ATYP_UNSUPPORTED).await;
        return Ok(());
    };
    let observe = |e: EgressEvent| {
        if let Some(o) = &cfg.observer {
            o(e);
        }
    };
    let (addrs, rule) = match decide(&cfg, &host, port).await {
        Ok(x) => x,
        Err(reason) => {
            observe(EgressEvent::Denied {
                host: host.clone(),
                port,
                reason,
            });
            socks_reply(&mut client, SOCKS_NOT_ALLOWED).await;
            return Ok(());
        }
    };
    let Some(up) = connect_any(&cfg, &addrs).await else {
        socks_reply(&mut client, SOCKS_HOST_UNREACHABLE).await;
        return Ok(());
    };
    observe(EgressEvent::Allowed {
        host: host.clone(),
        port,
        rule,
    });
    socks_reply(&mut client, SOCKS_OK).await;
    tunnel(&cfg, client, up, Vec::new(), &host, port).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::NetworkProfile;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FixedResolver(HashMap<String, IpAddr>);
    impl Resolver for FixedResolver {
        fn resolve(&self, host: String, port: u16) -> BoxFut<std::io::Result<Vec<SocketAddr>>> {
            let r = self
                .0
                .get(&host)
                .map(|ip| vec![SocketAddr::new(*ip, port)])
                .ok_or_else(|| std::io::Error::other("nxdomain"));
            Box::pin(async move { r })
        }
    }

    struct CountingAsker {
        answer: Option<AskDecision>,
        calls: AtomicUsize,
    }
    impl Asker for CountingAsker {
        fn ask(&self, _host: String, _port: u16) -> BoxFut<AskDecision> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let a = self.answer;
            Box::pin(async move {
                match a {
                    Some(a) => a,
                    None => std::future::pending().await,
                }
            })
        }
    }

    /// Tiny HTTP origin on 127.0.0.1 that answers every request with `ok`.
    async fn origin() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let _ = read_head(&mut s).await;
                    let _ = s
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .await;
                    let _ = s.shutdown().await;
                });
            }
        });
        port
    }

    fn authority(url: &str) -> &str {
        let rest = url.strip_prefix("http://").unwrap_or(url);
        rest.split('/').next().unwrap_or(rest)
    }

    async fn get_via(proxy: u16, url: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", proxy)).await.unwrap();
        s.write_all(
            format!(
                "GET {url} HTTP/1.1\r\nHost: {}\r\nProxy-Connection: keep-alive\r\n\r\n",
                authority(url)
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out).await;
        out
    }

    fn cfg(policy: EgressPolicy) -> ProxyConfig {
        let mut c = ProxyConfig::new(policy);
        let mut m = HashMap::new();
        m.insert("asked.test".to_string(), "127.0.0.1".parse().unwrap());
        m.insert(
            "metadata.test".to_string(),
            "169.254.169.254".parse().unwrap(),
        );
        m.insert("private.test".to_string(), "10.1.2.3".parse().unwrap());
        m.insert("rebind.test".to_string(), "127.0.0.1".parse().unwrap());
        c.resolver = Arc::new(FixedResolver(m));
        c.ask_timeout = Duration::from_millis(300);
        c
    }

    #[tokio::test]
    async fn forward_allowed_and_denied_by_loopback_port() {
        let o = origin().await;
        let mut pol = EgressPolicy::new(NetworkProfile::Dev);
        pol.local_ports.insert(o);
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ev = events.clone();
        let mut c = cfg(pol);
        c.observer = Some(Arc::new(move |e| ev.lock().unwrap().push(e)));
        let p = EgressProxy::start(c, 0, None).await.unwrap();
        let ok = get_via(p.port, &format!("http://127.0.0.1:{o}/hello")).await;
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        assert!(ok.ends_with("ok"));
        // A loopback port that was not declared is refused before connecting.
        let other = origin().await;
        let denied = get_via(p.port, &format!("http://127.0.0.1:{other}/")).await;
        assert!(denied.starts_with("HTTP/1.1 403"), "{denied}");
        assert!(denied.contains("X-Vibeke-Egress: denied"));
        let ev = events.lock().unwrap().clone();
        assert!(matches!(&ev[0], EgressEvent::Allowed { port, .. } if *port == o));
        assert!(matches!(&ev[1], EgressEvent::Denied { port, .. } if *port == other));
    }

    #[tokio::test]
    async fn connect_tunnel() {
        let o = origin().await;
        let mut pol = EgressPolicy::new(NetworkProfile::HarnessApis);
        pol.local_ports.insert(o);
        let p = EgressProxy::start(cfg(pol), 0, None).await.unwrap();
        let mut s = TcpStream::connect(("127.0.0.1", p.port)).await.unwrap();
        s.write_all(
            format!("CONNECT 127.0.0.1:{o} HTTP/1.1\r\nHost: 127.0.0.1:{o}\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
        let mut buf = [0u8; 39];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"HTTP/1.1 200 Connection Established\r\n\r\n");
        s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        assert!(out.ends_with("ok"));
    }

    #[tokio::test]
    async fn resolved_ip_checks_block_metadata_and_private() {
        let mut pol = EgressPolicy::new(NetworkProfile::Open);
        pol.extra_allow.insert("metadata.test".into());
        let c = cfg(pol);
        let e = decide(&c, "metadata.test", 80).await.unwrap_err();
        assert!(e.contains("metadata"), "{e}");
        let e = decide(&c, "private.test", 80).await.unwrap_err();
        assert!(e.contains("private"), "{e}");
        // A public-looking name that resolves to loopback (rebinding) is refused.
        let e = decide(&c, "rebind.test", 80).await.unwrap_err();
        assert!(e.contains("loopback"), "{e}");
        let e = decide(&c, "169.254.169.254", 80).await.unwrap_err();
        assert!(e.contains("metadata"), "{e}");
    }

    #[tokio::test]
    async fn ask_allow_once_task_and_fail_closed() {
        let o = origin().await;
        let mut pol = EgressPolicy::new(NetworkProfile::HarnessApis);
        pol.local_ports.insert(o); // lets asked.test's loopback answer pass the IP check
        // No asker: unknown hosts are denied.
        let c = cfg(pol.clone());
        assert!(decide(&c, "asked.test", o).await.is_err());
        // Allow once: asked every time.
        let asker = Arc::new(CountingAsker {
            answer: Some(AskDecision::AllowOnce),
            calls: AtomicUsize::new(0),
        });
        let mut c = cfg(pol.clone());
        c.asker = Some(asker.clone());
        let p = EgressProxy::start(c, 0, None).await.unwrap();
        for _ in 0..2 {
            let r = get_via(p.port, &format!("http://asked.test:{o}/")).await;
            assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        }
        assert_eq!(asker.calls.load(Ordering::SeqCst), 2);
        // Allow for task: asked once, then on the task allowlist.
        let asker = Arc::new(CountingAsker {
            answer: Some(AskDecision::AllowTask),
            calls: AtomicUsize::new(0),
        });
        let mut c = cfg(pol.clone());
        c.asker = Some(asker.clone());
        let p = EgressProxy::start(c, 0, None).await.unwrap();
        for _ in 0..2 {
            let r = get_via(p.port, &format!("http://asked.test:{o}/")).await;
            assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        }
        assert_eq!(asker.calls.load(Ordering::SeqCst), 1);
        assert!(
            p.policy
                .read()
                .unwrap()
                .task_allow
                .contains(&format!("asked.test:{o}"))
        );
        // The approval covered that endpoint only: another port on the same host asks again.
        let other = origin().await;
        p.policy.write().unwrap().local_ports.insert(other);
        let r = get_via(p.port, &format!("http://asked.test:{other}/")).await;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert_eq!(asker.calls.load(Ordering::SeqCst), 2);
        // Nobody answers: denied after the hold timeout.
        let mut c = cfg(pol);
        c.asker = Some(Arc::new(CountingAsker {
            answer: None,
            calls: AtomicUsize::new(0),
        }));
        let e = decide(&c, "asked.test", o).await.unwrap_err();
        assert!(e.contains("fail closed"), "{e}");
    }

    /// Origin that answers the first request and records every byte it receives afterwards.
    async fn recording_origin() -> (u16, Arc<std::sync::Mutex<Vec<u8>>>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let seen: Arc<std::sync::Mutex<Vec<u8>>> = Arc::default();
        let s2 = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else {
                    return;
                };
                let seen = s2.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let mut got = Vec::new();
                    // Read for a while: everything the proxy forwards ends up in `seen`.
                    let deadline = tokio::time::Instant::now() + Duration::from_millis(400);
                    let mut answered = false;
                    while let Ok(Ok(n)) = tokio::time::timeout_at(deadline, s.read(&mut buf)).await
                    {
                        if n == 0 {
                            break;
                        }
                        got.extend_from_slice(&buf[..n]);
                        if !answered && got.windows(4).any(|w| w == b"\r\n\r\n") {
                            answered = true;
                            let _ = s
                                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                                .await;
                        }
                    }
                    seen.lock().unwrap().extend_from_slice(&got);
                    let _ = s.shutdown().await;
                });
            }
        });
        (port, seen)
    }

    #[tokio::test]
    async fn exactly_one_request_is_forwarded_per_connection() {
        let (o, seen) = recording_origin().await;
        let mut pol = EgressPolicy::new(NetworkProfile::Dev);
        pol.local_ports.insert(o);
        let p = EgressProxy::start(cfg(pol), 0, None).await.unwrap();
        let mut s = TcpStream::connect(("127.0.0.1", p.port)).await.unwrap();
        // A POST with a body, then a pipelined request meant for another host on the same
        // (allowed) connection.
        s.write_all(
            format!(
                "POST http://127.0.0.1:{o}/a HTTP/1.1\r\nHost: 127.0.0.1:{o}\r\nContent-Length: 4\r\n\r\nBODYGET http://evil.test/steal HTTP/1.1\r\nHost: evil.test\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        // More bytes later on the same connection.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = s
            .write_all(b"GET /second HTTP/1.1\r\nHost: evil.test\r\n\r\n")
            .await;
        let mut out = String::new();
        let r = tokio::time::timeout(Duration::from_secs(5), s.read_to_string(&mut out)).await;
        assert!(out.starts_with("HTTP/1.1 200"), "{r:?} {out}");
        tokio::time::sleep(Duration::from_millis(500)).await;
        let got = String::from_utf8_lossy(&seen.lock().unwrap()).into_owned();
        assert!(got.starts_with("POST /a HTTP/1.1\r\n"), "{got}");
        assert!(got.ends_with("\r\n\r\nBODY"), "{got}");
        assert!(
            !got.contains("evil"),
            "a second request reached the origin: {got}"
        );
        assert!(!got.contains("/second"), "{got}");
        // Chunked bodies (unknown length) are refused rather than relayed blindly.
        let mut s = TcpStream::connect(("127.0.0.1", p.port)).await.unwrap();
        s.write_all(
            format!("POST http://127.0.0.1:{o}/ HTTP/1.1\r\nHost: 127.0.0.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out).await;
        assert!(out.starts_with("HTTP/1.1 400"), "{out}");
    }

    #[tokio::test]
    async fn connect_to_a_listed_host_on_another_port_is_asked() {
        let asker = Arc::new(CountingAsker {
            answer: Some(AskDecision::Deny),
            calls: AtomicUsize::new(0),
        });
        let mut c = cfg(EgressPolicy::new(NetworkProfile::Dev));
        c.asker = Some(asker.clone());
        // github.com is listed for 80/443 only: port 22 (ssh through CONNECT) needs a decision.
        let e = decide(&c, "github.com", 22).await.unwrap_err();
        assert!(e.contains("denied by user"), "{e}");
        assert_eq!(asker.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unix_socket_listener() {
        let o = origin().await;
        let t = tempfile::tempdir().unwrap();
        let sock = t.path().join("egress.sock");
        let mut pol = EgressPolicy::new(NetworkProfile::Dev);
        pol.local_ports.insert(o);
        let _p = EgressProxy::start(cfg(pol), 0, Some(sock.clone()))
            .await
            .unwrap();
        let mut s = tokio::net::UnixStream::connect(&sock).await.unwrap();
        s.write_all(
            format!("GET http://127.0.0.1:{o}/ HTTP/1.1\r\nHost: 127.0.0.1:{o}\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        assert!(out.ends_with("ok"));
    }

    #[test]
    fn head_parsing() {
        let req = b"GET http://user@example.com:8080/a?b HTTP/1.1\r\nHost: example.com\r\nProxy-Authorization: x\r\nConnection: keep-alive\r\n\r\nBODY";
        let end = req.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let h = parse_head(req, end).unwrap();
        assert_eq!((h.host.as_str(), h.port), ("example.com", 8080));
        let fwd = String::from_utf8(h.forward.unwrap()).unwrap();
        assert!(fwd.starts_with("GET /a?b HTTP/1.1\r\n"));
        assert!(!fwd.to_ascii_lowercase().contains("proxy-authorization"));
        assert!(fwd.ends_with("Connection: close\r\n\r\n"));
        assert_eq!(h.rest, b"BODY");
        assert_eq!(split_host_port("[::1]:443", 1), Some(("::1".into(), 443)));
        assert_eq!(
            split_host_port("example.com", 443),
            Some(("example.com".into(), 443))
        );
        assert_eq!(split_host_port("::1", 443), None);
        let bad = b"GET https://example.com/ HTTP/1.1\r\n\r\n";
        assert!(parse_head(bad, bad.len() - 4).is_err());
        let h = b"GET http://a.test/ HTTP/1.1\r\nHost: B.test:80\r\n\r\n";
        let ph = parse_head(h, h.len() - 4).unwrap();
        assert_eq!(ph.host_header.as_deref(), Some("b.test"));
    }

    /// A minimal TLS 1.2-style ClientHello record carrying `sni` (or no SNI extension).
    fn client_hello(sni: Option<&str>) -> Vec<u8> {
        let mut ext = Vec::new();
        if let Some(n) = sni {
            let n = n.as_bytes();
            let mut list = vec![0u8];
            list.extend_from_slice(&(n.len() as u16).to_be_bytes());
            list.extend_from_slice(n);
            let mut data = (list.len() as u16).to_be_bytes().to_vec();
            data.extend_from_slice(&list);
            ext.extend_from_slice(&0u16.to_be_bytes());
            ext.extend_from_slice(&(data.len() as u16).to_be_bytes());
            ext.extend_from_slice(&data);
        }
        // An unrelated extension first (supported_groups), as real clients send many.
        let mut exts = vec![0x00, 0x0a, 0x00, 0x04, 0x00, 0x02, 0x00, 0x1d];
        exts.extend_from_slice(&ext);
        let mut body = vec![0x03, 0x03];
        body.extend_from_slice(&[7u8; 32]);
        body.push(0); // session id
        body.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]); // one cipher suite
        body.extend_from_slice(&[0x01, 0x00]); // null compression
        body.extend_from_slice(&(exts.len() as u16).to_be_bytes());
        body.extend_from_slice(&exts);
        let mut hs = vec![0x01];
        hs.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        hs.extend_from_slice(&body);
        let mut rec = vec![0x16, 0x03, 0x01];
        rec.extend_from_slice(&(hs.len() as u16).to_be_bytes());
        rec.extend_from_slice(&hs);
        rec
    }

    #[test]
    fn sni_parsing() {
        let h = client_hello(Some("Example.COM"));
        assert_eq!(parse_sni(&h), Sni::Name("example.com".into()));
        assert_eq!(parse_sni(&h[..h.len() - 3]), Sni::Incomplete);
        assert_eq!(parse_sni(&client_hello(None)), Sni::Absent);
        assert_eq!(parse_sni(b"GET / HTTP/1.1\r\n"), Sni::NotTls);
        assert_eq!(parse_sni(b"SSH-2.0-x"), Sni::NotTls);
        assert_eq!(parse_sni(&[]), Sni::Incomplete);
        // Split across two TLS records.
        let h = client_hello(Some("split.test"));
        let hs = &h[5..];
        let (a, b) = hs.split_at(20);
        let mut two = vec![0x16, 0x03, 0x01];
        two.extend_from_slice(&(a.len() as u16).to_be_bytes());
        two.extend_from_slice(a);
        two.extend_from_slice(&[0x16, 0x03, 0x01]);
        two.extend_from_slice(&(b.len() as u16).to_be_bytes());
        two.extend_from_slice(b);
        assert_eq!(parse_sni(&two), Sni::Name("split.test".into()));
    }

    async fn connect_tunnel_to(proxy: u16, target: &str) -> TcpStream {
        let mut s = TcpStream::connect(("127.0.0.1", proxy)).await.unwrap();
        s.write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut buf = [0u8; 39];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"HTTP/1.1 200 Connection Established\r\n\r\n");
        s
    }

    #[tokio::test]
    async fn sni_mismatch_in_a_tunnel_is_refused() {
        let (o, seen) = recording_origin().await;
        let mut pol = EgressPolicy::new(NetworkProfile::HarnessApis);
        pol.local_ports.insert(o);
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ev = events.clone();
        let mut c = cfg(pol);
        c.observer = Some(Arc::new(move |e| ev.lock().unwrap().push(e)));
        let p = EgressProxy::start(c, 0, None).await.unwrap();
        // A ClientHello for a host on no list: nothing reaches the origin.
        let mut s = connect_tunnel_to(p.port, &format!("127.0.0.1:{o}")).await;
        s.write_all(&client_hello(Some("evil.test"))).await.unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            seen.lock().unwrap().is_empty(),
            "fronted bytes were relayed"
        );
        assert!(events.lock().unwrap().iter().any(|e| matches!(
            e,
            EgressEvent::Denied { reason, .. } if reason.contains("domain fronting")
        )));
        // A name the policy allows for that port (loopback name, declared port) goes through.
        let mut s = connect_tunnel_to(p.port, &format!("127.0.0.1:{o}")).await;
        let hello = client_hello(Some("localhost"));
        s.write_all(&hello).await.unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(seen.lock().unwrap().starts_with(&hello));
    }

    /// Final review P1 4: an incomplete ClientHello held past the inspection deadline, then
    /// completed with a forbidden name, never reaches the origin: the tunnel closes at the
    /// deadline. A client that half-closes mid-ClientHello gets nothing forwarded either.
    #[tokio::test]
    async fn delayed_or_truncated_client_hello_closes_the_tunnel() {
        // An origin that records everything it ever receives.
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let o = l.local_addr().unwrap().port();
        let seen: Arc<std::sync::Mutex<Vec<u8>>> = Arc::default();
        let s2 = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let seen = s2.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = s.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        seen.lock().unwrap().extend_from_slice(&buf[..n]);
                    }
                });
            }
        });
        let mut pol = EgressPolicy::new(NetworkProfile::HarnessApis);
        pol.local_ports.insert(o);
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ev = events.clone();
        let mut c = cfg(pol);
        c.sni_timeout = Duration::from_millis(300);
        c.observer = Some(Arc::new(move |e| ev.lock().unwrap().push(e)));
        let p = EgressProxy::start(c, 0, None).await.unwrap();
        let hello = client_hello(Some("evil.test"));
        // Delayed: part now, the rest (naming a forbidden host) after the deadline.
        let mut s = connect_tunnel_to(p.port, &format!("127.0.0.1:{o}")).await;
        s.write_all(&hello[..10]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(700)).await;
        let _ = s.write_all(&hello[10..]).await;
        let mut out = Vec::new();
        let r = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
        assert!(r.is_ok(), "the tunnel was closed");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            seen.lock().unwrap().is_empty(),
            "uninspected bytes were forwarded"
        );
        assert!(events.lock().unwrap().iter().any(|e| matches!(
            e,
            EgressEvent::Denied { reason, .. } if reason.contains("ClientHello")
        )));
        // Truncated: half a ClientHello, then the client closes its side.
        let mut s = connect_tunnel_to(p.port, &format!("127.0.0.1:{o}")).await;
        s.write_all(&hello[..hello.len() / 2]).await.unwrap();
        s.shutdown().await.unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            seen.lock().unwrap().is_empty(),
            "a truncated ClientHello was forwarded"
        );
        // A complete, allowed ClientHello still goes through.
        let mut s = connect_tunnel_to(p.port, &format!("127.0.0.1:{o}")).await;
        let ok = client_hello(Some("localhost"));
        s.write_all(&ok).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(seen.lock().unwrap().starts_with(&ok));
    }

    #[tokio::test]
    async fn sni_check_off_relays_and_non_tls_is_not_delayed() {
        // A server-first protocol: the origin greets before the client says anything.
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let o = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                tokio::spawn(async move {
                    let _ = s.write_all(b"220 hello\r\n").await;
                    let mut b = [0u8; 4];
                    let _ = s.read_exact(&mut b).await;
                    let _ = s.write_all(&b).await;
                });
            }
        });
        let mut pol = EgressPolicy::new(NetworkProfile::HarnessApis);
        pol.local_ports.insert(o);
        let p = EgressProxy::start(cfg(pol), 0, None).await.unwrap();
        let mut s = connect_tunnel_to(p.port, &format!("127.0.0.1:{o}")).await;
        let mut greet = [0u8; 11];
        tokio::time::timeout(Duration::from_secs(2), s.read_exact(&mut greet))
            .await
            .expect("the server greeting arrives before the client speaks")
            .unwrap();
        assert_eq!(&greet, b"220 hello\r\n");
        s.write_all(b"EHLO").await.unwrap();
        let mut echo = [0u8; 4];
        s.read_exact(&mut echo).await.unwrap();
        assert_eq!(&echo, b"EHLO");
        // With the check off, a mismatching ClientHello is relayed.
        let (o2, seen) = recording_origin().await;
        let mut pol = EgressPolicy::new(NetworkProfile::HarnessApis);
        pol.local_ports.insert(o2);
        let mut c = cfg(pol);
        c.sni_check = false;
        let p = EgressProxy::start(c, 0, None).await.unwrap();
        let mut s = connect_tunnel_to(p.port, &format!("127.0.0.1:{o2}")).await;
        s.write_all(&client_hello(Some("evil.test"))).await.unwrap();
        drop(s);
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(!seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn host_header_must_match_the_target() {
        let o = origin().await;
        let mut pol = EgressPolicy::new(NetworkProfile::Dev);
        pol.local_ports.insert(o);
        let p = EgressProxy::start(cfg(pol), 0, None).await.unwrap();
        let mut s = TcpStream::connect(("127.0.0.1", p.port)).await.unwrap();
        s.write_all(
            format!("GET http://127.0.0.1:{o}/ HTTP/1.1\r\nHost: evil.test\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out).await;
        assert!(out.starts_with("HTTP/1.1 403"), "{out}");
        assert!(out.contains("does not match"), "{out}");
    }

    async fn socks_connect(proxy: u16, atyp: u8, addr: &[u8], port: u16) -> (TcpStream, u8) {
        let mut s = TcpStream::connect(("127.0.0.1", proxy)).await.unwrap();
        s.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut m = [0u8; 2];
        s.read_exact(&mut m).await.unwrap();
        assert_eq!(m, [0x05, 0x00]);
        let mut req = vec![0x05, 0x01, 0x00, atyp];
        req.extend_from_slice(addr);
        req.extend_from_slice(&port.to_be_bytes());
        s.write_all(&req).await.unwrap();
        let mut rep = [0u8; 10];
        s.read_exact(&mut rep).await.unwrap();
        (s, rep[1])
    }

    #[tokio::test]
    async fn socks5_connect_follows_the_same_policy() {
        let o = origin().await;
        let mut pol = EgressPolicy::new(NetworkProfile::HarnessApis);
        pol.local_ports.insert(o);
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ev = events.clone();
        let mut c = cfg(pol);
        c.observer = Some(Arc::new(move |e| ev.lock().unwrap().push(e)));
        let p = EgressProxy::start(c, 0, None).await.unwrap();
        // IPv4 to a declared loopback port: allowed, then plain bytes through the tunnel.
        let (mut s, code) = socks_connect(p.port, 0x01, &[127, 0, 0, 1], o).await;
        assert_eq!(code, SOCKS_OK);
        s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        assert!(out.ends_with("ok"), "{out}");
        // A domain the proxy resolves to metadata: refused with "not allowed".
        let name = b"metadata.test";
        let mut addr = vec![name.len() as u8];
        addr.extend_from_slice(name);
        let (_s, code) = socks_connect(p.port, 0x03, &addr, 80).await;
        assert_eq!(code, SOCKS_NOT_ALLOWED);
        // An undeclared loopback port: refused before connecting.
        let (_s, code) = socks_connect(p.port, 0x01, &[127, 0, 0, 1], 1).await;
        assert_eq!(code, SOCKS_NOT_ALLOWED);
        // Only CONNECT: BIND is refused.
        let mut s = TcpStream::connect(("127.0.0.1", p.port)).await.unwrap();
        s.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut m = [0u8; 2];
        s.read_exact(&mut m).await.unwrap();
        s.write_all(&[0x05, 0x02, 0x00, 0x01, 127, 0, 0, 1, 0, 80])
            .await
            .unwrap();
        let mut rep = [0u8; 10];
        s.read_exact(&mut rep).await.unwrap();
        assert_eq!(rep[1], SOCKS_CMD_UNSUPPORTED);
        // No acceptable auth method.
        let mut s = TcpStream::connect(("127.0.0.1", p.port)).await.unwrap();
        s.write_all(&[0x05, 0x01, 0x02]).await.unwrap();
        let mut m = [0u8; 2];
        s.read_exact(&mut m).await.unwrap();
        assert_eq!(m, [0x05, 0xff]);
        let ev = events.lock().unwrap().clone();
        assert!(
            ev.iter()
                .any(|e| matches!(e, EgressEvent::Allowed { port, .. } if *port == o))
        );
        assert!(
            ev.iter()
                .any(|e| matches!(e, EgressEvent::Denied { host, .. } if host == "metadata.test"))
        );
    }

    #[tokio::test]
    async fn allow_always_goes_to_the_global_list() {
        let o = origin().await;
        let mut pol = EgressPolicy::new(NetworkProfile::HarnessApis);
        pol.local_ports.insert(o);
        let asker = Arc::new(CountingAsker {
            answer: Some(AskDecision::AllowAlways),
            calls: AtomicUsize::new(0),
        });
        let mut c = cfg(pol);
        c.asker = Some(asker.clone());
        let (_, rule) = decide(&c, "asked.test", o).await.unwrap();
        assert_eq!(rule, format!("global:asked.test:{o}"));
        assert!(
            c.policy
                .read()
                .unwrap()
                .global_allow
                .contains(&format!("asked.test:{o}"))
        );
        // Asked once; afterwards the global entry answers.
        decide(&c, "asked.test", o).await.unwrap();
        assert_eq!(asker.calls.load(Ordering::SeqCst), 1);
    }
}
