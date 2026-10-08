//! Terminal effects in the TUI (03 §8): OSC 52 read policy and prompt, OSC 8 hover and
//! click, progress bars, exit badges and copy-mode prompt jumps. Fake machines only; nothing
//! touches the host terminal or clipboard.

use super::*;
use crate::app::{App, Mode, PaneBuf, Popup, test_app};
use crate::copy::{CopyMode, Outcome};
use crossterm::event::{KeyModifiers, MouseButton as CtButton, MouseEvent, MouseEventKind};
use serde_json::json;
use tokio::sync::mpsc;
use vk_proto::input::{Key, KeyEvent, Mods};
use vk_proto::model::*;
use vk_proto::render::{ClientFrame, Cursor, Link, Row, ServerFrame, Span, Style, mark};

type Rx = mpsc::UnboundedReceiver<ClientFrame>;

fn replies(rx: &mut Rx) -> Vec<(u64, String, Option<Vec<u8>>)> {
    let mut v = Vec::new();
    while let Ok(f) = rx.try_recv() {
        if let ClientFrame::ClipboardReply { req, pane, data } = f {
            v.push((req, pane, data));
        }
    }
    v
}

fn row(s: &str) -> Row {
    Row::new(
        vec![Span {
            style: Style::default(),
            text: s.into(),
            cols: s.chars().count() as u16,
        }],
        false,
    )
}

fn marked(s: &str, m: u8) -> Row {
    Row { mark: m, ..row(s) }
}

fn pane(id: &str) -> Pane {
    serde_json::from_value(json!({
        "id": id, "handle": format!("w1:{id}"), "tab": "t1", "workspace": "w1", "title": null,
        "auto_title": format!("sh-{id}"), "cwd": null, "cols": 80, "rows": 24, "child_pid": null,
        "fg_cmdline": [], "exited": false, "exit_code": null, "unread": false,
        "marked_unread": false, "pinned": false, "created_by": "user", "recovered": null
    }))
    .unwrap()
}

/// One workspace, one tab split p1 | p2, focus on p1.
fn setup() -> (App, Vec<Rx>) {
    let (mut app, rxs) = test_app(1);
    let m = &mut app.machines[0];
    m.model.workspaces = vec![
        serde_json::from_value(json!({
            "id": "w1", "handle": "w1", "name": "api", "auto_name": "api",
            "root_path": "/tmp", "task": null, "order": 1.0, "branch": null
        }))
        .unwrap(),
    ];
    m.model.tabs = vec![
        serde_json::from_value(json!({
            "id": "t1", "handle": "w1:t1", "workspace": "w1", "title": null, "number": 1,
            "layout": {"Split": {"dir": "Vertical", "children": [[{"Leaf": {"pane": "p1"}}, 0.5], [{"Leaf": {"pane": "p2"}}, 0.5]]}},
            "focused_pane": "p1", "zoomed_pane": null, "order": 1.0
        }))
        .unwrap(),
    ];
    m.model.panes = vec![pane("p1"), pane("p2")];
    m.focus = ClientFocus {
        workspace: Some("w1".into()),
        tab: Some("t1".into()),
        pane: Some("p1".into()),
    };
    (app, rxs)
}

fn query(app: &mut App, req: u64, pane: &str) {
    app.on_frame(
        0,
        ServerFrame::ClipboardQuery {
            req,
            pane: pane.into(),
            selection: vk_proto::render::ClipSel::Clipboard,
        },
    );
}

fn last_toast(app: &App) -> String {
    app.toasts
        .last()
        .map(|t| t.text.clone())
        .unwrap_or_default()
}

#[test]
fn clipboard_read_deny_is_the_default_and_says_so() {
    let (mut app, mut rxs) = setup();
    assert_eq!(
        app.config.clipboard.osc52_read,
        vk_config::Osc52Read::Deny,
        "default"
    );
    query(&mut app, 7, "p2");
    assert_eq!(replies(&mut rxs[0]), vec![(7, "p2".into(), None)]);
    assert!(last_toast(&app).contains("w1:p2"), "{}", last_toast(&app));
    assert!(last_toast(&app).contains("denied"));
}

#[test]
fn clipboard_read_allow_shares_and_toasts() {
    let (mut app, mut rxs) = setup();
    app.config.clipboard.osc52_read = vk_config::Osc52Read::Allow;
    query(&mut app, 1, "p1");
    assert_eq!(
        replies(&mut rxs[0]),
        vec![(1, "p1".into(), Some(b"(test clipboard)".to_vec()))]
    );
    assert!(last_toast(&app).contains("shared your clipboard (16 bytes)"));
    // Over SSH without a read command the clipboard here is not the user's: denied.
    app.osc = State::default();
    app.copyout.env.ssh = true;
    query(&mut app, 2, "p1");
    assert_eq!(replies(&mut rxs[0]), vec![(2, "p1".into(), None)]);
    assert!(
        last_toast(&app).contains("over SSH"),
        "{}",
        last_toast(&app)
    );
    // `VIBEKE_CLIPBOARD_READ_CMD` (here set directly) works anywhere.
    app.osc.read_cmd = Some("printf from-cmd".into());
    query(&mut app, 3, "p1");
    assert_eq!(
        replies(&mut rxs[0]),
        vec![(3, "p1".into(), Some(b"from-cmd".to_vec()))]
    );
}

#[test]
fn clipboard_read_ask_queues_a_prompt_naming_the_pane() {
    let (mut app, mut rxs) = setup();
    app.config.clipboard.osc52_read = vk_config::Osc52Read::Ask;
    query(&mut app, 1, "p2");
    // Nothing answered yet, no modal takeover: a notice.
    assert!(replies(&mut rxs[0]).is_empty());
    assert!(matches!(app.mode, Mode::Normal));
    assert!(last_toast(&app).contains("wants to read your clipboard"));
    app.action("review_clipboard", None);
    let Mode::Popup(Popup::ClipboardRead(r)) = &app.mode else {
        panic!("prompt");
    };
    assert_eq!((r.req, r.pane.as_str()), (1, "p2"));
    let mut g = crate::screen::Grid::new(120, 40);
    crate::draw::compose(&app, &mut g);
    let text = crate::tasks::grid_text(&g);
    assert!(
        text.contains("w1:p2") && text.contains("READ your clipboard"),
        "{text}"
    );
    // Unrelated keys keep the prompt.
    app.on_key(KeyEvent::ch('x'));
    assert!(matches!(app.mode, Mode::Popup(Popup::ClipboardRead(_))));
    app.on_key(KeyEvent::ch('y'));
    assert_eq!(
        replies(&mut rxs[0]),
        vec![(1, "p2".into(), Some(b"(test clipboard)".to_vec()))]
    );
    // Deny.
    query(&mut app, 2, "p2");
    app.action("review_clipboard", None);
    app.on_key(KeyEvent::ch('n'));
    assert_eq!(replies(&mut rxs[0]), vec![(2, "p2".into(), None)]);
    // Always for this pane: later reads are granted (with a toast) without asking.
    query(&mut app, 3, "p2");
    app.action("review_clipboard", None);
    app.on_key(KeyEvent::ch('a'));
    assert_eq!(replies(&mut rxs[0]).len(), 1);
    query(&mut app, 4, "p2");
    assert_eq!(
        replies(&mut rxs[0]),
        vec![(4, "p2".into(), Some(b"(test clipboard)".to_vec()))]
    );
    assert!(last_toast(&app).contains("shared"));
    // Another pane still asks; a full queue denies the oldest.
    for req in 10..15 {
        query(&mut app, req, &format!("x{req}"));
    }
    assert_eq!(replies(&mut rxs[0]), vec![(10, "x10".into(), None)]);
    assert_eq!(app.osc.reads.len(), 4);
}

fn linked() -> Row {
    Row {
        links: vec![Link {
            col: 4,
            cols: 5,
            uri: "https://example.com/x".into(),
        }],
        ..row("see docs! and")
    }
}

fn mouse(kind: MouseEventKind, x: u16, y: u16, mods: KeyModifiers) -> MouseEvent {
    MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers: mods,
    }
}

/// The host cell of pane `p`'s local (`col`, `row`).
fn at(app: &App, p: &str, col: u16, row: u16) -> (u16, u16) {
    let r = app
        .pane_rects()
        .into_iter()
        .find(|(id, _)| id == p)
        .unwrap()
        .1;
    (r.x + col, r.y + row)
}

#[test]
fn ctrl_hover_underlines_and_ctrl_click_opens_links() {
    let (mut app, _rxs) = setup();
    let mut b = PaneBuf::blank();
    b.lines = vec![
        linked(),
        row("plain https://plain.example/y here"),
        linked(),
    ];
    app.machines[0].panes.insert("p2".into(), b);
    let (x, y) = at(&app, "p2", 6, 0);
    // Hover without a modifier: nothing.
    app.on_mouse(mouse(MouseEventKind::Moved, x, y, KeyModifiers::NONE));
    assert!(app.osc.hover.is_none());
    app.on_mouse(mouse(MouseEventKind::Moved, x, y, KeyModifiers::CONTROL));
    assert_eq!(
        app.osc.hover,
        Some((0, "p2".into(), "https://example.com/x".into()))
    );
    let mut g = crate::screen::Grid::new(120, 40);
    crate::draw::compose(&app, &mut g);
    let (lx, ly) = at(&app, "p2", 4, 0);
    // Every run of the link (both rows) is underlined; the text around it is not.
    for dy in [0, 2] {
        for dx in 0..5 {
            let c = g.get(lx + dx, ly + dy).unwrap();
            assert!(c.style.attrs & vk_proto::render::attr::UNDERLINE != 0);
            assert_eq!(c.link.as_deref(), Some("https://example.com/x"));
        }
        assert_eq!(g.get(lx - 1, ly + dy).unwrap().style.attrs, 0);
        assert_eq!(g.get(lx + 5, ly + dy).unwrap().style.attrs, 0);
    }
    // Releasing the modifier clears the hover.
    app.on_mouse(mouse(MouseEventKind::Moved, x, y, KeyModifiers::NONE));
    assert!(app.osc.hover.is_none());
    // Ctrl+click opens (in tests the opener only records).
    app.on_mouse(mouse(
        MouseEventKind::Down(CtButton::Left),
        x,
        y,
        KeyModifiers::CONTROL,
    ));
    assert_eq!(app.nav.opened, vec!["https://example.com/x".to_string()]);
    // The plain-text linkifier is the fallback.
    let (px, py) = at(&app, "p2", 10, 1);
    app.on_mouse(mouse(
        MouseEventKind::Down(CtButton::Left),
        px,
        py,
        KeyModifiers::CONTROL,
    ));
    assert_eq!(app.nav.opened.last().unwrap(), "https://plain.example/y");
}

#[test]
fn non_web_links_are_copied_not_opened() {
    let (mut app, _rxs) = setup();
    activate(&mut app, 0, "p2", "file:///etc/passwd");
    assert!(app.nav.opened.is_empty());
    assert_eq!(
        app.clipboard_sink.as_ref().unwrap().last().unwrap().0,
        b"file:///etc/passwd"
    );
    assert!(last_toast(&app).contains("file: links are copied"));
    activate(&mut app, 0, "p2", "javascript:alert(1)");
    assert!(app.nav.opened.is_empty());
    activate(&mut app, 0, "p2", "https://a.b/\x1b]x");
    assert!(app.nav.opened.is_empty());
    assert!(last_toast(&app).contains("unsafe"));
}

#[test]
fn progress_bars_in_tab_and_sidebar() {
    let (mut app, _rxs) = setup();
    app.machines[0].model.pane_live = vec![PaneLive {
        pane: "p2".into(),
        progress: Some(Progress {
            state: ProgressState::Normal,
            pct: Some(50),
        }),
        ..Default::default()
    }];
    let mut g = crate::screen::Grid::new(120, 40);
    crate::draw::compose(&app, &mut g);
    let text = crate::tasks::grid_text(&g);
    assert!(text.contains("1 sh-p1 ▰▰▱▱"), "tab: {text}");
    let (bar, _) = progress_bar(
        &app,
        &Progress {
            state: ProgressState::Indeterminate,
            pct: None,
        },
    );
    assert_eq!(bar, "⋯");
    let (bar, st) = progress_bar(
        &app,
        &Progress {
            state: ProgressState::Error,
            pct: Some(100),
        },
    );
    assert_eq!(bar, "▰▰▰▰");
    assert_eq!(st.fg, app.theme.red);
}

#[test]
fn exit_badges_show_for_five_seconds() {
    let (mut app, _rxs) = setup();
    let mut b = PaneBuf::blank();
    b.lines = vec![row("output")];
    app.machines[0].panes.insert("p2".into(), b);
    app.machines[0].model.pane_live = vec![
        PaneLive {
            pane: "p2".into(),
            last_exit: Some(ExitMark { code: 3, at_ms: 1 }),
            ..Default::default()
        },
        PaneLive {
            pane: "p1".into(),
            last_exit: Some(ExitMark { code: 1, at_ms: 2 }),
            ..Default::default()
        },
    ];
    on_model(&mut app, 0);
    let now = std::time::Instant::now();
    assert_eq!(
        exit_badge(&app, 0, "p2", now).as_deref(),
        Some(" ✗ exit 3 ")
    );
    assert_eq!(exit_badge(&app, 0, "p2", now + EXIT_BADGE), None);
    let mut g = crate::screen::Grid::new(120, 40);
    crate::draw::compose(&app, &mut g);
    let text = crate::tasks::grid_text(&g);
    // The unfocused pane carries its badge; the focused one's is in the tab bar.
    assert!(text.contains("✗ exit 3"), "{text}");
    assert!(text.lines().next().unwrap().contains("✗ exit 1"), "{text}");
    let d = app.deadlines(now);
    assert!(format!("{d:?}").contains("exit_badge"), "{d:?}");
    // A repeated model with the same mark keeps the first-seen time.
    on_model(&mut app, 0);
    assert!(exit_badge(&app, 0, "p2", now + EXIT_BADGE).is_none());
}

/// Copy mode `[` / `]` / `o` over OSC 133 prompt rows (03 §8, §11.1).
#[test]
fn copy_mode_prompt_jumps_and_select_output() {
    let lines = vec![
        marked("$ make", mark::PROMPT),
        row("compiling"),
        row("done"),
        marked("$ ls -l \\", mark::PROMPT),
        marked("  more", mark::PROMPT_CONT),
        row("a.txt"),
        row("b.txt"),
        row(""),
        marked("$ ", mark::PROMPT),
    ];
    let mut cm = CopyMode::new(
        "p1",
        lines,
        40,
        Cursor {
            row: 8,
            col: 2,
            ..Default::default()
        },
    );
    let key = |cm: &mut CopyMode, c: char| cm.key(&KeyEvent::new(Key::Char(c), Mods::empty()));
    key(&mut cm, '[');
    assert_eq!(cm.cursor_row(), 3);
    key(&mut cm, '[');
    assert_eq!(cm.cursor_row(), 0);
    key(&mut cm, '[');
    assert_eq!(cm.cursor_row(), 0, "no earlier prompt");
    assert_eq!(cm.message(), Some("no earlier prompt loaded"));
    key(&mut cm, ']');
    assert_eq!(cm.cursor_row(), 3);
    // Select the output of `ls -l \ more`: continuation and trailing blank rows excluded.
    key(&mut cm, 'o');
    match key(&mut cm, 'y') {
        Outcome::Yank(t) => assert_eq!(t, "a.txt\nb.txt"),
        _ => panic!("yank"),
    }
    // From inside the first command's output.
    let mut cm2 = CopyMode::new(
        "p1",
        vec![
            marked("$ make", mark::PROMPT),
            row("compiling"),
            row("done"),
            marked("$ ", mark::PROMPT),
        ],
        40,
        Cursor {
            row: 2,
            ..Default::default()
        },
    );
    key(&mut cm2, 'o');
    match key(&mut cm2, 'y') {
        Outcome::Yank(t) => assert_eq!(t, "compiling\ndone"),
        _ => panic!("yank"),
    }
    // Without marks: a message, no selection.
    let mut cm3 = CopyMode::new("p1", vec![row("x")], 40, Cursor::default());
    key(&mut cm3, '[');
    assert!(cm3.message().unwrap().contains("OSC 133"));
    key(&mut cm3, 'o');
    assert!(cm3.message().unwrap().contains("OSC 133"));
}

#[test]
fn recovery_notice_is_transient_and_lost_output_is_persistent() {
    let (mut app, _rx) = setup();
    app.machines[0].model.panes[0].recovered = Some("ring_only".into());
    app.machines[0].model.panes[1].recovered = Some("lost".into());
    on_model(&mut app, 0);
    let now = Instant::now();
    assert_eq!(
        recovery_badge(&app, 0, "p1", "ring_only", now),
        Some(" reconnected after server restart ")
    );
    assert_eq!(
        recovery_badge(&app, 0, "p1", "ring_only", now + RECOVERY_NOTICE),
        None
    );
    let lost = |t| recovery_badge(&app, 0, "p2", "lost", t);
    assert_eq!(
        lost(now + RECOVERY_NOTICE * 10),
        Some(" earlier output lost ")
    );
    // A keypress in the focused pane (p1) dismisses its notice.
    dismiss_recovery(&mut app);
    assert_eq!(recovery_badge(&app, 0, "p1", "ring_only", now), None);
}
