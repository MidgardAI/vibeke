//! Relay integration tests over real WebSockets (spec 16 §11).

use std::net::SocketAddr;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use vk_e2e::relay::{Ctrl, accept_message, host_auth_message, sign_ticket};
use vk_e2e::{HostKeys, b64};
use vk_relay::{Authorizer, Config, Decision, HostConnect, Limits, Open, Relay, StaticTokens};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn start(limits: Limits) -> SocketAddr {
    start_with(
        Config {
            limits,
            ..Config::default()
        },
        Box::new(Open),
    )
    .await
}

async fn start_with(cfg: Config, auth: Box<dyn Authorizer>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let relay = Relay::new(
        Config {
            public_origins: vec![format!("http://{addr}")],
            log_ip_raw: true,
            ..cfg
        },
        auth,
    )
    .unwrap();
    let app = relay
        .router()
        .into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

async fn ws(addr: SocketAddr, path: &str) -> Ws {
    connect_async(format!("ws://{addr}{path}")).await.unwrap().0
}

async fn next_text(ws: &mut Ws) -> String {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timeout")
        {
            Some(Ok(Message::Text(t))) => return t.to_string(),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            other => panic!("expected text, got {other:?}"),
        }
    }
}

async fn close_code(ws: &mut Ws) -> u16 {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timeout")
        {
            Some(Ok(Message::Close(Some(f)))) => return u16::from(f.code),
            Some(Ok(Message::Close(None))) | None | Some(Err(_)) => return 0,
            Some(Ok(_)) => continue,
        }
    }
}

async fn register_with(addr: SocketAddr, keys: &HostKeys, query: &str) -> Ws {
    let mut ctrl = ws(addr, &format!("/v1/host{query}")).await;
    let Ctrl::Challenge { nonce, origin } = Ctrl::parse(&next_text(&mut ctrl).await).unwrap()
    else {
        panic!()
    };
    let sig = keys.sign(&host_auth_message(&origin, &b64::decode(&nonce).unwrap()));
    let auth = Ctrl::Auth {
        host: keys.host_id(),
        public: b64::encode(keys.relay_public()),
        sig: b64::encode(sig),
    };
    ctrl.send(Message::Text(auth.to_text().into()))
        .await
        .unwrap();
    ctrl
}

struct Host {
    keys: HostKeys,
    ctrl: Ws,
    generation: u64,
    origin: String,
}

async fn register(addr: SocketAddr, keys: HostKeys) -> Host {
    let mut ctrl = ws(addr, "/v1/host").await;
    let Ctrl::Challenge { nonce, origin } = Ctrl::parse(&next_text(&mut ctrl).await).unwrap()
    else {
        panic!()
    };
    assert_eq!(origin, format!("http://{addr}"));
    let sig = keys.sign(&host_auth_message(&origin, &b64::decode(&nonce).unwrap()));
    let auth = Ctrl::Auth {
        host: keys.host_id(),
        public: b64::encode(keys.relay_public()),
        sig: b64::encode(sig),
    };
    ctrl.send(Message::Text(auth.to_text().into()))
        .await
        .unwrap();
    let Ctrl::Ok { generation, .. } = Ctrl::parse(&next_text(&mut ctrl).await).unwrap() else {
        panic!()
    };
    Host {
        keys,
        ctrl,
        generation,
        origin,
    }
}

impl Host {
    async fn incoming(&mut self) -> (String, u64) {
        match Ctrl::parse(&next_text(&mut self.ctrl).await).unwrap() {
            Ctrl::Incoming { conn, generation } => (conn, generation),
            other => panic!("expected incoming, got {other:?}"),
        }
    }
    async fn accept(&self, addr: SocketAddr, conn: &str, generation: u64) -> Ws {
        let mut data = ws(addr, "/v1/accept").await;
        let sig = self.keys.sign(&accept_message(
            &self.origin,
            &self.keys.host_id(),
            generation,
            conn,
        ));
        let a = Ctrl::Accept {
            host: self.keys.host_id(),
            conn: conn.into(),
            generation,
            sig: b64::encode(sig),
        };
        data.send(Message::Text(a.to_text().into())).await.unwrap();
        data
    }
}

#[tokio::test]
async fn splice_preserves_messages_both_ways() {
    let addr = start(Limits::default()).await;
    let mut host = register(addr, HostKeys::generate()).await;
    let id = host.keys.host_id();
    let client = tokio::spawn(async move {
        let mut c = ws(addr, &format!("/v1/connect?host={id}")).await;
        c.send(Message::Text("hello".into())).await.unwrap();
        c.send(Message::Binary(vec![1, 2, 3].into())).await.unwrap();
        let back = c.next().await.unwrap().unwrap();
        assert_eq!(back, Message::Binary(vec![9].into()));
        c.close(None).await.ok();
    });
    let (conn, generation) = host.incoming().await;
    let mut data = host.accept(addr, &conn, generation).await;
    assert_eq!(next_text(&mut data).await, "hello");
    assert_eq!(
        data.next().await.unwrap().unwrap(),
        Message::Binary(vec![1, 2, 3].into())
    );
    data.send(Message::Binary(vec![9].into())).await.unwrap();
    client.await.unwrap();
}

#[tokio::test]
async fn bad_signature_is_rejected() {
    let addr = start(Limits::default()).await;
    let mut ctrl = ws(addr, "/v1/host").await;
    let Ctrl::Challenge { nonce, origin } = Ctrl::parse(&next_text(&mut ctrl).await).unwrap()
    else {
        panic!()
    };
    let keys = HostKeys::generate();
    let other = HostKeys::generate();
    // Signed by another key than the one claimed.
    let sig = other.sign(&host_auth_message(&origin, &b64::decode(&nonce).unwrap()));
    let auth = Ctrl::Auth {
        host: keys.host_id(),
        public: b64::encode(keys.relay_public()),
        sig: b64::encode(sig),
    };
    ctrl.send(Message::Text(auth.to_text().into()))
        .await
        .unwrap();
    assert_eq!(close_code(&mut ctrl).await, 4401);
}

#[tokio::test]
async fn wrong_origin_is_rejected() {
    let addr = start(Limits::default()).await;
    let mut ctrl = ws(addr, "/v1/host").await;
    let Ctrl::Challenge { nonce, .. } = Ctrl::parse(&next_text(&mut ctrl).await).unwrap() else {
        panic!()
    };
    let keys = HostKeys::generate();
    let sig = keys.sign(&host_auth_message(
        "https://evil.example",
        &b64::decode(&nonce).unwrap(),
    ));
    let auth = Ctrl::Auth {
        host: keys.host_id(),
        public: b64::encode(keys.relay_public()),
        sig: b64::encode(sig),
    };
    ctrl.send(Message::Text(auth.to_text().into()))
        .await
        .unwrap();
    assert_eq!(close_code(&mut ctrl).await, 4401);
}

#[tokio::test]
async fn offline_host_and_accept_timeout() {
    let limits = Limits {
        accept_timeout: Duration::from_millis(300),
        ..Limits::default()
    };
    let addr = start(limits).await;
    let id = HostKeys::generate().host_id();
    let mut c = ws(addr, &format!("/v1/connect?host={id}")).await;
    assert_eq!(close_code(&mut c).await, 4404);

    let mut host = register(addr, HostKeys::generate()).await;
    let id = host.keys.host_id();
    let mut c = ws(addr, &format!("/v1/connect?host={id}")).await;
    let (conn, generation) = host.incoming().await;
    assert_eq!(close_code(&mut c).await, 4408);
    // A late accept finds nothing.
    let mut late = host.accept(addr, &conn, generation).await;
    assert_eq!(close_code(&mut late).await, 4401);
}

#[tokio::test]
async fn replacement_is_fenced_and_stale_generation_rejected() {
    let addr = start(Limits::default()).await;
    let keys = HostKeys::generate();
    let mut old = register(addr, keys.clone()).await;
    let mut new = register(addr, keys.clone()).await;
    assert!(new.generation > old.generation);
    assert_eq!(close_code(&mut old.ctrl).await, 4409);
    drop(old.ctrl);
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The old socket's cleanup must not have removed the new registration.
    let status: serde_json::Value = serde_json::from_str(
        &reqwest_get(addr, &format!("/v1/status?host={}", keys.host_id())).await,
    )
    .unwrap();
    assert_eq!(status["online"], true);

    let id = keys.host_id();
    let client = tokio::spawn(async move {
        let mut c = ws(addr, &format!("/v1/connect?host={id}")).await;
        c.send(Message::Text("x".into())).await.unwrap();
        c
    });
    let (conn, generation) = new.incoming().await;
    // Accept under the old generation is refused; the pending entry survives for the right one.
    let mut stale = new.accept(addr, &conn, old.generation).await;
    assert_eq!(close_code(&mut stale).await, 4401);
    let mut data = new.accept(addr, &conn, generation).await;
    assert_eq!(next_text(&mut data).await, "x");
    drop(client.await.unwrap());
}

#[tokio::test]
async fn oversized_message_closes() {
    let limits = Limits {
        max_message: 1024,
        ..Limits::default()
    };
    let addr = start(limits).await;
    let mut host = register(addr, HostKeys::generate()).await;
    let id = host.keys.host_id();
    let mut c = ws(addr, &format!("/v1/connect?host={id}")).await;
    let (conn, generation) = host.incoming().await;
    let mut data = host.accept(addr, &conn, generation).await;
    c.send(Message::Binary(vec![0u8; 4096].into()))
        .await
        .unwrap();
    // The relay drops the oversized message's socket; the host side sees the splice close.
    let code = close_code(&mut data).await;
    assert!(code == 1001 || code == 1009 || code == 0, "code {code}");
}

/// Minimal HTTP GET without pulling in an HTTP client.
async fn reqwest_get(addr: SocketAddr, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).await.unwrap();
    buf.split("\r\n\r\n").nth(1).unwrap_or("").to_string()
}

#[tokio::test]
async fn pending_connections_count_against_the_global_cap() {
    let limits = Limits {
        max_conns: 1,
        accept_timeout: Duration::from_secs(2),
        ..Limits::default()
    };
    let addr = start(limits).await;
    let mut host = register(addr, HostKeys::generate()).await;
    let id = host.keys.host_id();
    let _first = ws(addr, &format!("/v1/connect?host={id}")).await;
    let _ = host.incoming().await;
    // The first connection is only pending (not accepted yet), but it holds the only slot.
    let mut second = ws(addr, &format!("/v1/connect?host={id}")).await;
    assert_eq!(close_code(&mut second).await, 4429);
}

#[tokio::test]
async fn announces_are_bounded_per_ip_and_host() {
    let limits = Limits {
        ip_host_announces_per_min: 2,
        ..Limits::default()
    };
    let addr = start(limits).await;
    let a = HostKeys::generate().host_id();
    let b = HostKeys::generate().host_id();
    for _ in 0..2 {
        let mut c = ws(addr, &format!("/v1/connect?host={a}")).await;
        assert_eq!(close_code(&mut c).await, 4404);
    }
    // The third announce for the same host from this address is refused before the upgrade.
    assert!(
        connect_async(format!("ws://{addr}/v1/connect?host={a}"))
            .await
            .is_err()
    );
    // Another host is unaffected.
    let mut c = ws(addr, &format!("/v1/connect?host={b}")).await;
    assert_eq!(close_code(&mut c).await, 4404);
}

#[tokio::test]
async fn unauthenticated_sockets_are_bounded_per_ip() {
    let limits = Limits {
        ip_unauthenticated: 2,
        ..Limits::default()
    };
    let addr = start(limits).await;
    let _a = ws(addr, "/v1/accept").await;
    let _b = ws(addr, "/v1/host").await;
    assert!(
        connect_async(format!("ws://{addr}/v1/accept"))
            .await
            .is_err()
    );
    // Authenticating frees the slot.
    drop(_a);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let host = register(addr, HostKeys::generate()).await;
    let _c = ws(addr, "/v1/accept").await;
    drop(host);
}

// ---------------------------------------------------------------------------------------------
// Tickets and authorizers (spec 16 §6.6)

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

async fn tickets_relay() -> SocketAddr {
    start_with(
        Config {
            require_tickets: true,
            account_url: Some("https://example.com/account".into()),
            ..Config::default()
        },
        Box::new(Open),
    )
    .await
}

/// Expect the plaintext refusal a browser can read, then close 4401.
async fn expect_denied(c: &mut Ws, reason: &str) {
    let v: serde_json::Value = serde_json::from_str(&next_text(c).await).unwrap();
    assert_eq!(
        v,
        serde_json::json!({ "error": "unauthorized", "reason": reason })
    );
    assert_eq!(close_code(c).await, 4401);
}

fn connect_path(host: &str, ticket: &str) -> String {
    format!("/v1/connect?host={host}&ticket={ticket}")
}

#[tokio::test]
async fn tickets_are_required_and_verified() {
    let addr = tickets_relay().await;
    let mut host = register(addr, HostKeys::generate()).await;
    let id = host.keys.host_id();

    let mut c = ws(addr, &format!("/v1/connect?host={id}")).await;
    expect_denied(&mut c, "ticket_missing").await;

    let expired = sign_ticket(&host.keys, "dev:d1", now() - 1);
    let mut c = ws(addr, &connect_path(&id, &expired)).await;
    expect_denied(&mut c, "ticket_expired").await;

    // Claims this host, signed by another key.
    let other = HostKeys::generate();
    let forged = sign_ticket(&other, "dev:d1", now() + 60);
    let (_, sig) = forged.split_once('.').unwrap();
    let claims = b64::encode(format!(
        r#"{{"v":1,"host":"{id}","sub":"dev:d1","exp":{}}}"#,
        now() + 60
    ));
    let mut c = ws(addr, &connect_path(&id, &format!("{claims}.{sig}"))).await;
    expect_denied(&mut c, "ticket_invalid").await;
    // Another host's ticket.
    let mut c = ws(addr, &connect_path(&id, &forged)).await;
    expect_denied(&mut c, "ticket_invalid").await;

    let good = sign_ticket(&host.keys, "dev:d1", now() + 60);
    let client = tokio::spawn(async move {
        let mut c = ws(addr, &connect_path(&id, &good)).await;
        c.send(Message::Text("hi".into())).await.unwrap();
        c
    });
    let (conn, generation) = host.incoming().await;
    let mut data = host.accept(addr, &conn, generation).await;
    assert_eq!(next_text(&mut data).await, "hi");
    drop(client.await.unwrap());
}

#[tokio::test]
async fn revoked_subject_is_refused_and_disconnected() {
    let addr = tickets_relay().await;
    let mut host = register(addr, HostKeys::generate()).await;
    let id = host.keys.host_id();
    let ticket = sign_ticket(&host.keys, "dev:d1", now() + 60);

    // A live splice for the subject closes when the host revokes it.
    let mut live = ws(addr, &connect_path(&id, &ticket)).await;
    let (conn, generation) = host.incoming().await;
    let mut data = host.accept(addr, &conn, generation).await;
    live.send(Message::Text("x".into())).await.unwrap();
    assert_eq!(next_text(&mut data).await, "x");
    let revoke = Ctrl::Revoke {
        sub: "dev:d1".into(),
    };
    host.ctrl
        .send(Message::Text(revoke.to_text().into()))
        .await
        .unwrap();
    assert_eq!(close_code(&mut live).await, 4401);
    assert_eq!(close_code(&mut data).await, 4401);

    let mut c = ws(addr, &connect_path(&id, &ticket)).await;
    expect_denied(&mut c, "ticket_revoked").await;
    // Other subjects are unaffected.
    let other = sign_ticket(&host.keys, "dev:d2", now() + 60);
    let _c = ws(addr, &connect_path(&id, &other)).await;
    host.incoming().await;
}

#[tokio::test]
async fn open_relay_still_checks_present_tickets() {
    let addr = start(Limits::default()).await;
    let mut host = register(addr, HostKeys::generate()).await;
    let id = host.keys.host_id();
    let _ticketless = ws(addr, &format!("/v1/connect?host={id}")).await;
    host.incoming().await;
    let mut c = ws(addr, &connect_path(&id, "garbage")).await;
    expect_denied(&mut c, "ticket_invalid").await;
    let foreign = sign_ticket(&HostKeys::generate(), "dev:d1", now() + 60);
    let mut c = ws(addr, &connect_path(&id, &foreign)).await;
    expect_denied(&mut c, "ticket_invalid").await;
}

struct DenyHosts;
impl Authorizer for DenyHosts {
    fn host_connect<'a>(&'a self, _: HostConnect<'a>) -> BoxFuture<'a, Decision> {
        Box::pin(async { Decision::deny(4401, "account_required") })
    }
}

#[tokio::test]
async fn denied_host_gets_error_before_close() {
    let addr = start_with(Config::default(), Box::new(DenyHosts)).await;
    let mut ctrl = register_with(addr, &HostKeys::generate(), "").await;
    assert_eq!(
        Ctrl::parse(&next_text(&mut ctrl).await).unwrap(),
        Ctrl::Error {
            code: 4401,
            reason: "account_required".into()
        }
    );
    assert_eq!(close_code(&mut ctrl).await, 4401);
}

#[tokio::test]
async fn static_tokens_gate_hosts() {
    let addr = start_with(
        Config::default(),
        Box::new(StaticTokens(vec!["s3cret".into()])),
    )
    .await;
    let keys = HostKeys::generate();
    for query in ["", "?token=wrong"] {
        let mut ctrl = register_with(addr, &keys, query).await;
        assert_eq!(
            Ctrl::parse(&next_text(&mut ctrl).await).unwrap(),
            Ctrl::Error {
                code: 4401,
                reason: "token_invalid".into()
            }
        );
        assert_eq!(close_code(&mut ctrl).await, 4401);
    }
    let mut ctrl = register_with(addr, &keys, "?token=s3cret").await;
    assert!(matches!(
        Ctrl::parse(&next_text(&mut ctrl).await).unwrap(),
        Ctrl::Ok { .. }
    ));
}

#[tokio::test]
async fn status_reports_auth_mode() {
    let addr = tickets_relay().await;
    let id = HostKeys::generate().host_id();
    let v: serde_json::Value =
        serde_json::from_str(&reqwest_get(addr, &format!("/v1/status?host={id}")).await).unwrap();
    assert_eq!(
        v,
        serde_json::json!({
            "online": false,
            "auth": "tickets",
            "account_url": "https://example.com/account",
        })
    );
    let addr = start(Limits::default()).await;
    let v: serde_json::Value =
        serde_json::from_str(&reqwest_get(addr, &format!("/v1/status?host={id}")).await).unwrap();
    assert_eq!(v["auth"], "open");
    assert_eq!(v["account_url"], serde_json::Value::Null);
}
