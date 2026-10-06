//! Browser pane page I/O against the in-process fake Chromium (06 B3.2): console/network
//! capture and its API, relaying to a remote owner, the page clipboard, files into the page and
//! pinned (letterboxed) device viewports.

use super::followup_tests::{Env, ctx};
use super::*;
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

    // The media-host side: a target owned by a remote machine queues entries for relaying,
    // and drops them once the page leaves loopback.
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
        !t.st.lock().unwrap().io.relay.is_empty()
    })
    .await;
    t.st.lock().unwrap().url = "https://bank.example/".into();
    page_io::relay(&e.server);
    assert!(t.st.lock().unwrap().io.relay.is_empty());
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

#[tokio::test(flavor = "multi_thread")]
async fn files_are_dropped_or_attached_to_an_open_chooser_after_checks() {
    let e = Env::new();
    let fake = host(&e, spec());
    let mut ms = MediaSession::new(&e.server, false);
    let (st, sess) = streaming(&e, &fake, &mut ms, mp(spec())).await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("shot.png");
    std::fs::write(&file, b"\x89PNG fake").unwrap();
    let canon = file.canonicalize().unwrap().to_string_lossy().into_owned();
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
        BrowserCmd::DropFiles(vec![file.to_string_lossy().into_owned()]),
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
        assert_eq!(drags[2].0["data"]["files"], json!([canon]));
        assert_eq!(
            (drags[2].0["x"].as_f64(), drags[2].0["y"].as_f64()),
            (Some(12.0), Some(20.0))
        );
        assert_eq!(drags[2].1.as_deref(), Some(sess.as_str()));
    }
    let notice = until(&e, &mut ms, 10, |f| {
        matches!(f, ServerFrame::BrowserState { state, .. } if state.notice.as_deref().is_some_and(|n| n.starts_with("dropped 1 file")))
    })
    .await;
    drop(notice);
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
    ms.on_cmd(&e.server, "BP1", BrowserCmd::DropFiles(vec![canon.clone()]));
    wait_for("setFileInputFiles", || {
        !st.lock().unwrap().calls("DOM.setFileInputFiles").is_empty()
    })
    .await;
    let set = st.lock().unwrap().calls("DOM.setFileInputFiles");
    assert_eq!(set[0].0, json!({"files": [canon], "backendNodeId": 42}));
    // Refused: a directory, a relative path, a file over 50 MiB; nothing reaches the page.
    let big = dir.path().join("big.bin");
    std::fs::File::create(&big)
        .unwrap()
        .set_len(page_io::DROP_MAX + 1)
        .unwrap();
    for (p, why) in [
        (
            dir.path().to_string_lossy().into_owned(),
            "not a regular file",
        ),
        ("relative/file.txt".to_string(), "not an absolute path"),
        (big.to_string_lossy().into_owned(), "larger than 50 MiB"),
    ] {
        assert!(
            page_io::check_drop_path(&p).unwrap_err().contains(why),
            "{p}"
        );
        ms.on_cmd(&e.server, "BP1", BrowserCmd::DropFiles(vec![p.clone()]));
        until(&e, &mut ms, 10, |f| {
            matches!(f, ServerFrame::BrowserState { state, .. } if state.notice.as_deref().is_some_and(|n| n.starts_with("not dropped") && n.contains(why)))
        })
        .await;
    }
    assert_eq!(st.lock().unwrap().calls("Input.dispatchDragEvent").len(), 3);
    assert_eq!(st.lock().unwrap().calls("DOM.setFileInputFiles").len(), 1);
    ms.close(&e.server);
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
