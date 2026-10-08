//! Preview fabric, in-process: proxy mode for a local preview and for a remote preview over a
//! fake bridge (an in-process mux whose acceptor plays the remote machine: a JSON-RPC
//! `socket` answering `preview.get`, and `tcp:localhost:<remote port>` channels mapped to a
//! loopback server here), mirror enable/conflict/peer/unmirror, and task `[previews]` with
//! leased ports. No browser is launched (`VIBEKE_NO_OPEN=1`), no remote machine is used.

use super::*;
use crate::ServerOpts;
use crate::paths::Paths;
use std::collections::HashMap;
use std::sync::Once;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use vk_remote::{Link, Mux};

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let base = std::env::temp_dir().join(format!("vk-fabric-tests-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        // SAFETY: set once, before any server in this test binary reads them.
        unsafe {
            // Never open the user's browser from a test.
            std::env::set_var("VIBEKE_NO_OPEN", "1");
            if std::env::var_os("VIBEKE_RUNTIME_DIR").is_none() {
                std::env::set_var("VIBEKE_RUNTIME_DIR", base.join("run"));
            }
            if std::env::var_os("VIBEKE_STATE_DIR").is_none() {
                std::env::set_var("VIBEKE_STATE_DIR", base.join("state"));
            }
            if std::env::var_os("VIBEKE_CONFIG").is_none() {
                std::env::set_var("VIBEKE_CONFIG", base.join("config.toml"));
            }
        }
    });
}

struct Env {
    _dir: tempfile::TempDir,
    server: Arc<Server>,
}

fn ctx_full() -> Ctx {
    Ctx {
        client_id: "c-user".into(),
        kind: "cli".into(),
        pane_scope: None,
        remote: false,
    }
}

fn ctx_pane(p: &str) -> Ctx {
    Ctx {
        client_id: format!("c-{p}"),
        kind: "cli".into(),
        pane_scope: Some(p.into()),
        remote: false,
    }
}

impl Env {
    fn new() -> Env {
        init_env();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let paths = Paths {
            session: "t".into(),
            runtime: root.join("run"),
            state: root.join("state"),
        };
        let opts = ServerOpts {
            session: "t".into(),
            machine: "testbox".into(),
            bin: "/bin/false".into(),
            hold_args: vec![],
            default_shell: None,
            env: vec![],
            shims: false,
            gateway: None,
        };
        let server = Server::new(paths, opts).unwrap();
        // Never the machine-wide default port (a user's real proxy may hold it; parallel
        // tests would collide on it).
        server.previews.set_proxy_port(0);
        let e = Env { _dir: dir, server };
        e.put_pane("pane-a");
        e
    }

    fn put_pane(&self, id: &str) {
        let p = Pane {
            id: id.into(),
            handle: id.into(),
            tab: "tab-a".into(),
            workspace: "ws-a".into(),
            title: None,
            auto_title: String::new(),
            cwd: None,
            cols: 80,
            rows: 24,
            child_pid: None,
            fg_cmdline: vec![],
            exited: false,
            exit_code: None,
            unread: false,
            marked_unread: false,
            pinned: false,
            created_by: "user".into(),
            recovered: None,
            isolation: Default::default(),
            browser: None,
        };
        let mut c = self.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.pane(p);
        self.server.commit(&mut c, tx).unwrap();
    }

    async fn call(&self, ctx: &Ctx, method: &str, p: Value) -> R {
        crate::preview::api(&self.server, ctx, method, &p)
            .await
            .expect("preview method")
    }

    fn events(&self, kind: &str) -> Vec<Value> {
        self.server.with_core(|c| {
            c.store
                .events_after(0, 10_000, &[kind.to_string()])
                .unwrap()
                .into_iter()
                .map(|e| json!({"subject": e.subject, "data": e.data}))
                .collect()
        })
    }
}

/// A loopback HTTP server: records request heads, answers `<p>{tag} {path}</p>` and closes.
async fn app(tag: &'static str) -> (u16, Arc<std::sync::Mutex<Vec<String>>>) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let s2 = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            let seen = s2.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut b = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut b).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&b[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf).into_owned();
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                seen.lock().unwrap().push(head);
                let body = format!("<p>{tag} {path}</p>");
                let _ = s
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            });
        }
    });
    (port, seen)
}

async fn http(port: u16, host: &str, path: &str, cookie: Option<&str>) -> (u16, String, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let ck = cookie
        .map(|c| format!("Cookie: {}={c}\r\n", vk_preview::proxy::COOKIE))
        .unwrap_or_default();
    s.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{ck}Connection: close\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut out)).await;
    let t = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = t.split_once("\r\n\r\n").unwrap_or((&t, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|x| x.parse().ok())
        .unwrap_or(0);
    (status, head.to_string(), body.to_string())
}

/// Follow a tokenized `open_url`: → (host:port, session cookie).
async fn login(open_url: &str) -> (u16, String, String) {
    let rest = open_url.strip_prefix("http://").unwrap();
    let (authority, path) = rest.split_at(rest.find('/').unwrap());
    let port: u16 = authority.rsplit_once(':').unwrap().1.parse().unwrap();
    let (st, head, _) = http(port, authority, path, None).await;
    assert_eq!(st, 303, "{head}");
    let cookie = head
        .lines()
        .find_map(|l| l.strip_prefix(&format!("set-cookie: {}=", vk_preview::proxy::COOKIE)))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    (port, authority.to_string(), cookie)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The fake remote machine "fakebox": `previews` maps handle → remote port; `ports` maps a
/// remote port → where it really listens here.
fn fake_link(previews: HashMap<String, u16>, ports: HashMap<u16, u16>) -> Link {
    fake_link_shared(
        Arc::new(std::sync::Mutex::new(previews)),
        Arc::new(std::sync::Mutex::new(ports)),
    )
}

type Shared<K, V> = Arc<std::sync::Mutex<HashMap<K, V>>>;

/// [`fake_link`] whose remote previews and port map the test can change afterwards (a
/// preview forgotten or moved on the remote, a port reused by another service).
fn fake_link_shared(previews: Shared<String, u16>, ports: Shared<u16, u16>) -> Link {
    let connector: vk_remote::link::Connector = Arc::new(move || {
        let previews = previews.clone();
        let ports = ports.clone();
        Box::pin(async move {
            let (a, b) = tokio::io::duplex(1 << 16);
            let (ar, aw) = tokio::io::split(a);
            let (br, bw) = tokio::io::split(b);
            let acceptor: vk_remote::mux::Acceptor = Arc::new(move |kind: String| {
                let previews = previews.clone();
                let ports = ports.clone();
                Box::pin(async move {
                    if kind == "socket" {
                        let (x, y) = tokio::io::duplex(1 << 16);
                        tokio::spawn(fake_rpc(y, previews));
                        return Ok(Box::new(x) as Box<dyn vk_remote::mux::Stream>);
                    }
                    let target = kind
                        .strip_prefix("tcp:")
                        .ok_or_else(|| anyhow::anyhow!("refused"))?;
                    let (h, p) =
                        vk_remote::split_host_port(target).ok_or_else(|| anyhow::anyhow!("bad"))?;
                    anyhow::ensure!(vk_remote::is_loopback_host(&h), "not loopback");
                    let real = ports
                        .lock()
                        .unwrap()
                        .get(&p)
                        .copied()
                        .ok_or_else(|| anyhow::anyhow!("connection refused"))?;
                    let s = TcpStream::connect(("127.0.0.1", real)).await?;
                    Ok(Box::new(s) as Box<dyn vk_remote::mux::Stream>)
                })
            });
            let bridge = Mux::start(br, bw, "bridge", Some(acceptor));
            tokio::spawn(async move { bridge.closed().await });
            Ok((Mux::start(ar, aw, "client", None), None))
        })
    });
    Link::with_connector("fakebox", connector)
}

async fn fake_rpc(s: tokio::io::DuplexStream, previews: Shared<String, u16>) {
    let (rd, mut wr) = tokio::io::split(s);
    let mut rd = tokio::io::BufReader::new(rd);
    let mut line = String::new();
    if rd.read_line(&mut line).await.is_err() {
        return;
    }
    let req: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
    // By handle or by id (`01REMOTE<handle>`).
    let target = req["params"]["preview"].as_str().unwrap_or("");
    let handle = target
        .strip_prefix("01REMOTE")
        .unwrap_or(target)
        .to_string();
    let port = previews.lock().unwrap().get(&handle).copied();
    let resp = match port.as_ref() {
        Some(port) => {
            let pv = Preview {
                id: format!("01REMOTE{handle}"),
                handle: handle.clone(),
                machine: "fakebox".into(),
                pane: None,
                task: None,
                port: *port,
                path: "/app".into(),
                label: Some("web".into()),
                url: format!("http://localhost:{port}/app"),
                scheme: "http".into(),
                status: PreviewStatus::Up,
                source: PreviewSource::Declared,
                pid: None,
                first_seen_ms: 0,
                last_seen_ms: 0,
            };
            json!({"jsonrpc": "2.0", "id": req["id"], "result": {"preview": pv}})
        }
        None => {
            json!({"jsonrpc": "2.0", "id": req["id"], "error": crate::api::not_found("preview", &handle)})
        }
    };
    let _ = wr.write_all(format!("{resp}\n").as_bytes()).await;
}

#[tokio::test]
async fn proxy_mode_for_a_local_preview() {
    let e = Env::new();
    let (port, seen) = app("local-app").await;
    let full = ctx_full();
    let d = e
        .call(
            &full,
            "preview.declare",
            json!({"port": port, "path": "/dash", "label": "web"}),
        )
        .await
        .unwrap();
    let handle = d["preview"]["handle"].as_str().unwrap().to_string();
    let r = e
        .call(
            &full,
            "preview.open",
            json!({"preview": handle, "mode": "proxy", "no_open": true}),
        )
        .await
        .unwrap();
    assert_eq!(r["opened_in"], "proxy", "{r}");
    assert_eq!(r["opened"], false);
    assert_eq!(r["machine"], "local");
    let host = r["host"].as_str().unwrap().to_string();
    assert!(
        host.starts_with(&format!("{handle}-")) && host.ends_with(".vibeke.localhost"),
        "{host}"
    );
    let open_url = r["open_url"].as_str().unwrap().to_string();
    assert!(open_url.contains("/dash?vk_token="), "{open_url}");
    let plain = r["url"].as_str().unwrap();
    assert!(
        !plain.contains("vk_token") && plain.ends_with("/dash"),
        "{plain}"
    );
    // The browser flow: token → cookie → the app.
    let (pport, authority, cookie) = login(&open_url).await;
    let (st, _, body) = http(pport, &authority, "/dash", Some(&cookie)).await;
    assert_eq!(st, 200);
    assert_eq!(body, "<p>local-app /dash</p>");
    let up = seen
        .lock()
        .unwrap()
        .last()
        .cloned()
        .unwrap()
        .to_ascii_lowercase();
    assert!(
        up.contains(&format!("host: localhost:{port}")) && !up.contains("vk_"),
        "{up}"
    );
    // No credential → 401; nothing new reached the app.
    let n = seen.lock().unwrap().len();
    assert_eq!(http(pport, &authority, "/", None).await.0, 401);
    assert_eq!(seen.lock().unwrap().len(), n);
    // The hostname is `<handle>-<26 random base32 chars>`.
    let label = host.strip_suffix(".vibeke.localhost").unwrap();
    assert_eq!(label.rsplit_once('-').unwrap().1.len(), 26, "{host}");
    // `preview.url` reports the origin to a full-scope client, without a credential.
    let u = e
        .call(&full, "preview.url", json!({"preview": handle}))
        .await
        .unwrap();
    assert_eq!(u["proxy_url"], json!(plain));
    // Another pane never learns the hostname: not from preview.url, preview.status or events.
    let other = ctx_pane("pane-b");
    let u = e
        .call(&other, "preview.url", json!({"preview": handle}))
        .await
        .unwrap();
    assert_eq!(u["proxy_url"], Value::Null, "{u}");
    let st = e.call(&other, "preview.status", json!({})).await.unwrap();
    assert_eq!(st["proxy"]["routes"], json!([]), "{st}");
    let ev = e.events("preview.opened");
    assert!(!ev.is_empty());
    let evs = json!(ev).to_string();
    assert!(!evs.contains("vk_token"), "{ev:?}");
    assert!(
        !evs.contains(&host) && !evs.contains("vibeke.localhost"),
        "{ev:?}"
    );
    // Status (full scope): the proxy and its route; no mirror unless explicitly enabled.
    let st = e.call(&full, "preview.status", json!({})).await.unwrap();
    assert_eq!(st["proxy"]["port"], json!(pport));
    assert_eq!(st["proxy"]["routes"][0]["host"], json!(host));
    assert_eq!(st["mirrors"], json!([]));
    // Re-opening rotates the origin: a new unguessable host, the old one and its session gone.
    let r2 = e
        .call(
            &full,
            "preview.open",
            json!({"preview": handle, "proxy": true, "no_open": true}),
        )
        .await
        .unwrap();
    let host2 = r2["host"].as_str().unwrap().to_string();
    assert_ne!(host2, host);
    assert_eq!(http(pport, &authority, "/", Some(&cookie)).await.0, 421);
    assert_eq!(r2["session_ttl_s"], json!(8 * 3600));
    // A pane-scoped agent can open it for the human but never receives the credential; it
    // learns its own (fresh) hostname and nobody else's.
    let pa = ctx_pane("pane-a");
    let rp = e
        .call(
            &pa,
            "preview.open",
            json!({"preview": handle, "mode": "proxy", "no_open": true}),
        )
        .await
        .unwrap();
    assert!(rp.get("open_url").is_none(), "{rp}");
    let host3 = rp["host"].as_str().unwrap().to_string();
    assert_ne!(host3, host2);
    let u = e
        .call(&pa, "preview.url", json!({"preview": handle}))
        .await
        .unwrap();
    assert!(u["proxy_url"].as_str().unwrap().contains(&host3), "{u}");
    let u = e
        .call(&other, "preview.url", json!({"preview": handle}))
        .await
        .unwrap();
    assert_eq!(u["proxy_url"], Value::Null, "{u}");
    let st = e.call(&pa, "preview.status", json!({})).await.unwrap();
    assert_eq!(st["proxy"]["routes"][0]["host"], json!(host3));
    assert_eq!(st["proxy"]["routes"].as_array().unwrap().len(), 1);
    // Forgetting the preview removes its origin.
    let authority3 = format!("{host3}:{pport}");
    e.call(&full, "preview.forget", json!({"preview": handle}))
        .await
        .unwrap();
    assert_eq!(http(pport, &authority3, "/", None).await.0, 421);
    // Proxy mode needs a preview, not a URL.
    let bad = e
        .call(
            &full,
            "preview.open",
            json!({"url": "http://localhost:1/", "mode": "proxy"}),
        )
        .await
        .unwrap_err();
    assert_eq!(bad.code, ErrorKind::InvalidParams.code(), "{bad:?}");
}

#[tokio::test]
async fn proxy_and_mirror_route_to_a_remote_over_the_bridge() {
    let e = Env::new();
    let (real, seen) = app("remote-app").await;
    // Remote ports as the remote sees them: 7 is free here (mirrorable), 8 is busy here.
    let remote_free = free_port();
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let remote_busy = busy.local_addr().unwrap().port();
    e.server.previews.set_link_factory(Arc::new(move |m: &str| {
        (m == "fakebox").then(|| {
            fake_link(
                HashMap::from([
                    ("v7".to_string(), remote_free),
                    ("v8".to_string(), remote_busy),
                ]),
                HashMap::from([(remote_free, real), (remote_busy, real)]),
            )
        })
    }));
    let full = ctx_full();

    // Proxy: a remote preview through the bridge `tcp:` channel.
    let r = e
        .call(
            &full,
            "preview.open",
            json!({"preview": "fakebox/v7", "mode": "proxy", "no_open": true}),
        )
        .await
        .unwrap();
    assert_eq!(r["machine"], "fakebox");
    let host = r["host"].as_str().unwrap();
    assert!(host.starts_with("v7-"), "{host}");
    let (pport, authority, cookie) = login(r["open_url"].as_str().unwrap()).await;
    let (st, head, body) = http(pport, &authority, "/app", Some(&cookie)).await;
    assert_eq!(st, 200, "{head}");
    assert_eq!(body, "<p>remote-app /app</p>");
    let up = seen
        .lock()
        .unwrap()
        .last()
        .cloned()
        .unwrap()
        .to_ascii_lowercase();
    assert!(
        up.contains(&format!("host: localhost:{remote_free}")),
        "{up}"
    );
    assert!(!up.contains("vk_"), "{up}");
    let link = e.server.previews.link(&e.server, "fakebox").unwrap();
    assert!(
        link.bytes().await.unwrap().0 > 50,
        "traffic crossed the link"
    );

    // Mirror: not from a pane; a local preview can't be mirrored.
    let denied = e
        .call(
            &ctx_pane("pane-a"),
            "preview.mirror",
            json!({"preview": "fakebox/v7"}),
        )
        .await
        .unwrap_err();
    assert_eq!(denied.code, ErrorKind::PermissionDenied.code());
    let d = e
        .call(&full, "preview.declare", json!({"port": real}))
        .await
        .unwrap();
    let lh = d["preview"]["handle"].as_str().unwrap().to_string();
    assert_eq!(
        e.call(&full, "preview.mirror", json!({"preview": lh}))
            .await
            .unwrap_err()
            .code,
        ErrorKind::InvalidParams.code()
    );
    // Busy local port → conflict, nothing bound.
    let c = e
        .call(&full, "preview.mirror", json!({"preview": "fakebox/v8"}))
        .await
        .unwrap_err();
    assert_eq!(c.code, ErrorKind::Conflict.code(), "{c:?}");
    assert!(c.message.contains("busy"), "{}", c.message);
    // Free port → the remote port number answers on this machine's loopback.
    let m = e
        .call(&full, "preview.mirror", json!({"preview": "fakebox/v7"}))
        .await
        .unwrap();
    assert_eq!(m["local_port"], json!(remote_free));
    assert_eq!(m["authenticated"], false);
    assert!(m["warning"].as_str().unwrap().contains("Unauthenticated"));
    let (st, _, body) = http(
        remote_free,
        &format!("localhost:{remote_free}"),
        "/raw",
        None,
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(body, "<p>remote-app /raw</p>");
    let again = e
        .call(&full, "preview.mirror", json!({"preview": "fakebox/v7"}))
        .await
        .unwrap();
    assert_eq!(again["already"], true);
    let st = e.call(&full, "preview.status", json!({})).await.unwrap();
    let mirrors = st["mirrors"].as_array().unwrap();
    assert_eq!(mirrors.len(), 1);
    assert_eq!(mirrors[0]["accepted"], 1);
    assert_eq!(mirrors[0]["rejected"], 0);
    assert_eq!(e.events("preview.mirrored").len(), 1);
    // Unmirror closes the port.
    let u = e
        .call(&full, "preview.unmirror", json!({"preview": "fakebox/v7"}))
        .await
        .unwrap();
    assert_eq!(u["local_port"], json!(remote_free));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        TcpStream::connect(("127.0.0.1", remote_free))
            .await
            .is_err()
    );
    assert_eq!(
        e.call(&full, "preview.unmirror", json!({"preview": "fakebox/v7"}))
            .await
            .unwrap_err()
            .code,
        ErrorKind::NotFound.code()
    );
    drop(busy);
}

#[tokio::test]
async fn task_previews_use_the_lease_and_retire_with_the_task() {
    let e = Env::new();
    let checkout = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(checkout.path().join(".vibeke")).unwrap();
    std::fs::write(
        checkout.path().join(".vibeke/task.toml"),
        "[ports]\nenv = { PORT = 0, API_PORT = 3 }\n\n[previews]\nweb = { port_env = \"PORT\", path = \"/app\" }\napi = { port_env = \"API_PORT\", label = \"api\" }\nssh = { port = 22 }\n",
    )
    .unwrap();
    std::fs::write(
        checkout.path().join(".vibeke/previews.toml"),
        "docs = { offset = 9, scheme = \"https\" }\nweb = { offset = 5 }\n",
    )
    .unwrap();
    let lease = vk_tasks::Lease {
        start: 23450,
        end: 23459,
        task_id: "task-1".into(),
        session: "t".into(),
        owner_pid: None,
        created_at: 0,
    };
    let task = Task {
        id: "task-1".into(),
        handle: "k1".into(),
        title: "t".into(),
        slug: "fix-login".into(),
        port_range: Some((23450, 23459)),
        ..Default::default()
    };
    {
        let mut c = e.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.task(task.clone());
        e.server.commit(&mut c, tx).unwrap();
    }
    let v = declare_task_previews(
        &e.server,
        &ctx_full(),
        &task,
        "pane-a",
        checkout.path(),
        Some(&lease),
        &json!({"previews": {"admin": {"port": 23999, "label": "admin"}}}),
    );
    let previews = v["previews"].as_array().unwrap();
    let by = |n: &str| {
        previews
            .iter()
            .find(|p| p["name"] == n)
            .cloned()
            .unwrap_or(Value::Null)
    };
    assert_eq!(by("web")["port"], 23450, "{v}");
    assert_eq!(by("web")["path"], "/app");
    assert_eq!(by("web")["url"], "http://localhost:23450/app");
    assert_eq!(by("api")["port"], 23453);
    assert_eq!(by("api")["label"], "api");
    assert_eq!(by("docs")["port"], 23459);
    assert_eq!(by("docs")["scheme"], "https");
    // User-supplied (task.create params) may use an absolute port; the repo may not.
    assert_eq!(by("admin")["port"], 23999);
    assert!(by("ssh").is_null());
    let warnings = v["warnings"].to_string();
    assert!(warnings.contains("not port = 22"), "{warnings}");
    for p in previews {
        assert_eq!(p["task"], "task-1");
        assert_eq!(p["source"], "declared");
        assert_eq!(p["pane"], "pane-a");
    }
    // Declared before any server runs: listed for the task right away.
    let l = e
        .call(&ctx_full(), "preview.list", json!({"task": "k1"}))
        .await
        .unwrap();
    assert_eq!(l["previews"].as_array().unwrap().len(), 4, "{l}");
    // A dev server on the leased port turns the preview up.
    let srv = tokio::net::TcpListener::bind(("127.0.0.1", 23450)).await;
    if let Ok(_srv) = srv {
        crate::preview::discover(&e.server, &PreviewConfig::default()).await;
        let up = e.server.with_core(|c| {
            c.model
                .previews
                .iter()
                .find(|p| p.port == 23450)
                .map(|p| p.status)
        });
        assert_eq!(up, Some(PreviewStatus::Up));
    }
    // A proxy origin for a task preview goes with the task.
    let web = by("web")["handle"].as_str().unwrap().to_string();
    let r = e
        .call(
            &ctx_full(),
            "preview.open",
            json!({"preview": web, "mode": "proxy", "no_open": true}),
        )
        .await
        .unwrap();
    let host = r["host"].as_str().unwrap().to_string();
    assert!(proxy_port(&e.server).is_some());
    retire_task_previews(&e.server, "task-1");
    let st = e
        .call(&ctx_full(), "preview.status", json!({}))
        .await
        .unwrap();
    assert!(
        !st["proxy"]["routes"].to_string().contains(&host),
        "task finish revokes the origin: {st}"
    );
    let left = e.server.with_core(|c| {
        c.model
            .previews
            .iter()
            .filter(|p| p.task.as_deref() == Some("task-1"))
            .count()
    });
    assert_eq!(left, 0);
    assert!(e.events("preview.gone").len() >= 4);
}

#[test]
fn task_panes_get_the_leased_ports_and_other_panes_do_not() {
    let e = Env::new();
    let checkout = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(checkout.path().join(".vibeke")).unwrap();
    std::fs::write(
        checkout.path().join(".vibeke/task.toml"),
        "[ports]\nenv = { API_PORT = 3, VITE_PORT = 1, \"BAD-NAME\" = 2, FAR = 99 }\n",
    )
    .unwrap();
    let task = Task {
        id: "task-env".into(),
        handle: "k9".into(),
        title: "t".into(),
        slug: "env".into(),
        workspace: Some("ws-task".into()),
        worktree_path: Some(checkout.path().to_string_lossy().into_owned()),
        port_range: Some((24100, 24109)),
        ..Default::default()
    };
    {
        let mut c = e.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.task(task.clone());
        e.server.commit(&mut c, tx).unwrap();
    }
    let env_of = |ws_task: Option<&str>| {
        let te = e.server.with_core(|c| e.server.task_env_for(c, ws_task));
        e.server.pane_env_for("p1", "h1", "t1", "w1", &te)
    };
    let get = |env: &[(String, String)], k: &str| {
        env.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone())
    };
    let t = env_of(Some("task-env"));
    assert_eq!(get(&t, "PORT").as_deref(), Some("24100"));
    assert_eq!(get(&t, "API_PORT").as_deref(), Some("24103"));
    assert_eq!(get(&t, "VITE_PORT").as_deref(), Some("24101"));
    // Not a valid shell name / outside the lease: never exported.
    assert_eq!(get(&t, "BAD-NAME"), None);
    assert_eq!(get(&t, "FAR"), None);
    // Any other pane (plain workspace, or an unknown task) has none of them.
    for other in [None, Some("no-such-task")] {
        let o = env_of(other);
        assert_eq!(get(&o, "PORT"), None);
        assert_eq!(get(&o, "API_PORT"), None);
        assert_eq!(get(&o, "VIBEKE").as_deref(), Some("1"));
    }
    // A task without a lease gets nothing either.
    let unleased = Task {
        port_range: None,
        ..task
    };
    assert!(task_port_env(&unleased).is_empty());
    // The first pane of a task being created sees the pending env.
    e.server
        .pending_task_env
        .lock()
        .unwrap()
        .insert("new".into(), vec![("PORT".into(), "1".into())]);
    let p = e
        .server
        .with_core(|c| e.server.task_env_for(c, Some("new")));
    assert_eq!(p, vec![("PORT".to_string(), "1".to_string())]);
}

/// Two independent servers (sessions) with identical preview handles and labels: their proxy
/// origins never share a hostname (cookies are per host, not per port), each proxy serves
/// only its own hosts, and a cookie obtained from one is useless on the other.
#[tokio::test]
async fn two_sessions_with_identical_previews_never_share_a_host() {
    let a = Env::new();
    let b = Env::new();
    let (pa, seen_a) = app("app-a").await;
    let (pb, seen_b) = app("app-b").await;
    let full = ctx_full();
    let mut opened = vec![];
    for (e, port) in [(&a, pa), (&b, pb)] {
        let d = e
            .call(
                &full,
                "preview.declare",
                json!({"port": port, "label": "web"}),
            )
            .await
            .unwrap();
        let h = d["preview"]["handle"].as_str().unwrap().to_string();
        let r = e
            .call(
                &full,
                "preview.open",
                json!({"preview": h, "mode": "proxy", "no_open": true}),
            )
            .await
            .unwrap();
        opened.push((h, r));
    }
    assert_eq!(opened[0].0, opened[1].0, "same handle in both sessions");
    let (ha, hb) = (
        opened[0].1["host"].as_str().unwrap(),
        opened[1].1["host"].as_str().unwrap(),
    );
    assert_ne!(ha, hb, "two sessions must never share a preview hostname");
    // One browser cookie jar: log into both. A's cookie is only ever sent to A's host; and
    // even replayed against B's proxy under A's host it is refused (B doesn't serve it).
    let (porta, autha, cka) = login(opened[0].1["open_url"].as_str().unwrap()).await;
    let (portb, authb, ckb) = login(opened[1].1["open_url"].as_str().unwrap()).await;
    assert_ne!(porta, portb);
    assert_eq!(
        http(porta, &autha, "/", Some(&cka)).await.2,
        "<p>app-a /</p>"
    );
    assert_eq!(
        http(portb, &authb, "/", Some(&ckb)).await.2,
        "<p>app-b /</p>"
    );
    let a_on_b = format!("{ha}:{portb}");
    assert_eq!(http(portb, &a_on_b, "/", Some(&cka)).await.0, 421);
    assert_eq!(http(portb, &authb, "/", Some(&cka)).await.0, 401);
    assert_eq!(seen_a.lock().unwrap().len(), 1);
    assert_eq!(seen_b.lock().unwrap().len(), 1);
}

/// The configured proxy port is machine-wide: a second session asking for the same port is
/// refused (naming the owner), never silently moved to another port.
#[tokio::test]
async fn a_second_proxy_on_the_same_port_is_refused() {
    let a = Env::new();
    let b = Env::new();
    let port = free_port();
    a.server.previews.set_proxy_port(port);
    b.server.previews.set_proxy_port(port);
    let (app_port, _) = app("x").await;
    let full = ctx_full();
    let mut results = vec![];
    for e in [&a, &b] {
        let d = e
            .call(&full, "preview.declare", json!({"port": app_port}))
            .await
            .unwrap();
        let h = d["preview"]["handle"].as_str().unwrap().to_string();
        results.push(
            e.call(
                &full,
                "preview.open",
                json!({"preview": h, "mode": "proxy", "no_open": true}),
            )
            .await,
        );
    }
    let first = results[0].as_ref().unwrap();
    assert_eq!(first["proxy_port"], json!(port));
    let second = results[1].as_ref().unwrap_err();
    assert_eq!(second.code, ErrorKind::Conflict.code(), "{second:?}");
    assert!(
        second.message.contains(&port.to_string()) && second.message.contains("pid"),
        "{}",
        second.message
    );
    assert!(proxy_port(&b.server).is_none(), "no proxy started for b");
}

/// Repo- or caller-controlled preview paths are absolute-path references; proxy mode re-checks
/// the stored path (e.g. one a remote machine reports) before minting a link.
#[tokio::test]
async fn preview_paths_cannot_leave_the_preview_origin() {
    let e = Env::new();
    let (port, _) = app("p").await;
    let full = ctx_full();
    for bad in [
        "//attacker.example/",
        "/\\attacker.example/",
        "https://attacker.example/",
        "javascript:alert(1)",
    ] {
        let r = e
            .call(&full, "preview.declare", json!({"port": port, "path": bad}))
            .await
            .unwrap_err();
        assert_eq!(r.code, ErrorKind::InvalidParams.code(), "{bad}: {r:?}");
    }
    let d = e
        .call(
            &full,
            "preview.declare",
            json!({"port": port, "path": "/app#/route"}),
        )
        .await
        .unwrap();
    let h = d["preview"]["handle"].as_str().unwrap().to_string();
    let r = e
        .call(
            &full,
            "preview.open",
            json!({"preview": h, "mode": "proxy", "no_open": true}),
        )
        .await
        .unwrap();
    let open_url = r["open_url"].as_str().unwrap();
    assert!(
        open_url.contains("/app?vk_token=") && open_url.ends_with("#/route"),
        "{open_url}"
    );
    // A stored record with a network-path reference (older data / a remote's answer).
    let mut pv = find_local(&e.server, &h).unwrap();
    pv.path = "//attacker.example/".into();
    commit_previews(&e.server, vec![(pv, None)]);
    let r = e
        .call(
            &full,
            "preview.open",
            json!({"preview": h, "mode": "proxy", "no_open": true}),
        )
        .await
        .unwrap_err();
    assert_eq!(r.code, ErrorKind::InvalidParams.code(), "{r:?}");
}

/// Retiring a local preview by any path (lifecycle, task finish) revokes its origin and
/// sessions; a request re-checks the preview before connecting.
#[tokio::test]
async fn retired_local_previews_lose_their_origin() {
    let e = Env::new();
    let (port, seen) = app("local").await;
    let full = ctx_full();
    let d = e
        .call(&full, "preview.declare", json!({"port": port}))
        .await
        .unwrap();
    let h = d["preview"]["handle"].as_str().unwrap().to_string();
    let r = e
        .call(
            &full,
            "preview.open",
            json!({"preview": h, "mode": "proxy", "no_open": true}),
        )
        .await
        .unwrap();
    let (pport, auth, ck) = login(r["open_url"].as_str().unwrap()).await;
    assert_eq!(http(pport, &auth, "/", Some(&ck)).await.0, 200);
    // Moved to another port in the model (e.g. re-declared): the old route is refused
    // before anything connects, then gone.
    let mut pv = find_local(&e.server, &h).unwrap();
    pv.port = free_port();
    commit_previews(&e.server, vec![(pv.clone(), None)]);
    let n = seen.lock().unwrap().len();
    assert_eq!(http(pport, &auth, "/", Some(&ck)).await.0, 410);
    assert_eq!(http(pport, &auth, "/", Some(&ck)).await.0, 421);
    assert_eq!(seen.lock().unwrap().len(), n);
    // Retired through the commit path (what lifecycle timeouts and task finish use).
    pv.port = port;
    commit_previews(&e.server, vec![(pv.clone(), None)]);
    let r = e
        .call(
            &full,
            "preview.open",
            json!({"preview": h, "mode": "proxy", "no_open": true}),
        )
        .await
        .unwrap();
    let (pport, auth, ck) = login(r["open_url"].as_str().unwrap()).await;
    assert_eq!(http(pport, &auth, "/", Some(&ck)).await.0, 200);
    vk_preview::lifecycle::retire(&mut pv);
    commit_previews(&e.server, vec![(pv, Some("preview.gone"))]);
    assert_eq!(http(pport, &auth, "/", Some(&ck)).await.0, 421);
}

/// Forgetting a remote preview revokes this server's origin for it at once; a remote preview
/// that disappears or moves on its machine (task finished there, port reused) is revoked on the
/// next request. The old cookie never reaches the replacement service.
#[tokio::test]
async fn remote_forget_and_retirement_revoke_the_route() {
    let e = Env::new();
    let (real, seen) = app("remote-app").await;
    let (other, other_seen) = app("replacement").await;
    let remote_port = free_port();
    let previews: Shared<String, u16> = Arc::new(std::sync::Mutex::new(HashMap::from([
        ("v7".to_string(), remote_port),
        ("v9".to_string(), remote_port + 1),
    ])));
    let ports: Shared<u16, u16> = Arc::new(std::sync::Mutex::new(HashMap::from([
        (remote_port, real),
        (remote_port + 1, real),
    ])));
    let (pv2, po2) = (previews.clone(), ports.clone());
    e.server.previews.set_link_factory(Arc::new(move |m: &str| {
        (m == "fakebox").then(|| fake_link_shared(pv2.clone(), po2.clone()))
    }));
    let full = ctx_full();
    let open = |t: &'static str| {
        let e = &e;
        let full = full.clone();
        async move {
            e.call(
                &full,
                "preview.open",
                json!({"preview": t, "mode": "proxy", "no_open": true}),
            )
            .await
            .unwrap()
        }
    };
    // 1. Explicit forget through this server.
    let r = open("fakebox/v7").await;
    let (pport, auth, ck) = login(r["open_url"].as_str().unwrap()).await;
    assert_eq!(http(pport, &auth, "/app", Some(&ck)).await.0, 200);
    e.call(&full, "preview.forget", json!({"preview": "fakebox/v7"}))
        .await
        .unwrap();
    // The remote port now belongs to another service.
    ports.lock().unwrap().insert(remote_port, other);
    let n = seen.lock().unwrap().len();
    assert_eq!(http(pport, &auth, "/app", Some(&ck)).await.0, 421);
    assert_eq!(seen.lock().unwrap().len(), n);
    assert!(other_seen.lock().unwrap().is_empty());

    // 2. Retired on the remote (e.g. its task finished there): refused on the next request.
    let r = open("fakebox/v9").await;
    let (pport, auth, ck) = login(r["open_url"].as_str().unwrap()).await;
    assert_eq!(http(pport, &auth, "/app", Some(&ck)).await.0, 200);
    previews.lock().unwrap().remove("v9");
    ports.lock().unwrap().insert(remote_port + 1, other);
    tokio::time::sleep(REMOTE_CHECK_TTL + Duration::from_millis(100)).await;
    assert_eq!(http(pport, &auth, "/app", Some(&ck)).await.0, 410);
    assert_eq!(http(pport, &auth, "/app", Some(&ck)).await.0, 421);
    assert!(other_seen.lock().unwrap().is_empty());

    // 3. Moved on the remote (same handle, new port): the old route is revoked too.
    previews
        .lock()
        .unwrap()
        .insert("v9".into(), remote_port + 1);
    ports.lock().unwrap().insert(remote_port + 1, real);
    let r = open("fakebox/v9").await;
    let (pport, auth, ck) = login(r["open_url"].as_str().unwrap()).await;
    assert_eq!(http(pport, &auth, "/app", Some(&ck)).await.0, 200);
    previews
        .lock()
        .unwrap()
        .insert("v9".into(), remote_port + 2);
    tokio::time::sleep(REMOTE_CHECK_TTL + Duration::from_millis(100)).await;
    assert_eq!(http(pport, &auth, "/app", Some(&ck)).await.0, 410);
}

// ---- tls_origin ---------------------------------------------------------------------------------

/// One HTTPS request to the proxy; the client trusts only the preview CA (never the system).
async fn https(port: u16, host: &str, path: &str, cookie: Option<&str>) -> (u16, String, String) {
    let ca = crate::preview_ca::load().unwrap();
    let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let name = rustls::pki_types::ServerName::try_from(host.to_string()).unwrap();
    let mut s = tokio_rustls::TlsConnector::from(vk_preview::ca::client_config_trusting(&[ca
        .cert_der()
        .clone()]))
    .connect(name, tcp)
    .await
    .expect("TLS handshake with the preview CA as the only root");
    let ck = cookie
        .map(|c| format!("Cookie: {}={c}\r\n", vk_preview::proxy::COOKIE))
        .unwrap_or_default();
    s.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\n{ck}Connection: close\r\n\r\n")
            .as_bytes(),
    )
    .await
    .unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut out)).await;
    let t = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = t.split_once("\r\n\r\n").unwrap_or((&t, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|x| x.parse().ok())
        .unwrap_or(0);
    (status, head.to_string(), body.to_string())
}

fn split_url(url: &str, scheme: &str) -> (u16, String, String) {
    let rest = url.strip_prefix(&format!("{scheme}://")).unwrap();
    let (authority, path) = rest.split_at(rest.find('/').unwrap());
    let (host, port) = authority.rsplit_once(':').unwrap();
    (port.parse().unwrap(), host.to_string(), path.to_string())
}

#[tokio::test]
async fn tls_origin_opens_an_https_origin_signed_by_the_local_ca() {
    let e = Env::new();
    let (port, seen) = app("secure-app").await;
    let full = ctx_full();
    let d = e
        .call(
            &full,
            "preview.declare",
            json!({"port": port, "path": "/dash", "label": "web"}),
        )
        .await
        .unwrap();
    let handle = d["preview"]["handle"].as_str().unwrap().to_string();
    let r = e
        .call(
            &full,
            "preview.open",
            json!({"preview": handle, "proxy": true, "no_open": true, "tls_origin": true}),
        )
        .await
        .unwrap();
    assert_eq!(r["tls_origin"], true, "{r}");
    let url = r["url"].as_str().unwrap().to_string();
    assert!(
        url.starts_with("https://") && url.ends_with("/dash"),
        "{url}"
    );
    let open_url = r["open_url"].as_str().unwrap().to_string();
    assert!(open_url.starts_with("https://") && open_url.contains("vk_token="));
    // The CA is reported (public data only) and lives in a private directory; nothing installed it.
    let ca_path = std::path::PathBuf::from(r["ca"]["path"].as_str().unwrap());
    assert!(ca_path.exists());
    assert_eq!(ca_path.parent().unwrap(), crate::preview_ca::ca_dir());
    assert!(r["ca"]["sha256"].as_str().unwrap().contains(':'));
    assert!(!r.to_string().contains("PRIVATE KEY"));
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(ca_path.parent().unwrap()), 0o700);
    assert_eq!(mode(&ca_path.with_file_name("preview-ca-key.pem")), 0o600);

    // Browser flow over https: token -> Secure cookie -> the app.
    let (pport, host, path) = split_url(&open_url, "https");
    let (st, head, _) = https(pport, &host, &path, None).await;
    assert_eq!(st, 303, "{head}");
    let lower = head.to_ascii_lowercase();
    assert!(
        lower.contains(&format!("location: https://{host}:{pport}/dash")),
        "{head}"
    );
    let cookie_line = lower
        .lines()
        .find(|l| l.starts_with("set-cookie: __host-vk_preview="))
        .unwrap()
        .to_string();
    assert!(
        cookie_line.contains("; secure") && cookie_line.contains("httponly"),
        "{cookie_line}"
    );
    let cookie = head
        .lines()
        .find_map(|l| l.strip_prefix(&format!("set-cookie: {}=", vk_preview::proxy::COOKIE)))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let (st, _, body) = https(pport, &host, "/dash", Some(&cookie)).await;
    assert_eq!(st, 200);
    assert_eq!(body, "<p>secure-app /dash</p>");
    let up = seen
        .lock()
        .unwrap()
        .last()
        .cloned()
        .unwrap()
        .to_ascii_lowercase();
    assert!(
        up.contains(&format!("host: localhost:{port}")) && !up.contains("vk_"),
        "{up}"
    );
    // Plain HTTP to the https origin is refused.
    let (st, _, _) = http(pport, &format!("{host}:{pport}"), "/dash", None).await;
    assert_eq!(st, 421);

    // preview.url and preview.status show the https origin.
    let u = e
        .call(&full, "preview.url", json!({"preview": handle}))
        .await
        .unwrap();
    assert_eq!(u["proxy_url"], url, "{u}");
    let st = e.call(&full, "preview.status", json!({})).await.unwrap();
    assert_eq!(st["proxy"]["tls"], true, "{st}");
    assert_eq!(st["proxy"]["routes"][0]["tls"], true);

    // Re-opening with tls_origin off switches the origin back to http and drops the old session.
    let r2 = e
        .call(
            &full,
            "preview.open",
            json!({"preview": handle, "proxy": true, "no_open": true, "tls_origin": false}),
        )
        .await
        .unwrap();
    assert_eq!(r2["tls_origin"], false);
    assert!(r2["url"].as_str().unwrap().starts_with("http://"), "{r2}");
    assert!(r2.get("ca").is_none());
    // The https origin is gone: no certificate is issued for it any more.
    let ca = crate::preview_ca::load().unwrap();
    let tcp = TcpStream::connect(("127.0.0.1", pport)).await.unwrap();
    let name = rustls::pki_types::ServerName::try_from(host.clone()).unwrap();
    let hs = tokio_rustls::TlsConnector::from(vk_preview::ca::client_config_trusting(&[ca
        .cert_der()
        .clone()]))
    .connect(name, tcp)
    .await;
    assert!(hs.is_err());
}

#[tokio::test]
async fn tls_origin_per_preview_and_repo_defaults() {
    let e = Env::new();
    let full = ctx_full();
    // Per preview: declare remembers it; the global default (off) applies to the others.
    let (p1, _) = app("one").await;
    let (p2, _) = app("two").await;
    let h = |d: &Value| d["preview"]["handle"].as_str().unwrap().to_string();
    let d1 = e
        .call(
            &full,
            "preview.declare",
            json!({"port": p1, "tls_origin": true}),
        )
        .await
        .unwrap();
    let d2 = e
        .call(&full, "preview.declare", json!({"port": p2}))
        .await
        .unwrap();
    let open = |handle: String| {
        let e = &e;
        let full = &full;
        async move {
            e.call(
                full,
                "preview.open",
                json!({"preview": handle, "proxy": true, "no_open": true}),
            )
            .await
            .unwrap()
        }
    };
    let r1 = open(h(&d1)).await;
    assert_eq!(r1["tls_origin"], true, "{r1}");
    assert!(r1["url"].as_str().unwrap().starts_with("https://"));
    let r2 = open(h(&d2)).await;
    assert_eq!(r2["tls_origin"], false);
    assert!(r2["url"].as_str().unwrap().starts_with("http://"));
    // The setting is stored with the session (survives a re-open without the parameter).
    assert_eq!(
        tls_origin_of(&e.server, d1["preview"]["id"].as_str().unwrap()),
        Some(true)
    );
    assert_eq!(
        tls_origin_of(&e.server, d2["preview"]["id"].as_str().unwrap()),
        None
    );
    // The https and the http origin share one proxy port.
    assert_eq!(r1["proxy_port"], r2["proxy_port"]);

    // Repo `[previews] tls_origin = true` is the default for task previews; an entry overrides.
    let checkout = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(checkout.path().join(".vibeke")).unwrap();
    std::fs::write(
        checkout.path().join(".vibeke/task.toml"),
        "[ports]\nenv = { PORT = 0, API_PORT = 3 }\n\n[previews]\ntls_origin = true\nweb = { port_env = \"PORT\" }\napi = { port_env = \"API_PORT\", tls_origin = false }\n",
    )
    .unwrap();
    let lease = vk_tasks::Lease {
        start: 23470,
        end: 23479,
        task_id: "task-tls".into(),
        session: "t".into(),
        owner_pid: None,
        created_at: 0,
    };
    let task = Task {
        id: "task-tls".into(),
        handle: "k9".into(),
        title: "t".into(),
        slug: "tls".into(),
        port_range: Some((23470, 23479)),
        ..Default::default()
    };
    {
        let mut c = e.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.task(task.clone());
        e.server.commit(&mut c, tx).unwrap();
    }
    let v = declare_task_previews(
        &e.server,
        &full,
        &task,
        "pane-a",
        checkout.path(),
        Some(&lease),
        &json!({}),
    );
    let by = |n: &str| {
        v["previews"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == n)
            .cloned()
            .unwrap_or(Value::Null)
    };
    assert!(
        by("web")["port"] == 23470 && by("api")["port"] == 23473,
        "{v}"
    );
    assert!(v["warnings"].as_array().unwrap().is_empty(), "{v}");
    let web = open(by("web")["handle"].as_str().unwrap().to_string()).await;
    let api = open(by("api")["handle"].as_str().unwrap().to_string()).await;
    assert_eq!(web["tls_origin"], true, "{web}");
    assert!(web["url"].as_str().unwrap().starts_with("https://"));
    assert_eq!(api["tls_origin"], false, "{api}");
    assert!(api["url"].as_str().unwrap().starts_with("http://"));
}
