//! The decision guard (08 §8): releases and repeats never decide, a change the user did not
//! cause waits out the arm delay (pushed out by typing), the user's own navigation adds no
//! delay, a key held since before arming does not count; and the surfaces that use it (the
//! close confirmation, the interaction card, the inbox, `prefix+a`, the mouse over popups,
//! `ui.confirm_close`).

use super::*;
use crate::app::{Mode, Popup, test_app, test_interaction};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;
use vk_proto::input::{KeyEvent, KeyKind, Mods, NamedKey};
use vk_proto::model::InteractionKind;
use vk_proto::render::ClientFrame;

type Rx = UnboundedReceiver<ClientFrame>;

fn press(k: Key) -> KeyEvent {
    KeyEvent::new(k, Mods::empty())
}

fn kind(k: Key, kind: KeyKind) -> KeyEvent {
    let mut e = press(k);
    e.kind = kind;
    e
}

fn y() -> Key {
    Key::Char('y')
}

fn methods(rx: &mut Rx) -> Vec<(String, Value)> {
    let mut v = Vec::new();
    while let Ok(f) = rx.try_recv() {
        if let ClientFrame::Command { json, .. } = f {
            let j: Value = serde_json::from_str(&json).unwrap();
            v.push((
                j["method"].as_str().unwrap_or("").to_string(),
                j["params"].clone(),
            ));
        }
    }
    v
}

fn sent(rx: &mut Rx, method: &str) -> Vec<Value> {
    methods(rx)
        .into_iter()
        .filter(|(m, _)| m == method)
        .map(|(_, p)| p)
        .collect()
}

/// Let the arm delay pass without sleeping: show the surface, move its deadline into the past
/// and draw again.
fn pass_delay(app: &App) {
    before_draw(app);
    app.decide.rearm(Instant::now() - ARM_DELAY * 2);
    before_draw(app);
}

// ---- the guard itself ------------------------------------------------------------------------

#[test]
fn releases_repeats_and_modified_keys_never_decide() {
    let g = Guard::default();
    let t0 = Instant::now();
    g.observe(Some("a"), t0);
    let t1 = t0 + ARM_DELAY;
    g.drawn(Some("a"), t1);
    assert!(g.armed(t1));
    assert_eq!(
        g.gate(Some("a"), &kind(y(), KeyKind::Release), t1, false, true),
        Gate::Ignore
    );
    assert_eq!(
        g.gate(Some("a"), &kind(y(), KeyKind::Repeat), t1, false, true),
        Gate::Ignore
    );
    for m in [Mods::CTRL, Mods::ALT, Mods::SUPER, Mods::META] {
        assert_eq!(
            g.gate(Some("a"), &KeyEvent::new(y(), m), t1, false, true),
            Gate::Ignore
        );
    }
    // Shift alone is fine (`X`, `A`).
    assert_eq!(
        g.gate(
            Some("a"),
            &KeyEvent::new(Key::Char('X'), Mods::SHIFT),
            t1,
            false,
            true
        ),
        Gate::Go
    );
    assert_eq!(g.gate(Some("a"), &press(y()), t1, false, true), Gate::Go);
    // Navigation passes, repeats included; only its release is dropped.
    let j = Key::Char('j');
    assert_eq!(
        g.gate(Some("a"), &kind(j, KeyKind::Repeat), t1, false, false),
        Gate::Go
    );
    assert_eq!(
        g.gate(Some("a"), &kind(j, KeyKind::Release), t1, false, false),
        Gate::Ignore
    );
}

#[test]
fn an_unexpected_change_waits_and_typing_pushes_the_wait_out() {
    let g = Guard::default();
    let t0 = Instant::now();
    g.observe(Some("a"), t0);
    g.drawn(Some("a"), t0 + ARM_DELAY);
    let t1 = t0 + ARM_DELAY;
    assert_eq!(g.gate(Some("a"), &press(y()), t1, false, true), Gate::Go);
    // Item b slides into place: a key at once waits.
    let t2 = t1 + Duration::from_millis(10);
    assert_eq!(g.gate(Some("b"), &press(y()), t2, false, true), Gate::Wait);
    // Typing (or a repeating key without the kitty protocol) keeps it waiting.
    let t3 = t2 + Duration::from_millis(500);
    assert_eq!(g.gate(Some("b"), &press(y()), t3, false, true), Gate::Wait);
    let t4 = t3 + Duration::from_millis(500);
    assert_eq!(g.gate(Some("b"), &press(y()), t4, false, true), Gate::Wait);
    // A pause, but no frame showed b with its keys live yet.
    let t5 = t4 + ARM_DELAY;
    assert_eq!(g.gate(Some("b"), &press(y()), t5, false, true), Gate::Wait);
    let t6 = t5 + ARM_DELAY;
    g.drawn(Some("b"), t6);
    assert_eq!(g.gate(Some("b"), &press(y()), t6, false, true), Gate::Go);
}

#[test]
fn the_users_own_moves_add_no_delay() {
    let g = Guard::default();
    let t0 = Instant::now();
    g.observe(Some("a"), t0);
    let t1 = t0 + ARM_DELAY;
    g.drawn(Some("a"), t1);
    g.follow(Some("b"));
    assert_eq!(g.gate(Some("b"), &press(y()), t1, false, true), Gate::Go);
    // Closing the surface and opening another with a key: no delay either.
    g.follow(None);
    g.follow(Some("confirm:x"));
    assert_eq!(
        g.gate(Some("confirm:x"), &press(y()), t1, false, true),
        Gate::Go
    );
    // Nothing selected (None) and back: an async selection waits.
    g.follow(None);
    assert_eq!(g.gate(Some("c"), &press(y()), t1, false, true), Gate::Wait);
}

#[test]
fn with_kitty_a_key_held_before_arming_counts_only_after_its_release() {
    let g = Guard::default();
    let t0 = Instant::now();
    g.observe(Some("a"), t0);
    assert_eq!(g.gate(Some("a"), &press(y()), t0, true, true), Gate::Wait);
    let t1 = t0 + ARM_DELAY * 2;
    g.drawn(Some("a"), t1);
    // Its repeats, and a press we never saw released, do not decide.
    assert_eq!(
        g.gate(Some("a"), &kind(y(), KeyKind::Repeat), t1, true, true),
        Gate::Ignore
    );
    assert_eq!(g.gate(Some("a"), &press(y()), t1, true, true), Gate::Ignore);
    assert_eq!(g.gate(Some("a"), &press(y()), t1, true, true), Gate::Go);
    // Held, released, pressed: decides.
    g.observe(Some("b"), t1);
    assert_eq!(g.gate(Some("b"), &press(y()), t1, true, true), Gate::Wait);
    let t2 = t1 + ARM_DELAY;
    g.drawn(Some("b"), t2);
    assert_eq!(
        g.gate(Some("b"), &kind(y(), KeyKind::Release), t2, true, true),
        Gate::Ignore
    );
    assert_eq!(g.gate(Some("b"), &press(y()), t2, true, true), Gate::Go);
}

#[test]
fn the_redraw_deadline_is_armed_until_a_frame_showed_the_keys_live() {
    let g = Guard::default();
    let t0 = Instant::now();
    let mut d = crate::deadline::Deadlines::default();
    g.deadlines(&mut d);
    assert!(d.names().is_empty());
    g.observe(Some("a"), t0);
    let mut d = crate::deadline::Deadlines::default();
    g.deadlines(&mut d);
    assert_eq!(d.get("decide.arm"), Some(t0 + ARM_DELAY));
    g.drawn(Some("a"), t0 + ARM_DELAY);
    let mut d = crate::deadline::Deadlines::default();
    g.deadlines(&mut d);
    assert!(d.names().is_empty());
}

// ---- surfaces ---------------------------------------------------------------------------------

fn confirm_close(app: &mut App) {
    app.mode = Mode::Popup(Popup::Confirm {
        message: "Close tab 1?".into(),
        action: Box::new(crate::app::Action::CloseTab("T1".into())),
    });
}

#[test]
fn the_close_confirmation_takes_only_a_fresh_y() {
    let (mut app, mut rxs) = test_app(1);
    confirm_close(&mut app);
    // Enter never confirms; a release of y does not either.
    app.on_key(press(Key::Named(NamedKey::Enter)));
    app.on_key(kind(y(), KeyKind::Release));
    assert!(matches!(app.mode, Mode::Popup(Popup::Confirm { .. })));
    assert!(sent(&mut rxs[0], "tab.close").is_empty());
    app.on_key(press(y()));
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(sent(&mut rxs[0], "tab.close").len(), 1);
}

#[test]
fn a_confirmation_opened_by_an_async_reply_waits() {
    let (mut app, mut rxs) = test_app(1);
    app.decide.delay = ARM_DELAY;
    // Opened outside a key (as a mouse click or a reply does): the next frame arms the guard.
    confirm_close(&mut app);
    before_draw(&app);
    app.on_key(press(y()));
    assert!(matches!(app.mode, Mode::Popup(Popup::Confirm { .. })));
    assert!(sent(&mut rxs[0], "tab.close").is_empty());
    pass_delay(&app);
    app.on_key(press(y()));
    assert_eq!(sent(&mut rxs[0], "tab.close").len(), 1);
}

fn card(app: &mut App, it: vk_proto::model::Interaction) {
    let id = it.id.clone();
    app.machines[0].model.interactions.push(it);
    app.mode = Mode::Popup(Popup::Card {
        interaction: id,
        sel: 0,
    });
}

#[test]
fn one_x_keystroke_never_dismisses_a_card() {
    let (mut app, mut rxs) = test_app(1);
    card(&mut app, test_interaction("i1", "p2", "Allow rm?", 0));
    // Press and release: only the confirmation is armed.
    app.on_key(press(Key::Char('x')));
    app.on_key(kind(Key::Char('x'), KeyKind::Release));
    assert!(sent(&mut rxs[0], "attention.update").is_empty());
    assert!(matches!(app.mode, Mode::Popup(Popup::Card { .. })));
    assert!(screen_text(&app).contains("Press x again"));
    // The second press dismisses, and a toast names what was dismissed.
    app.on_key(press(Key::Char('x')));
    let u = sent(&mut rxs[0], "attention.update");
    assert_eq!(u.len(), 1);
    assert_eq!(u[0]["key"], json!({"kind": "interaction", "id": "i1"}));
    assert_eq!(
        app.inbox.last_dismissed.as_ref().map(|d| d.title.as_str()),
        Some("i1 Allow rm?")
    );
}

fn screen_text(app: &App) -> String {
    let mut g = crate::screen::Grid::new(app.size.0, app.size.1);
    crate::draw::compose(app, &mut g);
    crate::tasks::grid_text(&g)
}

fn question(id: &str) -> vk_proto::model::Interaction {
    let mut it = test_interaction(id, "p2", "Which one?", 0);
    it.kind = InteractionKind::Question;
    it.questions = vec![vk_proto::model::Question {
        id: "q".into(),
        prompt: "Which one?".into(),
        header: None,
        multi: false,
        options: vec![
            vk_proto::model::QuestionOption {
                id: "a".into(),
                label: "First".into(),
                description: None,
                selected: false,
            },
            vk_proto::model::QuestionOption {
                id: "b".into(),
                label: "Second".into(),
                description: None,
                selected: false,
            },
        ],
        allow_free_text: false,
    }];
    it
}

#[test]
fn the_release_of_the_enter_that_opened_a_card_does_not_answer_it() {
    let (mut app, mut rxs) = test_app(1);
    card(&mut app, question("i1"));
    app.on_key(kind(Key::Named(NamedKey::Enter), KeyKind::Release));
    assert!(sent(&mut rxs[0], "interaction.answer").is_empty());
    assert!(matches!(app.mode, Mode::Popup(Popup::Card { .. })));
    app.on_key(press(Key::Named(NamedKey::Enter)));
    let a = sent(&mut rxs[0], "interaction.answer");
    assert_eq!(a.len(), 1);
    assert_eq!(a[0]["choices"]["q"], json!(["a"]));
}

#[test]
fn a_card_that_changed_under_the_user_waits() {
    let (mut app, mut rxs) = test_app(1);
    app.decide.delay = ARM_DELAY;
    card(&mut app, test_interaction("i1", "p2", "Allow rm?", 0));
    pass_delay(&app);
    // Another card replaces it without a key (an async open): y waits.
    card(&mut app, test_interaction("i2", "p1", "Allow curl?", 0));
    app.on_key(press(y()));
    assert!(sent(&mut rxs[0], "interaction.answer").is_empty());
    assert!(screen_text(&app).contains(WAIT_HINT));
    pass_delay(&app);
    app.on_key(press(y()));
    let a = sent(&mut rxs[0], "interaction.answer");
    assert_eq!(a.len(), 1);
    assert_eq!(a[0]["interaction"], "i2");
}

#[test]
fn a_long_command_wraps_says_what_is_left_out_and_keeps_the_keys() {
    let (mut app, _rxs) = test_app(1);
    app.size = (100, 30);
    let mut it = test_interaction("i1", "p2", "Allow?", 0);
    let long = format!("echo {}", "x".repeat(150));
    let mut cmd = vec![long.clone()];
    cmd.extend((0..30).map(|i| format!("line {i}")));
    it.action = Some(vk_proto::model::ActionInfo {
        tool: "Bash".into(),
        summary: "run".into(),
        command: Some(cmd.join("\n")),
        paths: (0..10).map(|i| format!("/src/f{i}.rs")).collect(),
        diff: None,
        risk: vk_proto::model::Risk::Low,
        risk_reasons: vec![],
    });
    card(&mut app, it);
    let s = screen_text(&app);
    // The long first line continues on the next row instead of being cut.
    assert!(s.matches('x').count() >= 150, "{s}");
    assert!(s.contains("more line(s)"), "{s}");
    assert!(s.contains("more path(s)"), "{s}");
    // The key hint stays on screen.
    assert!(s.contains("[y]") && s.contains("[esc]"), "{s}");
}

#[test]
fn card_lines_list_hidden_paths_and_diff_lines() {
    let (app, _rxs) = test_app(1);
    let mut it = test_interaction("i1", "p2", "Allow?", 0);
    it.action = Some(vk_proto::model::ActionInfo {
        tool: "Edit".into(),
        summary: "edit".into(),
        command: None,
        paths: (0..9).map(|i| format!("/src/f{i}.rs")).collect(),
        diff: Some(
            (0..20)
                .map(|i| format!("+line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        risk: vk_proto::model::Risk::Low,
        risk_reasons: vec![],
    });
    let mut out = Vec::new();
    crate::popups::card_lines(&app, 0, &it, 0, 60, &mut out);
    let text: Vec<&str> = out.iter().map(|(s, _)| s.as_str()).collect();
    assert!(text.contains(&"  … 3 more path(s)"), "{text:?}");
    assert!(
        text.iter().any(|l| l.starts_with("… 9 more diff line(s)")),
        "{text:?}"
    );
    assert!(text.iter().all(|l| l.chars().count() <= 60), "{text:?}");
}

#[test]
fn hard_wrap_keeps_every_character() {
    let s = "a  b   c";
    let rows = crate::popups::hard_wrap(s, 3);
    assert_eq!(rows.concat(), s);
    assert!(rows.iter().all(|r| r.chars().count() <= 3));
}

// ---- prefix+a --------------------------------------------------------------------------------

fn stale_next_attention(app: &mut App, rxs: &mut [Rx]) -> u64 {
    app.machines[0]
        .model
        .interactions
        .push(test_interaction("i1", "p2", "Allow rm?", 0));
    crate::inbox::next_attention(app, false);
    let mut req = None;
    while let Ok(f) = rxs[0].try_recv() {
        if let ClientFrame::Command { req: r, json } = f
            && json.contains("attention.list")
        {
            req = Some(r);
        }
    }
    req.expect("attention.list sent")
}

fn ranking(app: &mut App, req: u64) {
    let int = json!({"key": {"kind": "interaction", "id": "i1"}, "class": 3,
        "title": "Allow rm?", "subtitle": "", "task": null, "run": null, "pane": "p2",
        "interaction": "i1", "explanation": "Waiting", "age_ms": 1000, "risk": null,
        "effort": null, "snoozed_until_ms": null, "woke_from_snooze": null, "urgent": false});
    let result = json!({"items": [int], "coverage": {"complete": true, "notes": []}});
    let json = json!({"jsonrpc": "2.0", "id": req, "result": result}).to_string();
    app.on_frame(
        0,
        vk_proto::render::ServerFrame::CommandResult { req, json },
    );
}

#[test]
fn prefix_a_opens_nothing_once_the_user_pressed_another_key() {
    let (mut app, mut rxs) = test_app(1);
    let req = stale_next_attention(&mut app, &mut rxs);
    app.on_key(press(Key::Char('l')));
    ranking(&mut app, req);
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn prefix_a_never_replaces_a_popup_the_user_opened() {
    let (mut app, mut rxs) = test_app(1);
    let req = stale_next_attention(&mut app, &mut rxs);
    // Opened without a key press (a click): the waiting prefix+a must not replace it.
    confirm_close(&mut app);
    ranking(&mut app, req);
    assert!(matches!(app.mode, Mode::Popup(Popup::Confirm { .. })));
}

#[test]
fn a_late_ranking_opens_nothing_and_says_so() {
    let (mut app, mut rxs) = test_app(1);
    let req = stale_next_attention(&mut app, &mut rxs);
    app.inbox.next_after_at = Some(Instant::now() - crate::inbox::NEXT_AFTER_MAX * 2);
    ranking(&mut app, req);
    assert!(matches!(app.mode, Mode::Normal));
    assert!(
        app.toasts
            .last()
            .is_some_and(|t| t.text.contains("took too long"))
    );
}

#[test]
fn a_card_opened_by_a_ranking_reply_is_not_armed() {
    let (mut app, mut rxs) = test_app(1);
    app.decide.delay = ARM_DELAY;
    let req = stale_next_attention(&mut app, &mut rxs);
    ranking(&mut app, req);
    assert!(matches!(app.mode, Mode::Popup(Popup::Card { .. })));
    app.on_key(press(y()));
    assert!(sent(&mut rxs[0], "interaction.answer").is_empty());
}

// ---- the mouse over popups -------------------------------------------------------------------

fn click(app: &mut App, x: u16, y: u16) {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    });
}

#[test]
fn a_click_outside_a_popup_closes_it_and_reaches_nothing_behind() {
    let (mut app, _rxs) = test_app(1);
    confirm_close(&mut app);
    // Draw once so the popup's frame is known, then click inside it: nothing happens.
    screen_text(&app);
    let r = app.popup_rect.get().expect("the confirm frame was drawn");
    click(&mut app, r.x + 1, r.y + 1);
    assert!(matches!(app.mode, Mode::Popup(Popup::Confirm { .. })));
    // A click elsewhere closes it like esc (nothing is closed or focused).
    click(&mut app, 0, app.size.1 - 1);
    assert!(matches!(app.mode, Mode::Normal));
}

// ---- ui.confirm_close ------------------------------------------------------------------------

#[test]
fn confirm_close_applies_the_same_rule_to_panes_tabs_and_workspaces() {
    use vk_config::ConfirmClose;
    let (mut app, _rxs) = crate::drafts::tests::fleet();
    for (setting, busy, ask) in [
        (ConfirmClose::Never, true, false),
        (ConfirmClose::Never, false, false),
        (ConfirmClose::Always, false, true),
        (ConfirmClose::Always, true, true),
        (ConfirmClose::Running, false, false),
        (ConfirmClose::Running, true, true),
    ] {
        app.config.ui.confirm_close = setting;
        assert_eq!(
            crate::popups::should_confirm_close(&app, busy),
            ask,
            "{setting:?} busy={busy}"
        );
        for p in app.machines[0].model.panes.iter_mut() {
            p.fg_cmdline = if busy {
                vec!["vim".into()]
            } else {
                vec!["zsh".into()]
            };
        }
        assert_eq!(crate::popups::pane_running(&app, 0, "p1"), busy);
        assert_eq!(crate::popups::workspace_busy(&app, 0, "W1"), busy);
        for action in ["close_pane", "close_tab", "close_workspace"] {
            app.mode = Mode::Normal;
            app.action(action, None);
            assert_eq!(
                matches!(app.mode, Mode::Popup(Popup::Confirm { .. })),
                ask,
                "{action} with {setting:?} busy={busy}"
            );
        }
    }
}
