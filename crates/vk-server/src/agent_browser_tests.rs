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
    let net = e
        .call(&a, "browser.network", json!({"session": "b1"}))
        .await
        .unwrap();
    let blocked: Vec<&Value> = net["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|x| !x["blocked_by_policy"].is_null())
        .collect();
    assert_eq!(blocked.len(), 4);
    assert_eq!(e.events("browser.request_denied").len(), 4);
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
    until("owner gone → closed", || {
        e.server.agent_browser.session("b2").is_none()
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
