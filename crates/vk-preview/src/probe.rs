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

async fn connect_any(addrs: &[SocketAddr], timeout: Duration) -> Option<TcpStream> {
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
}
