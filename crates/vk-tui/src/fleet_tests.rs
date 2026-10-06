use super::*;
use crate::app::test_run;
use crate::drafts::tests::{commands, fleet_n, reply, screen};
use vk_proto::input::Mods;

fn key(app: &mut App, k: Key) {
    app.on_key(KeyEvent::new(k, Mods::empty()));
}

#[test]
fn tiles_cover_every_machine_and_show_live_screens() {
    let (mut app, mut rxs) = fleet_n(2);
    app.machines[1].model.runs = vec![test_run("r9", "p2", "codex")];
    app.action("fleet", None);
    assert!(matches!(app.mode, Mode::Popup(Popup::Fleet)));
    assert_eq!(tiles(&app).len(), 2);
    // One read per tile, on its own machine.
    let c0 = commands(&mut rxs[0]);
    let c1 = commands(&mut rxs[1]);
    let r0 = c0.iter().find(|c| c.1 == "pane.read").unwrap();
    let r1 = c1.iter().find(|c| c.1 == "pane.read").unwrap();
    assert_eq!(r0.2["pane"], "p1");
    assert_eq!(r1.2["pane"], "p2");
    assert_eq!(r0.2["source"], "visible");
    reply(
        &mut app,
        0,
        r0.0,
        json!({"text": "$ cargo test\nrunning 12 tests\nok\n\n"}),
    );
    reply(&mut app, 1, r1.0, json!({"text": "codex says hi\n"}));
    let s = screen(&app);
    assert!(s.contains("fleet · 2 agent(s) on 2 machine(s)"), "{s}");
    assert!(s.contains("running 12 tests"), "{s}");
    assert!(s.contains("codex says hi"), "{s}");
    assert!(s.contains("m1/api"), "{s}");
    // A read in flight is not repeated; after the refresh interval it is.
    fetch(&mut app);
    assert_eq!(
        commands(&mut rxs[0])
            .iter()
            .filter(|c| c.1 == "pane.read")
            .count(),
        1
    );
    fetch(&mut app);
    assert!(
        commands(&mut rxs[0]).iter().all(|c| c.1 != "pane.read"),
        "still in flight"
    );
    let inflight: Vec<_> = app
        .ux
        .fleet
        .as_ref()
        .unwrap()
        .inflight
        .iter()
        .cloned()
        .collect();
    assert_eq!(inflight.len(), 2);
    app.ux.fleet.as_mut().unwrap().inflight.clear();
    let at = app.ux.fleet.as_ref().unwrap().fetched_at.unwrap();
    assert_eq!(app.deadlines(at).get("fleet"), Some(at + REFRESH));
    tick(&mut app, at + REFRESH);
    assert!(commands(&mut rxs[0]).iter().any(|c| c.1 == "pane.read"));
}

#[test]
fn t_flips_a_tile_to_the_timeline_and_enter_focuses() {
    let (mut app, _rx) = fleet_n(2);
    app.machines[1].model.runs = vec![test_run("r9", "p2", "codex")];
    app.machines[1].model.runs[0].last_tool = Some("Bash cargo build".into());
    app.machines[1].model.runs[0].last_message = Some("All green.".into());
    app.action("fleet", None);
    key(&mut app, Key::Char('l'));
    assert_eq!(app.ux.fleet.as_ref().unwrap().sel, 1);
    key(&mut app, Key::Char('t'));
    let s = screen(&app);
    assert!(
        s.contains("tool: Bash cargo build") && s.contains("All green."),
        "{s}"
    );
    let v = app.ux.fleet.as_ref().unwrap();
    assert_eq!(tile_view(&app, v, &(1, "r9".into())), TileView::Timeline);
    assert_eq!(tile_view(&app, v, &(0, "r1".into())), TileView::Terminal);
    key(&mut app, Key::Char('T'));
    let v = app.ux.fleet.as_ref().unwrap();
    assert_eq!(tile_view(&app, v, &(0, "r1".into())), TileView::Timeline);
    // Config default timeline flips the meaning.
    app.config.ui.fleet.tile_view = TileView::Timeline;
    let v = app.ux.fleet.as_ref().unwrap();
    assert_eq!(tile_view(&app, v, &(0, "r1".into())), TileView::Terminal);
    key(&mut app, Key::Named(NamedKey::Enter));
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.cur, 1);
    assert_eq!(app.focused_pane().as_deref(), Some("p2"));
}

#[test]
fn closed_grid_stops_polling() {
    let (mut app, mut rxs) = fleet_n(1);
    app.action("fleet", None);
    key(&mut app, Key::Named(NamedKey::Escape));
    assert!(matches!(app.mode, Mode::Normal));
    commands(&mut rxs[0]);
    app.on_tick();
    assert!(app.ux.fleet.is_none());
    assert!(app.deadlines(Instant::now()).get("fleet").is_none());
    assert!(commands(&mut rxs[0]).iter().all(|c| c.1 != "pane.read"));
}
