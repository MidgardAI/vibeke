//! Goal 03 Codex review follow-up tests for the browser pane (kept in their own file so the
//! pane module's own tests stay untouched).

use super::*;
use crate::ServerOpts;
use crate::paths::Paths;
use std::sync::Once;
use vk_proto::model::Pane;

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let base = std::env::temp_dir().join(format!("vk-pane-followup-{}", std::process::id()));
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

pub(super) struct Env {
    pub _dir: tempfile::TempDir,
    pub server: Arc<Server>,
}

pub(super) fn ctx(scope: Option<&str>) -> Ctx {
    Ctx {
        client_id: format!("c-{}", scope.unwrap_or("user")),
        kind: "cli".into(),
        pane_scope: scope.map(str::to_string),
        remote: false,
    }
}

impl Env {
    pub fn new() -> Env {
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
        e.put_pane("pane-a", "ws-a", None);
        e
    }

    pub fn put_pane(&self, id: &str, ws: &str, browser: Option<BrowserPane>) {
        let p = Pane {
            id: id.into(),
            handle: id.into(),
            tab: format!("tab-{ws}"),
            workspace: ws.into(),
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
            browser,
        };
        let mut c = self.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.pane(p);
        self.server.commit(&mut c, tx).unwrap();
    }

    /// One JSON-RPC call through the real dispatch (authorization included).
    pub async fn call(&self, ctx: &Ctx, method: &str, params: Value) -> Value {
        let line = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let out = crate::api::handle_line(&self.server, ctx, &line.to_string()).await;
        serde_json::from_str(&out).unwrap()
    }
}

fn denied(v: &Value) -> Option<String> {
    let e = v.get("error")?;
    (e["data"]["kind"] == "permission_denied" || e.to_string().contains("permission_denied"))
        .then(|| e["message"].as_str().unwrap_or_default().to_string())
}

/// Finding 1: a remote-profile pane browser (SOCKS route) gets the WebRTC policy in the form
/// both the headless shell and full Chromium read; a local one has no proxy and no policy.
#[test]
fn pane_launch_flags_keep_webrtc_on_the_route() {
    let req = LaunchReq {
        profile: "devbox".into(),
        profile_dir: "/state/browser-profiles/devbox".into(),
        dpr: 2.0,
        socks_port: Some(41000),
        log: None,
    };
    for bin in ["/pw/chrome-headless-shell", "/pw/Google Chrome for Testing"] {
        let a = pane_launch_options(std::path::Path::new(bin), &req).args();
        assert!(a.contains(&"--proxy-server=socks5://127.0.0.1:41000".to_string()));
        assert!(
            a.contains(&"--force-webrtc-ip-handling-policy=disable_non_proxied_udp".to_string()),
            "{a:?}"
        );
        assert!(a.contains(&"--webrtc-ip-handling-policy=disable_non_proxied_udp".to_string()));
        assert!(!a.iter().any(|x| x == "--force-webrtc-ip-handling-policy"));
    }
    let local = LaunchReq {
        socks_port: None,
        ..req
    };
    let a = pane_launch_options(std::path::Path::new("/pw/chrome-headless-shell"), &local).args();
    assert!(!a.iter().any(|x| x.starts_with("--proxy-server")));
}

/// Finding 4: URLs are checked and stored in the canonical form Chromium loads.
#[test]
fn urls_are_canonical_and_parsed_like_the_browser() {
    assert_eq!(
        normalize_url("http://192.168.1.10\\@localhost:5173/").as_deref(),
        Some("http://192.168.1.10/@localhost:5173/")
    );
    assert_eq!(normalize_url("http://localhost@10.0.0.1/"), None);
    assert_eq!(
        normalize_url("2130706433:5173").as_deref(),
        Some("http://127.0.0.1:5173/")
    );
    // A remote owner's record may only start on that machine's loopback.
    assert_eq!(
        initial_url("http://192.168.1.10\\@localhost:5173/", true),
        None
    );
    assert_eq!(
        initial_url("http://[::ffff:192.168.1.10]:5173/", true),
        None
    );
    assert_eq!(
        initial_url("http://0x7f.1:5173/", true).as_deref(),
        Some("http://127.0.0.1:5173/")
    );
    assert_eq!(
        initial_url("http://[::ffff:127.0.0.1]:5173/", true).as_deref(),
        Some("http://[::ffff:7f00:1]:5173/")
    );
    assert_eq!(
        initial_url("http://0.0.0.0:5173/", true).as_deref(),
        Some("http://0.0.0.0:5173/")
    );
    assert_eq!(initial_url("http://u:p@localhost:5173/", true), None);
    assert_eq!(
        initial_url("https://example.com/", false).as_deref(),
        Some("https://example.com/")
    );
}

/// Findings 4 and 6 through the real dispatch: a pane token can't open a non-loopback URL
/// disguised with a backslash, and can't create a browser pane (or open a preview) for
/// another machine; the same calls with a full-scope token get past authorization.
#[tokio::test(flavor = "multi_thread")]
async fn pane_scope_cannot_create_cross_machine_or_disguised_panes() {
    let e = Env::new();
    let pane = ctx(Some("pane-a"));
    let r = e
        .call(
            &pane,
            "browser.pane.create",
            json!({"url": "http://localhost:5173/", "machine": "devbox", "pane": "pane-a"}),
        )
        .await;
    let why = denied(&r).unwrap_or_else(|| panic!("cross-machine create allowed: {r}"));
    assert!(why.contains("another machine"), "{why}");
    let r = e
        .call(
            &pane,
            "preview.open",
            json!({"url": "http://localhost:5173/", "machine": "devbox"}),
        )
        .await;
    assert!(denied(&r).is_some(), "{r}");
    let r = e
        .call(
            &pane,
            "browser.pane.create",
            json!({"preview": "devbox/p1", "pane": "pane-a"}),
        )
        .await;
    assert!(denied(&r).is_some(), "{r}");
    // The Codex counterexample: WHATWG says the host is 192.168.1.10.
    let r = e
        .call(
            &pane,
            "browser.pane.create",
            json!({"url": "http://192.168.1.10\\@localhost:5173/", "pane": "pane-a"}),
        )
        .await;
    let why = denied(&r).unwrap_or_else(|| panic!("disguised LAN URL allowed: {r}"));
    assert!(why.contains("loopback"), "{why}");
    let r = e
        .call(
            &pane,
            "preview.open",
            json!({"url": "http://192.168.1.10\\@localhost:5173/", "window": true}),
        )
        .await;
    assert!(denied(&r).is_some(), "{r}");
    // Naming this machine is fine from a pane (authorization passes; the pane is created).
    let r = e
        .call(
            &pane,
            "browser.pane.create",
            json!({"url": "http://localhost:5173/", "machine": "testbox", "pane": "pane-a"}),
        )
        .await;
    assert!(denied(&r).is_none(), "{r}");
    // Full scope may pick another machine (the pane record is created; nothing launches
    // until a client views it).
    let r = e
        .call(
            &ctx(None),
            "browser.pane.create",
            json!({"url": "http://localhost:5173/", "machine": "devbox", "pane": "pane-a"}),
        )
        .await;
    assert!(denied(&r).is_none(), "{r}");
}

// ---- media host lifecycle with the fake Chromium (findings 10, 12, 13) ---------------------------

fn fake_host(e: &Env) -> Arc<FakeLauncher> {
    e.server
        .browser
        .set_profiles_root(e._dir.path().join("profiles"));
    let fake = Arc::new(FakeLauncher::default());
    e.server.browser.set_launcher(fake.clone());
    // The pane's own record (a local pane missing from the model is closed by the GC).
    e.put_pane(
        "BP1",
        "ws-a",
        Some(BrowserPane {
            url: "http://localhost:5173/".into(),
            source_pane: Some("pane-a".into()),
            ..Default::default()
        }),
    );
    fake
}

fn media_pane(dpr: f32) -> MediaPane {
    MediaPane {
        pane: "BP1".into(),
        owner: String::new(),
        spec: BrowserPane {
            url: "http://localhost:5173/".into(),
            ..Default::default()
        },
        cols: 8,
        rows: 4,
        cell_w: 16,
        cell_h: 32,
        dpr,
    }
}

fn decode_frames(buf: &mut Vec<u8>) -> Vec<ServerFrame> {
    let mut fb = vk_proto::frame::FrameBuf::default();
    fb.push(buf);
    buf.clear();
    let mut v = Vec::new();
    while let Some(f) = fb.next_frame::<ServerFrame>().unwrap() {
        v.push(f);
    }
    v
}

/// Flush until a media frame matching `want` arrives (acking when `ack`).
async fn media_until(
    server: &Arc<Server>,
    ms: &mut MediaSession,
    ack: bool,
    secs: u64,
    want: impl Fn(&MediaFrame) -> bool,
) -> MediaFrame {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let mut buf = Vec::new();
        ms.flush(server, &mut buf).await.unwrap();
        for f in decode_frames(&mut buf) {
            if let ServerFrame::Media(m) = f {
                if ack {
                    ms.on_ack(&m.pane, m.seq);
                }
                if want(&m) {
                    return *m;
                }
            }
        }
        assert!(Instant::now() < deadline, "no matching media frame");
        let _ = tokio::time::timeout(Duration::from_millis(50), ms.notify.notified()).await;
    }
}

async fn wait_for(what: &str, secs: u64, f: impl Fn() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(
            t.elapsed() < Duration::from_secs(secs),
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The relaunched browser has the pane's page screencasting again, and a key changes the
/// pixels the client receives.
async fn streams_again(
    e: &Env,
    ms: &mut MediaSession,
    fake: &Arc<Mutex<vk_browser::fake_chromium::State>>,
) -> MediaFrame {
    wait_for("page re-created and screencasting", 10, || {
        fake.lock().unwrap().screencasting() == 1
            && e.server
                .browser
                .target("BP1")
                .is_some_and(|t| t.page().is_some())
    })
    .await;
    // Drain what is pending, then type.
    let mut buf = Vec::new();
    ms.flush(&e.server, &mut buf).await.unwrap();
    for f in decode_frames(&mut buf) {
        if let ServerFrame::Media(m) = f {
            ms.on_ack(&m.pane, m.seq);
        }
    }
    ms.on_cmd(
        &e.server,
        "BP1",
        BrowserCmd::Key(vk_proto::input::KeyEvent::ch('a')),
    );
    media_until(&e.server, ms, true, 10, |m| !m.tiles.is_empty()).await
}

/// Finding 10: a crashed Chromium is relaunched (after the 3 s backoff) and the visible pane
/// gets frames again: the pump thread now carries a runtime handle.
#[tokio::test(flavor = "multi_thread")]
async fn chromium_crash_relaunches_and_frames_resume() {
    let e = Env::new();
    let fake = fake_host(&e);
    let mut ms = MediaSession::new(&e.server, false);
    ms.on_view(&e.server, vec![media_pane(2.0)], false, true);
    media_until(&e.server, &mut ms, true, 10, |m| m.reset).await;
    let first = fake.launched.lock().unwrap()[0].1.clone();
    first.lock().unwrap().crash();
    wait_for("relaunch after crash", 10, || {
        fake.launched.lock().unwrap().len() == 2
    })
    .await;
    // The re-created page streams again: input reaches it and its pixels arrive.
    let second = fake.launched.lock().unwrap()[1].1.clone();
    streams_again(&e, &mut ms, &second).await;
    assert_eq!(
        second.lock().unwrap().urls(),
        vec!["http://localhost:5173/".to_string()]
    );
    ms.close(&e.server);
}

/// Finding 10: a DPR change closes the profile's Chromium (waiting for it) and relaunches it
/// at the new DPR right away; the pane keeps streaming.
#[tokio::test(flavor = "multi_thread")]
async fn dpr_change_relaunches_at_the_new_dpr() {
    let e = Env::new();
    let fake = fake_host(&e);
    let mut ms = MediaSession::new(&e.server, false);
    ms.on_view(&e.server, vec![media_pane(2.0)], false, true);
    media_until(&e.server, &mut ms, true, 10, |m| m.reset).await;
    // Same cells, host moved to a DPR 1 screen.
    ms.on_view(&e.server, vec![media_pane(1.0)], false, true);
    wait_for("relaunch at DPR 1", 5, || {
        fake.launched.lock().unwrap().len() == 2
    })
    .await;
    {
        let l = fake.launched.lock().unwrap();
        assert!((l[0].0.dpr - 2.0).abs() < 1e-6);
        assert!(
            (l[1].0.dpr - 1.0).abs() < 1e-6,
            "relaunched at {}",
            l[1].0.dpr
        );
        assert!(l[0].1.lock().unwrap().closed, "the old browser was closed");
    }
    let second = fake.launched.lock().unwrap()[1].1.clone();
    let m = streams_again(&e, &mut ms, &second).await;
    assert_eq!(
        (m.width, m.height),
        (128, 128),
        "device px = cells × cell px"
    );
    // CSS viewport at DPR 1 is the device size.
    assert_eq!(second.lock().unwrap().viewports(), vec![(128, 128)]);
    ms.close(&e.server);
}

/// Finding 12: shm tiles sent for a pane that is then hidden stay tracked until acked or the
/// client goes away; a hide/disconnect race leaves no shm objects behind.
#[tokio::test(flavor = "multi_thread")]
async fn hidden_pane_shm_is_freed_on_disconnect() {
    let e = Env::new();
    let _fake = fake_host(&e);
    let mut ms = MediaSession::new(&e.server, false);
    ms.on_view(&e.server, vec![media_pane(2.0)], true, true);
    // Don't ack: the client hasn't consumed the frame yet.
    let m = media_until(&e.server, &mut ms, false, 10, |m| m.reset).await;
    let names: Vec<String> = m
        .tiles
        .iter()
        .filter_map(|t| match &t.data {
            TileData::Shm { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(names.len(), 4, "shm tiles for a same-machine client");
    for n in &names {
        assert!(vk_browser::kitty::shm::is_tile_name_for(n, "BP1"), "{n}");
        assert!(vk_browser::kitty::shm::object_size(n).is_ok(), "{n}");
    }
    // Hidden before the client consumed it, then the client disconnects.
    ms.on_view(&e.server, vec![], true, true);
    let outstanding = ms.outstanding_shm();
    for n in &names {
        assert!(outstanding.contains(n), "{n} no longer tracked after hide");
    }
    ms.close(&e.server);
    for n in &names {
        assert!(
            vk_browser::kitty::shm::object_size(n).is_err(),
            "{n} leaked after disconnect"
        );
    }
    // An ack after hide (the client consumed or dropped it) ends the tracking without
    // unlinking (the terminal owns the object then).
    let mut ms = MediaSession::new(&e.server, false);
    ms.on_view(&e.server, vec![media_pane(2.0)], true, true);
    let m = media_until(&e.server, &mut ms, false, 10, |m| m.reset).await;
    ms.on_view(&e.server, vec![], true, true);
    ms.on_ack(&m.pane, m.seq);
    assert!(ms.outstanding_shm().is_empty());
    for t in &m.tiles {
        if let TileData::Shm { name, .. } = &t.data {
            vk_browser::kitty::shm::unlink(name);
        }
    }
    ms.close(&e.server);
}

/// Finding 13: the pane's screenshot action records a `screenshot` entity through
/// `record_screenshot` (local_pane, profile, taken_by user, document identity), visible in the
/// screenshot API and subject to retention; nothing is written under `<state>/screenshots`.
#[tokio::test(flavor = "multi_thread")]
async fn pane_screenshot_is_a_managed_record() {
    let e = Env::new();
    let _fake = fake_host(&e);
    let mut ms = MediaSession::new(&e.server, false);
    ms.on_view(&e.server, vec![media_pane(2.0)], false, true);
    media_until(&e.server, &mut ms, true, 10, |m| m.reset).await;
    let t = e.server.browser.target("BP1").unwrap();
    let m = screenshot(&e.server, &t).await.unwrap();
    assert_eq!(m.environment.kind.as_str(), "local_pane");
    assert_eq!(m.environment.profile.as_deref(), Some("local"));
    assert_eq!(m.taken_by.kind, "user");
    assert_eq!(m.label, "your browser pane · profile local");
    assert_eq!(
        m.document.as_ref().map(|d| d.href.as_str()),
        Some("http://localhost:5173/")
    );
    assert_eq!(
        m.binding,
        crate::screenshots::ScreenshotBinding::Illustrative
    );
    assert!(m.path(&e.server).exists());
    assert!(!e.server.paths.state.join("screenshots").exists());
    let list = crate::screenshots::api(&e.server, &ctx(None), "screenshot.list", &json!({}))
        .await
        .unwrap()
        .unwrap();
    assert!(list.to_string().contains(&m.id), "{list}");
    // The action from the pane's chrome reports the record.
    command(&e.server, "BP1", BrowserCmd::Screenshot, false);
    wait_for("screenshot notice", 10, || {
        t.st.lock()
            .unwrap()
            .notice
            .as_deref()
            .is_some_and(|n| n.starts_with("screenshot s2 saved"))
    })
    .await;
    // Retention applies (unreferenced, older than keep_days).
    let cfg = crate::screenshots::ScreenshotConfig {
        keep_days: 1,
        ..Default::default()
    };
    crate::screenshots::cleanup_with(
        &e.server,
        vk_store::now_ms() + 3 * 24 * 3600 * 1000,
        &cfg,
        None,
    );
    assert!(crate::screenshots::find(&e.server, &m.id).is_none());
    ms.close(&e.server);
}

// ---- finding 8: media never holds terminal frames ---------------------------------------------

/// A writer that drains at `rate` bytes/s (a slow SSH link): a write waits until the bytes
/// before it have drained.
struct Throttled {
    data: Vec<u8>,
    rate: f64,
    ready_at: tokio::time::Instant,
    sleep: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
}

impl Throttled {
    fn new(rate: f64) -> Self {
        Throttled {
            data: Vec::new(),
            rate,
            ready_at: tokio::time::Instant::now(),
            sleep: None,
        }
    }
}

impl tokio::io::AsyncWrite for Throttled {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        use std::future::Future;
        let now = tokio::time::Instant::now();
        if now < self.ready_at {
            let at = self.ready_at;
            let s = self
                .sleep
                .get_or_insert_with(|| Box::pin(tokio::time::sleep_until(at)));
            s.as_mut().reset(at);
            if s.as_mut().poll(cx).is_pending() {
                return std::task::Poll::Pending;
            }
        }
        let n = buf.len().min(16 * 1024);
        self.data.extend_from_slice(&buf[..n]);
        let rate = self.rate;
        self.ready_at = tokio::time::Instant::now() + Duration::from_secs_f64(n as f64 / rate);
        std::task::Poll::Ready(Ok(n))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// Finding 8: a large media frame on a slow link goes out in parts within a per-pass budget,
/// so the render loop's cell frames (typing) keep a small latency bound while the frame is
/// in flight; the parts reassemble into the whole frame and are acked one by one.
#[tokio::test(flavor = "multi_thread")]
async fn media_parts_keep_cells_responsive_on_a_slow_link() {
    let e = Env::new();
    let fake = fake_host(&e);
    fake.noise.store(true, Ordering::Relaxed);
    // A remote client (zlib tiles) on a 2 MB/s link, 960×960 px of noise (~3 MB per frame).
    let mut ms = MediaSession::new(&e.server, true);
    let mut mp = media_pane(2.0);
    mp.cols = 60;
    mp.rows = 30;
    ms.on_view(&e.server, vec![mp], false, true);
    let mut sink = Throttled::new(2_000_000.0);
    let cell_frame = vec![b'x'; 200];
    let (mut tiles, mut media_bytes, mut parts) = (0usize, 0usize, 0usize);
    let (mut max_cell, mut max_flush) = (Duration::ZERO, Duration::ZERO);
    let mut grid = 0usize;
    let mut resets = 0;
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = 0usize;
    loop {
        assert!(Instant::now() < deadline, "frame never completed");
        // A keystroke's cell update, written first in the pass (03 §5).
        let t0 = Instant::now();
        tokio::io::AsyncWriteExt::write_all(&mut sink, &cell_frame)
            .await
            .unwrap();
        max_cell = max_cell.max(t0.elapsed());
        let t1 = Instant::now();
        ms.flush(&e.server, &mut sink).await.unwrap();
        max_flush = max_flush.max(t1.elapsed());
        // The client side: decode what arrived after the cell bytes, ack each part.
        let mut fb = vk_proto::frame::FrameBuf::default();
        let new = sink.data[seen..].to_vec();
        seen = sink.data.len();
        let stripped: Vec<u8> = {
            // Remove the 200-byte cell markers (not frames) from this pass's bytes.
            let mut v = new;
            if v.starts_with(&cell_frame) {
                v.drain(..cell_frame.len());
            }
            v
        };
        fb.push(&stripped);
        while let Some(f) = fb.next_frame::<ServerFrame>().unwrap() {
            if let ServerFrame::Media(m) = f {
                parts += 1;
                if m.reset {
                    resets += 1;
                    grid = m.grid_cols as usize * m.grid_rows as usize;
                }
                tiles += m.tiles.len();
                media_bytes += m
                    .tiles
                    .iter()
                    .map(|t| match &t.data {
                        TileData::ZlibRgba(z) => z.len(),
                        _ => 0,
                    })
                    .sum::<usize>();
                ms.on_ack(&m.pane, m.seq);
            }
        }
        if grid > 0 && tiles >= grid {
            break;
        }
        // The loop sleeps only when nothing is pending (media_wake would fire).
        let _ = tokio::time::timeout(Duration::from_millis(20), ms.notify.notified()).await;
    }
    assert_eq!(resets, 1, "only the first part resets");
    assert_eq!(tiles, grid, "the parts make up the whole frame");
    assert!(parts > 4, "split into parts ({parts})");
    assert!(
        media_bytes > 2_000_000,
        "a frame that takes >1 s on this link ({media_bytes} B)"
    );
    // Unbounded, one flush would take the whole frame's transfer time.
    let whole = Duration::from_secs_f64(media_bytes as f64 / 2_000_000.0);
    assert!(
        // Relative bound only: absolute wall-clock limits flake on a loaded machine.
        max_flush * 2 < whole,
        "a flush held the loop for {max_flush:?} (frame {whole:?})"
    );
    assert!(
        max_cell < Duration::from_millis(300) && max_cell * 4 < whole,
        "a cell frame waited {max_cell:?} behind media"
    );
    assert!(ms.outstanding_shm().is_empty());
    assert_eq!(ms.frames_in_flight("BP1"), 0, "every part acked");
    ms.close(&e.server);
}

#[test]
fn split_frame_parts_share_seq_and_reset_once() {
    let tile = |i: u32, n: usize| MediaTile {
        index: i,
        col: 0,
        row: 0,
        cols: 4,
        rows: 2,
        w: 64,
        h: 64,
        data: TileData::ZlibRgba(vec![0; n]),
    };
    let f = MediaFrame {
        pane: "P".into(),
        seq: 9,
        width: 640,
        height: 640,
        cell_w: 16,
        cell_h: 32,
        tile_cols: 4,
        tile_rows: 2,
        grid_cols: 10,
        grid_rows: 10,
        reset: true,
        tiles: (0..10).map(|i| tile(i, 10_000)).collect(),
    };
    let parts = split_frame(f, 32 * 1024);
    assert_eq!(parts.len(), 4, "3+3+3+1 tiles of 10 kB");
    assert!(parts[0].frame.reset && parts[1..].iter().all(|p| !p.frame.reset));
    assert!(
        parts
            .iter()
            .all(|p| p.frame.seq == 9 && p.frame.pane == "P")
    );
    assert_eq!(parts.iter().map(|p| p.frame.tiles.len()).sum::<usize>(), 10);
    // A single oversized tile is still one part.
    let big = MediaFrame {
        tiles: vec![tile(0, 100_000)],
        reset: false,
        ..parts[0].frame.clone()
    };
    assert_eq!(split_frame(big, 32 * 1024).len(), 1);
}

// ---- finding 9: render protocol negotiation ------------------------------------------------

/// `render.attach` from a client of another render protocol (an old client sends none) is
/// refused with `version_mismatch` and an upgrade hint; the current one gets the stream.
#[tokio::test(flavor = "multi_thread")]
async fn render_attach_refuses_other_protocol_versions() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let e = Env::new();
    for (params, ok) in [
        (json!({"client_id": "old"}), false),
        (json!({"client_id": "older", "protocol": 1}), false),
        (json!({"client_id": "newer", "protocol": 99}), false),
        (
            json!({"client_id": "same", "protocol": vk_proto::render::PROTOCOL}),
            true,
        ),
    ] {
        let (client, server_end) = tokio::io::duplex(1 << 20);
        let conn = tokio::spawn(crate::run::connection(e.server.clone(), server_end, None));
        let (rd, mut wr) = tokio::io::split(client);
        let mut rd = tokio::io::BufReader::new(rd);
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": "render.attach", "params": params});
        wr.write_all(format!("{req}\n").as_bytes()).await.unwrap();
        let mut line = String::new();
        rd.read_line(&mut line).await.unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        if ok {
            assert_eq!(v["result"]["protocol"], vk_proto::render::PROTOCOL);
            assert!(vk_proto::render::check_attach_reply(&v).is_ok());
            let hello: ServerFrame = vk_proto::frame::asyncio::read_frame(&mut rd).await.unwrap();
            assert!(
                matches!(hello, ServerFrame::Hello { protocol, .. } if protocol == vk_proto::render::PROTOCOL)
            );
            drop(wr);
            drop(rd);
        } else {
            assert_eq!(v["error"]["data"]["kind"], "version_mismatch", "{v}");
            assert_eq!(
                v["error"]["data"]["details"]["server_protocol"],
                vk_proto::render::PROTOCOL
            );
            let msg = v["error"]["message"].as_str().unwrap();
            assert!(msg.contains("upgrade"), "{msg}");
            // The connection ends; no binary frame follows.
            let mut rest = Vec::new();
            tokio::io::AsyncReadExt::read_to_end(&mut rd, &mut rest)
                .await
                .unwrap();
            assert!(rest.is_empty());
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), conn).await;
    }
}

// ---- user-found bug A: harness listeners suggested as previews --------------------------------

/// Listeners in a pane's process tree that speak HTTP but aren't web pages (a harness's 426
/// websocket bridge, a 400, a JSON-RPC endpoint) are not suggested; a dev server is. The
/// pane's "process tree" is this test process.
#[tokio::test(flavor = "multi_thread")]
async fn harness_style_listeners_are_not_suggested() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    async fn serve(resp: &'static str) -> u16 {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut b = [0u8; 512];
                let _ = s.read(&mut b).await;
                let _ = s.write_all(resp.as_bytes()).await;
            }
        });
        p
    }
    let e = Env::new();
    let ws = serve("HTTP/1.1 426 Upgrade Required\r\nConnection: close\r\n\r\n").await;
    let bad = serve("HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n").await;
    let json = serve(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"jsonrpc\":\"2.0\"}",
    )
    .await;
    let web = serve(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n<!doctype html><html></html>",
    )
    .await;
    {
        let mut c = e.server.core.lock().unwrap();
        let mut p = c.pane("pane-a").cloned().unwrap();
        p.child_pid = Some(std::process::id());
        p.fg_cmdline = vec!["node".into(), "server.js".into()];
        let mut tx = Tx::new();
        tx.pane(p);
        e.server.commit(&mut c, tx).unwrap();
    }
    let cfg = PreviewConfig::default();
    crate::preview::discover(&e.server, &cfg).await;
    let ports: Vec<u16> = e
        .server
        .with_core(|c| c.model.previews.iter().map(|p| p.port).collect());
    assert!(
        ports.contains(&web),
        "the dev server is suggested: {ports:?}"
    );
    for p in [ws, bad, json] {
        assert!(!ports.contains(&p), "port {p} suggested: {ports:?}");
    }
}
