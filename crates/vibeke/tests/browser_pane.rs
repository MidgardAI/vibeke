//! Goal 03 Stage 2 end to end (06 B3.2): `vibeke preview open <url> --split right` creates a
//! browser pane in the layout; the local server renders it with a fake Chromium
//! (`vibeke debug fake-chromium` behind `VIBEKE_CHROMIUM`, speaking CDP over the debugging
//! pipe like the real one) and streams changed tiles on the render stream's media channel to
//! a minimal render client in this test. Keys reach the page, navigation persists in the pane
//! record, hidden panes stop their screencast, the page comes back at its last URL after a
//! server restart, and closing the pane closes its target.
//!
//! `VIBEKE_BROWSER_TESTS=1` runs the same pipeline against Playwright's headless Chromium (temp
//! profile under the test's state dir) with a real page, asserting tiles arrive and a key
//! reaches the page, and prints key→tile latency and scroll fps through the whole pipeline.
//! The user's real browser and profiles are never used, and nothing is drawn to a terminal.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use vk_proto::model::BrowserPane;
use vk_proto::render::{
    BrowserCmd, BrowserStatus, ClientFrame, MediaFrame, MediaPane, ServerFrame, TileData,
};

struct Session {
    dir: tempfile::TempDir,
    env: Vec<(String, String)>,
}

impl Session {
    fn new(fake: bool) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("vkbp")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::write(dir.path().join("config.toml"), "").unwrap();
        let mut env = vec![];
        if fake {
            let script = dir.path().join("fake-chromium.sh");
            std::fs::write(
                &script,
                format!(
                    "#!/bin/sh\nexec '{}' debug fake-chromium \"$@\"\n",
                    env!("CARGO_BIN_EXE_vibeke")
                ),
            )
            .unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            env.push(("VIBEKE_CHROMIUM".into(), script.display().to_string()));
            env.push((
                "VIBEKE_FAKE_CHROMIUM_LOG".into(),
                dir.path().join("cdp.log").display().to_string(),
            ));
            // The "headful window" for the handover: a process that just lives (06 B3.3).
            let window = dir.path().join("fake-window.sh");
            std::fs::write(&window, "#!/bin/sh\nexec sleep 60\n").unwrap();
            std::fs::set_permissions(&window, std::fs::Permissions::from_mode(0o755)).unwrap();
            env.push(("VIBEKE_BROWSER".into(), window.display().to_string()));
        }
        Session { dir, env }
    }
    fn path(&self) -> &Path {
        self.dir.path()
    }
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_vibeke"));
        let d = self.dir.path();
        c.env("VIBEKE_RUNTIME_DIR", d.join("run"))
            .env("VIBEKE_STATE_DIR", d.join("state"))
            .env("VIBEKE_CONFIG", d.join("config.toml"))
            .env_remove("VIBEKE_CHROMIUM");
        for k in [
            "VIBEKE",
            "VIBEKE_SOCKET",
            "VIBEKE_SESSION",
            "VIBEKE_PANE_TOKEN",
            "VIBEKE_PANE_ID",
        ] {
            c.env_remove(k);
        }
        for (k, v) in &self.env {
            c.env(k, v);
        }
        c.arg("--json").args(args);
        c
    }
    fn json(&self, args: &[&str]) -> Value {
        let out = self.cmd(args).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}\n{}",
            String::from_utf8_lossy(&out.stderr),
            self.log_tail()
        );
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }
    fn log_tail(&self) -> String {
        let log = std::fs::read_to_string(self.path().join("state/default/logs/server.log"))
            .unwrap_or_default();
        let lines: Vec<&str> = log.lines().collect();
        lines[lines.len().saturating_sub(30)..].join("\n")
    }
    fn socket(&self) -> PathBuf {
        self.path().join("run/default/vibeke.sock")
    }
    /// CDP commands the fake browser received (one JSON object per line; `argv` lines mark
    /// launches).
    fn cdp_log(&self) -> Vec<Value> {
        std::fs::read_to_string(self.path().join("cdp.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
    fn wait_cdp(&self, what: &str, f: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        let t0 = Instant::now();
        loop {
            let log = self.cdp_log();
            if f(&log) {
                return log;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(15),
                "timed out waiting for {what}; launches {:?}; cdp log: {:#?}\n{}\nbrowser log: {}",
                log.iter()
                    .filter(|v| v.get("argv").is_some())
                    .collect::<Vec<_>>(),
                &log[log.len().saturating_sub(8)..],
                self.log_tail(),
                std::fs::read_to_string(
                    self.path()
                        .join("state/default/logs/browser-pane-local.log")
                )
                .unwrap_or_default(),
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    fn browser_spec(&self, pane: &str) -> BrowserPane {
        let p = self.json(&["pane", "get", pane]);
        let b = p
            .get("pane")
            .and_then(|x| x.get("browser"))
            .or_else(|| p.get("browser"))
            .cloned()
            .unwrap_or(Value::Null);
        serde_json::from_value(b).unwrap_or_else(|e| panic!("no browser spec in {p}: {e}"))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop", "--kill-panes"]).output();
    }
}

fn count(log: &[Value], method: &str) -> usize {
    log.iter().filter(|v| v["method"] == method).count()
}

/// A minimal render-stream client: `render.attach`, then postcard frames.
struct Render {
    w: UnixStream,
    rx: mpsc::Receiver<ServerFrame>,
    next_input: u64,
}

impl Render {
    fn attach(sock: &Path, id: &str) -> Render {
        let s = UnixStream::connect(sock).expect("connect render socket");
        let mut w = s.try_clone().unwrap();
        let req = json!({"jsonrpc":"2.0","id":1,"method":"render.attach","params":{"client_id": id, "protocol": vk_proto::render::PROTOCOL, "caps": {"max_fps": 60}}});
        w.write_all(format!("{req}\n").as_bytes()).unwrap();
        let mut rd = BufReader::new(s);
        let mut line = String::new();
        rd.read_line(&mut line).unwrap();
        assert!(!line.contains("\"error\""), "render.attach: {line}");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(f) = vk_proto::frame::read_frame::<_, ServerFrame>(&mut rd) {
                if tx.send(f).is_err() {
                    break;
                }
            }
        });
        Render {
            w,
            rx,
            next_input: 1,
        }
    }
    fn send(&mut self, f: &ClientFrame) {
        vk_proto::frame::write_frame(&mut self.w, f).unwrap();
        self.w.flush().unwrap();
    }
    fn view(&mut self, panes: Vec<MediaPane>) {
        self.send(&ClientFrame::MediaView {
            panes,
            shm: true,
            key_releases: false,
        });
    }
    fn cmd(&mut self, pane: &str, cmd: BrowserCmd) {
        let input_id = self.next_input;
        self.next_input += 1;
        self.send(&ClientFrame::Browser {
            input_id,
            pane: pane.into(),
            cmd,
        });
    }
    /// Next media frame matching `f` (acking every media frame); states seen on the way.
    fn media(
        &mut self,
        f: impl Fn(&MediaFrame) -> bool,
        timeout: Duration,
    ) -> (MediaFrame, Vec<BrowserStatus>) {
        let t0 = Instant::now();
        let mut states = vec![];
        loop {
            let left = timeout.saturating_sub(t0.elapsed());
            assert!(
                !left.is_zero(),
                "no matching media frame; states {states:?}"
            );
            match self.rx.recv_timeout(left) {
                Ok(ServerFrame::Media(m)) => {
                    let m = *m;
                    self.send(&ClientFrame::MediaAck {
                        pane: m.pane.clone(),
                        seq: m.seq,
                    });
                    if f(&m) {
                        return (m, states);
                    }
                    release(&m);
                }
                Ok(ServerFrame::BrowserState { state, .. }) => states.push(state),
                Ok(_) => {}
                Err(_) => panic!("no matching media frame; states {states:?}"),
            }
        }
    }
    fn state(&mut self, f: impl Fn(&BrowserStatus) -> bool, timeout: Duration) -> BrowserStatus {
        let t0 = Instant::now();
        let mut last = None;
        loop {
            let left = timeout.saturating_sub(t0.elapsed());
            assert!(!left.is_zero(), "no matching browser state; last {last:?}");
            match self.rx.recv_timeout(left) {
                Ok(ServerFrame::BrowserState { state, .. }) if f(&state) => return state,
                Ok(ServerFrame::BrowserState { state, .. }) => last = Some(state),
                Ok(ServerFrame::Media(m)) => {
                    self.send(&ClientFrame::MediaAck {
                        pane: m.pane.clone(),
                        seq: m.seq,
                    });
                    release(&m);
                }
                Ok(_) => {}
                Err(_) => panic!("no matching browser state; last {last:?}"),
            }
        }
    }
}

/// What the host terminal would do with shm tiles: read (here: only unlink).
fn release(m: &MediaFrame) {
    for t in &m.tiles {
        if let TileData::Shm { name, .. } = &t.data {
            vk_browser::kitty::shm::unlink(name);
        }
    }
}

/// RGBA of a tile (reading and unlinking shm objects, inflating zlib).
fn tile_px(t: &vk_proto::render::MediaTile) -> Vec<u8> {
    match &t.data {
        TileData::Shm { name, len } => {
            let v = vk_browser::kitty::shm::read(name, *len as usize).expect("shm tile");
            vk_browser::kitty::shm::unlink(name);
            v
        }
        TileData::ZlibRgba(z) => vk_browser::kitty::unzlib(z).unwrap(),
        TileData::Rgba(p) => p.clone(),
    }
}

fn media_pane(pane: &str, spec: BrowserPane, cols: u16, rows: u16) -> MediaPane {
    MediaPane {
        pane: pane.into(),
        owner: String::new(),
        spec,
        cols,
        rows,
        cell_w: 16,
        cell_h: 32,
        dpr: 2.0,
    }
}

/// Loopback HTTP server serving `pages` (path → html).
fn http(pages: Vec<(&'static str, String)>) -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            let pages = pages.clone();
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 8192];
                let mut got = 0;
                let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                while got < buf.len() {
                    match s.read(&mut buf[got..]) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => got += n,
                    }
                    if buf[..got].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let req = String::from_utf8_lossy(&buf[..got]).into_owned();
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                let body = pages
                    .iter()
                    .find(|(p, _)| *p == path)
                    .map(|(_, b)| b.clone())
                    .unwrap_or_else(|| "<html><body>not found</body></html>".into());
                let _ = write!(
                    s,
                    "HTTP/1.0 200 OK\r\nContent-Type: text/html\r\nCache-Control: no-store\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    port
}

fn root_pane(s: &Session) -> String {
    s.json(&["workspace", "create", "--cwd", "/tmp"])["root_pane"]["id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn browser_pane_end_to_end_with_fake_chromium() {
    let s = Session::new(true);
    let src = root_pane(&s);
    let port = http(vec![
        ("/", "<p>one</p>".into()),
        ("/two", "<p>two</p>".into()),
    ]);
    let url = format!("http://localhost:{port}/");
    // CLI: a browser pane split to the right of the source pane.
    let o = s.json(&["preview", "open", &url, "--split", "right", "--pane", &src]);
    assert_eq!(o["opened_in"], "pane", "{o}");
    let bp = o["pane"].as_str().unwrap().to_string();
    assert_eq!(o["source_pane"], json!(src));
    let spec = s.browser_spec(&bp);
    assert_eq!(spec.url, url);
    assert_eq!(spec.source_pane.as_deref(), Some(src.as_str()));
    let list = s.json(&["browser", "panes"]);
    assert_eq!(list["panes"].as_array().unwrap().len(), 1, "{list}");
    // The layout holds both panes; no holder was spawned for the browser pane.
    let st = s.json(&["server", "status"]);
    assert_eq!(st["panes"], 2, "{st}");

    // A render client shows it: 40×20 content cells of 16×32 px at DPR 2.
    let mut r = Render::attach(&s.socket(), "bp-e2e");
    r.view(vec![media_pane(&bp, spec.clone(), 40, 20)]);
    let (m, states) = r.media(|m| m.reset, Duration::from_secs(20));
    assert_eq!((m.width, m.height), (640, 640));
    assert_eq!((m.grid_cols, m.grid_rows), (10, 10));
    assert_eq!(m.tiles.len(), 100);
    assert!(
        matches!(m.tiles[0].data, TileData::Shm { .. }),
        "same-machine client gets shm tiles"
    );
    // Bottom-right tile: the fake page's solid colour (one per generation).
    let px = tile_px(m.tiles.last().unwrap());
    assert_eq!(px.len(), 64 * 64 * 4);
    let near = |c: [u8; 3]| (0..3).all(|i| (px[i] as i32 - c[i] as i32).abs() < 12);
    assert!(
        (0..6).any(|g| near(vk_browser::fake_chromium::color_for(g))),
        "{:?}",
        &px[..4]
    );
    release(&m);
    assert!(
        states.iter().any(|st| st.env.ends_with("chromium → local")),
        "{states:?}"
    );
    let log = s.wait_cdp("launch + target", |l| {
        count(l, "Target.createTarget") >= 1 && count(l, "Page.startScreencast") >= 1
    });
    let argv = log.iter().find(|v| v.get("argv").is_some()).unwrap();
    let argv: Vec<String> = serde_json::from_value(argv["argv"].clone()).unwrap();
    assert!(
        argv.contains(&"--force-device-scale-factor=2".to_string()),
        "{argv:?}"
    );
    assert!(
        argv.iter()
            .any(|a| a.starts_with("--user-data-dir=")
                && a.contains("/state/browser-profiles/local")),
        "a Vibeke profile under the test's state dir: {argv:?}"
    );
    assert!(argv.contains(&"--disable-smooth-scrolling".to_string()));
    assert!(
        log.iter()
            .any(|v| v["method"] == "Emulation.setDeviceMetricsOverride"
                && v["params"]["width"] == 320
                && v["params"]["deviceScaleFactor"] == 2.0)
    );

    // A key reaches the page (and changes its pixels).
    let seq = m.seq;
    r.cmd(&bp, BrowserCmd::Key(vk_proto::input::KeyEvent::ch('a')));
    let (m2, _) = r.media(|m| m.seq > seq && !m.reset, Duration::from_secs(10));
    release(&m2);
    let log = s.wait_cdp("key", |l| {
        l.iter().any(|v| {
            v["method"] == "Input.dispatchKeyEvent"
                && v["params"]["type"] == "keyDown"
                && v["params"]["key"] == "a"
        })
    });
    // No key releases from this client: the server adds the keyUp.
    assert!(
        log.iter()
            .any(|v| v["method"] == "Input.dispatchKeyEvent" && v["params"]["type"] == "keyUp")
    );

    // Navigation persists in the owner's pane record.
    r.cmd(&bp, BrowserCmd::Navigate(format!("localhost:{port}/two")));
    let two = format!("http://localhost:{port}/two");
    r.state(|st| st.url == two && st.can_back, Duration::from_secs(10));
    let t0 = Instant::now();
    while s.browser_spec(&bp).url != two {
        assert!(t0.elapsed() < Duration::from_secs(5), "url not persisted");
        std::thread::sleep(Duration::from_millis(100));
    }
    let spec2 = s.browser_spec(&bp);
    assert_eq!(spec2.history, vec![url.clone(), two.clone()]);
    assert_eq!(spec2.history_index, 1);
    // Back via the API (same path as the chrome's ← button).
    s.json(&["browser", "command", &bp, "back"]);
    r.state(
        |st| st.url == url && st.can_forward,
        Duration::from_secs(10),
    );

    // The pane's screenshot action records through the one capture path (06 B6/B8): a
    // `screenshot` record with `environment.kind = local_pane`, attributed to the source pane.
    s.json(&["browser", "command", &bp, "screenshot"]);
    let t0 = Instant::now();
    let shot = loop {
        let l = s.json(&["screenshot", "list"]);
        if let Some(x) = l["screenshots"].as_array().and_then(|a| a.first()).cloned() {
            break x;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "no screenshot: {l}");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(shot["environment"]["kind"], "local_pane", "{shot}");
    assert_eq!(shot["environment"]["fresh_context"], false, "{shot}");
    assert_eq!(shot["url"], json!(url), "{shot}");
    assert_eq!(shot["pane"], json!(src), "{shot}");
    assert!(shot["handle"].as_str().unwrap().starts_with('s'), "{shot}");
    assert!(
        shot["label"].as_str().unwrap().contains("browser pane"),
        "{shot}"
    );
    r.state(
        |st| {
            st.notice
                .as_deref()
                .is_some_and(|n| n.starts_with("screenshot s"))
        },
        Duration::from_secs(10),
    );

    // Open in window (06 B3.3): the headless browser on the profile closes, a window process
    // takes the profile, the pane shows "windowed"; back to the pane relaunches headless.
    let launches = |l: &[Value]| l.iter().filter(|v| v.get("argv").is_some()).count();
    let n0 = launches(&s.cdp_log());
    let w = s.json(&["browser", "command", &bp, "window"]);
    assert_eq!(w["opened_in"], "window", "{w}");
    assert_eq!(w["profile"], "local");
    assert_eq!(w["url"], json!(url));
    r.state(|st| st.windowed, Duration::from_secs(10));
    s.wait_cdp("headless closed", |l| count(l, "Browser.close") >= 1);
    let ps = s.json(&["preview", "profile", "list"]);
    assert!(
        ps["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "local" && p["running"] == true),
        "{ps}"
    );
    s.json(&["browser", "command", &bp, "pane"]);
    r.state(|st| !st.windowed, Duration::from_secs(10));
    s.wait_cdp("relaunched after window", |l| launches(l) > n0);
    // Frames flow again (changed tiles against what this client already has).
    let (m4, _) = r.media(|m| !m.tiles.is_empty(), Duration::from_secs(10));
    release(&m4);

    // Hidden → the screencast stops.
    r.view(vec![]);
    s.wait_cdp("stop screencast", |l| count(l, "Page.stopScreencast") >= 1);
    let st = s.json(&["browser", "pane-status"]);
    assert_eq!(st["targets"][0]["viewers"], 0, "{st}");
    assert_eq!(st["targets"][0]["screencast"], false, "{st}");
    drop(r);

    // Server restart: the pane survives in the layout and the page reloads at its last URL.
    s.json(&["server", "stop"]);
    // Wait for the old server to exit, then start over with a fresh CDP log.
    let t0 = Instant::now();
    while UnixStream::connect(s.socket()).is_ok() && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(300));
    let _ = std::fs::rename(s.path().join("cdp.log"), s.path().join("cdp-1.log"));
    let spec3 = s.browser_spec(&bp); // restarts the server
    assert_eq!(spec3.url, url, "last committed URL (after back)");
    let mut r = Render::attach(&s.socket(), "bp-e2e-2");
    r.view(vec![media_pane(&bp, spec3.clone(), 40, 20)]);
    let (m3, _) = r.media(|m| m.reset, Duration::from_secs(20));
    release(&m3);
    let log = s.wait_cdp("relaunch", |l| {
        l.iter().any(|v| v.get("argv").is_some()) && count(l, "Page.navigate") >= 1
    });
    let last_launch = log.iter().rposition(|v| v.get("argv").is_some()).unwrap();
    assert!(
        log[last_launch..]
            .iter()
            .any(|v| v["method"] == "Page.navigate" && v["params"]["url"] == url.as_str()),
        "reloaded at the last URL"
    );

    // Closing the pane removes it from the layout and closes its target.
    s.json(&["pane", "close", &bp]);
    s.wait_cdp("close target", |l| count(l, "Target.closeTarget") >= 1);
    let list = s.json(&["browser", "panes"]);
    assert!(list["panes"].as_array().unwrap().is_empty(), "{list}");
}

/// Real Chromium (Playwright headless shell): tiles arrive, a key reaches the page, and the
/// whole pipeline's latency and scroll fps are printed (`--nocapture`).
#[test]
fn browser_pane_real_chromium() {
    if std::env::var("VIBEKE_BROWSER_TESTS").as_deref() != Ok("1") {
        eprintln!("VIBEKE_BROWSER_TESTS != 1; skipping");
        return;
    }
    if vk_browser::cdp::discover_chromium(true).is_none() {
        eprintln!("no Playwright Chromium on disk; skipping");
        return;
    }
    let s = Session::new(false);
    let src = root_pane(&s);
    let page = r#"<html><head><style>
        html,body{margin:0;background:#fff;font:16px sans-serif}
        #rows div{height:40px;border-bottom:1px solid #ccc;padding-left:8px}
      </style></head><body>
      <input id=i autofocus style="width:300px;height:30px">
      <div id=ind style="position:fixed;right:0;bottom:0;width:64px;height:64px;background:#fff"></div>
      <div id=rows></div>
      <script>
        const rows = document.getElementById('rows');
        for (let k = 0; k < 600; k++) { const d = document.createElement('div');
          d.textContent = 'row ' + k; d.style.background = k % 2 ? '#eef' : '#fff'; rows.appendChild(d); }
        let n = 0;
        document.addEventListener('keydown', e => {
          n++; document.getElementById('ind').style.background = n % 2 ? 'rgb(200,0,0)' : 'rgb(0,0,200)';
          document.title = 'keys ' + n + ' ' + e.key;
        });
      </script></body></html>"#;
    let port = http(vec![("/", page.to_string())]);
    let url = format!("http://localhost:{port}/");
    let o = s.json(&["preview", "open", &url, "--split", "right", "--pane", &src]);
    let bp = o["pane"].as_str().unwrap().to_string();
    let spec = s.browser_spec(&bp);
    let mut r = Render::attach(&s.socket(), "bp-real");
    let t_open = Instant::now();
    r.view(vec![media_pane(&bp, spec, 100, 40)]);
    let (m, early) = r.media(|m| m.reset, Duration::from_secs(30));
    let first_frame = t_open.elapsed();
    assert_eq!((m.width, m.height), (1600, 1280));
    release(&m);
    // Wait for the page to load (title from the page is empty; url committed). The state may
    // already have arrived before the first frame.
    if !early.iter().any(|st| st.url == url && !st.loading) {
        r.state(|st| st.url == url && !st.loading, Duration::from_secs(20));
    }
    std::thread::sleep(Duration::from_millis(500));
    // Drain.
    while let Ok(f) = r.rx.recv_timeout(Duration::from_millis(300)) {
        if let ServerFrame::Media(m) = f {
            r.send(&ClientFrame::MediaAck {
                pane: m.pane.clone(),
                seq: m.seq,
            });
            release(&m);
        }
    }
    // Key → a frame whose bottom-right tile shows the page's new background.
    let mut lat = vec![];
    for k in 0..20 {
        let want_red = k % 2 == 0;
        let t = Instant::now();
        r.cmd(&bp, BrowserCmd::Key(vk_proto::input::KeyEvent::ch('k')));
        let (m, _) = r.media(
            |m| {
                m.tiles.iter().any(|t| {
                    if t.index != (m.grid_cols as u32 * m.grid_rows as u32 - 1) {
                        return false;
                    }
                    let px = tile_px(t);
                    let (rr, b) = (px[px.len() - 4], px[px.len() - 2]);
                    if want_red {
                        rr > 150 && b < 80
                    } else {
                        b > 150 && rr < 80
                    }
                })
            },
            Duration::from_secs(10),
        );
        lat.push(t.elapsed().as_secs_f64() * 1000.0);
        release(&m);
        std::thread::sleep(Duration::from_millis(60));
    }
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| lat[((lat.len() as f64 - 1.0) * q).round() as usize];
    // Scrolling: 60 Hz wheel for 3 s, count media frames.
    let t0 = Instant::now();
    let mut frames = 0u32;
    let mut tiles = 0usize;
    let mut next = Instant::now();
    while t0.elapsed() < Duration::from_secs(3) {
        if Instant::now() >= next {
            r.cmd(
                &bp,
                BrowserCmd::Wheel {
                    x: 400.0,
                    y: 300.0,
                    dx: 0.0,
                    dy: 40.0,
                    mods: vk_proto::input::Mods::empty(),
                },
            );
            next += Duration::from_millis(16);
        }
        if let Ok(ServerFrame::Media(m)) = r.rx.recv_timeout(Duration::from_millis(2)) {
            frames += 1;
            tiles += m.tiles.len();
            r.send(&ClientFrame::MediaAck {
                pane: m.pane.clone(),
                seq: m.seq,
            });
            release(&m);
        }
    }
    let fps = frames as f64 / t0.elapsed().as_secs_f64();
    let st = s.json(&["browser", "pane-status"]);
    eprintln!(
        "REAL CHROMIUM PIPELINE: first frame {:.0} ms; key→tile p50 {:.0} ms p95 {:.0} ms max {:.0} ms; scroll {:.1} fps ({} frames, {:.0} tiles/frame); decode {} ms; status {}",
        first_frame.as_secs_f64() * 1000.0,
        p(0.5),
        p(0.95),
        lat.last().unwrap(),
        fps,
        frames,
        tiles as f64 / frames.max(1) as f64,
        st["targets"][0]["decode_ms"],
        st["targets"][0]
    );
    assert!(frames > 10, "frames while scrolling: {frames}");
    let profile_dir = s.path().join("state/browser-profiles/local");
    assert!(
        profile_dir.is_dir(),
        "Vibeke profile under the test state dir"
    );
}

/// Playwright's headless shell on disk (never downloaded here).
fn playwright_shell() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let mut best: Option<(u32, String)> = None;
    for root in [
        format!("{home}/Library/Caches/ms-playwright"),
        format!("{home}/.cache/ms-playwright"),
    ] {
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(rev) = name
                .strip_prefix("chromium_headless_shell-")
                .and_then(|r| r.parse::<u32>().ok())
            else {
                continue;
            };
            for sub in [
                "chrome-headless-shell-mac-arm64/chrome-headless-shell",
                "chrome-headless-shell-mac-x64/chrome-headless-shell",
                "chrome-headless-shell-linux64/chrome-headless-shell",
            ] {
                let p = e.path().join(sub);
                if p.is_file() && best.as_ref().is_none_or(|b| b.0 < rev) {
                    best = Some((rev, p.display().to_string()));
                }
            }
        }
    }
    best.map(|b| b.1)
}

fn release_tile(t: &vk_proto::render::MediaTile) {
    if let TileData::Shm { name, .. } = &t.data {
        vk_browser::kitty::shm::unlink(name);
    }
}

/// Watch / take over an agent's browser session (06 B7) with Playwright's headless shell:
/// `vibeke browser watch b1` opens a watch pane next to a pane; the agent session's
/// screencast arrives as tiles on the media channel; keys are ignored until the pane takes
/// the session over; then they reach the page; closing the pane releases it.
#[test]
fn watch_agent_session_real_chromium() {
    if std::env::var("VIBEKE_BROWSER_TESTS").as_deref() != Ok("1") {
        eprintln!("VIBEKE_BROWSER_TESTS != 1; skipping");
        return;
    }
    let Some(shell) = playwright_shell() else {
        eprintln!("no Playwright chrome-headless-shell on disk; skipping");
        return;
    };
    let s = Session::new(false);
    std::fs::write(
        s.path().join("config.toml"),
        format!("[preview]\nbrowser_path = \"{shell}\"\nbrowser_idle = \"60s\"\n"),
    )
    .unwrap();
    let page = r#"<html><head><title>keys 0</title><style>
        html,body{margin:0;height:100%;background:rgb(0,160,0)}</style></head><body>
      <script>
        let n = 0;
        document.addEventListener('keydown', e => {
          n++; document.title = 'keys ' + n + ' ' + e.key;
          document.body.style.background = n % 2 ? 'rgb(200,0,0)' : 'rgb(0,0,200)';
        });
      </script></body></html>"#;
    let port = http(vec![("/", page.to_string())]);
    let src = root_pane(&s);
    s.json(&["preview", "declare", &port.to_string(), "--label", "app"]);
    let open = s.json(&["browser", "open", "v1"]);
    assert_eq!(open["session"], "b1", "{open}");
    let title = |s: &Session| -> String {
        s.json(&["browser", "eval", "b1", "document.title"])["value"]
            .as_str()
            .unwrap_or("")
            .to_string()
    };
    assert_eq!(title(&s), "keys 0");
    let w = s.json(&["browser", "watch", "b1", "--pane", &src]);
    assert_eq!(w["opened_in"], "watch", "{w}");
    let wp = w["pane"].as_str().unwrap().to_string();
    let spec = s.browser_spec(&wp);
    assert_eq!(spec.watch.as_deref(), Some("b1"));
    let mut r = Render::attach(&s.socket(), "watcher");
    r.view(vec![media_pane(&wp, spec, 60, 30)]);
    // The agent's page (green) arrives scaled into 60×30 cells of 16×32 px.
    let (m, states) = r.media(|m| m.reset && !m.tiles.is_empty(), Duration::from_secs(30));
    assert!(
        m.width <= 960 && m.height <= 960,
        "{}x{}",
        m.width,
        m.height
    );
    let px = tile_px(&m.tiles[0]);
    assert!(px[1] > 120 && px[0] < 60, "green page: {:?}", &px[..4]);
    for t in &m.tiles[1..] {
        release_tile(t);
    }
    if let Some(st) = states.last() {
        assert_eq!(st.watch.as_deref(), Some("b1"));
        assert!(!st.human_control);
    }
    // Read-only: a key does nothing to the page.
    r.cmd(&wp, BrowserCmd::Key(vk_proto::input::KeyEvent::ch('x')));
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(title(&s), "keys 0");
    // Take over: keys reach the page through the agent session.
    r.cmd(&wp, BrowserCmd::TakeOver(true));
    let st = r.state(|st| st.controlled_here, Duration::from_secs(10));
    assert!(st.human_control);
    r.cmd(&wp, BrowserCmd::Key(vk_proto::input::KeyEvent::ch('k')));
    let t0 = Instant::now();
    while title(&s) != "keys 1 k" {
        assert!(t0.elapsed() < Duration::from_secs(10), "key not delivered");
        std::thread::sleep(Duration::from_millis(100));
    }
    // The page turned red: a changed frame arrives.
    let (m, _) = r.media(
        |m| {
            m.tiles.first().is_some_and(|t| {
                let px = tile_px(t);
                px[0] > 150 && px[1] < 80
            })
        },
        Duration::from_secs(10),
    );
    for t in &m.tiles[1..] {
        release_tile(t);
    }
    let list = s.json(&["browser", "list"]);
    assert_eq!(list["sessions"][0]["human_control"], true, "{list}");
    // Release.
    r.cmd(&wp, BrowserCmd::TakeOver(false));
    r.state(|st| !st.human_control, Duration::from_secs(10));
    // Take over again and close the pane: control goes back to the agent.
    r.cmd(&wp, BrowserCmd::TakeOver(true));
    r.state(|st| st.controlled_here, Duration::from_secs(10));
    s.json(&["pane", "close", &wp]);
    let t0 = Instant::now();
    loop {
        let list = s.json(&["browser", "list"]);
        if list["sessions"][0]["human_control"] == false {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "not released: {list}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Next clipboard frame for `pane` (acking media on the way).
fn clipboard_frame(r: &mut Render, pane: &str, timeout: Duration) -> Vec<u8> {
    let t0 = Instant::now();
    loop {
        let left = timeout.saturating_sub(t0.elapsed());
        assert!(!left.is_zero(), "no clipboard frame");
        match r.rx.recv_timeout(left) {
            Ok(ServerFrame::Clipboard { data, pane: p, .. }) if p == pane => return data,
            Ok(ServerFrame::Media(m)) => {
                r.send(&ClientFrame::MediaAck {
                    pane: m.pane.clone(),
                    seq: m.seq,
                });
                release(&m);
            }
            Ok(_) => {}
            Err(_) => panic!("no clipboard frame"),
        }
    }
}

/// 06 B3.2 "not built yet" items, end to end through the CLI, the server and the out-of-process
/// fake Chromium: `--device` letterboxing, console/network capture (`vibeke browser console
/// --pane`) and the console split running its follower in a pane, page clipboard → the render
/// client, dropped files (drag events, or an open file chooser), unpinning.
#[test]
fn browser_pane_console_clipboard_drops_and_device_with_fake_chromium() {
    let s = Session::new(true);
    let src = root_pane(&s);
    let port = http(vec![("/", "<p>one</p>".into())]);
    let url = format!("http://localhost:{port}/");
    let o = s.json(&[
        "preview",
        "open",
        &url,
        "--split",
        "right",
        "--pane",
        &src,
        "--device",
        "iphone-15",
    ]);
    let bp = o["pane"].as_str().unwrap().to_string();
    let spec = s.browser_spec(&bp);
    assert_eq!(spec.device.as_deref(), Some("iphone-15"));

    // Letterboxed: the frame fills the 40×20-cell content area; the phone sits in the middle.
    let mut r = Render::attach(&s.socket(), "bp-io");
    r.view(vec![media_pane(&bp, spec.clone(), 40, 20)]);
    let (m, _) = r.media(|m| m.reset, Duration::from_secs(20));
    assert_eq!((m.width, m.height), (640, 640));
    let corner = tile_px(&m.tiles[0]);
    assert_eq!(
        &corner[..4],
        &vk_browser::devices::FILL,
        "neutral fill in the margin"
    );
    release(&m);
    let log = s.wait_cdp("device emulation", |l| {
        l.iter().any(|v| {
            v["method"] == "Emulation.setDeviceMetricsOverride"
                && v["params"]["width"] == 393
                && v["params"]["mobile"] == true
        }) && l
            .iter()
            .any(|v| v["method"] == "Page.startScreencast" && v["params"]["maxHeight"] == 640)
    });
    assert!(log.iter().any(|v| {
        v["method"] == "Emulation.setUserAgentOverride"
            && v["params"]["userAgent"]
                .as_str()
                .unwrap_or("")
                .contains("iPhone")
    }));

    // Console and network capture through the CLI (redacted, filterable).
    r.cmd(
        &bp,
        BrowserCmd::Text("error:boom token=hunter2hunter2".into()),
    );
    r.cmd(&bp, BrowserCmd::Text("fail:http://localhost:9/api".into()));
    let t0 = Instant::now();
    let entries = loop {
        let v = s.json(&["browser", "console", "--pane", &bp]);
        let e = v["entries"].as_array().cloned().unwrap_or_default();
        let has = |f: &dyn Fn(&Value) -> bool| e.iter().any(f);
        if has(&|x| x["text"].as_str().is_some_and(|t| t.contains("boom")))
            && has(&|x| x["error"].is_string())
            && has(&|x| x["text"].as_str().is_some_and(|t| t.starts_with("loaded")))
        {
            break e;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "capture: {v}");
        std::thread::sleep(Duration::from_millis(100));
    };
    let boom = entries
        .iter()
        .find(|x| x["text"].as_str().is_some_and(|t| t.contains("boom")))
        .unwrap();
    assert!(!boom.to_string().contains("hunter2hunter2"), "{boom}");
    let errs = s.json(&["browser", "console", "--pane", &bp, "--errors"]);
    let errs = errs["entries"].as_array().unwrap();
    assert_eq!(errs.len(), 2, "{errs:?}");
    let net = s.json(&["browser", "console", "--pane", &bp, "--network"]);
    assert!(
        net["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["kind"] == "network")
    );

    // The console split: a pane under the browser pane running the follower.
    let c = s.json(&["browser", "console-split", &bp]);
    let cp = c["pane"].as_str().expect("split pane").to_string();
    let p = s.json(&["pane", "get", &cp]);
    assert_eq!(
        p["pane"]["created_by"],
        json!(format!("browser-console:{bp}")),
        "{p}"
    );
    let screen = |s: &Session| {
        s.json(&["pane", "read", &cp, "--source", "recent", "--lines", "40"])["text"]
            .as_str()
            .unwrap_or("")
            .to_string()
    };
    let t0 = Instant::now();
    while !screen(&s).contains("boom") {
        assert!(
            t0.elapsed() < Duration::from_secs(15),
            "follower output: {}",
            screen(&s)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    r.cmd(&bp, BrowserCmd::Text("log:after the split".into()));
    let t0 = Instant::now();
    while !screen(&s).contains("after the split") {
        assert!(
            t0.elapsed() < Duration::from_secs(15),
            "follower output: {}",
            screen(&s)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // Toggle: the second call closes it.
    let c = s.json(&["browser", "console-split", &bp]);
    assert_eq!(c["closed"], json!(cp));

    // Page clipboard → the viewer (after user input to the page, which the Text above was).
    r.cmd(&bp, BrowserCmd::Text("copy:hello from the page".into()));
    assert_eq!(
        clipboard_frame(&mut r, &bp, Duration::from_secs(10)),
        b"hello from the page"
    );

    // Files: dropped at the pointer, or into a file chooser the page opened.
    let file = s.path().join("upload me.txt");
    std::fs::write(&file, b"hello").unwrap();
    let canon = file.canonicalize().unwrap().display().to_string();
    r.cmd(&bp, BrowserCmd::DropFiles(vec![file.display().to_string()]));
    s.wait_cdp("drop", |l| {
        l.iter().any(|v| {
            v["method"] == "Input.dispatchDragEvent"
                && v["params"]["type"] == "drop"
                && v["params"]["data"]["files"] == json!([canon])
        })
    });
    r.cmd(&bp, BrowserCmd::Text("chooser".into()));
    r.state(
        |st| {
            st.notice
                .as_deref()
                .is_some_and(|n| n.contains("file chooser"))
        },
        Duration::from_secs(10),
    );
    r.cmd(&bp, BrowserCmd::DropFiles(vec![canon.clone()]));
    s.wait_cdp("file chooser", |l| {
        l.iter().any(|v| {
            v["method"] == "DOM.setFileInputFiles"
                && v["params"]["backendNodeId"] == 42
                && v["params"]["files"] == json!([canon])
        })
    });

    // Unpin from the CLI: the pane's own size again.
    s.json(&["browser", "viewport", &bp, "fit"]);
    let spec = s.browser_spec(&bp);
    assert_eq!((spec.device.clone(), spec.viewport.clone()), (None, None));
    r.view(vec![media_pane(&bp, spec, 40, 20)]);
    s.wait_cdp("unpinned", |l| {
        l.iter().any(|v| {
            v["method"] == "Emulation.setDeviceMetricsOverride"
                && v["params"]["width"] == 320
                && v["params"]["mobile"] == false
        })
    });
}

/// Real Chromium: the page clipboard hook, console capture, a drop and a pinned device.
#[test]
fn browser_pane_io_real_chromium() {
    if std::env::var("VIBEKE_BROWSER_TESTS").as_deref() != Ok("1") {
        eprintln!("VIBEKE_BROWSER_TESTS != 1; skipping");
        return;
    }
    if vk_browser::cdp::discover_chromium(true).is_none() {
        eprintln!("no Playwright Chromium on disk; skipping");
        return;
    }
    let s = Session::new(false);
    let src = root_pane(&s);
    let page = r#"<html><head><meta name="viewport" content="width=device-width,initial-scale=1"></head>
      <body style="margin:0">
      <input id=i autofocus style="width:300px;height:30px" value="copy me">
      <div id=drop style="position:fixed;left:0;top:60px;width:100%;height:300px;background:#eee">drop here</div>
      <script>
        console.log('page ready', innerWidth + 'x' + innerHeight, devicePixelRatio, navigator.userAgent.includes('iPhone'));
        const d = document.getElementById('drop');
        d.addEventListener('dragover', e => e.preventDefault());
        d.addEventListener('drop', e => { e.preventDefault();
          console.log('dropped ' + [...e.dataTransfer.files].map(f => f.name + ':' + f.size).join(',')); });
        document.addEventListener('keydown', e => {
          if (e.key === 'k') navigator.clipboard.writeText('from writeText');
          if (e.key === 'x') { fetch('http://localhost:9/nothing').catch(() => {}); throw new Error('boom'); }
        });
      </script></body></html>"#;
    let port = http(vec![("/", page.to_string())]);
    let url = format!("http://localhost:{port}/");
    let o = s.json(&[
        "preview",
        "open",
        &url,
        "--split",
        "right",
        "--pane",
        &src,
        "--device",
        "iphone-15",
    ]);
    let bp = o["pane"].as_str().unwrap().to_string();
    let spec = s.browser_spec(&bp);
    let mut r = Render::attach(&s.socket(), "bp-io-real");
    r.view(vec![media_pane(&bp, spec, 100, 40)]);
    let (m, _) = r.media(|m| m.reset, Duration::from_secs(30));
    assert_eq!((m.width, m.height), (1600, 1280));
    release(&m);
    let wait_entry = |s: &Session, what: &str, f: &dyn Fn(&Value) -> bool| -> Value {
        let t0 = Instant::now();
        loop {
            let v = s.json(&["browser", "console", "--pane", &bp]);
            if let Some(e) = v["entries"]
                .as_array()
                .and_then(|a| a.iter().find(|e| f(e)).cloned())
            {
                return e;
            }
            assert!(t0.elapsed() < Duration::from_secs(20), "{what}: {v}");
            std::thread::sleep(Duration::from_millis(200));
        }
    };
    let ready = wait_entry(&s, "page ready", &|e| {
        e["text"]
            .as_str()
            .is_some_and(|t| t.starts_with("page ready"))
    });
    eprintln!("REAL CHROMIUM DEVICE: {}", ready["text"]);
    assert!(
        ready["text"].as_str().unwrap().contains("393x852 3 true"),
        "{ready}"
    );
    // Clipboard: a key press makes the page call writeText.
    r.cmd(&bp, BrowserCmd::Key(vk_proto::input::KeyEvent::ch('k')));
    assert_eq!(
        clipboard_frame(&mut r, &bp, Duration::from_secs(10)),
        b"from writeText"
    );
    // A copy of the input's selection (Cmd+A, Cmd+C: Chromium editing commands) reaches the
    // viewer through the copy-event hook.
    if cfg!(target_os = "macos") {
        let cmd = |c| {
            vk_proto::input::KeyEvent::new(
                vk_proto::input::Key::Char(c),
                vk_proto::input::Mods::SUPER,
            )
        };
        r.cmd(&bp, BrowserCmd::Key(cmd('a')));
        r.cmd(&bp, BrowserCmd::Key(cmd('c')));
        assert_eq!(
            clipboard_frame(&mut r, &bp, Duration::from_secs(10)),
            b"kcopy me" // the `k` above was typed into the focused input
        );
    }
    // Errors and failed requests.
    r.cmd(&bp, BrowserCmd::Key(vk_proto::input::KeyEvent::ch('x')));
    wait_entry(&s, "exception", &|e| {
        e["level"] == "error" && e["text"].as_str().is_some_and(|t| t.contains("boom"))
    });
    wait_entry(&s, "failed request", &|e| {
        e["kind"] == "network" && e["error"].is_string()
    });
    // A drop on the page's drop zone (pointer there first; pane coords are letterboxed).
    let file = s.path().join("dropped.txt");
    std::fs::write(&file, b"12345").unwrap();
    r.cmd(
        &bp,
        BrowserCmd::Mouse {
            kind: vk_proto::input::MouseKind::Move,
            button: vk_proto::input::MouseButton::None,
            x: 400.0,
            y: 200.0,
            mods: vk_proto::input::Mods::empty(),
            clicks: 0,
        },
    );
    r.cmd(&bp, BrowserCmd::DropFiles(vec![file.display().to_string()]));
    wait_entry(&s, "drop", &|e| e["text"] == "dropped dropped.txt:5");
}
