//! Handoff tests: the accept overlay (summary, choosing a clone, Browse… and the worktree picker,
//! the branch field, toggles, the params `handoff.accept` gets, `repo_mismatch`, progress phases,
//! focusing the imported pane, Retry resume, decline), the inbox entry, the send flow's params
//! and the progress shown for a send; the key bindings, the `⇣N` badge, the pane menu and
//! Handoff details, and pairing two machines before a send.

use super::*;
use crate::app::App;
use crate::drafts::tests::{ch, commands, ctl, fleet, named, only, reply, reply_err, screen, typ};
use crate::path_picker::DirEntry;
use std::path::PathBuf;
use tokio::sync::mpsc::UnboundedReceiver;
use vk_proto::render::ClientFrame;

/// Folders by absolute path; names ending in `*` are git repositories.
struct FakeDirs(BTreeMap<&'static str, Vec<&'static str>>);

impl DirSource for FakeDirs {
    fn list(&self, dir: &Path, _dot: bool) -> Option<Vec<DirEntry>> {
        let key = dir.to_str()?.trim_end_matches('/');
        Some(
            self.0
                .get(key)?
                .iter()
                .map(|n| DirEntry {
                    name: n.trim_end_matches('*').to_string(),
                    git_repo: n.ends_with('*'),
                })
                .collect(),
        )
    }
    fn home(&self) -> Option<PathBuf> {
        Some(PathBuf::from("/home/u"))
    }
}

fn fake_dirs() -> Arc<dyn DirSource> {
    Arc::new(FakeDirs(BTreeMap::from([
        ("/home/u", vec!["code"]),
        ("/home/u/code", vec!["other*", "vibeke*", "scratch"]),
    ])))
}

fn incoming(id: &str, state: &str) -> Value {
    json!({
        "id": id,
        "from": {"host": "marvin", "owner": "teammate", "user": "Ann <ann@example.com>"},
        "manifest": {
            "source_host": "marvin", "repo_name": "vibeke",
            "origin": "git@github.com:acme/vibeke.git", "branch": "feature/x",
            "head": "a1b2c3d4e5f6", "harness": "claude", "session_id": "s-1", "cwd_rel": "",
            "skipped": [{"path": ".env", "reason": "secret"},
                        {"path": "big.bin", "reason": "larger than 5 MiB"}],
            "last_message": "Tests pass;\nnext: wire the CLI.", "untracked": 3,
            "transcript": true, "redactions": 0, "created_at": 1
        },
        "size": 2048, "bundle_path": "/state/handoffs/in/x.tar.zst", "state": state,
        "error": null, "result": null,
        "created_at_ms": now_ms() - 60_000, "updated_at_ms": now_ms(),
        "expires_at_ms": now_ms() + 86_400_000
    })
}

fn get_result(id: &str, repos: &[&str]) -> Value {
    let repo = repos.first().copied();
    json!({
        "incoming": incoming(id, "pending"),
        "suggested": {
            "repos": repos,
            "repo": repo,
            "worktree_path": repo.map(|_| "/home/u/code/vibeke-handoff-feature-x"),
            "branch": "handoff/feature-x"
        }
    })
}

fn event(kind: &str, id: &str, data: Value) -> Value {
    json!({"seq": 1, "type": kind, "subject": {"incoming": id}, "data": data})
}

/// One machine with handoff `h1` waiting (as the `handoff.incoming` event delivers it), the
/// overlay opened from its inbox item and `handoff.incoming.get` answered.
fn opened(repos: &[&str]) -> (App, Vec<UnboundedReceiver<ClientFrame>>) {
    let (mut app, mut rxs) = fleet();
    app.ux.handoff.dirs = Some(fake_dirs());
    on_event(
        &mut app,
        0,
        "handoff.incoming",
        &event(
            "handoff.incoming",
            "h1",
            json!({"incoming": incoming("h1", "pending")}),
        ),
    );
    let items = crate::inbox::view(&app).items;
    let it = items
        .iter()
        .find(|i| i.key.kind == "handoff")
        .expect("a handoff item in the inbox")
        .clone();
    assert_eq!(it.key.id, "h1");
    assert_eq!(it.title, "Incoming handoff from marvin: feature/x");
    commands(&mut rxs[0]);
    crate::inbox::open_item(&mut app, &it, false);
    assert!(matches!(app.mode, Mode::Popup(Popup::HandoffAccept)));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "handoff.incoming.get");
    assert_eq!(p, json!({"id": "h1"}));
    reply(&mut app, 0, req, get_result("h1", repos));
    (app, rxs)
}

fn accept_params(rx: &mut UnboundedReceiver<ClientFrame>) -> (u64, Value) {
    let cmds = commands(rx);
    only(&cmds, "handoff.accept")
}

fn press(app: &mut App, n: NamedKey, times: usize) {
    for _ in 0..times {
        app.on_key(named(n));
    }
}

fn focus(app: &App) -> Row {
    app.ux.handoff.accept.as_ref().unwrap().focus
}

#[test]
fn overlay_shows_what_arrived_and_accepts_the_suggestion() {
    let (mut app, mut rxs) = opened(&["/home/u/code/vibeke"]);
    let s = screen(&app);
    assert!(
        s.contains("from     marvin (teammate, Ann <ann@example.com>)"),
        "{s}"
    );
    assert!(
        s.contains("branch   feature/x @ a1b2c3d · 3 untracked file(s)"),
        "{s}"
    );
    assert!(s.contains("claude · the conversation resumes"), "{s}");
    assert!(s.contains("bring your own: .env"), "{s}");
    assert!(s.contains("not carried: 1 other file(s)"), "{s}");
    assert!(s.contains("“Tests pass; next: wire the CLI.”"), "{s}");
    assert!(
        s.contains("› (•) /home/u/code/vibeke") || s.contains("(•) /home/u/code/vibeke"),
        "{s}"
    );
    // A fresh clone never lands on the clone that is already there.
    assert!(s.contains("Clone to…  /home/u/code/vibeke-2"), "{s}");
    assert!(
        s.contains("Worktree   /home/u/code/vibeke-handoff-feature-x"),
        "{s}"
    );
    assert!(s.contains("Branch     handoff/feature-x"), "{s}");
    assert!(s.contains("[x] Resume agent (claude)"), "{s}");
    assert!(s.contains("[ ] Trust mise config"), "{s}");
    assert!(s.contains("[> Accept <]"), "{s}");
    // Enter on the focused Accept button.
    assert_eq!(focus(&app), Row::Accept);
    app.on_key(named(NamedKey::Enter));
    let (_, p) = accept_params(&mut rxs[0]);
    assert_eq!(
        p,
        json!({"id": "h1", "repo": {"path": "/home/u/code/vibeke"},
               "worktree_path": "/home/u/code/vibeke-handoff-feature-x", "start_agent": true}),
        "the suggested branch is left to the server"
    );
    assert!(screen(&app).contains("⏳ accepting…"));
    // Busy: keys don't send anything else.
    app.on_key(ch('a'));
    assert!(commands(&mut rxs[0]).is_empty());
}

#[test]
fn browse_worktree_branch_toggles_mismatch_then_imported_pane_gets_focus() {
    let (mut app, mut rxs) = opened(&["/home/u/code/vibeke"]);
    // Accept → Browse… (Repo 0, Browse, Clone to, Worktree, Branch, Resume, mise, direnv, Accept).
    press(&mut app, NamedKey::Up, 7);
    assert_eq!(focus(&app), Row::Browse);
    app.on_key(named(NamedKey::Enter));
    {
        let a = app.ux.handoff.accept.as_ref().unwrap();
        let (what, p) = a.picker.as_ref().expect("a picker for Browse…");
        assert_eq!(*what, PickFor::Browse);
        assert_eq!(
            p.input, "/home/u/code/",
            "starts next to the suggested clone"
        );
    }
    let s = screen(&app);
    assert!(s.contains("other/"), "{s}");
    assert!(s.contains("git"), "{s}");
    typ(&mut app, "oth");
    app.on_key(named(NamedKey::Tab));
    assert_eq!(
        app.ux
            .handoff
            .accept
            .as_ref()
            .unwrap()
            .picker
            .as_ref()
            .unwrap()
            .1
            .input,
        "/home/u/code/other/"
    );
    app.on_key(named(NamedKey::Enter));
    {
        let a = app.ux.handoff.accept.as_ref().unwrap();
        assert!(a.picker.is_none());
        assert_eq!(a.repo, RepoSel::Browse);
        assert_eq!(a.browse_path.as_deref(), Some("/home/u/code/other"));
        assert_eq!(
            a.worktree, None,
            "another clone: the server places the worktree"
        );
    }
    assert!(screen(&app).contains("(next to the repository)"));
    // Worktree: picked under the new clone's folder.
    press(&mut app, NamedKey::Down, 2);
    assert_eq!(focus(&app), Row::Worktree);
    app.on_key(named(NamedKey::Enter));
    assert_eq!(
        app.ux
            .handoff
            .accept
            .as_ref()
            .unwrap()
            .picker
            .as_ref()
            .unwrap()
            .1
            .input,
        "/home/u/code/"
    );
    // A paste lands in the picker.
    app.on_paste("wt-x".into());
    app.on_key(named(NamedKey::Enter));
    assert_eq!(
        app.ux.handoff.accept.as_ref().unwrap().worktree.as_deref(),
        Some("/home/u/code/wt-x")
    );
    // The branch is typed in place ('a' and 'd' are letters there, not Accept / Decline).
    app.on_key(named(NamedKey::Down));
    assert_eq!(focus(&app), Row::Branch);
    app.on_key(ctl('u'));
    typ(&mut app, "feature-x-here");
    assert!(commands(&mut rxs[0]).is_empty());
    assert!(screen(&app).contains("Branch     feature-x-here▏"));
    // Resume off, trust mise on.
    app.on_key(named(NamedKey::Down));
    app.on_key(named(NamedKey::Space));
    app.on_key(named(NamedKey::Down));
    app.on_key(ch(' '));
    assert_eq!(focus(&app), Row::TrustMise);
    app.on_key(ch('a'));
    let (req, p) = accept_params(&mut rxs[0]);
    assert_eq!(
        p,
        json!({"id": "h1", "repo": {"path": "/home/u/code/other"},
               "worktree_path": "/home/u/code/wt-x", "branch": "feature-x-here",
               "start_agent": false, "trust": ["mise"]})
    );
    // Progress from handoff.updated.
    on_event(
        &mut app,
        0,
        "handoff.updated",
        &event(
            "handoff.updated",
            "h1",
            json!({"incoming": incoming("h1", "importing"), "phase": "importing"}),
        ),
    );
    assert!(screen(&app).contains("⏳ importing the work…"));
    // Not a clone of the origin: the overlay stays, with the clone's remotes.
    reply_err(
        &mut app,
        0,
        req,
        "conflict",
        json!({"reason": "repo_mismatch", "repo": "/home/u/code/other",
               "origin": "git@github.com:acme/vibeke.git",
               "remotes": ["git@github.com:acme/other.git"]}),
    );
    assert!(matches!(app.mode, Mode::Popup(Popup::HandoffAccept)));
    let s = screen(&app);
    assert!(s.contains("that clone is a different repository"), "{s}");
    assert!(s.contains("git@github.com:acme/other.git"), "{s}");
    assert!(!s.contains("⏳"), "{s}");
    // Back to the suggested clone; the picked worktree path stays.
    press(&mut app, NamedKey::Up, 6);
    assert_eq!(focus(&app), Row::Repo(0));
    app.on_key(named(NamedKey::Space));
    assert!(app.ux.handoff.accept.as_ref().unwrap().mismatch.is_none());
    app.on_key(ch('a'));
    let (req, p) = accept_params(&mut rxs[0]);
    assert_eq!(p["repo"], json!({"path": "/home/u/code/vibeke"}));
    assert_eq!(p["worktree_path"], "/home/u/code/wt-x");
    let mut done = incoming("h1", "imported");
    done["result"] = json!({"repo": "/home/u/code/vibeke", "worktree": "/home/u/code/wt-x",
        "branch": "feature-x-here", "pane": "p9", "workspace": "W9", "not_written": [],
        "trust": [{"tool": "mise", "status": "trusted"}], "resumed": false});
    reply(&mut app, 0, req, json!({"incoming": done}));
    assert!(matches!(app.mode, Mode::Normal));
    assert!(app.ux.handoff.accept.is_none());
    let mut focused = None;
    while let Ok(f) = rxs[0].try_recv() {
        if let ClientFrame::Focus { pane } = f {
            focused = Some(pane);
        }
    }
    assert_eq!(focused.as_deref(), Some("p9"));
    assert!(
        app.toasts
            .iter()
            .any(|t| t.text == "⇣ imported feature-x-here into /home/u/code/wt-x")
    );
    // Imported: no longer in the inbox.
    assert!(
        !crate::inbox::view(&app)
            .items
            .iter()
            .any(|i| i.key.kind == "handoff")
    );
}

#[test]
fn without_a_clone_it_offers_a_fresh_clone() {
    let (mut app, mut rxs) = opened(&[]);
    let a = app.ux.handoff.accept.as_ref().unwrap();
    assert_eq!(a.repo, RepoSel::CloneTo);
    assert_eq!(a.clone_to, "~/code/vibeke");
    assert_eq!(a.rows()[0], Row::Browse);
    // Space on an unpicked Browse… opens its picker instead of choosing nothing; esc drops it.
    press(&mut app, NamedKey::Up, 7);
    assert_eq!(focus(&app), Row::Browse);
    app.on_key(named(NamedKey::Space));
    assert!(app.ux.handoff.accept.as_ref().unwrap().picker.is_some());
    assert_eq!(
        app.ux.handoff.accept.as_ref().unwrap().repo,
        RepoSel::CloneTo
    );
    app.on_key(named(NamedKey::Escape));
    assert!(app.ux.handoff.accept.as_ref().unwrap().picker.is_none());
    assert!(matches!(app.mode, Mode::Popup(Popup::HandoffAccept)));
    // Clone to…: pick a folder.
    app.on_key(named(NamedKey::Down));
    assert_eq!(focus(&app), Row::CloneTo);
    app.on_key(named(NamedKey::Enter));
    app.on_key(ctl('u'));
    typ(&mut app, "~/code/vk");
    app.on_key(named(NamedKey::Enter));
    app.on_key(ch('a'));
    let (_, p) = accept_params(&mut rxs[0]);
    assert_eq!(
        p,
        json!({"id": "h1", "repo": {"clone_to": "/home/u/code/vk"}, "start_agent": true})
    );
    on_event(
        &mut app,
        0,
        "handoff.updated",
        &event(
            "handoff.updated",
            "h1",
            json!({"incoming": incoming("h1", "importing"), "phase": "cloning"}),
        ),
    );
    assert!(screen(&app).contains("⏳ cloning the repository…"));
}

#[test]
fn an_agent_that_did_not_start_offers_retry_resume() {
    let (mut app, mut rxs) = opened(&["/home/u/code/vibeke"]);
    app.on_key(ch('a'));
    let (req, _) = accept_params(&mut rxs[0]);
    let mut done = incoming("h1", "imported");
    done["result"] = json!({"worktree": "/w/x",
        "branch": "handoff/feature-x", "pane": "p9", "workspace": "W9", "not_written": ["a.txt"],
        "agent_error": {"code": -32000, "message": "claude: command not found"}});
    reply(&mut app, 0, req, json!({"incoming": done}));
    assert!(matches!(app.mode, Mode::Popup(Popup::HandoffAccept)));
    let s = screen(&app);
    assert!(s.contains("1 file(s) not written"), "{s}");
    assert!(
        s.contains("the agent did not start: claude: command not found"),
        "{s}"
    );
    assert!(s.contains("[r] retry resume"), "{s}");
    app.on_key(ch('r'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "handoff.resume");
    assert_eq!(p, json!({"id": "h1"}));
    // Still failing: the overlay says why.
    reply(
        &mut app,
        0,
        req,
        json!({"incoming": done, "agent_error": {"message": "no login"}}),
    );
    assert!(screen(&app).contains("the agent did not start: no login"));
    app.on_key(ch('r'));
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "handoff.resume");
    reply(
        &mut app,
        0,
        req,
        json!({"incoming": done, "run": {"id": "r9"}}),
    );
    assert!(matches!(app.mode, Mode::Normal));
    assert!(app.toasts.iter().any(|t| t.text == "▶ agent resumed"));
    // The handoffs list offers it too.
    app.action("handoffs", None);
    commands(&mut rxs[0]);
    assert!(screen(&app).contains("imported · agent did not start (r retries)"));
    app.on_key(ch('r'));
    let cmds = commands(&mut rxs[0]);
    assert_eq!(only(&cmds, "handoff.resume").1, json!({"id": "h1"}));
}

#[test]
fn decline_asks_first_and_leaves_the_inbox() {
    let (mut app, mut rxs) = opened(&["/home/u/code/vibeke"]);
    app.on_key(ch('d'));
    assert!(screen(&app).contains("Decline this handoff?"));
    // Any other key keeps it.
    app.on_key(named(NamedKey::Down));
    assert!(commands(&mut rxs[0]).is_empty());
    app.on_key(ch('d'));
    app.on_key(ch('d'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "handoff.decline");
    assert_eq!(p, json!({"id": "h1"}));
    reply(
        &mut app,
        0,
        req,
        json!({"incoming": incoming("h1", "declined")}),
    );
    assert!(app.ux.handoff.accept.is_none());
    assert!(!matches!(app.mode, Mode::Popup(Popup::HandoffAccept)));
    assert!(inbox_items(&app).is_empty());
    assert!(
        app.toasts
            .iter()
            .any(|t| t.text == "declined the handoff from marvin")
    );
}

#[test]
fn later_from_the_inbox_returns_there_and_esc_leaves_an_import_running() {
    let (mut app, mut rxs) = fleet();
    on_event(
        &mut app,
        0,
        "handoff.incoming",
        &event(
            "handoff.incoming",
            "h1",
            json!({"incoming": incoming("h1", "pending")}),
        ),
    );
    // Open the inbox, select the handoff and press enter.
    app.action("inbox", None);
    let idx = crate::inbox::view(&app)
        .items
        .iter()
        .position(|i| i.key.kind == "handoff")
        .unwrap();
    app.inbox.sel_idx = idx;
    app.inbox.selected = None;
    let items = crate::inbox::view(&app).items;
    crate::inbox::sync_selection(&mut app.inbox, &items);
    app.on_key(named(NamedKey::Enter));
    assert!(matches!(app.mode, Mode::Popup(Popup::HandoffAccept)));
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "handoff.incoming.get");
    reply(&mut app, 0, req, get_result("h1", &["/home/u/code/vibeke"]));
    // Later: back in the inbox, the handoff still waiting.
    press(&mut app, NamedKey::Right, 2);
    assert_eq!(focus(&app), Row::Later);
    app.on_key(named(NamedKey::Enter));
    assert!(matches!(app.mode, Mode::Popup(Popup::Inbox)));
    assert_eq!(inbox_items(&app).len(), 1);
    // Accept, then close while it runs: the outcome arrives as a toast.
    crate::handoff::open_accept(&mut app, 0, "h1");
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "handoff.incoming.get");
    reply(&mut app, 0, req, get_result("h1", &["/home/u/code/vibeke"]));
    app.on_key(ch('a'));
    let (req, _) = accept_params(&mut rxs[0]);
    app.on_key(named(NamedKey::Escape));
    assert!(app.ux.handoff.accept.is_none());
    reply_err(&mut app, 0, req, "conflict", json!({}));
    assert!(
        app.toasts
            .iter()
            .any(|t| t.text.starts_with("✗ handoff not imported"))
    );
}

#[test]
fn expired_and_failed_handoffs() {
    let (mut app, _rxs) = fleet();
    let mut failed = incoming("h2", "failed");
    failed["error"] = json!({"kind": "conflict", "message": "branch handoff/x already exists"});
    on_event(
        &mut app,
        0,
        "handoff.updated",
        &event("handoff.updated", "h2", json!({"incoming": failed})),
    );
    let items = inbox_items(&app);
    assert_eq!(items.len(), 1);
    assert!(
        items[0]
            .explanation
            .contains("The last import failed: branch handoff/x already exists"),
        "{}",
        items[0].explanation
    );
    on_event(
        &mut app,
        0,
        "handoff.expired",
        &event("handoff.expired", "h2", json!({})),
    );
    assert!(inbox_items(&app).is_empty());
}

#[test]
fn pushed_handoff_events_are_routed() {
    assert!(crate::push::TYPES.contains(&"handoff.*"));
    let (mut app, _rxs) = fleet();
    let ev = vk_proto::render::PushedEvent {
        seq: 5,
        kind: "handoff.incoming".into(),
        json: event(
            "handoff.incoming",
            "h1",
            json!({"incoming": incoming("h1", "pending")}),
        )
        .to_string(),
    };
    crate::push::on_events(&mut app, 0, vec![ev], false);
    assert_eq!(app.ux.handoff.incoming[&0].len(), 1);
}

#[test]
fn send_flow_builds_params_and_shows_progress() {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    app.action("handoff_send", None);
    assert!(matches!(app.mode, Mode::Popup(Popup::HandoffSend)));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "handoff.peers");
    assert_eq!(p, json!({}));
    assert!(screen(&app).contains("loading paired hosts"));
    reply(
        &mut app,
        0,
        req,
        json!({"peers": [
            {"id": "pr1", "name": "marvin", "owner": "self"},
            {"id": "pr2", "name": "laptop", "owner": "teammate", "expires_at": now_ms() + 7_200_000}
        ]}),
    );
    let s = screen(&app);
    assert!(s.contains("marvin"), "{s}");
    assert!(s.contains("teammate · expires in"), "{s}");
    // Fuzzy filter, then the summary step.
    typ(&mut app, "lap");
    assert_eq!(app.ux.handoff.send.as_ref().unwrap().ranked().len(), 1);
    app.on_key(named(NamedKey::Enter));
    let s = screen(&app);
    assert!(s.contains("to laptop?"), "{s}");
    assert!(s.contains("[ ] Interrupt agent if busy"), "{s}");
    app.on_key(ch('i'));
    assert!(screen(&app).contains("[x] Interrupt agent if busy"));
    app.on_key(named(NamedKey::Enter));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "handoff.send");
    assert_eq!(p, json!({"pane": "p1", "peer": "pr2", "interrupt": true}));
    assert_eq!(
        send_params("p1", &app.ux.handoff.send.as_ref().unwrap().peers[0], false),
        json!({"pane": "p1", "peer": "pr1", "interrupt": false})
    );
    reply(
        &mut app,
        0,
        req,
        json!({"job": {"id": "j1", "pane": "p1", "peer": "pr2", "peer_name": "laptop",
                       "state": "queued", "sent": 0, "total": 0}}),
    );
    assert!(matches!(app.mode, Mode::Normal));
    assert!(app.ux.handoff.send.is_none());
    assert_eq!(status(&app).as_deref(), Some("⇢ laptop queued"));
    let job = |state: &str, sent: u64, inc: Value| {
        json!({"seq": 2, "type": "handoff.job", "subject": {"job": "j1"},
               "data": {"job": {"id": "j1", "pane": "p1", "peer": "pr2", "peer_name": "laptop",
                                "state": state, "sent": sent, "total": 100,
                                "incoming_state": inc}}})
    };
    on_event(&mut app, 0, "handoff.job", &job("sending", 42, Value::Null));
    assert_eq!(status(&app).as_deref(), Some("⇢ laptop 42%"));
    app.toasts.clear();
    assert!(screen(&app).contains("⇢ laptop 42%"));
    // Cancel from the handoffs list.
    app.action("handoffs", None);
    let cmds = commands(&mut rxs[0]);
    assert!(cmds.iter().any(|c| c.1 == "handoff.jobs"));
    assert!(screen(&app).contains("x cancels"));
    app.on_key(ch('x'));
    let cmds = commands(&mut rxs[0]);
    assert_eq!(only(&cmds, "handoff.cancel").1, json!({"id": "j1"}));
    app.on_key(named(NamedKey::Escape));
    // Delivered: off the tab bar, a toast says where it is.
    on_event(
        &mut app,
        0,
        "handoff.job",
        &job("delivered", 100, json!("pending")),
    );
    assert_eq!(status(&app), None);
    assert!(
        app.toasts
            .iter()
            .any(|t| t.text == "⇢ laptop delivered — waiting for accept")
    );
    on_event(
        &mut app,
        0,
        "handoff.job",
        &job("delivered", 100, json!("imported")),
    );
    assert!(app.toasts.iter().any(|t| t.text == "⇢ laptop imported"));
}

#[test]
fn send_needs_a_pane_and_reports_errors() {
    let (mut app, mut rxs) = fleet();
    app.machines[0].focus.pane = None;
    app.action("handoff_send", None);
    assert!(matches!(app.mode, Mode::Normal));
    assert!(
        app.toasts
            .iter()
            .any(|t| t.text == "no focused pane to hand off")
    );
    assert!(commands(&mut rxs[0]).is_empty());
    let (mut app, mut rxs) = fleet();
    app.action("handoff_send", None);
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "handoff.peers");
    reply(&mut app, 0, req, json!({"peers": []}));
    assert!(screen(&app).contains("no paired hosts"));
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(app.mode, Mode::Normal));
    assert!(app.ux.handoff.send.is_none());
    // A server without sending says so.
    app.action("handoff_send", None);
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "handoff.peers");
    reply_err(&mut app, 0, req, "method_not_found", json!({}));
    assert!(screen(&app).contains("can't send handoffs"));
}

#[test]
fn job_status_formats_every_state() {
    let j = |state: &str, sent: u64, total: u64, inc: Option<&str>| Job {
        id: "j".into(),
        pane: "p1".into(),
        peer: "pr1".into(),
        peer_name: "marvin".into(),
        state: state.into(),
        sent,
        total,
        incoming_state: inc.map(str::to_string),
        error: (state == "failed").then(|| "peer offline".to_string()),
    };
    assert_eq!(job_status(&j("queued", 0, 0, None)), "⇢ marvin queued");
    assert_eq!(
        job_status(&j("exporting", 0, 0, None)),
        "⇢ marvin exporting…"
    );
    assert_eq!(job_status(&j("sending", 0, 0, None)), "⇢ marvin sending…");
    assert_eq!(job_status(&j("sending", 21, 50, None)), "⇢ marvin 42%");
    assert_eq!(job_status(&j("sending", 60, 50, None)), "⇢ marvin 100%");
    assert_eq!(
        job_status(&j("delivered", 50, 50, None)),
        "⇢ marvin delivered — waiting for accept"
    );
    assert_eq!(
        job_status(&j("delivered", 50, 50, Some("pending"))),
        "⇢ marvin delivered — waiting for accept"
    );
    assert_eq!(
        job_status(&j("delivered", 50, 50, Some("imported"))),
        "⇢ marvin imported"
    );
    assert_eq!(
        job_status(&j("failed", 0, 0, None)),
        "⇢ marvin failed: peer offline"
    );
    assert_eq!(
        job_status(&j("cancelled", 0, 0, None)),
        "⇢ marvin cancelled"
    );
    // No peer name: the peer id.
    let mut x = j("queued", 0, 0, None);
    x.peer_name.clear();
    assert_eq!(job_status(&x), "⇢ pr1 queued");
    // Errors as objects too.
    let v = Job::from_value(&json!({"id": "j", "state": "failed", "error": {"message": "boom"}}))
        .unwrap();
    assert_eq!(v.error.as_deref(), Some("boom"));
    assert!(!v.active());
}

#[test]
fn status_shows_at_most_two_sends() {
    let (mut app, _rxs) = fleet();
    let mk = |id: &str, name: &str| Job {
        id: id.into(),
        peer_name: name.into(),
        state: "exporting".into(),
        ..Default::default()
    };
    app.ux.handoff.jobs.insert(
        0,
        vec![mk("a", "marvin"), mk("b", "laptop"), mk("c", "box")],
    );
    assert_eq!(
        status(&app).as_deref(),
        Some("⇢ marvin exporting… · ⇢ laptop exporting… · +1 more")
    );
}

#[test]
fn clone_defaults() {
    assert_eq!(
        default_clone_to(Some("/home/u/src/api"), "vibeke", &[]),
        "/home/u/src/vibeke"
    );
    assert_eq!(default_clone_to(None, "vibeke", &[]), "~/code/vibeke");
    assert_eq!(
        default_clone_to(
            Some("/home/u/code/vibeke"),
            "vibeke",
            &["/home/u/code/vibeke".into()]
        ),
        "/home/u/code/vibeke-2"
    );
    assert_eq!(
        default_clone_to(None, "acme/web app.git", &[]),
        "~/code/web-app"
    );
    assert_eq!(default_clone_to(None, "", &[]), "~/code/repo");
    assert_eq!(phase_text("starting"), "starting the agent…");
    let r = Rec::from_value(&incoming("h1", "pending")).unwrap();
    assert!(r.resumable());
    assert_eq!(r.secrets(), vec![".env"]);
    let mut v = incoming("h1", "pending");
    v["manifest"]["transcript"] = json!(false);
    assert!(!Rec::from_value(&v).unwrap().resumable());
    v["manifest"]["harness"] = json!("aider");
    assert!(Rec::from_value(&v).unwrap().agent().is_none());
}

// ---- entry points (B1) and auto-pairing (B4) -------------------------------------------------------

fn prefix(app: &mut App, k: KeyEvent) {
    app.on_key(app.keymap.prefix.clone());
    app.on_key(k);
}

#[test]
fn bindings_open_the_handoff_views() {
    let (mut app, mut rxs) = fleet();
    assert_eq!(
        app.keymap.binding_for("handoffs").as_deref(),
        Some("prefix+shift+h")
    );
    assert_eq!(
        app.keymap.binding_for("handoff_send").as_deref(),
        Some("prefix+alt+h")
    );
    prefix(
        &mut app,
        KeyEvent::new(Key::Char('h'), vk_proto::input::Mods::SHIFT),
    );
    assert!(matches!(app.mode, Mode::Popup(Popup::Handoffs)));
    assert!(
        commands(&mut rxs[0])
            .iter()
            .any(|c| c.1 == "handoff.incoming.list")
    );
    app.on_key(named(NamedKey::Escape));
    prefix(
        &mut app,
        KeyEvent::new(Key::Char('h'), vk_proto::input::Mods::ALT),
    );
    assert!(matches!(app.mode, Mode::Popup(Popup::HandoffSend)));
    only(&commands(&mut rxs[0]), "handoff.peers");
    app.on_key(named(NamedKey::Escape));
    app.action("sharing", None);
    assert!(matches!(app.mode, Mode::Popup(Popup::Sharing)));
}

#[test]
fn the_badge_counts_waiting_handoffs_and_a_click_opens_the_list() {
    let (mut app, mut rxs) = fleet();
    assert_eq!(badge(&app), None);
    let mut failed = incoming("h2", "failed");
    failed["error"] = json!({"kind": "conflict", "message": "boom"});
    let mut imported = incoming("h3", "imported");
    imported["result"] = json!({"pane": "p2"});
    for (id, v) in [
        ("h1", incoming("h1", "pending")),
        ("h2", failed),
        ("h3", imported),
    ] {
        on_event(
            &mut app,
            0,
            "handoff.incoming",
            &event("handoff.incoming", id, json!({"incoming": v})),
        );
    }
    assert_eq!(waiting_count(&app), 2);
    assert_eq!(badge(&app).as_deref(), Some(" ⇣2 "));
    let s = screen(&app);
    assert!(s.contains("⇣2"), "{s}");
    // Where the badge is drawn, a click opens the handoffs list.
    let row = crate::chrome::tab_row(&app).expect("a tab row");
    let x = (0..app.size.0)
        .find(|&x| crate::draw::right_cluster_at(&app, x, row).as_deref() == Some(" ⇣2 "))
        .expect("the badge is in the right cluster");
    commands(&mut rxs[0]);
    app.on_mouse(crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: x,
        row,
        modifiers: crossterm::event::KeyModifiers::NONE,
    });
    assert!(matches!(app.mode, Mode::Popup(Popup::Handoffs)));
    // Accepting from the list opens the overlay (as from the inbox); newest first, so the
    // imported h3 leads and the failed h2 follows.
    app.on_key(ch('j'));
    app.on_key(named(NamedKey::Enter));
    assert!(matches!(app.mode, Mode::Popup(Popup::HandoffAccept)));
    assert!(
        commands(&mut rxs[0])
            .iter()
            .any(|c| c.1 == "handoff.incoming.get")
    );
}

#[test]
fn the_pane_menu_offers_hand_off_and_details_of_an_imported_pane() {
    let (mut app, mut rxs) = fleet();
    let mut imported = incoming("h3", "imported");
    imported["result"] = json!({"pane": "p2", "worktree": "/w"});
    on_event(
        &mut app,
        0,
        "handoff.incoming",
        &event("handoff.incoming", "h3", json!({"incoming": imported})),
    );
    assert_eq!(imported_into(&app, 0, "p2").as_deref(), Some("h3"));
    // Right-click on p2's sidebar row: the palette on its handoff actions.
    pane_menu(&mut app, 0, "p2");
    match &app.mode {
        Mode::Popup(Popup::Palette { filter, .. }) => assert_eq!(filter, "handoff"),
        m => panic!("{m:?}"),
    }
    let entries = crate::nav::palette_entries(&app);
    assert!(entries.iter().any(|e| e.id == "handoff_send"));
    assert!(entries.iter().any(|e| e.id == "handoff_details"));
    commands(&mut rxs[0]);
    app.action("handoff_details", None);
    let cmds = commands(&mut rxs[0]);
    assert_eq!(only(&cmds, "handoff.incoming.get").1, json!({"id": "h3"}));
    assert!(matches!(app.mode, Mode::Popup(Popup::HandoffAccept)));
    // Hand off…: the right-clicked pane, not the focused one.
    app.on_key(named(NamedKey::Escape));
    pane_menu(&mut app, 0, "p2");
    app.action("handoff_send", None);
    assert_eq!(app.ux.handoff.send.as_ref().unwrap().pane, "p2");
    // A pane that didn't come from a handoff.
    app.on_key(named(NamedKey::Escape));
    app.action("handoff_details", None);
    assert!(
        app.toasts
            .iter()
            .any(|t| t.text == "this pane didn't come from a handoff")
    );
}

#[test]
fn sending_to_an_unpaired_machine_pairs_them_first() {
    let (mut app, mut rxs) = crate::drafts::tests::fleet_n(3);
    // m2 is already a peer of m0 (by name); m1 is not.
    commands(&mut rxs[0]);
    app.action("handoff_send", None);
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "handoff.peers");
    reply(
        &mut app,
        0,
        req,
        json!({"peers": [{"id": "pr2", "name": "M2.local", "owner": "self"}]}),
    );
    let s = screen(&app);
    assert!(s.contains("Your machines (will pair)"), "{s}");
    assert!(s.contains("your machine · will pair"), "{s}");
    let f = app.ux.handoff.send.as_ref().unwrap();
    let names: Vec<String> = f
        .ranked()
        .iter()
        .map(|(d, _)| d.name().to_string())
        .collect();
    assert_eq!(names, vec!["M2.local", "m1"]);
    assert_eq!(plan_send(&f.ranked()[1].0, now_ms()), Plan::Pair(1));
    // Choose m1.
    typ(&mut app, "m1");
    app.on_key(named(NamedKey::Enter));
    assert!(screen(&app).contains("not paired with m0 yet"));
    app.on_key(named(NamedKey::Enter));
    // 1. An invitation on m1.
    let cmds = commands(&mut rxs[1]);
    let (req, p) = only(&cmds, "gateway.call");
    assert_eq!(p, json!({"method": "peer.invite", "params": {}}));
    assert!(commands(&mut rxs[0]).is_empty());
    let link = "https://app.example/#/pair?d=xyz";
    reply(
        &mut app,
        1,
        req,
        json!({"link": link, "pid": "pidP", "open_by": now_ms() / 1000 + 900}),
    );
    // 2. Accepted on m0.
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "gateway.call");
    assert_eq!(
        p,
        json!({"method": "peer.redeem", "params": {"link": link, "share_user": false},
               "timeout_ms": 60_000})
    );
    reply(
        &mut app,
        0,
        req,
        json!({"peer": {"id": "pr9", "name": "m1", "owner": "self", "expires_at": null}}),
    );
    // 3. The send itself, to the new peer.
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "handoff.send");
    assert_eq!(p, json!({"pane": "p1", "peer": "pr9", "interrupt": false}));
    assert!(app.toasts.iter().any(|t| t.text == "paired with m1"));
    reply(
        &mut app,
        0,
        req,
        json!({"job": {"id": "j9", "pane": "p1", "peer": "pr9", "peer_name": "m1",
                       "state": "queued", "sent": 0, "total": 0}}),
    );
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(status(&app).as_deref(), Some("⇢ m1 queued"));
}

#[test]
fn pairing_failures_and_unavailable_destinations_stay_in_the_summary() {
    let (mut app, mut rxs) = crate::drafts::tests::fleet_n(2);
    app.action("handoff_send", None);
    let (req, _) = only(&commands(&mut rxs[0]), "handoff.peers");
    reply(
        &mut app,
        0,
        req,
        json!({"peers": [{"id": "old", "name": "box", "owner": "teammate",
                          "expires_at": 1, "expired": true}]}),
    );
    // An expired peer is refused here.
    app.on_key(named(NamedKey::Enter));
    app.on_key(named(NamedKey::Enter));
    assert!(screen(&app).contains("the pairing with box has expired"));
    assert!(commands(&mut rxs[0]).is_empty());
    // The gateway of m1 isn't running.
    app.on_key(named(NamedKey::Escape));
    app.on_key(named(NamedKey::Down));
    app.on_key(named(NamedKey::Enter));
    app.on_key(named(NamedKey::Enter));
    let (req, _) = only(&commands(&mut rxs[1]), "gateway.call");
    let json = json!({"jsonrpc": "2.0", "id": req, "error": {"code": -32000,
        "message": "the gateway isn't running: start it with `vibeke gateway run`",
        "data": {"kind": "remote_unavailable", "details": null}}})
    .to_string();
    app.on_frame(
        1,
        vk_proto::render::ServerFrame::CommandResult { req, json },
    );
    let s = screen(&app);
    assert!(
        s.contains("pairing failed on m1: the gateway isn't running"),
        "{s}"
    );
    assert!(!app.ux.handoff.send.as_ref().unwrap().busy);
    // An offline machine can't be paired.
    app.machines[1].tx = None;
    app.on_key(named(NamedKey::Escape));
    let f = app.ux.handoff.send.as_mut().unwrap();
    f.machines = vec![Dest::Machine {
        mi: 1,
        name: "m1".into(),
        online: false,
    }];
    assert_eq!(
        plan_send(&f.machines[0], now_ms()),
        Plan::Unavailable("offline")
    );
    assert!(same_host("Mini.local", "mini"));
    assert!(!same_host("", ""));
}

/// A `peer.invite` link from host `host` named `name` (the shape the gateway returns).
fn peer_link(host: &str, name: &str) -> String {
    use base64::Engine as _;
    let b = |x: [u8; 32]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(x);
    let v = json!({
        "v": 1, "relay": "wss://relay.example", "host": host, "hk": b([1; 32]),
        "pid": "pidP", "psk": b([2; 32]), "exp": now_ms() / 1000 + 900, "name": name,
        "share": {"kind": "peer", "scope": "full", "until": 0, "label": null, "limit": null}
    });
    let d =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&v).unwrap());
    format!("https://app.example/#/pair?d={d}")
}

/// The send form on m0 with these peers, machine `m1` chosen (the row after the peers) and
/// submitted: the `peer.invite` request on m1.
fn pair_m1(app: &mut App, rxs: &mut [UnboundedReceiver<ClientFrame>], peers: Value) -> u64 {
    commands(&mut rxs[0]);
    app.action("handoff_send", None);
    let (req, _) = only(&commands(&mut rxs[0]), "handoff.peers");
    let n = peers.as_array().map_or(0, Vec::len);
    reply(app, 0, req, json!({ "peers": peers }));
    for _ in 0..n {
        app.on_key(named(NamedKey::Down));
    }
    app.on_key(named(NamedKey::Enter));
    let f = app.ux.handoff.send.as_ref().unwrap();
    assert!(
        matches!(&f.chosen, Some(Dest::Machine { mi: 1, .. })),
        "{:?}",
        f.chosen
    );
    app.on_key(named(NamedKey::Enter));
    let (req, p) = only(&commands(&mut rxs[1]), "gateway.call");
    assert_eq!(p, json!({"method": "peer.invite", "params": {}}));
    req
}

#[test]
fn a_same_named_teammate_peer_is_never_the_destination_for_your_machine() {
    let (mut app, mut rxs) = crate::drafts::tests::fleet_n(2);
    // A teammate's host is also called m1: your machine m1 is still listed, to pair.
    let req = pair_m1(
        &mut app,
        &mut rxs,
        json!([{"id": "tm", "name": "m1", "owner": "teammate"}]),
    );
    let link = peer_link("H1", "m1");
    reply(
        &mut app,
        1,
        req,
        json!({"link": link, "pid": "pidP", "open_by": now_ms() / 1000 + 900}),
    );
    // The source's peers by host id: a teammate and an own host both named m1, other hosts.
    let (req, p) = only(&commands(&mut rxs[0]), "gateway.call");
    assert_eq!(p, json!({"method": "peer.list", "params": {}}));
    reply(
        &mut app,
        0,
        req,
        json!({"peers": [
            {"id": "tm", "name": "m1", "host": "T9", "owner": "teammate"},
            {"id": "old", "name": "m1", "host": "H0", "owner": "self"}
        ]}),
    );
    // No identity match: the invitation is redeemed, nothing goes to "tm" or "old".
    let cmds = commands(&mut rxs[0]);
    assert!(!cmds.iter().any(|c| c.1 == "handoff.send"), "{cmds:?}");
    let (req, p) = only(&cmds, "gateway.call");
    assert_eq!(p["method"], "peer.redeem");
    assert_eq!(p["params"]["link"], json!(link));
    reply(
        &mut app,
        0,
        req,
        json!({"peer": {"id": "pr9", "name": "m1", "owner": "self"}}),
    );
    let (_, p) = only(&commands(&mut rxs[0]), "handoff.send");
    assert_eq!(p, json!({"pane": "p1", "peer": "pr9", "interrupt": false}));
}

#[test]
fn an_own_peer_with_the_invitations_host_id_is_used_and_the_invitation_revoked() {
    let (mut app, mut rxs) = crate::drafts::tests::fleet_n(2);
    // Paired already, under another name (and a teammate claims the machine's name).
    let req = pair_m1(
        &mut app,
        &mut rxs,
        json!([{"id": "tm", "name": "m1", "owner": "teammate"},
               {"id": "mine", "name": "studio", "owner": "self"}]),
    );
    reply(
        &mut app,
        1,
        req,
        json!({"link": peer_link("H1", "m1"), "pid": "pidP"}),
    );
    let (req, _) = only(&commands(&mut rxs[0]), "gateway.call");
    reply(
        &mut app,
        0,
        req,
        json!({"peers": [
            {"id": "tm", "name": "m1", "host": "H1", "owner": "teammate"},
            {"id": "mine", "name": "studio", "host": "H1", "owner": "self"}
        ]}),
    );
    let (_, p) = only(&commands(&mut rxs[1]), "gateway.call");
    assert_eq!(
        p,
        json!({"method": "share.revoke", "params": {"id": "pidP"}})
    );
    let (_, p) = only(&commands(&mut rxs[0]), "handoff.send");
    assert_eq!(p, json!({"pane": "p1", "peer": "mine", "interrupt": false}));
}

#[test]
fn a_late_pairing_reply_after_esc_never_drives_a_new_send_form() {
    let (mut app, mut rxs) = crate::drafts::tests::fleet_n(3);
    // Form A: p1 to m1; Esc while the invitation is being made.
    commands(&mut rxs[0]);
    app.action("handoff_send", None);
    let (req, _) = only(&commands(&mut rxs[0]), "handoff.peers");
    reply(&mut app, 0, req, json!({"peers": []}));
    typ(&mut app, "m1");
    app.on_key(named(NamedKey::Enter));
    app.on_key(named(NamedKey::Enter));
    let (inv_a, _) = only(&commands(&mut rxs[1]), "gateway.call");
    app.on_key(named(NamedKey::Escape));
    assert!(app.ux.handoff.send.is_none());
    // Form B: p2 to m2.
    pane_menu(&mut app, 0, "p2");
    app.action("handoff_send", None);
    let (req, _) = only(&commands(&mut rxs[0]), "handoff.peers");
    reply(&mut app, 0, req, json!({"peers": []}));
    typ(&mut app, "m2");
    app.on_key(named(NamedKey::Enter));
    app.on_key(named(NamedKey::Enter));
    let (inv_b, _) = only(&commands(&mut rxs[2]), "gateway.call");
    // A's invitation arrives: revoked on m1, nothing happens on m0, B still waits for m2.
    reply(
        &mut app,
        1,
        inv_a,
        json!({"link": peer_link("H1", "m1"), "pid": "pidA"}),
    );
    let (_, p) = only(&commands(&mut rxs[1]), "gateway.call");
    assert_eq!(
        p,
        json!({"method": "share.revoke", "params": {"id": "pidA"}})
    );
    assert!(commands(&mut rxs[0]).is_empty());
    let f = app.ux.handoff.send.as_ref().unwrap();
    assert_eq!(f.pane, "p2");
    assert_eq!(f.chosen.as_ref().map(Dest::name), Some("m2"));
    assert!(f.busy && f.pairing.is_some());
    // B's invitation (a link without a host id) goes straight to redeem; then Esc and form C.
    reply(
        &mut app,
        2,
        inv_b,
        json!({"link": "https://app.example/#/pair?d=xyz", "pid": "pidB"}),
    );
    let (redeem_b, p) = only(&commands(&mut rxs[0]), "gateway.call");
    assert_eq!(p["method"], "peer.redeem");
    app.on_key(named(NamedKey::Escape));
    commands(&mut rxs[0]);
    app.action("handoff_send", None);
    let (req, _) = only(&commands(&mut rxs[0]), "handoff.peers");
    reply(&mut app, 0, req, json!({"peers": []}));
    typ(&mut app, "m1");
    app.on_key(named(NamedKey::Enter));
    app.on_key(named(NamedKey::Enter));
    only(&commands(&mut rxs[1]), "gateway.call");
    // B's late redeem: no send for form C's pane, C still pairing with m1.
    reply(
        &mut app,
        0,
        redeem_b,
        json!({"peer": {"id": "pr2", "name": "m2", "owner": "self"}}),
    );
    assert!(commands(&mut rxs[0]).is_empty());
    let f = app.ux.handoff.send.as_ref().unwrap();
    assert_eq!(f.pane, "p1");
    assert_eq!(f.chosen.as_ref().map(Dest::name), Some("m1"));
    assert!(f.busy && f.pairing.is_some());
}

#[test]
fn a_late_send_reply_never_clears_or_fails_a_newer_form() {
    let (mut app, mut rxs) = fleet();
    let open = |app: &mut App, rxs: &mut Vec<UnboundedReceiver<ClientFrame>>| {
        commands(&mut rxs[0]);
        app.action("handoff_send", None);
        let (req, _) = only(&commands(&mut rxs[0]), "handoff.peers");
        reply(
            app,
            0,
            req,
            json!({"peers": [{"id": "pr1", "name": "marvin", "owner": "self"}]}),
        );
    };
    // Form A sends to marvin, then Esc before the reply.
    open(&mut app, &mut rxs);
    app.on_key(named(NamedKey::Enter));
    app.on_key(named(NamedKey::Enter));
    let (send_a, _) = only(&commands(&mut rxs[0]), "handoff.send");
    app.on_key(named(NamedKey::Escape));
    assert!(app.ux.handoff.send.is_none());
    // Form B is open; A's failure only toasts.
    open(&mut app, &mut rxs);
    reply_err(&mut app, 0, send_a, "conflict", json!({}));
    let f = app.ux.handoff.send.as_ref().unwrap();
    assert!(f.error.is_none() && !f.busy);
    assert!(matches!(app.mode, Mode::Popup(Popup::HandoffSend)));
    // B sends; A's (late) success would not close B either: only B's own reply does.
    app.on_key(named(NamedKey::Enter));
    app.on_key(named(NamedKey::Enter));
    let (send_b, _) = only(&commands(&mut rxs[0]), "handoff.send");
    reply(&mut app, 0, send_a, json!({}));
    assert!(app.ux.handoff.send.as_ref().is_some_and(|f| f.busy));
    reply(&mut app, 0, send_b, json!({}));
    assert!(app.ux.handoff.send.is_none());
}
