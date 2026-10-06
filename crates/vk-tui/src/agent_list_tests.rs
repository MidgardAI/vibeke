//! Cross-machine agent list: every live run on every machine, by attention; filter, jump to
//! the pane on its machine, the card for an open interaction, the binding.

use super::*;
use crate::app::{test_interaction, test_run};
use crate::drafts::tests::{fleet_n, named, screen, typ};
use vk_proto::input::{Key, Mods, NamedKey};

/// m0: claude r1 (done, unseen) in p1, codex r2 working in p2; m1: claude r9 in p1 with an
/// open approval, pi r8 (ended) in p2.
fn setup() -> (
    App,
    Vec<tokio::sync::mpsc::UnboundedReceiver<vk_proto::render::ClientFrame>>,
) {
    let (mut app, rx) = fleet_n(2);
    let mut r2 = test_run("r2", "p2", "codex");
    r2.execution.value = Execution::Working;
    r2.execution.since_ms = 1_000;
    app.machines[0].model.runs.push(r2);
    let mut r9 = test_run("r9", "p1", "claude");
    r9.name = Some("reviewer".into());
    let mut it = test_interaction("i9", "p1", "Bash rm -rf build", 5_000);
    it.run = "r9".into();
    app.machines[1].model.runs = vec![r9];
    app.machines[1].model.interactions = vec![it];
    let mut r8 = test_run("r8", "p2", "pi");
    r8.ended_at_ms = Some(9);
    app.machines[1].model.runs.push(r8);
    (app, rx)
}

#[test]
fn lists_every_machine_by_attention() {
    let (app, _rx) = setup();
    let e = entries(&app);
    let runs: Vec<(usize, &str)> = e.iter().map(|x| (x.mi, x.run.as_str())).collect();
    // Approval first, then done-unseen, then working; ended runs left out.
    assert_eq!(runs, vec![(1, "r9"), (0, "r1"), (0, "r2")]);
    assert_eq!(e[0].interaction.as_deref(), Some("i9"));
    assert!(e[0].label.starts_with("m1 · "), "{}", e[0].label);
    assert!(e[0].label.contains("reviewer · api"), "{}", e[0].label);
}

#[test]
fn binding_opens_the_popup_and_draws_rows() {
    let (mut app, _rx) = setup();
    // prefix+alt+a (prefix+a stays next_attention).
    let b = app
        .keymap
        .prefixed(&KeyEvent::new(Key::Char('a'), Mods::ALT))
        .unwrap();
    assert_eq!(b.action, "agent_list");
    assert_eq!(
        app.keymap
            .prefixed(&KeyEvent::new(Key::Char('a'), Mods::empty()))
            .unwrap()
            .action,
        "next_attention"
    );
    app.action("agent_list", None);
    assert!(matches!(app.mode, Mode::Popup(Popup::Agents { .. })));
    let s = screen(&app);
    assert!(
        s.contains("agents · 3 on 2 machine(s) · by attention"),
        "{s}"
    );
    let lines: Vec<&str> = s.lines().collect();
    let a = lines.iter().position(|l| l.contains("reviewer")).unwrap();
    let b = lines.iter().position(|l| l.contains("working")).unwrap();
    assert!(a < b, "{s}");
    assert!(lines[a].contains("approve: "), "{}", lines[a]);
}

#[test]
fn filter_and_enter_jump_to_the_pane_on_its_machine() {
    let (mut app, _rx) = setup();
    app.action("agent_list", None);
    typ(&mut app, "codex");
    let Mode::Popup(Popup::Agents { filter, .. }) = &app.mode else {
        panic!("still open")
    };
    assert_eq!(filter, "codex");
    assert_eq!(ranked(&app, "codex").len(), 1);
    app.on_key(named(NamedKey::Enter));
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.cur, 0);
    assert_eq!(app.focused_pane().as_deref(), Some("p2"));
    // The approval on m1: jump switches machines.
    app.action("agent_list", None);
    app.on_key(named(NamedKey::Enter));
    assert_eq!(app.cur, 1);
    assert_eq!(app.focused_pane().as_deref(), Some("p1"));
}

#[test]
fn alt_enter_opens_the_card_of_an_unfocused_agent() {
    let (mut app, _rx) = setup();
    app.action("agent_list", None);
    app.on_key(KeyEvent::new(Key::Named(NamedKey::Enter), Mods::ALT));
    assert!(
        matches!(&app.mode, Mode::Popup(Popup::Card { interaction, .. }) if interaction == "i9"),
        "{:?}",
        app.mode
    );
    // Nothing matches: says so.
    app.mode = Mode::Normal;
    app.action("agent_list", None);
    typ(&mut app, "zzzz");
    assert!(screen(&app).contains("nothing matches"));
}
