//! B4 reverse proxy: capability exchange, per-host sessions, cross-origin refusals, credential
//! stripping, cookie/Location/ACAO rewriting, WebSocket tunnelling, unbuffered streaming and
//! HTTPS upstreams — against loopback upstreams in this process.

use super::*;
use std::sync::Mutex as StdMutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Routes' `port` is the *remote* port; the fake maps it to where the upstream really listens
/// (like a bridge `tcp:` channel to another machine's loopback).
struct FakeUpstream {
    map: StdMutex<HashMap<u16, u16>>,
    connects: AtomicU64,
    /// Preview ids whose route check answers `Gone` (forgotten/retired on their machine).
    gone: StdMutex<Vec<String>>,
    checks: AtomicU64,
}

impl Upstream for FakeUpstream {
    fn connect(&self, route: &Route) -> BoxFuture<std::io::Result<Box<dyn Stream>>> {
        self.connects.fetch_add(1, Ordering::Relaxed);
        let real = self.map.lock().unwrap().get(&route.port).copied();
        Box::pin(async move {
            let p = real.ok_or_else(|| std::io::Error::other("no such upstream"))?;
            let s = TcpStream::connect(("127.0.0.1", p)).await?;
            Ok(Box::new(s) as Box<dyn Stream>)
        })
    }

    fn check(&self, route: &Route) -> BoxFuture<RouteCheck> {
        self.checks.fetch_add(1, Ordering::Relaxed);
        let gone = self.gone.lock().unwrap().contains(&route.preview);
        Box::pin(async move {
            if gone {
                RouteCheck::Gone
            } else {
                RouteCheck::Live
            }
        })
    }
}

#[derive(Clone, Default)]
struct Seen(Arc<StdMutex<Vec<String>>>);

impl Seen {
    fn all(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
    fn last(&self) -> String {
        self.all().last().cloned().unwrap_or_default()
    }
}

async fn read_head(s: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut b = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        match s.read(&mut b).await {
            Ok(1) => buf.push(b[0]),
            _ => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// A dev server: records request heads; `/ws` upgrades and echoes; `/sse` streams two events,
/// the second only after `go` fires; `/redirect` redirects to an absolute upstream URL.
async fn upstream(remote_port: u16) -> (u16, Seen, Arc<tokio::sync::Notify>) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let seen = Seen::default();
    let go = Arc::new(tokio::sync::Notify::new());
    let (seen2, go2) = (seen.clone(), go.clone());
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            let seen = seen2.clone();
            let go = go2.clone();
            tokio::spawn(async move {
                let head = read_head(&mut s).await;
                if head.is_empty() {
                    return;
                }
                seen.0.lock().unwrap().push(head.clone());
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                let o = format!("http://localhost:{remote_port}");
                if path.starts_with("/ws") {
                    let _ = s
                        .write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: x\r\n\r\n")
                        .await;
                    let (mut r, mut w) = s.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                    return;
                }
                if path.starts_with("/sse") {
                    let _ = s
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\ndata: one\n\n")
                        .await;
                    let _ = s.flush().await;
                    go.notified().await;
                    let _ = s.write_all(b"data: two\n\n").await;
                    return;
                }
                if path.starts_with("/post") {
                    // Drain the small body the tests send.
                    let mut b = [0u8; 64];
                    let _ = tokio::time::timeout(Duration::from_millis(200), s.read(&mut b)).await;
                }
                let resp = if path.starts_with("/redirect") {
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: {o}/next?a=1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                } else {
                    let body = format!("<!doctype html><p>hello {path}</p>");
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nSet-Cookie: app=1; Domain=vibeke.localhost; Path=/; HttpOnly\r\nSet-Cookie: vk_token=evil; Path=/\r\nSet-Cookie: __Host-vk_preview=evil; Path=/; Secure\r\nSet-Cookie: VK_preview=evil\r\nAccess-Control-Allow-Origin: {o}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    (port, seen, go)
}

struct Fixture {
    proxy: Arc<Proxy>,
    up: Arc<FakeUpstream>,
    a: Route,
    b: Route,
    seen_a: Seen,
    go_a: Arc<tokio::sync::Notify>,
}

async fn fixture() -> Fixture {
    let up = Arc::new(FakeUpstream {
        map: StdMutex::new(HashMap::new()),
        connects: AtomicU64::new(0),
        gone: StdMutex::new(vec![]),
        checks: AtomicU64::new(0),
    });
    let proxy = Proxy::new(up.clone());
    proxy.serve(Proxy::bind(0).await.unwrap());
    // Two previews of the same app on "devbox", both on remote port 5173 of different tasks.
    let (pa, seen_a, go_a) = upstream(5173).await;
    let (pb, _seen_b, _) = upstream(5174).await;
    up.map.lock().unwrap().insert(5173, pa);
    up.map.lock().unwrap().insert(5174, pb);
    let route = |id: &str, handle: &str, port: u16, slug: &str| Route {
        host: hostname_for(handle, Some("devbox"), Some(slug), "s0123456789"),
        machine: "devbox".into(),
        preview: id.into(),
        handle: handle.into(),
        port,
        scheme: "http".into(),
    };
    let a = proxy.register(route("01A", "v4", 5173, "fix-login"));
    let b = proxy.register(route("01B", "v5", 5174, "fix-login"));
    Fixture {
        proxy,
        up,
        a,
        b,
        seen_a,
        go_a,
    }
}

struct Resp {
    status: u16,
    head: String,
    body: String,
}

impl Resp {
    fn header_values(&self, name: &str) -> Vec<String> {
        self.head
            .lines()
            .filter_map(|l| l.split_once(':'))
            .filter(|(k, _)| k.trim().eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_string())
            .collect()
    }
    fn header(&self, name: &str) -> Option<String> {
        self.header_values(name).into_iter().next()
    }
}

async fn send(port: u16, raw: String) -> Resp {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(raw.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut out)).await;
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    Resp {
        status,
        head: head.to_string(),
        body: body.to_string(),
    }
}

fn get(host: &str, port: u16, path: &str, extra: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\n{extra}Connection: close\r\n\r\n")
}

/// Token exchange → the session cookie value.
async fn login(f: &Fixture, r: &Route) -> String {
    let port = f.proxy.port();
    let t = f.proxy.mint_token(&r.host).unwrap();
    let resp = send(
        port,
        get(&r.host, port, &format!("/?{TOKEN_PARAM}={t}"), ""),
    )
    .await;
    assert_eq!(resp.status, 303, "{}", resp.head);
    let c = resp
        .header_values("set-cookie")
        .into_iter()
        .find(|c| c.starts_with(&format!("{COOKIE}=")))
        .unwrap();
    c.split(';')
        .next()
        .unwrap()
        .split_once('=')
        .unwrap()
        .1
        .to_string()
}

#[test]
fn hostnames_and_cookie_rules() {
    assert_eq!(
        hostname_for("v4", None, Some("fix-login"), "sabc"),
        "v4-fix-login-sabc.vibeke.localhost"
    );
    assert_eq!(
        hostname_for("v12", Some("demo@devbox.ts.net"), Some("Fix Login!"), "s1"),
        "v12-demo-devbox-ts-net-fix-login-s1.vibeke.localhost"
    );
    // The session tag survives truncation: two sessions never share a host.
    let tag = session_tag();
    assert_eq!(tag.len(), 10, "{tag}");
    assert!(
        tag.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    );
    assert_ne!(session_tag(), tag);
    let long = hostname_for("v1", Some("m"), Some(&"x".repeat(200)), &tag);
    let label = long.split('.').next().unwrap();
    assert!(label.len() <= 63, "{long}");
    assert!(label.ends_with(&format!("-{tag}")), "{long}");
    assert_ne!(
        hostname_for("v1", None, Some("web"), "saaaaaaaaa"),
        hostname_for("v1", None, Some("web"), "sbbbbbbbbb")
    );
    assert!(reserved_cookie("vk_token") && reserved_cookie("__Host-vk_preview"));
    assert!(reserved_cookie("__Secure-VK_x") && !reserved_cookie("app"));
    assert_eq!(
        rewrite_set_cookie("a=1; Domain=.vibeke.localhost; Path=/; domain = x; Secure").as_deref(),
        Some("a=1; Path=/; Secure")
    );
    assert_eq!(rewrite_set_cookie("vk_preview=1; Path=/"), None);
    assert_eq!(rewrite_set_cookie("__Host-vk_preview=1"), None);
    assert_eq!(
        strip_reserved_cookies("a=1; __Host-vk_preview=s; vk_preview=s; b=2").as_deref(),
        Some("a=1; b=2")
    );
    assert_eq!(strip_reserved_cookies("vk_preview=s"), None);
    assert_eq!(
        take_param("a=1&vk_token=abc&b=2", TOKEN_PARAM),
        (Some("abc".into()), "a=1&b=2".into())
    );
    assert_eq!(take_param("", TOKEN_PARAM), (None, String::new()));
}

#[tokio::test]
async fn capability_is_required_and_bound_to_its_host() {
    let f = fixture().await;
    let port = f.proxy.port();
    // Unknown host, right host with a wrong port, an IP literal: 421, nothing forwarded.
    for (h, p) in [
        ("evil.vibeke.localhost", port),
        (f.a.host.as_str(), port.wrapping_add(1)),
        ("127.0.0.1", port),
    ] {
        let r = send(port, get(h, p, "/", "")).await;
        assert_eq!(r.status, 421, "{h}:{p} {}", r.head);
    }
    // No credential: 401 with how-to, nothing forwarded.
    let r = send(port, get(&f.a.host, port, "/", "")).await;
    assert_eq!(r.status, 401);
    assert!(
        r.body.contains("vibeke preview open devbox/v4 --proxy"),
        "{}",
        r.body
    );
    // A forged token or a forged cookie: 401.
    let r = send(port, get(&f.a.host, port, "/?vk_token=00", "")).await;
    assert_eq!(r.status, 401);
    let r = send(
        port,
        get(
            &f.a.host,
            port,
            "/",
            &format!("Cookie: {COOKIE}=deadbeef\r\n"),
        ),
    )
    .await;
    assert_eq!(r.status, 401);
    assert!(f.seen_a.all().is_empty(), "nothing reached the app");
    assert_eq!(f.up.connects.load(Ordering::Relaxed), 0);

    // The one-time token: 303 to the same URL (absolute, on the preview's own origin) without
    // it; one cookie, Secure + HttpOnly + SameSite=Strict (no non-Secure copy).
    let t = f.proxy.mint_token(&f.a.host).unwrap();
    let r = send(
        port,
        get(
            &f.a.host,
            port,
            &format!("/app?x=1&{TOKEN_PARAM}={t}&y=2"),
            "",
        ),
    )
    .await;
    assert_eq!(r.status, 303, "{}", r.head);
    assert_eq!(
        r.header("location"),
        Some(format!("{}/app?x=1&y=2", f.proxy.origin(&f.a.host)))
    );
    let cookies = r.header_values("set-cookie");
    assert_eq!(cookies.len(), 1, "{cookies:?}");
    for c in &cookies {
        assert!(c.contains("HttpOnly") && c.contains("SameSite=Strict") && c.contains("Path=/"));
        assert!(!c.to_ascii_lowercase().contains("domain"), "host-only: {c}");
    }
    assert!(cookies[0].starts_with(&format!("{COOKIE}=")) && cookies[0].contains("Secure"));
    assert_eq!(r.header("cache-control").as_deref(), Some("no-store"));
    assert_eq!(r.header("x-frame-options").as_deref(), Some("DENY"));
    assert_eq!(
        r.header("content-security-policy").as_deref(),
        Some("frame-ancestors 'none'")
    );
    assert_eq!(r.header("referrer-policy").as_deref(), Some("no-referrer"));
    let sess = cookies[0]
        .split(';')
        .next()
        .unwrap()
        .split_once('=')
        .unwrap()
        .1;
    // Used once: a replay is refused.
    let r = send(
        port,
        get(&f.a.host, port, &format!("/?{TOKEN_PARAM}={t}"), ""),
    )
    .await;
    assert_eq!(r.status, 401);
    // A token for A does not log into B.
    let ta = f.proxy.mint_token(&f.a.host).unwrap();
    let r = send(
        port,
        get(&f.b.host, port, &format!("/?{TOKEN_PARAM}={ta}"), ""),
    )
    .await;
    assert_eq!(r.status, 401);

    // With the session: forwarded.
    let r = send(
        port,
        get(
            &f.a.host,
            port,
            "/app",
            &format!("Cookie: theme=dark; {COOKIE}={sess}\r\n"),
        ),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.head);
    assert!(r.body.contains("hello /app"));
    // A non-Secure `vk_preview` cookie with the session value is not a credential (another
    // local listener could have read or planted it).
    let r = send(
        port,
        get(
            &f.a.host,
            port,
            "/c",
            &format!("Cookie: vk_preview={sess}\r\n"),
        ),
    )
    .await;
    assert_eq!(r.status, 401);
    // Proxy-generated pages can't be framed either.
    assert_eq!(r.header("x-frame-options").as_deref(), Some("DENY"));
    // A's session on B's host: 401 (sessions are per origin).
    let r = send(
        port,
        get(
            &f.b.host,
            port,
            "/",
            &format!("Cookie: {COOKIE}={sess}\r\n"),
        ),
    )
    .await;
    assert_eq!(r.status, 401);
    assert!(f.proxy.stats().denied >= 8);
}

#[tokio::test]
async fn credentials_never_reach_the_app_and_cookies_are_host_only() {
    let f = fixture().await;
    let port = f.proxy.port();
    let sess = login(&f, &f.a).await;
    let own = f.proxy.origin(&f.a.host);
    let r = send(
        port,
        get(
            &f.a.host,
            port,
            "/page?q=1",
            &format!(
                "Cookie: {COOKIE}={sess}; app=1; vk_preview={sess}; vk_other=x\r\nOrigin: {own}\r\nReferer: {own}/prev?z=1\r\n"
            ),
        ),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.head);
    let up = f.seen_a.last();
    let lower = up.to_ascii_lowercase();
    assert!(up.starts_with("GET /page?q=1 HTTP/1.1\r\n"), "{up}");
    assert!(lower.contains("host: localhost:5173\r\n"), "{up}");
    assert!(lower.contains("origin: http://localhost:5173\r\n"), "{up}");
    assert!(
        lower.contains("referer: http://localhost:5173/prev?z=1\r\n"),
        "{up}"
    );
    assert!(lower.contains("cookie: app=1\r\n"), "{up}");
    assert!(
        !lower.contains("vk_"),
        "the app saw a proxy credential: {up}"
    );
    assert!(!lower.contains(&sess), "{up}");
    // Upstream Set-Cookie: reserved names dropped (any case), Domain stripped.
    let sc = r.header_values("set-cookie");
    assert_eq!(sc, vec!["app=1; Path=/; HttpOnly".to_string()], "{sc:?}");
    // ACAO for the upstream origin is mapped to the preview origin.
    assert_eq!(r.header("access-control-allow-origin"), Some(own.clone()));
    // Redirects to the upstream origin come back to the preview origin.
    let r = send(
        port,
        get(
            &f.a.host,
            port,
            "/redirect",
            &format!("Cookie: {COOKIE}={sess}\r\n"),
        ),
    )
    .await;
    assert_eq!(r.status, 302);
    assert_eq!(r.header("location"), Some(format!("{own}/next?a=1")));
    // The token parameter is never forwarded even next to a valid cookie: it is exchanged
    // (or refused) by the proxy itself.
    let n = f.seen_a.all().len();
    let r = send(
        port,
        get(
            &f.a.host,
            port,
            "/x?vk_token=forged",
            &format!("Cookie: {COOKIE}={sess}\r\n"),
        ),
    )
    .await;
    assert_eq!(r.status, 401);
    assert_eq!(f.seen_a.all().len(), n);
}

#[tokio::test]
async fn cross_origin_requests_are_refused() {
    let f = fixture().await;
    let port = f.proxy.port();
    let sess = login(&f, &f.a).await;
    let ck = format!("Cookie: {COOKIE}={sess}\r\n");
    let sibling = f.proxy.origin(&f.b.host);
    let own = f.proxy.origin(&f.a.host);
    // A sibling preview (same site!) posting with A's cookie.
    let post = |origin: &str| {
        format!(
            "POST /post HTTP/1.1\r\nHost: {}:{port}\r\n{ck}Origin: {origin}\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi",
            f.a.host
        )
    };
    assert_eq!(send(port, post(&sibling)).await.status, 403);
    assert_eq!(send(port, post("null")).await.status, 403);
    assert_eq!(send(port, post("https://evil.example")).await.status, 403);
    assert_eq!(send(port, post(&own)).await.status, 200);
    // Fetch/subresource from another (same-site) origin.
    let r = send(
        port,
        get(
            &f.a.host,
            port,
            "/api",
            &format!(
                "{ck}Sec-Fetch-Site: same-site\r\nSec-Fetch-Mode: cors\r\nOrigin: {sibling}\r\n"
            ),
        ),
    )
    .await;
    assert_eq!(r.status, 403);
    let r = send(
        port,
        get(
            &f.a.host,
            port,
            "/img",
            &format!("{ck}Sec-Fetch-Site: cross-site\r\nSec-Fetch-Mode: no-cors\r\n"),
        ),
    )
    .await;
    assert_eq!(r.status, 403);
    // A sibling preview (same site) can't navigate to A at all: not in an iframe, not at top
    // level either.
    for dest in ["iframe", "document", "frame", "embed"] {
        let r = send(
            port,
            get(
                &f.a.host,
                port,
                "/framed",
                &format!(
                    "{ck}Sec-Fetch-Site: same-site\r\nSec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: {dest}\r\n"
                ),
            ),
        )
        .await;
        assert_eq!(r.status, 403, "same-site {dest}: {}", r.head);
        assert_eq!(r.header("x-frame-options").as_deref(), Some("DENY"));
    }
    // A cross-site iframe navigation (or one without a destination) is refused...
    for extra in [
        "Sec-Fetch-Dest: iframe\r\n",
        "Sec-Fetch-Dest: frame\r\n",
        "",
    ] {
        let r = send(
            port,
            get(
                &f.a.host,
                port,
                "/framed",
                &format!("{ck}Sec-Fetch-Site: cross-site\r\nSec-Fetch-Mode: navigate\r\n{extra}"),
            ),
        )
        .await;
        assert_eq!(r.status, 403, "cross-site {extra:?}: {}", r.head);
    }
    // ...while a top-level document navigation (link from elsewhere) is fine, as are
    // same-origin fetches and navigations typed by the user (`none`).
    for site in ["cross-site", "none", "same-origin"] {
        let r = send(
            port,
            get(
                &f.a.host,
                port,
                "/nav",
                &format!(
                    "{ck}Sec-Fetch-Site: {site}\r\nSec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: document\r\n"
                ),
            ),
        )
        .await;
        assert_eq!(r.status, 200, "{site}: {}", r.head);
    }
    let r = send(
        port,
        get(
            &f.a.host,
            port,
            "/f",
            &format!(
                "{ck}Sec-Fetch-Site: same-origin\r\nSec-Fetch-Mode: cors\r\nOrigin: {own}\r\n"
            ),
        ),
    )
    .await;
    assert_eq!(r.status, 200);
    // Cross-origin WebSocket (CSWSH) with a valid cookie: refused.
    let ws = |origin: &str| {
        format!(
            "GET /ws HTTP/1.1\r\nHost: {}:{port}\r\n{ck}Origin: {origin}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
            f.a.host
        )
    };
    assert_eq!(send(port, ws(&sibling)).await.status, 403);
    assert!(!f.seen_a.all().iter().any(|h| h.contains("/ws")
        || h.contains("/api")
        || h.contains("/img")
        || h.contains("/framed")));
}

/// Repo-controlled preview paths can't turn the token exchange into an open redirect: the
/// `Location` is always the authenticated preview's own origin + the request path.
#[tokio::test]
async fn token_exchange_redirect_stays_on_the_preview_origin() {
    let f = fixture().await;
    let port = f.proxy.port();
    let own = f.proxy.origin(&f.a.host);
    for (path, want) in [
        ("//attacker.example/", "//attacker.example/"),
        ("//attacker.example/x?y=1", "//attacker.example/x?y=1"),
        ("/\\attacker.example/", "/%5Cattacker.example/"),
        ("/app", "/app"),
    ] {
        let t = f.proxy.mint_token(&f.a.host).unwrap();
        let sep = if path.contains('?') { '&' } else { '?' };
        let r = send(
            port,
            get(
                &f.a.host,
                port,
                &format!("{path}{sep}{TOKEN_PARAM}={t}"),
                "",
            ),
        )
        .await;
        if r.status == 400 && path.contains('\\') {
            continue; // hyper refused the raw backslash request target: also fine
        }
        assert_eq!(r.status, 303, "{path}: {}", r.head);
        let loc = r.header("location").unwrap();
        assert!(loc.starts_with(&format!("{own}/")), "{path} -> {loc}");
        let rest = &loc[own.len()..];
        assert!(rest == path || rest == want, "{path} -> {loc}");
    }
    // `url()` puts the token before a fragment (the fragment never reaches the server and
    // survives the redirect in the browser).
    let u = f.proxy.url(&f.a.host, "/#/dash", Some("tok"));
    assert_eq!(u, format!("{own}/?{TOKEN_PARAM}=tok#/dash"));
    let u = f.proxy.url(&f.a.host, "/app?x=1#frag", Some("tok"));
    assert_eq!(u, format!("{own}/app?x=1&{TOKEN_PARAM}=tok#frag"));
    assert_eq!(f.proxy.url(&f.a.host, "/a#b", None), format!("{own}/a#b"));
}

/// A route whose preview is gone (forgotten, retired, moved) is revoked on the next request:
/// the old cookie can't reach anything, not even after the preview's port is reused.
#[tokio::test]
async fn a_gone_preview_revokes_its_route_and_sessions() {
    let f = fixture().await;
    let port = f.proxy.port();
    let sess = login(&f, &f.a).await;
    let ck = format!("Cookie: {COOKIE}={sess}\r\n");
    assert_eq!(send(port, get(&f.a.host, port, "/", &ck)).await.status, 200);
    assert!(f.up.checks.load(Ordering::Relaxed) >= 1);
    f.up.gone.lock().unwrap().push(f.a.preview.clone());
    let n = f.seen_a.all().len();
    let connects = f.up.connects.load(Ordering::Relaxed);
    let r = send(port, get(&f.a.host, port, "/", &ck)).await;
    assert_eq!(r.status, 410, "{}", r.head);
    assert_eq!(f.seen_a.all().len(), n, "nothing reached the old upstream");
    assert_eq!(f.up.connects.load(Ordering::Relaxed), connects);
    // The route itself is gone (421), even if the preview "comes back" on the same port.
    f.up.gone.lock().unwrap().clear();
    assert_eq!(send(port, get(&f.a.host, port, "/", &ck)).await.status, 421);
    assert!(f.proxy.routes().iter().all(|r| r.host != f.a.host));
    // Re-registering the preview starts with no sessions: the old cookie is refused.
    let a2 = f.proxy.register(f.a.clone());
    assert_eq!(send(port, get(&a2.host, port, "/", &ck)).await.status, 401);
    // `remove_matching` (a remote forget by handle) drops the route too.
    assert_eq!(f.proxy.remove_matching("devbox", &f.b.handle), 1);
    assert!(f.proxy.routes().iter().all(|r| r.host != f.b.host));
}

/// Another listener on `[::1]:<port>` (where a browser may send `*.localhost` first) stops the
/// proxy from starting on that port.
#[tokio::test]
async fn bind_refuses_a_port_held_on_either_loopback() {
    let v4 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let p = v4.local_addr().unwrap().port();
    let e = Proxy::bind(p).await.unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::AddrInUse);
    drop(v4);
    if let Ok(v6) = std::net::TcpListener::bind("[::1]:0") {
        let p = v6.local_addr().unwrap().port();
        if std::net::TcpListener::bind(("127.0.0.1", p)).is_ok() {
            let e = Proxy::bind(p).await.unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::AddrInUse, "{p}");
        }
        // Ephemeral: both families on one port.
        let ls = Proxy::bind(0).await.unwrap();
        assert_eq!(ls.len(), 2);
        assert_eq!(
            ls[0].local_addr().unwrap().port(),
            ls[1].local_addr().unwrap().port()
        );
    }
}

#[tokio::test]
async fn websocket_and_streaming_through_the_proxy() {
    let f = fixture().await;
    let port = f.proxy.port();
    let sess = login(&f, &f.a).await;
    let own = f.proxy.origin(&f.a.host);
    // HMR-style WebSocket: 101, then bytes both ways; the app saw its own origin and host.
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(
        format!(
            "GET /ws?token=hmr HTTP/1.1\r\nHost: {}:{port}\r\nCookie: {COOKIE}={sess}\r\nOrigin: {own}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
            f.a.host
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let head = read_head(&mut s).await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    assert!(
        head.to_ascii_lowercase().contains("upgrade: websocket"),
        "{head}"
    );
    let frame = b"\x81\x0bhello-frame";
    s.write_all(frame).await.unwrap();
    let mut buf = [0u8; 13];
    tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buf, frame);
    let up = f.seen_a.last().to_ascii_lowercase();
    assert!(up.starts_with("get /ws?token=hmr http/1.1"), "{up}");
    assert!(
        up.contains("upgrade: websocket") && up.contains("connection: upgrade"),
        "{up}"
    );
    assert!(up.contains("origin: http://localhost:5173\r\n"), "{up}");
    assert!(!up.contains("vk_preview"), "{up}");
    assert_eq!(f.proxy.stats().websockets, 1);
    drop(s);

    // SSE: the first event arrives before the app writes the second (no buffering).
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(
        get(
            &f.a.host,
            port,
            "/sse",
            &format!("Cookie: {COOKIE}={sess}\r\n"),
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut got = Vec::new();
    let mut b = [0u8; 256];
    let first = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let n = s.read(&mut b).await.unwrap();
            assert!(n > 0, "closed early: {}", String::from_utf8_lossy(&got));
            got.extend_from_slice(&b[..n]);
            if String::from_utf8_lossy(&got).contains("data: one") {
                break;
            }
        }
    })
    .await;
    assert!(
        first.is_ok(),
        "SSE was buffered: {}",
        String::from_utf8_lossy(&got)
    );
    assert!(!String::from_utf8_lossy(&got).contains("data: two"));
    f.go_a.notify_one();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut got)).await;
    assert!(String::from_utf8_lossy(&got).contains("data: two"));
}

#[tokio::test]
async fn keep_alive_reuses_the_upstream_and_unreachable_is_502() {
    let f = fixture().await;
    let port = f.proxy.port();
    let sess = login(&f, &f.a).await;
    // An upstream that is down: 502, not a hang.
    f.up.map.lock().unwrap().remove(&5173);
    let r = send(
        port,
        get(
            &f.a.host,
            port,
            "/",
            &format!("Cookie: {COOKIE}={sess}\r\n"),
        ),
    )
    .await;
    assert_eq!(r.status, 502, "{}", r.head);
    assert!(r.body.contains("port 5173 of devbox"), "{}", r.body);
    // Re-registering the same preview keeps its host (and its sessions).
    let again = f.proxy.register(Route {
        host: "something-else.vibeke.localhost".into(),
        ..f.a.clone()
    });
    assert_eq!(again.host, f.a.host);
    // Name collision with another preview: a suffixed host.
    let c = f.proxy.register(Route {
        preview: "01C".into(),
        ..f.a.clone()
    });
    assert_ne!(c.host, f.a.host);
    assert!(
        c.host.ends_with(".vibeke.localhost") && c.host.contains("-2"),
        "{}",
        c.host
    );
    f.proxy.remove_preview("devbox", "01C");
    assert!(!f.proxy.routes().iter().any(|r| r.preview == "01C"));
}

#[tokio::test]
async fn https_upstream_with_a_self_signed_certificate() {
    let Some(cert) = crate::testutil::self_signed() else {
        eprintln!("openssl not available; skipping");
        return;
    };
    let srv = crate::testutil::tls_server(
        &cert,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 13\r\nConnection: close\r\n\r\n<p>secure</p>",
    )
    .await;
    let up = Arc::new(FakeUpstream {
        map: StdMutex::new(HashMap::from([(8443, srv.port)])),
        connects: AtomicU64::new(0),
        gone: StdMutex::new(vec![]),
        checks: AtomicU64::new(0),
    });
    let proxy = Proxy::new(up);
    proxy.serve(Proxy::bind(0).await.unwrap());
    let r = proxy.register(Route {
        host: hostname_for("v9", None, Some("tls"), "s1"),
        machine: "local".into(),
        preview: "01T".into(),
        handle: "v9".into(),
        port: 8443,
        scheme: "https".into(),
    });
    let port = proxy.port();
    let t = proxy.mint_token(&r.host).unwrap();
    let resp = send(
        port,
        get(&r.host, port, &format!("/?{TOKEN_PARAM}={t}"), ""),
    )
    .await;
    let c = resp.header("set-cookie").unwrap();
    let sess = c
        .split(';')
        .next()
        .unwrap()
        .split_once('=')
        .unwrap()
        .1
        .to_string();
    let resp = send(
        port,
        get(&r.host, port, "/", &format!("Cookie: {COOKIE}={sess}\r\n")),
    )
    .await;
    assert_eq!(resp.status, 200, "{}", resp.head);
    assert_eq!(resp.body, "<p>secure</p>");
    let seen = srv.requests.lock().unwrap().join("\n").to_ascii_lowercase();
    assert!(seen.contains("host: localhost:8443"), "{seen}");
    assert!(!seen.contains("vk_"), "{seen}");
}
