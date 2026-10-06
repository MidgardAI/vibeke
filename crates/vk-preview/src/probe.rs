//! Liveness and HTTP classification probes against this machine's loopback (06 B2).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Where to reach a listener bound to `bind`: wildcard and loopback binds of either family.
pub fn targets(bind: Option<IpAddr>, port: u16) -> Vec<SocketAddr> {
    let v4 = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, port));
    match bind {
        Some(IpAddr::V4(a)) if a.is_loopback() => vec![SocketAddr::from((a, port))],
        Some(IpAddr::V4(_)) => vec![v4],
        Some(IpAddr::V6(a)) if a.is_loopback() => vec![v6],
        // `::` usually accepts v4 too (dual stack); try v6 first.
        Some(IpAddr::V6(_)) => vec![v6, v4],
        None => vec![v4, v6],
    }
}

pub(crate) async fn connect_any(addrs: &[SocketAddr], timeout: Duration) -> Option<TcpStream> {
    for a in addrs {
        if let Ok(Ok(s)) = tokio::time::timeout(timeout, TcpStream::connect(a)).await {
            return Some(s);
        }
    }
    None
}

/// Something accepts TCP connections on `port` (loopback, either family).
pub async fn tcp_alive(bind: Option<IpAddr>, port: u16) -> bool {
    connect_any(&targets(bind, port), Duration::from_millis(500))
        .await
        .is_some()
}

/// Send `HEAD / HTTP/1.0` and accept any `HTTP/` reply within `timeout` (default 1 s).
pub async fn is_http(bind: Option<IpAddr>, port: u16, timeout: Duration) -> bool {
    let fut = async {
        let mut s = connect_any(&targets(bind, port), timeout).await?;
        s.write_all(b"HEAD / HTTP/1.0\r\nHost: localhost\r\n\r\n")
            .await
            .ok()?;
        let mut buf = [0u8; 5];
        let mut got = 0;
        while got < 5 {
            let n = s.read(&mut buf[got..]).await.ok()?;
            if n == 0 {
                return None;
            }
            got += n;
        }
        (&buf == b"HTTP/").then_some(())
    };
    matches!(tokio::time::timeout(timeout, fut).await, Ok(Some(())))
}

/// Whether a raw HTTP response (head and the start of the body) is a web page worth suggesting
/// as a preview: `2xx`/`3xx` (or a `404` HTML page, an SPA's not-found route) that is
/// `text/html` — or has no content type and a body that starts like HTML. Redirects with a
/// `Location` count. `426 Upgrade Required`, `400`, websocket-only and JSON(-RPC) endpoints
/// (agent harness bridges, APIs) do not.
pub fn looks_like_web_page(resp: &[u8]) -> bool {
    let text = String::from_utf8_lossy(resp);
    let (head, body) = match text.split_once("\r\n\r\n") {
        Some((h, b)) => (h.to_string(), b.to_string()),
        None => (text.to_string(), String::new()),
    };
    let mut lines = head.lines();
    let Some(status) = lines
        .next()
        .filter(|l| l.starts_with("HTTP/"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
    else {
        return false;
    };
    let mut ctype = None;
    let mut location = false;
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "content-type" => ctype = Some(v.trim().to_ascii_lowercase()),
                "location" => location = !v.trim().is_empty(),
                _ => {}
            }
        }
    }
    let html_body = {
        let b = body.trim_start().to_ascii_lowercase();
        b.starts_with("<!doctype") || b.starts_with("<html")
    };
    let html = match &ctype {
        Some(c) => c.starts_with("text/html") || c.starts_with("application/xhtml"),
        None => html_body,
    };
    match status {
        200..=299 => html,
        300..=399 => html || location,
        404 => html,
        _ => false,
    }
}

/// `GET /` (Accept: text/html) and classify with [`looks_like_web_page`] within `timeout`.
/// Used for preview *suggestions*; declared previews are not probed this way.
pub async fn is_web_page(bind: Option<IpAddr>, port: u16, timeout: Duration) -> bool {
    let fut = async {
        let mut s = connect_any(&targets(bind, port), timeout).await?;
        s.write_all(
            b"GET / HTTP/1.0\r\nHost: localhost\r\nAccept: text/html,application/xhtml+xml\r\nUser-Agent: vibeke-preview-probe\r\nConnection: close\r\n\r\n",
        )
        .await
        .ok()?;
        let mut buf = Vec::with_capacity(4096);
        let mut chunk = [0u8; 2048];
        while buf.len() < 4096 {
            let n = s.read(&mut chunk).await.ok()?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            // Enough to decide: the head plus the start of the body.
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n")
                && buf.len() >= i + 4 + 64
            {
                break;
            }
        }
        Some(looks_like_web_page(&buf))
    };
    matches!(tokio::time::timeout(timeout, fut).await, Ok(Some(true)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn classifies_http_and_other_tcp() {
        let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hp = http.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = http.accept().await {
                let mut b = [0u8; 256];
                let _ = s.read(&mut b).await;
                let _ = s.write_all(b"HTTP/1.0 200 OK\r\n\r\n").await;
            }
        });
        let raw = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let rp = raw.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = raw.accept().await {
                let _ = s.write_all(b"SSH-2.0-x\r\n").await;
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
        let t = Duration::from_secs(1);
        assert!(is_http(Some("127.0.0.1".parse().unwrap()), hp, t).await);
        assert!(is_http(None, hp, t).await);
        assert!(!is_http(None, rp, t).await);
        assert!(tcp_alive(None, rp).await);
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(!tcp_alive(Some("127.0.0.1".parse().unwrap()), closed).await);
    }

    #[test]
    fn web_page_classification() {
        let page = |r: &str| looks_like_web_page(r.as_bytes());
        assert!(page(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<!doctype html>"
        ));
        assert!(page("HTTP/1.1 200 OK\r\n\r\n<!DOCTYPE html><html>"));
        assert!(page("HTTP/1.1 302 Found\r\nLocation: /app/\r\n\r\n"));
        assert!(page(
            "HTTP/1.1 404 Not Found\r\nContent-Type: text/html\r\n\r\n<html>"
        ));
        // Harness bridges, APIs, websockets.
        assert!(!page(
            "HTTP/1.1 426 Upgrade Required\r\nContent-Type: text/plain\r\n\r\nUpgrade Required"
        ));
        assert!(!page("HTTP/1.1 400 Bad Request\r\n\r\n"));
        assert!(!page(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"jsonrpc\":\"2.0\"}"
        ));
        assert!(!page("HTTP/1.1 200 OK\r\n\r\n{\"ok\":true}"));
        assert!(!page(
            "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\n\r\n{}"
        ));
        assert!(!page("SSH-2.0-x\r\n"));
    }

    /// Fake harness listeners (a 426 websocket bridge, a 400, a JSON-RPC endpoint) are not
    /// web pages; a dev server is.
    #[tokio::test]
    async fn harness_bridges_are_not_web_pages() {
        async fn serve(resp: &'static str) -> u16 {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let p = l.local_addr().unwrap().port();
            tokio::spawn(async move {
                while let Ok((mut s, _)) = l.accept().await {
                    let mut b = [0u8; 512];
                    let _ = s.read(&mut b).await;
                    let _ = s.write_all(resp.as_bytes()).await;
                }
            });
            p
        }
        let t = Duration::from_secs(1);
        let ws = serve("HTTP/1.1 426 Upgrade Required\r\nConnection: close\r\n\r\n").await;
        let bad = serve("HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n").await;
        let json = serve("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"jsonrpc\":\"2.0\",\"result\":{}}").await;
        let vite = serve("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n<!doctype html><html><head><script type=\"module\" src=\"/@vite/client\"></script></head></html>").await;
        for p in [ws, bad, json] {
            assert!(is_http(None, p, t).await, "speaks HTTP");
            assert!(!is_web_page(None, p, t).await, "port {p} suggested");
        }
        assert!(is_web_page(None, vite, t).await);
    }
}
