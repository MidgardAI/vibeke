//! Browser pane page I/O in the TUI (06 B3.2): console split toggle, page clipboard through
//! the `clipboard` config, file drops with confirmation, clipboard images, pinned viewports in
//! the view and the chrome. No host terminal or real clipboard is touched.

use super::*;
use crate::app::test_app;
use crate::browser::{Reply, update_views};
use crate::screen::Grid;
use vk_proto::model::*;
use vk_proto::render::{BrowserStatus, MediaPane, ServerFrame};

type Rx = tokio::sync::mpsc::UnboundedReceiver<ClientFrame>;

fn pane(id: &str, browser: bool) -> Pane {
    let mut p: Pane = serde_json::from_value(json!({
        "id": id, "handle": format!("w1:{id}"), "tab": "T", "workspace": "W", "title": null,
        "auto_title": "x", "cwd": null, "cols": 80, "rows": 24,
        "child_pid": null, "fg_cmdline": [], "exited": false, "exit_code": null,
        "unread": false, "marked_unread": false, "pinned": false, "created_by": "user",
        "recovered": null,
        "browser": {"url": "http://localhost:5173/", "machine": "", "task": null, "preview": null,
                    "source_pane": "p1", "history": [], "history_index": 0, "title": ""}
    }))
    .unwrap();
    if !browser {
        p.browser = None;
    }
    p
}

/// `n` machines (0 local); the tab with shell p1 and browser pane bp belongs to machine
/// `n - 1`; views sent.
fn setup(n: usize) -> (App, Vec<Rx>) {
    let (mut app, mut rxs) = test_app(n);
    let mi = n - 1;
    app.cur = mi;
    let m = &mut app.machines[mi];
    m.model.workspaces = vec![Workspace {
        id: "W".into(),
        handle: "w1".into(),
        name: None,
        auto_name: "w".into(),
        root_path: "/".into(),
        task: None,
        order: 1.0,
        branch: None,
    }];
    m.model.tabs = vec![Tab {
        id: "T".into(),
        handle: "w1:t1".into(),
        workspace: "W".into(),
        title: None,
        number: 1,
        layout: LayoutNode::Split {
            dir: SplitDir::Horizontal,
            children: vec![
                (LayoutNode::Leaf { pane: "p1".into() }, 0.5),
                (LayoutNode::Leaf { pane: "bp".into() }, 0.5),
            ],
        },
        focused_pane: Some("bp".into()),
        zoomed_pane: None,
        order: 1.0,
        floating: Default::default(),
        floats_hidden: false,
    }];
    m.model.panes = vec![pane("p1", false), pane("bp", true)];
    m.focus = ClientFocus {
        workspace: Some("W".into()),
        tab: Some("T".into()),
        pane: Some("bp".into()),
    };
    app.caps.kitty_graphics = true;
    app.caps.truecolor = true;
    app.caps.cell_w = 16;
    app.caps.cell_h = 32;
    app.caps.dpr_x100 = 200;
    app.sidebar = false;
    app.size = (81, 25);
    update_views(&mut app);
    for rx in rxs.iter_mut() {
        while rx.try_recv().is_ok() {}
    }
    (app, rxs)
}

fn drain(rx: &mut Rx) -> Vec<ClientFrame> {
    let mut v = Vec::new();
    while let Ok(f) = rx.try_recv() {
        v.push(f);
    }
    v
}

fn commands(rx: &mut Rx) -> Vec<serde_json::Value> {
    drain(rx)
        .into_iter()
        .filter_map(|f| match f {
            ClientFrame::Command { json, .. } => serde_json::from_str(&json).ok(),
            _ => None,
        })
        .collect()
}

fn drops(rx: &mut Rx) -> Vec<Vec<String>> {
    drain(rx)
        .into_iter()
        .filter_map(|f| match f {
            ClientFrame::Browser {
                cmd: BrowserCmd::DropFiles(p),
                ..
            } => Some(p),
            _ => None,
        })
        .collect()
}

fn prefix(app: &mut App, k: KeyEvent) {
    app.on_key(app.keymap.prefix.clone());
    app.on_key(k);
}

#[test]
fn prefix_alt_c_toggles_the_console_split_on_the_owner() {
    let (mut app, mut rxs) = setup(2);
    prefix(
        &mut app,
        KeyEvent::new(Key::Char('c'), vk_proto::input::Mods::ALT),
    );
    let cmds = commands(&mut rxs[1]);
    assert!(
        cmds.iter().any(|c| c["method"] == "browser.pane.console"
            && c["params"]["pane"] == "bp"
            && c["params"]["toggle"] == true),
        "the split goes into the owner's layout: {cmds:?}"
    );
    assert!(commands(&mut rxs[0]).is_empty());
    // The palette action is the same.
    assert!(crate::browser::action_name(&mut app, "browser_console"));
    assert_eq!(commands(&mut rxs[1]).len(), 1);
    // Replies: opened / closed.
    crate::browser::on_reply(
        &mut app,
        1,
        Reply::ConsoleSplit,
        Ok(json!({"pane": "C1", "browser_pane": "bp"})),
    );
    assert!(app.toasts.iter().any(|t| t.text.contains("c console")));
    crate::browser::on_reply(
        &mut app,
        1,
        Reply::ConsoleSplit,
        Ok(json!({"closed": "C1"})),
    );
    assert!(app.toasts.iter().any(|t| t.text == "console split closed"));
}

#[test]
fn page_clipboard_is_judged_as_a_write_from_the_panes_owner() {
    // Local pane (machine 0 owns and renders it): `osc52_write = allow` → copied.
    let (mut app, _rxs) = setup(1);
    app.on_frame(
        0,
        ServerFrame::Clipboard {
            selection: vk_proto::render::ClipSel::Clipboard,
            data: b"from the page".to_vec(),
            pane: "bp".into(),
        },
    );
    assert_eq!(
        app.clipboard_sink.as_ref().unwrap(),
        &vec![(b"from the page".to_vec(), false)]
    );
    // A remote machine's page rendered by the local server: ask-once for that machine.
    let (mut app, _rxs) = setup(2);
    app.on_frame(
        0,
        ServerFrame::Clipboard {
            selection: vk_proto::render::ClipSel::Clipboard,
            data: b"devbox page".to_vec(),
            pane: "bp".into(),
        },
    );
    assert!(app.clipboard_sink.as_ref().unwrap().is_empty());
    assert_eq!(app.clip.pending.len(), 1);
    assert_eq!(app.clip.pending[0].machine, 1, "attributed to the owner");
    // Over the size limit: dropped.
    app.config.clipboard.remote_write_max_bytes = vk_config::ByteSize(4);
    app.machines[1].clipboard_allowed = Some(true);
    app.on_frame(
        0,
        ServerFrame::Clipboard {
            selection: vk_proto::render::ClipSel::Clipboard,
            data: b"too long".to_vec(),
            pane: "bp".into(),
        },
    );
    assert!(app.clipboard_sink.as_ref().unwrap().is_empty());
    // An ordinary pane's OSC 52 is unchanged.
    let (mut app, _rxs) = setup(1);
    app.on_frame(
        0,
        ServerFrame::Clipboard {
            selection: vk_proto::render::ClipSel::Clipboard,
            data: b"shell".to_vec(),
            pane: "p1".into(),
        },
    );
    assert_eq!(app.clipboard_sink.as_ref().unwrap().len(), 1);
}

#[test]
fn dropped_paths_are_confirmed_then_sent_or_uploaded() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("my shot.png");
    std::fs::write(&f, b"png").unwrap();
    let canon = f.canonicalize().unwrap().to_string_lossy().into_owned();
    let pasted = format!("'{}'", f.display());
    // Local media host (one machine): confirm, then the paths go to the page.
    let (mut app, mut rxs) = setup(1);
    assert!(crate::browser::on_paste(&mut app, &pasted));
    assert!(matches!(app.mode, Mode::Popup(Popup::BrowserDrop(_))));
    assert!(
        drops(&mut rxs[0]).is_empty(),
        "nothing before the user confirms"
    );
    let mut g = Grid::new(81, 25);
    crate::draw::compose(&app, &mut g);
    let text: String = (0..25)
        .map(|y| {
            g.row(y)
                .iter()
                .map(|c| c.text.as_str().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("files into the page"), "{text}");
    assert!(text.contains("my shot.png"), "{text}");
    // Other keys do nothing.
    app.on_key(KeyEvent::ch('x'));
    assert!(matches!(app.mode, Mode::Popup(Popup::BrowserDrop(_))));
    app.on_key(KeyEvent::ch('d'));
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(drops(&mut rxs[0]), vec![vec![canon.clone()]]);
    // `t` pastes the text instead; esc cancels.
    assert!(crate::browser::on_paste(&mut app, &pasted));
    app.on_key(KeyEvent::ch('t'));
    assert!(drain(&mut rxs[0]).iter().any(
        |f| matches!(f, ClientFrame::Browser { cmd: BrowserCmd::Text(t), .. } if *t == pasted)
    ));
    assert!(crate::browser::on_paste(&mut app, &pasted));
    app.on_key(KeyEvent::named(NamedKey::Escape));
    assert!(drain(&mut rxs[0]).is_empty());
    // Not files: a directory path and ordinary text are pasted as text, no question.
    let dirpath = dir.path().display().to_string();
    assert!(crate::browser::on_paste(&mut app, &dirpath));
    assert!(matches!(app.mode, Mode::Normal));
    assert!(drain(&mut rxs[0]).iter().any(|f| matches!(
        f,
        ClientFrame::Browser {
            cmd: BrowserCmd::Text(_),
            ..
        }
    )));
    // Too big: pasted as text with a note.
    let big = dir.path().join("big.bin");
    std::fs::File::create(&big)
        .unwrap()
        .set_len(DROP_MAX + 1)
        .unwrap();
    assert!(crate::browser::on_paste(
        &mut app,
        &big.display().to_string()
    ));
    assert!(matches!(app.mode, Mode::Normal));
    assert!(
        app.toasts
            .iter()
            .any(|t| t.text.contains("larger than 50 MiB"))
    );
    // Remote media host (plain-SSH topology: no local server connected): the file is uploaded
    // to that machine's inbox first; the inbox paths then go to the page.
    let (mut app, mut rxs) = setup(2);
    app.machines[0].status = "offline".into();
    app.machines[0].tx = None;
    app.browser.panes.entry("bp".into()).or_default().host = 1;
    assert!(crate::browser::on_paste(&mut app, &pasted));
    app.on_key(KeyEvent::named(NamedKey::Enter));
    assert!(drops(&mut rxs[1]).is_empty());
    let (id, t) = app
        .uploads
        .transfers
        .iter()
        .next()
        .map(|(k, v)| (k.clone(), v.browser))
        .expect("a transfer");
    assert!(t, "a browser transfer");
    assert_eq!((id.machine, id.pane.as_str()), (1, "bp"));
    crate::upload::on_event(
        &mut app,
        crate::upload::UploadEvent::FileDone {
            id,
            index: 0,
            path: "/inbox/abc/my shot.png".into(),
        },
    );
    assert_eq!(
        drops(&mut rxs[1]),
        vec![vec!["/inbox/abc/my shot.png".to_string()]]
    );
}

#[test]
fn prefix_shift_v_reads_the_clipboard_image_only_when_asked() {
    let (mut app, mut rxs) = setup(1);
    assert!(app.browser.clip_read.is_none());
    prefix(
        &mut app,
        KeyEvent::new(Key::Char('v'), vk_proto::input::Mods::SHIFT),
    );
    assert_eq!(app.browser.clip_read.as_deref(), Some("bp"));
    // The main loop takes the request once.
    assert_eq!(take_clip_read(&mut app).as_deref(), Some("bp"));
    assert!(take_clip_read(&mut app).is_none());
    // An image arrived: uploaded to the media host's inbox, then dropped into the page.
    on_clip_image(
        &mut app,
        "bp",
        Some(("image/png".into(), vec![0x89, b'P', b'N', b'G'])),
    );
    let (id, t) = app.uploads.transfers.iter().next().expect("transfer");
    assert!(t.browser && t.items[0].name.ends_with(".png") && t.total == 4);
    let id = id.clone();
    crate::upload::on_event(
        &mut app,
        crate::upload::UploadEvent::FileDone {
            id,
            index: 0,
            path: "/inbox/x/clipboard.png".into(),
        },
    );
    assert_eq!(
        drops(&mut rxs[0]),
        vec![vec!["/inbox/x/clipboard.png".to_string()]]
    );
    // Nothing on the clipboard: a hint, nothing sent.
    on_clip_image(&mut app, "bp", None);
    assert!(
        app.toasts
            .iter()
            .any(|t| t.text.contains("no image on the clipboard"))
    );
    assert!(app.uploads.transfers.is_empty());
}

#[test]
fn osc5522_replies_parse() {
    let png = vec![0x89u8, b'P', b'N', b'G', 1, 2, 3];
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let req = osc5522_request("image/png", true);
    assert_eq!(
        req,
        format!("\x1b]5522;type=read;{}\x1b\\\x1b[c", b64(b"image/png")).into_bytes()
    );
    assert!(!osc5522_request("image/png", false).ends_with(b"\x1b[c"));
    let mime = b64(b"image/png");
    let msg = |meta: &str, payload: &str| format!("\x1b]5522;{meta};{payload}\x1b\\");
    let mut buf = msg("type=read:status=OK", "");
    assert_eq!(parse_5522(buf.as_bytes()), Clip5522::Pending { seen: true });
    // Data in two chunks, BEL- and ST-terminated.
    buf += &format!(
        "\x1b]5522;type=read:status=DATA:mime={mime};{}\x07",
        b64(&png[..3])
    );
    buf += &msg(
        &format!("type=read:status=DATA:mime={mime}"),
        &b64(&png[3..]),
    );
    assert_eq!(parse_5522(buf.as_bytes()), Clip5522::Pending { seen: true });
    buf += &msg("type=read:status=DONE", "");
    assert_eq!(
        parse_5522(buf.as_bytes()),
        Clip5522::Done(vec![("image/png".into(), png.clone())])
    );
    // Refused, unsupported (DA1 first), incomplete.
    assert_eq!(
        parse_5522(msg("type=read:status=EPERM", "").as_bytes()),
        Clip5522::Failed("EPERM".into())
    );
    assert!(matches!(parse_5522(b"\x1b[?62;22c"), Clip5522::Failed(_)));
    assert_eq!(
        parse_5522(b"\x1b]5522;type=read:status=DA"),
        Clip5522::Pending { seen: false }
    );
}

#[test]
fn pinned_device_reaches_the_media_host_and_shows_in_the_chrome() {
    let (mut app, mut rxs) = setup(2);
    // Pinning (the owner's model changes): a new view goes to the media host with the spec.
    app.machines[1].model.panes[1]
        .browser
        .as_mut()
        .unwrap()
        .device = Some("iphone-15".into());
    update_views(&mut app);
    let views: Vec<MediaPane> = drain(&mut rxs[0])
        .into_iter()
        .filter_map(|f| match f {
            ClientFrame::MediaView { panes, .. } => Some(panes),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(views.len(), 1, "re-sent on pin change");
    assert_eq!(views[0].spec.device.as_deref(), Some("iphone-15"));
    // Unchanged → not re-sent.
    update_views(&mut app);
    assert!(drain(&mut rxs[0]).is_empty());
    crate::browser::on_state(
        &mut app,
        0,
        "bp".into(),
        BrowserStatus {
            url: "http://localhost:5173/".into(),
            env: "laptop chromium → m1 loopback".into(),
            ..Default::default()
        },
    );
    let mut g = Grid::new(121, 25);
    app.size = (121, 25);
    crate::draw::compose(&app, &mut g);
    let row: String = g
        .row(1)
        .iter()
        .map(|c| c.text.as_str().to_string())
        .collect();
    assert!(row.contains("▯ iphone-15"), "{row}");
}
