use super::*;
use crate::input::Input;

fn row(text: &str) -> Row {
    Row {
        spans: vec![Span {
            text: text.into(),
            cols: text.len() as u16,
            style: Style::default(),
        }],
        ..Row::default()
    }
}

fn ready(sync: bool) -> App {
    let (mut app, _rx) = crate::drafts::tests::fleet();
    app.config.ui.animate = false;
    app.config.ui.title_sync = false;
    app.machines[0].model.runs.clear();
    app.caps.sync_update = sync;
    let rect = app.tiled_rects()[0].1;
    app.on_frame(
        0,
        ServerFrame::PaneFull {
            pane: "p1".into(),
            epoch: 1,
            rev: 1,
            cols: rect.w,
            rows: rect.h,
            lines: vec![row(""); rect.h as usize],
            cursor: Cursor {
                visible: true,
                ..Cursor::default()
            },
            modes: PaneModes::default(),
            title: String::new(),
        },
    );
    assert!(!app.draw_frame(true).is_empty());
    assert!(app.draw_frame(true).is_empty());
    app
}

#[test]
fn native_key_and_ack_do_not_spend_the_echo_paint_budget() {
    for sync in [false, true] {
        let mut app = ready(sync);
        let baseline = Instant::now() - Duration::from_millis(20);
        let mut last_draw = baseline;
        app.on_input(Input::Key(KeyEvent::new(Key::Char('x'), Mods::empty())));
        if !app.draw_frame(true).is_empty() {
            last_draw = Instant::now();
        }
        app.on_frame(
            0,
            ServerFrame::InputAck {
                input_id: 1,
                status: AckStatus::Written,
            },
        );
        if !app.draw_frame(true).is_empty() {
            last_draw = Instant::now();
        }
        assert_eq!(
            last_draw, baseline,
            "key and successful ACK are semantic no-ops"
        );
        assert!(
            Duration::from_millis(8)
                .saturating_sub(last_draw.elapsed())
                .is_zero()
        );
        app.on_frame(
            0,
            ServerFrame::PaneDiff {
                pane: "p1".into(),
                epoch: 1,
                base_rev: 1,
                rev: 2,
                ops: vec![DiffOp::Rows(vec![(0, row("x"))])],
                cursor: Cursor {
                    col: 1,
                    visible: true,
                    ..Cursor::default()
                },
                modes: PaneModes::default(),
                title: String::new(),
            },
        );
        assert!(!app.draw_frame(true).is_empty(), "echo paints immediately");
        assert!(app.draw_frame(true).is_empty());
    }
}

#[test]
fn native_cursor_title_media_and_invalidations_always_write() {
    let mut app = ready(false);
    app.machines[0].panes.get_mut("p1").unwrap().cursor.col = 2;
    assert!(!app.draw_frame(true).is_empty(), "cursor-only move");
    assert!(app.draw_frame(true).is_empty());
    app.config.ui.title_sync = true;
    crate::plugins::on_title_event(&mut app, 0, &json!({"data": {"title": "changed title"}}));
    let out = app.draw_frame(true);
    assert!(String::from_utf8_lossy(&out).contains("\x1b]2;changed title"));
    assert!(app.draw_frame(true).is_empty());
    // Queued graphics must be written even when their placeholder cells did not change.
    app.browser.out.extend_from_slice(b"\x1b_Ga=d,d=A\x1b\\");
    let out = app.draw_frame(true);
    assert!(out.windows(3).any(|w| w == b"\x1b_G"));
    assert!(
        out.ends_with(b"\x1b[?25h"),
        "restore cursor after one-shot media"
    );
    assert!(app.draw_frame(true).is_empty());
    app.invalidate(crate::screen::Rect {
        x: 1,
        y: 1,
        w: 2,
        h: 1,
    });
    assert!(
        !app.draw_frame(true).is_empty(),
        "invalidated region is rewritten"
    );
    app.prev = Grid::new(0, 0);
    assert!(
        !app.draw_frame(true).is_empty(),
        "external-editor/full redraw"
    );
    app.size.0 -= 1;
    assert!(!app.draw_frame(true).is_empty(), "resize");
    app.toast("a visible notice");
    assert!(!app.draw_frame(true).is_empty());
    app.toasts.clear();
    assert!(!app.draw_frame(true).is_empty(), "notice disappears");
}

#[test]
fn host_scroll_candidate_allows_collapsed_rail_but_rejects_sidebar_splits_and_overlays() {
    let mut app = ready(true);
    assert!(app.scroll_candidate().is_none());
    app.host_scroll = true;
    app.sidebar = false;
    app.ux.sidebar.hide_rail = true;
    app.machines[0].panes.get_mut("p1").unwrap().pending_scroll = 1;
    assert!(app.scroll_candidate().is_none(), "split layout");
    app.machines[0].model.tabs[0].layout = LayoutNode::Leaf { pane: "p1".into() };
    assert!(app.scroll_candidate().is_some());
    app.sidebar = true;
    assert!(app.scroll_candidate().is_none());
    app.sidebar = false;
    app.ux.sidebar.hide_rail = false;
    let (rect, _) = app
        .scroll_candidate()
        .expect("collapsed rail is repaired by the diff");
    assert_eq!(rect.x, crate::sidebar::RAIL_W);
    assert_eq!(rect.w + crate::sidebar::RAIL_W, app.size.0);
    app.ux.sidebar.hide_rail = true;
    app.mode = Mode::Navigate { sel: 0 };
    assert!(app.scroll_candidate().is_none());
    app.mode = Mode::Normal;
    app.caps.sync_update = false;
    assert!(app.scroll_candidate().is_none());
}

#[test]
fn frame_local_sidebar_rows_match_fresh_rows_after_width_change() {
    let mut app = ready(false);
    app.sidebar_w = 8;
    let rows = crate::sidebar::fit_rows(&mut app);
    assert!(app.sidebar_w > 8);
    let mut cached = Grid::new(app.size.0, app.size.1);
    let mut fresh = Grid::new(app.size.0, app.size.1);
    let a = draw::compose_with_sidebar(&app, &mut cached, &rows);
    let b = draw::compose(&app, &mut fresh);
    assert_eq!(a, b);
    assert_eq!(cached, fresh);
}
