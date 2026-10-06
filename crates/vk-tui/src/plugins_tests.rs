//! Herdr plugin surfaces in the TUI: popup/overlay geometry, modality and drawing, palette
//! entries (disabled when untrusted), `plugin_action` key bindings, link handlers, the window
//! title and scroll reports. Fake machines only; nothing touches the host terminal.

use super::*;
use crate::app::{App, Mode, PaneBuf, Popup, test_app};
use crate::screen::Grid;
use crossterm::event::{KeyModifiers, MouseButton as CtButton, MouseEvent, MouseEventKind};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use vk_proto::input::{Key, KeyEvent, Mods, NamedKey};
use vk_proto::model::*;
use vk_proto::render::{ClientFrame, Row, ServerFrame, Span, Style};

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
    }
}

fn pane(id: &str, created_by: &str) -> Pane {
    serde_json::from_value(json!({
        "id": id, "handle": format!("w1:{id}"), "tab": "t1", "workspace": "w1", "title": null,
        "auto_title": format!("sh-{id}"), "cwd": null, "cols": 80, "rows": 24, "child_pid": null,
        "fg_cmdline": [], "exited": false, "exit_code": null, "unread": false,
        "marked_unread": false, "pinned": false, "created_by": created_by, "recovered": null
    }))
    .unwrap()
}

const POPUP: &str = "plugin-surface:popup:60%:10:acme.demo/picker";
const OVERLAY: &str = "plugin-surface:overlay:::acme.demo/board";

/// Tab t1: tiled p1, an ordinary float p2 and the given plugin surfaces (as floats);
/// focus on `focus`.
fn setup(surfaces: &[(&str, &str)], focus: &str) -> (App, Vec<Rx>) {
    let (mut app, rxs) = test_app(1);
    let m = &mut app.machines[0];
    m.model.workspaces = vec![
        serde_json::from_value(json!({
            "id": "w1", "handle": "w1", "name": "api", "auto_name": "api",
            "root_path": "/tmp", "task": null, "order": 1.0, "branch": null
        }))
        .unwrap(),
    ];
    let mut t: Tab = serde_json::from_value(json!({
        "id": "t1", "handle": "w1:t1", "workspace": "w1", "title": null, "number": 1,
        "layout": {"Leaf": {"pane": "p1"}}, "focused_pane": null, "zoomed_pane": null,
        "order": 1.0
    }))
    .unwrap();
    t.floating = vec![FloatingPane::centred("p2", 1)];
    m.model.panes = vec![pane("p1", "user"), pane("p2", "user")];
    for (i, (id, tag)) in surfaces.iter().enumerate() {
        t.floating.push(FloatingPane::centred(id, 2 + i as u32));
        let mut p = pane(id, tag);
        p.title = Some(format!("Title {id}"));
        m.model.panes.push(p);
        m.panes.insert(id.to_string(), PaneBuf::blank());
    }
    m.model.tabs = vec![t];
    m.focus = ClientFocus {
        workspace: Some("w1".into()),
        tab: Some("t1".into()),
        pane: Some(focus.into()),
    };
    (app, rxs)
}

// ---- geometry and state -----------------------------------------------------------------------

#[test]
fn popup_and_overlay_geometry() {
    let area = Rect {
        x: 10,
        y: 1,
        w: 100,
        h: 40,
    };
    let p = PluginSurface::parse(POPUP).unwrap();
    let (o, i) = geometry(&p, area).unwrap();
    assert_eq!((o.w, o.h), (60, 10), "60% wide, 10 cells high");
    assert_eq!((o.x, o.y), (10 + 20, 1 + 15), "centred");
    assert_eq!((i.x, i.y, i.w, i.h), (o.x + 1, o.y + 1, 58, 8));
    let d = PluginSurface::parse("plugin-surface:popup:::a/b").unwrap();
    let (o, _) = geometry(&d, area).unwrap();
    assert_eq!((o.w, o.h), (80, 32), "default 80%×80%");
    let big = PluginSurface::parse("plugin-surface:popup:500:500:a/b").unwrap();
    assert_eq!(
        geometry(&big, area).unwrap().0,
        area,
        "clamped to the pane area"
    );
    let ov = PluginSurface::parse(OVERLAY).unwrap();
    let (o, i) = geometry(&ov, area).unwrap();
    assert_eq!(o, area);
    assert_eq!((i.y, i.h, i.w), (2, 39, 100), "one header row");
    let tiny = Rect {
        x: 0,
        y: 0,
        w: 3,
        h: 2,
    };
    assert!(geometry(&p, tiny).is_none());
}

#[test]
fn surfaces_sit_above_floats_and_overlays_hide_the_tiling() {
    let (app, _rx) = setup(&[("pp", POPUP), ("ov", OVERLAY)], "pp");
    let s = surfaces(&app);
    assert_eq!(
        s.iter().map(|x| x.pane.as_str()).collect::<Vec<_>>(),
        ["ov", "pp"],
        "popups above overlays"
    );
    // Ordinary floats exclude surfaces.
    let f: Vec<String> = crate::floats::visible(&app)
        .into_iter()
        .map(|f| f.pane)
        .collect();
    assert_eq!(f, ["p2"]);
    // Under an overlay nothing else is hit or sized.
    let rects: Vec<String> = app.pane_rects().into_iter().map(|(p, _)| p).collect();
    assert_eq!(rects, ["pp", "ov"]);
    let (app, _rx) = setup(&[("pp", POPUP)], "pp");
    let rects: Vec<String> = app.pane_rects().into_iter().map(|(p, _)| p).collect();
    assert_eq!(
        rects,
        ["pp", "p2", "p1"],
        "a popup alone covers only its rect"
    );
    assert!(is_surface(&app, "pp") && !is_surface(&app, "p2"));
    assert_eq!(popup(&app).unwrap().pane, "pp");
}

#[test]
fn view_hint_sizes_the_popup_terminal() {
    let (mut app, mut rx) = setup(&[("pp", POPUP)], "pp");
    frames(&mut rx[0]);
    app.send_view_hints(true);
    let hint = frames(&mut rx[0])
        .into_iter()
        .find_map(|f| match f {
            ClientFrame::ViewHint { panes, .. } => Some(panes),
            _ => None,
        })
        .unwrap();
    let pp = hint.iter().find(|r| r.pane == "pp").unwrap();
    let s = popup(&app).unwrap();
    assert_eq!((pp.cols, pp.rows), (s.inner.w, s.inner.h));
}

// ---- modality ---------------------------------------------------------------------------------

#[test]
fn popup_is_modal_for_keys() {
    let (mut app, mut rx) = setup(&[("pp", POPUP)], "pp");
    // A direct binding would normally fire; with the popup focused the key goes to it.
    app.config
        .keys
        .bindings
        .insert("goto".into(), "ctrl+g".into());
    app.keymap = crate::keymap::Keymap::from_config(&app.config);
    app.on_key(KeyEvent::new(Key::Char('g'), Mods::CTRL));
    app.on_key(KeyEvent::ch('a'));
    let keys: Vec<(String, Key)> = frames(&mut rx[0])
        .into_iter()
        .filter_map(|f| match f {
            ClientFrame::Key { pane, key, .. } => Some((pane, key.key)),
            _ => None,
        })
        .collect();
    assert_eq!(
        keys,
        [("pp".into(), Key::Char('g')), ("pp".into(), Key::Char('a'))]
    );
    assert!(matches!(app.mode, Mode::Normal), "no goto popup");
    // Esc while the command runs is the command's.
    app.on_key(KeyEvent::named(NamedKey::Escape));
    assert!(commands(&mut rx[0]).is_empty());
    // Prefix actions are refused with a hint; prefix+x closes the popup.
    app.on_key(KeyEvent::new(Key::Char('b'), Mods::CTRL));
    app.on_key(KeyEvent::ch('c'));
    assert!(commands(&mut rx[0]).is_empty(), "no new tab under a popup");
    assert!(app.toasts.last().unwrap().text.contains("prefix+x"));
    app.on_key(KeyEvent::new(Key::Char('b'), Mods::CTRL));
    app.on_key(KeyEvent::ch('x'));
    let c = commands(&mut rx[0]);
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(c[0].1["method"], "plugin.surface.close");
    assert_eq!(c[0].1["params"], json!({"pane": "pp"}));
}

#[test]
fn exited_popup_closes_on_esc_and_by_itself() {
    let (mut app, mut rx) = setup(&[("pp", POPUP)], "pp");
    app.machines[0]
        .model
        .panes
        .iter_mut()
        .find(|p| p.id == "pp")
        .unwrap()
        .exited = true;
    assert!(text(&app).contains("esc closes"));
    app.on_key(KeyEvent::named(NamedKey::Escape));
    let c = commands(&mut rx[0]);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].1["method"], "plugin.surface.close");
    // The per-frame observer doesn't resend a dismissal in flight.
    observe(&mut app);
    assert!(commands(&mut rx[0]).is_empty());
    // Once the pane is gone the record is forgotten.
    app.machines[0].model.panes.retain(|p| p.id != "pp");
    observe(&mut app);
    assert!(app.plugins.closing.is_empty());
}

#[test]
fn focus_returns_to_an_open_popup_and_clicks_outside_are_ignored() {
    let (mut app, mut rx) = setup(&[("pp", POPUP)], "p1");
    observe(&mut app);
    let f: Vec<String> = frames(&mut rx[0])
        .into_iter()
        .filter_map(|f| match f {
            ClientFrame::Focus { pane } => Some(pane),
            _ => None,
        })
        .collect();
    assert_eq!(f, ["pp"], "session-modal popup takes the focus back");
    let s = popup(&app).unwrap();
    let click = |x, y| MouseEvent {
        kind: MouseEventKind::Down(CtButton::Left),
        column: x,
        row: y,
        modifiers: KeyModifiers::empty(),
    };
    // Outside the popup (on p1): swallowed, no focus change.
    let area = app.pane_area();
    app.on_mouse(click(area.x + 1, area.y + 1));
    assert!(frames(&mut rx[0]).is_empty());
    assert_eq!(app.focused_pane().as_deref(), Some("pp"));
    // Inside it: reaches the pane (no capture by Vibeke).
    assert!(!on_mouse(&mut app, s.inner.x + 1, s.inner.y + 1));
    assert!(
        on_mouse(&mut app, s.outer.x, s.outer.y),
        "the frame is chrome"
    );
}

#[test]
fn overlay_close_pane_restores_through_the_server() {
    let (mut app, mut rx) = setup(&[("ov", OVERLAY)], "ov");
    app.action("close_pane", None);
    let c = commands(&mut rx[0]);
    assert_eq!(c[0].1["method"], "plugin.surface.close", "{c:?}");
    // An ordinary pane still closes the ordinary way.
    let (mut app, mut rx) = setup(&[("ov", OVERLAY)], "p1");
    app.action("close_pane", None);
    let c = commands(&mut rx[0]);
    assert_eq!(c[0].1["method"], "pane.close");
    // Overlay header row is chrome; its content isn't.
    let (mut app, _rx) = setup(&[("ov", OVERLAY)], "ov");
    let a = app.pane_area();
    assert!(on_mouse(&mut app, a.x + 3, a.y));
    assert!(!on_mouse(&mut app, a.x + 3, a.y + 2));
}

#[test]
fn surfaces_draw_over_the_layout() {
    let (mut app, _rx) = setup(&[("pp", POPUP)], "pp");
    app.machines[0].panes.insert(
        "pp".into(),
        PaneBuf {
            lines: vec![row("pick a branch")],
            cols: 20,
            rows: 1,
            ..PaneBuf::blank()
        },
    );
    let t = text(&app);
    assert!(t.contains("Title pp · acme.demo"), "{t}");
    assert!(t.contains("pick a branch"));
    assert!(t.contains("╭") && t.contains("╯"));
    assert!(t.contains("prefix+x closes"));
    let (app, _rx) = setup(&[("ov", OVERLAY)], "ov");
    let t = text(&app);
    assert!(t.contains("⧉ Title ov · acme.demo"), "{t}");
    // Popup drawn with plugin-provided control characters removed.
    let (mut app, _rx) = setup(&[("pp", POPUP)], "pp");
    app.machines[0].model.panes.last_mut().unwrap().title =
        Some("evil\x1b]2;x\x07\u{202E}t".into());
    let t = text(&app);
    assert!(t.contains("evil]2;xt"), "{t}");
}

// ---- palette and key bindings -----------------------------------------------------------------

fn actions_reply() -> Value {
    json!({"actions": [
        {"plugin_id": "acme.demo", "action_id": "open", "qualified_id": "acme.demo.open",
         "title": "Open board", "description": null, "contexts": ["global"],
         "available": true, "status": "active"},
        {"plugin_id": "evil.corp", "action_id": "run", "qualified_id": "evil.corp.run",
         "title": "Run things", "available": false, "status": "untrusted"},
    ]})
}

/// Feed the replies `on_connected` asked for.
fn connect(app: &mut App, rx: &mut Rx, actions: Value, handlers: Value) {
    on_connected(app, 0);
    for (req, c) in commands(rx) {
        match c["method"].as_str().unwrap() {
            "plugin.action.list" => reply(app, 0, req, actions.clone()),
            "plugin.link_handler.list" => reply(app, 0, req, handlers.clone()),
            "compat.ui.state" => reply(app, 0, req, json!({"window_title": null})),
            m => panic!("unexpected {m}"),
        }
    }
}

#[test]
fn palette_lists_plugin_actions_and_disables_untrusted_ones() {
    let (mut app, mut rx) = setup(&[], "p1");
    app.config.keys.command.push(vk_config::KeyCommand {
        key: "prefix+alt+o".into(),
        kind: vk_config::CommandType::PluginAction,
        command: "acme.demo.open".into(),
        ..Default::default()
    });
    connect(
        &mut app,
        &mut rx[0],
        actions_reply(),
        json!({"handlers": []}),
    );
    let e = crate::nav::palette_entries(&app);
    let open = e
        .iter()
        .find(|x| x.id == "plugin:0:acme.demo.open")
        .unwrap();
    assert_eq!(open.desc, "Plugin: Open board (acme.demo)");
    assert!(!open.disabled);
    assert_eq!(open.binding.as_deref(), Some("prefix+alt+o"));
    assert!(
        !e.iter().any(|x| x.id == "command:0"),
        "the binding is shown on the plugin entry"
    );
    let evil = e.iter().find(|x| x.id == "plugin:0:evil.corp.run").unwrap();
    assert!(evil.disabled);
    assert!(
        evil.desc
            .contains("trust with vibeke plugin trust evil.corp --legacy"),
        "{}",
        evil.desc
    );
    // Fuzzy-findable and drawn.
    let ranked = crate::nav::palette_ranked(&app, "open board");
    assert_eq!(ranked[0].0.id, "plugin:0:acme.demo.open");
    // Running the disabled one only explains; the trusted one runs in the focused context.
    crate::nav::run_palette(&mut app, "plugin:0:evil.corp.run");
    assert!(commands(&mut rx[0]).is_empty());
    assert!(app.toasts.last().unwrap().text.contains("--legacy"));
    crate::nav::run_palette(&mut app, "plugin:0:acme.demo.open");
    let c = commands(&mut rx[0]);
    assert_eq!(c[0].1["method"], "plugin.action.run");
    assert_eq!(
        c[0].1["params"],
        json!({"plugin": "acme.demo", "action": "open", "pane": "p1", "source": "palette"})
    );
    reply(&mut app, 0, c[0].0, json!({"log": {"status": "running"}}));
    assert!(app.toasts.last().unwrap().text.contains("Open board"));
    // Opening the palette refreshes the list.
    crate::nav::open_palette(&mut app, String::new());
    let m: Vec<String> = commands(&mut rx[0])
        .into_iter()
        .map(|(_, c)| c["method"].as_str().unwrap().to_string())
        .collect();
    assert!(m.contains(&"plugin.action.list".to_string()), "{m:?}");
    let t = text(&app);
    assert!(t.contains("Plugin: Open board"), "{t}");
}

#[test]
fn older_servers_without_plugins_list_nothing() {
    let (mut app, mut rx) = setup(&[], "p1");
    on_connected(&mut app, 0);
    for (req, _) in commands(&mut rx[0]) {
        let json = json!({"jsonrpc": "2.0", "id": req, "error": {"code": -32601, "message": "no", "data": {"kind": "method_not_found"}}}).to_string();
        app.on_frame(0, ServerFrame::CommandResult { req, json });
    }
    assert!(palette_entries(&app).is_empty());
    assert!(window_title(&app).is_none());
}

#[test]
fn plugin_action_key_binding_parses_and_runs() {
    let toml = r#"
[[keys.command]]
key = "prefix+alt+o"
type = "plugin_action"
command = "acme.demo.open"
description = "Open the board"
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, toml).unwrap();
    let (cfg, warnings) = vk_config::Config::load(&path).unwrap();
    assert!(
        warnings.iter().all(|w| !w.key.contains("description")),
        "{warnings:?}"
    );
    let c = &cfg.keys.command[0];
    assert_eq!(c.kind, vk_config::CommandType::PluginAction);
    assert_eq!(c.description.as_deref(), Some("Open the board"));
    let (mut app, mut rx) = setup(&[], "p1");
    app.config = cfg;
    app.keymap = crate::keymap::Keymap::from_config(&app.config);
    app.on_key(KeyEvent::new(Key::Char('b'), Mods::CTRL));
    app.on_key(KeyEvent::new(Key::Char('o'), Mods::ALT));
    let c = commands(&mut rx[0]);
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(c[0].1["method"], "plugin.action.run");
    assert_eq!(
        c[0].1["params"],
        json!({"action": "acme.demo.open", "pane": "p1", "source": "keybinding"})
    );
    reply(&mut app, 0, c[0].0, json!({"log": {}}));
    assert!(app.toasts.last().unwrap().text.contains("Open the board"));
}

// ---- link handlers ----------------------------------------------------------------------------

fn handlers_reply() -> Value {
    json!({"handlers": [
        {"plugin_id": "acme.hunk", "handler_id": "github-commit", "title": "Review commit",
         "pattern": "^https://github\\.com/[^/]+/[^/]+/commit/[0-9a-f]{7,40}$",
         "action_id": "review", "available": true, "status": "active"},
        {"plugin_id": "evil.corp", "handler_id": "any", "title": "Grab it",
         "pattern": "^https://", "action_id": "x", "available": false, "status": "untrusted"},
    ]})
}

fn with_lines(app: &mut App, pane: &str, lines: &[&str]) {
    app.machines[0].panes.insert(
        pane.into(),
        PaneBuf {
            lines: lines.iter().map(|l| row(l)).collect(),
            cols: 80,
            rows: lines.len() as u16,
            ..PaneBuf::blank()
        },
    );
}

#[test]
fn hint_on_a_matching_url_offers_handlers_and_runs_one() {
    let (mut app, mut rx) = setup(&[], "p1");
    connect(
        &mut app,
        &mut rx[0],
        json!({"actions": []}),
        handlers_reply(),
    );
    let url = "https://github.com/acme/app/commit/3f9c2ab1";
    with_lines(&mut app, "p1", &[&format!("see {url} now")]);
    app.action("url_hints", None);
    let _ = commands(&mut rx[0]);
    app.on_key(KeyEvent::ch('a'));
    let Mode::Popup(Popup::PluginLink(c)) = &app.mode else {
        panic!("expected the link chooser, got {:?}", app.mode);
    };
    assert_eq!(c.url, url);
    assert_eq!(
        c.handlers.len(),
        2,
        "both match; the untrusted one disabled"
    );
    assert!(c.default_open);
    let t = text(&app);
    assert!(t.contains("Review commit (acme.hunk)"), "{t}");
    assert!(
        t.contains("untrusted — trust with vibeke plugin trust evil.corp"),
        "{t}"
    );
    assert!(t.contains("Open (default)"));
    // The untrusted handler only explains.
    app.on_key(KeyEvent::ch('2'));
    assert!(commands(&mut rx[0]).is_empty());
    assert!(app.toasts.last().unwrap().text.contains("--legacy"));
    // Enter on the first runs `plugin.link.open` with the URL and handler.
    app.action("url_hints", None);
    let _ = commands(&mut rx[0]);
    app.on_key(KeyEvent::ch('a'));
    app.on_key(KeyEvent::named(NamedKey::Enter));
    let c = commands(&mut rx[0]);
    assert_eq!(c[0].1["method"], "plugin.link.open");
    assert_eq!(
        c[0].1["params"],
        json!({"plugin": "acme.hunk", "handler": "github-commit", "url": url, "pane": "p1"})
    );
    // The default row opens it as before.
    app.action("url_hints", None);
    app.on_key(KeyEvent::ch('a'));
    app.on_key(KeyEvent::ch('3'));
    assert_eq!(app.nav.opened, [url]);
}

#[test]
fn shift_label_copies_and_non_matching_links_skip_the_chooser() {
    let (mut app, mut rx) = setup(&[], "p1");
    connect(
        &mut app,
        &mut rx[0],
        json!({"actions": []}),
        handlers_reply(),
    );
    with_lines(
        &mut app,
        "p1",
        &["https://github.com/acme/app/commit/3f9c2ab1"],
    );
    app.action("url_hints", None);
    app.on_key(KeyEvent::ch('A'));
    assert!(matches!(app.mode, Mode::Normal));
    let sink = app.clipboard_sink.as_ref().unwrap();
    assert_eq!(
        sink.last().unwrap().0,
        b"https://github.com/acme/app/commit/3f9c2ab1"
    );
    // A git SHA matches no handler: copied directly.
    with_lines(&mut app, "p1", &["commit 3f9c2ab1 done"]);
    app.action("url_hints", None);
    app.on_key(KeyEvent::ch('a'));
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(
        app.clipboard_sink.as_ref().unwrap().last().unwrap().0,
        b"3f9c2ab1"
    );
}

#[test]
fn ctrl_click_on_a_path_offers_a_file_handler() {
    let (mut app, mut rx) = setup(&[], "p1");
    connect(
        &mut app,
        &mut rx[0],
        json!({"actions": []}),
        json!({"handlers": [{"plugin_id": "nvim", "handler_id": "file-path", "title": "Open in nvim",
            "pattern": "^~?[\\w.@/-]+/[\\w.@-]+\\.[A-Za-z]\\w*(?::\\d+){0,2}$", "available": true, "status": "active"}]}),
    );
    with_lines(&mut app, "p1", &["error at (src/main.rs:42), see"]);
    assert_eq!(
        token_at(&app, 0, "p1", 12, 0).as_deref(),
        Some("src/main.rs:42")
    );
    assert_eq!(token_at(&app, 0, "p1", 6, 0).as_deref(), Some("at"));
    assert!(token_at(&app, 0, "p1", 8, 0).is_none(), "whitespace");
    let r = app
        .pane_rects()
        .into_iter()
        .find(|(p, _)| p == "p1")
        .unwrap()
        .1;
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(CtButton::Left),
        column: r.x + 12,
        row: r.y,
        modifiers: KeyModifiers::CONTROL,
    });
    let Mode::Popup(Popup::PluginLink(c)) = &app.mode else {
        panic!("expected the link chooser, got {:?}", app.mode);
    };
    assert_eq!(c.url, "src/main.rs:42");
    assert!(!c.default_open, "the default for a path copies");
}

// ---- window title -----------------------------------------------------------------------------

#[test]
fn plugin_window_title_goes_to_osc2_or_the_tab_bar() {
    let (mut app, _rx) = setup(&[], "p1");
    app.machines[0].features = vec!["event_push".into()];
    let ev = json!({"seq": 5, "type": "client.window_title_changed", "subject": {},
        "data": {"title": "deploy \x1b[31mprod\u{202E}"}});
    crate::push::on_events(
        &mut app,
        0,
        vec![vk_proto::render::PushedEvent {
            seq: 5,
            kind: "client.window_title_changed".into(),
            json: ev.to_string(),
        }],
        false,
    );
    assert_eq!(window_title(&app), Some("deploy [31mprod"));
    assert!(app.config.ui.title_sync, "default on");
    let out = String::from_utf8(crate::nav::title_update(&mut app).unwrap()).unwrap();
    assert!(out.ends_with("\x1b]2;deploy [31mprod\x07"), "{out:?}");
    assert!(
        !text(&app).contains("deploy [31mprod"),
        "not duplicated in the tab bar"
    );
    // Without title sync it shows in the tab bar.
    app.config.ui.title_sync = false;
    let first_row = text(&app).lines().next().unwrap().to_string();
    assert!(first_row.contains("deploy [31mprod"), "{first_row}");
    // Cleared: back to the formatted title.
    app.config.ui.title_sync = true;
    on_title_event(&mut app, 0, &json!({"data": {"title": null}}));
    assert!(window_title(&app).is_none());
    assert!(crate::nav::title(&app).unwrap() != "deploy [31mprod");
    assert!(crate::push::TYPES.contains(&"client.window_title_changed"));
}

// ---- scroll reports ---------------------------------------------------------------------------

#[test]
fn copy_mode_scroll_is_reported_coalesced_and_reset() {
    let (mut app, mut rx) = setup(&[], "p1");
    let t0 = Instant::now();
    // Not without the server feature.
    let mut cm = crate::copy::CopyMode::new("p1", vec![row("x"); 10], 80, Default::default());
    cm.pending_req = Some(1);
    cm.on_history(1, 0, 50, vec![row("h"); 50]);
    cm.scroll_up(12);
    app.mode = Mode::Copy(Box::new(cm));
    report_scroll(&mut app, t0);
    assert!(frames(&mut rx[0]).is_empty());
    app.machines[0].features = vec![SCROLL_FEATURE.into()];
    let scrolls = |rx: &mut Rx| -> Vec<(String, u32, u32)> {
        frames(rx)
            .into_iter()
            .filter_map(|f| match f {
                ClientFrame::ScrollView {
                    pane,
                    offset,
                    total,
                } => Some((pane, offset, total)),
                _ => None,
            })
            .collect()
    };
    report_scroll(&mut app, t0);
    assert_eq!(scrolls(&mut rx[0]), [("p1".into(), 12, 50)]);
    // Unchanged → nothing; a change within the interval waits for the tick.
    report_scroll(&mut app, t0 + Duration::from_millis(10));
    if let Mode::Copy(cm) = &mut app.mode {
        cm.scroll_up(3);
    }
    report_scroll(&mut app, t0 + Duration::from_millis(50));
    assert!(scrolls(&mut rx[0]).is_empty());
    report_scroll(&mut app, t0 + Duration::from_millis(400));
    assert_eq!(scrolls(&mut rx[0]), [("p1".into(), 15, 50)]);
    // Leaving copy mode reports the bottom once.
    app.mode = Mode::Normal;
    report_scroll(&mut app, t0 + Duration::from_millis(800));
    assert_eq!(scrolls(&mut rx[0]), [("p1".into(), 0, 0)]);
    report_scroll(&mut app, t0 + Duration::from_millis(1200));
    assert!(scrolls(&mut rx[0]).is_empty());
}

// ---- action contexts, manifest key bindings, agent views --------------------------------------

fn ctx_actions() -> Value {
    json!({"actions": [
        {"plugin_id": "acme.ctx", "action_id": "any", "qualified_id": "acme.ctx.any",
         "title": "Anywhere", "contexts": ["global"], "available": true, "status": "active"},
        {"plugin_id": "acme.ctx", "action_id": "ws", "qualified_id": "acme.ctx.ws",
         "title": "Workspace thing", "contexts": ["workspace"], "available": true, "status": "active"},
        {"plugin_id": "acme.ctx", "action_id": "sel", "qualified_id": "acme.ctx.sel",
         "title": "Selected text", "contexts": ["selection"], "available": true, "status": "active"},
        {"plugin_id": "acme.ctx", "action_id": "pane", "qualified_id": "acme.ctx.pane",
         "title": "Pane thing", "contexts": ["pane", "tab"], "available": true, "status": "active"},
    ]})
}

fn plugin_ids(app: &App) -> Vec<String> {
    crate::nav::palette_entries(app)
        .into_iter()
        .filter(|e| e.id.starts_with("plugin:0:acme.ctx."))
        .map(|e| e.id.trim_start_matches("plugin:0:acme.ctx.").to_string())
        .collect()
}

#[test]
fn the_palette_only_offers_actions_that_apply_here() {
    let (mut app, mut rx) = setup(&[], "p1");
    connect(&mut app, &mut rx[0], ctx_actions(), json!({"handlers": []}));
    // Focused workspace, tab and pane; no selection.
    assert_eq!(plugin_ids(&app), vec!["any", "ws", "pane"]);
    // Nothing focused: only `global` actions remain.
    app.machines[0].focus = ClientFocus::default();
    assert_eq!(plugin_ids(&app), vec!["any"]);
    // Running a hidden action (stale palette id or key binding) explains instead of running.
    crate::nav::run_palette(&mut app, "plugin:0:acme.ctx.ws");
    assert!(commands(&mut rx[0]).is_empty());
    let t = app.toasts.last().unwrap().text.clone();
    assert!(
        t.contains("not available here") && t.contains("workspace"),
        "{t}"
    );
    app.config.keys.command.push(vk_config::KeyCommand {
        key: "prefix+alt+s".into(),
        kind: vk_config::CommandType::PluginAction,
        command: "acme.ctx.sel".into(),
        ..Default::default()
    });
    let c = app.config.keys.command[0].clone();
    run_key_command(&mut app, &c);
    assert!(commands(&mut rx[0]).is_empty());
    assert!(app.toasts.last().unwrap().text.contains("selection"));
    // A global action always runs.
    crate::nav::run_palette(&mut app, "plugin:0:acme.ctx.any");
    assert_eq!(commands(&mut rx[0])[0].1["method"], "plugin.action.run");
}

fn with_keys(keys: Value) -> Value {
    let mut v = actions_reply();
    v["keybindings"] = keys;
    v
}

#[test]
fn manifest_default_bindings_are_installed_and_removed_with_the_plugin() {
    let (mut app, mut rx) = setup(&[], "p1");
    // The user already uses prefix+alt+j for something else: the server reported that plugin
    // binding as a conflict, and a stale installed duplicate cannot take it over either.
    app.config
        .keys
        .bindings
        .insert("help".into(), "prefix+alt+j".into());
    app.keymap = crate::keymap::Keymap::from_config(&app.config);
    connect(
        &mut app,
        &mut rx[0],
        with_keys(json!([
            {"plugin_id": "acme.demo", "key": "prefix+alt+k", "action": "acme.demo.open",
             "installed": true},
            {"plugin_id": "acme.demo", "key": "prefix+alt+j", "action": "acme.demo.open",
             "installed": false, "reason": "conflict", "conflicts_with": "help"},
        ])),
        json!({"handlers": []}),
    );
    assert_eq!(
        app.keymap.binding_for("plugin:0:acme.demo.open").as_deref(),
        Some("prefix+alt+k")
    );
    let ev = |c: char| KeyEvent::new(Key::Char(c), Mods::ALT);
    assert_eq!(
        app.keymap.prefixed(&ev('k')).map(|b| b.action.as_str()),
        Some("plugin:0:acme.demo.open")
    );
    assert_eq!(
        app.keymap.prefixed(&ev('j')).map(|b| b.action.as_str()),
        Some("help"),
        "the user's key wins"
    );
    // The palette shows the installed binding.
    let e = crate::nav::palette_entries(&app);
    let open = e
        .iter()
        .find(|x| x.id == "plugin:0:acme.demo.open")
        .unwrap();
    assert_eq!(open.binding.as_deref(), Some("prefix+alt+k"));
    // Firing it runs the action.
    app.action("plugin:0:acme.demo.open", None);
    assert_eq!(commands(&mut rx[0])[0].1["method"], "plugin.action.run");
    // The plugin is disabled: the next list has no installed bindings and the key is gone.
    on_registry_event(&mut app, 0);
    for (req, c) in commands(&mut rx[0]) {
        if c["method"] == "plugin.action.list" {
            reply(
                &mut app,
                0,
                req,
                with_keys(json!([
                    {"plugin_id": "acme.demo", "key": "prefix+alt+k", "action": "acme.demo.open",
                     "installed": false, "reason": "disabled"}
                ])),
            );
        }
    }
    assert!(app.keymap.binding_for("plugin:0:acme.demo.open").is_none());
    assert!(app.keymap.prefixed(&ev('k')).is_none());
    assert_eq!(
        app.keymap.prefixed(&ev('j')).map(|b| b.action.as_str()),
        Some("help")
    );
}

#[test]
fn agent_views_show_in_the_sidebar_and_go_away() {
    let (mut app, mut rxs) = crate::drafts::tests::fleet();
    on_connected(&mut app, 0);
    let mut ui = None;
    for (req, c) in commands(&mut rxs[0]) {
        if c["method"] == "compat.ui.state" {
            ui = Some(req);
        }
    }
    let views = json!({"window_title": null, "agent_views": [
        {"plugin_id": "acme.ci", "run": "r1", "text": "tests 41/50\u{1b}[31m",
         "detail": "line1\nline2", "tone": "warn"},
        {"plugin_id": "acme.ci", "run": "gone", "text": "x", "tone": "info"},
    ]});
    reply(&mut app, 0, ui.unwrap(), views);
    let row = crate::draw::agent_row_text(&app, 0, "r1");
    assert!(row.contains("▸ tests 41/50[31m"), "{row}");
    assert!(!row.contains('\u{1b}'), "control characters are stripped");
    assert_eq!(
        agent_view(&app, 0, "r1").unwrap().detail.as_deref(),
        Some("line1\nline2")
    );
    // The pushed event makes the client re-read; an empty list clears the line.
    let ev = vk_proto::render::PushedEvent {
        seq: 1,
        kind: "plugin.agent_view_changed".into(),
        json: json!({"type": "plugin.agent_view_changed", "subject": {}, "data": {"plugin": "acme.ci"}})
            .to_string(),
    };
    crate::push::on_events(&mut app, 0, vec![ev], false);
    let c = commands(&mut rxs[0]);
    let req = c
        .iter()
        .find(|(_, c)| c["method"] == "compat.ui.state")
        .unwrap()
        .0;
    reply(
        &mut app,
        0,
        req,
        json!({"window_title": null, "agent_views": []}),
    );
    assert!(!crate::draw::agent_row_text(&app, 0, "r1").contains('▸'));
    assert!(crate::push::TYPES.contains(&"plugin.agent_view_changed"));
    assert!(crate::push::TYPES.contains(&"plugin.registry_changed"));
}
