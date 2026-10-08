//! Handoff end to end (spec 16 §15.2): the source gateway's worker exports a real repository and
//! sends it to a peer host, whose gateway delivers it to its (fake) server as an incoming handoff;
//! the server's copy imports into another clone of the same origin and the (fake) Claude session
//! would resume there. Also: a teammate's host only delivers (nothing is placed or started), and
//! an app cannot claim a handoff invitation.
//!
//! Its own test binary: pairing handshakes share a per-process budget (4/min, spec 16 §6.4).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UnixListener};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use vk_e2e::{DeviceKey, Hello, Initiator, PairingLink, Session};
use vk_gateway::api::Call;
use vk_gateway::peer_client::PeerClient;
use vk_gateway::state::{Device, PairingStatus, PeerRecord, Scope, StateDir};
use vk_gateway::{Gateway, server};

type Handler = Arc<dyn Fn(&str, &Value) -> Value + Send + Sync>;
type Calls = Arc<Mutex<Vec<(String, Value)>>>;

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
/// `handle` (`{}` by default); every call is recorded.
fn fake_server(path: PathBuf, handle: Handler, calls: Calls) {
    let listener = UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (handle, calls) = (handle.clone(), calls.clone());
            tokio::spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let method = req["method"].as_str().unwrap().to_string();
                    calls
                        .lock()
                        .unwrap()
                        .push((method.clone(), req["params"].clone()));
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
) -> (Arc<Gateway>, Device, Calls) {
    let calls: Calls = Arc::default();
    let sock = root.join(format!("{name}.sock"));
    fake_server(sock.clone(), handle, calls.clone());
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
    (gw, owner, calls)
}

/// B invites (a `peer` invitation for the owner's own hosts, or a teammate's `handoff` one), A
/// redeems: A's record for B.
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

fn calls_of(calls: &Calls, method: &str) -> Vec<Value> {
    calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, _)| m == method)
        .map(|(_, p)| p.clone())
        .collect()
}

/// What the source host's server does for a job: one pane in `cwd` running `run`, one job whose
/// state follows the gateway's `handoff.job.update` calls.
struct JobServer {
    job: Mutex<Value>,
    updates: Mutex<Vec<Value>>,
}

impl JobServer {
    fn new() -> Arc<Self> {
        Arc::new(JobServer {
            job: Mutex::new(Value::Null),
            updates: Mutex::default(),
        })
    }

    fn handler(self: &Arc<Self>, cwd: PathBuf, run: Value) -> Handler {
        let js = self.clone();
        Arc::new(move |m: &str, p: &Value| match m {
            "pane.get" => json!({"pane": {"id": "p1", "workspace": "w1"}, "cwd": cwd, "run": run}),
            "handoff.jobs" => {
                let job = js.job.lock().unwrap().clone();
                json!({"jobs": if job.is_null() { vec![] } else { vec![job] }})
            }
            "handoff.job.update" => {
                js.updates.lock().unwrap().push(p.clone());
                let mut job = js.job.lock().unwrap();
                for (k, v) in p.as_object().unwrap() {
                    job[k] = v.clone();
                }
                json!({"job": job.clone()})
            }
            "handoff.peers.set" => json!({"peers": 1, "changed": true}),
            _ => json!({}),
        })
    }

    /// The server announces a queued job to the gateway's worker and waits for its outcome.
    async fn run(&self, a: &Arc<Gateway>, peer: &PeerRecord) -> Vec<Value> {
        let job = json!({"id": "job1", "pane": "p1", "peer": peer.id, "peer_name": peer.name,
                         "interrupt": false, "state": "queued", "sent": 0, "total": 0,
                         "created_at": 1, "updated_at": 1});
        *self.job.lock().unwrap() = job.clone();
        a.hub.push(
            json!({"seq": 1_000_000, "type": "handoff.job", "subject": {"job": "job1"},
                          "data": job}),
        );
        let mut state = String::new();
        for _ in 0..600 {
            state = self.job.lock().unwrap()["state"]
                .as_str()
                .unwrap_or("")
                .to_string();
            if matches!(state.as_str(), "delivered" | "failed") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let updates = self.updates.lock().unwrap().clone();
        assert_eq!(state, "delivered", "{updates:#?}");
        updates
    }
}

/// The receiving host's server: `handoff.incoming.add` keeps a copy of the bundle (as the real
/// server does) at `<root>/received-<n>.tar.zst` and leaves the handoff pending.
fn receiving_server(root: PathBuf) -> Handler {
    let n = Mutex::new(0usize);
    Arc::new(move |m: &str, p: &Value| match m {
        "handoff.incoming.add" => {
            let mut n = n.lock().unwrap();
            *n += 1;
            let from = PathBuf::from(p["path"].as_str().unwrap());
            std::fs::copy(&from, root.join(format!("received-{n}.tar.zst"))).unwrap();
            json!({"incoming": {"id": format!("in{n}"), "state": "pending", "result": null}})
        }
        _ => json!({}),
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn export_send_deliver_import_resume() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().canonicalize().unwrap();
    let origin = root.join("origin.git");
    std::fs::create_dir(&origin).unwrap();
    git(&origin, &["init", "-q", "--bare", "-b", "main"]);

    // Source clone: a pushed main, a local feature branch with one unpushed commit, uncommitted
    // changes, an untracked file and a secret.
    let src = root.join("src");
    git(&root, &["clone", "-q", origin.to_str().unwrap(), "src"]);
    std::fs::create_dir_all(src.join("app")).unwrap();
    std::fs::write(src.join("app/main.txt"), "v1\n").unwrap();
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-qm", "base"]);
    git(&src, &["push", "-q", "origin", "HEAD:main"]);
    git(&src, &["checkout", "-qb", "feature"]);
    std::fs::write(src.join("app/main.txt"), "v1\nfeature\n").unwrap();
    git(&src, &["commit", "-qam", "feature work"]);
    std::fs::write(src.join("app/main.txt"), "v1\nfeature\nuncommitted\n").unwrap();
    std::fs::write(src.join("app/notes.md"), "untracked notes\n").unwrap();
    std::fs::write(src.join(".env"), "TOKEN=hunter2\n").unwrap();

    // Destination clone (only knows main).
    let dst = root.join("dst");
    git(&root, &["clone", "-q", origin.to_str().unwrap(), "dst"]);

    // A Claude transcript mentioning the source path and a secret.
    let claude_src = root.join("claude-src/projects/-src-app");
    std::fs::create_dir_all(&claude_src).unwrap();
    let transcript = claude_src.join("sess1.jsonl");
    std::fs::write(
        &transcript,
        format!(
            "{}\n{}\n",
            json!({"type": "user", "cwd": src.join("app"), "message": "hi"}),
            json!({"type": "assistant", "message": "export OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz0123456789"})
        ),
    )
    .unwrap();
    let claude_dst = root.join("claude-dst");
    // SAFETY: set before the gateways run; only this test reads it.
    unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &claude_dst) };

    let relay = start_relay().await;
    let js = JobServer::new();
    let run = json!({"id": "r1", "harness": "claude", "execution": {"value": "Idle"},
                     "harness_session_id": "sess1", "transcript_path": transcript,
                     "resume_argv": ["claude", "--resume", "sess1"], "last_message": "done"});
    let (a, own_a, _) = gateway(&root, "alpha", relay, js.handler(src.join("app"), run)).await;
    let (b, own_b, b_calls) = gateway(&root, "beta", relay, receiving_server(root.clone())).await;
    let rec = pair((&a, &own_a), (&b, &own_b), "peer").await;

    let updates = js.run(&a, &rec).await;
    let states: Vec<&str> = updates.iter().filter_map(|u| u["state"].as_str()).collect();
    assert_eq!(
        states,
        ["exporting", "sending", "delivered"],
        "{updates:#?}"
    );
    assert_eq!(updates.last().unwrap()["incoming_state"], "pending");

    // The bundle reached B's server as a pending incoming handoff from one of the owner's hosts.
    let adds = calls_of(&b_calls, "handoff.incoming.add");
    assert_eq!(adds.len(), 1);
    let add = &adds[0];
    let m = &add["manifest"];
    assert_eq!(m["branch"], "feature");
    assert_eq!(m["bundle"], "thin");
    assert_eq!(m["cwd_rel"], "app");
    assert_eq!(m["source_job"], "job1");
    assert_eq!(m["untracked"], json!(["app/notes.md"]));
    assert!(
        m["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["path"] == ".env" && s["reason"] == "secret")
    );
    assert_eq!(m["redactions"], 1);
    assert_eq!(add["from"]["owner"], "self");
    assert_eq!(add["from"]["host"], "alpha");
    let peer_dev = b.devices().into_iter().find(|d| d.kind == "peer").unwrap();
    assert_eq!(
        add["from"]["device"],
        peer_dev.id.as_str(),
        "quota keyed by the authenticated device"
    );
    assert_eq!(add["actor"], "gateway:alpha");
    assert!(
        !Path::new(add["path"].as_str().unwrap()).exists(),
        "the gateway drops its copy once the server has the bundle"
    );
    assert!(calls_of(&b_calls, "handoff.accept").is_empty());
    assert!(
        calls_of(&b_calls, "agent.start").is_empty(),
        "the gateway never imports or starts agents"
    );

    // The server's copy imports into the destination clone (what `handoff.accept` runs).
    let received = root.join("received-1.tar.zst");
    let work = root.join("work");
    std::fs::create_dir(&work).unwrap();
    let packed = vk_handoff::unpack(&received, &work).unwrap();
    let reviewed: vk_handoff::Manifest = serde_json::from_value(m.clone()).unwrap();
    vk_handoff::verify(&packed, &reviewed).unwrap();
    let imported = vk_handoff::import(&work, &packed, &dst, None, None)
        .await
        .unwrap();
    let wt = imported.worktree.clone();
    assert_eq!(imported.branch, "handoff/feature");
    assert!(imported.resumed);
    assert_eq!(
        imported.resume_args,
        Some(vec!["--resume".to_string(), "sess1".to_string()])
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("app/main.txt")).unwrap(),
        "v1\nfeature\nuncommitted\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("app/notes.md")).unwrap(),
        "untracked notes\n"
    );
    assert!(!wt.join(".env").exists(), "secrets never travel");

    // Transcript installed for the new cwd, paths rewritten, secret redacted.
    let new_cwd = wt.join("app");
    let installed = claude_dst
        .join("projects")
        .join(vk_gateway::handoff::claude_project_dir(&new_cwd))
        .join("sess1.jsonl");
    let text = std::fs::read_to_string(&installed).unwrap();
    assert!(text.contains(&new_cwd.display().to_string()));
    assert!(!text.contains(&src.join("app").display().to_string()));
    assert!(!text.contains("sk-abcdefghijklmnopqrstuvwxyz"));

    // The source is untouched.
    assert_eq!(
        std::fs::read_to_string(src.join("app/main.txt")).unwrap(),
        "v1\nfeature\nuncommitted\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_teammates_host_only_delivers() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().canonicalize().unwrap();
    let src = root.join("src");
    std::fs::create_dir_all(src.join("app")).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    std::fs::write(src.join("app/a.txt"), "a\n").unwrap();
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-qm", "base"]);

    let relay = start_relay().await;
    let js = JobServer::new();
    let (a, own_a, _) = gateway(
        &root,
        "alpha",
        relay,
        js.handler(src.join("app"), Value::Null),
    )
    .await;
    let (b, own_b, b_calls) = gateway(&root, "beta", relay, receiving_server(root.clone())).await;
    let rec = pair((&a, &own_a), (&b, &own_b), "handoff").await;

    // Delivered as a teammate's handoff: it waits as pending on B; nothing is placed or started.
    js.run(&a, &rec).await;
    let adds = calls_of(&b_calls, "handoff.incoming.add");
    assert_eq!(adds.len(), 1);
    assert_eq!(adds[0]["from"]["owner"], "teammate");
    assert_eq!(adds[0]["from"]["host"], "alpha");
    let peer_dev = b.devices().into_iter().find(|d| d.kind == "peer").unwrap();
    assert_eq!(adds[0]["from"]["device"], peer_dev.id.as_str());
    assert_eq!(adds[0]["actor"], "gateway:alpha");
    assert!(calls_of(&b_calls, "handoff.accept").is_empty());
    assert!(calls_of(&b_calls, "agent.start").is_empty());

    // The teammate's host reaches nothing else on B: not the receiver's decisions, not the owner's
    // job API, not the retired courier methods.
    let mut conn = PeerClient::connect(&rec).await.unwrap();
    for m in [
        "handoff.accept",
        "handoff.incoming.list",
        "handoff.send",
        "handoff.jobs",
        "handoff.peers",
        "handoff.export",
        "handoff.begin",
        "handoff.finish",
    ] {
        let e = conn.call(m, json!({})).await.unwrap_err();
        assert_eq!(e.kind, "forbidden", "{m}: {e:?}");
    }
    conn.close().await;
    assert!(calls_of(&b_calls, "handoff.accept").is_empty());
}

struct Client {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    session: Session,
}

impl Client {
    async fn open(relay: SocketAddr, link: &PairingLink) -> Client {
        let (mut ws, _) = connect_async(format!("ws://{relay}/v1/connect?host={}", link.host))
            .await
            .unwrap();
        let hb = Hello::pair(&link.pid).to_bytes();
        ws.send(Message::Text(String::from_utf8(hb.clone()).unwrap().into()))
            .await
            .unwrap();
        let dev = DeviceKey::generate();
        let (hk, psk) = (link.host_key().unwrap(), link.psk_bytes().unwrap());
        let mut i = Initiator::new(&hb, &dev.private, &hk, Some(&psk)).unwrap();
        ws.send(Message::Binary(i.write_first(b"").unwrap().into()))
            .await
            .unwrap();
        let Some(Ok(Message::Binary(m2))) = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .unwrap()
        else {
            panic!("no handshake answer");
        };
        let (_, session) = i.read_second(&m2).unwrap();
        Client { ws, session }
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        for f in self.session.encrypt(req.to_string().as_bytes()).unwrap() {
            self.ws.send(Message::Binary(f.into())).await.unwrap();
        }
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
}

#[tokio::test(flavor = "multi_thread")]
async fn an_app_cannot_claim_a_handoff_invitation() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().canonicalize().unwrap();
    let relay = start_relay().await;
    let (b, own_b, _) = gateway(
        &root,
        "beta",
        relay,
        Arc::new(|_: &str, _: &Value| json!({})),
    )
    .await;
    let on_b = Call {
        gw: &b,
        device: &own_b,
    };
    let inv = on_b
        .dispatch("share.create", json!({"kind": "handoff", "ttl_s": 3600}))
        .await
        .unwrap();
    let link = PairingLink::parse(inv["link"].as_str().unwrap()).unwrap();

    // An app (anything but a host introducing itself) is told to open it on one of its hosts.
    let mut c = Client::open(relay, &link).await;
    let r = c
        .call("pair.claim", json!({"name": "phone", "platform": "ios"}))
        .await;
    assert_eq!(r["error"]["data"]["kind"], "forbidden", "{r}");
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("one of your hosts"),
        "{r}"
    );
    assert_eq!(
        b.state.pairing(&link.pid).unwrap().unwrap().status,
        PairingStatus::Pending,
        "the invitation is not burned"
    );
    assert_eq!(b.devices().len(), 1, "no device was added");
}
