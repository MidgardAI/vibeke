//! Cloud send and bring-back tests: the masked token field and its bracketed paste, the Auth
//! stage's methods, the stages of a send (provider, sign-in, sandbox, confirm, job), the retry
//! after a `needs_auth` error, bringing a pane back, and the job progress in the tab bar.

use super::*;
use crate::drafts::tests::{ch, commands, ctl, fleet, named, only, reply, reply_err, screen};
use vk_proto::render::PushedEvent;

fn methods() -> Vec<AuthMethod> {
    vec![
        AuthMethod::PasteToken {
            label: "Token".into(),
            help_url: Some("https://sprites.dev/account".into()),
            hint: None,
        },
        AuthMethod::Import {
            source: "fly".into(),
            label: "Import from fly".into(),
        },
        AuthMethod::Env {
            var: "SPRITES_TOKEN".into(),
        },
    ]
}

fn providers() -> Value {
    json!({"providers": [
        {"id": "sprites", "label": "Sprites", "caps": {}, "default": true,
         "auth": {"state": "missing"},
         "methods": [
            {"kind": "paste_token", "label": "Token", "help_url": "https://sprites.dev/account",
             "hint": "from your account page"},
            {"kind": "import", "source": "fly", "label": "Import from fly"},
            {"kind": "env", "var": "SPRITES_TOKEN"}]},
        {"id": "e2b", "label": "E2B", "caps": {}, "default": false,
         "auth": {"state": "ok", "source": "keychain", "account": "acme"}, "methods": []}
    ]})
}

fn job(state: &str, done: u64) -> Value {
    json!({"id": "j1", "direction": "send", "pane": "p1",
           "from": {"kind": "local"}, "to": {"kind": "cloud", "provider": "sprites"},
           "state": state, "progress": {"done": done, "total": 4},
           "created_at": 1, "updated_at": 2})
}

fn push(app: &mut App, kind: &str, subject: Value, data: Value) {
    let ev = PushedEvent {
        seq: 1,
        kind: kind.into(),
        json: json!({"seq": 1, "type": kind, "subject": subject, "data": data}).to_string(),
    };
    crate::push::on_events(app, 0, vec![ev], false);
}

fn stage_is_auth(app: &App) -> bool {
    matches!(
        app.ux.cloud.flow.as_ref().map(|f| &f.stage),
        Some(Stage::Auth(_))
    )
}

#[test]
fn the_token_field_shows_bullets_and_takes_a_paste() {
    let mut a = AuthState::new("sprites", "Sprites", methods());
    assert!(a.on_token_row());
    for c in "sk-secret".chars() {
        assert_eq!(a.key(&ch(c)), AuthOut::None);
    }
    assert_eq!(masked(&a.token, 80), "•".repeat(9));
    assert_eq!(masked(&a.token, 4), "••••");
    // A paste drops whitespace and newlines.
    a.paste("  abc\n def ");
    assert_eq!(a.token, "sk-secretabcdef");
    a.key(&named(NamedKey::Backspace));
    assert_eq!(a.token, "sk-secretabcde");
    // Enter submits the trimmed token; an empty field asks for one.
    assert_eq!(
        a.key(&named(NamedKey::Enter)),
        AuthOut::SetToken("sk-secretabcde".into())
    );
    a.key(&ctl('u'));
    assert_eq!(a.token, "");
    assert_eq!(a.key(&named(NamedKey::Enter)), AuthOut::None);
    assert!(a.error.is_some());
}

#[test]
fn auth_methods_import_open_and_env() {
    let mut a = AuthState::new("sprites", "Sprites", methods());
    // ctrl+o opens the token page from the field; typed `o` is part of the token.
    assert_eq!(
        a.key(&ctl('o')),
        AuthOut::Open("https://sprites.dev/account".into())
    );
    a.key(&ch('o'));
    assert_eq!(a.token, "o");
    // Down: the import row; there `o` opens the page and enter imports.
    a.key(&named(NamedKey::Down));
    assert!(!a.on_token_row());
    assert_eq!(
        a.key(&ch('o')),
        AuthOut::Open("https://sprites.dev/account".into())
    );
    assert_eq!(
        a.key(&named(NamedKey::Enter)),
        AuthOut::Import("fly".into())
    );
    // The env row is a note: the selection skips it.
    a.key(&named(NamedKey::Down));
    assert_eq!(a.sel, 0);
    assert_eq!(a.key(&named(NamedKey::Escape)), AuthOut::Cancel);
    // Busy: nothing but Esc counts.
    a.busy = Some("checking".into());
    assert_eq!(a.key(&ch('x')), AuthOut::None);
    assert_eq!(a.key(&named(NamedKey::Escape)), AuthOut::Cancel);
}

#[test]
fn a_send_signs_in_picks_a_sandbox_and_follows_the_job() {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    app.action("cloud_send", None);
    assert!(matches!(app.mode, Mode::Popup(Popup::CloudSend)));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "cloud.providers");
    assert_eq!(p, json!({}));
    reply(&mut app, 0, req, providers());
    let s = screen(&app);
    assert!(s.contains("Sprites"), "{s}");
    assert!(s.contains("not signed in"), "{s}");
    assert!(s.contains("signed in as acme"), "{s}");
    // The signed-in provider is preselected; Sprites needs a sign-in first.
    assert!(matches!(
        app.ux.cloud.flow.as_ref().unwrap().stage,
        Stage::PickProvider { sel: 1 }
    ));
    app.on_key(ch('k'));
    app.on_key(named(NamedKey::Enter));
    assert!(stage_is_auth(&app));
    assert!(screen(&app).contains("Sign in to Sprites"));
    // Type some, paste the rest: the screen shows bullets only.
    for c in "sk-".chars() {
        app.on_key(ch(c));
    }
    app.on_paste("topsecret".into());
    let s = screen(&app);
    assert!(s.contains("••••••••••••"), "{s}");
    assert!(!s.contains("topsecret") && !s.contains("sk-"), "{s}");
    app.on_key(named(NamedKey::Enter));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "cloud.auth.set");
    assert_eq!(p, json!({"provider": "sprites", "token": "sk-topsecret"}));
    // The field is empty once the token is sent.
    match &app.ux.cloud.flow.as_ref().unwrap().stage {
        Stage::Auth(a) => assert_eq!(a.token, ""),
        s => panic!("{s:?}"),
    }
    reply(
        &mut app,
        0,
        req,
        json!({"provider": "sprites", "account": "acme"}),
    );
    // Signed in: the task's sandboxes are listed; none, so the confirm step.
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "cloud.box.list");
    assert_eq!(p, json!({"provider": "sprites", "refresh": false}));
    reply(&mut app, 0, req, json!({"boxes": [], "errors": []}));
    let s = screen(&app);
    assert!(s.contains("a new sandbox on Sprites"), "{s}");
    app.on_key(ch('i'));
    app.on_key(named(NamedKey::Enter));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "cloud.move");
    assert_eq!(
        p,
        json!({"pane": "p1", "to": {"kind": "cloud", "provider": "sprites"}, "interrupt": true})
    );
    reply(&mut app, 0, req, job("creating", 1));
    let s = screen(&app);
    assert!(s.contains("creating the sandbox"), "{s}");
    assert!(s.contains("25%"), "{s}");
    assert_eq!(status(&app).as_deref(), Some("☁ sprites creating 25%"));
    // The job's events drive the rest.
    push(
        &mut app,
        "cloud.job",
        json!({"job": "j1"}),
        job("uploading", 3),
    );
    assert_eq!(status(&app).as_deref(), Some("☁ sprites uploading 75%"));
    let mut done = job("done", 4);
    done["result"] = json!({"pane": "p2", "box": "sprites/b1"});
    push(&mut app, "cloud.job", json!({"job": "j1"}), done);
    assert_eq!(status(&app), None);
    let s = screen(&app);
    assert!(
        s.contains("New pane: ") && s.contains("(enter opens it)"),
        "{s}"
    );
    // Enter opens the new pane.
    app.on_key(named(NamedKey::Enter));
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.machines[0].focus.pane.as_deref(), Some("p2"));
}

#[test]
fn needs_auth_signs_in_and_repeats_the_call_once() {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    app.action("cloud_send", None);
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "cloud.providers");
    reply(&mut app, 0, req, providers());
    // E2B is signed in, so the sandboxes are listed at once.
    app.on_key(named(NamedKey::Enter));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "cloud.box.list");
    assert_eq!(p["provider"], "e2b");
    reply_err(
        &mut app,
        0,
        req,
        "permission_denied",
        json!({"reason": "needs_auth", "provider": "e2b",
               "methods": [{"kind": "paste_token", "label": "API key"}]}),
    );
    assert!(stage_is_auth(&app));
    assert!(screen(&app).contains("API key"));
    app.on_paste("e2b-key".into());
    app.on_key(named(NamedKey::Enter));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "cloud.auth.set");
    assert_eq!(p, json!({"provider": "e2b", "token": "e2b-key"}));
    reply(
        &mut app,
        0,
        req,
        json!({"provider": "e2b", "account": "acme"}),
    );
    // The original call is made again.
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "cloud.box.list");
    assert_eq!(p["provider"], "e2b");
    // A second `needs_auth` is an error, not another sign-in.
    reply_err(
        &mut app,
        0,
        req,
        "permission_denied",
        json!({"reason": "needs_auth", "provider": "e2b"}),
    );
    assert!(!stage_is_auth(&app));
    assert!(
        app.ux
            .cloud
            .flow
            .as_ref()
            .unwrap()
            .error
            .as_deref()
            .unwrap()
            .contains("still not signed in")
    );
    assert!(commands(&mut rxs[0]).is_empty());
}

#[test]
fn an_existing_sandbox_of_the_task_is_offered() {
    let (mut app, mut rxs) = fleet();
    app.machines[0].model.workspaces[0].task = Some("T1".into());
    commands(&mut rxs[0]);
    app.action("cloud_send", None);
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "cloud.providers");
    reply(&mut app, 0, req, providers());
    app.on_key(named(NamedKey::Enter));
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "cloud.box.list");
    let boxv = |id: &str, task: &str, own: &str| {
        json!({"box": format!("e2b/{id}"), "provider": "e2b", "id": id, "name": format!("vk-{id}"),
               "state": "running", "ownership": own, "task": task, "panes": ["p2"]})
    };
    reply(
        &mut app,
        0,
        req,
        json!({"boxes": [boxv("a1", "T1", "attached"), boxv("a2", "T2", "attached"),
                         boxv("a3", "T1", "orphaned")], "errors": []}),
    );
    let f = app.ux.cloud.flow.as_ref().unwrap();
    assert_eq!(f.boxes.len(), 1, "only this task's attached sandbox");
    assert!(matches!(f.stage, Stage::PickBox { sel: 0 }));
    let s = screen(&app);
    assert!(s.contains("vk-a1") && s.contains("New sandbox"), "{s}");
    app.on_key(named(NamedKey::Enter));
    app.on_key(named(NamedKey::Enter));
    let cmds = commands(&mut rxs[0]);
    let (_, p) = only(&cmds, "cloud.move");
    assert_eq!(
        p["to"],
        json!({"kind": "cloud", "provider": "e2b", "box": "e2b/a1"})
    );
    // A failed start stays on the confirm step with the error.
    let req = cmds.iter().find(|c| c.1 == "cloud.move").unwrap().0;
    reply_err(
        &mut app,
        0,
        req,
        "unavailable",
        json!({"reason": "provider_down"}),
    );
    let s = screen(&app);
    assert!(s.contains("provider_down"), "{s}");
    assert!(matches!(
        app.ux.cloud.flow.as_ref().unwrap().stage,
        Stage::Confirm
    ));
}

#[test]
fn bring_back_picks_this_host_or_a_peer() {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    app.action("cloud_bring_back", None);
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "handoff.peers");
    reply(
        &mut app,
        0,
        req,
        json!({"peers": [{"id": "pr1", "name": "marvin", "owner": "self"}]}),
    );
    let s = screen(&app);
    assert!(s.contains("This host") && s.contains("marvin"), "{s}");
    app.on_key(named(NamedKey::Enter));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "cloud.move");
    assert_eq!(p, json!({"pane": "p1", "to": {"kind": "local"}}));
    let mut j = job("exporting", 0);
    j["direction"] = json!("bring_back");
    j["from"] = json!({"kind": "cloud", "provider": "sprites"});
    j["to"] = json!({"kind": "local"});
    reply(&mut app, 0, req, j);
    assert!(screen(&app).contains("exporting the work"));
    // x cancels the running job.
    app.on_key(ch('x'));
    let cmds = commands(&mut rxs[0]);
    assert_eq!(only(&cmds, "cloud.cancel").1, json!({"id": "j1"}));
    // A peer.
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(app.mode, Mode::Normal));
    app.action("cloud_bring_back", None);
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "handoff.peers");
    reply(
        &mut app,
        0,
        req,
        json!({"peers": [{"id": "pr1", "name": "marvin", "owner": "self"}]}),
    );
    app.on_key(ch('j'));
    app.on_key(named(NamedKey::Enter));
    let cmds = commands(&mut rxs[0]);
    assert_eq!(
        only(&cmds, "cloud.move").1,
        json!({"pane": "p1", "to": {"kind": "peer", "peer": "pr1"}})
    );
}

#[test]
fn a_failed_job_for_lack_of_a_sign_in_signs_in_and_retries_once() {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    app.action("cloud_send", None);
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "cloud.providers");
    reply(&mut app, 0, req, providers());
    app.on_key(named(NamedKey::Enter));
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "cloud.box.list");
    reply(&mut app, 0, req, json!({"boxes": [], "errors": []}));
    app.on_key(named(NamedKey::Enter));
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "cloud.move");
    reply(&mut app, 0, req, job("creating", 0));
    let mut failed = job("failed", 0);
    failed["error"] = json!({"kind": "permission_denied", "message": "sign in",
        "details": {"reason": "needs_auth", "provider": "e2b",
                    "methods": [{"kind": "paste_token", "label": "API key"}]}});
    push(&mut app, "cloud.job", json!({"job": "j1"}), failed.clone());
    assert!(stage_is_auth(&app));
    // Signing in starts the same move once more.
    app.on_paste("e2b-key".into());
    app.on_key(named(NamedKey::Enter));
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "cloud.auth.set");
    reply(
        &mut app,
        0,
        req,
        json!({"provider": "e2b", "account": "acme"}),
    );
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "cloud.move");
    let mut second = job("creating", 0);
    second["id"] = json!("j2");
    second["updated_at"] = json!(3);
    reply(&mut app, 0, req, second);
    // That job fails the same way: an error, not another sign-in (no endless prompts).
    let mut again = failed;
    again["id"] = json!("j2");
    again["updated_at"] = json!(4);
    push(&mut app, "cloud.job", json!({"job": "j2"}), again);
    assert!(!stage_is_auth(&app));
    let f = app.ux.cloud.flow.as_ref().unwrap();
    assert!(f.error.as_deref().unwrap().contains("still not signed in"));
    assert!(!f.retried);
    assert!(commands(&mut rxs[0]).is_empty());
}

#[test]
fn cloud_events_are_subscribed_and_the_polling_waits_for_a_job() {
    for t in ["cloud.job", "cloud.box.changed", "cloud.auth.changed"] {
        assert!(crate::push::TYPES.contains(&t), "{t}");
    }
    let j = Job::from_value(&job("waiting_turn", 0)).unwrap();
    assert!(j.active());
    assert_eq!(j.status(), "☁ sprites waiting turn 0%");
    assert_eq!(j.percent(), Some(0));
    let (mut app, _rxs) = fleet();
    let mut d = crate::deadline::Deadlines::default();
    deadlines(&app, Instant::now(), &mut d);
    assert!(d.is_empty());
    tick(&mut app);
}

#[test]
fn closing_the_popup_leaves_a_running_job_in_the_tab_bar() {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    push(
        &mut app,
        "cloud.job",
        json!({"job": "j1"}),
        job("creating", 2),
    );
    assert_eq!(status(&app).as_deref(), Some("☁ sprites creating 50%"));
    assert!(screen(&app).contains("☁ sprites creating 50%"));
    let mut other = job("bootstrapping", 0);
    other["id"] = json!("j2");
    push(&mut app, "cloud.job", json!({"job": "j2"}), other);
    let s = status(&app).unwrap();
    assert!(s.contains("creating") && s.contains("bootstrapping"), "{s}");
}

// ---- review, shared checkout, input while moving ---------------------------------------------

fn review_job(state: &str, confirmed: bool) -> Value {
    let mut j = job(state, 0);
    j["direction"] = json!("bring_back");
    j["from"] = json!({"kind": "cloud", "provider": "sprites"});
    j["to"] = json!({"kind": "local"});
    j["confirmed"] = json!(confirmed);
    j["result"] = json!({"review": {
        "warning": "The sandbox ran with full permissions.",
        "files": [{"path": "src/login.rs", "added": 3, "removed": 1}],
        "files_total": 4, "added": 10, "removed": 2,
        "untracked": ["notes.txt"], "untracked_total": 1,
        "skipped": [{"path": ".env", "reason": "secret"}],
        "harness": "claude", "transcript": true}});
    j
}

fn stage_is_review(app: &App) -> bool {
    matches!(
        app.ux.cloud.flow.as_ref().map(|f| &f.stage),
        Some(Stage::Review { .. })
    )
}

/// Start a bring-back of `p1` to this host and let its job reach the review.
fn bring_back_to_review(
    app: &mut App,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<vk_proto::render::ClientFrame>,
) {
    commands(rx);
    app.action("cloud_bring_back", None);
    let cmds = commands(rx);
    let (req, _) = only(&cmds, "handoff.peers");
    reply(app, 0, req, json!({"peers": []}));
    app.on_key(named(NamedKey::Enter));
    let cmds = commands(rx);
    let (req, _) = only(&cmds, "cloud.move");
    reply(app, 0, req, review_job("exporting", false));
    push(
        app,
        "cloud.job",
        json!({"job": "j1"}),
        review_job("awaiting_confirmation", false),
    );
}

#[test]
fn a_review_lists_the_changes_and_what_stays_behind() {
    let r = Review::from_value(&review_job("awaiting_confirmation", false)["result"]["review"])
        .unwrap();
    let l = r.lines().join("\n");
    assert!(l.contains("4 changed files (+10 −2), 1 new file"), "{l}");
    assert!(l.contains("src/login.rs  +3 −1"), "{l}");
    assert!(l.contains("… 3 more changed files"), "{l}");
    assert!(l.contains(".env (secret)"), "{l}");
    assert!(l.contains("resumes claude"), "{l}");
    assert!(Review::from_value(&Value::Null).is_none());
}

#[test]
fn a_bring_back_stops_for_review_and_applies_on_a() {
    let (mut app, mut rxs) = fleet();
    bring_back_to_review(&mut app, &mut rxs[0]);
    assert!(stage_is_review(&app));
    let s = screen(&app);
    assert!(s.contains("review the changes"), "{s}");
    assert!(
        s.contains("full permissions") && s.contains("src/login.rs"),
        "{s}"
    );
    // Enter decides nothing here.
    app.on_key(named(NamedKey::Enter));
    assert!(commands(&mut rxs[0]).is_empty());
    app.on_key(ch('a'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "cloud.confirm");
    assert_eq!(p, json!({"id": "j1", "code_only": false}));
    reply(
        &mut app,
        0,
        req,
        json!({"job": review_job("awaiting_confirmation", true)}),
    );
    // Confirmed: the popup follows the job again.
    assert!(!stage_is_review(&app));
    assert!(screen(&app).contains("applying the changes"));
}

#[test]
fn code_only_and_cancel_from_a_review_opened_later() {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    // The review arrives while no popup is open: a toast says how to open it.
    push(
        &mut app,
        "cloud.job",
        json!({"job": "j1"}),
        review_job("awaiting_confirmation", false),
    );
    assert!(matches!(app.mode, Mode::Normal));
    assert!(
        app.toasts
            .iter()
            .any(|t| t.text.contains("waits for your review")),
        "{:?}",
        app.toasts.iter().map(|t| &t.text).collect::<Vec<_>>()
    );
    assert!(cloud_status_text(&app).contains("review waiting"));
    // Bring back from cloud on that pane opens the review.
    app.action("cloud_bring_back", None);
    assert!(stage_is_review(&app));
    assert!(commands(&mut rxs[0]).iter().all(|c| c.1 != "handoff.peers"));
    app.on_key(ch('c'));
    let cmds = commands(&mut rxs[0]);
    assert_eq!(
        only(&cmds, "cloud.confirm").1,
        json!({"id": "j1", "code_only": true})
    );
    // Esc leaves it waiting; x in a reopened review cancels the move.
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(app.mode, Mode::Normal));
    app.action("cloud_bring_back", None);
    app.on_key(ch('x'));
    let cmds = commands(&mut rxs[0]);
    assert_eq!(only(&cmds, "cloud.cancel").1, json!({"id": "j1"}));
}

fn cloud_status_text(app: &App) -> String {
    status(app).unwrap_or_default()
}

#[test]
fn review_keys_wait_when_the_review_appears_on_its_own() {
    let (mut app, mut rxs) = fleet();
    app.decide.delay = crate::decide::ARM_DELAY;
    bring_back_to_review(&mut app, &mut rxs[0]);
    assert!(stage_is_review(&app));
    // An `a` typed right after the review replaced the progress decides nothing.
    app.on_key(ch('a'));
    assert!(commands(&mut rxs[0]).iter().all(|c| c.1 != "cloud.confirm"));
    assert!(screen(&app).contains(crate::decide::WAIT_HINT));
}

#[test]
fn a_shared_checkout_offers_to_send_a_new_task() {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    app.action("cloud_send", None);
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "cloud.providers");
    reply(&mut app, 0, req, providers());
    app.on_key(named(NamedKey::Enter));
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "cloud.box.list");
    reply(&mut app, 0, req, json!({"boxes": [], "errors": []}));
    app.on_key(named(NamedKey::Enter));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "cloud.move");
    assert!(p.get("new_task").is_none());
    reply_err(
        &mut app,
        0,
        req,
        "conflict",
        json!({"reason": "shared_checkout", "why": "the repository's main checkout", "retry": {"new_task": true}}),
    );
    let s = screen(&app);
    assert!(s.contains("create a task and send"), "{s}");
    app.on_key(ch('t'));
    let cmds = commands(&mut rxs[0]);
    let (_, p) = only(&cmds, "cloud.move");
    assert_eq!(p["new_task"], true);
}

#[test]
fn input_refused_while_the_agent_moves_says_so() {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    // Still waiting for the turn: input goes on, nothing to say.
    push(
        &mut app,
        "cloud.job",
        json!({"job": "j1"}),
        job("waiting_turn", 0),
    );
    app.on_frame(
        0,
        vk_proto::render::ServerFrame::InputAck {
            input_id: 1,
            status: vk_proto::render::AckStatus::Rejected,
        },
    );
    assert!(
        !app.toasts
            .iter()
            .any(|t| t.text.contains("Moving to the cloud"))
    );
    push(
        &mut app,
        "cloud.job",
        json!({"job": "j1"}),
        job("exporting", 1),
    );
    app.on_frame(
        0,
        vk_proto::render::ServerFrame::InputAck {
            input_id: 2,
            status: vk_proto::render::AckStatus::Rejected,
        },
    );
    assert!(
        app.toasts
            .iter()
            .any(|t| t.text.contains("Moving to the cloud"))
    );
    assert!(moving(&app, 0, "p1"));
    assert!(!moving(&app, 0, "p2"));
}
