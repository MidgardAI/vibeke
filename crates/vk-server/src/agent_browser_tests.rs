//! In-process tests for the agents' headless browser API with a fake CDP browser
//! (`vk_browser::fake`): session ownership under pane scope, destination denials, the Fetch
//! layer, console/network capture, screenshots as blobs, `human_control`, screencast attach.

use super::*;
use crate::ServerOpts;
use crate::paths::Paths;
use std::sync::Once;
use vk_browser::fake::FakeBrowser;
use vk_proto::model::{Pane, Preview, PreviewSource};

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let base =
            std::env::temp_dir().join(format!("vk-agent-browser-tests-{}", std::process::id()));
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

struct Env {
    _dir: tempfile::TempDir,
    server: Arc<Server>,
    fake: Arc<Mutex<Option<Arc<FakeState>>>>,
}

/// What the test keeps of the fake browser after the launcher handed it to the server.
struct FakeState {
    calls: Arc<Mutex<Vec<vk_browser::fake::Call>>>,
    emitter: vk_browser::fake::Emitter,
}

impl FakeState {
    fn calls_of(&self, m: &str) -> Vec<vk_browser::fake::Call> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.0 == m)
            .cloned()
            .collect()
    }
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
        let fake: Arc<Mutex<Option<Arc<FakeState>>>> = Arc::default();
        let slot = fake.clone();
        server
            .agent_browser
            .set_launcher(Arc::new(move |ctx: &LaunchCtx| {
                assert!(ctx.profile_dir.ends_with("agent-browser/profile"));
                let mut f = FakeBrowser::start();
                let events = f.take_events();
                *slot.lock().unwrap() = Some(Arc::new(FakeState {
                    calls: f.calls.clone(),
                    emitter: f.emitter(),
                }));
                let cdp = f.cdp.clone();
                let holder = Mutex::new(Some(f));
                Ok(Launched {
                    cdp,
                    events,
                    pid: None,
                    product: "HeadlessChrome/0.0-fake".into(),
                    binary: "/fake/chrome-headless-shell".into(),
                    kind: "fake".into(),
                    stop: Box::new(move || {
                        holder.lock().unwrap().take();
                    }),
                })
            }));
        let e = Env {
            _dir: dir,
            server,
            fake,
        };
        e.put_pane("pane-a", "user");
        e.put_pane("pane-b", "user");
        e.put_pane("pane-c", "agent:pane-a");
        e.put_preview("v1", 5173);
        e
    }

    fn fake(&self) -> Arc<FakeState> {
        self.fake.lock().unwrap().clone().expect("browser launched")
    }

    fn put_pane(&self, id: &str, created_by: &str) {
        let p = Pane {
            id: id.into(),
            handle: id.into(),
            tab: "tab".into(),
            workspace: "ws".into(),
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
            created_by: created_by.into(),
            recovered: None,
            isolation: Default::default(),
            browser: None,
        };
        let mut c = self.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.pane(p);
        self.server.commit(&mut c, tx).unwrap();
    }

    fn put_preview(&self, handle: &str, port: u16) {
        let p = Preview {
            id: ulid(),
            handle: handle.into(),
            machine: "testbox".into(),
            pane: Some("pane-a".into()),
            task: None,
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

    async fn call(&self, ctx: &Ctx, method: &str, p: Value) -> R {
        crate::api::authorize(&self.server, ctx, method, &p)?;
        api(&self.server, ctx, method, &p)
            .await
            .expect("browser.* method")
    }

    fn events(&self, kind: &str) -> Vec<vk_store::Event> {
        self.server.with_core(|c| {
            c.store
                .events_after(0, 10_000, &[kind.to_string()])
                .unwrap()
        })
    }
}

async fn until<F: Fn() -> bool>(what: &str, f: F) {
    let t = Instant::now();
    while !f() {
        assert!(
            t.elapsed() < Duration::from_secs(8),
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn kind(e: &RpcError) -> &str {
    &e.data.kind
}

#[tokio::test(flavor = "multi_thread")]
async fn open_preview_from_pane_and_ownership() {
    let e = Env::new();
    let a = ctx_pane("pane-a");
    let r = e
        .call(&a, "browser.open", json!({"preview": "v1"}))
        .await
        .unwrap();
    assert_eq!(r["session"], "b1");
    assert_eq!(r["preview"], "v1");
    assert_eq!(r["owner"]["pane"], "pane-a");
    assert_eq!(r["environment"]["kind"], "remote_headless");
    assert_eq!(r["environment"]["machine"], "testbox");
    let f = e.fake();
    // The context uses the session's own filtering proxy, loopback included.
    let ctxc = f.calls_of("Target.createBrowserContext");
    assert_eq!(ctxc.len(), 1);
    let proxy = ctxc[0].1["proxyServer"].as_str().unwrap().to_string();
    assert!(proxy.starts_with("http://127.0.0.1:"), "{proxy}");
    assert_eq!(ctxc[0].1["proxyBypassList"], "<-loopback>");
    assert_eq!(
        r["proxy_port"].as_u64().unwrap().to_string(),
        proxy.rsplit(':').next().unwrap()
    );
    let page = f.calls_of("Target.attachToTarget")[0].clone();
    let _ = page;
    let fetch = f.calls_of("Fetch.enable");
    assert_eq!(fetch[0].1["patterns"][0]["urlPattern"], "*");
    assert_eq!(
        f.calls_of("Browser.setDownloadBehavior")[0].1["behavior"],
        "deny"
    );
    assert_eq!(
        f.calls_of("Page.navigate")[0].1["url"],
        "http://localhost:5173/"
    );

    // Another agent can't see or drive it.
    let bctx = ctx_pane("pane-b");
    let err = e
        .call(
            &bctx,
            "browser.navigate",
            json!({"session": "b1", "url": "http://localhost:5173/x"}),
        )
        .await
        .unwrap_err();
    assert_eq!(kind(&err), "not_found");
    assert_eq!(
        e.call(&bctx, "browser.list", json!({})).await.unwrap()["sessions"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    // A session opened by a pane the owner created is the owner's too.
    let c = ctx_pane("pane-c");
    let rc = e.call(&c, "browser.open", json!({})).await.unwrap();
    assert_eq!(rc["session"], "b2");
    let la = e.call(&a, "browser.list", json!({})).await.unwrap();
    assert_eq!(la["sessions"].as_array().unwrap().len(), 2);
    assert!(
        e.call(&a, "browser.console", json!({"session": "b2"}))
            .await
            .is_ok()
    );
    // ... but not the other way round.
    assert!(
        e.call(&c, "browser.console", json!({"session": "b1"}))
            .await
            .is_err()
    );
    // The user sees everything.
    let lu = e
        .call(&ctx_full(), "browser.list", json!({}))
        .await
        .unwrap();
    assert_eq!(lu["sessions"].as_array().unwrap().len(), 2);
    assert_eq!(lu["browser"]["running"], true);
    // Remote previews must be opened on their machine.
    let err = e
        .call(&a, "browser.open", json!({"preview": "devbox/v4"}))
        .await
        .unwrap_err();
    assert!(err.message.contains("--machine"), "{err}");
    assert_eq!(e.events("browser.session_opened").len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn destinations_are_denied_with_reasons() {
    let e = Env::new();
    let a = ctx_pane("pane-a");
    for (url, reason) in [
        ("http://127.0.0.1:5432/", "loopback_port_not_a_preview"),
        (
            "http://169.254.169.254/latest/meta-data/",
            "metadata_address",
        ),
        ("http://10.0.0.8/", "private_address"),
        ("file:///etc/passwd", "scheme_not_allowed"),
        ("chrome://settings", "scheme_not_allowed"),
    ] {
        let err = e
            .call(&a, "browser.open", json!({"url": url}))
            .await
            .unwrap_err();
        assert_eq!(kind(&err), "destination_denied", "{url}");
        assert_eq!(err.data.details["reason"], reason, "{url}");
    }
    // Nothing was started for refused opens.
    assert!(e.fake.lock().unwrap().is_none());
    let r = e
        .call(&a, "browser.open", json!({"url": "http://localhost:5173/"}))
        .await
        .unwrap();
    let sid = r["session"].as_str().unwrap().to_string();
    let err = e
        .call(
            &a,
            "browser.navigate",
            json!({"session": sid, "url": "http://localhost:6000/"}),
        )
        .await
        .unwrap_err();
    assert_eq!(kind(&err), "destination_denied");
    let net = e
        .call(
            &a,
            "browser.network",
            json!({"session": sid, "failed_only": true}),
        )
        .await
        .unwrap();
    let entries = net["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0]["blocked_by_policy"],
        "loopback_port_not_a_preview"
    );
    let ev = e.events("browser.request_denied");
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].data["url"], "http://localhost:6000/");
    // A relative path stays on the current origin.
    let ok = e
        .call(
            &a,
            "browser.navigate",
            json!({"session": sid, "path": "/settings"}),
        )
        .await
        .unwrap();
    assert_eq!(ok["session"], sid.as_str());
    assert_eq!(
        e.fake().calls_of("Page.navigate").last().unwrap().1["url"],
        "http://localhost:5173/settings"
    );
    // Declaring a preview makes its port reachable immediately.
    e.put_preview("v2", 6001);
    assert!(
        e.call(
            &a,
            "browser.navigate",
            json!({"session": sid, "url": "http://localhost:6001/"})
        )
        .await
        .is_ok()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_layer_blocks_schemes_and_internal_addresses() {
    let e = Env::new();
    let a = ctx_pane("pane-a");
    e.call(&a, "browser.open", json!({})).await.unwrap();
    let f = e.fake();
    let page = f.calls_of("Target.attachToTarget")[0].clone();
    let _ = page;
    let sess = e.server.agent_browser.session("b1").unwrap();
    let cs = sess.cdp_session.clone();
    let paused = |id: &str, url: &str, rtype: &str, frame: &str| {
        f.emitter.emit(
            "Fetch.requestPaused",
            json!({"requestId": id, "request": {"url": url, "method": "GET"}, "resourceType": rtype, "frameId": frame}),
            Some(&cs),
        );
    };
    let main = sess.target_id.clone();
    paused("r1", "file:///etc/hosts", "Document", &main);
    paused("r2", "http://169.254.169.254/latest", "Image", &main);
    paused("r3", "http://localhost:5173/app.js", "Script", &main);
    paused("r4", "ws://127.0.0.1:6001/hmr", "WebSocket", &main);
    paused("r5", "https://cdn.example.com/font.woff2", "Font", &main);
    paused("r6", "data:text/plain,hi", "Image", &main);
    until("six decisions", || {
        f.calls_of("Fetch.failRequest").len() + f.calls_of("Fetch.continueRequest").len() >= 6
    })
    .await;
    let failed: Vec<String> = f
        .calls_of("Fetch.failRequest")
        .iter()
        .map(|c| c.1["requestId"].as_str().unwrap().to_string())
        .collect();
    let cont: Vec<String> = f
        .calls_of("Fetch.continueRequest")
        .iter()
        .map(|c| c.1["requestId"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(failed.len(), 3, "{failed:?}");
    for r in ["r1", "r2", "r4"] {
        assert!(failed.contains(&r.to_string()), "{r} should be blocked");
    }
    // Names are left to the proxy (which resolves and pins); local schemes pass.
    for r in ["r3", "r5", "r6"] {
        assert!(cont.contains(&r.to_string()), "{r} should continue");
    }
    assert!(f.calls_of("Fetch.failRequest").iter().all(|c| c.2.as_deref() == Some(cs.as_str())
        && c.1["errorReason"] == "BlockedByClient"));
    // Auto-attached iframes/workers of this context get the same interception.
    f.emitter.emit(
        "Target.attachedToTarget",
        json!({"sessionId": "child-1", "targetInfo": {"type": "iframe", "browserContextId": sess.context_id}, "waitingForDebugger": true}),
        Some(&cs),
    );
    f.emitter.emit(
        "Target.attachedToTarget",
        json!({"sessionId": "stranger", "targetInfo": {"type": "page", "browserContextId": "other"}, "waitingForDebugger": true}),
        Some(&cs),
    );
    until("child setup", || {
        f.calls_of("Runtime.runIfWaitingForDebugger").len() >= 2
    })
    .await;
    assert!(
        f.calls_of("Fetch.enable")
            .iter()
            .any(|c| c.2.as_deref() == Some("child-1"))
    );
    assert!(
        !f.calls_of("Fetch.enable")
            .iter()
            .any(|c| c.2.as_deref() == Some("stranger"))
    );
    assert_eq!(
        f.calls_of("Target.detachFromTarget")[0].1["sessionId"],
        "stranger"
    );
    // Requests from the child are decided too.
    f.emitter.emit(
        "Fetch.requestPaused",
        json!({"requestId": "c1", "request": {"url": "http://127.0.0.1:22/"}, "resourceType": "XHR", "frameId": "f2"}),
        Some("child-1"),
    );
    until("child decision", || {
        f.calls_of("Fetch.failRequest")
            .iter()
            .any(|c| c.1["requestId"] == "c1")
    })
    .await;
    // The entry is recorded right after the decision goes out: poll for it rather than racing.
    let t = Instant::now();
    loop {
        let net = e
            .call(&a, "browser.network", json!({"session": "b1"}))
            .await
            .unwrap();
        let blocked = net["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|x| !x["blocked_by_policy"].is_null())
            .count();
        if blocked == 4 && e.events("browser.request_denied").len() == 4 {
            break;
        }
        assert!(
            blocked <= 4 && t.elapsed() < Duration::from_secs(8),
            "blocked entries: {blocked}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn console_and_network_capture() {
    let e = Env::new();
    let a = ctx_pane("pane-a");
    e.call(&a, "browser.open", json!({})).await.unwrap();
    let f = e.fake();
    let sess = e.server.agent_browser.session("b1").unwrap();
    let cs = Some(sess.cdp_session.as_str());
    f.emitter.emit(
        "Runtime.consoleAPICalled",
        json!({"type": "log", "args": [{"type": "string", "value": "hello"}, {"type": "number", "value": 42}]}),
        cs,
    );
    f.emitter.emit(
        "Runtime.exceptionThrown",
        json!({"exceptionDetails": {"text": "Uncaught", "exception": {"description": "TypeError: x is undefined"}, "url": "http://localhost:5173/app.js", "lineNumber": 3}}),
        cs,
    );
    f.emitter.emit(
        "Network.requestWillBeSent",
        json!({"requestId": "n1", "request": {"url": "http://localhost:5173/api", "method": "POST"}, "type": "Fetch", "frameId": "x"}),
        cs,
    );
    f.emitter.emit(
        "Network.responseReceived",
        json!({"requestId": "n1", "response": {"status": 404, "mimeType": "application/json"}, "type": "Fetch", "frameId": "x"}),
        cs,
    );
    f.emitter
        .emit("Network.loadingFinished", json!({"requestId": "n1"}), cs);
    f.emitter.emit(
        "Network.requestWillBeSent",
        json!({"requestId": "n2", "request": {"url": "http://localhost:5173/ok.css", "method": "GET"}, "type": "Stylesheet", "frameId": "x"}),
        cs,
    );
    f.emitter.emit(
        "Network.responseReceived",
        json!({"requestId": "n2", "response": {"status": 200}, "type": "Stylesheet", "frameId": "x"}),
        cs,
    );
    f.emitter
        .emit("Network.loadingFinished", json!({"requestId": "n2"}), cs);
    until("captured", || {
        sess.network.lock().unwrap().len() == 2 && sess.console.lock().unwrap().len() == 2
    })
    .await;
    let all = e
        .call(&a, "browser.console", json!({"session": "b1"}))
        .await
        .unwrap();
    assert_eq!(all["entries"][0]["text"], "hello 42");
    let errs = e
        .call(
            &a,
            "browser.console",
            json!({"session": "b1", "level": "error"}),
        )
        .await
        .unwrap();
    let errs = errs["entries"].as_array().unwrap();
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0]["text"], "TypeError: x is undefined");
    assert_eq!(errs[0]["source"], "exception");
    let failed = e
        .call(
            &a,
            "browser.network",
            json!({"session": "b1", "failed_only": true}),
        )
        .await
        .unwrap();
    let failed = failed["entries"].as_array().unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["status"], 404);
    assert_eq!(failed[0]["method"], "POST");
    // The ring is bounded.
    for i in 0..(RING + 20) {
        Session::push_ring(&sess.console, json!({"ts": i, "level": "log", "text": i}));
    }
    assert_eq!(sess.console.lock().unwrap().len(), RING);
}

#[tokio::test(flavor = "multi_thread")]
async fn human_control_blocks_the_agent_until_released() {
    let e = Env::new();
    let a = ctx_pane("pane-a");
    let user = ctx_full();
    e.call(&a, "browser.open", json!({})).await.unwrap();
    // Agents can't take over (or watch) themselves.
    for m in [
        "browser.take_over",
        "browser.attach_screencast",
        "browser.install",
    ] {
        let err = e.call(&a, m, json!({"session": "b1"})).await.unwrap_err();
        assert_eq!(kind(&err), "permission_denied", "{m}");
    }
    let r = e
        .call(&user, "browser.take_over", json!({"session": "b1"}))
        .await
        .unwrap();
    assert_eq!(r["human_control"], true);
    let err = e
        .call(
            &a,
            "browser.click",
            json!({"session": "b1", "x": 10, "y": 10}),
        )
        .await
        .unwrap_err();
    assert_eq!(kind(&err), "human_control");
    assert!(err.data.retryable);
    let err = e
        .call(&a, "browser.screenshot", json!({"session": "b1"}))
        .await
        .unwrap_err();
    assert_eq!(kind(&err), "human_control");
    // The human keeps working.
    e.call(
        &user,
        "browser.click",
        json!({"session": "b1", "x": 10, "y": 10}),
    )
    .await
    .unwrap();
    assert_eq!(
        e.call(&user, "browser.list", json!({})).await.unwrap()["sessions"][0]["human_control"],
        true
    );
    e.call(&user, "browser.release", json!({"session": "b1"}))
        .await
        .unwrap();
    let r = e
        .call(
            &a,
            "browser.click",
            json!({"session": "b1", "x": 5, "y": 6}),
        )
        .await
        .unwrap();
    assert_eq!(r["x"], 5.0);
    let presses = e.fake().calls_of("Input.dispatchMouseEvent");
    assert_eq!(presses.len(), 6);
    assert_eq!(e.events("browser.taken_over").len(), 1);
    assert_eq!(e.events("browser.released").len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn screenshot_is_a_blob_with_metadata() {
    let e = Env::new();
    let a = ctx_pane("pane-a");
    e.call(&a, "browser.open", json!({"preview": "v1"}))
        .await
        .unwrap();
    let r = e
        .call(
            &a,
            "browser.screenshot",
            json!({"session": "b1", "inline": true}),
        )
        .await
        .unwrap();
    let path = PathBuf::from(r["path_on_machine"].as_str().unwrap());
    assert!(path.starts_with(e.server.paths.blobs()));
    assert_eq!(std::fs::read(&path).unwrap(), vk_browser::fake::PNG_1X1);
    assert_eq!(
        r["blob"],
        blake3::hash(vk_browser::fake::PNG_1X1).to_hex().to_string()
    );
    assert_eq!(
        (r["width"].as_u64(), r["height"].as_u64()),
        (Some(1), Some(1))
    );
    let meta = &r["meta"];
    assert_eq!(meta["environment"]["kind"], "remote_headless");
    assert_eq!(meta["environment"]["machine"], "testbox");
    assert_eq!(meta["environment"]["fresh_context"], true);
    assert_eq!(meta["taken_by"]["kind"], "agent");
    assert_eq!(meta["preview"], "v1");
    assert!(meta["taken_at"].as_i64().unwrap() > 0);
    assert!(r["data_b64"].as_str().is_some());
    // Stage 4: a `screenshot` record with code state (none here: the pane has no checkout),
    // running-build identity (unknown) and binding.
    assert_eq!(r["handle"], "s1");
    assert_eq!(meta["binding"], "illustrative");
    assert_eq!(meta["code"], Value::Null);
    assert!(meta["code_note"].is_string());
    assert_eq!(meta["runtime"]["status"], "unknown");
    assert_eq!(meta["session"], "b1");
    assert_eq!(meta["label"], "testbox · headless · fresh context");
    assert_eq!(meta["environment"]["browser_version"], "0.0-fake", "{meta}");
    let rec = crate::screenshots::find(&e.server, r["id"].as_str().unwrap()).unwrap();
    assert_eq!(rec.blob, r["blob"]);
    assert_eq!(e.events("screenshot.captured").len(), 1);
    let sidecar = path.with_extension("json");
    let saved: Value = serde_json::from_slice(&std::fs::read(sidecar).unwrap()).unwrap();
    assert_eq!(saved["environment"]["kind"], "remote_headless");
    // Full page uses the layout size.
    e.call(
        &a,
        "browser.screenshot",
        json!({"session": "b1", "full_page": true}),
    )
    .await
    .unwrap();
    let shot = e.fake().calls_of("Page.captureScreenshot");
    assert_eq!(shot.last().unwrap().1["captureBeyondViewport"], true);
    assert_eq!(e.events("browser.screenshot").len(), 2);
    // Snapshot from the accessibility tree.
    let snap = e
        .call(&a, "browser.snapshot", json!({"session": "b1"}))
        .await
        .unwrap();
    assert_eq!(
        snap["content"],
        "RootWebArea \"Fake page\"\n  button \"Save\"\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn eval_needs_the_script_capability_from_a_pane() {
    let e = Env::new();
    let a = ctx_pane("pane-a");
    e.call(&a, "browser.open", json!({})).await.unwrap();
    let err = e
        .call(
            &a,
            "browser.eval",
            json!({"session": "b1", "expression": "1+1"}),
        )
        .await
        .unwrap_err();
    assert_eq!(kind(&err), "permission_denied");
    assert_eq!(err.data.details["capability"], "browser.script");
    let r = e
        .call(
            &ctx_full(),
            "browser.eval",
            json!({"session": "b1", "expression": "document.readyState"}),
        )
        .await
        .unwrap();
    assert_eq!(r["value"], "complete");
    // install without confirm only plans (no download in tests).
    let plan = e.call(&ctx_full(), "browser.install", json!({})).await;
    if vk_browser::install::platform().is_some() {
        let plan = plan.unwrap();
        assert_eq!(plan["confirm_required"], true);
        assert!(
            plan["plan"]["url"]
                .as_str()
                .unwrap()
                .starts_with("https://")
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_close_explicitly_and_with_their_pane() {
    let e = Env::new();
    let a = ctx_pane("pane-a");
    e.call(&a, "browser.open", json!({})).await.unwrap();
    e.call(&a, "browser.open", json!({})).await.unwrap();
    let r = e
        .call(&a, "browser.close", json!({"session": "b1"}))
        .await
        .unwrap();
    assert_eq!(r["closed"], true);
    assert_eq!(e.fake().calls_of("Target.disposeBrowserContext").len(), 1);
    assert!(e.server.agent_browser.session("b1").is_none());
    assert_eq!(
        kind(
            &e.call(&a, "browser.console", json!({"session": "b1"}))
                .await
                .unwrap_err()
        ),
        "not_found"
    );
    // Closing the owner pane closes its sessions (idle loop, every 2 s).
    e.server
        .with_core(|c| c.model.panes.retain(|p| p.id != "pane-a"));
    // The event is committed after the session leaves the map: wait for the event.
    until("owner gone → closed", || {
        e.events("browser.session_closed").len() == 2
    })
    .await;
    // The session leaves the table before its close event is committed.
    until("close event recorded", || {
        e.events("browser.session_closed").len() == 2
    })
    .await;
    let closed = e.events("browser.session_closed");
    assert_eq!(closed.len(), 2);
    assert_eq!(closed[1].data["reason"], "owner_pane_closed");
}

#[tokio::test(flavor = "multi_thread")]
async fn screencast_attach_delivers_latest_frames() {
    use base64::Engine as _;
    let e = Env::new();
    let user = ctx_full();
    e.call(&ctx_pane("pane-a"), "browser.open", json!({}))
        .await
        .unwrap();
    let mut sub = e
        .server
        .agent_browser
        .attach_screencast("b1")
        .await
        .unwrap();
    let f = e.fake();
    assert_eq!(f.calls_of("Page.startScreencast").len(), 1);
    let sess = e.server.agent_browser.session("b1").unwrap();
    let data = base64::engine::general_purpose::STANDARD.encode(b"\xff\xd8jpeg");
    f.emitter.emit(
        "Page.screencastFrame",
        json!({"sessionId": 7, "data": data, "metadata": {"deviceWidth": 1440, "deviceHeight": 900}}),
        Some(&sess.cdp_session),
    );
    tokio::time::timeout(Duration::from_secs(5), sub.frames.changed())
        .await
        .unwrap()
        .unwrap();
    let frame = sub.frames.borrow().clone().unwrap();
    assert_eq!(frame.seq, 1);
    assert_eq!(frame.data, b"\xff\xd8jpeg");
    assert_eq!(frame.width, Some(1440));
    until("ack", || !f.calls_of("Page.screencastFrameAck").is_empty()).await;
    assert_eq!(f.calls_of("Page.screencastFrameAck")[0].1["sessionId"], 7);
    // The JSON-RPC view polls the latest frame.
    let r = e
        .call(&user, "browser.screencast_frame", json!({"session": "b1"}))
        .await
        .unwrap();
    assert_eq!(r["seq"], 1);
    assert!(r["data_b64"].is_string());
    let r = e
        .call(
            &user,
            "browser.screencast_frame",
            json!({"session": "b1", "after_seq": 1}),
        )
        .await
        .unwrap();
    assert!(r["data_b64"].is_null());
    drop(sub);
    until("stop", || !f.calls_of("Page.stopScreencast").is_empty()).await;
}

/// Codex review finding 11: a client that attached a screencast and then went away without
/// detaching must not keep the session alive. The connection's end drops its subscriptions;
/// the idle collector then closes the session. (A take-over outlives the CLI call that made
/// it, so it is released explicitly here.)
#[tokio::test(flavor = "multi_thread")]
async fn disconnected_screencast_client_releases_the_session() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let e = Env::new();
    e.call(&ctx_pane("pane-a"), "browser.open", json!({}))
        .await
        .unwrap();
    let (client, server_end) = tokio::io::duplex(64 * 1024);
    let conn = tokio::spawn(crate::run::connection(e.server.clone(), server_end, None));
    let (rd, mut wr) = tokio::io::split(client);
    let mut rd = tokio::io::BufReader::new(rd);
    let mut line = String::new();
    for (id, method, params) in [
        (
            1,
            "client.hello",
            json!({"client_id": "c-crashy", "kind": "cli"}),
        ),
        (2, "browser.attach_screencast", json!({"session": "b1"})),
    ] {
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        wr.write_all(format!("{req}\n").as_bytes()).await.unwrap();
        loop {
            line.clear();
            rd.read_line(&mut line).await.unwrap();
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == id {
                assert!(v.get("error").is_none(), "{method}: {v}");
                break;
            }
        }
    }
    let sess = e.server.agent_browser.session("b1").unwrap();
    assert_eq!(sess.screencast_subs.load(Ordering::SeqCst), 1);
    // Long unused: only the subscription keeps it open.
    *sess.last_used.lock().unwrap() = Instant::now() - Duration::from_secs(24 * 3600);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(e.server.agent_browser.session("b1").is_some());
    // The client crashes (connection drops without detach/release).
    drop(wr);
    drop(rd);
    tokio::time::timeout(Duration::from_secs(5), conn)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(sess.screencast_subs.load(Ordering::SeqCst), 0);
    // stopScreencast is sent asynchronously after the subscription is dropped.
    until("screencast stopped", || {
        !e.fake().calls_of("Page.stopScreencast").is_empty()
    })
    .await;
    *sess.last_used.lock().unwrap() = Instant::now() - Duration::from_secs(24 * 3600);
    until("idle collector closes the session", || {
        !e.events("browser.session_closed").is_empty()
    })
    .await;
    assert!(e.server.agent_browser.session("b1").is_none());
    let closed = e.events("browser.session_closed");
    assert_eq!(closed.last().unwrap().data["reason"], "idle");
}

// ---- device presets, one-shot screenshots, console errors, preview scope --------------------

#[tokio::test(flavor = "multi_thread")]
async fn device_preset_emulates_and_is_recorded() {
    let e = Env::new();
    let a = ctx_pane("pane-a");
    // Unknown presets are refused before anything starts, naming the known ones.
    let err = e
        .call(
            &a,
            "browser.open",
            json!({"preview": "v1", "device": "nokia"}),
        )
        .await
        .unwrap_err();
    assert_eq!(kind(&err), "invalid_params");
    assert!(err.message.contains("iphone-15"), "{err}");
    assert!(e.fake.lock().unwrap().is_none());

    let r = e
        .call(
            &a,
            "browser.open",
            json!({"preview": "v1", "device": "iphone-15"}),
        )
        .await
        .unwrap();
    assert_eq!(r["device"], "iphone-15");
    assert_eq!(r["viewport"], json!({"width": 393, "height": 852}));
    assert_eq!(r["environment"]["device"], "iphone-15");
    let f = e.fake();
    let m = f.calls_of("Emulation.setDeviceMetricsOverride");
    assert_eq!(
        (m[0].1["width"].as_u64(), m[0].1["height"].as_u64()),
        (Some(393), Some(852))
    );
    assert_eq!(m[0].1["deviceScaleFactor"], 3.0);
    assert_eq!(m[0].1["mobile"], true);
    assert!(
        f.calls_of("Emulation.setUserAgentOverride")[0].1["userAgent"]
            .as_str()
            .unwrap()
            .contains("iPhone")
    );
    assert_eq!(
        f.calls_of("Emulation.setTouchEmulationEnabled")[0].1["enabled"],
        true
    );
    let shot = e
        .call(&a, "browser.screenshot", json!({"session": "b1"}))
        .await
        .unwrap();
    let env = &shot["meta"]["environment"];
    assert_eq!(env["device"], "iphone-15");
    assert_eq!(env["dpr"], 3.0);
    assert_eq!(env["viewport"]["width"], 393);
    assert!(
        shot["meta"]["label"]
            .as_str()
            .unwrap()
            .contains("iphone-15")
    );

    // Explicit viewport/dpr override the preset's size and ratio; desktop presets aren't mobile.
    let r2 = e
        .call(
            &a,
            "browser.open",
            json!({"device": "desktop-1280", "viewport": "800x600", "dpr": 2}),
        )
        .await
        .unwrap();
    assert_eq!(r2["viewport"], json!({"width": 800, "height": 600}));
    let m = f.calls_of("Emulation.setDeviceMetricsOverride");
    assert_eq!(m[1].1["deviceScaleFactor"], 2.0);
    assert_eq!(m[1].1["mobile"], false);
    assert_eq!(f.calls_of("Emulation.setTouchEmulationEnabled").len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn one_shot_screenshot_needs_no_session() {
    let e = Env::new();
    let a = ctx_pane("pane-a");
    let r = e
        .call(
            &a,
            "browser.screenshot",
            json!({"preview": "v1", "device": "pixel-8", "full_page": true, "inline": true}),
        )
        .await
        .unwrap();
    assert_eq!(r["one_shot"], true);
    assert_eq!(r["session"], Value::Null);
    assert_eq!(r["meta"]["environment"]["device"], "pixel-8");
    assert_eq!(r["meta"]["preview"], "v1");
    assert_eq!(r["meta"]["taken_by"]["kind"], "agent");
    assert!(r["data_b64"].as_str().is_some());
    let f = e.fake();
    assert_eq!(
        f.calls_of("Page.navigate")[0].1["url"],
        "http://localhost:5173/"
    );
    assert_eq!(
        f.calls_of("Page.captureScreenshot").last().unwrap().1["captureBeyondViewport"],
        true
    );
    // The context is gone again.
    assert_eq!(f.calls_of("Target.disposeBrowserContext").len(), 1);
    let l = e.call(&a, "browser.list", json!({})).await.unwrap();
    assert_eq!(l["sessions"].as_array().unwrap().len(), 0);
    assert_eq!(e.events("browser.session_closed").len(), 1);
    assert_eq!(e.events("screenshot.captured").len(), 1);

    // Same destination policy as browser.open; nothing to capture is an error.
    let err = e
        .call(
            &a,
            "browser.screenshot",
            json!({"url": "http://127.0.0.1:5432/"}),
        )
        .await
        .unwrap_err();
    assert_eq!(kind(&err), "destination_denied");
    let err = e
        .call(
            &a,
            "browser.screenshot",
            json!({"url": "file:///etc/passwd"}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.data.details["reason"], "scheme_not_allowed");
    let err = e
        .call(&a, "browser.screenshot", json!({"full_page": true}))
        .await
        .unwrap_err();
    assert_eq!(kind(&err), "invalid_params");
    // The user can do it too.
    let u = e
        .call(&ctx_full(), "browser.screenshot", json!({"preview": "v1"}))
        .await
        .unwrap();
    assert_eq!(u["meta"]["taken_by"]["kind"], "user");
}

#[tokio::test(flavor = "multi_thread")]
async fn console_errors_become_rate_limited_redacted_events() {
    let e = Env::new();
    let a = ctx_pane("pane-a");
    e.call(&a, "browser.open", json!({"preview": "v1"}))
        .await
        .unwrap();
    let f = e.fake();
    let sess = e.server.agent_browser.session("b1").unwrap();
    let cs = Some(sess.cdp_session.as_str());
    // Plain logs and warnings don't count.
    f.emitter.emit(
        "Runtime.consoleAPICalled",
        json!({"type": "warning", "args": [{"value": "careful"}]}),
        cs,
    );
    f.emitter.emit(
        "Runtime.exceptionThrown",
        json!({"exceptionDetails": {"text": "Uncaught", "exception": {"description": "TypeError: x is undefined password=hunter2hunter2"}, "url": "http://localhost:5173/app.js", "lineNumber": 3}}),
        cs,
    );
    for i in 0..9 {
        f.emitter.emit(
            "Runtime.consoleAPICalled",
            json!({"type": "error", "args": [{"value": format!("boom {i}")}]}),
            cs,
        );
    }
    until("console entries", || {
        sess.console.lock().unwrap().len() == 11
    })
    .await;
    let evs = e.events("preview.console_error");
    assert_eq!(
        evs.len(),
        crate::preview_console::BUDGET as usize,
        "{evs:?}"
    );
    let first = &evs[0];
    assert_eq!(first.subject["preview"], "v1");
    assert_eq!(first.subject["pane"], "pane-a");
    assert!(first.subject.get("task").is_some());
    assert_eq!(first.data["source"], "exception");
    let text = first.data["text"].as_str().unwrap();
    assert!(
        text.contains("TypeError") && !text.contains("hunter2"),
        "{text}"
    );
    assert_eq!(first.data["count"], 1);
    assert_eq!(first.data["session"], "b1");
}

fn put_task_workspace(e: &Env, ws: &str, task: &str) {
    e.server.with_core(|c| {
        c.model.workspaces.push(vk_proto::model::Workspace {
            id: ws.into(),
            handle: ws.into(),
            name: None,
            auto_name: ws.into(),
            root_path: "/tmp".into(),
            task: Some(task.into()),
            order: 0.0,
            branch: None,
        });
        for p in c.model.panes.iter_mut().filter(|p| p.id == "pane-b") {
            p.workspace = ws.into();
        }
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_reach_only_their_own_previews() {
    let e = Env::new();
    // v1 (:5173) belongs to pane-a; v2 (:6000) to pane-b, whose workspace is task T1; v3 (:6100)
    // is the same task's, declared without a pane.
    e.put_preview("v2", 6000);
    e.put_preview("v3", 6100);
    put_task_workspace(&e, "ws-t1", "T1");
    e.server.with_core(|c| {
        for p in c.model.previews.iter_mut() {
            match p.handle.as_str() {
                "v2" => p.pane = Some("pane-b".into()),
                "v3" => {
                    p.pane = None;
                    p.task = Some("T1".into());
                }
                _ => {}
            }
        }
    });
    let a = ctx_pane("pane-a");
    let b = ctx_pane("pane-b");

    // Opening someone else's preview by handle or URL: foreign_preview, nothing started.
    let err = e
        .call(&a, "browser.open", json!({"preview": "v2"}))
        .await
        .unwrap_err();
    assert_eq!(kind(&err), "destination_denied");
    assert_eq!(err.data.details["reason"], "foreign_preview");
    assert!(e.fake.lock().unwrap().is_none());
    let err = e
        .call(&a, "browser.open", json!({"url": "http://localhost:6000/"}))
        .await
        .unwrap_err();
    assert_eq!(err.data.details["reason"], "foreign_preview");
    // A port that is no preview at all keeps its own reason.
    let err = e
        .call(&a, "browser.open", json!({"url": "http://127.0.0.1:5432/"}))
        .await
        .unwrap_err();
    assert_eq!(err.data.details["reason"], "loopback_port_not_a_preview");

    // Own preview works; navigating on to another task's preview is refused and logged.
    e.call(&a, "browser.open", json!({"preview": "v1"}))
        .await
        .unwrap();
    let err = e
        .call(
            &a,
            "browser.navigate",
            json!({"session": "b1", "url": "http://localhost:6000/x"}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.data.details["reason"], "foreign_preview");
    let net = e
        .call(&a, "browser.network", json!({"session": "b1"}))
        .await
        .unwrap();
    assert_eq!(net["entries"][0]["blocked_by_policy"], "foreign_preview");
    let denied = e.events("browser.request_denied");
    assert_eq!(denied.len(), 1);
    assert_eq!(denied[0].data["reason"], "foreign_preview");
    // The Fetch layer says the same for IP-literal subresources.
    let sess = e.server.agent_browser.session("b1").unwrap();
    assert_eq!(
        fetch_decision(
            &e.server,
            sess.scope.as_ref(),
            "http://127.0.0.1:6000/a.js",
            "Script",
            false
        )
        .await,
        Some("foreign_preview")
    );
    assert_eq!(
        fetch_decision(
            &e.server,
            sess.scope.as_ref(),
            "http://127.0.0.1:5173/a.js",
            "Script",
            false
        )
        .await,
        None
    );

    // The task's previews are reachable from the task's pane, with or without a pane of their own.
    assert!(
        e.call(&b, "browser.open", json!({"preview": "v2"}))
            .await
            .is_ok()
    );
    assert!(
        e.call(&b, "browser.open", json!({"preview": "v3"}))
            .await
            .is_ok()
    );
    assert_eq!(
        e.call(&b, "browser.open", json!({"preview": "v1"}))
            .await
            .unwrap_err()
            .data
            .details["reason"],
        "foreign_preview"
    );
    // Full-scope callers are unchanged.
    assert!(
        e.call(&ctx_full(), "browser.open", json!({"preview": "v2"}))
            .await
            .is_ok()
    );

    // `[browser] session_previews = "machine"` restores machine-wide reach.
    let mut cfg = AgentBrowserConfig::default();
    assert!(cfg.own_previews_only());
    cfg.session_previews = "machine".into();
    *e.server.agent_browser.cfg_cache.lock().unwrap() = Some((Instant::now(), cfg));
    assert!(
        e.call(&a, "browser.open", json!({"preview": "v2"}))
            .await
            .is_ok()
    );
    let l = e.call(&a, "browser.list", json!({})).await.unwrap();
    assert!(
        l["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["previews"] == "machine")
    );
}

#[test]
fn browser_config_section_parses() {
    let parse = |src: &str| {
        let (c, _) = vk_config::Config::parse(src, std::path::Path::new("t.toml")).unwrap();
        AgentBrowserConfig::from_config(&c)
    };
    assert_eq!(
        parse("[browser]\nsession_previews = \"machine\"\n").session_previews,
        "machine"
    );
    assert_eq!(
        parse("[browser]\nsession_previews = \"bogus\"\n").session_previews,
        "own"
    );
    assert!(parse("").own_previews_only());
}

/// Review finding 5: a pane denied `foreign_preview` can't make the preview its own by
/// redeclaring the port: the declare is refused, the owner stays, and the retry is still denied
/// at all three layers (the `browser.open`/`navigate` pre-check, the session proxy, Fetch).
/// An unowned preview is claimable only on the caller's own listener; full scope can reassign.
#[tokio::test(flavor = "multi_thread")]
async fn redeclaring_a_foreign_preview_does_not_take_it_over() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let e = Env::new();
    e.put_preview("v2", 6000);
    e.server.with_core(|c| {
        for p in c.model.previews.iter_mut().filter(|p| p.handle == "v2") {
            p.pane = Some("pane-b".into());
        }
    });
    let a = ctx_pane("pane-a");
    let declare = async |ctx: &Ctx, p: Value| {
        crate::api::dispatch(&e.server, ctx, "preview.declare", &p).await
    };
    let owner = |port: u16| {
        e.server.with_core(|c| {
            c.model
                .previews
                .iter()
                .find(|p| p.port == port)
                .map(|p| (p.pane.clone(), p.task.clone()))
        })
    };

    let err = e
        .call(&a, "browser.open", json!({"preview": "v2"}))
        .await
        .unwrap_err();
    assert_eq!(err.data.details["reason"], "foreign_preview");

    // Redeclaring, with or without naming itself as the owner, is refused.
    for p in [
        json!({"port": 6000}),
        json!({"port": 6000, "pane": "pane-a"}),
        json!({"port": 6000, "label": "mine now"}),
    ] {
        let err = declare(&a, p.clone()).await.unwrap_err();
        assert_eq!(kind(&err), "permission_denied", "{p}");
        assert_eq!(err.data.details["reason"], "foreign_preview");
    }
    // Nor may a pane declare on behalf of a pane it doesn't own.
    let err = declare(&a, json!({"port": 6200, "pane": "pane-b"}))
        .await
        .unwrap_err();
    assert_eq!(kind(&err), "permission_denied");
    assert_eq!(owner(6000), Some((Some("pane-b".into()), None)));
    assert_eq!(owner(6200), None);

    // Retry: layer 1, the pre-check.
    let err = e
        .call(&a, "browser.open", json!({"preview": "v2"}))
        .await
        .unwrap_err();
    assert_eq!(err.data.details["reason"], "foreign_preview");
    let r = e
        .call(&a, "browser.open", json!({"preview": "v1"}))
        .await
        .unwrap();
    let err = e
        .call(
            &a,
            "browser.navigate",
            json!({"session": "b1", "url": "http://localhost:6000/"}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.data.details["reason"], "foreign_preview");
    // Layer 2: the session's filtering proxy.
    let proxy_port = r["proxy_port"].as_u64().unwrap() as u16;
    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .unwrap();
    sock.write_all(b"GET http://127.0.0.1:6000/ HTTP/1.1\r\nHost: 127.0.0.1:6000\r\n\r\n")
        .await
        .unwrap();
    let mut buf = vec![0u8; 1024];
    let n = sock.read(&mut buf).await.unwrap();
    let head = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(head.starts_with("HTTP/1.1 403"), "{head}");
    assert!(head.contains("foreign_preview"), "{head}");
    // Layer 3: Fetch.
    let sess = e.server.agent_browser.session("b1").unwrap();
    assert_eq!(
        fetch_decision(
            &e.server,
            sess.scope.as_ref(),
            "http://127.0.0.1:6000/a.js",
            "Script",
            false
        )
        .await,
        Some("foreign_preview")
    );

    // Its own preview redeclares fine and keeps its owner.
    declare(&a, json!({"port": 5173, "label": "web"}))
        .await
        .unwrap();
    assert_eq!(owner(5173), Some((Some("pane-a".into()), None)));

    // A machine-level preview nobody in pane-a listens on: refused.
    e.put_preview("v4", 6300);
    e.server.with_core(|c| {
        for p in c.model.previews.iter_mut().filter(|p| p.handle == "v4") {
            p.pane = None;
        }
    });
    let err = declare(&a, json!({"port": 6300})).await.unwrap_err();
    assert_eq!(err.data.details["reason"], "foreign_preview");
    assert_eq!(owner(6300), Some((None, None)));

    // A machine-level preview on pane-a's own listener (this test process): claimable.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mine = l.local_addr().unwrap().port();
    e.put_preview("v5", mine);
    e.server.with_core(|c| {
        for p in c.model.previews.iter_mut().filter(|p| p.handle == "v5") {
            p.pane = None;
        }
        for p in c.model.panes.iter_mut().filter(|p| p.id == "pane-a") {
            p.child_pid = Some(std::process::id());
        }
    });
    declare(&a, json!({"port": mine})).await.unwrap();
    assert_eq!(owner(mine), Some((Some("pane-a".into()), None)));
    // A fresh port on another pane's listener can't be declared by pane-b either.
    let l2 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let theirs = l2.local_addr().unwrap().port();
    let err = declare(&ctx_pane("pane-b"), json!({"port": theirs}))
        .await
        .unwrap_err();
    assert_eq!(err.data.details["reason"], "foreign_preview");
    drop((l, l2));

    // Full scope moves ownership; then pane-a reaches it.
    declare(&ctx_full(), json!({"port": 6000, "pane": "pane-a"}))
        .await
        .unwrap();
    assert_eq!(owner(6000), Some((Some("pane-a".into()), None)));
    assert!(
        e.call(&a, "browser.open", json!({"preview": "v2"}))
            .await
            .is_ok()
    );
}

/// Security review: a pane-scoped `preview.declare` on a port no pane's process listens on would
/// add that port to the pane's agent-browser loopback allowlist. It now needs the user's
/// confirmation (an approved call decided outside the pane); approval remembers the port for
/// the pane's process, denial and timeout refuse, and panes can't decide.
#[tokio::test(flavor = "multi_thread")]
async fn unattributed_port_needs_the_users_confirmation() {
    let e = Env::new();
    let a = ctx_pane("pane-a");
    let full = ctx_full();
    let dispatch = async |ctx: &Ctx, method: &str, p: Value| {
        crate::api::dispatch(&e.server, ctx, method, &p).await
    };
    let owner = |port: u16| {
        e.server.with_core(|c| {
            c.model
                .previews
                .iter()
                .find(|p| p.port == port && p.status != vk_proto::model::PreviewStatus::Gone)
                .map(|p| p.pane.clone())
        })
    };

    // Asking: nothing is declared yet; the request carries the server's summary.
    let r = dispatch(
        &a,
        "preview.declare",
        json!({"port": 6400, "label": "db?", "wait": false}),
    )
    .await
    .unwrap();
    assert_eq!(r["status"], "pending", "{r}");
    assert_eq!(r["method"], "preview.declare");
    assert_eq!(r["facts"]["port"], 6400);
    assert!(r["summary"].as_str().unwrap().contains("6400"), "{r}");
    assert_eq!(r["always_allowed"], false);
    let id = r["request"].as_str().unwrap().to_string();
    assert_eq!(owner(6400), None);

    // Panes can't decide (not even their own), and "always" isn't offered.
    for ctx in [&a, &ctx_pane("pane-b")] {
        let err = dispatch(
            ctx,
            "auth.approve.decide",
            json!({"request": id, "decision": "approve"}),
        )
        .await
        .unwrap_err();
        assert_eq!(kind(&err), "permission_denied");
    }
    assert!(
        dispatch(
            &full,
            "auth.approve.decide",
            json!({"request": id, "decision": "always"}),
        )
        .await
        .is_err()
    );
    assert_eq!(owner(6400), None);

    // The user approves: the preview is the pane's, and the waiting call gets it.
    let d = dispatch(
        &full,
        "auth.approve.decide",
        json!({"request": id, "decision": "approve"}),
    )
    .await
    .unwrap();
    assert_eq!(d["ok"], true, "{d}");
    assert_eq!(owner(6400), Some(Some("pane-a".into())));
    let v = dispatch(&a, "preview.declare", json!({"port": 6400, "request": id}))
        .await
        .unwrap();
    assert_eq!(v["preview"]["port"], 6400, "{v}");

    // Remembered for the pane's process: a later declare of the same port needs no new ask.
    let gone = || {
        e.server.with_core(|c| {
            for p in c.model.previews.iter_mut().filter(|p| p.port == 6400) {
                p.status = vk_proto::model::PreviewStatus::Gone;
            }
        })
    };
    // The declare's background probe writes the preview back once it settles (up or down);
    // let it finish first, or it would resurrect the preview this test marks gone.
    for _ in 0..200 {
        let settled = e.server.with_core(|c| {
            c.model.previews.iter().any(|p| {
                p.port == 6400
                    && matches!(
                        p.status,
                        vk_proto::model::PreviewStatus::Up | vk_proto::model::PreviewStatus::Down
                    )
            })
        });
        if settled {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    gone();
    let v = dispatch(&a, "preview.declare", json!({"port": 6400}))
        .await
        .unwrap();
    assert_eq!(v["preview"]["port"], 6400, "{v}");

    // Not for another pane, and denial refuses.
    let b = ctx_pane("pane-b");
    let r = dispatch(&b, "preview.declare", json!({"port": 6401, "wait": false}))
        .await
        .unwrap();
    let id_b = r["request"].as_str().unwrap().to_string();
    dispatch(
        &full,
        "auth.approve.decide",
        json!({"request": id_b, "decision": "deny"}),
    )
    .await
    .unwrap();
    let err = dispatch(
        &b,
        "preview.declare",
        json!({"port": 6401, "request": id_b}),
    )
    .await
    .unwrap_err();
    assert_eq!(kind(&err), "permission_denied");
    assert!(err.message.contains("approval_denied"), "{}", err.message);
    assert_eq!(owner(6401), None);

    // No decision in time: refused, nothing declared.
    let err = dispatch(
        &a,
        "preview.declare",
        json!({"port": 6402, "timeout_ms": 50}),
    )
    .await
    .unwrap_err();
    assert_eq!(kind(&err), "timeout");
    assert!(err.data.details["request"].is_string());
    assert_eq!(owner(6402), None);

    // Synchronous callers (task previews with an absolute port) are refused outright.
    let err = crate::preview::declare(&e.server, &b, &json!({"port": 6403})).unwrap_err();
    assert_eq!(err.data.details["reason"], "confirmation_required");
    assert_eq!(owner(6403), None);

    // A restarted pane (new child process) asks again.
    gone();
    e.server.with_core(|c| {
        for p in c.model.panes.iter_mut().filter(|p| p.id == "pane-a") {
            p.child_pid = Some(4_000_000);
        }
    });
    let r = dispatch(&a, "preview.declare", json!({"port": 6400, "wait": false}))
        .await
        .unwrap();
    assert_eq!(r["status"], "pending", "{r}");

    // Full scope keeps the direct path.
    let v = dispatch(&full, "preview.declare", json!({"port": 6404}))
        .await
        .unwrap();
    assert_eq!(v["preview"]["port"], 6404);
}

/// Code review: a pane that prints `http://localhost:<port>` gets an existing suggestion on an
/// unrelated local service attributed to it by output discovery. That attribution alone must
/// not let it declare without confirmation, promote the suggestion, or have `browser.open`
/// promote it; its own verified listener does.
#[tokio::test(flavor = "multi_thread")]
async fn printed_url_attribution_is_not_ownership() {
    let e = Env::new();
    let a = ctx_pane("pane-a");
    let dispatch = async |ctx: &Ctx, method: &str, p: Value| {
        crate::api::dispatch(&e.server, ctx, method, &p).await
    };
    // Another program (this test process, outside every pane) listens on the port.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    e.put_preview("v7", port);
    e.server.with_core(|c| {
        for p in c.model.previews.iter_mut().filter(|p| p.handle == "v7") {
            p.status = PreviewStatus::Suggested;
            p.source = PreviewSource::OutputUrl;
        }
    });
    let status = || {
        e.server.with_core(|c| {
            c.model
                .previews
                .iter()
                .find(|p| p.handle == "v7")
                .map(|p| p.status)
        })
    };

    let r = dispatch(&a, "preview.declare", json!({"port": port, "wait": false}))
        .await
        .unwrap();
    assert_eq!(r["status"], "pending", "{r}");
    let err = dispatch(&a, "preview.promote", json!({"preview": "v7"}))
        .await
        .unwrap_err();
    assert_eq!(err.data.details["reason"], "confirmation_required");
    let err = e
        .call(&a, "browser.open", json!({"preview": "v7"}))
        .await
        .unwrap_err();
    assert_eq!(err.data.details["reason"], "confirmation_required");
    assert_eq!(status(), Some(PreviewStatus::Suggested));

    // Its own verified listener: promotion works.
    e.server.with_core(|c| {
        for p in c.model.panes.iter_mut().filter(|p| p.id == "pane-a") {
            p.child_pid = Some(std::process::id());
        }
    });
    dispatch(&a, "preview.promote", json!({"preview": "v7"}))
        .await
        .unwrap();
    assert_eq!(status(), Some(PreviewStatus::Up));
    drop(l);
}
