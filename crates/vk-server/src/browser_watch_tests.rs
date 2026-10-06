//! Watch / take-over tests with the agent browser fake (`vk_browser::fake`): a watch pane is
//! fed by the agent session's screencast through the media channel, input is ignored until
//! the pane takes the session over, take-over gives the agent `human_control` errors, and
//! release (or closing the pane) hands it back.

use super::*;
use crate::ServerOpts;
use crate::agent_browser::{LaunchCtx, Launched};
use crate::api::Ctx;
use crate::browser_pane::MediaSession;
use crate::core::Tx;
use crate::paths::Paths;
use std::sync::{Mutex, Once};
use vk_browser::fake::{Call, Emitter, FakeBrowser};
use vk_proto::input::{Key, KeyEvent, Mods};
use vk_proto::model::{
    BrowserPane, LayoutNode, Pane, Preview, PreviewSource, PreviewStatus, Tab, Workspace,
};
use vk_proto::render::{MediaPane, ServerFrame};

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let base = std::env::temp_dir().join(format!("vk-watch-tests-{}", std::process::id()));
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

struct Fake {
    calls: Arc<Mutex<Vec<Call>>>,
    emitter: Emitter,
}

impl Fake {
    fn calls_of(&self, m: &str) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.0 == m)
            .cloned()
            .collect()
    }
}

struct Env {
    _dir: tempfile::TempDir,
    server: Arc<Server>,
    fake: Arc<Mutex<Option<Arc<Fake>>>>,
}

fn user() -> Ctx {
    Ctx {
        client_id: "c-user".into(),
        kind: "cli".into(),
        pane_scope: None,
        remote: false,
    }
}

fn agent() -> Ctx {
    Ctx {
        client_id: "c-agent".into(),
        kind: "cli".into(),
        pane_scope: Some("pane-a".into()),
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
            machine: "devbox".into(),
            bin: "/bin/false".into(),
            hold_args: vec![],
            default_shell: None,
            env: vec![],
            shims: false,
        };
        let server = Server::new(paths, opts).unwrap();
        let fake: Arc<Mutex<Option<Arc<Fake>>>> = Arc::default();
        let slot = fake.clone();
        server
            .agent_browser
            .set_launcher(Arc::new(move |_ctx: &LaunchCtx| {
                let mut f = FakeBrowser::start();
                let events = f.take_events();
                *slot.lock().unwrap() = Some(Arc::new(Fake {
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
        e.layout();
        e
    }

    fn fake(&self) -> Arc<Fake> {
        self.fake.lock().unwrap().clone().expect("browser launched")
    }

    /// One workspace, one tab, the agent's pane `pane-a`; a declared preview on :5173.
    fn layout(&self) {
        let mut c = self.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.ws(Workspace {
            id: "ws".into(),
            handle: "w1".into(),
            name: None,
            auto_name: "app".into(),
            root_path: "/tmp".into(),
            task: None,
            order: 1.0,
            branch: None,
        });
        tx.tab(Tab {
            id: "tab".into(),
            handle: "w1:t1".into(),
            workspace: "ws".into(),
            title: None,
            number: 1,
            layout: LayoutNode::Leaf {
                pane: "pane-a".into(),
            },
            focused_pane: Some("pane-a".into()),
            zoomed_pane: None,
            order: 1.0,
            floating: Default::default(),
            floats_hidden: false,
        });
        tx.pane(Pane {
            id: "pane-a".into(),
            handle: "w1:p1".into(),
            tab: "tab".into(),
            workspace: "ws".into(),
            title: None,
            auto_title: "claude".into(),
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
        });
        self.server.commit(&mut c, tx).unwrap();
        drop(c);
        self.server.with_core(|c| {
            c.model.previews.push(Preview {
                id: crate::core::ulid(),
                handle: "v1".into(),
                machine: "devbox".into(),
                pane: Some("pane-a".into()),
                task: None,
                port: 5173,
                path: "/".into(),
                label: None,
                url: "http://localhost:5173/".into(),
                scheme: "http".into(),
                status: PreviewStatus::Up,
                source: PreviewSource::Declared,
                pid: None,
                first_seen_ms: 0,
                last_seen_ms: 0,
            })
        });
    }

    async fn call(&self, ctx: &Ctx, method: &str, p: Value) -> R {
        crate::api::authorize(&self.server, ctx, method, &p)?;
        if let Some(r) = super::super::api(&self.server, ctx, method, &p).await {
            return r;
        }
        crate::agent_browser::api(&self.server, ctx, method, &p)
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

    fn cdp_session(&self, handle: &str) -> String {
        self.server
            .agent_browser
            .sessions()
            .into_iter()
            .find(|x| x.handle == handle)
            .unwrap()
            .cdp_session
            .clone()
    }

    fn spec(&self, pane: &str) -> BrowserPane {
        self.server
            .with_core(|c| c.pane(pane).and_then(|p| p.browser.clone()))
            .unwrap()
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

fn frames(buf: &mut Vec<u8>) -> Vec<ServerFrame> {
    let mut out = Vec::new();
    let mut fb = vk_proto::frame::FrameBuf::default();
    fb.push(buf);
    buf.clear();
    while let Ok(Some(f)) = fb.next_frame::<ServerFrame>() {
        out.push(f);
    }
    out
}

/// Flush the media session until a frame matching `pred` arrives; returns it with the chrome
/// states seen on the way.
async fn next_frame(
    server: &Arc<Server>,
    ms: &mut MediaSession,
    pred: impl Fn(&ServerFrame) -> bool,
) -> (ServerFrame, Vec<vk_proto::render::BrowserStatus>) {
    let t = Instant::now();
    let mut states = Vec::new();
    loop {
        let mut buf = Vec::new();
        ms.flush(server, &mut buf).await.unwrap();
        for f in frames(&mut buf) {
            if let ServerFrame::BrowserState { state, .. } = &f {
                states.push(state.clone());
            }
            if let ServerFrame::Media(m) = &f {
                ms.on_ack(&m.pane, m.seq);
            }
            if pred(&f) {
                return (f, states);
            }
        }
        assert!(t.elapsed() < Duration::from_secs(8), "no matching frame");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn png(w: u32, h: u32, px: [u8; 4]) -> String {
    use base64::Engine as _;
    let mut img = Rgba::new(w, h);
    img.fill_rect(0, 0, w, h, px);
    let png = vk_browser::frame::encode_png(w, h, &img.data, true).unwrap();
    base64::engine::general_purpose::STANDARD.encode(png)
}

fn mp(pane: &str, spec: BrowserPane) -> MediaPane {
    MediaPane {
        pane: pane.into(),
        owner: String::new(),
        spec,
        cols: 20,
        rows: 10,
        cell_w: 10,
        cell_h: 20,
        dpr: 2.0,
    }
}

fn key(c: char) -> BrowserCmd {
    BrowserCmd::Key(KeyEvent::new(Key::Char(c), Mods::empty()))
}

#[test]
fn decide_is_read_only_until_taken_over() {
    let click = BrowserCmd::Mouse {
        kind: MouseKind::Press,
        button: MouseButton::Left,
        x: 50.0,
        y: 25.0,
        mods: Mods::empty(),
        clicks: 1,
    };
    for cmd in [key('a'), BrowserCmd::Text("hi".into()), click.clone()] {
        assert_eq!(decide(&cmd, false, false, 1.0, 2.0), Decision::ReadOnly);
    }
    assert_eq!(
        decide(&BrowserCmd::TakeOver(true), false, false, 1.0, 2.0),
        Decision::Control(true)
    );
    assert_eq!(
        decide(&BrowserCmd::Back, true, false, 1.0, 2.0),
        Decision::Unsupported
    );
    // Taken over here: keys map to CDP key events (keyUp synthesized without releases).
    let Decision::Forward(v) = decide(&key('a'), true, false, 1.0, 2.0) else {
        panic!()
    };
    assert_eq!(v.len(), 2);
    // Mouse: pane CSS px × DPR = pane device px, × to_page = agent CSS px.
    let Decision::Forward(v) = decide(&click, true, false, 0.5, 2.0) else {
        panic!()
    };
    let (m, p) = v[0].to_command();
    assert_eq!(m, "Input.dispatchMouseEvent");
    assert_eq!((p["x"].as_f64(), p["y"].as_f64()), (Some(50.0), Some(25.0)));
    assert_eq!(p["type"], "mousePressed");
    assert_eq!(p["clickCount"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_take_over_release_and_close() {
    let e = Env::new();
    let opened = e
        .call(&agent(), "browser.open", json!({}))
        .await
        .expect("agent opens a session");
    assert_eq!(opened["session"], "b1");
    // Agents can't watch.
    let denied = e
        .call(&agent(), "browser.watch", json!({"session": "b1"}))
        .await
        .unwrap_err();
    assert_eq!(denied.data.kind, "permission_denied");
    // By agent pane: the newest session of that pane, next to it.
    let r = e
        .call(&user(), "browser.watch", json!({"agent_pane": "pane-a"}))
        .await
        .unwrap();
    assert_eq!(r["session"], "b1");
    assert_eq!(r["source_pane"], "pane-a");
    let wp = r["pane"].as_str().unwrap().to_string();
    let spec = e.spec(&wp);
    assert_eq!(spec.watch.as_deref(), Some("b1"));
    assert_eq!(
        e.server
            .with_core(|c| c.pane(&wp).map(|p| p.auto_title.clone())),
        Some("◉ watching b1".into())
    );
    let sess = e.cdp_session("b1");

    // Visible: the pump attaches to the agent's screencast; frames flow as tiles.
    let mut ms = MediaSession::new(&e.server, false);
    ms.on_view(&e.server, vec![mp(&wp, spec.clone())], false, false);
    let f = e.fake();
    until("screencast", || {
        !f.calls_of("Page.startScreencast").is_empty()
    })
    .await;
    assert_eq!(
        f.calls_of("Page.startScreencast")[0].2.as_deref(),
        Some(sess.as_str())
    );
    f.emitter.emit(
        "Page.screencastFrame",
        json!({"sessionId": 1, "data": png(400, 300, [200, 10, 10, 255]),
               "metadata": {"deviceWidth": 400, "deviceHeight": 300}}),
        Some(&sess),
    );
    let (m, states) = next_frame(&e.server, &mut ms, |f| matches!(f, ServerFrame::Media(_))).await;
    let ServerFrame::Media(m) = m else { panic!() };
    // 400×300 fitted into 20×10 cells of 10×20 px (200×200): 200×150.
    assert_eq!((m.width, m.height), (200, 150));
    assert!(m.reset && !m.tiles.is_empty());
    let st = states.last().expect("chrome state");
    assert_eq!(st.watch.as_deref(), Some("b1"));
    assert!(!st.human_control);
    assert_eq!(st.env, "agent session b1 · devbox");

    // Read-only: input is ignored (nothing reaches the page) and a hint is shown.
    let keys_before = f.calls_of("Input.dispatchKeyEvent").len();
    ms.on_cmd(&e.server, &wp, key('x'));
    ms.on_cmd(
        &e.server,
        &wp,
        BrowserCmd::Wheel {
            x: 1.0,
            y: 1.0,
            dx: 0.0,
            dy: 40.0,
            mods: Mods::empty(),
        },
    );
    let (_, states) = next_frame(&e.server, &mut ms, |f| {
        matches!(f, ServerFrame::BrowserState { state, .. } if state.notice.as_deref().is_some_and(|n| n.contains("read-only")))
    })
    .await;
    drop(states);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(f.calls_of("Input.dispatchKeyEvent").len(), keys_before);
    assert!(f.calls_of("Input.dispatchMouseEvent").is_empty());
    // The agent still drives.
    e.call(
        &agent(),
        "browser.press",
        json!({"session": "b1", "key": "Tab"}),
    )
    .await
    .expect("agent works while watched");

    // Take over: the agent gets human_control, the pane's input reaches the page.
    ms.on_cmd(&e.server, &wp, BrowserCmd::TakeOver(true));
    let (ServerFrame::BrowserState { state, .. }, _) = next_frame(
        &e.server,
        &mut ms,
        |f| matches!(f, ServerFrame::BrowserState { state, .. } if state.controlled_here),
    )
    .await
    else {
        panic!()
    };
    assert!(state.human_control);
    assert_eq!(e.events("browser.taken_over").len(), 1);
    let err = e
        .call(
            &agent(),
            "browser.press",
            json!({"session": "b1", "key": "Tab"}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.data.kind, "human_control");
    let keys_before = f.calls_of("Input.dispatchKeyEvent").len();
    ms.on_cmd(&e.server, &wp, key('q'));
    until("human key", || {
        f.calls_of("Input.dispatchKeyEvent").len() >= keys_before + 2
    })
    .await;
    let k = f.calls_of("Input.dispatchKeyEvent");
    assert_eq!(k[keys_before].1["key"], "q");
    assert_eq!(k[keys_before].2.as_deref(), Some(sess.as_str()));
    // A click at pane CSS (50, 25): ×2 DPR = device (100, 50); frame scaled 0.5 → agent (200, 100).
    ms.on_cmd(
        &e.server,
        &wp,
        BrowserCmd::Mouse {
            kind: MouseKind::Press,
            button: MouseButton::Left,
            x: 50.0,
            y: 25.0,
            mods: Mods::empty(),
            clicks: 1,
        },
    );
    until("human click", || {
        !f.calls_of("Input.dispatchMouseEvent").is_empty()
    })
    .await;
    let c = &f.calls_of("Input.dispatchMouseEvent")[0];
    assert_eq!(c.1["x"].as_f64(), Some(200.0));
    assert_eq!(c.1["y"].as_f64(), Some(100.0));

    // Release: the agent drives again.
    ms.on_cmd(&e.server, &wp, BrowserCmd::TakeOver(false));
    next_frame(&e.server, &mut ms, |f| {
        matches!(f, ServerFrame::BrowserState { state, .. } if !state.human_control && state.notice.as_deref().is_some_and(|n| n.contains("released")))
    })
    .await;
    assert_eq!(e.events("browser.released").len(), 1);
    e.call(
        &agent(),
        "browser.press",
        json!({"session": "b1", "key": "Tab"}),
    )
    .await
    .expect("agent works after release");
    let keys_before = f.calls_of("Input.dispatchKeyEvent").len();
    ms.on_cmd(&e.server, &wp, key('z'));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        f.calls_of("Input.dispatchKeyEvent").len(),
        keys_before,
        "read-only again"
    );

    // A take-over from the CLI shows in the pane as "elsewhere" (still read-only here).
    e.call(&user(), "browser.take_over", json!({"session": "b1"}))
        .await
        .unwrap();
    next_frame(&e.server, &mut ms, |f| {
        matches!(f, ServerFrame::BrowserState { state, .. } if state.human_control && !state.controlled_here)
    })
    .await;
    e.call(&user(), "browser.release", json!({"session": "b1"}))
        .await
        .unwrap();

    // Hidden: the agent's screencast stops.
    ms.on_view(&e.server, vec![], false, false);
    until("stop", || !f.calls_of("Page.stopScreencast").is_empty()).await;

    // Take over again, then close the pane: control goes back to the agent.
    ms.on_view(&e.server, vec![mp(&wp, spec)], false, false);
    ms.on_cmd(&e.server, &wp, BrowserCmd::TakeOver(true));
    until("taken", || {
        e.server
            .agent_browser
            .watch_info("b1")
            .is_some_and(|i| i.human.is_some())
    })
    .await;
    let pane = e.server.with_core(|c| c.pane(&wp).cloned()).unwrap();
    {
        let mut c = e.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.close_pane(&pane);
        e.server.commit(&mut c, tx).unwrap();
    }
    crate::browser_pane::gc(&e.server);
    until("released on close", || {
        e.server
            .agent_browser
            .watch_info("b1")
            .is_some_and(|i| i.human.is_none())
    })
    .await;
    assert_eq!(e.events("browser.released").len(), 3);
    ms.close(&e.server);
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_errors() {
    let e = Env::new();
    let r = e
        .call(&user(), "browser.watch", json!({"session": "b9"}))
        .await
        .unwrap_err();
    assert_eq!(r.data.kind, "not_found");
    let r = e
        .call(&user(), "browser.watch", json!({"agent_pane": "pane-a"}))
        .await
        .unwrap_err();
    assert_eq!(r.data.kind, "not_found");
    assert!(
        r.message.contains("no open browser session"),
        "{}",
        r.message
    );
    let r = e
        .call(&user(), "browser.watch", json!({}))
        .await
        .unwrap_err();
    assert_eq!(r.data.kind, "invalid_params");
}
