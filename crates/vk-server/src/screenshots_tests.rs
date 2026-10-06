//! Screenshots as evidence (Goal 03 Stage 4): code state capture, running-build probe and
//! binding, retention with an injected clock, visual diff, pane-scope reads.

use super::*;
use crate::ServerOpts;
use crate::paths::Paths;
use std::process::Command;
use std::sync::Once;
use vk_proto::model::{Pane, Preview, PreviewSource};
use vk_review::subject::DirtyState;

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let base = std::env::temp_dir().join(format!("vk-screenshot-tests-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        // SAFETY: set once, before any server in this test binary reads them.
        unsafe {
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

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

struct Env {
    _dir: tempfile::TempDir,
    server: Arc<Server>,
    repo: PathBuf,
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

fn png(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
    let img = image::RgbaImage::from_fn(w, h, |x, y| image::Rgba(f(x, y)));
    let mut v = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut v), image::ImageFormat::Png)
        .unwrap();
    v
}

fn env_of(kind: EnvKind) -> Environment {
    Environment {
        kind,
        machine: "testbox".into(),
        runner: "host".into(),
        browser: "HeadlessChrome/153.0".into(),
        browser_version: Some("153.0".into()),
        viewport: Viewport {
            width: 1440,
            height: 900,
        },
        dpr: 1.0,
        color_scheme: None,
        device: None,
        fresh_context: kind == EnvKind::RemoteHeadless,
        profile: (kind == EnvKind::LocalPane).then(|| "devbox".into()),
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
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("app.js"), "blue\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        let e = Env {
            _dir: dir,
            server,
            repo,
        };
        e.put_pane("pane-a", "ws-a", Some(&e.repo.clone()));
        e.put_pane("pane-b", "ws-b", None);
        e
    }

    fn put_pane(&self, id: &str, ws: &str, cwd: Option<&Path>) {
        let p = Pane {
            id: id.into(),
            handle: id.into(),
            tab: format!("tab-{ws}"),
            workspace: ws.into(),
            title: None,
            auto_title: String::new(),
            cwd: cwd.map(|c| c.to_string_lossy().into_owned()),
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

    fn put_preview(&self, handle: &str, port: u16, task: Option<&str>) {
        let p = Preview {
            id: ulid(),
            handle: handle.into(),
            machine: "testbox".into(),
            pane: Some("pane-a".into()),
            task: task.map(str::to_string),
            port,
            path: "/".into(),
            label: None,
            url: format!("http://localhost:{port}/"),
            scheme: "http".into(),
            status: PreviewStatus::Up,
            source: PreviewSource::Declared,
            pid: None,
            first_seen_ms: 0,
            last_seen_ms: 0,
        };
        self.server.with_core(|c| c.model.previews.push(p));
    }

    fn inputs(&self, pane: &str, kind: EnvKind) -> ShotInputs {
        ShotInputs {
            environment: env_of(kind),
            url: "http://localhost:5173/".into(),
            final_url: None,
            title: None,
            preview: None,
            session: None,
            taken_by: Requester {
                kind: "agent".into(),
                pane: Some(pane.into()),
                run: None,
                client: None,
            },
            full_page: false,
            selector: None,
            checkout: None,
            runtime: None,
            probe_runtime: false,
            document: None,
        }
    }

    async fn shot(&self, data: &[u8], inputs: ShotInputs) -> ScreenshotMeta {
        record_screenshot(&self.server, data, inputs).await.unwrap()
    }

    async fn call(&self, ctx: &Ctx, method: &str, p: Value) -> R {
        crate::api::authorize(&self.server, ctx, method, &p)?;
        api(&self.server, ctx, method, &p)
            .await
            .expect("screenshot method")
    }

    fn events(&self, kind: &str) -> Vec<vk_store::Event> {
        self.server.with_core(|c| {
            c.store
                .events_after(0, 10_000, &[kind.to_string()])
                .unwrap()
        })
    }

    /// Backdate a record (retention tests).
    fn set_created(&self, id: &str, at: i64) {
        let mut m = find(&self.server, id).unwrap();
        m.created_at_ms = at;
        m.taken_at = at;
        let mut c = self.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.put(KIND, &m.id, Some(&m.handle), &m);
        self.server.commit(&mut c, tx).unwrap();
    }

    fn set_task(&self, id: &str, task: &str) {
        let mut m = find(&self.server, id).unwrap();
        m.task = Some(task.into());
        let mut c = self.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.put(KIND, &m.id, Some(&m.handle), &m);
        self.server.commit(&mut c, tx).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn records_code_state_clean_and_dirty_with_events() {
    let e = Env::new();
    let head = git(&e.repo, &["rev-parse", "HEAD"]);
    let data = png(4, 3, |_, _| [255, 255, 255, 255]);
    let m = e
        .shot(&data, e.inputs("pane-a", EnvKind::RemoteHeadless))
        .await;
    assert_eq!(m.handle, "s1");
    assert_eq!((m.width, m.height), (4, 3));
    assert_eq!(m.workspace.as_deref(), Some("ws-a"));
    assert_eq!(m.pane.as_deref(), Some("pane-a"));
    let code = m.code.clone().expect("code state from the pane's cwd");
    assert_eq!(code.head_sha.as_deref(), Some(head.as_str()));
    assert_eq!(code.dirty_state, DirtyState::Clean);
    assert_eq!(code.dirty_digest, None);
    // No runtime identity → illustrative, "Build not verified".
    assert_eq!(m.runtime.status, RuntimeStatus::Unknown);
    assert_eq!(m.binding, Binding::Illustrative);
    assert_eq!(m.label, "testbox · headless · fresh context");
    assert!(m.path(&e.server).exists());
    let side: Value =
        serde_json::from_slice(&std::fs::read(m.path(&e.server).with_extension("json")).unwrap())
            .unwrap();
    assert_eq!(side["id"], m.id);

    // Dirty tree → digest; the browser pane path labels the environment distinctly.
    std::fs::write(e.repo.join("app.js"), "red\n").unwrap();
    let m2 = e.shot(&data, e.inputs("pane-a", EnvKind::LocalPane)).await;
    let c2 = m2.code.clone().unwrap();
    assert_eq!(c2.dirty_state, DirtyState::Dirty);
    assert!(c2.dirty_digest.is_some());
    assert_eq!(m2.handle, "s2");
    assert_eq!(m2.label, "your browser pane · profile devbox");
    assert_eq!(m2.environment.kind.as_str(), "local_pane");

    // A pane without a checkout: no code state, and the note says why.
    let m3 = e
        .shot(&data, e.inputs("pane-b", EnvKind::RemoteHeadless))
        .await;
    assert!(m3.code.is_none());
    assert!(m3.code_note.is_some());
    assert_eq!(m3.binding, Binding::Illustrative);

    // Caller-provided runtime identity equal to the checkout → bound.
    let rt = RuntimeIdentity::from_report(&serde_json::to_value(&c2).unwrap(), "caller", 1);
    let mut i = e.inputs("pane-a", EnvKind::LocalPane);
    i.runtime = rt;
    let m4 = e.shot(&data, i).await;
    assert_eq!(m4.binding, Binding::Bound, "{}", m4.binding_reason);

    let ev = e.events("screenshot.captured");
    assert_eq!(ev.len(), 4);
    assert_eq!(ev[0].data["binding"], "illustrative");
    assert_eq!(ev[3].data["binding"], "bound");
    assert_eq!(ev[3].data["id"], m4.id);
    assert!(ev[3].data.get("data_b64").is_none());
}

/// A loopback HTTP server answering every request with `body` (and `header`, if any).
async fn build_server(status: &'static str, header: Option<String>, body: String) -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            let header = header.clone();
            let body = body.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                assert!(req.starts_with("GET /__vibeke_build "), "{req}");
                let extra = header
                    .map(|h| format!("X-Vibeke-Build: {h}\r\n"))
                    .unwrap_or_default();
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{extra}Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    port
}

/// The captured document (as the browser reports it around the pixels), loaded after now.
fn doc(href: &str, build: Option<Value>) -> Option<DocumentCapture> {
    let u = url::Url::parse(href).unwrap();
    Some(DocumentCapture::Same(DocumentIdentity {
        href: href.into(),
        origin: u.origin().ascii_serialization(),
        title: Some("page".into()),
        time_origin_ms: Some(vk_store::now_ms() as f64 + 1000.0),
        build,
    }))
}

#[tokio::test(flavor = "multi_thread")]
async fn running_build_probe_decides_binding() {
    let e = Env::new();
    let code = capture_code_state(&e.repo).unwrap();
    let data = png(2, 2, |_, _| [0, 0, 0, 255]);

    // The app serves the checkout state it was built from (`vibeke screenshot code-state`),
    // which started before the captured page was loaded.
    let port = build_server("200 OK", None, serde_json::to_string(&code).unwrap()).await;
    e.put_preview("v1", port, None);
    let mut i = e.inputs("pane-a", EnvKind::RemoteHeadless);
    i.url = format!("http://localhost:{port}/page");
    i.preview = Some("v1".into());
    i.probe_runtime = true;
    i.document = doc(&i.url, None);
    // Without the captured document's identity the probe can't vouch for these pixels.
    let mut no_doc = i.clone();
    no_doc.document = None;
    let m = e.shot(&data, no_doc).await;
    assert_eq!(m.runtime.source, "probe");
    assert_eq!(m.binding, Binding::Illustrative);
    assert!(
        m.binding_reason.contains("identity is unknown"),
        "{}",
        m.binding_reason
    );
    let m = e.shot(&data, i.clone()).await;
    assert_eq!(m.runtime.status, RuntimeStatus::Known);
    assert_eq!(m.runtime.source, "probe");
    assert_eq!(m.binding, Binding::Bound, "{}", m.binding_reason);
    assert_eq!(m.preview.as_deref(), Some("v1"));

    // The checkout moved on; the server still reports the old build → illustrative.
    std::fs::write(e.repo.join("app.js"), "green\n").unwrap();
    git(&e.repo, &["commit", "-qam", "green"]);
    let m = e.shot(&data, i.clone()).await;
    assert_eq!(m.binding, Binding::Illustrative);
    assert!(
        m.binding_reason.contains("Running build is"),
        "{}",
        m.binding_reason
    );

    // Header form on a 404 body: it carries no start time, so the server's build can't be
    // tied to a page loaded earlier → illustrative; the same identity in the page binds.
    let head = git(&e.repo, &["rev-parse", "HEAD"]);
    let port2 = build_server("404 Not Found", Some(head.clone()), "{}".into()).await;
    e.put_preview("v2", port2, None);
    let mut i2 = i.clone();
    i2.url = format!("http://127.0.0.1:{port2}/");
    i2.preview = Some("v2".into());
    i2.document = doc(&i2.url, None);
    let m = e.shot(&data, i2.clone()).await;
    assert_eq!(m.runtime.source, "header");
    assert_eq!(m.binding, Binding::Illustrative, "{}", m.binding_reason);
    i2.document = doc(&i2.url, Some(json!(head.clone())));
    let m = e.shot(&data, i2).await;
    assert_eq!(m.runtime.source, "page");
    assert_eq!(m.binding, Binding::Bound, "{}", m.binding_reason);

    // Not a preview port → never probed.
    let port3 = build_server("200 OK", None, serde_json::to_string(&code).unwrap()).await;
    let mut i3 = i.clone();
    i3.url = format!("http://localhost:{port3}/");
    i3.preview = None;
    i3.document = doc(&i3.url, None);
    let m = e.shot(&data, i3).await;
    assert_eq!(m.runtime.status, RuntimeStatus::Unknown);
    assert!(
        m.runtime
            .detail
            .as_deref()
            .unwrap()
            .contains("not served by a preview"),
        "{:?}",
        m.runtime.detail
    );
    assert_eq!(m.binding, Binding::Illustrative);

    // A preview that answers without an identity.
    let port4 = build_server("200 OK", None, "{\"ok\":true}".into()).await;
    e.put_preview("v4", port4, None);
    let mut i4 = i.clone();
    i4.url = format!("http://localhost:{port4}/");
    i4.document = doc(&i4.url, None);
    let m = e.shot(&data, i4).await;
    assert_eq!(m.runtime.status, RuntimeStatus::Unknown);
    assert!(m.binding_reason.contains("Build not verified"));
}

/// An identity server on `addr` whose `/__vibeke_build` answer depends on the `Host` header.
async fn identity_server(
    l: tokio::net::TcpListener,
    answer: impl Fn(&str) -> String + Send + Sync + 'static,
) {
    let answer = Arc::new(answer);
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            let answer = answer.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let host = req
                    .lines()
                    .find_map(|l| l.strip_prefix("Host: "))
                    .unwrap_or("")
                    .to_string();
                let body = answer(&host);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
}

/// Codex review finding 7: the binding is the captured document's. Loaded A / serving B,
/// navigation during capture, different loopback addresses on one port and virtual hosts on
/// one port are each attributed correctly; uncertainty is illustrative.
#[tokio::test(flavor = "multi_thread")]
async fn binding_is_the_captured_documents() {
    let e = Env::new();
    let data = png(2, 2, |_, _| [0, 0, 0, 255]);
    let a = capture_code_state(&e.repo).unwrap();
    let a_json = serde_json::to_value(&a).unwrap();

    // Loaded A (the page says so), then checkout and server moved to B without a reload.
    std::fs::write(e.repo.join("app.js"), "b\n").unwrap();
    git(&e.repo, &["commit", "-qam", "b"]);
    let b = capture_code_state(&e.repo).unwrap();
    let b_body = serde_json::to_string(&b).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let bb = b_body.clone();
    identity_server(l, move |_| bb.clone()).await;
    e.put_preview("v1", port, None);
    let mut i = e.inputs("pane-a", EnvKind::RemoteHeadless);
    i.url = format!("http://localhost:{port}/");
    i.preview = Some("v1".into());
    i.probe_runtime = true;
    i.document = doc(&i.url, Some(a_json.clone()));
    let m = e.shot(&data, i.clone()).await;
    assert_eq!(m.runtime.source, "page");
    assert_eq!(m.runtime.head_sha, a.head_sha);
    assert_eq!(
        m.binding,
        Binding::Illustrative,
        "A's pixels never bind to B"
    );
    assert_eq!(
        m.document.as_ref().map(|d| d.href.as_str()),
        Some(i.url.as_str())
    );
    // Same, but the page carries no identity: the server reports B, which started after the
    // page was loaded → can't be the page's build.
    let mut stale = i.clone();
    stale.document = Some(DocumentCapture::Same(DocumentIdentity {
        href: i.url.clone(),
        origin: format!("http://localhost:{port}"),
        title: None,
        time_origin_ms: Some((b.captured_at_ms - 60_000) as f64),
        build: None,
    }));
    let m = e.shot(&data, stale).await;
    assert_eq!(m.runtime.source, "probe");
    assert_eq!(m.binding, Binding::Illustrative, "{}", m.binding_reason);
    assert!(m.binding_reason.contains("since the page was loaded"));
    // A page loaded after B started binds through the probe.
    let mut fresh = i.clone();
    fresh.document = doc(&i.url, None);
    let m = e.shot(&data, fresh).await;
    assert_eq!(m.binding, Binding::Bound, "{}", m.binding_reason);

    // Navigation during the capture.
    let before =
        json!({"href": i.url, "origin": format!("http://localhost:{port}"), "time_origin_ms": 1.0});
    let after = json!({"href": format!("{}other", i.url), "origin": format!("http://localhost:{port}"), "time_origin_ms": 2.0});
    let mut nav = i.clone();
    nav.document = DocumentCapture::from_reads(Some(&before), Some(&after));
    assert!(matches!(nav.document, Some(DocumentCapture::Changed(_))));
    let m = e.shot(&data, nav).await;
    assert_eq!(m.binding, Binding::Illustrative);
    assert!(
        m.runtime
            .detail
            .as_deref()
            .unwrap_or("")
            .contains("navigated")
    );
    assert!(m.document.is_none());

    // Different loopback addresses on one port: each origin is probed exactly. The current
    // build (B) serves on the alternative address, a stale one (A) on 127.0.0.1.
    let a_body = serde_json::to_string(&a).unwrap();
    for alt in ["127.0.0.2", "::1"] {
        let alt_ip: std::net::IpAddr = alt.parse().unwrap();
        let (l1, l2) = loop {
            let l1 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let p = l1.local_addr().unwrap().port();
            match tokio::net::TcpListener::bind((alt_ip, p)).await {
                Ok(l2) => break (Some(l1), Some(l2)),
                Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => continue,
                Err(_) => break (None, None), // address not configured here (127.0.0.2 on macOS)
            }
        };
        let (Some(l1), Some(l2)) = (l1, l2) else {
            assert!(
                !cfg!(target_os = "linux") || alt == "::1",
                "{alt} should exist on Linux"
            );
            continue;
        };
        let p = l1.local_addr().unwrap().port();
        let ab = a_body.clone();
        identity_server(l1, move |_| ab.clone()).await;
        let bb = b_body.clone();
        identity_server(l2, move |_| bb.clone()).await;
        e.put_preview(&format!("va{p}"), p, None);
        let host = if alt.contains(':') {
            format!("[{alt}]")
        } else {
            alt.to_string()
        };
        let mut j = i.clone();
        j.preview = None;
        j.url = format!("http://{host}:{p}/");
        j.document = doc(&j.url, None);
        let m = e.shot(&data, j.clone()).await;
        assert_eq!(
            m.runtime.head_sha, b.head_sha,
            "{alt}: probed the wrong address"
        );
        assert_eq!(m.binding, Binding::Bound, "{alt}: {}", m.binding_reason);
        j.url = format!("http://127.0.0.1:{p}/");
        j.document = doc(&j.url, None);
        let m = e.shot(&data, j).await;
        assert_eq!(m.runtime.head_sha, a.head_sha);
        assert_eq!(m.binding, Binding::Illustrative);
    }

    // Virtual hosts on one port: `app.localhost` is B, everything else A.
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let vport = l.local_addr().unwrap().port();
    let (ab, bb) = (a_body.clone(), b_body.clone());
    identity_server(l, move |host| {
        if host.starts_with("app.localhost:") {
            bb.clone()
        } else {
            ab.clone()
        }
    })
    .await;
    e.put_preview("vh", vport, None);
    let mut v = i.clone();
    v.preview = None;
    v.url = format!("http://app.localhost:{vport}/");
    v.document = doc(&v.url, None);
    let m = e.shot(&data, v.clone()).await;
    assert_eq!(m.runtime.head_sha, b.head_sha, "probe sent the wrong Host");
    assert_eq!(m.binding, Binding::Bound, "{}", m.binding_reason);
    v.url = format!("http://other.localhost:{vport}/");
    v.document = doc(&v.url, None);
    let m = e.shot(&data, v).await;
    assert_eq!(m.runtime.head_sha, a.head_sha);
    assert_eq!(m.binding, Binding::Illustrative);
}

#[tokio::test(flavor = "multi_thread")]
async fn retention_by_age_and_count_keeps_referenced_screenshots() {
    let e = Env::new();
    let now = vk_store::now_ms();
    let shared = png(2, 2, |_, _| [1, 2, 3, 255]);
    let mut ids = Vec::new();
    for n in 0..5u8 {
        if n == 1 {
            // Later screenshots show a newer revision than the accepted one.
            std::fs::write(e.repo.join("app.js"), "teal\n").unwrap();
            git(&e.repo, &["commit", "-qam", "teal"]);
        }
        let data = png(2, 2, move |_, _| [n, 0, 0, 255]);
        let m = e
            .shot(&data, e.inputs("pane-a", EnvKind::RemoteHeadless))
            .await;
        e.set_task(&m.id, "task-1");
        e.set_created(&m.id, now - (5 - n as i64) * 1000);
        ids.push(m);
    }
    // Two records sharing one blob, no task.
    let s1 = e
        .shot(&shared, e.inputs("pane-a", EnvKind::RemoteHeadless))
        .await;
    let s2 = e
        .shot(&shared, e.inputs("pane-a", EnvKind::RemoteHeadless))
        .await;
    e.set_created(&s1.id, now - 40 * DAY_MS);
    e.set_created(&s2.id, now);
    assert_eq!(s1.blob, s2.blob);

    // An acceptance of task-1 on the head the oldest screenshot shows references it.
    let head = ids[0].code.as_ref().unwrap().head_sha.clone().unwrap();
    {
        let acc = vk_review::readiness::ReviewAcceptance {
            id: "acc-1".into(),
            task_id: "task-1".into(),
            intent_revision: 1,
            package_revision: 1,
            subject_id: "subj".into(),
            head_sha: head,
            checks: vec![],
            exceptions: vec![],
            actor: vk_review::Actor::user("demo"),
            accepted_at_ms: now,
            idempotency_key: "k".into(),
        };
        let rec = crate::review::AcceptanceRec {
            acceptance: acc,
            outdated_at_ms: None,
            outdated_reasons: vec![],
        };
        let mut c = e.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.put("review_acceptance", "acc-1", None, &rec);
        e.server.commit(&mut c, tx).unwrap();
    }
    assert!(referenced_by_acceptance(
        &e.server,
        &find(&e.server, &ids[0].id).unwrap()
    ));

    // Count cap 2 per task: the two newest stay, the referenced oldest stays, the rest go.
    let cfg = ScreenshotConfig {
        keep_days: 30,
        max_per_task: 2,
        referenced_keep_days: 365,
        probe_build: false,
    };
    let r = cleanup_with(&e.server, now, &cfg, None);
    let mut removed = r.removed.clone();
    removed.sort();
    let mut want = vec![ids[1].id.clone(), ids[2].id.clone(), s1.id.clone()];
    want.sort();
    assert_eq!(removed, want);
    assert_eq!(r.kept_referenced, vec![ids[0].id.clone()]);
    // Blobs of removed records are gone; the shared blob survives (s2 still uses it).
    assert!(!ids[1].path(&e.server).exists());
    assert!(!ids[2].path(&e.server).exists());
    assert!(ids[0].path(&e.server).exists());
    assert!(s2.path(&e.server).exists());
    assert!(find(&e.server, &s1.id).is_none());
    assert_eq!(e.events("screenshot.deleted").len(), 1);

    // 60 days later: everything unreferenced expires; the referenced one is kept until
    // `referenced_keep_days`.
    let later = now + 60 * DAY_MS;
    let r = cleanup_with(&e.server, later, &cfg, None);
    assert!(r.removed.contains(&ids[3].id) && r.removed.contains(&s2.id));
    assert!(!r.removed.contains(&ids[0].id));
    assert!(!s2.path(&e.server).exists());
    let r = cleanup_with(&e.server, now + 400 * DAY_MS, &cfg, None);
    assert_eq!(r.removed, vec![ids[0].id.clone()]);
    assert!(load_all(&e.server).is_empty());

    // Explicit delete of a referenced screenshot needs force.
    let m = e
        .shot(&shared, e.inputs("pane-a", EnvKind::RemoteHeadless))
        .await;
    {
        // Pretend it shows the accepted revision.
        let mut r = find(&e.server, &m.id).unwrap();
        r.code.as_mut().unwrap().head_sha = ids[0].code.as_ref().unwrap().head_sha.clone();
        r.task = Some("task-1".into());
        r.created_at_ms = now - 1000;
        let mut c = e.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.put(KIND, &r.id, Some(&r.handle), &r);
        e.server.commit(&mut c, tx).unwrap();
    }
    let err = e
        .call(&ctx_full(), "screenshot.delete", json!({"id": m.handle}))
        .await
        .unwrap_err();
    assert_eq!(err.data.details["reason"], "referenced_by_acceptance");
    let ok = e
        .call(
            &ctx_full(),
            "screenshot.delete",
            json!({"id": m.handle, "force": true}),
        )
        .await
        .unwrap();
    assert_eq!(ok["deleted"], true);
    assert!(!m.path(&e.server).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn diff_of_two_screenshots_and_environment_guard() {
    let e = Env::new();
    let before = png(64, 32, |_, _| [255, 255, 255, 255]);
    let after = png(64, 32, |x, y| {
        if (10..20).contains(&x) && (5..10).contains(&y) {
            [0, 0, 0, 255]
        } else {
            [255, 255, 255, 255]
        }
    });
    let a = e
        .shot(&before, e.inputs("pane-a", EnvKind::RemoteHeadless))
        .await;
    let b = e
        .shot(&after, e.inputs("pane-a", EnvKind::RemoteHeadless))
        .await;
    let pane_shot = e.shot(&after, e.inputs("pane-a", EnvKind::LocalPane)).await;
    let r = e
        .call(
            &ctx_pane("pane-a"),
            "browser.diff",
            json!({"a": a.handle, "b": b.id, "inline": true}),
        )
        .await
        .unwrap();
    assert_eq!(r["changed_pixels"], 50);
    assert_eq!(r["total_pixels"], 64 * 32);
    assert_eq!(
        r["regions"][0],
        json!({"x": 10, "y": 5, "width": 10, "height": 5, "pixels": 50})
    );
    assert!((r["changed_ratio"].as_f64().unwrap() - 50.0 / 2048.0).abs() < 1e-9);
    let path = PathBuf::from(r["path_on_machine"].as_str().unwrap());
    assert!(path.exists());
    assert!(r["data_b64"].is_string());
    let side: Value =
        serde_json::from_slice(&std::fs::read(path.with_extension("json")).unwrap()).unwrap();
    assert_eq!(side["kind"], "screenshot_diff");
    // Headless vs browser pane: refused unless forced.
    let err = e
        .call(
            &ctx_full(),
            "browser.diff",
            json!({"a": a.id, "b": pane_shot.id}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.data.details["reason"], "environment_mismatch");
    let r = e
        .call(
            &ctx_full(),
            "browser.diff",
            json!({"a": a.id, "b": pane_shot.id, "force": true, "threshold": 0.5}),
        )
        .await
        .unwrap();
    assert_eq!(r["forced"], true);
    assert_eq!(r["changed_pixels"], 50);
    // Raw blob hashes work for the user, not for panes.
    let r = e
        .call(
            &ctx_full(),
            "browser.diff",
            json!({"a": a.blob, "b": b.blob}),
        )
        .await
        .unwrap();
    assert_eq!(r["changed_pixels"], 50);
    let err = e
        .call(
            &ctx_pane("pane-a"),
            "browser.diff",
            json!({"a": a.blob, "b": b.blob}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.data.kind, "not_found");
    // Diff blobs expire with keep_days.
    let cfg = ScreenshotConfig::default();
    let r = cleanup_with(&e.server, vk_store::now_ms() + 31 * DAY_MS, &cfg, None);
    assert!(r.diff_blobs_removed >= 1);
    assert!(!path.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn pane_scope_sees_only_its_workspace() {
    let e = Env::new();
    e.put_pane("pane-a2", "ws-a", None);
    let data = png(2, 2, |_, _| [9, 9, 9, 255]);
    let a = e
        .shot(&data, e.inputs("pane-a", EnvKind::RemoteHeadless))
        .await;
    let b = e
        .shot(&data, e.inputs("pane-b", EnvKind::RemoteHeadless))
        .await;

    // The user sees both.
    let all = e
        .call(&ctx_full(), "screenshot.list", json!({}))
        .await
        .unwrap();
    assert_eq!(all["count"], 2);
    // A pane sees its workspace's screenshots (also those of its sibling pane).
    for p in ["pane-a", "pane-a2"] {
        let l = e
            .call(&ctx_pane(p), "screenshot.list", json!({}))
            .await
            .unwrap();
        let ids: Vec<&str> = l["screenshots"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec![a.id.as_str()], "{p}");
    }
    let got = e
        .call(
            &ctx_pane("pane-a"),
            "screenshot.get",
            json!({"id": a.handle}),
        )
        .await
        .unwrap();
    assert_eq!(got["id"], a.id);
    assert_eq!(got["exists"], true);
    let err = e
        .call(&ctx_pane("pane-a"), "screenshot.get", json!({"id": b.id}))
        .await
        .unwrap_err();
    assert_eq!(err.data.kind, "not_found");
    let err = e
        .call(
            &ctx_pane("pane-a"),
            "screenshot.open",
            json!({"id": b.handle}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.data.kind, "not_found");
    let err = e
        .call(
            &ctx_pane("pane-a"),
            "browser.diff",
            json!({"a": a.id, "b": b.id}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.data.kind, "not_found");
    // Agents can't delete evidence.
    let err = e
        .call(
            &ctx_pane("pane-a"),
            "screenshot.delete",
            json!({"id": a.id}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.data.kind, "permission_denied");
    // `open` inlines the image.
    let o = e
        .call(&ctx_full(), "screenshot.open", json!({"id": a.id}))
        .await
        .unwrap();
    assert!(o["data_b64"].is_string());
    // Filters.
    let l = e
        .call(
            &ctx_full(),
            "screenshot.list",
            json!({"since": vk_store::now_ms() + 1000}),
        )
        .await
        .unwrap();
    assert_eq!(l["count"], 0);
    let l = e
        .call(
            &ctx_full(),
            "screenshot.list",
            json!({"since": "5m", "limit": 1}),
        )
        .await
        .unwrap();
    assert_eq!(
        (l["count"].as_u64(), l["total"].as_u64()),
        (Some(1), Some(2))
    );
    assert_eq!(l["screenshots"][0]["id"], b.id);
}
