//! Sandboxes overview tests: grouping and the footer counts, live updates from
//! `cloud.box.changed`, suspend and resume by capability, destroy with the unsynced conflict
//! (Bring back first, Destroy anyway), clean up with a dry run, adopt, open, and signing in.

use super::*;
use crate::drafts::tests::{ch, commands, fleet, named, only, reply, reply_err, screen};
use vk_proto::render::PushedEvent;

fn providers() -> Value {
    json!({"providers": [
        {"id": "sprites", "label": "Sprites", "caps": {}, "default": true,
         "auth": {"state": "ok", "account": "acme"},
         "methods": [{"kind": "paste_token", "label": "Token"}]},
        {"id": "e2b", "label": "E2B", "caps": {}, "default": false,
         "auth": {"state": "missing"},
         "methods": [{"kind": "paste_token", "label": "API key"}]}
    ]})
}

fn boxes() -> Value {
    json!({"boxes": [
        {"box": "sprites/b1", "provider": "sprites", "id": "b1", "name": "vk-aaaa-bbbb",
         "state": "running", "ownership": "attached", "task": "T9", "workspace": "W1",
         "panes": ["p2"], "sessions": 1, "created_at": 1000, "last_activity_at": 2000,
         "unsynced": {"commits": 2, "dirty": 0, "untracked": 1, "summary": "2 commits"},
         "caps": {"checkpoints": true, "explicit_suspend": false}},
        {"box": "sprites/b3", "provider": "sprites", "id": "b3", "name": "vk-zzzz",
         "state": "cold", "ownership": "orphaned", "panes": [],
         "caps": {"checkpoints": true, "explicit_suspend": false}},
        {"box": "e2b/x1", "provider": "e2b", "id": "x1", "name": "vk-cccc-dddd",
         "state": "running", "ownership": "idle", "panes": [], "unsynced": null,
         "caps": {"explicit_suspend": true}}
    ], "errors": []})
}

/// The view opened and both lists answered.
fn opened() -> (
    App,
    Vec<tokio::sync::mpsc::UnboundedReceiver<vk_proto::render::ClientFrame>>,
) {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    app.action("sandboxes", None);
    assert!(matches!(app.mode, Mode::Popup(Popup::Sandboxes)));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "cloud.providers");
    assert_eq!(p, json!({}));
    reply(&mut app, 0, req, providers());
    let (req, p) = only(&cmds, "cloud.box.list");
    assert_eq!(p, json!({"refresh": true}));
    reply(&mut app, 0, req, boxes());
    (app, rxs)
}

fn push(app: &mut App, data: Value) {
    let ev = PushedEvent {
        seq: 1,
        kind: "cloud.box.changed".into(),
        json: json!({"seq": 1, "type": "cloud.box.changed", "subject": {"box": data["box"]},
                     "data": data})
        .to_string(),
    };
    crate::push::on_events(app, 0, vec![ev], false);
}

#[test]
fn boxes_are_grouped_by_provider_with_the_sign_in_state_and_counts() {
    let (app, _rxs) = opened();
    let s = screen(&app);
    assert!(s.contains("Sprites — signed in as acme"), "{s}");
    assert!(s.contains("E2B — not signed in"), "{s}");
    assert!(
        s.contains("vk-aaaa-bbbb") && s.contains("vk-cccc-dddd"),
        "{s}"
    );
    assert!(s.contains("⚠ unsynced"), "{s}");
    assert!(s.contains("orphaned") && s.contains("idle"), "{s}");
    assert!(s.contains("2 running · 1 idle"), "{s}");
    let v = app.ux.sandboxes.as_ref().unwrap();
    let names: Vec<_> = v.rows.iter().map(|b| b.box_id.as_str()).collect();
    assert_eq!(names, ["sprites/b1", "sprites/b3", "e2b/x1"]);
}

#[test]
fn changed_events_update_and_destroyed_removes() {
    let (mut app, _rxs) = opened();
    push(
        &mut app,
        json!({"box": "e2b/x1", "provider": "e2b", "id": "x1", "name": "vk-cccc-dddd",
               "state": "suspended", "ownership": "idle", "panes": []}),
    );
    assert_eq!(app.ux.sandboxes.as_ref().unwrap().counts(), (1, 1));
    push(
        &mut app,
        json!({"box": "sprites/b3", "provider": "sprites", "id": "b3", "name": "vk-zzzz",
               "state": "destroyed", "ownership": "orphaned", "panes": []}),
    );
    assert_eq!(app.ux.sandboxes.as_ref().unwrap().rows.len(), 2);
    push(
        &mut app,
        json!({"box": "sprites/n1", "provider": "sprites", "id": "n1", "name": "vk-new",
               "state": "running", "ownership": "attached", "panes": []}),
    );
    assert_eq!(app.ux.sandboxes.as_ref().unwrap().rows.len(), 3);
}

#[test]
fn destroy_asks_and_unsynced_work_offers_bring_back_or_force() {
    let (mut app, mut rxs) = opened();
    app.on_key(ch('d'));
    assert!(screen(&app).contains("Destroy vk-aaaa-bbbb?"));
    app.on_key(ch('n'));
    assert!(commands(&mut rxs[0]).is_empty());
    app.on_key(ch('d'));
    app.on_key(ch('y'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "cloud.box.destroy");
    assert_eq!(p, json!({"box": "sprites/b1"}));
    reply_err(
        &mut app,
        0,
        req,
        "conflict",
        json!({"reason": "unsynced_changes", "unsynced": {"summary": "2 commits ahead"}}),
    );
    let s = screen(&app);
    assert!(
        s.contains("2 commits ahead") && s.contains("bring back first"),
        "{s}"
    );
    // Destroy anyway sends `force`.
    app.on_key(ch('d'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "cloud.box.destroy");
    assert_eq!(p, json!({"box": "sprites/b1", "force": true}));
    reply(
        &mut app,
        0,
        req,
        json!({"box": "sprites/b1", "destroyed": true}),
    );
    assert_eq!(app.ux.sandboxes.as_ref().unwrap().rows.len(), 2);
}

#[test]
fn bring_back_first_opens_the_flow_for_the_box() {
    let (mut app, mut rxs) = opened();
    app.on_key(ch('d'));
    app.on_key(ch('y'));
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "cloud.box.destroy");
    reply_err(
        &mut app,
        0,
        req,
        "conflict",
        json!({"reason": "unsynced_changes", "unsynced": {"summary": "x"}}),
    );
    app.on_key(ch('b'));
    assert!(matches!(app.mode, Mode::Popup(Popup::CloudSend)));
    assert!(app.ux.sandboxes.is_none());
    let f = app.ux.cloud.flow.as_ref().unwrap();
    assert_eq!(f.src, crate::cloud::Source::Box("sprites/b1".into()));
    let cmds = commands(&mut rxs[0]);
    assert_eq!(only(&cmds, "handoff.peers").1, json!({}));
}

#[test]
fn suspend_and_resume_follow_the_capabilities() {
    let (mut app, mut rxs) = opened();
    // Sprites suspends by itself.
    app.on_key(ch('p'));
    assert!(commands(&mut rxs[0]).is_empty());
    assert!(screen(&app).contains("suspends sandboxes by itself"));
    // E2B can.
    app.on_key(ch('j'));
    app.on_key(ch('j'));
    app.on_key(ch('p'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "cloud.box.suspend");
    assert_eq!(p, json!({"box": "e2b/x1"}));
    reply(
        &mut app,
        0,
        req,
        json!({"box": "e2b/x1", "provider": "e2b", "id": "x1", "name": "vk-cccc-dddd",
               "state": "suspended", "ownership": "idle", "panes": [],
               "caps": {"explicit_suspend": true}}),
    );
    app.on_key(ch('p'));
    let cmds = commands(&mut rxs[0]);
    assert_eq!(only(&cmds, "cloud.box.resume").1, json!({"box": "e2b/x1"}));
    // No checkpoints on E2B.
    app.on_key(ch('c'));
    assert!(commands(&mut rxs[0]).is_empty());
}

#[test]
fn checkpoint_and_adopt() {
    let (mut app, mut rxs) = opened();
    app.on_key(ch('c'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "cloud.box.checkpoint");
    assert_eq!(p, json!({"box": "sprites/b1"}));
    reply(
        &mut app,
        0,
        req,
        json!({"box": "sprites/b1", "checkpoint": "v3"}),
    );
    assert!(screen(&app).contains("v3"));
    // Adopting an attached sandbox is refused here; an orphaned one is sent.
    app.on_key(ch('a'));
    assert!(commands(&mut rxs[0]).is_empty());
    app.on_key(ch('j'));
    app.on_key(ch('a'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "cloud.box.adopt");
    assert_eq!(p, json!({"box": "sprites/b3"}));
    reply(&mut app, 0, req, json!({"box": "sprites/b3", "task": "T5"}));
    assert!(screen(&app).contains("adopted vk-zzzz as task T5"));
    // The list is read again.
    let cmds = commands(&mut rxs[0]);
    assert_eq!(only(&cmds, "cloud.box.list").1, json!({"refresh": true}));
}

#[test]
fn clean_up_lists_the_dry_run_then_confirms() {
    let (mut app, mut rxs) = opened();
    app.on_key(ch('C'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "cloud.prune");
    assert_eq!(p, json!({"dry_run": true}));
    reply(
        &mut app,
        0,
        req,
        json!({"candidates": [{"box": "sprites/b3", "provider": "sprites", "id": "b3",
                               "name": "vk-zzzz", "state": "cold", "ownership": "orphaned",
                               "panes": []}],
               "destroyed": [], "skipped": []}),
    );
    let s = screen(&app);
    assert!(
        s.contains("Clean up 1 sandbox(es)") && s.contains("vk-zzzz"),
        "{s}"
    );
    app.on_key(ch('y'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "cloud.prune");
    assert_eq!(p, json!({}));
    reply(
        &mut app,
        0,
        req,
        json!({"candidates": [], "destroyed": ["sprites/b3"], "skipped": []}),
    );
    assert!(screen(&app).contains("cleaned up 1"));
    assert!(
        commands(&mut rxs[0])
            .iter()
            .any(|c| c.1 == "cloud.box.list")
    );
    // Nothing to clean up says so.
    app.on_key(ch('C'));
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "cloud.prune");
    reply(
        &mut app,
        0,
        req,
        json!({"candidates": [], "destroyed": [], "skipped": []}),
    );
    assert!(screen(&app).contains("nothing to clean up"));
}

#[test]
fn enter_opens_the_boxes_pane() {
    let (mut app, _rxs) = opened();
    app.on_key(named(NamedKey::Enter));
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.machines[0].focus.pane.as_deref(), Some("p2"));
}

#[test]
fn sign_in_reuses_the_auth_stage_and_refreshes() {
    let (mut app, mut rxs) = opened();
    // On the E2B row, `s` signs in to E2B.
    app.on_key(ch('j'));
    app.on_key(ch('j'));
    app.on_key(ch('s'));
    assert!(screen(&app).contains("Sign in to E2B"));
    app.on_paste("key-1".into());
    let s = screen(&app);
    assert!(!s.contains("key-1"), "{s}");
    app.on_key(named(NamedKey::Enter));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "cloud.auth.set");
    assert_eq!(p, json!({"provider": "e2b", "token": "key-1"}));
    reply(
        &mut app,
        0,
        req,
        json!({"provider": "e2b", "account": "team"}),
    );
    let v = app.ux.sandboxes.as_ref().unwrap();
    assert!(matches!(v.stage, Stage::List));
    assert!(v.providers.iter().any(|p| p.id == "e2b" && p.signed_in()));
    let cmds = commands(&mut rxs[0]);
    assert_eq!(only(&cmds, "cloud.box.list").1, json!({"refresh": true}));
    // Esc closes the overview.
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn an_action_that_needs_a_sign_in_signs_in_and_repeats() {
    let (mut app, mut rxs) = opened();
    app.on_key(ch('j'));
    app.on_key(ch('j'));
    app.on_key(ch('p'));
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "cloud.box.suspend");
    reply_err(
        &mut app,
        0,
        req,
        "permission_denied",
        json!({"reason": "needs_auth", "provider": "e2b",
               "methods": [{"kind": "paste_token", "label": "API key"}]}),
    );
    assert!(screen(&app).contains("API key"));
    app.on_paste("k".into());
    app.on_key(named(NamedKey::Enter));
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "cloud.auth.set");
    reply(
        &mut app,
        0,
        req,
        json!({"provider": "e2b", "account": "team"}),
    );
    let cmds = commands(&mut rxs[0]);
    assert_eq!(only(&cmds, "cloud.box.suspend").1, json!({"box": "e2b/x1"}));
}

#[test]
fn tab_focuses_a_provider_and_s_signs_in_to_it() {
    let (mut app, _rxs) = opened();
    // The first sandbox (Sprites) is selected, so Sprites is focused.
    assert!(screen(&app).contains("▸ Sprites"), "{}", screen(&app));
    app.on_key(named(NamedKey::Tab));
    let v = app.ux.sandboxes.as_ref().unwrap();
    assert_eq!(v.focused().as_deref(), Some("e2b"));
    assert_eq!(v.selected().map(|b| b.provider.as_str()), Some("e2b"));
    assert!(screen(&app).contains("▸ E2B"), "{}", screen(&app));
    app.on_key(ch('s'));
    let v = app.ux.sandboxes.as_ref().unwrap();
    match &v.stage {
        Stage::Auth(a) => assert_eq!(a.provider, "e2b"),
        _ => panic!("expected the sign-in stage"),
    }
}

#[test]
fn tab_reaches_a_provider_without_sandboxes() {
    let (mut app, _rxs) = opened();
    // Shift-tab from Sprites wraps around to the last provider.
    let mut back = named(NamedKey::Tab);
    back.mods = vk_proto::input::Mods::SHIFT;
    app.on_key(back);
    let v = app.ux.sandboxes.as_ref().unwrap();
    let last = v.provider_order().last().cloned();
    assert_eq!(v.focused(), last);
}
