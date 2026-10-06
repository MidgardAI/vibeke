//! Mouse drag selection, copy-on-select, PRIMARY and copy-mode mouse. Clipboard writes go to
//! `App::clipboard_sink` (never the host terminal or the real clipboard).

use super::*;
use crate::drafts::tests::{ch, fleet};
use crossterm::event::KeyModifiers;
use vk_proto::render::{Row, Span, Style};

fn row(s: &str) -> Row {
    Row {
        spans: vec![Span {
            style: Style::default(),
            text: s.into(),
            cols: s.chars().count() as u16,
        }],
        wrapped: false,
    }
}

fn mouse(kind: MouseEventKind, x: u16, y: u16, shift: bool) -> MouseEvent {
    MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers: if shift {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        },
    }
}

/// p1's content rect, with text in it.
fn setup() -> (App, Rect) {
    let (mut app, _rx) = fleet();
    let b = app.machines[0].panes.get_mut("p1").unwrap();
    b.cols = 40;
    b.rows = 3;
    b.lines = vec![row("hello world"), row("second line here"), row("third")];
    let r = app
        .pane_rects()
        .into_iter()
        .find(|(p, _)| p == "p1")
        .unwrap()
        .1;
    (app, r)
}

fn drag(app: &mut App, r: Rect, from: (u16, u16), to: (u16, u16), shift: bool) {
    let (x0, y0) = (r.x + from.0, r.y + from.1);
    let (x1, y1) = (r.x + to.0, r.y + to.1);
    app.on_mouse(mouse(MouseEventKind::Down(CtButton::Left), x0, y0, shift));
    app.on_mouse(mouse(MouseEventKind::Drag(CtButton::Left), x1, y1, shift));
    app.on_mouse(mouse(MouseEventKind::Up(CtButton::Left), x1, y1, shift));
}

fn sink(app: &App) -> Vec<(String, bool)> {
    app.clipboard_sink
        .as_ref()
        .unwrap()
        .iter()
        .map(|(d, p)| (String::from_utf8(d.clone()).unwrap(), *p))
        .collect()
}

#[test]
fn drag_selects_into_copy_mode_and_y_copies() {
    let (mut app, r) = setup();
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
    app.config.clipboard.copy_on_select = true;
    app.config.clipboard.primary_selection = true;
    drag(&mut app, r, (0, 1), (5, 1), false);
    assert!(matches!(app.mode, Mode::Normal), "copy mode closes");
    assert_eq!(
        sink(&app),
        vec![("second".to_string(), false), ("second".to_string(), true)]
    );
    // A plain click copies nothing and doesn't enter copy mode.
    let x = r.x + 2;
    app.on_mouse(mouse(MouseEventKind::Down(CtButton::Left), x, r.y, false));
    app.on_mouse(mouse(MouseEventKind::Up(CtButton::Left), x, r.y, false));
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(sink(&app).len(), 2);
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
fn mouse_mode_panes_need_shift_to_select() {
    let (mut app, r) = setup();
    app.machines[0].panes.get_mut("p1").unwrap().modes.mouse = true;
    drag(&mut app, r, (0, 0), (4, 0), false);
    assert!(matches!(app.mode, Mode::Normal), "the app gets the mouse");
    app.config.clipboard.copy_on_select = true;
    drag(&mut app, r, (0, 0), (4, 0), true);
    assert_eq!(sink(&app), vec![("hello".to_string(), false)]);
}

#[test]
fn in_copy_mode_press_restarts_drag_extends_click_clears_and_wheel_scrolls() {
    let (mut app, r) = setup();
    app.enter_copy(None);
    drag(&mut app, r, (0, 2), (2, 2), false);
    let Mode::Copy(cm) = &app.mode else { panic!() };
    assert_eq!(cm.selection_text().as_deref(), Some("thi"));
    // A click without a drag only moves the cursor.
    app.on_mouse(mouse(
        MouseEventKind::Down(CtButton::Left),
        r.x + 1,
        r.y,
        false,
    ));
    app.on_mouse(mouse(
        MouseEventKind::Up(CtButton::Left),
        r.x + 1,
        r.y,
        false,
    ));
    let Mode::Copy(cm) = &app.mode else { panic!() };
    assert_eq!(cm.selection_text(), None);
    // The wheel stays in copy mode (no re-entry).
    app.on_mouse(mouse(MouseEventKind::ScrollDown, r.x + 1, r.y, false));
    app.on_mouse(mouse(MouseEventKind::ScrollUp, r.x + 1, r.y, false));
    assert!(matches!(app.mode, Mode::Copy(_)));
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
