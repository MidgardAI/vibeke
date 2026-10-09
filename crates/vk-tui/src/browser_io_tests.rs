//! Browser pane page I/O in the TUI (06 B3.2): console split toggle, page clipboard through
//! the `clipboard` config, file drops with confirmation, clipboard images, pinned viewports in
//! the view and the chrome. No host terminal or real clipboard is touched.

use super::*;
use crate::app::test_app;
use crate::browser::{Reply, update_views};
use crate::screen::Grid;
use std::io::Write as _;
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
    app.config.clipboard.remote_write = vk_config::RemoteWrite::AskOnce;
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

/// Finding: a clipboard frame naming a browser pane is attributed to the machine that sent it,
/// and only the pane's media host may send one.
#[test]
fn page_clipboard_only_from_the_panes_media_host() {
    let clip = |app: &mut App, from: usize, pane: &str, data: &[u8]| {
        app.on_frame(
            from,
            ServerFrame::Clipboard {
                selection: vk_proto::render::ClipSel::Clipboard,
                data: data.to_vec(),
                pane: pane.into(),
            },
        );
    };
    // A local browser pane (machine 0 owns and renders it); machine 1 is a remote that knows
    // its id and names it to borrow the local `osc52_write` policy: dropped.
    let (mut app, _rxs) = setup(1);
    let mut remote = test_app(2).0.machines.remove(1);
    remote.label = "devbox".into();
    app.machines.push(remote);
    assert!(crate::browser::renders(&app, 0, "bp") && !crate::browser::renders(&app, 1, "bp"));
    clip(&mut app, 1, "bp", b"from devbox");
    assert!(app.clipboard_sink.as_ref().unwrap().is_empty());
    assert!(app.clip.pending.is_empty(), "no prompt either");
    assert_eq!(app.clip.dropped, 1);
    // The local media host's frame for it still goes through.
    clip(&mut app, 0, "bp", b"local page");
    assert_eq!(app.clipboard_sink.as_ref().unwrap().len(), 1);

    // Three machines: bp belongs to machine 2 and is rendered by the local server (0). Machine 1
    // names machine 2's pane: dropped, and not judged under machine 2's (allowed) policy.
    let (mut app, _rxs) = setup(3);
    app.machines[2].clipboard_allowed = Some(true);
    app.machines[1].clipboard_allowed = Some(true);
    assert!(crate::browser::renders(&app, 0, "bp"));
    clip(&mut app, 1, "bp", b"from machine 1");
    assert!(app.clipboard_sink.as_ref().unwrap().is_empty());
    assert_eq!(app.clip.dropped, 1);
    // A remote media host (the plain-SSH topology) renders it: judged as that machine's write.
    app.machines[0].status = "offline".into();
    app.machines[0].tx = None;
    update_views(&mut app);
    assert!(crate::browser::renders(&app, 2, "bp"));
    app.machines[2].clipboard_allowed = None;
    app.config.clipboard.remote_write = vk_config::RemoteWrite::AskOnce;
    clip(&mut app, 2, "bp", b"rendered remotely");
    assert!(app.clipboard_sink.as_ref().unwrap().is_empty());
    assert_eq!(app.clip.pending.len(), 1);
    assert_eq!(app.clip.pending[0].machine, 2);
}

/// A file that changes between the prompt and the confirmation is refused.
fn ask_for(app: &mut App, path: &Path) {
    let pasted = format!("'{}'", path.display());
    assert!(crate::browser::on_paste(app, &pasted));
    assert!(matches!(app.mode, Mode::Popup(Popup::BrowserDrop(_))));
}

fn toasts(app: &App) -> Vec<String> {
    app.toasts.iter().map(|t| t.text.clone()).collect()
}

fn refused(app: &App, why: &str) -> bool {
    app.toasts
        .iter()
        .any(|t| t.text.contains(why) && t.text.contains("not dropped"))
}

#[test]
fn dropped_paths_are_confirmed_then_uploaded_from_the_checked_file() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("my shot.png");
    std::fs::write(&f, b"png").unwrap();
    let pasted = format!("'{}'", f.display());
    // Local media host (one machine): confirm, then the file is uploaded to the local server's
    // drop directory from the opened descriptor (never by path), then dropped from there.
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
    assert!(
        drops(&mut rxs[0]).is_empty(),
        "no local path goes to the page"
    );
    let (id, t) = app.uploads.transfers.iter().next().expect("a transfer");
    assert!(t.browser && t.total == 3);
    assert!(matches!(t.items[0].src, Source::File(_)), "the opened file");
    assert_eq!((id.machine, id.pane.as_str()), (0, "bp"));
    let id = id.clone();
    crate::upload::on_event(
        &mut app,
        crate::upload::UploadEvent::FileDone {
            id,
            index: 0,
            path: "/state/browser-drops/0123456789ab/my shot.png".into(),
        },
    );
    assert_eq!(
        drops(&mut rxs[0]),
        vec![vec![
            "/state/browser-drops/0123456789ab/my shot.png".to_string()
        ]]
    );
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
    // Remote media host (plain-SSH topology: no local server connected): uploaded the same way
    // to that machine; its drop paths then go to the page.
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
            path: "/drops/abc/my shot.png".into(),
        },
    );
    assert_eq!(
        drops(&mut rxs[1]),
        vec![vec!["/drops/abc/my shot.png".to_string()]]
    );
}

#[test]
fn a_file_replaced_after_the_prompt_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("report.pdf");
    std::fs::write(&f, b"the confirmed file").unwrap();
    let secret = dir.path().join("secret");
    std::fs::write(&secret, b"something else").unwrap();
    let (mut app, _rxs) = setup(1);
    // Replaced by a link to another file while the prompt is open.
    ask_for(&mut app, &f);
    std::fs::remove_file(&f).unwrap();
    std::os::unix::fs::symlink(&secret, &f).unwrap();
    app.on_key(KeyEvent::ch('d'));
    assert!(refused(&app, "replaced by a link"), "{:?}", toasts(&app));
    assert!(app.uploads.transfers.is_empty());
    // Replaced by another regular file of the same size (renamed over it).
    std::fs::remove_file(&f).unwrap();
    std::fs::write(&f, b"the confirmed file").unwrap();
    ask_for(&mut app, &f);
    let other = dir.path().join("other");
    std::fs::write(&other, b"an evil file here!").unwrap();
    std::fs::rename(&other, &f).unwrap();
    app.on_key(KeyEvent::ch('d'));
    assert!(
        refused(&app, "replaced since you confirmed"),
        "{:?}",
        toasts(&app)
    );
    assert!(app.uploads.transfers.is_empty());
}

#[test]
fn a_parent_directory_swapped_after_the_prompt_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    std::fs::write(work.join("a.txt"), b"mine").unwrap();
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("a.txt"), b"evil").unwrap();
    let (mut app, _rxs) = setup(1);
    ask_for(&mut app, &work.join("a.txt"));
    // The directory is moved away and a link to another directory takes its name.
    std::fs::rename(&work, dir.path().join("work.old")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &work).unwrap();
    app.on_key(KeyEvent::ch('d'));
    assert!(
        refused(&app, "replaced since you confirmed"),
        "{:?}",
        toasts(&app)
    );
    assert!(app.uploads.transfers.is_empty());
}

#[test]
fn a_file_that_grows_or_shrinks_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("log.txt");
    std::fs::write(&f, b"12345").unwrap();
    // Grew after the prompt: the size no longer matches.
    let (mut app, _rxs) = setup(1);
    ask_for(&mut app, &f);
    std::fs::write(&f, b"1234567890").unwrap();
    app.on_key(KeyEvent::ch('d'));
    assert!(refused(&app, "changed size"), "{:?}", toasts(&app));
    assert!(app.uploads.transfers.is_empty());
    // Grows (or shrinks) after it was opened, while the bytes are read for the upload.
    std::fs::write(&f, b"12345").unwrap();
    let df = DropFile::at(&f).unwrap();
    let file = open_snapshot(&df).unwrap();
    let mut buf = [0u8; 3];
    assert_eq!(crate::upload::read_snapshot(&file, 0, 5, &mut buf), Ok(3));
    assert_eq!(&buf, b"123");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&f)
        .unwrap()
        .write_all(b"678")
        .unwrap();
    assert_eq!(
        crate::upload::read_snapshot(&file, 3, 5, &mut buf),
        Err("file grew while uploading".into())
    );
    std::fs::File::create(&f).unwrap(); // truncated to 0
    assert_eq!(
        crate::upload::read_snapshot(&file, 3, 5, &mut buf),
        Err("file shrank while uploading".into())
    );
    // Unchanged: exactly the size, nothing more.
    std::fs::write(&f, b"12345").unwrap();
    let file = open_snapshot(&DropFile::at(&f).unwrap()).unwrap();
    let mut all = [0u8; 16];
    assert_eq!(crate::upload::read_snapshot(&file, 0, 5, &mut all), Ok(5));
}

#[test]
fn prefix_shift_v_reads_the_clipboard_image_only_when_asked() {
    let (mut app, mut rxs) = setup(1);
    // The key: nothing on the (test) clipboard → a hint, nothing sent, no terminal query.
    prefix(
        &mut app,
        KeyEvent::new(Key::Char('v'), vk_proto::input::Mods::SHIFT),
    );
    assert!(
        app.toasts
            .iter()
            .any(|t| t.text.contains("no image on the clipboard"))
    );
    assert!(app.uploads.transfers.is_empty());
    assert!(drain(&mut rxs[0]).is_empty());
    // Behind SSH the terminal's clipboard is another machine's: said so.
    app.caps.host_remote = true;
    request_image_paste(&mut app, "bp");
    assert!(
        app.toasts
            .iter()
            .any(|t| t.text.contains("terminal is on another machine"))
    );
    // An image arrived: uploaded to the media host's drop directory, then dropped.
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
            path: "/drops/x/clipboard.png".into(),
        },
    );
    assert_eq!(
        drops(&mut rxs[0]),
        vec![vec!["/drops/x/clipboard.png".to_string()]]
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
