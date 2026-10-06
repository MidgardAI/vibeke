//! Browser pane page I/O against the in-process fake Chromium (06 B3.2): console/network
//! capture and its API, relaying to a remote owner, the page clipboard, files into the page and
//! pinned (letterboxed) device viewports.

use super::followup_tests::{Env, ctx};
use super::*;
use std::path::Path;
use vk_browser::fake_chromium::State as FakeState;

type Fake = Arc<Mutex<FakeState>>;

fn host(e: &Env, spec: BrowserPane) -> Arc<FakeLauncher> {
    e.server
        .browser
        .set_profiles_root(e._dir.path().join("profiles"));
    let fake = Arc::new(FakeLauncher::default());
    e.server.browser.set_launcher(fake.clone());
    e.put_pane("BP1", "ws-a", Some(spec));
    fake
}

fn spec() -> BrowserPane {
    BrowserPane {
        url: "http://localhost:5173/".into(),
        source_pane: Some("pane-a".into()),
        ..Default::default()
    }
}

fn mp(spec: BrowserPane) -> MediaPane {
    MediaPane {
        pane: "BP1".into(),
        owner: String::new(),
        spec,
        cols: 8,
        rows: 4,
        cell_w: 16,
        cell_h: 32,
        dpr: 2.0,
    }
}

fn frames(buf: &mut Vec<u8>) -> Vec<ServerFrame> {
    let mut fb = vk_proto::frame::FrameBuf::default();
    fb.push(buf);
    buf.clear();
    let mut v = Vec::new();
    while let Some(f) = fb.next_frame::<ServerFrame>().unwrap() {
        v.push(f);
    }
    v
}

/// Flush (acking media) until `want` matches a frame, or panic after `secs`.
async fn until(
    e: &Env,
    ms: &mut MediaSession,
    secs: u64,
    want: impl Fn(&ServerFrame) -> bool,
) -> ServerFrame {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let mut buf = Vec::new();
        ms.flush(&e.server, &mut buf).await.unwrap();
        for f in frames(&mut buf) {
            if let ServerFrame::Media(m) = &f {
                ms.on_ack(&m.pane, m.seq);
            }
            if want(&f) {
                return f;
            }
        }
        assert!(Instant::now() < deadline, "no matching frame");
        let _ = tokio::time::timeout(Duration::from_millis(50), ms.notify.notified()).await;
    }
}

async fn wait_for(what: &str, f: impl Fn() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// View BP1 and wait until its page streams; returns the fake and the page's session.
async fn streaming(
    e: &Env,
    fake: &Arc<FakeLauncher>,
    ms: &mut MediaSession,
    p: MediaPane,
) -> (Fake, String) {
    ms.on_view(&e.server, vec![p], false, true);
    until(e, ms, 10, |f| matches!(f, ServerFrame::Media(m) if m.reset)).await;
    let st = fake.launched.lock().unwrap()[0].1.clone();
    let sess = st.lock().unwrap().sessions()[0].clone();
    (st, sess)
}

#[tokio::test(flavor = "multi_thread")]
async fn console_and_network_capture_redacted_filtered_and_scoped() {
    let e = Env::new();
    let fake = host(&e, spec());
    let mut ms = MediaSession::new(&e.server, false);
    let (st, sess) = streaming(&e, &fake, &mut ms, mp(spec())).await;
    // Capture is on before the page navigates.
    {
        let s = st.lock().unwrap();
        for m in ["Runtime.enable", "Log.enable", "Network.enable"] {
            assert!(
                s.calls(m)
                    .iter()
                    .any(|(_, x)| x.as_deref() == Some(sess.as_str())),
                "{m}"
            );
        }
        let log: Vec<&str> = s.log.iter().map(|(m, _, _)| m.as_str()).collect();
        let nav = log.iter().position(|m| *m == "Page.navigate").unwrap();
        assert!(log.iter().position(|m| *m == "Network.enable").unwrap() < nav);
    }
    {
        let mut s = st.lock().unwrap();
        s.inject(
            "Runtime.consoleAPICalled",
            json!({"type": "error", "args": [{"type": "string", "value": "boom password=hunter2"}]}),
            Some(&sess),
        );
        s.inject(
            "Network.requestWillBeSent",
            json!({"requestId": "x", "type": "Fetch", "request": {"method": "POST", "url": "http://localhost:5173/api?token=abcd1234secret"}}),
            Some(&sess),
        );
        s.inject(
            "Network.responseReceived",
            json!({"requestId": "x", "response": {"status": 500}}),
            Some(&sess),
        );
        s.inject(
            "Network.loadingFinished",
            json!({"requestId": "x"}),
            Some(&sess),
        );
    }
    let full = ctx(None);
    let mut all = Value::Null;
    for _ in 0..200 {
        all = e
            .call(&full, "browser.console", json!({"pane": "BP1"}))
            .await;
        let n = all["result"]["entries"].as_array().map_or(0, Vec::len);
        // navigation (document request + "loaded" line) + the injected two
        if n >= 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let r = &all["result"];
    assert_eq!(r["source"], "local", "{all}");
    let entries = r["entries"].as_array().unwrap();
    let boom = entries
        .iter()
        .find(|x| x["text"].as_str().is_some_and(|t| t.starts_with("boom")))
        .expect("console entry");
    assert_eq!(boom["level"], "error");
    assert!(
        !boom["text"].as_str().unwrap().contains("hunter2"),
        "{boom}"
    );
    let api = entries
        .iter()
        .find(|x| x["method"] == "POST")
        .expect("network entry");
    assert_eq!(api["status"], 500);
    assert!(
        !api["url"].as_str().unwrap().contains("abcd1234secret"),
        "{api}"
    );
    // Filters: errors only; network only (`browser.network`); after.
    let errs = e
        .call(
            &full,
            "browser.console",
            json!({"pane": "BP1", "errors": true}),
        )
        .await;
    let errs = errs["result"]["entries"].as_array().unwrap().clone();
    assert_eq!(errs.len(), 2, "{errs:?}");
    let net = e
        .call(
            &full,
            "browser.network",
            json!({"pane": "BP1", "failed": true}),
        )
        .await;
    assert_eq!(net["result"]["entries"].as_array().unwrap().len(), 1);
    let last = r["last_seq"].as_u64().unwrap();
    let none = e
        .call(
            &full,
            "browser.console",
            json!({"pane": "BP1", "after": last}),
        )
        .await;
    assert!(none["result"]["entries"].as_array().unwrap().is_empty());
    // Pane scope: an unrelated pane is refused; the browser pane's console split may read.
    let denied = e
        .call(
            &ctx(Some("pane-a")),
            "browser.console",
            json!({"pane": "BP1"}),
        )
        .await;
    assert!(
        denied["error"].to_string().contains("permission_denied"),
        "{denied}"
    );
    e.put_pane("CON1", "ws-a", None);
    {
        let mut c = e.server.core.lock().unwrap();
        let mut p = c.pane("CON1").cloned().unwrap();
        p.created_by = "browser-console:BP1".into();
        let mut tx = Tx::new();
        tx.pane(p);
        e.server.commit(&mut c, tx).unwrap();
    }
    let ok = e
        .call(
            &ctx(Some("CON1")),
            "browser.console",
            json!({"pane": "BP1"}),
        )
        .await;
    assert!(
        ok["result"]["entries"].as_array().unwrap().len() >= 4,
        "{ok}"
    );
    // The split methods are user actions.
    for m in ["browser.pane.console", "browser.pane.console_push"] {
        let r = e.call(&ctx(Some("CON1")), m, json!({"pane": "BP1"})).await;
        assert!(
            r["error"].to_string().contains("permission_denied"),
            "{m}: {r}"
        );
    }
    // Not a browser pane.
    let r = e
        .call(&full, "browser.console", json!({"pane": "pane-a"}))
        .await;
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not a browser pane")
    );
    ms.close(&e.server);
}

#[tokio::test(flavor = "multi_thread")]
async fn relayed_entries_are_cleaned_and_relaying_stops_off_loopback() {
    let e = Env::new();
    let _fake = host(&e, spec());
    // The owner side: the media host pushes entries (untrusted input, cleaned again).
    let full = ctx(None);
    let r = e
        .call(
            &full,
            "browser.pane.console_push",
            json!({"pane": "BP1", "entries": [
                {"kind": "console", "ts": 1, "level": "error", "text": "x api_key=sk-live-123456789012345678901234", "evil": {"a": 1}},
                {"kind": "network", "ts": 2, "method": "GET", "url": "http://localhost:5173/a", "status": 404, "nested": [1]},
                "junk"
            ]}),
        )
        .await;
    assert_eq!(r["result"]["stored"], 2, "{r}");
    let c = e
        .call(&full, "browser.console", json!({"pane": "BP1"}))
        .await;
    let entries = c["result"]["entries"].as_array().unwrap();
    assert_eq!(c["result"]["source"], "relayed");
    assert_eq!(entries.len(), 2);
    assert!(entries[0].get("evil").is_none());
    assert!(
        !entries[0]["text"].as_str().unwrap().contains("sk-live"),
        "{entries:?}"
    );
    assert_eq!(entries[1]["status"], 404);
    // Pushing for a pane that is not a browser pane is refused.
    let r = e
        .call(
            &full,
            "browser.pane.console_push",
            json!({"pane": "pane-a", "entries": []}),
        )
        .await;
    assert!(r.get("error").is_some());

    // The media-host side: a target owned by a remote machine queues the owner app's entries
    // for relaying (its first navigation: the document request and the "loaded" line).
    let mut ms = MediaSession::new(&e.server, false);
    let fake2 = Arc::new(FakeLauncher::default());
    e.server.browser.set_launcher(fake2.clone());
    let mut remote = mp(spec());
    remote.pane = "RP1".into();
    remote.owner = "devbox".into();
    ms.on_view(&e.server, vec![remote], false, true);
    until(
        &e,
        &mut ms,
        10,
        |f| matches!(f, ServerFrame::Media(m) if m.reset),
    )
    .await;
    let t = e.server.browser.target("RP1").unwrap();
    wait_for("navigation entries queued for relay", || {
        t.st.lock().unwrap().io.relay.len() >= 2
    })
    .await;
    page_io::relay(&e.server);
    assert!(t.st.lock().unwrap().io.relay.is_empty(), "sent");
    ms.close(&e.server);
}

/// Inject `events` into the fake page and wait until the last one was processed (it must
/// produce a capture entry with the marker text).
async fn feed(t: &Arc<Target>, st: &Fake, sess: &str, events: Vec<(&str, Value)>, marker: &str) {
    {
        let mut s = st.lock().unwrap();
        for (m, p) in events {
            s.inject(m, p, Some(sess));
        }
        s.inject(
            "Runtime.consoleAPICalled",
            json!({"type": "log", "executionContextId": 0, "args": [{"type": "string", "value": marker}]}),
            Some(sess),
        );
    }
    wait_for(marker, || {
        t.st.lock()
            .unwrap()
            .io
            .capture
            .console
            .iter()
            .any(|e| e["text"] == marker)
    })
    .await;
}

fn relayed(t: &Target) -> Vec<String> {
    t.st.lock()
        .unwrap()
        .io
        .relay
        .iter()
        .map(|e| {
            e["text"]
                .as_str()
                .or(e["url"].as_str())
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

fn nav(url: &str, loader: &str) -> (&'static str, Value) {
    (
        "Page.frameNavigated",
        json!({"frame": {"id": "F", "url": url, "loaderId": loader}}),
    )
}

fn context(id: i64, origin: &str, frame: &str) -> (&'static str, Value) {
    (
        "Runtime.executionContextCreated",
        json!({"context": {"id": id, "origin": origin, "auxData": {"isDefault": true, "frameId": frame}}}),
    )
}

fn log(ctx: i64, text: &str) -> (&'static str, Value) {
    (
        "Runtime.consoleAPICalled",
        json!({"type": "log", "executionContextId": ctx, "args": [{"type": "string", "value": text}]}),
    )
}

fn request(id: &str, url: &str, doc: &str, loader: &str) -> (&'static str, Value) {
    (
        "Network.requestWillBeSent",
        json!({"requestId": id, "loaderId": loader, "documentURL": doc, "type": "Fetch",
               "request": {"method": "GET", "url": url}}),
    )
}

fn finished(id: &str) -> (&'static str, Value) {
    ("Network.loadingFinished", json!({"requestId": id}))
}

/// Relay eligibility is decided per entry when it is captured: the three ways the old
/// flush-time URL check let off-origin capture reach the remote owner.
#[tokio::test(flavor = "multi_thread")]
async fn relay_eligibility_is_decided_per_entry_at_capture() {
    let e = Env::new();
    let _fake = host(&e, spec());
    let mut ms = MediaSession::new(&e.server, false);
    let fake = Arc::new(FakeLauncher::default());
    e.server.browser.set_launcher(fake.clone());
    let mut remote = mp(spec());
    remote.pane = "RP1".into();
    remote.owner = "devbox".into();
    let (st, sess) = streaming(&e, &fake, &mut ms, remote).await;
    let t = e.server.browser.target("RP1").unwrap();
    wait_for("first navigation queued", || {
        relayed(&t).iter().any(|x| x.starts_with("loaded "))
    })
    .await;
    t.st.lock().unwrap().io.relay.clear();

    // 1. External → loopback between two relay ticks: what the external site logged and
    //    requested is not queued; the loopback app's lines are.
    feed(
        &t,
        &st,
        &sess,
        vec![
            nav("https://bank.example/account", "LB"),
            context(50, "https://bank.example", "F"),
            log(50, "bank balance 1234"),
            request(
                "b1",
                "https://bank.example/api/me",
                "https://bank.example/account",
                "LB",
            ),
            finished("b1"),
            nav("http://localhost:5173/", "LA"),
            context(51, "http://localhost:5173", "F"),
            log(51, "app log after return"),
        ],
        "marker-1",
    )
    .await;
    let r = relayed(&t);
    assert!(r.contains(&"app log after return".to_string()), "{r:?}");
    assert!(
        !r.iter().any(|x| x.contains("bank")),
        "external capture queued: {r:?}"
    );
    t.st.lock().unwrap().io.relay.clear();

    // 2. A request started on the external site completes after the page returned to
    //    loopback; and one started by the previous loopback document completes after the
    //    next navigation. Neither is queued; the new document's own request is.
    feed(
        &t,
        &st,
        &sess,
        vec![
            nav("https://bank.example/", "LB2"),
            context(52, "https://bank.example", "F"),
            request(
                "late",
                "https://bank.example/slow",
                "https://bank.example/",
                "LB2",
            ),
            nav("http://localhost:5173/", "LA2"),
            context(53, "http://localhost:5173", "F"),
            finished("late"),
            request(
                "prev",
                "http://localhost:5173/slow",
                "http://localhost:5173/",
                "LA2",
            ),
            nav("http://localhost:5173/next", "LA3"),
            context(54, "http://localhost:5173", "F"),
            finished("prev"),
            request(
                "own",
                "http://localhost:5173/api",
                "http://localhost:5173/next",
                "LA3",
            ),
            finished("own"),
        ],
        "marker-2",
    )
    .await;
    let r = relayed(&t);
    assert!(
        r.contains(&"http://localhost:5173/api".to_string()),
        "{r:?}"
    );
    assert!(!r.iter().any(|x| x.contains("/slow")), "{r:?}");
    t.st.lock().unwrap().io.relay.clear();

    // 3. Events of an external iframe while the top-level page stays loopback: its console,
    //    its requests and log entries naming it are not queued; the top document's are.
    feed(
        &t,
        &st,
        &sess,
        vec![
            (
                "Page.frameNavigated",
                json!({"frame": {"id": "AD", "parentId": "F", "url": "https://ads.example/frame", "loaderId": "LAD"}}),
            ),
            context(60, "https://ads.example", "AD"),
            log(60, "tracker id 99"),
            request("ad", "https://ads.example/pixel", "https://ads.example/frame", "LAD"),
            finished("ad"),
            (
                "Log.entryAdded",
                json!({"entry": {"level": "error", "text": "ads blocked", "source": "network", "url": "https://ads.example/x.js"}}),
            ),
            log(54, "top document line"),
            // An unknown context (no origin): not queued.
            log(999, "nobody's line"),
        ],
        "marker-3",
    )
    .await;
    let r = relayed(&t);
    assert!(r.contains(&"top document line".to_string()), "{r:?}");
    assert!(
        !r.iter()
            .any(|x| x.contains("ads") || x.contains("tracker") || x.contains("nobody")),
        "{r:?}"
    );
    // Local readers still see everything (the capture is the page's).
    let all = t.st.lock().unwrap().io.capture.query(&Default::default());
    assert!(all.iter().any(|e| e["text"] == "tracker id 99"));
    let tracker = all.iter().find(|e| e["text"] == "tracker id 99").unwrap();
    assert_eq!(tracker["origin"], "https://ads.example");
    assert_eq!(tracker["document"], "https://ads.example/frame");
    ms.close(&e.server);
}

#[tokio::test(flavor = "multi_thread")]
async fn page_clipboard_reaches_viewers_only_after_user_input() {
    let e = Env::new();
    let fake = host(&e, spec());
    let mut ms = MediaSession::new(&e.server, false);
    let (st, sess) = streaming(&e, &fake, &mut ms, mp(spec())).await;
    let binding = {
        let s = st.lock().unwrap();
        let b = s.bindings(&sess);
        assert_eq!(b.len(), 1, "one clipboard binding per page");
        let scripts = s.scripts(&sess);
        assert!(scripts[0].contains(&b[0]) && scripts[0].contains("writeText"));
        assert_eq!(s.calls("Page.setInterceptFileChooserDialog").len(), 1);
        b[0].clone()
    };
    let clip = |st: &Fake, text: &str| {
        st.lock().unwrap().inject(
            "Runtime.bindingCalled",
            json!({"name": binding, "payload": text, "executionContextId": 1}),
            Some(&sess),
        );
    };
    // No user input yet: a page can't set the clipboard on its own.
    clip(&st, "unsolicited");
    let t = e.server.browser.target("BP1").unwrap();
    wait_for("blocked write counted", || {
        t.st.lock().unwrap().io.clips_blocked == 1
    })
    .await;
    // A binding with another name (page code guessing) is ignored outright.
    st.lock().unwrap().inject(
        "Runtime.bindingCalled",
        json!({"name": "__vk_clip_guess", "payload": "x"}),
        Some(&sess),
    );
    // After a click in the page, the copy goes to the viewer as a clipboard frame.
    ms.on_cmd(
        &e.server,
        "BP1",
        BrowserCmd::Mouse {
            kind: MouseKind::Press,
            button: MouseButton::Left,
            x: 10.0,
            y: 10.0,
            mods: vk_proto::input::Mods::empty(),
            clicks: 1,
        },
    );
    clip(&st, "copied text");
    let f = until(&e, &mut ms, 10, |f| {
        matches!(f, ServerFrame::Clipboard { .. })
    })
    .await;
    let ServerFrame::Clipboard { data, pane, .. } = f else {
        unreachable!()
    };
    assert_eq!(
        (data.as_slice(), pane.as_str()),
        (&b"copied text"[..], "BP1")
    );
    assert_eq!(t.st.lock().unwrap().io.clips, 1);
    assert_eq!(t.st.lock().unwrap().io.clips_blocked, 1);
    ms.close(&e.server);
}

/// A staged copy of `body` in the drop directory, as `blob.commit {stage: "browser"}` leaves it.
fn stage(root: &Path, hash: &str, name: &str, body: &[u8]) -> String {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(root.join(hash)).unwrap();
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let p = root.join(hash).join(name);
    std::fs::write(&p, body).unwrap();
    p.to_string_lossy().into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn files_are_dropped_or_attached_to_an_open_chooser_after_checks() {
    let e = Env::new();
    let fake = host(&e, spec());
    let root = e._dir.path().canonicalize().unwrap().join("drops");
    e.server.browser.set_drops_root(root.clone());
    let mut ms = MediaSession::new(&e.server, false);
    let (st, sess) = streaming(&e, &fake, &mut ms, mp(spec())).await;
    let staged = stage(&root, "0123456789ab", "shot.png", b"\x89PNG fake");
    // Pointer position first: drops land where the pointer was.
    ms.on_cmd(
        &e.server,
        "BP1",
        BrowserCmd::Mouse {
            kind: MouseKind::Move,
            button: MouseButton::None,
            x: 12.0,
            y: 20.0,
            mods: vk_proto::input::Mods::empty(),
            clicks: 0,
        },
    );
    ms.on_cmd(
        &e.server,
        "BP1",
        BrowserCmd::DropFiles(vec![staged.clone()]),
    );
    wait_for("drag events", || {
        st.lock().unwrap().calls("Input.dispatchDragEvent").len() == 3
    })
    .await;
    {
        let s = st.lock().unwrap();
        let drags = s.calls("Input.dispatchDragEvent");
        let types: Vec<&str> = drags
            .iter()
            .map(|(p, _)| p["type"].as_str().unwrap())
            .collect();
        assert_eq!(types, vec!["dragEnter", "dragOver", "drop"]);
        assert_eq!(drags[2].0["data"]["files"], json!([staged]));
        assert_eq!(
            (drags[2].0["x"].as_f64(), drags[2].0["y"].as_f64()),
            (Some(12.0), Some(20.0))
        );
        assert_eq!(drags[2].1.as_deref(), Some(sess.as_str()));
    }
    until(&e, &mut ms, 10, |f| {
        matches!(f, ServerFrame::BrowserState { state, .. } if state.notice.as_deref().is_some_and(|n| n.starts_with("dropped 1 file")))
    })
    .await;
    // A drop is not a user gesture for the page clipboard.
    assert!(
        e.server
            .browser
            .target("BP1")
            .unwrap()
            .st
            .lock()
            .unwrap()
            .io
            .last_input
            .is_none()
    );
    // The page opens a file chooser: the next drop fills it instead.
    st.lock().unwrap().inject(
        "Page.fileChooserOpened",
        json!({"frameId": "F", "mode": "selectSingle", "backendNodeId": 42}),
        Some(&sess),
    );
    until(&e, &mut ms, 10, |f| {
        matches!(f, ServerFrame::BrowserState { state, .. } if state.notice.as_deref().is_some_and(|n| n.contains("file chooser")))
    })
    .await;
    ms.on_cmd(
        &e.server,
        "BP1",
        BrowserCmd::DropFiles(vec![staged.clone()]),
    );
    wait_for("setFileInputFiles", || {
        !st.lock().unwrap().calls("DOM.setFileInputFiles").is_empty()
    })
    .await;
    let set = st.lock().unwrap().calls("DOM.setFileInputFiles");
    assert_eq!(set[0].0, json!({"files": [staged], "backendNodeId": 42}));
    // Refused, nothing reaching the page: a file outside the drop directory (any path the
    // user's file system names), a relative path, a staged name that is a link, a hash
    // directory that is a link, a directory, a staged file over 50 MiB, a malformed layout.
    let outside = e._dir.path().join("secret.txt");
    std::fs::write(&outside, b"secret").unwrap();
    let link = root.join("0123456789ab").join("link.txt");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let elsewhere = e._dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("x.txt"), b"x").unwrap();
    std::os::unix::fs::symlink(&elsewhere, root.join("aaaaaaaaaaaa")).unwrap();
    std::fs::create_dir_all(root.join("bbbbbbbbbbbb").join("dir")).unwrap();
    let big = stage(&root, "cccccccccccc", "big.bin", b"");
    std::fs::File::options()
        .write(true)
        .open(&big)
        .unwrap()
        .set_len(page_io::DROP_MAX + 1)
        .unwrap();
    let flat = stage(&root, "nothex-hash!", "f.txt", b"f");
    for (p, why) in [
        (outside.to_string_lossy().into_owned(), "not a staged copy"),
        ("relative/file.txt".to_string(), "not an absolute path"),
        (link.to_string_lossy().into_owned(), "not readable"),
        (
            root.join("aaaaaaaaaaaa/x.txt")
                .to_string_lossy()
                .into_owned(),
            "not a staged copy",
        ),
        (
            root.join("bbbbbbbbbbbb/dir").to_string_lossy().into_owned(),
            "not a regular file",
        ),
        (big.clone(), "larger than 50 MiB"),
        (flat, "not a staged copy"),
        (
            root.join("0123456789ab/../0123456789ab/shot.png")
                .to_string_lossy()
                .into_owned(),
            "not a staged copy",
        ),
    ] {
        assert!(
            page_io::check_drop_path(&root, &p)
                .unwrap_err()
                .contains(why),
            "{p}: {:?}",
            page_io::check_drop_path(&root, &p)
        );
        ms.on_cmd(&e.server, "BP1", BrowserCmd::DropFiles(vec![p.clone()]));
        until(&e, &mut ms, 10, |f| {
            matches!(f, ServerFrame::BrowserState { state, .. } if state.notice.as_deref().is_some_and(|n| n.starts_with("not dropped") && n.contains(why)))
        })
        .await;
    }
    assert_eq!(st.lock().unwrap().calls("Input.dispatchDragEvent").len(), 3);
    assert_eq!(st.lock().unwrap().calls("DOM.setFileInputFiles").len(), 1);
    assert!(outside.exists(), "a refused path outside is never touched");
    // Cleanup: staged copies older than the TTL go (a delivery refreshes the clock), fresh
    // ones stay.
    let old = std::time::SystemTime::now() - page_io::DROP_TTL - Duration::from_secs(5);
    let fresh = stage(&root, "dddddddddddd", "fresh.txt", b"new");
    std::fs::File::open(root.join("cccccccccccc"))
        .unwrap()
        .set_modified(old)
        .unwrap();
    page_io::sweep_drops(&e.server);
    assert!(!root.join("cccccccccccc").exists(), "expired");
    assert!(Path::new(&fresh).exists() && Path::new(&staged).exists());
    assert!(
        elsewhere.join("x.txt").exists(),
        "a link is removed, not followed"
    );
    ms.close(&e.server);
}

/// `blob.commit {stage: "browser"}` puts the upload in the drop directory (0700), which is what
/// `DropFiles` then accepts; a plain commit still goes to the inbox.
#[tokio::test(flavor = "multi_thread")]
async fn browser_stage_uploads_land_in_the_private_drop_directory() {
    use std::os::unix::fs::PermissionsExt;
    let e = Env::new();
    let root = e
        ._dir
        .path()
        .canonicalize()
        .unwrap()
        .join("state/browser-drops");
    e.server.browser.set_drops_root(root.clone());
    let full = ctx(None);
    let b = e
        .call(&full, "blob.begin", json!({"name": "../a.txt", "size": 5}))
        .await;
    let id = b["result"]["upload_id"].as_str().unwrap().to_string();
    e.call(
        &full,
        "blob.append",
        json!({"upload_id": id, "offset": 0, "data_b64": "aGVsbG8="}),
    )
    .await;
    let c = e
        .call(
            &full,
            "blob.commit",
            json!({"upload_id": id, "stage": "browser"}),
        )
        .await;
    let path = c["result"]["path"].as_str().unwrap().to_string();
    assert!(path.starts_with(root.to_str().unwrap()), "{c}");
    assert!(path.ends_with("/a.txt"));
    assert_eq!(
        std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        page_io::check_drop_path(&root, &path).unwrap(),
        Path::new(&path)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn device_pin_emulates_letterboxes_and_maps_input() {
    let e = Env::new();
    let phone = BrowserPane {
        device: Some("iphone-15".into()),
        ..spec()
    };
    let fake = host(&e, phone.clone());
    let mut ms = MediaSession::new(&e.server, false);
    // A wide pane: 40×10 cells of 16×32 = 640×320 device px at DPR 2.
    let mut wide = mp(phone.clone());
    wide.cols = 40;
    wide.rows = 10;
    ms.on_view(&e.server, vec![wide.clone()], false, true);
    let f = until(
        &e,
        &mut ms,
        10,
        |f| matches!(f, ServerFrame::Media(m) if m.reset && m.width == 640),
    )
    .await;
    let ServerFrame::Media(m) = f else {
        unreachable!()
    };
    assert_eq!(
        (m.width, m.height),
        (640, 320),
        "the frame fills the content area"
    );
    let st = fake.launched.lock().unwrap()[0].1.clone();
    let sess = st.lock().unwrap().sessions()[0].clone();
    {
        let s = st.lock().unwrap();
        let (dpr, mobile, ua) = s.emulation(&sess).unwrap();
        assert_eq!((dpr, mobile), (Some(3.0), true));
        assert!(ua.unwrap().contains("iPhone"));
        assert!(
            s.calls("Emulation.setTouchEmulationEnabled")
                .iter()
                .any(|(p, _)| p["enabled"] == true)
        );
        assert_eq!(s.viewports(), vec![(393, 852)]);
        // The screencast is bounded by the letterbox rectangle: 320 px tall → 148 px wide.
        let sc = s.calls("Page.startScreencast");
        let last = &sc.last().unwrap().0;
        assert_eq!(last["maxHeight"], 320);
        assert_eq!(last["maxWidth"], (393.0f64 * 320.0 / 852.0).round() as u64);
    }
    // Pixels: margins are the neutral fill, the middle is the page.
    let px_at = |m: &MediaFrame, x: u32, y: u32| -> [u8; 4] {
        let tw = m.cell_w as u32 * m.tile_cols as u32;
        let th = m.cell_h as u32 * m.tile_rows as u32;
        let idx = (y / th) * m.grid_cols as u32 + x / tw;
        let t = m.tiles.iter().find(|t| t.index == idx).unwrap();
        let TileData::ZlibRgba(z) = &t.data else {
            panic!()
        };
        let px = vk_browser::kitty::unzlib(z).unwrap();
        let (lx, ly) = (x % tw, y % th);
        let i = ((ly * t.w + lx) * 4) as usize;
        [px[i], px[i + 1], px[i + 2], px[i + 3]]
    };
    assert_eq!(px_at(&m, 10, 300), vk_browser::devices::FILL);
    assert_eq!(px_at(&m, 630, 300), vk_browser::devices::FILL);
    assert_ne!(px_at(&m, 320, 300), vk_browser::devices::FILL);
    let status = status_json(&e.server);
    assert_eq!(
        status["targets"][0]["pin"]["device"], "iphone-15",
        "{status}"
    );
    // Input: the pane centre is the page centre (CSS of the phone), the margin is nothing.
    let click = |x: f32, y: f32| BrowserCmd::Mouse {
        kind: MouseKind::Press,
        button: MouseButton::Left,
        x,
        y,
        mods: vk_proto::input::Mods::empty(),
        clicks: 1,
    };
    ms.on_cmd(&e.server, "BP1", click(160.0, 80.0)); // device (320, 160): the centre
    ms.on_cmd(&e.server, "BP1", click(5.0, 80.0)); // left margin
    wait_for("mouse event", || {
        st.lock()
            .unwrap()
            .calls("Input.dispatchMouseEvent")
            .iter()
            .any(|(p, _)| p["type"] == "mousePressed")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let presses: Vec<Value> = st
        .lock()
        .unwrap()
        .calls("Input.dispatchMouseEvent")
        .into_iter()
        .filter(|(p, _)| p["type"] == "mousePressed")
        .map(|(p, _)| p)
        .collect();
    assert_eq!(presses.len(), 1, "the margin click is dropped: {presses:?}");
    let (x, y) = (
        presses[0]["x"].as_f64().unwrap(),
        presses[0]["y"].as_f64().unwrap(),
    );
    assert!(
        (x - 196.5).abs() < 3.0 && (y - 426.0).abs() < 3.0,
        "{x},{y}"
    );
    // Unpinned again: the pane's own size, the browser's own user agent, no touch.
    let mut fit = wide.clone();
    fit.spec.device = None;
    ms.on_view(&e.server, vec![fit], false, true);
    wait_for("unpinned viewport", || {
        st.lock().unwrap().viewports() == vec![(320, 160)]
    })
    .await;
    wait_for("user agent restored", || {
        st.lock().unwrap().emulation(&sess).unwrap().2.is_none()
    })
    .await;
    let s = st.lock().unwrap();
    assert!(!s.emulation(&sess).unwrap().1, "not mobile");
    assert!(
        s.calls("Emulation.setTouchEmulationEnabled")
            .last()
            .unwrap()
            .0["enabled"]
            == false
    );
    drop(s);
    ms.close(&e.server);
}

fn key_press() -> BrowserCmd {
    BrowserCmd::Key(vk_proto::input::KeyEvent::ch('k'))
}

fn click(kind: MouseKind) -> BrowserCmd {
    BrowserCmd::Mouse {
        kind,
        button: MouseButton::Left,
        x: 10.0,
        y: 10.0,
        mods: vk_proto::input::Mods::empty(),
        clicks: 1,
    }
}

/// The clipboard bridge accepts a write only from the current top-level document's main world,
/// only after a real click or key press in the pane (not text, not a release, not API input),
/// and never across a navigation.
#[tokio::test(flavor = "multi_thread")]
async fn page_clipboard_needs_the_top_document_and_a_real_gesture() {
    let e = Env::new();
    let fake = host(&e, spec());
    let mut ms = MediaSession::new(&e.server, false);
    let (st, sess) = streaming(&e, &fake, &mut ms, mp(spec())).await;
    let t = e.server.browser.target("BP1").unwrap();
    let binding = st.lock().unwrap().bindings(&sess)[0].clone();
    let top = st.lock().unwrap().context(&sess);
    wait_for("the page's main world", || {
        t.st.lock().unwrap().io.capture.is_top_main_world(top)
    })
    .await;
    // The script reports only what Chromium accepted, and only trusted, activated copy/cut.
    let script = st.lock().unwrap().scripts(&sess)[0].clone();
    assert!(script.contains("wt(t).then((v) => { post(s)"), "{script}");
    assert!(script.contains("!e.isTrusted || !active()"), "{script}");
    assert!(
        !script.contains(".catch(() => undefined)"),
        "rejections reach the page"
    );
    assert!(
        st.lock()
            .unwrap()
            .calls("Browser.setPermission")
            .iter()
            .any(|(p, _)| p["permission"]["name"] == "clipboard-write")
    );
    let blocked = |n: u64| {
        let t = t.clone();
        move || t.st.lock().unwrap().io.clips_blocked == n
    };
    let write = |ctx: Option<i64>, text: &str| {
        let mut p = json!({"name": binding, "payload": text});
        if let Some(c) = ctx {
            p["executionContextId"] = json!(c);
        }
        st.lock()
            .unwrap()
            .inject("Runtime.bindingCalled", p, Some(&sess));
    };
    // Text, empty text, a release and API text never arm the window.
    ms.on_cmd(&e.server, "BP1", BrowserCmd::Text("hello".into()));
    ms.on_cmd(&e.server, "BP1", BrowserCmd::Text(String::new()));
    ms.on_cmd(&e.server, "BP1", click(MouseKind::Release));
    let r = e
        .call(
            &ctx(None),
            "browser.command",
            json!({"pane": "BP1", "cmd": "text", "text": "from an agent"}),
        )
        .await;
    assert!(r.get("result").is_some(), "{r}");
    assert!(t.st.lock().unwrap().io.last_input.is_none());
    write(Some(top), "after text");
    wait_for("blocked after text", blocked(1)).await;
    // A key press arms it, but an iframe's (or an isolated world's, or an unknown) context
    // still can't write, nor can a call that names no context.
    ms.on_cmd(&e.server, "BP1", key_press());
    st.lock().unwrap().inject(
        "Page.frameNavigated",
        json!({"frame": {"id": "IF", "parentId": "F", "url": "http://localhost:5173/frame", "loaderId": "LI"}}),
        Some(&sess),
    );
    st.lock().unwrap().inject(
        "Runtime.executionContextCreated",
        json!({"context": {"id": 700, "origin": "http://localhost:5173", "auxData": {"isDefault": true, "frameId": "IF"}}}),
        Some(&sess),
    );
    st.lock().unwrap().inject(
        "Runtime.executionContextCreated",
        json!({"context": {"id": 701, "origin": "http://localhost:5173", "auxData": {"isDefault": false, "frameId": "F"}}}),
        Some(&sess),
    );
    write(Some(700), "from the iframe");
    write(Some(701), "from an isolated world");
    write(Some(12345), "from nowhere");
    write(None, "no context");
    wait_for("iframe and others blocked", blocked(5)).await;
    // The top document's main world, right after the key press: forwarded.
    write(Some(top), "copied text");
    let f = until(&e, &mut ms, 10, |f| {
        matches!(f, ServerFrame::Clipboard { .. })
    })
    .await;
    let ServerFrame::Clipboard { data, .. } = f else {
        unreachable!()
    };
    assert_eq!(data, b"copied text");
    // A click, then the page navigates: neither the old document nor the new one may use
    // that click.
    ms.on_cmd(&e.server, "BP1", click(MouseKind::Press));
    ms.on_cmd(
        &e.server,
        "BP1",
        BrowserCmd::Navigate("http://localhost:5173/next".into()),
    );
    let next = top + 1;
    wait_for("the new document", || {
        t.st.lock().unwrap().io.capture.is_top_main_world(next)
    })
    .await;
    assert!(t.st.lock().unwrap().io.last_input.is_none(), "cleared");
    write(Some(top), "stale document");
    write(Some(next), "new document, old click");
    wait_for("blocked across navigation", blocked(7)).await;
    // A fresh click in the new document: forwarded.
    ms.on_cmd(&e.server, "BP1", click(MouseKind::Press));
    write(Some(next), "new document, new click");
    let f = until(&e, &mut ms, 10, |f| {
        matches!(f, ServerFrame::Clipboard { .. })
    })
    .await;
    let ServerFrame::Clipboard { data, .. } = f else {
        unreachable!()
    };
    assert_eq!(data, b"new document, new click");
    assert_eq!(t.st.lock().unwrap().io.clips, 2);
    ms.close(&e.server);
}

/// The whole console response is redacted (the page URL beside the entries too) and holds no
/// control characters; formatted for the split, hostile text is inert in a terminal.
#[tokio::test(flavor = "multi_thread")]
async fn console_responses_are_redacted_and_terminal_safe() {
    let e = Env::new();
    let secret_spec = BrowserPane {
        url: "http://localhost:5173/cb?token=abcd1234secret".into(),
        ..spec()
    };
    let fake = host(&e, secret_spec.clone());
    let mut ms = MediaSession::new(&e.server, false);
    let (st, sess) = streaming(&e, &fake, &mut ms, mp(secret_spec)).await;
    let t = e.server.browser.target("BP1").unwrap();
    let top = st.lock().unwrap().context(&sess);
    let hostile = "pwn \x1b]52;c;YXR0YWNrZXI=\x07 \x1b]0;title\x07 \x1b]9;hi\x07 \u{9b}2J";
    st.lock().unwrap().inject(
        "Runtime.consoleAPICalled",
        json!({"type": "error", "executionContextId": top, "args": [{"type": "string", "value": hostile}]}),
        Some(&sess),
    );
    wait_for("hostile entry", || {
        t.st.lock()
            .unwrap()
            .io
            .capture
            .console
            .iter()
            .any(|e| e["text"].as_str().is_some_and(|x| x.starts_with("pwn")))
    })
    .await;
    wait_for("page url", || t.st.lock().unwrap().url.contains("token=")).await;
    let full = ctx(None);
    for params in [
        json!({"pane": "BP1"}),
        json!({"pane": "BP1", "after": u64::MAX}),
        json!({"pane": "BP1", "kind": "network", "errors": true}),
    ] {
        for m in ["browser.console", "browser.network"] {
            let r = e.call(&full, m, params.clone()).await;
            let text = r.to_string();
            assert!(r.get("result").is_some(), "{r}");
            assert!(!text.contains("abcd1234secret"), "{m} {params}: {text}");
            assert!(r["result"]["url"].as_str().unwrap().contains("token="));
            assert!(
                !text.contains("\\u001b") && !text.contains("\\u0007"),
                "{text}"
            );
        }
    }
    // Relayed copies are served the same way (and the model URL is redacted).
    e.server
        .browser
        .relayed
        .lock()
        .unwrap()
        .insert("BP2".into(), page_io::PageIo::default().capture);
    let mut remote_spec = spec();
    remote_spec.url = "http://localhost:5173/?token=abcd1234secret".into();
    e.put_pane("BP2", "ws-a", Some(remote_spec));
    let r = e
        .call(&full, "browser.console", json!({"pane": "BP2"}))
        .await;
    assert_eq!(r["result"]["source"], "relayed", "{r}");
    assert!(!r.to_string().contains("abcd1234secret"), "{r}");
    // The split's view of the hostile entry: visible, and no terminal effect.
    let r = e
        .call(
            &full,
            "browser.console",
            json!({"pane": "BP1", "errors": true}),
        )
        .await;
    let entry = r["result"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["text"].as_str().is_some_and(|x| x.starts_with("pwn")))
        .cloned()
        .unwrap();
    assert!(
        entry["text"].as_str().unwrap().contains("\\x1b]52;"),
        "{entry}"
    );
    let mut engine = vk_term::Engine::new(160, 10, 100);
    let mut fx = Vec::new();
    let line = vk_browser::capture::format_line(&entry, 0);
    engine.feed(format!("{line}\r\n").as_bytes(), &mut fx);
    assert!(fx.is_empty(), "{fx:?}");
    assert!(engine.screen_text().contains("]52;c;YXR0YWNrZXI="));
    ms.close(&e.server);
}

/// A console split's output follows the `browser.console` rule: not archived or indexed, and
/// readable only by full scope, the split itself and the agent that opened its browser pane.
#[tokio::test(flavor = "multi_thread")]
async fn console_split_output_is_scoped_and_never_archived() {
    let e = Env::new();
    let _fake = host(&e, spec());
    // BP1 was opened by the agent in pane-b; CON1 is its console split; pane-a is unrelated.
    e.put_pane("pane-b", "ws-a", None);
    e.put_pane("CON1", "ws-a", None);
    {
        let mut c = e.server.core.lock().unwrap();
        let mut bp = c.pane("BP1").cloned().unwrap();
        bp.created_by = "agent:pane-b".into();
        let mut con = c.pane("CON1").cloned().unwrap();
        con.created_by = "browser-console:BP1".into();
        let mut tx = Tx::new();
        tx.pane(bp);
        tx.pane(con);
        e.server.commit(&mut c, tx).unwrap();
    }
    let (rt, _rx) = crate::pane::PaneRt::new("CON1", 80, 24);
    rt.screen.lock().unwrap().engine.feed(
        b"12:00:00.000 log    split secret 4711\r\n",
        &mut Vec::new(),
    );
    e.server
        .panes
        .lock()
        .unwrap()
        .insert("CON1".into(), rt.clone());
    let hits = |r: &Value| {
        r["result"]["hits"]
            .as_array()
            .map(|h| h.iter().filter(|x| x["pane"] == "CON1").count())
            .unwrap_or(0)
    };
    let search = json!({"q": "split secret 4711"});
    // Unrelated pane in the same workspace: direct reads refused, search finds nothing.
    let other = ctx(Some("pane-a"));
    for (m, p) in [
        ("pane.read", json!({"pane": "CON1", "source": "recent"})),
        ("pane.read", json!({"pane": "CON1", "source": "archive"})),
        (
            "pane.wait_output",
            json!({"pane": "CON1", "match": "split", "timeout_ms": 10}),
        ),
    ] {
        let r = e.call(&other, m, p.clone()).await;
        assert!(
            r["error"].to_string().contains("permission_denied"),
            "{m} {p}: {r}"
        );
    }
    let r = e.call(&other, "search.query", search.clone()).await;
    assert_eq!(hits(&r), 0, "{r}");
    let r = e
        .call(
            &other,
            "search.query",
            json!({"q": "split secret", "pane": "CON1"}),
        )
        .await;
    assert_eq!(hits(&r), 0, "{r}");
    // The split itself, the agent that opened the browser pane, and full scope may.
    for who in [Some("CON1"), Some("pane-b"), None] {
        let c = ctx(who);
        let r = e
            .call(&c, "pane.read", json!({"pane": "CON1", "source": "recent"}))
            .await;
        assert!(
            r["result"]["text"]
                .as_str()
                .is_some_and(|t| t.contains("split secret 4711")),
            "{who:?}: {r}"
        );
        let r = e.call(&c, "search.query", search.clone()).await;
        assert_eq!(hits(&r), 1, "{who:?}: {r}");
    }
    // Scrolled-off rows never reach the archive or the index.
    e.server.archive_rows(
        "CON1",
        vec![vk_store::archive::ArchivedRow {
            n: 0,
            t: "split secret 4711 archived".into(),
            w: false,
        }],
    );
    e.server.housekeeping();
    assert_eq!(e.server.archive_last_line("CON1"), None);
    // After the split closes, nothing of it is searchable (full scope, archive included).
    e.server.panes.lock().unwrap().remove("CON1");
    {
        let mut c = e.server.core.lock().unwrap();
        let con = c.pane("CON1").cloned().unwrap();
        let mut tx = Tx::new();
        tx.close_pane(&con);
        e.server.commit(&mut c, tx).unwrap();
    }
    let r = e
        .call(
            &ctx(None),
            "search.query",
            json!({"q": "split secret", "sources": ["live", "archive"]}),
        )
        .await;
    assert_eq!(hits(&r), 0, "{r}");
    let r = e
        .call(
            &ctx(None),
            "pane.read",
            json!({"pane": "CON1", "source": "archive"}),
        )
        .await;
    assert!(r.get("error").is_some(), "{r}");
}

/// Mobile emulation without a meta viewport: Chromium lays the page out 980 px wide and shows
/// it at page scale 393/980; pointer input is mapped through the frame's page scale.
#[tokio::test(flavor = "multi_thread")]
async fn letterboxed_input_follows_the_frames_page_scale() {
    let e = Env::new();
    let phone = BrowserPane {
        device: Some("iphone-15".into()),
        ..spec()
    };
    let fake = host(&e, phone.clone());
    let mut ms = MediaSession::new(&e.server, false);
    let mut wide = mp(phone);
    wide.cols = 40;
    wide.rows = 10;
    let (st, sess) = streaming(&e, &fake, &mut ms, wide).await;
    let t = e.server.browser.target("BP1").unwrap();
    st.lock().unwrap().set_page_scale(&sess, 393.0 / 980.0);
    wait_for("page scale in the frame metadata", || {
        t.st.lock()
            .unwrap()
            .io
            .meta
            .is_some_and(|m| (m.page_scale - 393.0 / 980.0).abs() < 1e-6)
    })
    .await;
    ms.on_cmd(
        &e.server,
        "BP1",
        BrowserCmd::Mouse {
            kind: MouseKind::Press,
            button: MouseButton::Left,
            x: 160.0,
            y: 80.0,
            mods: vk_proto::input::Mods::empty(),
            clicks: 1,
        },
    );
    wait_for("the centre click", || {
        st.lock()
            .unwrap()
            .calls("Input.dispatchMouseEvent")
            .iter()
            .filter(|(p, _)| p["type"] == "mousePressed")
            .count()
            >= 1
    })
    .await;
    let presses: Vec<Value> = st
        .lock()
        .unwrap()
        .calls("Input.dispatchMouseEvent")
        .into_iter()
        .filter(|(p, _)| p["type"] == "mousePressed")
        .map(|(p, _)| p)
        .collect();
    let last = presses.last().unwrap();
    let (x, y) = (last["x"].as_f64().unwrap(), last["y"].as_f64().unwrap());
    assert!(
        (x - 490.0).abs() < 4.0 && (y - 1062.0).abs() < 6.0,
        "{x},{y}"
    );
    // Unpinned at page scale 2 (zoomed in): CSS = pane CSS / 2.
    let mut fit = mp(spec());
    fit.cols = 40;
    fit.rows = 10;
    ms.on_view(&e.server, vec![fit], false, true);
    wait_for("unpinned", || {
        st.lock().unwrap().viewports() == vec![(320, 160)]
    })
    .await;
    st.lock().unwrap().set_page_scale(&sess, 2.0);
    wait_for("page scale 2", || {
        t.st.lock()
            .unwrap()
            .io
            .meta
            .is_some_and(|m| m.page_scale == 2.0 && m.device_width == 320.0)
    })
    .await;
    let n = presses.len();
    ms.on_cmd(
        &e.server,
        "BP1",
        BrowserCmd::Mouse {
            kind: MouseKind::Press,
            button: MouseButton::Left,
            x: 100.0,
            y: 60.0,
            mods: vk_proto::input::Mods::empty(),
            clicks: 1,
        },
    );
    wait_for("the unpinned click", || {
        st.lock()
            .unwrap()
            .calls("Input.dispatchMouseEvent")
            .iter()
            .filter(|(p, _)| p["type"] == "mousePressed")
            .count()
            > n
    })
    .await;
    let p = st
        .lock()
        .unwrap()
        .calls("Input.dispatchMouseEvent")
        .into_iter()
        .rfind(|(p, _)| p["type"] == "mousePressed")
        .unwrap()
        .0;
    assert_eq!((p["x"].as_f64(), p["y"].as_f64()), (Some(50.0), Some(30.0)));
    ms.close(&e.server);
}

#[test]
fn device_and_viewport_params_are_validated() {
    assert_eq!(
        pin_params(&json!({"device": "Pixel-8"})).unwrap(),
        (Some("pixel-8".to_string()), None)
    );
    assert_eq!(
        pin_params(&json!({"viewport": "390×844"})).unwrap(),
        (None, Some("390x844".to_string()))
    );
    assert_eq!(
        pin_params(&json!({"viewport": {"width": 1280, "height": 800}})).unwrap(),
        (None, Some("1280x800".to_string()))
    );
    assert_eq!(
        pin_params(&json!({"viewport": "fit"})).unwrap(),
        (None, None)
    );
    assert_eq!(pin_params(&json!({})).unwrap(), (None, None));
    for bad in [
        json!({"device": "nokia"}),
        json!({"viewport": "10x10"}),
        json!({"viewport": "wide"}),
        json!({"viewport": "390x844", "device": "ipad"}),
    ] {
        assert!(pin_params(&bad).is_err(), "{bad}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn device_and_viewport_are_persisted_on_the_pane() {
    let e = Env::new();
    let _fake = host(&e, spec());
    let full = ctx(None);
    let pin = || {
        e.server.with_core(|c| {
            let b = c.pane("BP1").unwrap().browser.clone().unwrap();
            (b.device, b.viewport)
        })
    };
    let r = e
        .call(
            &full,
            "browser.pane.update",
            json!({"pane": "BP1", "device": "ipad"}),
        )
        .await;
    assert!(r.get("result").is_some(), "{r}");
    assert_eq!(pin(), (Some("ipad".into()), None));
    e.call(
        &full,
        "browser.pane.update",
        json!({"pane": "BP1", "viewport": "390x844"}),
    )
    .await;
    assert_eq!(pin(), (None, Some("390x844".into())));
    let r = e
        .call(
            &full,
            "browser.pane.update",
            json!({"pane": "BP1", "device": "nokia"}),
        )
        .await;
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown device"),
        "{r}"
    );
    assert_eq!(pin(), (None, Some("390x844".into())));
    e.call(
        &full,
        "browser.pane.update",
        json!({"pane": "BP1", "viewport": "fit"}),
    )
    .await;
    assert_eq!(pin(), (None, None));
    // From a pane: refused (a user decision).
    let r = e
        .call(
            &ctx(Some("pane-a")),
            "browser.pane.update",
            json!({"pane": "BP1", "device": "ipad"}),
        )
        .await;
    assert!(r.get("error").is_some());
}
