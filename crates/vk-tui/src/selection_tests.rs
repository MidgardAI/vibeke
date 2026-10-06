//! Mouse drag selection, copy-on-select, double/triple click, modifiers over mouse-reporting
//! apps, autoscroll, PRIMARY / middle click, the scrollback viewer and copy-mode mouse.
//! Clipboard writes go to `App::clipboard_sink` (never the host terminal or the real
//! clipboard).

use super::*;
use crate::drafts::tests::{ch, commands, fleet, only, reply};
use crate::screen::Grid;
use crossterm::event::KeyModifiers;
use serde_json::json;
use tokio::sync::mpsc::UnboundedReceiver;
use vk_proto::render::{ClientFrame, Row, Span, Style};

fn row(s: &str) -> Row {
    wrow(s, false)
}

fn wrow(s: &str, wrapped: bool) -> Row {
    Row {
        spans: vec![Span {
            style: Style::default(),
            text: s.into(),
            cols: s.chars().count() as u16,
        }],
        wrapped,
        ..Default::default()
    }
}

fn mouse_m(kind: MouseEventKind, x: u16, y: u16, modifiers: KeyModifiers) -> MouseEvent {
    MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers,
    }
}

fn mouse(kind: MouseEventKind, x: u16, y: u16, shift: bool) -> MouseEvent {
    mouse_m(
        kind,
        x,
        y,
        if shift {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        },
    )
}

type Rx = Vec<UnboundedReceiver<ClientFrame>>;

/// p1's content rect, with `lines` in it.
fn setup_with(lines: Vec<Row>) -> (App, Rect, Rx) {
    let (mut app, rx) = fleet();
    let b = app.machines[0].panes.get_mut("p1").unwrap();
    b.cols = 40;
    b.rows = lines.len() as u16;
    b.lines = lines;
    let r = app
        .pane_rects()
        .into_iter()
        .find(|(p, _)| p == "p1")
        .unwrap()
        .1;
    (app, r, rx)
}

fn setup() -> (App, Rect) {
    let (app, r, _rx) = setup_with(vec![
        row("hello world"),
        row("second line here"),
        row("third"),
    ]);
    (app, r)
}

fn drag_m(app: &mut App, r: Rect, from: (u16, u16), to: (u16, u16), m: KeyModifiers) {
    let (x0, y0) = (r.x + from.0, r.y + from.1);
    let (x1, y1) = (r.x + to.0, r.y + to.1);
    app.on_mouse(mouse_m(MouseEventKind::Down(CtButton::Left), x0, y0, m));
    app.on_mouse(mouse_m(MouseEventKind::Drag(CtButton::Left), x1, y1, m));
    app.on_mouse(mouse_m(MouseEventKind::Up(CtButton::Left), x1, y1, m));
}

fn drag(app: &mut App, r: Rect, from: (u16, u16), to: (u16, u16), shift: bool) {
    let m = if shift {
        KeyModifiers::SHIFT
    } else {
        KeyModifiers::NONE
    };
    drag_m(app, r, from, to, m);
}

fn click(app: &mut App, x: u16, y: u16) {
    app.on_mouse(mouse(MouseEventKind::Down(CtButton::Left), x, y, false));
    app.on_mouse(mouse(MouseEventKind::Up(CtButton::Left), x, y, false));
}

fn sink(app: &App) -> Vec<(String, bool)> {
    app.clipboard_sink
        .as_ref()
        .unwrap()
        .iter()
        .map(|(d, p)| (String::from_utf8(d.clone()).unwrap(), *p))
        .collect()
}

fn toasts(app: &App) -> Vec<String> {
    app.toasts.iter().map(|t| t.text.clone()).collect()
}

/// Mouse and paste frames sent to the server.
fn input_frames(rx: &mut UnboundedReceiver<ClientFrame>) -> Vec<String> {
    let mut v = Vec::new();
    while let Ok(f) = rx.try_recv() {
        match f {
            ClientFrame::Mouse { pane, event, .. } => v.push(format!(
                "mouse {pane} {:?} {:?} {},{}",
                event.kind, event.button, event.col, event.row
            )),
            ClientFrame::Paste { pane, text, .. } => v.push(format!("paste {pane} {text}")),
            _ => {}
        }
    }
    v
}

#[test]
fn copy_on_select_is_the_default_and_toasts() {
    assert!(vk_config::Config::default().clipboard.copy_on_select);
    let (mut app, r) = setup();
    drag(&mut app, r, (0, 1), (5, 1), false);
    assert!(matches!(app.mode, Mode::Normal), "copy mode closes");
    assert_eq!(sink(&app), vec![("second".to_string(), false)]);
    assert!(
        toasts(&app).contains(&"copied 6 chars".to_string()),
        "{:?}",
        toasts(&app)
    );
}

#[test]
fn drag_selects_into_copy_mode_and_y_copies() {
    let (mut app, r) = setup();
    app.config.clipboard.copy_on_select = false;
    drag(&mut app, r, (6, 0), (5, 1), false);
    let Mode::Copy(cm) = &app.mode else {
        panic!("copy mode expected")
    };
    assert!(cm.mouse);
    assert_eq!(cm.selection_text().as_deref(), Some("world\nsecond"));
    assert!(
        sink(&app).is_empty(),
        "copy_on_select is off: nothing copied yet"
    );
    app.on_key(ch('y'));
    assert_eq!(sink(&app), vec![("world\nsecond".to_string(), false)]);
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn copy_on_select_copies_on_release_with_primary() {
    let (mut app, r) = setup();
    app.config.clipboard.primary_selection = true;
    drag(&mut app, r, (0, 1), (5, 1), false);
    assert!(matches!(app.mode, Mode::Normal), "copy mode closes");
    assert_eq!(
        sink(&app),
        vec![("second".to_string(), false), ("second".to_string(), true)]
    );
    // A plain click copies nothing and doesn't enter copy mode.
    click(&mut app, r.x + 2, r.y + 2);
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(sink(&app).len(), 2);
}

#[test]
fn the_selection_is_highlighted_while_dragging() {
    let (mut app, r) = setup();
    let (x, y) = (r.x, r.y + 1);
    app.on_mouse(mouse(MouseEventKind::Down(CtButton::Left), x, y, false));
    app.on_mouse(mouse(MouseEventKind::Drag(CtButton::Left), x + 5, y, false));
    assert!(matches!(app.mode, Mode::Copy(_)), "button still down");
    let mut g = Grid::new(app.size.0, app.size.1);
    crate::draw::compose(&app, &mut g);
    let sel_bg = app.theme.selection;
    for cx in 0..5 {
        let c = g.get(x + cx, y).unwrap();
        assert_eq!(c.style.bg, sel_bg, "cell {cx} of `second` is highlighted");
    }
    // The selection end carries the copy-mode cursor; past it nothing is selected.
    assert_ne!(g.get(x + 7, y).unwrap().style.bg, sel_bg);
    assert_ne!(g.get(x, y + 1).unwrap().style.bg, sel_bg);
    app.on_mouse(mouse(MouseEventKind::Up(CtButton::Left), x + 5, y, false));
    assert_eq!(sink(&app), vec![("second".to_string(), false)]);
}

#[test]
fn double_click_selects_a_word_and_triple_click_the_line() {
    let (mut app, r, _rx) = setup_with(vec![
        row("open /usr/lib/x.rs:12 (now)"),
        wrow("a wrapped line tha", true),
        row("t goes on  "),
    ]);
    let (x, y) = (r.x + 8, r.y);
    click(&mut app, x, y);
    click(&mut app, x, y);
    assert!(matches!(app.mode, Mode::Normal), "copied on release");
    assert_eq!(sink(&app).last().unwrap().0, "/usr/lib/x.rs:12");
    // Brackets are not word characters.
    click(&mut app, r.x + 24, y);
    click(&mut app, r.x + 24, y);
    assert_eq!(sink(&app).last().unwrap().0, "now");
    // A word across a soft wrap is one word.
    click(&mut app, r.x + 16, r.y + 1);
    click(&mut app, r.x + 16, r.y + 1);
    assert_eq!(sink(&app).last().unwrap().0, "that");
    // Triple click: the whole logical line, wraps joined, trailing blanks trimmed.
    for _ in 0..3 {
        click(&mut app, r.x + 2, r.y + 2);
    }
    assert_eq!(sink(&app).last().unwrap().0, "a wrapped line that goes on");
}

#[test]
fn word_classes() {
    use crate::selection::word_class as w;
    assert_eq!((w("a"), w("/"), w(":"), w("-"), w("é")), (1, 1, 1, 1, 1));
    assert_eq!((w(" "), w(""), w("\t")), (0, 0, 0));
    assert_eq!((w("("), w("\""), w("│"), w(","), w("|")), (2, 2, 2, 2, 2));
}

#[test]
fn click_counting() {
    use crate::selection::{MULTI_CLICK, click_count};
    let mut st = crate::selection::State::default();
    let t = std::time::Instant::now();
    assert_eq!(click_count(&mut st, "p", 1, 1, t), 1);
    assert_eq!(click_count(&mut st, "p", 1, 1, t), 2);
    assert_eq!(click_count(&mut st, "p", 1, 1, t), 3);
    assert_eq!(
        click_count(&mut st, "p", 1, 1, t),
        1,
        "a fourth starts over"
    );
    assert_eq!(click_count(&mut st, "p", 2, 1, t), 1, "another cell");
    assert_eq!(
        click_count(&mut st, "p", 2, 1, t + MULTI_CLICK * 2),
        1,
        "too slow"
    );
}

#[test]
fn soft_wraps_join_and_trailing_blanks_trim_on_drag() {
    let (mut app, r, _rx) = setup_with(vec![wrow("hello wor", true), row("ld   "), row("x  ")]);
    drag(&mut app, r, (0, 0), (30, 1), false);
    assert_eq!(sink(&app).last().unwrap().0, "hello world");
    drag(&mut app, r, (0, 1), (30, 2), false);
    assert_eq!(sink(&app).last().unwrap().0, "ld\nx");
}

#[test]
fn mouse_mode_panes_take_shift_or_alt_drag() {
    let (mut app, r, mut rx) = setup_with(vec![row("hello world"), row("second")]);
    app.machines[0].panes.get_mut("p1").unwrap().modes.mouse = true;
    drag(&mut app, r, (0, 0), (4, 0), false);
    assert!(matches!(app.mode, Mode::Normal), "the app gets the mouse");
    assert!(sink(&app).is_empty());
    let f = input_frames(&mut rx[0]);
    assert_eq!(f.len(), 3, "press, drag, release forwarded: {f:?}");
    drag(&mut app, r, (0, 0), (4, 0), true);
    assert_eq!(sink(&app), vec![("hello".to_string(), false)]);
    // alt/option+drag selects too (Ghostty keeps shift+drag for itself).
    drag_m(&mut app, r, (6, 0), (10, 0), KeyModifiers::ALT);
    assert_eq!(sink(&app).last().unwrap().0, "world");
    assert!(
        input_frames(&mut rx[0]).is_empty(),
        "the app saw none of it"
    );
}

#[test]
fn always_mode_drags_select_and_clicks_reach_the_app() {
    let (mut app, r, mut rx) = setup_with(vec![row("hello world"), row("second")]);
    app.machines[0].panes.get_mut("p1").unwrap().modes.mouse = true;
    app.config.clipboard.mouse_select_in_apps = vk_config::MouseSelectInApps::Always;
    drag(&mut app, r, (0, 1), (5, 1), false);
    assert_eq!(sink(&app), vec![("second".to_string(), false)]);
    assert!(
        input_frames(&mut rx[0]).is_empty(),
        "a drag never reaches the app"
    );
    // A click goes to the app on release: press then release.
    click(&mut app, r.x + 3, r.y);
    assert_eq!(
        input_frames(&mut rx[0]),
        vec![
            "mouse p1 Press Left 3,0".to_string(),
            "mouse p1 Release Left 3,0".to_string()
        ]
    );
    // The wheel too.
    app.on_mouse(mouse(MouseEventKind::ScrollUp, r.x + 3, r.y, false));
    assert_eq!(
        input_frames(&mut rx[0]),
        vec!["mouse p1 Press WheelUp 3,0".to_string()]
    );
    assert!(matches!(app.mode, Mode::Normal));
}

fn numbered(n: usize) -> Vec<Row> {
    (0..n).map(|i| row(&format!("row {i}"))).collect()
}

#[test]
fn dragging_past_the_edge_autoscrolls_and_the_wheel_extends() {
    let (mut app, r, _rx) = setup_with(numbered(80));
    assert!(r.h < 80);
    let x = r.x;
    app.on_mouse(mouse(MouseEventKind::Down(CtButton::Left), x, r.y, false));
    app.on_mouse(mouse(
        MouseEventKind::Drag(CtButton::Left),
        x + 1,
        r.y + 1,
        false,
    ));
    // Below the pane: scrolls one row now, more on the timer while the pointer stays there.
    app.on_mouse(mouse(
        MouseEventKind::Drag(CtButton::Left),
        x + 30,
        r.y + r.h + 2,
        false,
    ));
    let d = app.deadlines(std::time::Instant::now());
    assert!(
        d.items().iter().any(|i| i.what == "selection_autoscroll"),
        "autoscroll armed"
    );
    let later = std::time::Instant::now() + crate::selection::AUTOSCROLL * 2;
    crate::selection::tick(&mut app, later);
    // The wheel during the drag scrolls three more and the selection follows.
    app.on_mouse(mouse(
        MouseEventKind::ScrollDown,
        x + 30,
        r.y + r.h - 1,
        false,
    ));
    // Release outside the pane still finishes the drag.
    app.on_mouse(mouse(
        MouseEventKind::Up(CtButton::Left),
        x + 30,
        r.y + r.h + 2,
        false,
    ));
    assert!(matches!(app.mode, Mode::Normal));
    let text = sink(&app).last().unwrap().0.clone();
    let last = text.lines().last().unwrap().to_string();
    let want = r.h as usize - 1 + 1 + 1 + 3;
    assert!(text.starts_with("row 0\nrow 1\n"), "{text}");
    assert_eq!(last, format!("row {want}"), "{text}");
    assert!(app.parity.selection.autoscroll.is_none());
}

#[test]
fn primary_also_set_by_copy_mode_yank() {
    let (mut app, _r) = setup();
    app.config.clipboard.primary_selection = true;
    app.enter_copy(None);
    app.on_key(ch('V'));
    app.on_key(ch('y'));
    assert_eq!(
        sink(&app),
        vec![
            ("hello world".to_string(), false),
            ("hello world".to_string(), true)
        ]
    );
}

#[test]
fn middle_click_pastes_the_last_copy_with_primary_selection() {
    let (mut app, r, mut rx) = setup_with(vec![row("hello world")]);
    drag(&mut app, r, (0, 0), (4, 0), false);
    input_frames(&mut rx[0]);
    let mid = |k| mouse(k, r.x + 1, r.y, false);
    app.on_mouse(mid(MouseEventKind::Down(CtButton::Middle)));
    app.on_mouse(mid(MouseEventKind::Up(CtButton::Middle)));
    assert!(
        input_frames(&mut rx[0]).is_empty(),
        "off without primary_selection"
    );
    app.config.clipboard.primary_selection = true;
    app.on_mouse(mid(MouseEventKind::Down(CtButton::Middle)));
    app.on_mouse(mid(MouseEventKind::Up(CtButton::Middle)));
    assert_eq!(input_frames(&mut rx[0]), vec!["paste p1 hello".to_string()]);
}

#[test]
fn iterm2_over_ssh_hints_after_the_first_copy_only() {
    let (mut app, r) = setup();
    app.copyout.env = crate::copyout::HostEnv {
        ssh: true,
        iterm2: true,
        tmux: false,
    };
    drag(&mut app, r, (0, 2), (4, 2), false);
    let hint = crate::copyout::ITERM2_HINT.to_string();
    assert!(toasts(&app).contains(&hint), "{:?}", toasts(&app));
    app.toasts.clear();
    drag(&mut app, r, (0, 0), (4, 0), false);
    assert_eq!(toasts(&app), vec!["copied 5 chars".to_string()]);
}

#[test]
fn in_copy_mode_press_restarts_drag_extends_click_clears_and_wheel_scrolls() {
    let (mut app, r) = setup();
    app.config.clipboard.copy_on_select = false;
    app.enter_copy(None);
    drag(&mut app, r, (0, 2), (2, 2), false);
    let Mode::Copy(cm) = &app.mode else { panic!() };
    assert_eq!(cm.selection_text().as_deref(), Some("thi"));
    // A click without a drag only moves the cursor.
    click(&mut app, r.x + 1, r.y);
    let Mode::Copy(cm) = &app.mode else { panic!() };
    assert_eq!(cm.selection_text(), None);
    // The wheel stays in copy mode (no re-entry).
    app.on_mouse(mouse(MouseEventKind::ScrollDown, r.x + 1, r.y, false));
    app.on_mouse(mouse(MouseEventKind::ScrollUp, r.x + 1, r.y, false));
    assert!(matches!(app.mode, Mode::Copy(_)));
    // With copy_on_select a drag in copy mode copies and closes it.
    app.config.clipboard.copy_on_select = true;
    drag(&mut app, r, (0, 1), (5, 1), false);
    assert_eq!(sink(&app), vec![("second".to_string(), false)]);
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn copy_mode_keys_follow_the_config() {
    let (mut app, _r) = setup();
    let mut c = vk_config::CopyMode {
        mode: vk_config::CopyModeKind::Emacs,
        ..Default::default()
    };
    c.overrides.insert("x".into(), "copy".into());
    app.copy_keys = std::sync::Arc::new(crate::copykeys::CopyKeys::from_config(&c));
    app.enter_copy(None);
    // vi `V` does nothing in emacs mode; alt+l selects the line, x (override) copies.
    app.on_key(ch('V'));
    app.on_key(vk_proto::input::KeyEvent::new(
        vk_proto::input::Key::Char('l'),
        vk_proto::input::Mods::ALT,
    ));
    app.on_key(ch('x'));
    assert_eq!(sink(&app), vec![("hello world".to_string(), false)]);
}

// ---- scrollback viewer -------------------------------------------------------------------------

fn viewer(rows: &[(&str, bool)]) -> (App, Rx) {
    let (mut app, mut rx) = fleet();
    app.action("edit_scrollback", None);
    let (req, _) = only(&commands(&mut rx[0]), "pane.read");
    let rows: Vec<_> = rows
        .iter()
        .enumerate()
        .map(|(n, (t, w))| json!({"n": n, "text": t, "wrapped": w}))
        .collect();
    reply(
        &mut app,
        0,
        req,
        json!({"rows": rows, "first": 0, "more_before": false}),
    );
    app.scrollback.as_mut().unwrap().top = 0;
    (app, rx)
}

/// Screen cell of logical line `li`, char `ci` (no wrapping in these tests).
fn vcell(app: &App, li: u16, ci: u16) -> (u16, u16) {
    let a = app.pane_area();
    (a.x + 1 + ci, a.y + 2 + li)
}

#[test]
fn scrollback_viewer_drag_double_and_triple_click_copy() {
    let (mut app, _rx) = viewer(&[
        ("first line   ", false),
        ("hello wor", true),
        ("ld again", false),
        ("path/to/file.rs here", false),
    ]);
    assert_eq!(app.scrollback.as_ref().unwrap().lines.len(), 3);
    let (x0, y0) = vcell(&app, 0, 6);
    let (x1, y1) = vcell(&app, 1, 4);
    app.on_mouse(mouse(MouseEventKind::Down(CtButton::Left), x0, y0, false));
    app.on_mouse(mouse(MouseEventKind::Drag(CtButton::Left), x1, y1, false));
    // Highlighted while dragging.
    let mut g = Grid::new(app.size.0, app.size.1);
    crate::draw::compose(&app, &mut g);
    assert_eq!(g.get(x0, y0).unwrap().style.bg, app.theme.selection);
    assert_ne!(g.get(x0 - 1, y0).unwrap().style.bg, app.theme.selection);
    app.on_mouse(mouse(MouseEventKind::Up(CtButton::Left), x1, y1, false));
    assert_eq!(sink(&app).last().unwrap().0, "line\nhello");
    assert!(
        matches!(app.mode, Mode::Popup(crate::app::Popup::Scrollback)),
        "viewer stays"
    );
    // Double click: a word; triple click: the logical line (soft wraps joined).
    let (x, y) = vcell(&app, 2, 3);
    click(&mut app, x, y);
    click(&mut app, x, y);
    assert_eq!(sink(&app).last().unwrap().0, "path/to/file.rs");
    let (x, y) = vcell(&app, 1, 2);
    for _ in 0..3 {
        click(&mut app, x, y);
    }
    assert_eq!(sink(&app).last().unwrap().0, "hello world again");
    // The wheel scrolls the viewer.
    app.on_mouse(mouse(MouseEventKind::ScrollDown, x, y, false));
    assert_eq!(app.scrollback.as_ref().unwrap().top, 2);
}

#[test]
fn scrollback_viewer_keeps_the_selection_for_y_without_copy_on_select() {
    let (mut app, _rx) = viewer(&[("alpha beta", false), ("gamma", false)]);
    app.config.clipboard.copy_on_select = false;
    let (x0, y0) = vcell(&app, 0, 6);
    let (x1, y1) = vcell(&app, 1, 1);
    app.on_mouse(mouse(MouseEventKind::Down(CtButton::Left), x0, y0, false));
    app.on_mouse(mouse(MouseEventKind::Drag(CtButton::Left), x1, y1, false));
    app.on_mouse(mouse(MouseEventKind::Up(CtButton::Left), x1, y1, false));
    assert!(sink(&app).is_empty());
    app.on_key(ch('y'));
    assert_eq!(sink(&app), vec![("beta\nga".to_string(), false)]);
    assert!(matches!(
        app.mode,
        Mode::Popup(crate::app::Popup::Scrollback)
    ));
}
