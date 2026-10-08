//! Host-to-host trust end to end (spec 16 §15.3–§15.4): two gateways on one relay. Gateway A
//! redeems gateway B's `peer.invite` link and a teammate handoff invitation, calls B as a peer,
//! and B lists and revokes invitations and the devices they produced.
//!
//! Its own test binary: pairing handshakes share per-process budgets (spec 16 §4.3, §6.4).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UnixListener};
use vk_gateway::api::Call;
use vk_gateway::peer_client::{PeerClient, is_refusal};
use vk_gateway::state::{Device, Scope, StateDir};
use vk_gateway::{Gateway, server};

async fn start_relay() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let relay = vk_relay::Relay::new(
        vk_relay::Config {
            public_origins: vec![format!("http://{addr}")],
            log_ip_raw: true,
            ..Default::default()
        },
        Box::new(vk_relay::Open),
    )
    .unwrap();
    let app = relay
        .router()
        .into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// Just enough of the Vibeke server for the gateway to start.
fn fake_server(path: PathBuf) {
    let listener = UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let result = match req["method"].as_str().unwrap() {
                        "client.hello" => json!({"server_version": "test", "capabilities": ["*"]}),
                        "events.subscribe" => json!({"subscription_id": "s", "at": {"seq": 1}}),
                        _ => json!({}),
                    };
                    let line = json!({"jsonrpc": "2.0", "id": req["id"], "result": result})
                        .to_string()
                        + "\n";
                    if w.write_all(line.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
}

async fn online(addr: SocketAddr, host: &str) -> bool {
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

/// A running gateway named `name` on `relay`, plus the owner's own full device there.
async fn gateway(root: &Path, name: &str, relay: SocketAddr) -> (Arc<Gateway>, Device) {
    let sock = root.join(format!("{name}.sock"));
    fake_server(sock.clone());
    let state = StateDir::open(root.join(name)).unwrap();
    let mut cfg = state.config().unwrap();
    cfg.relay = Some(format!("http://{relay}"));
    cfg.app_url = Some("http://app.example".into());
    cfg.host_name = Some(name.into());
    cfg.local_socket = false;
    state.save_config(&cfg).unwrap();
    let gw = Gateway::new(state, server::Server::new(sock)).unwrap();
    let owner = Device {
        id: format!("own-{name}"),
        name: "owner's laptop".into(),
        platform: "test".into(),
        public: format!("pub-{name}"),
        scope: Scope::Full,
        paired_at: 0,
        vapid_private: None,
        push: vec![],
        prefs: Default::default(),
        push_failures: 0,
        kind: "device".into(),
        expires_at: None,
        limit: None,
        peer: None,
    };
    gw.add_device(owner.clone()).unwrap();
    tokio::spawn(vk_gateway::run(gw.clone()));
    let host = gw.keys.host_id();
    for _ in 0..200 {
        if online(relay, &host).await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(online(relay, &host).await, "{name} never came online");
    (gw, owner)
}

#[tokio::test(flavor = "multi_thread")]
async fn peers_pair_call_list_and_revoke() {
    let tmp = tempfile::tempdir().unwrap();
    let relay = start_relay().await;
    let (a, own_a) = gateway(tmp.path(), "alpha", relay).await;
    let (b, own_b) = gateway(tmp.path(), "beta", relay).await;
    let on_a = Call {
        gw: &a,
        device: &own_a,
    };
    let on_b = Call {
        gw: &b,
        device: &own_b,
    };

    // B invites A as one of the owner's own hosts.
    let inv = on_b.dispatch("peer.invite", json!({})).await.unwrap();
    let link = inv["link"].as_str().unwrap().to_string();
    let pid = inv["pid"].as_str().unwrap().to_string();
    let list = on_b.dispatch("share.list", json!({})).await.unwrap();
    let pending = list["invitations"].as_array().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["id"], pid.as_str());
    assert_eq!(pending[0]["kind"], "peer");
    assert!(pending[0]["device_expires_at"].is_null());

    // A redeems it over the relay.
    let r = on_a
        .dispatch("peer.redeem", json!({"link": link}))
        .await
        .unwrap();
    assert_eq!(r["peer"]["owner"], "self");
    assert_eq!(r["peer"]["name"], "beta");
    assert!(r["peer"]["expires_at"].is_null());
    let peers = on_a.dispatch("peer.list", json!({})).await.unwrap();
    assert_eq!(peers["peers"].as_array().unwrap().len(), 1);
    assert!(peers["peers"][0].get("device_key").is_none(), "{peers}");
    let mode = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(a.state.dir.join("peers.json"))
            .unwrap()
            .mode()
            & 0o777
    };
    assert_eq!(mode, 0o600);

    // B now has a peer device for A: own host, no expiry, introduced by name.
    let list = on_b.dispatch("share.list", json!({})).await.unwrap();
    assert!(list["invitations"].as_array().unwrap().is_empty(), "{list}");
    let devs = list["devices"].as_array().unwrap();
    assert_eq!(devs.len(), 1, "{list}");
    assert_eq!(devs[0]["kind"], "peer");
    assert_eq!(devs[0]["owner"], "self");
    assert_eq!(devs[0]["sender"]["host_name"], "alpha");
    assert!(devs[0]["expires_at"].is_null());
    let self_peer_id = devs[0]["id"].as_str().unwrap().to_string();

    // A calls B as a peer: ping and hello work, everything else is refused by kind.
    let rec = a.state.peers().unwrap().remove(0);
    let mut conn = PeerClient::connect(&rec).await.unwrap();
    assert_eq!(conn.info["host_name"], "beta");
    conn.call("ping", json!({})).await.unwrap();
    let h = conn.call("hello", json!({})).await.unwrap();
    assert_eq!(h["kind"], "peer");
    for m in [
        "devices.list",
        "dashboard.get",
        "peer.invite",
        "share.list",
        "handoff.accept",
    ] {
        let e = conn.call(m, json!({})).await.unwrap_err();
        assert_eq!(e.kind, "forbidden", "{m}: {e:?}");
    }
    conn.close().await;

    // A teammate's handoff invitation redeemed by a host makes an expiring teammate peer.
    let inv = on_b
        .dispatch("share.create", json!({"kind": "handoff", "ttl_s": 3600}))
        .await
        .unwrap();
    let r = on_a
        .dispatch(
            "peer.redeem",
            json!({"link": inv["link"], "share_user": false}),
        )
        .await
        .unwrap();
    assert_eq!(r["peer"]["owner"], "teammate");
    assert!(r["peer"]["expires_at"].as_u64().is_some());
    let teammate = a.state.peers().unwrap();
    assert_eq!(teammate.len(), 1, "a re-pairing replaces the record");
    let teammate = teammate[0].clone();
    let list = on_b.dispatch("share.list", json!({})).await.unwrap();
    let tm = list["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["owner"] == "teammate")
        .cloned()
        .unwrap();
    assert_eq!(tm["kind"], "peer");
    assert!(tm["expires_at"].as_u64().is_some());
    let mut conn = PeerClient::connect(&teammate).await.unwrap();
    conn.call("ping", json!({})).await.unwrap();
    conn.close().await;

    // A cancelled invitation can't be redeemed.
    let inv = on_b.dispatch("peer.invite", json!({})).await.unwrap();
    let r = on_b
        .dispatch("share.revoke", json!({"id": inv["pid"]}))
        .await
        .unwrap();
    assert_eq!(r["cancelled"], "invitation");
    assert!(
        on_a.dispatch("peer.redeem", json!({"link": inv["link"]}))
            .await
            .is_err()
    );

    // Revoking the teammate device locks A out (a refusal, not retried).
    let r = on_b
        .dispatch("share.revoke", json!({"id": tm["id"]}))
        .await
        .unwrap();
    assert_eq!(r["cancelled"], "device");
    let e = PeerClient::connect(&teammate).await.err().unwrap();
    assert!(is_refusal(&e), "{e:#}");
    let e = PeerClient::connect_with_backoff(&teammate, Duration::from_secs(5))
        .await
        .err()
        .unwrap();
    assert!(is_refusal(&e), "{e:#}");
    let audit = std::fs::read_to_string(b.state.dir.join("audit.log")).unwrap();
    assert!(audit.contains("invitation.cancelled") && audit.contains("device.revoked"));

    // share.revoke never touches the owner's own devices, and unknown ids are not found.
    for id in [own_b.id.as_str(), "nope"] {
        let e = on_b
            .dispatch("share.revoke", json!({"id": id}))
            .await
            .unwrap_err();
        assert_eq!(e.kind, "not_found", "{id}");
    }
    assert!(b.device(&self_peer_id).is_some());

    // peer.remove by name.
    on_a.dispatch("peer.remove", json!({"id": "beta"}))
        .await
        .unwrap();
    assert!(a.state.peers().unwrap().is_empty());
}
