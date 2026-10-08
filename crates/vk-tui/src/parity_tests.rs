//! M4 parity surfaces: groups, floats, status bar, search fallback, appearance, notifications,
//! palette entries and layouts. Fake machines only; nothing touches the host terminal.

use crate::app::{App, Mode, PaneBuf, Popup, PromptKind, test_app, test_interaction, test_run};
use crate::screen::Grid;
use crossterm::event::{KeyModifiers, MouseButton as CtButton, MouseEvent, MouseEventKind};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use vk_proto::input::{Key, KeyEvent, Mods, NamedKey};
use vk_proto::model::*;
use vk_proto::render::{ClientFrame, Cursor, Row, ServerFrame, Span, Style};

type Rx = mpsc::UnboundedReceiver<ClientFrame>;

fn frames(rx: &mut Rx) -> Vec<ClientFrame> {
    let mut v = Vec::new();
    while let Ok(f) = rx.try_recv() {
        v.push(f);
    }
    v
}

fn commands(rx: &mut Rx) -> Vec<(u64, Value)> {
    frames(rx)
        .into_iter()
        .filter_map(|f| match f {
            ClientFrame::Command { req, json } => Some((req, serde_json::from_str(&json).unwrap())),
            _ => None,
        })
        .collect()
}

fn reply(app: &mut App, mi: usize, req: u64, result: Value) {
    let json = json!({"jsonrpc": "2.0", "id": req, "result": result}).to_string();
    app.on_frame(mi, ServerFrame::CommandResult { req, json });
}

fn key(c: char) -> KeyEvent {
    KeyEvent::ch(c)
}

fn named(k: NamedKey) -> KeyEvent {
    KeyEvent::named(k)
}

fn text(app: &App) -> String {
    let mut g = Grid::new(app.size.0, app.size.1);
    crate::draw::compose(app, &mut g);
    crate::tasks::grid_text(&g)
}

fn row(s: &str) -> Row {
    Row {
        spans: vec![Span {
            style: Style::default(),
            text: s.into(),
            cols: s.chars().count() as u16,
        }],
        wrapped: false,
        ..Default::default()
    }
}

fn mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers: KeyModifiers::empty(),
    }
}

fn ws(id: &str, name: &str) -> Workspace {
    serde_json::from_value(json!({
        "id": id, "handle": format!("w-{id}"), "name": name, "auto_name": name,
        "root_path": "/tmp", "task": null, "order": 1.0, "branch": null
    }))
    .unwrap()
}

fn pane(id: &str, tab: &str, ws: &str) -> Pane {
    serde_json::from_value(json!({
        "id": id, "handle": format!("w1:{id}"), "tab": tab, "workspace": ws, "title": null,
        "auto_title": format!("sh-{id}"), "cwd": null, "cols": 80, "rows": 24, "child_pid": null,
        "fg_cmdline": [], "exited": false, "exit_code": null, "unread": false,
        "marked_unread": false, "pinned": false, "created_by": "user", "recovered": null
    }))
    .unwrap()
}

fn tab(id: &str, ws: &str, layout: Value, floating: Vec<FloatingPane>) -> Tab {
    let mut t: Tab = serde_json::from_value(json!({
        "id": id, "handle": format!("w1:{id}"), "workspace": ws, "title": null, "number": 1,
        "layout": layout, "focused_pane": null, "zoomed_pane": null, "order": 1.0
    }))
    .unwrap();
    t.floating = floating;
    t
}

fn leaf(p: &str) -> Value {
    json!({"Leaf": {"pane": p}})
}

/// One machine: workspace w1 with tab t1 (tiled p1, floating p2 when `float`), and workspaces
/// w2/w3 with one pane each; focus on p1.
fn setup(float: bool) -> (App, Vec<Rx>) {
    let (mut app, rxs) = test_app(1);
    let m = &mut app.machines[0];
    m.model.workspaces = vec![ws("w1", "api"), ws("w2", "web"), ws("w3", "docs")];
    let floating = if float {
        vec![FloatingPane::centred("p2", 1)]
    } else {
        vec![]
    };
    m.model.tabs = vec![
        tab("t1", "w1", leaf("p1"), floating),
        tab("t2", "w2", leaf("p3"), vec![]),
        tab("t3", "w3", leaf("p4"), vec![]),
    ];
    m.model.panes = vec![
        pane("p1", "t1", "w1"),
        pane("p2", "t1", "w1"),
        pane("p3", "t2", "w2"),
        pane("p4", "t3", "w3"),
    ];
    m.focus = ClientFocus {
        workspace: Some("w1".into()),
        tab: Some("t1".into()),
        pane: Some("p1".into()),
    };
    let mut buf = PaneBuf::blank();
    buf.lines = vec![row("tiled pane text"); 5];
    m.panes.insert("p1".into(), buf);
    let mut buf = PaneBuf::blank();
    buf.lines = vec![row("float content")];
    m.panes.insert("p2".into(), buf);
    (app, rxs)
}

// ---- groups ---------------------------------------------------------------------------------

fn with_groups(app: &mut App, collapsed: bool) {
    let m = &mut app.machines[0];
    m.model.groups = vec![Group {
        id: "g1".into(),
        handle: "g1".into(),
        name: "clients".into(),
        parent: None,
        collapsed,
        order: 1.0,
        workspaces: vec!["w2".into(), "w3".into()],
    }];
    let mut r = test_run("r1", "p3", "claude");
    r.execution.value = Execution::Working;
    m.model.runs.push(r);
    m.model.runs.push(test_run("r2", "p4", "codex"));
    m.model
        .interactions
        .push(test_interaction("i1", "p4", "Bash rm", 1));
    m.model.interactions[0].run = "r2".into();
}

#[test]
fn groups_draw_with_aggregate_counts_and_collapse() {
    let (mut app, _rx) = setup(false);
    with_groups(&mut app, false);
    let row = crate::groups::group_row_text(&app, 0, "g1");
    assert!(row.contains("▾ clients"), "{row}");
    assert!(row.contains("⚠1") && row.contains("●1"), "{row}");
    let t = text(&app);
    let lines: Vec<&str> = t.lines().collect();
    let gi = lines.iter().position(|l| l.contains("▾ clients")).unwrap();
    let webi = lines.iter().position(|l| l.contains("web")).unwrap();
    let apii = lines.iter().position(|l| l.contains("api")).unwrap();
    // Groups come first, members nested under them, ungrouped workspaces after.
    assert!(gi < webi && webi < apii, "{t}");
    assert!(
        lines[webi].contains("    web"),
        "member indented: {}",
        lines[webi]
    );
    with_groups(&mut app, true);
    app.machines[0].model.groups[0].collapsed = true;
    let t = text(&app);
    assert!(t.contains("▸ clients"), "{t}");
    assert!(!t.contains("web"), "collapsed members hidden: {t}");
}

#[test]
fn groups_navigate_collapse_move_and_create() {
    let (mut app, mut rx) = setup(false);
    with_groups(&mut app, false);
    // The group row is the first selectable row (no needs-you section in this order? it is:
    // r2 has an open interaction) — find it.
    app.config.ui.sidebar.attention_section = false;
    let targets = crate::draw::sidebar_targets(&app);
    let gsel = targets.iter().position(|(_, id)| id == "g1").unwrap();
    app.mode = Mode::Navigate { sel: gsel };
    app.on_key(named(NamedKey::Enter));
    let c = commands(&mut rx[0]);
    assert_eq!(c[0].1["method"], "group.collapse");
    assert_eq!(c[0].1["params"], json!({"group": "g1", "collapsed": true}));
    assert!(app.machines[0].model.groups[0].collapsed, "drawn at once");
    assert!(matches!(app.mode, Mode::Navigate { .. }));
    // Pane keys on a group row never reach the pane actions.
    app.on_key(key('x'));
    assert!(matches!(app.mode, Mode::Navigate { .. }));
    assert!(commands(&mut rx[0]).is_empty());
    // `m` on the `api` workspace row → picker → move into clients.
    app.machines[0].model.groups[0].collapsed = false;
    let targets = crate::draw::sidebar_targets(&app);
    let api = targets.iter().position(|(_, p)| p == "p1").unwrap();
    app.mode = Mode::Navigate { sel: api };
    app.on_key(key('m'));
    assert!(
        matches!(app.mode, Mode::Popup(Popup::GroupPick { .. })),
        "{:?}",
        app.mode
    );
    let t = text(&app);
    assert!(t.contains("(no group)") && t.contains("+ new group"), "{t}");
    app.on_key(key('j'));
    app.on_key(named(NamedKey::Enter));
    let c = commands(&mut rx[0]);
    assert_eq!(c[0].1["method"], "group.add");
    assert_eq!(c[0].1["params"], json!({"group": "g1", "workspace": "w1"}));
    // Palette: new group with the focused workspace moved in.
    app.action("group_new", None);
    let Mode::Prompt(p) = &app.mode else {
        panic!("prompt")
    };
    assert!(matches!(&p.kind, PromptKind::GroupNew { ws: Some(w), .. } if w == "w1"));
    for c in "infra".chars() {
        app.on_key(key(c));
    }
    app.on_key(named(NamedKey::Enter));
    let c = commands(&mut rx[0]);
    assert_eq!(c[0].1["method"], "group.create");
    reply(
        &mut app,
        0,
        c[0].0,
        json!({"group": {"id": "g2", "name": "infra"}}),
    );
    let c = commands(&mut rx[0]);
    assert_eq!(c[0].1["method"], "group.add");
    assert_eq!(c[0].1["params"]["group"], "g2");
}

#[test]
fn groups_mouse_click_toggles_and_drag_moves() {
    let (mut app, mut rx) = setup(false);
    with_groups(&mut app, false);
    app.config.ui.sidebar.attention_section = false;
    let rows = crate::draw::sidebar_rows(&app);
    let gy = rows.iter().position(|r| r.group.is_some()).unwrap() as u16 + 1;
    let api_y = rows
        .iter()
        .position(|r| r.target.as_ref().is_some_and(|t| t.1 == "p1"))
        .unwrap() as u16
        + 1;
    // A click (press and release on the row) toggles; the press alone may start a drag.
    app.on_mouse(mouse(MouseEventKind::Down(CtButton::Left), 2, gy));
    assert!(commands(&mut rx[0]).is_empty());
    app.on_mouse(mouse(MouseEventKind::Up(CtButton::Left), 2, gy));
    let c = commands(&mut rx[0]);
    assert_eq!(c[0].1["method"], "group.collapse");
    app.machines[0].model.groups[0].collapsed = false;
    // Drag `api` onto the group row.
    app.on_mouse(mouse(MouseEventKind::Down(CtButton::Left), 2, api_y));
    frames(&mut rx[0]);
    app.on_mouse(mouse(MouseEventKind::Drag(CtButton::Left), 2, gy));
    app.on_mouse(mouse(MouseEventKind::Up(CtButton::Left), 2, gy));
    let c = commands(&mut rx[0]);
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(c[0].1["method"], "group.add");
    assert_eq!(c[0].1["params"], json!({"group": "g1", "workspace": "w1"}));
}

/// Three top-level groups a, b, c (orders 1..3) plus `a`'s child `a1`.
fn three_groups(app: &mut App) {
    let g = |id: &str, parent: Option<&str>, order: f64, ws: &[&str]| Group {
        id: id.into(),
        handle: id.into(),
        name: format!("grp-{id}"),
        parent: parent.map(str::to_string),
        collapsed: false,
        order,
        workspaces: ws.iter().map(|w| w.to_string()).collect(),
    };
    app.machines[0].model.groups = vec![
        g("a", None, 1.0, &["w1"]),
        g("b", None, 2.0, &["w2"]),
        g("c", None, 3.0, &["w3"]),
        g("a1", Some("a"), 1.0, &[]),
    ];
    app.config.ui.sidebar.attention_section = false;
}

fn group_y(app: &App, id: &str) -> u16 {
    crate::draw::sidebar_rows(app)
        .iter()
        .position(|r| r.group.as_ref().is_some_and(|g| g.1 == id))
        .unwrap() as u16
        + 1
}

#[test]
fn group_drop_position_orders_among_siblings_and_refuses_cycles() {
    let (mut app, _rx) = setup(false);
    three_groups(&mut app);
    let gs = &app.machines[0].model.groups;
    // Down onto c: after it; up onto a: before it.
    assert_eq!(
        crate::groups::drop_position(gs, "a", "c", true),
        Some((None, 2))
    );
    assert_eq!(
        crate::groups::drop_position(gs, "c", "a", false),
        Some((None, 0))
    );
    // Onto a child group: becomes its sibling (under the same parent).
    assert_eq!(
        crate::groups::drop_position(gs, "c", "a1", true),
        Some((Some("a".into()), 1))
    );
    // Never into itself or below itself.
    assert_eq!(crate::groups::drop_position(gs, "a", "a", true), None);
    assert_eq!(crate::groups::drop_position(gs, "a", "a1", true), None);
}

#[test]
fn group_drag_reorders_with_group_move_and_shows_the_moving_row() {
    let (mut app, mut rx) = setup(false);
    three_groups(&mut app);
    let (ay, cy) = (group_y(&app, "a"), group_y(&app, "c"));
    app.on_mouse(mouse(MouseEventKind::Down(CtButton::Left), 2, ay));
    app.on_mouse(mouse(MouseEventKind::Drag(CtButton::Left), 2, cy));
    // Drawn while dragging.
    let t = text(&app);
    let line = t.lines().find(|l| l.contains("grp-a ")).unwrap();
    assert!(line.contains("⇅ moving"), "{t}");
    assert!(commands(&mut rx[0]).is_empty(), "nothing sent mid-drag");
    app.on_mouse(mouse(MouseEventKind::Up(CtButton::Left), 2, cy));
    let c = commands(&mut rx[0]);
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(c[0].1["method"], "group.move");
    assert_eq!(
        c[0].1["params"],
        json!({"group": "a", "parent": null, "index": 2})
    );
    assert!(app.parity.groups.group_drag.is_none());
    assert!(!text(&app).contains("⇅ moving"));
    // Dropping a group into its own child is refused locally.
    let (ay, a1y) = (group_y(&app, "a"), group_y(&app, "a1"));
    app.on_mouse(mouse(MouseEventKind::Down(CtButton::Left), 2, ay));
    app.on_mouse(mouse(MouseEventKind::Drag(CtButton::Left), 2, a1y));
    app.on_mouse(mouse(MouseEventKind::Up(CtButton::Left), 2, a1y));
    assert!(commands(&mut rx[0]).is_empty());
    assert!(text(&app).contains("can't move into itself"));
}

// ---- floats ---------------------------------------------------------------------------------

#[test]
fn float_geometry_and_view_hint() {
    let (mut app, mut rx) = setup(true);
    let area = app.pane_area();
    let vis = crate::floats::visible(&app);
    assert_eq!(vis.len(), 1);
    let f = &vis[0];
    // 70% × 70% centred, content inside the frame.
    assert_eq!(f.outer.w, (area.w as f32 * 0.7).round() as u16);
    assert_eq!(f.outer.h, (area.h as f32 * 0.7).round() as u16);
    assert_eq!(f.inner.w, f.outer.w - 2);
    assert_eq!(f.inner.x, f.outer.x + 1);
    // Floats first in hit order; ViewHint reports their content size.
    let rects = app.pane_rects();
    assert_eq!(rects[0].0, "p2");
    assert_eq!(rects[1].0, "p1");
    frames(&mut rx[0]);
    app.send_view_hints(true);
    let hint = frames(&mut rx[0])
        .into_iter()
        .find_map(|f| match f {
            ClientFrame::ViewHint { panes, .. } => Some(panes),
            _ => None,
        })
        .unwrap();
    let p2 = hint.iter().find(|p| p.pane == "p2").unwrap();
    assert_eq!((p2.cols, p2.rows), (f.inner.w, f.inner.h));
    // Clamped into the area and to a minimum size.
    let tiny = FloatingPane {
        pane: "x".into(),
        x: 99.0,
        y: 99.0,
        w: 1.0,
        h: 1.0,
        z: 1,
    };
    let o = crate::floats::outer_rect(&tiny, area).unwrap();
    assert!(o.w >= crate::floats::MIN_W && o.h >= crate::floats::MIN_H);
    assert!(o.x + o.w <= area.x + area.w && o.y + o.h <= area.y + area.h);
    // Hidden floats: not drawn, not hinted.
    app.machines[0].model.tabs[0].floats_hidden = true;
    assert!(crate::floats::visible(&app).is_empty());
    assert_eq!(app.pane_rects().len(), 1);
}

#[test]
fn floats_draw_over_the_tiling_in_z_order() {
    let (mut app, _rx) = setup(true);
    let t = text(&app);
    assert!(t.contains("float content"), "{t}");
    assert!(t.contains("╭") && t.contains("◢"), "{t}");
    assert!(t.contains("sh-p2"), "title: {t}");
    let f = crate::floats::visible(&app)[0].clone();
    // Inside the float the tiled pane's text is gone (rows under the frame were cleared).
    let mut g = Grid::new(app.size.0, app.size.1);
    crate::draw::compose(&app, &mut g);
    let y = f.inner.y + 2;
    let line: String = g.row(y)[f.inner.x as usize..(f.inner.x + f.inner.w) as usize]
        .iter()
        .map(|c| c.text.as_str().to_string())
        .collect();
    assert!(!line.contains("tiled"), "{line}");
    // A second float with a higher z draws on top of the first.
    app.machines[0].model.panes.push(pane("p5", "t1", "w1"));
    let mut top = FloatingPane::centred("p5", 7);
    top.x = 20.0;
    top.y = 20.0;
    app.machines[0].model.tabs[0].floating.push(top);
    let vis = crate::floats::visible(&app);
    assert_eq!(vis.last().unwrap().pane, "p5");
    assert_eq!(app.pane_rects()[0].0, "p5");
    // Zoomed tab: floats hidden.
    app.machines[0].model.tabs[0].zoomed_pane = Some("p1".into());
    assert!(crate::floats::visible(&app).is_empty());
}

#[test]
fn float_mouse_move_resize_and_raise() {
    let (mut app, mut rx) = setup(true);
    let f = crate::floats::visible(&app)[0].clone();
    let area = app.pane_area();
    // Drag the title row 10 cells right and 2 down.
    app.on_mouse(mouse(
        MouseEventKind::Down(CtButton::Left),
        f.outer.x + 4,
        f.outer.y,
    ));
    assert_eq!(app.focused_pane().as_deref(), Some("p2"));
    frames(&mut rx[0]);
    app.on_mouse(mouse(
        MouseEventKind::Drag(CtButton::Left),
        f.outer.x + 14,
        f.outer.y + 2,
    ));
    assert!(
        commands(&mut rx[0]).is_empty(),
        "nothing sent while dragging"
    );
    let moved = app.machines[0].model.tabs[0].floating[0].clone();
    assert!(
        (moved.x - (15.0 + 1000.0 / area.w as f32)).abs() < 0.01,
        "{moved:?}"
    );
    app.on_mouse(mouse(
        MouseEventKind::Up(CtButton::Left),
        f.outer.x + 14,
        f.outer.y + 2,
    ));
    let c = commands(&mut rx[0]);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].1["method"], "pane.float");
    assert_eq!(c[0].1["params"]["pane"], "p2");
    assert!((c[0].1["params"]["rect"]["x"].as_f64().unwrap() - moved.x as f64).abs() < 0.01);
    // Corner drag resizes.
    let f = crate::floats::visible(&app)[0].clone();
    let (cx, cy) = (f.outer.x + f.outer.w - 1, f.outer.y + f.outer.h - 1);
    app.on_mouse(mouse(MouseEventKind::Down(CtButton::Left), cx, cy));
    app.on_mouse(mouse(MouseEventKind::Up(CtButton::Left), cx - 5, cy - 3));
    let c = commands(&mut rx[0]);
    let w = c.last().unwrap().1["params"]["rect"]["w"].as_f64().unwrap();
    assert!(w < 70.0, "{c:?}");
    // A click in the content of a lower float raises it (pane.float {pane}, no rect).
    app.machines[0].model.panes.push(pane("p5", "t1", "w1"));
    let mut top = FloatingPane::centred("p5", 9);
    top.x = 0.0;
    top.y = 0.0;
    top.w = 20.0;
    top.h = 20.0;
    app.machines[0].model.tabs[0].floating.push(top);
    let f2 = crate::floats::visible(&app)
        .into_iter()
        .find(|v| v.pane == "p2")
        .unwrap();
    let (x, y) = (f2.inner.x + f2.inner.w - 2, f2.inner.y + f2.inner.h - 2);
    app.on_mouse(mouse(MouseEventKind::Down(CtButton::Left), x, y));
    let c = commands(&mut rx[0]);
    assert!(
        c.iter()
            .any(|(_, v)| v["method"] == "pane.float" && v["params"] == json!({"pane": "p2"})),
        "{c:?}"
    );
    assert_eq!(crate::floats::visible(&app).last().unwrap().pane, "p2");
}

#[test]
fn float_keys_and_actions() {
    let (mut app, mut rx) = setup(true);
    app.action("float_new", None);
    let c = commands(&mut rx[0]);
    assert_eq!(c[0].1["method"], "pane.float");
    assert_eq!(c[0].1["params"], json!({"tab": "t1", "focus": true}));
    // Resize mode on the focused float: l widens, m then l moves.
    app.machines[0].focus.pane = Some("p2".into());
    app.action("resize_mode", None);
    app.on_key(key('l'));
    let c = commands(&mut rx[0]);
    assert_eq!(c[0].1["params"]["rect"]["w"], json!(72.0));
    app.on_key(key('m'));
    app.on_key(key('l'));
    let c = commands(&mut rx[0]);
    assert_eq!(c[0].1["params"]["rect"]["x"], json!(17.0));
    assert!(text(&app).contains("· move"));
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(app.mode, Mode::Normal));
    // Embed back; hide floats moves focus to the tiling.
    app.action("float_pane", None);
    let c = commands(&mut rx[0]);
    assert_eq!(c[0].1["method"], "pane.embed");
    app.action("toggle_floats", None);
    let c = commands(&mut rx[0]);
    assert_eq!(c[0].1["method"], "tab.floats");
    assert_eq!(c[0].1["params"], json!({"tab": "t1", "visible": false}));
    assert_eq!(app.focused_pane().as_deref(), Some("p1"));
    // Float a tiled pane.
    app.action("float_pane", None);
    let c = commands(&mut rx[0]);
    assert_eq!(c[0].1["method"], "pane.float");
    assert_eq!(c[0].1["params"], json!({"pane": "p1", "focus": true}));
}

// ---- status bar -----------------------------------------------------------------------------

#[test]
fn status_bar_draws_segments_and_takes_a_row() {
    let (mut app, mut rx) = setup(false);
    let before = app.pane_area();
    app.config.ui.status_bar.enabled = true;
    app.config.ui.status_bar.left = vec!["machine".into(), "workspace".into(), "mode".into()];
    app.config.ui.status_bar.center = vec!["attention".into()];
    app.config.ui.status_bar.right = vec!["agents_summary".into(), "cpu".into(), "clock".into()];
    assert_eq!(app.pane_area().h, before.h - 1);
    assert_eq!(app.pane_area().y, before.y);
    // Before any reply: computed from the model.
    let t = text(&app);
    let last = t.lines().nth(39).unwrap().to_string();
    assert!(last.contains("m0 ●") && last.contains("api"), "{last}");
    assert!(last.contains("normal"), "{last}");
    // The refresh asks status.segments once, then not again until the model changes.
    app.on_frame(
        0,
        ServerFrame::Pong {
            nonce: 0,
            server_ts_ms: 0,
        },
    );
    crate::statusbar::tick(&mut app);
    let c = commands(&mut rx[0]);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].1["method"], "status.segments");
    crate::statusbar::tick(&mut app);
    assert!(commands(&mut rx[0]).is_empty());
    reply(
        &mut app,
        0,
        c[0].0,
        json!({"segments": {
            "machine": {"label": "devbox"}, "workspace": {"name": "api"},
            "attention": {"count": 2}, "agents_summary": {"working": 3, "done": 1, "needs_input": 2, "error": 0},
            "cpu": {"load": [1.5, 1.0, 0.5]}, "sync_input": {"enabled": false}}}),
    );
    let t = text(&app);
    let last = t.lines().nth(39).unwrap().to_string();
    assert!(last.contains("devbox"), "{last}");
    assert!(last.contains("2 need you"), "{last}");
    assert!(last.contains("●3 ✓1 ⚠2"), "{last}");
    assert!(last.contains("load 1.50"), "{last}");
    // Top position: the row under the tab bar.
    app.config.ui.status_bar.position = vk_config::BarPosition::Top;
    assert_eq!(app.pane_area().y, 2);
    let t = text(&app);
    assert!(t.lines().nth(1).unwrap().contains("devbox"));
    // Off again via the palette action.
    app.action("status_bar_toggle", None);
    assert_eq!(app.pane_area(), before);
}

// ---- search ---------------------------------------------------------------------------------

/// p1 shows a prompt; copy mode not entered yet.
fn copy_ready() -> (App, Vec<Rx>) {
    let (mut app, rx) = setup(false);
    let mut buf = PaneBuf::blank();
    buf.lines = vec![row("prompt $"), row("")];
    buf.cols = 40;
    buf.cursor = Cursor::default();
    app.machines[0].panes.insert("p1".into(), buf);
    (app, rx)
}

fn history_reply(app: &mut App, req: u64, start: u32, total: u32, rows: Vec<Row>) {
    app.on_frame(
        0,
        ServerFrame::History {
            pane: "p1".into(),
            req,
            start,
            total,
            lines: rows,
        },
    );
}

fn fetch_req(fs: &[ClientFrame]) -> Option<(u64, u32, u32)> {
    fs.iter().find_map(|f| match f {
        ClientFrame::FetchHistory {
            req, start, count, ..
        } => Some((*req, *start, *count)),
        _ => None,
    })
}

#[test]
fn copy_search_falls_back_to_the_archive_and_jumps() {
    let (mut app, mut rx) = copy_ready();
    app.enter_copy(None);
    let fs = frames(&mut rx[0]);
    let (req, _, _) = fetch_req(&fs).unwrap();
    // Bounds of memory/archive.
    let info = fs
        .iter()
        .find_map(|f| match f {
            ClientFrame::Command { req, json } => {
                let v: Value = serde_json::from_str(json).unwrap();
                (v["method"] == "pane.read").then_some((*req, v))
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(info.1["params"]["lines"], 0);
    reply(
        &mut app,
        0,
        info.0,
        json!({"first": 10, "end": 102, "mem_first": 97, "rows": []}),
    );
    // Size-only reply, then the in-memory page (lines 97..100 by absolute index).
    history_reply(&mut app, req, 0, 100, vec![]);
    let (req2, start, count) = fetch_req(&frames(&mut rx[0])).unwrap();
    assert_eq!((start, count), (97, 3), "the first page stays in memory");
    let page: Vec<Row> = (97..100).map(|i| row(&format!("line {i}"))).collect();
    history_reply(&mut app, req2, 97, 100, page);
    // `/needle` isn't loaded: search.query for the pane.
    app.on_key(key('/'));
    for c in "needle".chars() {
        app.on_key(key(c));
    }
    app.on_key(named(NamedKey::Enter));
    let c = commands(&mut rx[0]);
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(c[0].1["method"], "search.query");
    assert_eq!(c[0].1["params"]["pane"], "p1");
    assert_eq!(c[0].1["params"]["q"], "needle");
    let Mode::Copy(cm) = &app.mode else { panic!() };
    assert!(cm.message().unwrap().contains("searching"));
    reply(
        &mut app,
        0,
        c[0].0,
        json!({"hits": [
            {"pane": "p1", "line": 30, "text": "an old needle", "source": "archive"},
            {"pane": "p1", "line": 50, "text": "needle again", "source": "archive"}]}),
    );
    // The nearest older hit (50) is loaded with pane.read up to the loaded top (97).
    let c = commands(&mut rx[0]);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].1["method"], "pane.read");
    assert_eq!(c[0].1["params"]["source"], "archive");
    assert_eq!(c[0].1["params"]["to"], 97);
    assert_eq!(c[0].1["params"]["lines"], 50);
    let rows: Vec<Value> = (47..97)
        .map(|n| {
            json!({"n": n, "text": if n == 50 { "  needle again".to_string() } else { format!("line {n}") }, "wrapped": false})
        })
        .collect();
    reply(
        &mut app,
        0,
        c[0].0,
        json!({"first": 10, "from": 47, "to": 97, "more_before": true, "rows": rows}),
    );
    let Mode::Copy(cm) = &app.mode else { panic!() };
    assert_eq!(cm.top_abs(), 47);
    assert_eq!(cm.row_text(cm.cursor_row()), "  needle again");
    assert!(commands(&mut rx[0]).is_empty());
    // Nothing anywhere: a clear message, no more requests.
    app.on_key(key('/'));
    for c in "zzz".chars() {
        app.on_key(key(c));
    }
    app.on_key(named(NamedKey::Enter));
    let c = commands(&mut rx[0]);
    reply(&mut app, 0, c[0].0, json!({"hits": []}));
    let Mode::Copy(cm) = &app.mode else { panic!() };
    assert!(
        cm.message()
            .unwrap()
            .contains("not found: zzz (whole history)")
    );
    assert!(commands(&mut rx[0]).is_empty());
}

#[test]
fn copy_paging_switches_to_pane_read_past_memory() {
    let (mut app, mut rx) = setup(false);
    app.machines[0].panes.get_mut("p1").unwrap().lines = vec![row("$")];
    app.enter_copy(None);
    let fs = frames(&mut rx[0]);
    let (req, _, _) = fetch_req(&fs).unwrap();
    let info = commands_of(&fs);
    reply(
        &mut app,
        0,
        info[0].0,
        json!({"first": 5, "end": 41, "mem_first": 30, "rows": []}),
    );
    history_reply(&mut app, req, 0, 40, vec![]);
    let (req2, start, count) = fetch_req(&frames(&mut rx[0])).unwrap();
    assert_eq!((start, count), (30, 10), "memory only");
    history_reply(
        &mut app,
        req2,
        30,
        40,
        (30..40).map(|i| row(&format!("l{i}"))).collect(),
    );
    // At the top of memory: older rows come from pane.read, not FetchHistory.
    app.on_key(key('g'));
    let fs = frames(&mut rx[0]);
    assert!(fetch_req(&fs).is_none(), "{fs:?}");
    let c = commands_of(&fs);
    assert_eq!(c[0].1["method"], "pane.read");
    assert_eq!(c[0].1["params"]["to"], 30);
    assert_eq!(c[0].1["params"]["lines"], crate::search::PAGE);
    let rows: Vec<Value> = (5..30)
        .map(|n| json!({"n": n, "text": format!("a{n}"), "wrapped": false}))
        .collect();
    reply(
        &mut app,
        0,
        c[0].0,
        json!({"first": 5, "from": 5, "to": 30, "more_before": false, "rows": rows}),
    );
    let Mode::Copy(cm) = &app.mode else { panic!() };
    assert_eq!(cm.top_abs(), 5);
    assert_eq!(cm.row_text(0), "a5");
    assert!(cm.message().unwrap().contains("start of history"));
    // The archive's start: no further requests.
    app.on_key(key('k'));
    assert!(frames(&mut rx[0]).is_empty());
}

fn commands_of(fs: &[ClientFrame]) -> Vec<(u64, Value)> {
    fs.iter()
        .filter_map(|f| match f {
            ClientFrame::Command { req, json } => Some((*req, serde_json::from_str(json).unwrap())),
            _ => None,
        })
        .collect()
}

#[test]
fn hit_choice_prefers_the_nearest_older_line() {
    use crate::search::pick_hit;
    assert_eq!(pick_hit(&[30, 50, 120], 80, false), Some(50));
    assert_eq!(pick_hit(&[90, 95], 80, false), Some(95));
    assert_eq!(pick_hit(&[90, 95], 80, true), Some(90));
    assert_eq!(pick_hit(&[], 80, false), None);
}

#[test]
fn global_search_popup_lists_hits_and_jumps() {
    let (mut app, mut rx) = setup(false);
    app.action("search_global", None);
    assert!(matches!(app.mode, Mode::Popup(Popup::Search(_))));
    for c in "panic".chars() {
        app.on_key(key(c));
    }
    app.on_key(named(NamedKey::Enter));
    let c = commands(&mut rx[0]);
    assert_eq!(c[0].1["method"], "search.query");
    assert_eq!(c[0].1["params"]["q"], "panic");
    reply(
        &mut app,
        0,
        c[0].0,
        json!({"hits": [
            {"pane": "p3", "pane_handle": "w2:p3", "title": "cargo", "line": 12, "text": "thread panicked", "source": "scrollback", "live": true},
            {"pane": "p9", "pane_handle": "w2:p9", "title": "old", "line": 3, "text": "panic old", "source": "archive", "live": false}]}),
    );
    let t = text(&app);
    assert!(
        t.contains("2 hit(s)") && t.contains("thread panicked"),
        "{t}"
    );
    assert!(t.contains("(closed)"), "{t}");
    // Enter on the first hit: focus its pane; copy mode opens once its cells arrive.
    app.machines[0].panes.remove("p3");
    app.on_key(named(NamedKey::Enter));
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.focused_pane().as_deref(), Some("p3"));
    assert!(app.parity.search.jump.is_some());
    let mut buf = PaneBuf::blank();
    buf.lines = vec![row("$")];
    app.machines[0].panes.insert("p3".into(), buf);
    crate::search::poll_jump(&mut app);
    let Mode::Copy(cm) = &app.mode else {
        panic!("{:?}", app.mode)
    };
    assert_eq!(cm.pane, "p3");
    assert_eq!(cm.archive.goal.as_ref().unwrap().line, 12);
}

// ---- appearance -----------------------------------------------------------------------------

#[test]
fn appearance_parsing() {
    use crate::appearance::*;
    assert_eq!(parse_997(b"\x1b[?997;1n"), Some(true));
    assert_eq!(parse_997(b"junk\x1b[?997;2n\x1b[?64;1c"), Some(false));
    assert_eq!(
        parse_997(b"\x1b[?997;1n\x1b[?997;2n"),
        Some(false),
        "last wins"
    );
    assert_eq!(parse_997(b"\x1b[?997;9n"), None);
    assert_eq!(parse_osc11(b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\"), Some(true));
    assert_eq!(parse_osc11(b"\x1b]11;rgb:ffff/ffff/ffff\x07"), Some(false));
    assert_eq!(parse_osc11(b"\x1b]11;rgb:ef/f1/f5\x07"), Some(false));
    assert_eq!(parse_osc11(b"\x1b]11;#202020\x07"), Some(true));
    assert_eq!(parse_osc11(b"\x1b]11;garbage\x07"), None);
    // The colour-scheme report beats the background colour.
    let both = b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\\x1b[?997;1n\x1b[?62c";
    assert_eq!(
        parse_reports(both),
        Some(Detected {
            dark: true,
            source: Source::Csi996
        })
    );
    assert_eq!(
        parse_reports(b"\x1b]11;rgb:ffff/ffff/ffff\x07"),
        Some(Detected {
            dark: false,
            source: Source::Osc11
        })
    );
    assert_eq!(parse_reports(b"\x1b[?62c"), None);
    assert!(QUERIES.ends_with(b"\x1b[c"));
}

#[test]
fn appearance_reports_once_and_switches_theme() {
    use crate::appearance::*;
    let (mut app, mut rx) = test_app(2);
    app.machines[1].tx = None;
    on_detect(
        &mut app,
        Some(Detected {
            dark: false,
            source: Source::Osc11,
        }),
    );
    let c = commands(&mut rx[0]);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].1["method"], "client.appearance");
    assert_eq!(c[0].1["params"], json!({"dark": false, "source": "osc11"}));
    assert_eq!(app.parity.appearance.applied, "catppuccin-latte");
    assert_eq!(app.theme.fg, crate::theme::Theme::latte().fg);
    // Same value again: nothing re-sent; a reconnect reports again.
    on_detect(
        &mut app,
        Some(Detected {
            dark: false,
            source: Source::Csi996,
        }),
    );
    assert!(commands(&mut rx[0]).is_empty());
    crate::parity::on_connected(&mut app, 0);
    let c = commands(&mut rx[0]);
    let appearance = c.iter().filter(|(_, v)| v["method"] == "client.appearance");
    assert_eq!(appearance.count(), 1);
    // Flip to dark.
    on_detect(
        &mut app,
        Some(Detected {
            dark: true,
            source: Source::Csi996,
        }),
    );
    assert_eq!(commands(&mut rx[0])[0].1["params"]["dark"], true);
    assert_eq!(app.parity.appearance.applied, "catppuccin");
    // Focus regained → one re-probe request (auto mode only, throttled).
    on_focus_gained(&mut app);
    assert!(take_reprobe(&mut app));
    assert!(!take_reprobe(&mut app));
    on_focus_gained(&mut app);
    assert!(!take_reprobe(&mut app), "throttled");
}

#[test]
fn theme_name_resolution() {
    use crate::appearance::theme_name;
    let mut cfg = vk_config::Config::default();
    let unknown = Appearance::default();
    let server_light = Appearance {
        known: true,
        dark: false,
        ..Default::default()
    };
    assert_eq!(theme_name(&cfg, None, &unknown), "catppuccin");
    assert_eq!(theme_name(&cfg, Some(false), &unknown), "catppuccin-latte");
    assert_eq!(theme_name(&cfg, None, &server_light), "catppuccin-latte");
    // This host's detection wins over another client's report.
    assert_eq!(theme_name(&cfg, Some(true), &server_light), "catppuccin");
    cfg.theme.mode = vk_config::ThemeMode::Light;
    assert_eq!(theme_name(&cfg, Some(true), &unknown), "catppuccin-latte");
    cfg.theme.auto_switch = false;
    cfg.theme.name = "terminal".into();
    assert_eq!(theme_name(&cfg, Some(false), &server_light), "terminal");
}

// ---- notifications --------------------------------------------------------------------------

fn notify(app: &mut App, pane: &str, title: &str, delivered: &[&str]) {
    app.on_frame(
        0,
        ServerFrame::Notify {
            title: title.into(),
            body: String::new(),
            pane: Some(pane.into()),
            urgency: "normal".into(),
            delivered: delivered.iter().map(|s| s.to_string()).collect(),
        },
    );
}

#[test]
fn notifications_skip_osc_when_native_and_coalesce() {
    let (mut app, _rx) = setup(false);
    app.host_focused = false;
    notify(&mut app, "p3", "done", &["native"]);
    assert!(
        app.parity.notes.osc_sink.is_empty(),
        "server showed it natively"
    );
    assert_eq!(app.toasts.len(), 1);
    notify(&mut app, "p3", "done", &["native"]);
    notify(&mut app, "p3", "done", &[]);
    assert_eq!(app.toasts.len(), 1, "coalesced into one toast");
    assert!(
        app.toasts[0].text.ends_with("(×3)"),
        "{}",
        app.toasts[0].text
    );
    assert!(
        app.parity.notes.osc_sink.is_empty(),
        "coalesced: no extra OSC"
    );
    // Another pane: a new toast and (not native) an OSC 9 forward.
    notify(&mut app, "p4", "needs approval", &[]);
    assert_eq!(app.toasts.len(), 2);
    assert_eq!(app.parity.notes.osc_sink, vec!["\x1b]9;needs approval\x07"]);
    // Host focused: never forwarded.
    app.host_focused = true;
    notify(&mut app, "p9", "x", &[]);
    assert_eq!(app.parity.notes.osc_sink.len(), 1);
}

#[test]
fn host_metadata_for_render_attach() {
    use crate::notifications::host_from;
    assert_eq!(host_from(None, None), None);
    assert_eq!(host_from(Some(" ".into()), Some(String::new())), None);
    assert_eq!(
        host_from(Some("com.mitchellh.ghostty".into()), Some("ghostty".into())),
        Some(json!({"bundle_id": "com.mitchellh.ghostty", "term_program": "ghostty"}))
    );
    assert_eq!(
        host_from(None, Some("iTerm.app".into())),
        Some(json!({"bundle_id": null, "term_program": "iTerm.app"}))
    );
}

// ---- palette + layouts ----------------------------------------------------------------------

#[test]
fn palette_lists_the_m4_entries() {
    let (app, _rx) = setup(false);
    let entries = crate::nav::palette_entries(&app);
    for id in [
        "float_new",
        "toggle_floats",
        "float_pane",
        "embed_pane",
        "group_new",
        "group_move",
        "group_rename",
        "group_collapse",
        "search_global",
        "layout_save",
        "layout_apply",
        "status_bar_toggle",
        "theme_detect",
    ] {
        assert!(entries.iter().any(|e| e.id == id), "{id}");
    }
    let g = entries.iter().find(|e| e.id == "search_global").unwrap();
    assert_eq!(g.binding.as_deref(), Some("prefix+alt+slash"));
    let ranked = crate::nav::palette_ranked(&app, "save layout");
    assert_eq!(ranked[0].0.id, "layout_save");
    // `prefix+alt+/` opens the global search popup.
    let km = &app.keymap;
    let b = km
        .prefixed(&KeyEvent::new(Key::Char('/'), Mods::ALT))
        .unwrap();
    assert_eq!(b.action, "search_global");
}

#[test]
fn layout_save_prints_a_config_snippet() {
    let (mut app, mut rx) = setup(false);
    crate::nav::run_palette(&mut app, "layout_save");
    let Mode::Prompt(p) = &app.mode else {
        panic!("prompt")
    };
    assert!(matches!(p.kind, PromptKind::LayoutSave { .. }));
    for c in "my dev".chars() {
        app.on_key(key(c));
    }
    app.on_key(named(NamedKey::Enter));
    let c = commands(&mut rx[0]);
    assert_eq!(c[0].1["method"], "layout.export");
    assert_eq!(c[0].1["params"], json!({"tab": "t1", "format": "toml"}));
    let toml = "name = \"edit\"\ncwd = \"/tmp\"\n\n[[tab]]\ntitle = \"edit\"\n\n[tab.pane]\nsplit = \"right\"\n\n[[tab.pane.children]]\nrun = \"nvim .\"\n";
    reply(
        &mut app,
        0,
        c[0].0,
        json!({"layout": {}, "scope": "tab", "toml": toml}),
    );
    let Mode::Popup(Popup::Message { title, body }) = &app.mode else {
        panic!("{:?}", app.mode)
    };
    assert!(title.contains("my-dev"));
    assert!(body.contains("[layouts.my-dev]\nname = \"edit\""), "{body}");
    assert!(body.contains("[[layouts.my-dev.tab]]"), "{body}");
    assert!(body.contains("[layouts.my-dev.tab.pane]"), "{body}");
    assert!(
        body.contains("[[layouts.my-dev.tab.pane.children]]"),
        "{body}"
    );
    let clip = app.clipboard_sink.as_ref().unwrap();
    assert!(String::from_utf8_lossy(&clip[0].0).starts_with("[layouts.my-dev]"));
}

#[test]
fn layout_apply_lists_and_applies() {
    let (mut app, mut rx) = setup(false);
    app.action("layout_apply", None);
    let c = commands(&mut rx[0]);
    assert_eq!(c[0].1["method"], "layout.list");
    reply(
        &mut app,
        0,
        c[0].0,
        json!({"layouts": [
            {"name": "broken", "error": "bad toml"},
            {"name": "dev", "tabs": 2, "panes": 3, "valid": null}]}),
    );
    assert!(text(&app).contains("2 tab(s) · 3 pane(s)"));
    // An invalid one is refused with its problem.
    app.on_key(named(NamedKey::Enter));
    assert!(commands(&mut rx[0]).is_empty());
    assert!(app.toasts.last().unwrap().text.contains("bad toml"));
    app.on_key(key('j'));
    app.on_key(key('w'));
    let c = commands(&mut rx[0]);
    assert_eq!(c[0].1["method"], "layout.apply");
    assert_eq!(
        c[0].1["params"],
        json!({"name": "dev", "focus": true, "workspace": "w1"})
    );
    reply(&mut app, 0, c[0].0, json!({}));
    assert!(
        app.toasts
            .last()
            .unwrap()
            .text
            .contains("applied layout dev")
    );
}

#[test]
fn config_snippet_keeps_multiline_strings() {
    let s = crate::layouts::config_snippet(
        "x",
        "name = \"a\"\n\n[[tab]]\nrun = '''\n[not a header]\n'''\n",
    );
    assert!(s.contains("[[layouts.x.tab]]"));
    assert!(s.contains("\n[not a header]\n"), "{s}");
}
