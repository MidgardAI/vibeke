//! Herdr plugin surfaces end to end against an isolated session (M5, TUI side): a trusted
//! fixture plugin opens a popup and an overlay, a render-stream client (standing in for the
//! TUI) sees them in its model and lays them out with the TUI's own geometry, `popup.close`
//! restores the focus, a link handler gets `HERDR_PLUGIN_CLICKED_URL`, a pushed window title
//! reaches the client, and a `ScrollView` report becomes `pane.scroll_changed`.
//!
//! Temp VIBEKE_* dirs only; nothing touches a real Herdr, its config or sockets.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use vk_proto::layout::Rect;
use vk_proto::model::{SessionModel, SurfaceKind};
use vk_proto::render::{ClientFrame, ServerFrame};

struct Session {
    dir: tempfile::TempDir,
}

impl Session {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("vkm5s")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::write(dir.path().join("config.toml"), "").unwrap();
        Session { dir }
    }
    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().canonicalize().unwrap().join(p)
    }
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_vibeke"));
        let d = self.dir.path();
        c.env("VIBEKE_RUNTIME_DIR", d.join("run"))
            .env("VIBEKE_STATE_DIR", d.join("state"))
            .env("VIBEKE_CONFIG", d.join("config.toml"))
            .env("XDG_DATA_HOME", d.join("data"))
            .env(
                "VIBEKE_NOTIFIER",
                format!("log:{}", d.join("notes.jsonl").display()),
            );
        for k in [
            "VIBEKE",
            "VIBEKE_SOCKET",
            "VIBEKE_SESSION",
            "VIBEKE_PANE_TOKEN",
            "VIBEKE_HERDR_BROKER",
            "HERDR_ENV",
            "HERDR_SOCKET_PATH",
            "HERDR_PANE_ID",
            "HERDR_BIN_PATH",
            "HERDR_SESSION",
            "HERDR_PLUGIN_ID",
        ] {
            c.env_remove(k);
        }
        c.arg("--json").args(args);
        c
    }
    fn json(&self, args: &[&str]) -> Value {
        let out = self.cmd(args).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}{}",
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }
    fn herdr(&self, args: &[&str]) -> Value {
        let mut a = vec!["compat", "herdr"];
        a.extend(args);
        self.json(&a)
    }
    fn herdr_fail(&self, args: &[&str]) -> Value {
        let mut a = vec!["compat", "herdr"];
        a.extend(args);
        let out = self.cmd(&a).output().unwrap();
        assert!(!out.status.success(), "{args:?} should fail");
        serde_json::from_slice(&out.stderr).unwrap_or(Value::Null)
    }
    fn api(&self, method: &str, params: Value) -> Value {
        self.json(&["api", "call", method, &params.to_string()])
    }
    fn socket(&self) -> PathBuf {
        self.path("run/default/vibeke.sock")
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop", "--kill-panes"]).output();
    }
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

fn wait_for(what: &str, timeout_ms: u64, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

/// A minimal render-stream client standing in for the TUI.
struct Render {
    w: UnixStream,
    rx: mpsc::Receiver<ServerFrame>,
    model: SessionModel,
    focus: Option<String>,
    features: Vec<String>,
}

impl Render {
    fn attach(sock: &Path) -> Render {
        let st = UnixStream::connect(sock).expect("connect render socket");
        let mut w = st.try_clone().unwrap();
        let req = json!({"jsonrpc":"2.0","id":1,"method":"render.attach","params":{"client_id": "surf-tui", "protocol": vk_proto::render::PROTOCOL, "caps": {"max_fps": 60}}});
        w.write_all(format!("{req}\n").as_bytes()).unwrap();
        let mut rd = BufReader::new(st);
        let mut line = String::new();
        rd.read_line(&mut line).unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert!(v.get("error").is_none(), "render.attach: {line}");
        let features = v["result"]["features"]
            .as_array()
            .map(|a| a.iter().map(s).collect())
            .unwrap_or_default();
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
            model: SessionModel::default(),
            focus: None,
            features,
        }
    }
    fn send(&mut self, f: &ClientFrame) {
        vk_proto::frame::write_frame(&mut self.w, f).unwrap();
        self.w.flush().unwrap();
    }
    /// Apply frames until `f(model, focus)` holds; returns pushed events seen on the way.
    fn until(&mut self, what: &str, f: impl Fn(&SessionModel, Option<&str>) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut events = vec![];
        loop {
            if f(&self.model, self.focus.as_deref()) {
                return events;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "timed out waiting for {what}");
            match self.rx.recv_timeout(left) {
                Ok(ServerFrame::Model { model, focus, .. }) => {
                    self.model = *model;
                    self.focus = focus.pane;
                }
                Ok(ServerFrame::Events { events: evs, .. }) => {
                    events.extend(evs.iter().map(|e| serde_json::from_str(&e.json).unwrap()))
                }
                Ok(_) => {}
                Err(_) => panic!("timed out waiting for {what}"),
            }
        }
    }
}

fn write_plugin(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("herdr-plugin.toml"),
        r#"id = "acme.surf"
name = "Surfaces"
version = "0.1.0"

[[actions]]
id = "link"
title = "Review a commit"
command = ["sh", "-c", "env | grep '^HERDR_' | sort > \"$HERDR_PLUGIN_STATE_DIR/link-env.txt\""]

[[link_handlers]]
id = "gh-commit"
title = "Review this commit"
pattern = '^https://github\.com/[^/]+/[^/]+/commit/'
action = "link"

[[panes]]
id = "pop"
title = "Picker"
placement = "popup"
width = "50%"
height = 12
command = ["sh", "-c", "S=\"$HERDR_PLUGIN_STATE_DIR\"; env | grep '^HERDR_' | sort > \"$S/pop-env.txt\"; herdr pane current > \"$S/pop-current.json\" 2>&1; sleep 60"]

[[panes]]
id = "ov"
title = "Board"
placement = "overlay"
command = ["sh", "-c", "env | grep '^HERDR_' | sort > \"$HERDR_PLUGIN_STATE_DIR/ov-env.txt\"; sleep 2"]
"#,
    )
    .unwrap();
}

/// The TUI's pane area for this test (any rect works: geometry is relative to it).
const AREA: Rect = Rect {
    x: 30,
    y: 1,
    w: 100,
    h: 40,
};

#[test]
fn popup_and_overlay_from_a_trusted_plugin() {
    let s_ = Session::new();
    let src = s_.path("src/surf");
    write_plugin(&src);
    let st = s_.path("state/plugins/state/acme.surf");
    s_.json(&["plugin", "link", src.to_str().unwrap()]);
    s_.json(&["plugin", "trust", "acme.surf", "--legacy"]);
    let created = s_.herdr(&["workspace", "create", "--cwd", "/tmp"]);
    let root = s(&created["root_pane"]["pane_id"]);

    let mut r = Render::attach(&s_.socket());
    assert!(
        r.features.contains(&"scroll_report".to_string()),
        "{:?}",
        r.features
    );
    r.until("the workspace", |m, _| {
        m.panes.iter().any(|p| p.handle == root)
    });
    let root_id = r
        .model
        .panes
        .iter()
        .find(|p| p.handle == root)
        .unwrap()
        .id
        .clone();
    r.send(&ClientFrame::Focus {
        pane: root_id.clone(),
    });
    r.send(&ClientFrame::Subscribe {
        types: vec!["client.window_title_changed".into()],
        after: None,
    });
    r.until("focus on the root pane", |_, f| f == Some(root_id.as_str()));

    // ---- popup ----------------------------------------------------------------------------
    let opened = s_.herdr(&[
        "plugin",
        "pane",
        "open",
        "--plugin",
        "acme.surf",
        "--entrypoint",
        "pop",
    ]);
    assert_eq!(opened["type"], "plugin_pane_opened", "{opened}");
    assert_eq!(opened["placement"], "popup");
    assert!(opened["pane"].is_null(), "a popup has no pane id: {opened}");
    r.until("the popup in the model with the focus", |m, f| {
        m.panes
            .iter()
            .any(|p| p.plugin_surface().is_some() && Some(p.id.as_str()) == f)
    });
    let pop = r
        .model
        .panes
        .iter()
        .find(|p| p.plugin_surface().is_some_and(|x| x.is_popup()))
        .unwrap()
        .clone();
    let info = pop.plugin_surface().unwrap();
    assert_eq!(
        (info.plugin.as_str(), info.entrypoint.as_str()),
        ("acme.surf", "pop")
    );
    assert_eq!((info.width.as_str(), info.height.as_str()), ("50%", "12"));
    // The TUI lays it out as a centred modal window of the requested size.
    let tab = r.model.tabs.iter().find(|t| t.id == pop.tab).unwrap();
    let surf = vk_tui::plugins::surfaces_in(&r.model.panes, tab, AREA);
    assert_eq!(surf.len(), 1);
    assert_eq!(surf[0].info.kind, SurfaceKind::Popup);
    assert_eq!((surf[0].outer.w, surf[0].outer.h), (50, 12));
    assert_eq!(surf[0].outer.x, AREA.x + 25);
    // No pane identity in the compat API or the process; callbacks act on the pane underneath.
    let list = s_.herdr(&["pane", "list"]);
    let ids: Vec<String> = list["panes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| s(&p["pane_id"]))
        .collect();
    assert_eq!(ids, std::slice::from_ref(&root), "{list}");
    wait_for("popup callback", 15000, || {
        read(&st.join("pop-current.json")).contains("pane_info")
    });
    let env = read(&st.join("pop-env.txt"));
    assert!(!env.contains("HERDR_PANE_ID="), "{env}");
    assert!(env.contains("HERDR_PLUGIN_ENTRYPOINT_ID=pop"), "{env}");
    let cur: Value = serde_json::from_str(&read(&st.join("pop-current.json"))).unwrap();
    assert_eq!(cur["pane"]["pane_id"], root.as_str(), "{cur}");
    assert_eq!(
        s_.herdr(&["pane", "current"])["pane"]["pane_id"],
        root.as_str(),
        "focus context underneath the popup"
    );
    // Session-modal: one at a time.
    let e = s_.herdr_fail(&[
        "plugin",
        "pane",
        "open",
        "--plugin",
        "acme.surf",
        "--entrypoint",
        "pop",
    ]);
    assert_eq!(e["error"]["code"], "popup_busy", "{e}");
    // popup.close removes it and gives the focus back.
    s_.herdr(&["popup", "close"]);
    r.until("popup gone, focus restored", |m, f| {
        !m.panes.iter().any(|p| p.id == pop.id) && f == Some(root_id.as_str())
    });
    let e = s_.herdr_fail(&["popup", "close"]);
    assert_eq!(e["error"]["code"], "popup_not_found", "{e}");

    // ---- overlay --------------------------------------------------------------------------
    let opened = s_.herdr(&[
        "plugin",
        "pane",
        "open",
        "--plugin",
        "acme.surf",
        "--entrypoint",
        "ov",
    ]);
    let ov_handle = s(&opened["pane"]["pane_id"]);
    assert!(ov_handle.starts_with('w'), "an overlay is a pane: {opened}");
    r.until("the overlay with the focus", |m, f| {
        m.panes.iter().any(|p| {
            p.handle == ov_handle
                && p.plugin_surface().is_some_and(|x| !x.is_popup())
                && Some(p.id.as_str()) == f
        })
    });
    let ov = r
        .model
        .panes
        .iter()
        .find(|p| p.handle == ov_handle)
        .unwrap()
        .clone();
    let tab = r.model.tabs.iter().find(|t| t.id == ov.tab).unwrap();
    assert!(
        tab.layout.panes().iter().all(|p| *p != ov.id),
        "not in the tiling"
    );
    let surf = vk_tui::plugins::surfaces_in(&r.model.panes, tab, AREA);
    assert_eq!(surf[0].outer, AREA, "full-area layer");
    assert_eq!(surf[0].inner.h, AREA.h - 1);
    // The shell creates the file before `sort` writes it: wait for the content, not the file.
    wait_for("overlay env", 15000, || {
        read(&st.join("ov-env.txt")).contains("HERDR_PLUGIN_ID=")
    });
    let env = read(&st.join("ov-env.txt"));
    assert!(env.contains(&format!("HERDR_PANE_ID={ov_handle}")), "{env}");
    // Its command exits: it closes by itself and the focus comes back.
    r.until("overlay closed after its command exited", |m, f| {
        !m.panes.iter().any(|p| p.id == ov.id) && f == Some(root_id.as_str())
    });

    // ---- link handler ---------------------------------------------------------------------
    let h = s_.api("plugin.link_handler.list", json!({}));
    assert_eq!(h["handlers"][0]["handler_id"], "gh-commit", "{h}");
    assert_eq!(h["handlers"][0]["available"], true);
    let url = "https://github.com/acme/app/commit/3f9c2ab1";
    let log = s_.api(
        "plugin.link.open",
        json!({"plugin": "acme.surf", "handler": "gh-commit", "url": url, "pane": root}),
    );
    assert_eq!(log["log"]["source"], "link_handler", "{log}");
    wait_for("link action", 15000, || st.join("link-env.txt").exists());
    wait_for("link env written", 5000, || {
        read(&st.join("link-env.txt")).contains("HERDR_PLUGIN_LINK_HANDLER_ID")
    });
    let env = read(&st.join("link-env.txt"));
    assert!(
        env.contains(&format!("HERDR_PLUGIN_CLICKED_URL={url}")),
        "{env}"
    );
    assert!(
        env.contains("HERDR_PLUGIN_LINK_HANDLER_ID=gh-commit"),
        "{env}"
    );
    assert!(env.contains("HERDR_PLUGIN_ACTION_ID=link"), "{env}");
    let mut out = s_.cmd(&[
        "api",
        "call",
        "plugin.link.open",
        &json!({"plugin": "acme.surf", "handler": "gh-commit", "url": "https://example.com/"})
            .to_string(),
    ]);
    assert!(
        !out.output().unwrap().status.success(),
        "a non-matching URL is refused"
    );

    // ---- window title (pushed to the client) ----------------------------------------------
    s_.api(
        "compat.herdr.call",
        json!({"method": "client.window_title.set", "params": {"title": "deploying"}}),
    );
    let mut seen = vec![];
    let deadline = Instant::now() + Duration::from_secs(10);
    while !seen
        .iter()
        .any(|e: &Value| e["data"]["title"] == "deploying")
    {
        assert!(Instant::now() < deadline, "no pushed title event: {seen:?}");
        if let Ok(ServerFrame::Events { events, .. }) =
            r.rx.recv_timeout(Duration::from_millis(200))
        {
            seen.extend(
                events
                    .iter()
                    .map(|e| serde_json::from_str(&e.json).unwrap()),
            );
        }
    }
    assert_eq!(
        s_.api("compat.ui.state", json!({}))["window_title"],
        "deploying"
    );

    // ---- scroll report → pane.scroll_changed ------------------------------------------------
    r.send(&ClientFrame::ScrollView {
        pane: root_id.clone(),
        offset: 7,
        total: 120,
    });
    wait_for("scroll in pane.list", 10000, || {
        s_.herdr(&["pane", "list"])["panes"][0]["scroll"] == 7
    });
    let evs = s_.api("events.read", json!({"types": "pane.scroll_changed"}));
    let e = &evs["events"][0];
    assert_eq!(e["data"]["offset"], 7, "{evs}");
    assert_eq!(e["subject"]["pane"], root_id.as_str());
}
