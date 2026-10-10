//! End to end: relay + gateway + a fake Vibeke server + a device client speaking vibeke-e2e/1.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UnixListener};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use vk_e2e::{DeviceKey, Hello, Initiator, PairingLink, Session};
use vk_gateway::state::{PairingStatus, Scope, StateDir};
use vk_gateway::{Gateway, pair, server};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn start_relay() -> SocketAddr {
    start_relay_with(false, Box::new(vk_relay::Open)).await
}

async fn start_relay_with(
    require_tickets: bool,
    auth: Box<dyn vk_relay::Authorizer>,
) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let relay = vk_relay::Relay::new(
        vk_relay::Config {
            public_origins: vec![format!("http://{addr}")],
            log_ip_raw: true,
            require_tickets,
            // Requiring tickets alone needs no account; naming an account server does.
            account_url: require_tickets.then(|| "http://accounts.test".to_string()),
            ..Default::default()
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

/// A tiny stand-in for the Vibeke server's NDJSON JSON-RPC socket.
/// The `client.devices` reports the fake server received, in order.
type Reports = std::sync::Arc<std::sync::Mutex<Vec<Value>>>;

fn fake_server(path: PathBuf) -> Reports {
    let listener = UnixListener::bind(&path).unwrap();
    let reports = Reports::default();
    let out = reports.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let reports = reports.clone();
            tokio::spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let id = req["id"].clone();
                    let p = &req["params"];
                    if req["method"] == "client.hello"
                        && p["client"] == "vibeke-browser-tui"
                        && reports
                            .lock()
                            .unwrap()
                            .iter()
                            .any(|v| v["tui"] == "delay-attach")
                    {
                        reports
                            .lock()
                            .unwrap()
                            .push(json!({"tui":"attach-started"}));
                        assert!(lines.next_line().await.unwrap().is_none());
                        reports
                            .lock()
                            .unwrap()
                            .push(json!({"tui":"attach-cancelled"}));
                        return;
                    }
                    if req["method"] == "render.attach" {
                        assert_eq!(p["remote"], true);
                        reports.lock().unwrap().push(json!({"tui":"attached"}));
                        let reply = json!({"jsonrpc":"2.0", "id":id, "result":{"protocol":vk_proto::render::PROTOCOL,"features":["scoped_share"]}});
                        w.write_all(format!("{reply}\n").as_bytes()).await.unwrap();
                        let mut r = lines.into_inner();
                        while let Ok(f) = vk_proto::frame::asyncio::read_frame::<
                            _,
                            vk_proto::render::ClientFrame,
                        >(&mut r)
                        .await
                        {
                            use vk_proto::render::{ClientFrame, ServerFrame};
                            let response = match f {
                                ClientFrame::Ping { nonce } => ServerFrame::Pong {
                                    nonce,
                                    server_ts_ms: 0,
                                },
                                ClientFrame::Command { req, json } => {
                                    ServerFrame::CommandResult { req, json }
                                }
                                _ => continue,
                            };
                            if vk_proto::frame::asyncio::write_frame(&mut w, &response)
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        reports.lock().unwrap().push(json!({"tui":"closed"}));
                        return;
                    }
                    if req["method"] == "client.hello"
                        && p["client"] == "vibeke-browser-tui-command"
                    {
                        assert_eq!(p["kind"], "gateway-tui");
                        assert_eq!(p["remote"], true);
                        reports.lock().unwrap().push(json!({"tui":"control-hello"}));
                    }
                    if req["method"] == "handoff.test_slow" {
                        reports
                            .lock()
                            .unwrap()
                            .push(json!({"tui":"control-started", "actor":p["actor"]}));
                        // Withhold the command response until the gateway closes its reader.
                        assert!(lines.next_line().await.unwrap().is_none());
                        reports
                            .lock()
                            .unwrap()
                            .push(json!({"tui":"control-closed"}));
                        return;
                    }
                    let result = match req["method"].as_str().unwrap() {
                        "client.hello" => {
                            json!({"server_version": "test", "capabilities": ["*"], "features":["render.scoped_share"]})
                        }
                        "server.status" => json!({}),
                        "session.snapshot" => {
                            json!({"at_seq": 7, "workspaces": [], "panes": [], "runs": [],
                            "interactions": [{"id": "i1", "kind": "Approval", "status": "Open", "decision_rev": 3, "answerable": true,
                                              "action": {"tool": "Bash", "command": "pnpm test", "risk": "Low"}}]})
                        }
                        "pane.get" => json!({"pane":{"id":p["pane"],"workspace":"w1"}}),
                        "notification.list" => json!({"notifications": []}),
                        "interaction.get" => {
                            json!({"interaction": {"id": p["interaction"], "pane":"p1", "kind": "Approval", "status": "Open", "decision_rev": 3,
                                                    "answerable": true, "action": {"tool": "Bash", "command": "pnpm test", "risk": "Low"}}})
                        }
                        "interaction.answer" => {
                            json!({"interaction": {"id": p["interaction"], "status": "Answered", "delivery": "Delivering"},
                                                      "delivery": {"channel": "native"}, "echo": p})
                        }
                        "events.subscribe" => json!({"subscription_id": "s", "at": {"seq": 7}}),
                        "agent.commands" => {
                            json!({"commands": [{"name": "model", "description": "Set the AI model", "takes_arg": true, "opens_picker": true, "dangerous": false}],
                                   "source": "catalog", "echo": p})
                        }
                        "agent.models" => {
                            let line = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32010, "message": "no structured model control",
                                "data": {"kind": "unsupported", "details": {"reason": "harness", "fallback": "/model"}, "retryable": false}}})
                            .to_string()
                                + "\n";
                            if w.write_all(line.as_bytes()).await.is_err() {
                                return;
                            }
                            continue;
                        }
                        "client.devices" => {
                            reports.lock().unwrap().push(p["devices"].clone());
                            json!({})
                        }
                        _ => json!({}),
                    };
                    let line =
                        json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string() + "\n";
                    if w.write_all(line.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    out
}

/// Wait for the latest `client.devices` report to satisfy `ok`.
async fn report_where(reports: &Reports, ok: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..250 {
        if let Some(last) = reports.lock().unwrap().last().filter(|l| ok(l)) {
            return last.clone();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no matching report in {:?}", reports.lock().unwrap());
}

struct Client {
    ws: Ws,
    session: Session,
    next: u64,
}

impl Client {
    async fn open(
        relay: SocketAddr,
        host: &str,
        hello: Hello,
        dev: &DeviceKey,
        hk: &[u8; 32],
        psk: Option<&[u8; 32]>,
    ) -> Result<Client, String> {
        Self::open_with(relay, host, None, hello, dev, hk, psk).await
    }

    /// [`open`](Self::open) presenting a relay ticket (`&ticket=`, spec 16 §6.6).
    async fn open_with(
        relay: SocketAddr,
        host: &str,
        ticket: Option<&str>,
        hello: Hello,
        dev: &DeviceKey,
        hk: &[u8; 32],
        psk: Option<&[u8; 32]>,
    ) -> Result<Client, String> {
        let mut url = format!("ws://{relay}/v1/connect?host={host}");
        if let Some(t) = ticket {
            url.push_str(&format!("&ticket={t}"));
        }
        let (mut ws, _) = connect_async(url).await.unwrap();
        let hb = hello.to_bytes();
        // A refused connection may already be closed; its plaintext refusal is still readable.
        let _ = ws
            .send(Message::Text(String::from_utf8(hb.clone()).unwrap().into()))
            .await;
        let mut i = Initiator::new(&hb, &dev.private, hk, psk).unwrap();
        let _ = ws
            .send(Message::Binary(i.write_first(b"").unwrap().into()))
            .await;
        match tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .unwrap()
        {
            Some(Ok(Message::Binary(m2))) => {
                let (payload, session) = i.read_second(&m2).map_err(|e| e.to_string())?;
                let info: Value = serde_json::from_slice(&payload).unwrap();
                assert_eq!(info["v"], 1);
                Ok(Client {
                    ws,
                    session,
                    next: 1,
                })
            }
            Some(Ok(Message::Text(t))) => Err(t.to_string()),
            other => Err(format!("{other:?}")),
        }
    }

    async fn send(&mut self, v: Value) {
        for f in self.session.encrypt(v.to_string().as_bytes()).unwrap() {
            self.ws.send(Message::Binary(f.into())).await.unwrap();
        }
    }

    async fn recv(&mut self) -> Value {
        loop {
            match tokio::time::timeout(Duration::from_secs(10), self.ws.next())
                .await
                .expect("recv timeout")
            {
                Some(Ok(Message::Binary(b))) => {
                    if let Some(m) = self.session.decrypt(&b).unwrap() {
                        return serde_json::from_slice(&m).unwrap();
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        loop {
            let m = self.recv().await;
            if m["id"] == id {
                return m;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pair_confirm_call_and_revoke() {
    let tmp = tempfile::tempdir().unwrap();
    let relay = start_relay().await;
    let sock = tmp.path().join("vibeke.sock");
    fake_server(sock.clone());

    let state = StateDir::open(tmp.path().join("gw")).unwrap();
    let mut cfg = state.config().unwrap();
    cfg.relay = Some(format!("http://{relay}"));
    cfg.host_name = Some("devbox".into());
    state.save_config(&cfg).unwrap();
    let (pairing, link) = pair::create(
        &state,
        &format!("http://{relay}"),
        "devbox",
        Scope::Approve,
        false,
        Duration::from_secs(300),
    )
    .unwrap();
    let url = link.to_url(&format!("http://{relay}"));

    let gw = Gateway::new(
        StateDir::open(tmp.path().join("gw")).unwrap(),
        server::Server::new(sock),
    )
    .unwrap();
    tokio::spawn(vk_gateway::run(gw.clone()));
    for _ in 0..100 {
        if reqwest_status(relay, &link.host).await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Operator side: confirm once claimed.
    let st = StateDir::open(tmp.path().join("gw")).unwrap();
    let pid = pairing.pid.clone();
    let confirmer = tokio::spawn(async move {
        loop {
            if let Some(mut p) = st.pairing(&pid).unwrap()
                && let PairingStatus::Claimed { claim_id, .. } = p.status.clone()
            {
                p.confirmed = Some(true);
                p.confirmed_claim = Some(claim_id);
                st.save_pairing(&p).unwrap();
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });

    // Device side: pair.
    let link = PairingLink::parse(&url).unwrap();
    let dev = DeviceKey::generate();
    let hk = link.host_key().unwrap();
    let psk = link.psk_bytes().unwrap();

    // A wrong psk fails the handshake and does not burn the pairing.
    let bad = Client::open(
        relay,
        &link.host,
        Hello::pair(&link.pid),
        &dev,
        &hk,
        Some(&[9; 32]),
    )
    .await;
    if let Ok(mut c) = bad {
        c.send(json!({"jsonrpc": "2.0", "id": 1, "method": "pair.claim", "params": {}}))
            .await;
        assert!(
            tokio::time::timeout(Duration::from_secs(2), c.ws.next())
                .await
                .map_or(true, |m| !matches!(m, Some(Ok(Message::Binary(_)))))
        );
    }
    assert_eq!(
        gw.state.pairing(&link.pid).unwrap().unwrap().status,
        PairingStatus::Pending
    );

    let mut c = Client::open(
        relay,
        &link.host,
        Hello::pair(&link.pid),
        &dev,
        &hk,
        Some(&psk),
    )
    .await
    .unwrap();
    let r = c
        .call(
            "pair.claim",
            json!({"name": "test phone", "platform": "test"}),
        )
        .await;
    assert_eq!(r["result"]["status"], "pending");
    assert_eq!(
        r["result"]["fingerprint"],
        vk_e2e::keys::fingerprint(&dev.public())
    );
    let done = c.recv().await;
    assert_eq!(done["method"], "pair.done", "{done}");
    assert_eq!(done["params"]["scope"], "approve");
    confirmer.await.unwrap();
    let device_id = done["params"]["device_id"].as_str().unwrap().to_string();

    // The pairing is consumed.
    assert!(
        Client::open(
            relay,
            &link.host,
            Hello::pair(&link.pid),
            &DeviceKey::generate(),
            &hk,
            Some(&psk)
        )
        .await
        .is_err()
    );

    // An unknown device is refused.
    let err = Client::open(
        relay,
        &link.host,
        Hello::device(),
        &DeviceKey::generate(),
        &hk,
        None,
    )
    .await
    .err()
    .unwrap();
    assert!(err.contains("unauthorized"), "{err}");

    // Paired device: normal calls.
    let mut c = Client::open(relay, &link.host, Hello::device(), &dev, &hk, None)
        .await
        .unwrap();
    let h = c
        .call("hello", json!({"client": "test", "visible": true}))
        .await;
    assert_eq!(h["result"]["device_id"], device_id);
    let d = c.call("dashboard.get", json!({})).await;
    assert_eq!(d["result"]["at"], 7);
    assert_eq!(d["result"]["interactions"][0]["kind"], "approval");
    assert_eq!(d["result"]["interactions"][0]["action"]["risk"], "low");

    // Pickers: the command catalog and model list are reads (no op_id); an unsupported harness
    // stays `unsupported`; switching the model needs full scope.
    let cmds = c.call("agent.commands", json!({"target": "r1"})).await;
    assert_eq!(cmds["result"]["commands"][0]["name"], "model", "{cmds}");
    assert_eq!(cmds["result"]["echo"], json!({"target": "r1"}));
    let models = c.call("agent.models", json!({"target": "r1"})).await;
    assert_eq!(models["error"]["data"]["kind"], "unsupported", "{models}");
    assert_eq!(models["error"]["data"]["details"]["fallback"], "/model");
    let set = c
        .call(
            "agent.set_model",
            json!({"target": "r1", "model": "m", "op_id": "o0"}),
        )
        .await;
    assert_eq!(set["error"]["data"]["kind"], "forbidden", "{set}");

    // Scope: approve may not type into panes.
    let f = c
        .call(
            "pane.send_keys",
            json!({"pane": "p1", "keys": ["Enter"], "op_id": "o1"}),
        )
        .await;
    assert_eq!(f["error"]["data"]["kind"], "forbidden");
    // Mutations need op_id; stale decision_rev is refused.
    let e = c
        .call(
            "interaction.answer",
            json!({"interaction": "i1", "decision": "allow"}),
        )
        .await;
    assert_eq!(e["error"]["data"]["kind"], "invalid_params");
    let s = c
        .call(
            "interaction.answer",
            json!({"interaction": "i1", "decision": "allow", "decision_rev": 2, "op_id": "o2"}),
        )
        .await;
    assert_eq!(s["error"]["data"]["kind"], "stale");
    let ok = c
        .call(
            "interaction.answer",
            json!({"interaction": "i1", "decision": "allow", "decision_rev": 3, "op_id": "o3"}),
        )
        .await;
    assert_eq!(ok["result"]["interaction"]["status"], "answered");
    assert_eq!(ok["result"]["echo"]["actor"], "gateway:test phone");
    // Same op_id + same params → same result, no second answer; different params → refused.
    let again = c
        .call(
            "interaction.answer",
            json!({"interaction": "i1", "decision": "allow", "decision_rev": 3, "op_id": "o3"}),
        )
        .await;
    assert_eq!(again["result"], ok["result"]);
    let clash = c
        .call(
            "interaction.answer",
            json!({"interaction": "i1", "decision": "deny", "decision_rev": 3, "op_id": "o3"}),
        )
        .await;
    assert_eq!(clash["error"]["data"]["kind"], "invalid_params");

    // Revoke: the live connection is told and closed.
    gw.revoke(&device_id).await.unwrap();
    let m = c.recv().await;
    assert_eq!(m["method"], "device.revoked");
    let err = Client::open(relay, &link.host, Hello::device(), &dev, &hk, None)
        .await
        .err()
        .unwrap();
    assert!(err.contains("unauthorized"));
}

async fn reqwest_status(addr: SocketAddr, host: &str) -> bool {
    use tokio::io::AsyncReadExt;
    let Ok(mut s) = TcpStream::connect(addr).await else {
        return false;
    };
    let req =
        format!("GET /v1/status?host={host} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).await.unwrap();
    buf.contains("\"online\":true")
}

/// Local transport: the same Noise channel over the gateway's Unix socket (desktop on this machine).
#[tokio::test(flavor = "multi_thread")]
async fn local_socket_pairs_and_serves() {
    use tokio_tungstenite::client_async;
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("vibeke.sock");
    let reports = fake_server(sock.clone());
    let state = StateDir::open(tmp.path().join("gw")).unwrap();
    let mut cfg = state.config().unwrap();
    cfg.host_name = Some("mac".into()); // no relay: local only
    state.save_config(&cfg).unwrap();
    let (_, link) = pair::create(
        &state,
        "local",
        "mac",
        Scope::Full,
        true,
        Duration::from_secs(60),
    )
    .unwrap();
    let gw = Gateway::new(
        StateDir::open(tmp.path().join("gw")).unwrap(),
        server::Server::new(sock),
    )
    .unwrap();
    tokio::spawn(vk_gateway::run(gw.clone()));
    let path = vk_gateway::local::socket_path(&gw.state.dir);
    for _ in 0..100 {
        if path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    async fn open_local(
        path: &std::path::Path,
        hello: Hello,
        dev: &DeviceKey,
        hk: &[u8; 32],
        psk: Option<&[u8; 32]>,
    ) -> Client2 {
        let stream = tokio::net::UnixStream::connect(path).await.unwrap();
        let (mut ws, _) = client_async("ws://localhost/", stream).await.unwrap();
        let hb = hello.to_bytes();
        ws.send(Message::Text(String::from_utf8(hb.clone()).unwrap().into()))
            .await
            .unwrap();
        let mut i = Initiator::new(&hb, &dev.private, hk, psk).unwrap();
        ws.send(Message::Binary(i.write_first(b"").unwrap().into()))
            .await
            .unwrap();
        let Some(Ok(Message::Binary(m2))) = ws.next().await else {
            panic!("no msg2")
        };
        let (_, session) = i.read_second(&m2).unwrap();
        Client2 { ws, session }
    }
    struct Client2 {
        ws: WebSocketStream<tokio::net::UnixStream>,
        session: Session,
    }
    impl Client2 {
        async fn call(&mut self, id: u64, method: &str, params: Value) -> Value {
            let m = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
            for f in self.session.encrypt(m.to_string().as_bytes()).unwrap() {
                self.ws.send(Message::Binary(f.into())).await.unwrap();
            }
            loop {
                let Some(Ok(Message::Binary(b))) = self.ws.next().await else {
                    panic!("closed")
                };
                if let Some(m) = self.session.decrypt(&b).unwrap() {
                    let v: Value = serde_json::from_slice(&m).unwrap();
                    if v["id"] == id || v.get("method").is_some_and(|m| m == "pair.done") {
                        return v;
                    }
                }
            }
        }
    }

    let dev = DeviceKey::generate();
    let hk = link.host_key().unwrap();
    let mut c = open_local(
        &path,
        Hello::pair(&link.pid),
        &dev,
        &hk,
        Some(&link.psk_bytes().unwrap()),
    )
    .await;
    let r = c
        .call(
            1,
            "pair.claim",
            json!({"name": "desktop", "platform": "macos"}),
        )
        .await;
    assert_eq!(r["result"]["status"], "pending");
    let mut c = open_local(&path, Hello::device(), &dev, &hk, None).await;
    let d = c.call(2, "dashboard.get", json!({})).await;
    assert_eq!(d["result"]["at"], 7);
    // The server hears which devices are connected (the TUI's 📱 count), and when they leave.
    let list = report_where(&reports, |l| l.as_array().is_some_and(|a| a.len() == 1)).await;
    assert_eq!(list[0]["name"], "desktop", "{list}");
    assert_eq!(list[0]["platform"], "macos", "{list}");
    drop(c);
    report_where(&reports, |l| l.as_array().is_some_and(Vec::is_empty)).await;
}

/// Hosts need the token `t1` (a stand-in for an account host token).
struct TokenAuth;

impl vk_relay::Authorizer for TokenAuth {
    fn host_connect<'a>(
        &'a self,
        c: vk_relay::HostConnect<'a>,
    ) -> futures::future::BoxFuture<'a, vk_relay::Decision> {
        let ok = c.token == Some("t1");
        Box::pin(async move {
            if ok {
                vk_relay::Decision::Allow
            } else {
                vk_relay::Decision::deny(4401, "token_invalid")
            }
        })
    }
}

/// A relay that requires tickets (spec 16 §6.6): the pairing link's ticket admits the claim, the
/// claim result carries the device's ticket, `relay.ticket` renews it, a revoked device's ticket
/// stops working and a connect without a ticket is refused in plaintext.
#[tokio::test(flavor = "multi_thread")]
async fn tickets_admit_pairing_and_devices() {
    let tmp = tempfile::tempdir().unwrap();
    let relay = start_relay_with(true, Box::new(TokenAuth)).await;
    let sock = tmp.path().join("vibeke.sock");
    fake_server(sock.clone());
    let state = StateDir::open(tmp.path().join("gw")).unwrap();
    let mut cfg = state.config().unwrap();
    cfg.relay = Some(format!("http://{relay}"));
    cfg.relay_token = Some("t1".into());
    cfg.host_name = Some("devbox".into());
    state.save_config(&cfg).unwrap();
    let (_, link) = pair::create(
        &state,
        &format!("http://{relay}"),
        "devbox",
        Scope::View,
        true,
        Duration::from_secs(300),
    )
    .unwrap();
    let link = PairingLink::parse(&link.to_url(&format!("http://{relay}"))).unwrap();
    let gw = Gateway::new(
        StateDir::open(tmp.path().join("gw")).unwrap(),
        server::Server::new(sock),
    )
    .unwrap();
    tokio::spawn(vk_gateway::run(gw.clone()));
    let mut online = false;
    for _ in 0..100 {
        if reqwest_status(relay, &link.host).await {
            online = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(online, "the gateway registered with its static token");
    let hk = link.host_key().unwrap();
    let psk = link.psk_bytes().unwrap();
    let relay_pub = gw.keys.relay_public();
    let host = link.host.clone();

    // No ticket: refused by the relay before the host sees anything.
    let dev = DeviceKey::generate();
    let err = Client::open(relay, &host, Hello::pair(&link.pid), &dev, &hk, Some(&psk))
        .await
        .err()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&err).unwrap(),
        json!({"error": "unauthorized", "reason": "ticket_missing"})
    );

    // The link's pairing ticket admits the claim.
    let tk = link.tk.clone().expect("links carry a pairing ticket");
    let t =
        vk_e2e::relay::verify_ticket(&relay_pub, &host, &tk, vk_gateway::state::now_s()).unwrap();
    assert_eq!(t.sub, format!("pid:{}", link.pid));
    assert_eq!(t.exp, link.exp);
    let mut c = Client::open_with(
        relay,
        &host,
        Some(&tk),
        Hello::pair(&link.pid),
        &dev,
        &hk,
        Some(&psk),
    )
    .await
    .unwrap();
    let r = c
        .call("pair.claim", json!({"name": "phone", "platform": "test"}))
        .await;
    assert_eq!(r["result"]["status"], "pending");
    let done = c.recv().await;
    assert_eq!(done["method"], "pair.done", "{done}");
    let device_id = done["params"]["device_id"].as_str().unwrap().to_string();
    let ticket = done["params"]["ticket"].as_str().unwrap().to_string();
    let now = vk_gateway::state::now_s();
    let t = vk_e2e::relay::verify_ticket(&relay_pub, &host, &ticket, now).unwrap();
    assert_eq!(t.sub, format!("dev:{device_id}"));
    assert_eq!(done["params"]["ticket_exp"], t.exp);
    assert!(t.exp >= now + 29 * 24 * 3600 && t.exp <= now + 30 * 24 * 3600 + 5);

    // Reconnect with the device ticket; renew it.
    let mut c = Client::open_with(
        relay,
        &host,
        Some(&ticket),
        Hello::device(),
        &dev,
        &hk,
        None,
    )
    .await
    .unwrap();
    let r = c.call("relay.ticket", json!({})).await;
    let fresh = r["result"]["ticket"].as_str().expect("relay.ticket result");
    let ft = vk_e2e::relay::verify_ticket(&relay_pub, &host, fresh, now).unwrap();
    assert_eq!(ft.sub, format!("dev:{device_id}"));
    assert_eq!(r["result"]["exp"], ft.exp);

    // Another host's ticket is no good here.
    let other = vk_e2e::HostKeys::generate();
    let forged = vk_e2e::relay::sign_ticket(&other, &format!("dev:{device_id}"), now + 60);
    let err = Client::open_with(
        relay,
        &host,
        Some(&forged),
        Hello::device(),
        &dev,
        &hk,
        None,
    )
    .await
    .err()
    .unwrap();
    assert!(err.contains("ticket_invalid"), "{err}");

    // Revoking the device revokes its tickets at the relay (Ctrl::Revoke on the control socket).
    gw.revoke(&device_id).await.unwrap();
    let mut refused = String::new();
    for _ in 0..50 {
        match Client::open_with(relay, &host, Some(fresh), Hello::device(), &dev, &hk, None).await {
            Err(e) if e.contains("ticket_revoked") => {
                refused = e;
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    assert!(refused.contains("ticket_revoked"), "{refused}");
}

/// Hosts need an account host token from the fake control plane (`h<n>`).
struct AccountAuth;

impl vk_relay::Authorizer for AccountAuth {
    fn host_connect<'a>(
        &'a self,
        c: vk_relay::HostConnect<'a>,
    ) -> futures::future::BoxFuture<'a, vk_relay::Decision> {
        let ok = c.token.is_some_and(|t| t.starts_with('h'));
        Box::pin(async move {
            if ok {
                vk_relay::Decision::Allow
            } else {
                vk_relay::Decision::deny(4401, "account_required")
            }
        })
    }
}

/// A relay that requires accounts: without a login the gateway reports `login_required`; a
/// login stored later (as `vibeke login` does) brings it online with a host token.
#[tokio::test(flavor = "multi_thread")]
async fn account_login_brings_the_gateway_online() {
    // Only this test touches the account store; keep it off the OS keychain.
    // SAFETY: set before any gateway task reads it; no other test in this binary reads it.
    unsafe { std::env::set_var("VIBEKE_ACCOUNT_STORE", "file") };
    let tmp = tempfile::tempdir().unwrap();
    let relay = start_relay_with(true, Box::new(AccountAuth)).await;
    let accounts = vk_account::fake::FakeServer::start().await;
    let sock = tmp.path().join("vibeke.sock");
    fake_server(sock.clone());
    let dir = tmp.path().join("gw");
    let state = StateDir::open(dir.clone()).unwrap();
    let mut cfg = state.config().unwrap();
    cfg.relay = Some(format!("http://{relay}"));
    cfg.account_url = Some(accounts.url.clone());
    state.save_config(&cfg).unwrap();
    let gw = Gateway::new(state, server::Server::new(sock)).unwrap();
    gw.status.enable("connecting", cfg.relay.clone(), 0);
    tokio::spawn(vk_gateway::run(gw.clone()));
    let host = gw.keys.host_id();

    let mut st = None;
    for _ in 0..100 {
        st = vk_gateway::state::read_status(&dir);
        if st.as_ref().is_some_and(|s| s.state == "login_required") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let st = st.unwrap();
    assert_eq!(st.state, "login_required");
    assert_eq!(st.last_error.as_deref(), Some("run: vibeke login"));

    // `vibeke login` in another process: the credential lands in the store.
    let client = vk_account::Client::new(&accounts.url).unwrap();
    let cred = client.login(|_| {}).await.unwrap();
    use vk_account::CredentialStore;
    vk_account::KeychainStore::file_only(dir.join("account.json"))
        .save(&cred)
        .unwrap();
    // Watch the status file: polling the relay would trip its per-IP rate limit for the
    // gateway's own connects.
    let mut online = false;
    for _ in 0..300 {
        if vk_gateway::state::read_status(&dir).is_some_and(|s| s.state == "online") {
            online = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(online, "the gateway picked up the login");
    assert!(reqwest_status(relay, &host).await);
    assert_eq!(accounts.with(|s| s.host_tokens.clone()), ["h1"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn browser_tui_is_encrypted_scoped_and_closed_on_revocation() {
    use vk_proto::render::{ClientFrame, ServerFrame};
    let tmp = tempfile::Builder::new()
        .prefix("vkw")
        .tempdir_in("/tmp")
        .unwrap();
    let relay = start_relay().await;
    let sock = tmp.path().join("s");
    let reports = fake_server(sock.clone());
    let state = StateDir::open(tmp.path().join("gw")).unwrap();
    let mut cfg = state.config().unwrap();
    cfg.relay = Some(format!("http://{relay}"));
    state.save_config(&cfg).unwrap();
    let key = DeviceKey::generate();
    let device: vk_gateway::state::Device = serde_json::from_value(json!({
        "id":"browser", "name":"Browser", "public":vk_e2e::b64::encode(key.public()), "scope":"full", "paired_at":0
    })).unwrap();
    state.save_devices(std::slice::from_ref(&device)).unwrap();
    let gw = Gateway::new(state, server::Server::new(sock)).unwrap();
    tokio::spawn(vk_gateway::run(gw.clone()));
    let host = gw.keys.host_id();
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut delay = Duration::from_millis(50);
        while !reqwest_status(relay, &host).await {
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(1));
        }
    })
    .await
    .unwrap();
    let mut c = Client::open(
        relay,
        &host,
        Hello::device(),
        &key,
        &gw.keys.noise_public(),
        None,
    )
    .await
    .unwrap();
    let mismatch = c.call("tui.attach", json!({"protocol":0})).await;
    assert!(mismatch.get("error").is_some(), "{mismatch}");
    assert!(
        reports
            .lock()
            .unwrap()
            .iter()
            .all(|v| v["tui"] != "attached")
    );
    let attached = c
        .call("tui.attach", json!({"protocol":vk_proto::render::PROTOCOL}))
        .await;
    let stream = attached["result"]["stream"]
        .as_str()
        .expect("TUI attached")
        .to_string();
    let input = vk_proto::frame::encode(&ClientFrame::Ping { nonce: 123 }).unwrap();
    c.send(json!({"jsonrpc":"2.0","id":999,"method":"tui.send","params":{"stream":stream,"data":vk_e2e::b64::encode(&input)}})).await;
    let mut decoded = vk_proto::frame::FrameBuf::default();
    loop {
        let v = c.recv().await;
        if v["method"] != "tui.frame" {
            continue;
        }
        decoded.push(&vk_e2e::b64::decode(v["params"]["data"].as_str().unwrap()).unwrap());
        if let Some(f) = decoded.next_frame::<ServerFrame>().unwrap() {
            assert!(matches!(f, ServerFrame::Pong { nonce: 123, .. }));
            break;
        }
    }
    let input = vk_proto::frame::encode(&ClientFrame::Command {
        req: 88,
        json: json!({"method":"pane.close","params":{"pane":"p1","actor":"spoofed"}}).to_string(),
    })
    .unwrap();
    c.send(json!({"jsonrpc":"2.0","id":1000,"method":"tui.send","params":{"stream":stream,"data":vk_e2e::b64::encode(&input)}})).await;
    loop {
        let v = c.recv().await;
        if v["method"] != "tui.frame" {
            continue;
        }
        decoded.push(&vk_e2e::b64::decode(v["params"]["data"].as_str().unwrap()).unwrap());
        if let Some(ServerFrame::CommandResult { req, json }) =
            decoded.next_frame::<ServerFrame>().unwrap()
        {
            assert_eq!(req, 88);
            let command: Value = serde_json::from_str(&json).unwrap();
            assert_eq!(command["params"]["actor"], "gateway:Browser (browser)");
            break;
        }
    }
    let mut input = vk_proto::frame::encode(&ClientFrame::Command {
        req: 89,
        json: json!({"jsonrpc":"2.0", "id":89, "method":"handoff.test_slow", "params":{"actor":"spoofed"}}).to_string(),
    }).unwrap();
    input.extend(vk_proto::frame::encode(&ClientFrame::Ping { nonce: 456 }).unwrap());
    c.send(json!({"jsonrpc":"2.0","id":1001,"method":"tui.send","params":{"stream":stream,"data":vk_e2e::b64::encode(&input)}})).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let v = c.recv().await;
            if v["method"] != "tui.frame" {
                continue;
            }
            decoded.push(&vk_e2e::b64::decode(v["params"]["data"].as_str().unwrap()).unwrap());
            if let Some(ServerFrame::Pong { nonce: 456, .. }) =
                decoded.next_frame::<ServerFrame>().unwrap()
            {
                break;
            }
        }
        loop {
            if reports
                .lock()
                .unwrap()
                .iter()
                .any(|v| v["tui"] == "control-started" && v["actor"] == "gateway:Browser (browser)")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("slow command must not block render traffic");
    gw.revoke("browser").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if ["closed", "control-closed"]
                .iter()
                .all(|kind| reports.lock().unwrap().iter().any(|v| v["tui"] == *kind))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("revocation closes the server render socket");

    // Scope is checked before a server socket is opened.
    let mut limited = device;
    limited.scope = Scope::Approve;
    gw.state
        .save_devices(std::slice::from_ref(&limited))
        .unwrap();
    gw.reload_devices().unwrap();
    let mut c = Client::open(
        relay,
        &host,
        Hello::device(),
        &key,
        &gw.keys.noise_public(),
        None,
    )
    .await
    .unwrap();
    let denied = c
        .call("tui.attach", json!({"protocol":vk_proto::render::PROTOCOL}))
        .await;
    assert_eq!(denied["error"]["data"]["kind"], "forbidden");
    assert_eq!(
        reports
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v["tui"] == "attached")
            .count(),
        1
    );
    limited.scope = Scope::Full;
    gw.state
        .save_devices(std::slice::from_ref(&limited))
        .unwrap();
    gw.reload_devices().unwrap();
    reports.lock().unwrap().push(json!({"tui":"delay-attach"}));
    c.send(json!({"jsonrpc":"2.0", "id":700, "method":"tui.attach", "params":{"protocol":vk_proto::render::PROTOCOL}})).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while !reports
            .lock()
            .unwrap()
            .iter()
            .any(|v| v["tui"] == "attach-started")
        {
            tokio::task::yield_now().await;
        }
        let hello = c.call("hello", json!({})).await;
        assert_eq!(hello["result"]["scope"], "full");
    })
    .await
    .expect("a stalled attach must not block other device RPCs");
    gw.revoke("browser").await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !reports
            .lock()
            .unwrap()
            .iter()
            .any(|v| v["tui"] == "attach-cancelled")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("revocation cancels the pending attach socket promptly");
}

#[tokio::test]
async fn shared_tui_commands_use_the_existing_gateway_permissions() {
    use vk_proto::render::{ClientFrame, ServerFrame};
    let tmp = tempfile::Builder::new()
        .prefix("vks")
        .tempdir_in("/tmp")
        .unwrap();
    let relay = start_relay().await;
    let sock = tmp.path().join("s");
    let _reports = fake_server(sock.clone());
    let state = StateDir::open(tmp.path().join("gw")).unwrap();
    let mut cfg = state.config().unwrap();
    cfg.relay = Some(format!("http://{relay}"));
    state.save_config(&cfg).unwrap();
    let key = DeviceKey::generate();
    let mut device: vk_gateway::state::Device = serde_json::from_value(json!({"id":"guest","name":"Guest","public":vk_e2e::b64::encode(key.public()),"scope":"view","kind":"share","limit":{"pane":"p1"},"expires_at":u64::MAX,"paired_at":0})).unwrap();
    state.save_devices(std::slice::from_ref(&device)).unwrap();
    let gw = Gateway::new(state, server::Server::new(sock)).unwrap();
    tokio::spawn(vk_gateway::run(gw.clone()));
    let host = gw.keys.host_id();
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut delay = Duration::from_millis(50);
        while !reqwest_status(relay, &host).await {
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(1));
        }
    })
    .await
    .unwrap();
    for scope in [Scope::View, Scope::Approve, Scope::Full] {
        device.scope = scope;
        gw.state
            .save_devices(std::slice::from_ref(&device))
            .unwrap();
        gw.reload_devices().unwrap();
        let mut c = Client::open(
            relay,
            &host,
            Hello::device(),
            &key,
            &gw.keys.noise_public(),
            None,
        )
        .await
        .unwrap();
        let attach = c
            .call("tui.attach", json!({"protocol":vk_proto::render::PROTOCOL}))
            .await;
        let features = attach["result"]["features"].as_array().unwrap();
        assert!(features.contains(&json!("shared_tui")));
        assert_eq!(
            features.contains(&json!("shared_tui.approve")),
            scope >= Scope::Approve
        );
        assert_eq!(
            features.contains(&json!("shared_tui.control")),
            scope == Scope::Full
        );
        assert!(!features.contains(&json!("shared_tui.workspace")));
        let stream = attach["result"]["stream"].as_str().unwrap();
        for (req, method, params, permitted) in [
            (
                1,
                "interaction.answer",
                json!({"interaction":"i1","decision":"allow","decision_rev":3,"actor":"spoof"}),
                scope != Scope::View,
            ),
            (
                2,
                "pane.send_text",
                json!({"pane":"p2","text":"private"}),
                false,
            ),
            (3, "gateway.call", json!({"method":"devices.list"}), false),
            (
                4,
                "pane.send_text",
                json!({"pane":"p1","text":"shared"}),
                scope == Scope::Full,
            ),
        ] {
            let data = vk_proto::frame::encode(&ClientFrame::Command {
                req,
                json: json!({"method":method,"params":params}).to_string(),
            })
            .unwrap();
            c.send(json!({"jsonrpc":"2.0","id":900+req,"method":"tui.send","params":{"stream":stream,"data":vk_e2e::b64::encode(data)}})).await;
            loop {
                let message = c.recv().await;
                if message["method"] != "tui.frame" {
                    continue;
                }
                let data =
                    vk_e2e::b64::decode(message["params"]["data"].as_str().unwrap()).unwrap();
                if let ServerFrame::CommandResult { req: reply, json } =
                    vk_proto::frame::read_frame(&mut std::io::Cursor::new(data)).unwrap()
                {
                    assert_eq!(reply, req);
                    let response: Value = serde_json::from_str(&json).unwrap();
                    assert_eq!(
                        response.get("error").is_none(),
                        permitted,
                        "{scope:?} {method}: {response}"
                    );
                    if permitted && method == "interaction.answer" {
                        assert_eq!(response["result"]["echo"]["actor"], "gateway:Guest");
                    }
                    break;
                }
            }
        }
    }
}
