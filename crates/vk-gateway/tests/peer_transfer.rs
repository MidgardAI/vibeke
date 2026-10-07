//! Gateway-to-gateway handoff delivery end to end (spec 16 §15.2): two gateways on one relay,
//! paired as peers. Gateway A delivers a bundle to B with `handoff.offer` / binary
//! `handoff.write` / `handoff.commit`, loses its connection mid-transfer and resumes from
//! `handoff.status`; B hands the committed bundle to its (fake) server as
//! `handoff.incoming.add`. Then A's worker runs a whole server job: export a real repository,
//! send, commit, report `delivered`.
//!
//! Its own test binary: pairing handshakes share a per-process budget (4/min, spec 16 §6.4).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UnixListener};
use vk_gateway::api::Call;
use vk_gateway::peer_client::PeerClient;
use vk_gateway::state::{Device, PeerRecord, Scope, StateDir};
use vk_gateway::{Gateway, server};

type Handler = Arc<dyn Fn(&str, &Value) -> Value + Send + Sync>;

async fn start_relay() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let relay = vk_relay::Relay::new(
        vk_relay::Config {
            public_origins: vec![format!("http://{addr}")],
            app_dir: None,
            trust_proxy: false,
            log_ip_raw: true,
            limits: Default::default(),
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

/// A fake Vibeke server: `client.hello` and `events.subscribe` answered, everything else by
/// `handle` (`{}` by default).
fn fake_server(path: PathBuf, handle: Handler) {
    let listener = UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let handle = handle.clone();
            tokio::spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let method = req["method"].as_str().unwrap().to_string();
                    let result = match method.as_str() {
                        "client.hello" => json!({"server_version": "test", "capabilities": ["*"]}),
                        "events.subscribe" => json!({"subscription_id": "s", "at": {"seq": 1}}),
                        m => handle(m, &req["params"]),
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

/// A running gateway named `name` on `relay` with a fake server, plus the owner's own device.
async fn gateway(
    root: &Path,
    name: &str,
    relay: SocketAddr,
    handle: Handler,
) -> (Arc<Gateway>, Device) {
    let sock = root.join(format!("{name}.sock"));
    fake_server(sock.clone(), handle);
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

/// B invites, A redeems: A's record for B.
async fn pair(a: (&Arc<Gateway>, &Device), b: (&Arc<Gateway>, &Device), kind: &str) -> PeerRecord {
    let on_a = Call {
        gw: a.0,
        device: a.1,
    };
    let on_b = Call {
        gw: b.0,
        device: b.1,
    };
    let inv = match kind {
        "peer" => on_b.dispatch("peer.invite", json!({})).await.unwrap(),
        _ => on_b
            .dispatch("share.create", json!({"kind": "handoff", "ttl_s": 3600}))
            .await
            .unwrap(),
    };
    on_a.dispatch("peer.redeem", json!({"link": inv["link"]}))
        .await
        .unwrap();
    let rec = a.0.state.peers().unwrap().remove(0);
    assert_eq!(rec.owner, if kind == "peer" { "self" } else { "teammate" });
    rec
}

/// A deterministic bundle-sized blob and its sha256.
fn blob(dir: &Path, name: &str, len: usize, salt: u8) -> (Vec<u8>, String) {
    let data: Vec<u8> = (0..len)
        .map(|i| ((i * 31 + salt as usize) % 251) as u8)
        .collect();
    let p = dir.join(name);
    std::fs::write(&p, &data).unwrap();
    let (_, sha) = vk_handoff::hash_file(&p).unwrap();
    (data, sha)
}

fn manifest(repo: &str) -> Value {
    serde_json::to_value(vk_handoff::Manifest {
        v: 1,
        source_host: "alpha".into(),
        repo_name: repo.into(),
        branch: Some("feature".into()),
        head: "0123456789abcdef0123456789abcdef01234567".into(),
        bundle: "thin".into(),
        ..Default::default()
    })
    .unwrap()
}

const MIB: usize = 1024 * 1024;

#[tokio::test(flavor = "multi_thread")]
async fn offer_write_resume_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let relay = start_relay().await;
    let added: Arc<Mutex<Vec<Value>>> = Arc::default();
    let rec_b = added.clone();
    let on_b_server: Handler = Arc::new(move |m: &str, p: &Value| match m {
        "handoff.incoming.add" => {
            let mut v = rec_b.lock().unwrap();
            v.push(p.clone());
            json!({"incoming": format!("in-{}", v.len()), "state": "pending"})
        }
        _ => json!({}),
    });
    let (a, own_a) = gateway(
        tmp.path(),
        "alpha",
        relay,
        Arc::new(|_: &str, _: &Value| json!({})),
    )
    .await;
    let (b, own_b) = gateway(tmp.path(), "beta", relay, on_b_server).await;
    let rec = pair((&a, &own_a), (&b, &own_b), "peer").await;

    let (data, sha) = blob(tmp.path(), "bundle", 2 * MIB + MIB / 2, 1);
    let size = data.len() as u64;
    let offer = json!({"manifest": manifest("app"), "size": size, "sha256": sha});

    let mut conn = PeerClient::connect(&rec).await.unwrap();
    let r = conn.call("handoff.offer", offer.clone()).await.unwrap();
    let id = r["id"].as_str().unwrap().to_string();
    assert_eq!(r["received"], 0);
    assert_eq!(r["size"], size);
    // Idempotent per bundle.
    let again = conn.call("handoff.offer", offer.clone()).await.unwrap();
    assert_eq!(again["id"], id.as_str());
    assert_eq!(again["received"], 0);

    // A binary payload only goes with handoff.write.
    let e = conn
        .call_with_payload("handoff.status", json!({"id": id}), b"x")
        .await
        .unwrap_err();
    assert_eq!(e.kind, "invalid_params", "{e:?}");

    // Two binary chunks.
    for i in 0..2 {
        let r = conn
            .call_with_payload(
                "handoff.write",
                json!({"id": id, "offset": i * MIB}),
                &data[i * MIB..(i + 1) * MIB],
            )
            .await
            .unwrap();
        assert_eq!(r["received"], ((i + 1) * MIB) as u64);
    }
    // Committing early names what is missing.
    let e = conn
        .call("handoff.commit", json!({"id": id}))
        .await
        .unwrap_err();
    assert_eq!(e.kind, "conflict");
    assert_eq!(e.details["received"], (2 * MIB) as u64);
    // A gap is refused; too much data too.
    let e = conn
        .call_with_payload(
            "handoff.write",
            json!({"id": id, "offset": 3 * MIB}),
            &data[..10],
        )
        .await
        .unwrap_err();
    assert_eq!(e.kind, "conflict");

    // The connection dies mid-transfer.
    conn.close().await;

    // A new connection learns where the upload stands, from status or from a repeated offer.
    let mut conn = PeerClient::connect_with_backoff(&rec, Duration::from_secs(10))
        .await
        .unwrap();
    let st = conn
        .call("handoff.status", json!({"id": id}))
        .await
        .unwrap();
    assert_eq!(st["received"], (2 * MIB) as u64);
    assert_eq!(st["size"], size);
    assert_eq!(st["state"], "receiving");
    let again = conn.call("handoff.offer", offer.clone()).await.unwrap();
    assert_eq!(again["id"], id.as_str());
    assert_eq!(again["received"], (2 * MIB) as u64);

    // Rewriting an earlier chunk is harmless (a retry after a lost answer).
    let r = conn
        .call_with_payload(
            "handoff.write",
            json!({"id": id, "offset": MIB}),
            &data[MIB..2 * MIB],
        )
        .await
        .unwrap();
    assert_eq!(r["received"], (2 * MIB) as u64);
    // The rest as base64 (still accepted).
    let tail = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data[2 * MIB..]);
    let too_much = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        [&data[2 * MIB..], &[0u8; 3][..]].concat(),
    );
    let e = conn
        .call(
            "handoff.write",
            json!({"id": id, "offset": 2 * MIB, "data_b64": too_much}),
        )
        .await
        .unwrap_err();
    assert_eq!(e.kind, "too_large", "more than offered");
    let r = conn
        .call(
            "handoff.write",
            json!({"id": id, "offset": 2 * MIB, "data_b64": tail}),
        )
        .await
        .unwrap();
    assert_eq!(r["received"], size);

    // Commit: verified, handed to B's server, B's copy removed.
    let r = conn
        .call("handoff.commit", json!({"id": id}))
        .await
        .unwrap();
    assert_eq!(r, json!({"incoming": "in-1", "state": "pending"}));
    let calls = added.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call["sha256"], sha.as_str());
    assert_eq!(call["manifest"]["repo_name"], "app");
    assert_eq!(call["manifest"]["branch"], "feature");
    assert_eq!(call["from"]["owner"], "self");
    assert_eq!(call["from"]["host"], "alpha");
    assert_eq!(call["actor"], "gateway:alpha");
    let path = PathBuf::from(call["path"].as_str().unwrap());
    assert!(path.starts_with(b.state.dir.join("handoffs")), "{path:?}");
    assert!(!path.exists(), "the gateway's copy is gone after delivery");

    // Committing, offering or asking again answers from the record; nothing is delivered twice.
    let r = conn
        .call("handoff.commit", json!({"id": id}))
        .await
        .unwrap();
    assert_eq!(r["incoming"], "in-1");
    let st = conn
        .call("handoff.status", json!({"id": id}))
        .await
        .unwrap();
    assert_eq!(st["state"], "committed");
    assert_eq!(st["result"]["incoming"], "in-1");
    let again = conn.call("handoff.offer", offer.clone()).await.unwrap();
    assert_eq!(again["committed"], true);
    assert_eq!(again["id"], id.as_str());
    assert_eq!(added.lock().unwrap().len(), 1);

    // A corrupted upload fails its checksum and is dropped.
    let (bad, _) = blob(tmp.path(), "bad", 1000, 2);
    let r = conn
        .call(
            "handoff.offer",
            json!({"manifest": manifest("app"), "size": 1000, "sha256": "ab".repeat(32)}),
        )
        .await
        .unwrap();
    let bad_id = r["id"].as_str().unwrap().to_string();
    conn.call_with_payload("handoff.write", json!({"id": bad_id, "offset": 0}), &bad)
        .await
        .unwrap();
    let e = conn
        .call("handoff.commit", json!({"id": bad_id}))
        .await
        .unwrap_err();
    assert!(e.message.contains("checksum"), "{e:?}");
    let e = conn
        .call("handoff.status", json!({"id": bad_id}))
        .await
        .unwrap_err();
    assert_eq!(e.kind, "not_found");

    // At most two unfinished uploads per sender; discard frees a slot.
    let mut ids = vec![];
    for salt in 3..5 {
        let (_, sha) = blob(tmp.path(), &format!("b{salt}"), 100, salt);
        let r = conn
            .call(
                "handoff.offer",
                json!({"manifest": manifest("app"), "size": 100, "sha256": sha}),
            )
            .await
            .unwrap();
        ids.push(r["id"].as_str().unwrap().to_string());
    }
    let (_, sha5) = blob(tmp.path(), "b5", 100, 5);
    let third = json!({"manifest": manifest("app"), "size": 100, "sha256": sha5});
    let e = conn.call("handoff.offer", third.clone()).await.unwrap_err();
    assert_eq!(e.kind, "rate_limited");
    conn.call("handoff.discard", json!({"id": ids[0]}))
        .await
        .unwrap();
    let e = conn
        .call("handoff.status", json!({"id": ids[0]}))
        .await
        .unwrap_err();
    assert_eq!(e.kind, "not_found");
    conn.call("handoff.offer", third).await.unwrap();
    conn.close().await;

    // A teammate's host delivers as a teammate (the receiver decides; nothing starts by itself).
    let teammate = pair((&a, &own_a), (&b, &own_b), "handoff").await;
    let mut conn = PeerClient::connect(&teammate).await.unwrap();
    let (small, sha) = blob(tmp.path(), "small", 4000, 6);
    let r = conn
        .call(
            "handoff.offer",
            json!({"manifest": manifest("app"), "size": 4000, "sha256": sha}),
        )
        .await
        .unwrap();
    let tid = r["id"].as_str().unwrap().to_string();
    conn.call_with_payload("handoff.write", json!({"id": tid, "offset": 0}), &small)
        .await
        .unwrap();
    let r = conn
        .call("handoff.commit", json!({"id": tid}))
        .await
        .unwrap();
    assert_eq!(r["incoming"], "in-2");
    // Teammates may not use the owner's job API.
    for m in [
        "handoff.send",
        "handoff.jobs",
        "handoff.peers",
        "handoff.export",
    ] {
        let e = conn.call(m, json!({})).await.unwrap_err();
        assert_eq!(e.kind, "forbidden", "{m}: {e:?}");
    }
    conn.close().await;
    let last = added.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last["from"]["owner"], "teammate");
    let audit = std::fs::read_to_string(b.state.dir.join("audit.log")).unwrap();
    assert!(audit.contains("handoff.received"));
}

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?} in {}", dir.display());
}

/// A's server for the job test: one pane in a real repository and one job, whose state follows
/// the gateway's `handoff.job.update` calls.
struct JobServer {
    job: Mutex<Value>,
    updates: Mutex<Vec<Value>>,
    peers: Mutex<Option<Value>>,
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_job_is_exported_sent_and_delivered() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join("app")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("app/main.txt"), "v1\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "base"]);
    std::fs::write(repo.join("app/main.txt"), "v1\nuncommitted\n").unwrap();

    let relay = start_relay().await;
    let js = Arc::new(JobServer {
        job: Mutex::new(Value::Null),
        updates: Mutex::default(),
        peers: Mutex::default(),
    });
    let (j2, cwd) = (js.clone(), repo.join("app"));
    let on_a_server: Handler = Arc::new(move |m: &str, p: &Value| match m {
        "pane.get" => json!({"pane": {"id": "p1", "workspace": "w1"}, "cwd": cwd, "run": null}),
        "handoff.jobs" => {
            let job = j2.job.lock().unwrap().clone();
            json!({"jobs": if job.is_null() { vec![] } else { vec![job] }})
        }
        "handoff.job.update" => {
            j2.updates.lock().unwrap().push(p.clone());
            let mut job = j2.job.lock().unwrap();
            for (k, v) in p.as_object().unwrap() {
                job[k] = v.clone();
            }
            json!({"job": job.clone()})
        }
        "handoff.peers.set" => {
            *j2.peers.lock().unwrap() = Some(p["peers"].clone());
            json!({"peers": 1, "changed": true})
        }
        _ => json!({}),
    });
    let added: Arc<Mutex<Vec<Value>>> = Arc::default();
    let rec_b = added.clone();
    let on_b_server: Handler = Arc::new(move |m: &str, p: &Value| match m {
        "handoff.incoming.add" => {
            let path = PathBuf::from(p["path"].as_str().unwrap());
            let (size, sha) = vk_handoff::hash_file(&path).unwrap();
            assert_eq!(sha, p["sha256"].as_str().unwrap(), "delivered intact");
            let mut v = rec_b.lock().unwrap();
            v.push(json!({"params": p, "size": size}));
            json!({"incoming": "in-job", "state": "imported"})
        }
        _ => json!({}),
    });
    let (a, own_a) = gateway(&root, "alpha", relay, on_a_server).await;
    let (b, own_b) = gateway(&root, "beta", relay, on_b_server).await;
    let rec = pair((&a, &own_a), (&b, &own_b), "peer").await;

    // Pairing published the peer list to A's server (no keys or addresses in it).
    let published = js.peers.lock().unwrap().clone().unwrap();
    assert_eq!(published[0]["id"], rec.id.as_str());
    assert_eq!(published[0]["name"], "beta");
    assert!(published[0].get("device_key").is_none() && published[0].get("relay").is_none());

    // The server announces a queued job.
    let job = json!({"id": "job1", "pane": "p1", "peer": rec.id, "peer_name": "beta",
                     "interrupt": false, "state": "queued", "sent": 0, "total": 0,
                     "created_at": 1, "updated_at": 1});
    *js.job.lock().unwrap() = job.clone();
    a.hub.push(
        json!({"seq": 1_000_000, "type": "handoff.job", "subject": {"job": "job1"}, "data": job}),
    );

    let mut state = String::new();
    for _ in 0..600 {
        state = js.job.lock().unwrap()["state"]
            .as_str()
            .unwrap_or("")
            .to_string();
        if matches!(state.as_str(), "delivered" | "failed") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let updates = js.updates.lock().unwrap().clone();
    assert_eq!(state, "delivered", "{updates:#?}");
    let states: Vec<&str> = updates.iter().filter_map(|u| u["state"].as_str()).collect();
    assert_eq!(
        states,
        ["exporting", "sending", "delivered"],
        "{updates:#?}"
    );
    let last = updates.last().unwrap();
    assert_eq!(last["incoming_state"], "imported");
    assert_eq!(last["incoming"], "in-job");
    let total = updates[1]["total"].as_u64().unwrap();
    assert!(total > 0);
    assert!(
        updates
            .iter()
            .any(|u| u["sent"].as_u64() == Some(total) || u["state"] == "delivered")
    );

    // B's server got the bundle A exported, from one of the owner's own hosts.
    let got = added.lock().unwrap().clone();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0]["size"], total);
    let p = &got[0]["params"];
    assert_eq!(p["from"]["owner"], "self");
    assert_eq!(p["manifest"]["source_host"], "alpha");
    assert_eq!(p["manifest"]["branch"], "main");
    assert_eq!(p["manifest"]["cwd_rel"], "app");

    // Neither side keeps a copy.
    let left = |state_dir: &Path| -> Vec<String> {
        std::fs::read_dir(state_dir.join("handoffs"))
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| n.ends_with(".tar.zst"))
                    .collect()
            })
            .unwrap_or_default()
    };
    assert!(left(&a.state.dir).is_empty(), "{:?}", left(&a.state.dir));
    assert!(left(&b.state.dir).is_empty(), "{:?}", left(&b.state.dir));
}
