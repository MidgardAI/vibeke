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
        };
        let server = Server::new(paths, opts).unwrap();
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
    let previews = Arc::new(previews);
    let ports = Arc::new(ports);
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
                    let real = *ports
                        .get(&p)
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

async fn fake_rpc(s: tokio::io::DuplexStream, previews: Arc<HashMap<String, u16>>) {
    let (rd, mut wr) = tokio::io::split(s);
    let mut rd = tokio::io::BufReader::new(rd);
    let mut line = String::new();
    if rd.read_line(&mut line).await.is_err() {
        return;
    }
    let req: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
    let handle = req["params"]["preview"].as_str().unwrap_or("").to_string();
    let resp = match previews.get(&handle) {
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
            json!({"jsonrpc": "2.0", "id": req["id"], "error": {"code": -32001, "message": "not found"}})
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
        host.starts_with(&format!("{handle}-web")) && host.ends_with(".vibeke.localhost"),
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
    // Re-opening keeps the origin; `preview.url` reports it without a credential.
    let r2 = e
        .call(
            &full,
            "preview.open",
            json!({"preview": handle, "proxy": true, "no_open": true}),
        )
        .await
        .unwrap();
    assert_eq!(r2["host"], json!(host));
    let u = e
        .call(&full, "preview.url", json!({"preview": handle}))
        .await
        .unwrap();
    assert_eq!(u["proxy_url"], json!(plain));
    // A pane-scoped agent can open it for the human but never receives the credential.
    let rp = e
        .call(
            &ctx_pane("pane-a"),
            "preview.open",
            json!({"preview": handle, "mode": "proxy", "no_open": true}),
        )
        .await
        .unwrap();
    assert!(rp.get("open_url").is_none(), "{rp}");
    // Events never carry the token.
    let ev = e.events("preview.opened");
    assert!(!ev.is_empty());
    assert!(!json!(ev).to_string().contains("vk_token"), "{ev:?}");
    // Status: the proxy and its route; no mirror unless explicitly enabled.
    let st = e.call(&full, "preview.status", json!({})).await.unwrap();
    assert_eq!(st["proxy"]["port"], json!(pport));
    assert_eq!(st["proxy"]["routes"][0]["host"], json!(host));
    assert_eq!(st["mirrors"], json!([]));
    // Forgetting the preview removes its origin.
    e.call(&full, "preview.forget", json!({"preview": handle}))
        .await
        .unwrap();
    assert_eq!(http(pport, &authority, "/", Some(&cookie)).await.0, 421);
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
    assert!(host.starts_with("v7-fakebox"), "{host}");
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
    retire_task_previews(&e.server, "task-1");
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
