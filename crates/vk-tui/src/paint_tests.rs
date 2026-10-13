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

#[test]
fn native_pane_scroll_diff_uses_host_scroll_and_matches_the_fallback_frame() {
    for (tabs, letterbox) in [
        (vk_config::TabsPosition::Top, false),
        (vk_config::TabsPosition::Hidden, false),
        (vk_config::TabsPosition::Hidden, true),
    ] {
        let mut optimized = ready(true);
        let mut fallback = ready(true);
        let mut initial = Vec::new();
        for app in [&mut optimized, &mut fallback] {
            app.sidebar = false;
            app.config.ui.tabs.position = tabs;
            // This is the normal collapsed sidebar, including its two-column rail.
            app.ux.sidebar.hide_rail = false;
            app.machines[0].model.tabs[0].layout = LayoutNode::Leaf { pane: "p1".into() };
            app.machines[0].model.panes.retain(|p| p.id == "p1");
            app.machines[0].panes.remove("p2");
            let rect = app.tiled_rects()[0].1;
            assert_eq!(rect.x, crate::sidebar::RAIL_W);
            if tabs == vk_config::TabsPosition::Hidden {
                assert_eq!(rect.y, 0);
            }
            let pane_rows = rect.h - if letterbox { 2 } else { 0 };
            app.on_frame(
                0,
                ServerFrame::PaneFull {
                    pane: "p1".into(),
                    epoch: 2,
                    rev: 1,
                    cols: rect.w - if letterbox { 2 } else { 0 },
                    rows: pane_rows,
                    lines: (0..pane_rows)
                        .map(|y| row(&format!("log row {y:03} with terminal output")))
                        .collect(),
                    cursor: Cursor {
                        visible: true,
                        ..Cursor::default()
                    },
                    modes: PaneModes::default(),
                    title: String::new(),
                },
            );
            app.prev = Grid::new(0, 0);
            initial.push(app.draw_frame(true));
        }
        assert_eq!(optimized.prev, fallback.prev);
        optimized.host_scroll = true;
        let pane_rows = optimized.m().panes["p1"].rows;
        let frame = ServerFrame::PaneDiff {
            pane: "p1".into(),
            epoch: 2,
            base_rev: 1,
            rev: 2,
            ops: vec![
                DiffOp::ScrollUp { n: 1 },
                DiffOp::Rows(vec![(pane_rows - 1, row("new log row"))]),
            ],
            cursor: Cursor {
                row: pane_rows - 1,
                visible: true,
                ..Cursor::default()
            },
            modes: PaneModes::default(),
            title: String::new(),
        };
        optimized.on_frame(0, frame.clone());
        fallback.on_frame(0, frame);
        let scrolled = optimized.draw_frame(true);
        let repainted = fallback.draw_frame(true);
        assert_eq!(
            scrolled.windows(4).any(|w| w == b"\x1b[1S"),
            !letterbox,
            "letterboxed panes must preserve their fixed size hint using the fallback"
        );
        assert!(!repainted.windows(4).any(|w| w == b"\x1b[1S"));
        if !letterbox {
            assert!(scrolled.len() < repainted.len());
        }
        assert_eq!(optimized.prev, fallback.prev);
        let mut scrolled_host = vk_term::Engine::new(optimized.size.0, optimized.size.1, 0);
        let mut repainted_host = vk_term::Engine::new(fallback.size.0, fallback.size.1, 0);
        scrolled_host.feed(&initial[0], &mut Vec::new());
        repainted_host.feed(&initial[1], &mut Vec::new());
        scrolled_host.feed(&scrolled, &mut Vec::new());
        repainted_host.feed(&repainted, &mut Vec::new());
        assert_eq!(scrolled_host.visible_rows(), repainted_host.visible_rows());
        assert_eq!(optimized.m().panes["p1"].pending_scroll, 0);
        assert!(
            optimized.draw_frame(true).is_empty(),
            "the scroll is consumed once"
        );
    }
}

#[test]
fn native_browser_media_only_frames_write_once_on_kitty_and_iterm() {
    for kitty in [false, true] {
        let (mut app, mut rxs) = crate::drafts::tests::fleet();
        app.config.ui.animate = false;
        app.config.ui.title_sync = false;
        app.machines[0].model.runs.clear();
        app.caps.kitty_graphics = kitty;
        app.caps.iterm2_images = !kitty;
        app.caps.cell_w = 4;
        app.caps.cell_h = 4;
        app.caps.truecolor = true;
        app.imgview.tmux = false;
        app.machines[0].model.panes[1].browser = Some(serde_json::from_value(json!({
            "url": "https://example.com/", "machine": "", "task": null, "preview": null,
            "source_pane": "p1", "history": ["https://example.com/"], "history_index": 0, "title": ""
        })).unwrap());
        app.draw_frame(true);
        let frame = |seq, reset, shade| MediaFrame {
            pane: "p2".into(),
            seq,
            reset,
            width: 4,
            height: 4,
            cell_w: 4,
            cell_h: 4,
            tile_cols: 1,
            tile_rows: 1,
            grid_cols: 1,
            grid_rows: 1,
            tiles: vec![MediaTile {
                index: 0,
                col: 0,
                row: 0,
                cols: 1,
                rows: 1,
                w: 4,
                h: 4,
                data: TileData::Rgba(vec![shade; 4 * 4 * 4]),
            }],
        };
        crate::browser::on_media(&mut app, 0, frame(1, true, 100));
        let first = app.draw_frame(true);
        let marker: &[u8] = if kitty { b"\x1b_G" } else { b"\x1b]1337;" };
        assert!(
            first.windows(marker.len()).any(|w| w == marker),
            "first image kitty={kitty}"
        );
        let before = app.prev.clone();
        app.browser.panes.get_mut("p2").unwrap().last_inline = None;
        crate::browser::on_media(&mut app, 0, frame(2, false, 200));
        let media_only = app.draw_frame(true);
        assert_eq!(
            before, app.prev,
            "media changed without changing any composed cells"
        );
        assert!(
            media_only.windows(marker.len()).any(|w| w == marker),
            "media-only output kitty={kitty}"
        );
        assert!(
            media_only.ends_with(b"\x1b[?25l"),
            "restore hidden cursor after media"
        );
        assert!(
            app.draw_frame(true).is_empty(),
            "no duplicate media write kitty={kitty}"
        );
        let mut acks = 0;
        while let Ok(frame) = rxs[0].try_recv() {
            if matches!(frame, ClientFrame::MediaAck { ref pane, seq: 2 } if pane == "p2") {
                acks += 1;
            }
        }
        assert_eq!(acks, 1, "the media ACK observer still runs exactly once");
    }
}

#[test]
fn native_gallery_images_repaint_on_resize_and_disappear_on_close() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sample.png");
    let png = vk_browser::frame::encode_png(4, 4, &[255; 4 * 4 * 4], true).unwrap();
    std::fs::write(&path, png).unwrap();
    for kitty in [false, true] {
        let mut app = ready(false);
        app.caps.kitty_graphics = kitty;
        app.caps.iterm2_images = !kitty;
        app.caps.cell_w = 10;
        app.caps.cell_h = 20;
        app.imgview.tmux = false;
        app.imgview.gfx_dir = Some(dir.path().join("gfx"));
        crate::gallery::open(
            &mut app,
            0,
            crate::gallery::Scope::All,
            crate::gallery::View::Full,
            false,
        );
        let shot = crate::gallery::Shot::parse(&json!({
            "id": "S1", "handle": "s1", "path_on_machine": path.to_string_lossy(), "exists": true,
            "environment": {"kind": "agent"}, "created_at_ms": 1, "width": 4, "height": 4
        }))
        .unwrap();
        let gallery = app.gallery.view.as_mut().unwrap();
        gallery.shots = vec![shot];
        gallery.loading = false;
        crate::gallery::ensure_local_image(&mut app);
        let out = app.draw_frame(true);
        let marker: &[u8] = if kitty { b"\x1b_G" } else { b"\x1b]1337;" };
        assert!(
            out.windows(marker.len()).any(|w| w == marker),
            "gallery image kitty={kitty}"
        );
        assert!(
            app.draw_frame(true).is_empty(),
            "unchanged gallery is a native no-op"
        );
        app.size.0 -= 1;
        let resized = app.draw_frame(true);
        assert!(!resized.is_empty());
        if !kitty {
            assert!(
                resized.windows(marker.len()).any(|w| w == marker),
                "resize re-emits inline image"
            );
        }
        app.mode = Mode::Normal;
        app.gallery.view = None;
        assert!(
            !app.draw_frame(true).is_empty(),
            "closing removes the displayed image"
        );
        assert!(app.draw_frame(true).is_empty());
    }
}
