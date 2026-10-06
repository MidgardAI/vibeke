use super::*;
use crate::drafts::tests::{commands, fleet, only, reply, reply_err, screen};
use vk_proto::input::Mods;

fn info_json(trusted: bool) -> Value {
    json!({
        "repo": "/src/api", "file": "/src/api/.vibeke/config.toml", "digest": "d1",
        "trusted": trusted,
        "text": "[[keys.command]]\nkey = \"prefix+alt+t\"\ntype = \"popup\"\ncommand = \"make test\"\ntitle = \"tests\"\n",
        "warnings": ["ui: ignored (a repo may set tasks, preview, policy, keys)"],
        "commands": [{"key": "prefix+alt+t", "type": "popup", "command": "make test", "title": "tests", "width": null, "height": null}],
        "error": null
    })
}

#[test]
fn untrusted_repo_gets_a_notice_review_and_trust_with_the_reviewed_digest() {
    let (mut app, mut rxs) = fleet();
    app.on_tick();
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "policy.trust");
    assert_eq!(p, json!({"path": "/src/api", "check": true}));
    reply(&mut app, 0, req, info_json(false));
    assert!(app.toasts.last().unwrap().text.contains(":trust_repo"));
    // Checked once per workspace.
    app.on_tick();
    assert!(commands(&mut rxs[0]).iter().all(|c| c.1 != "policy.trust"));
    // Not trusted: no repo commands anywhere.
    assert!(palette_entries(&app).is_empty());
    app.action("repo:0:W1:0", None);
    assert!(app.toasts.last().unwrap().text.contains("not trusted"));
    // Review.
    app.action("trust_repo", None);
    let (req, _) = only(&commands(&mut rxs[0]), "policy.trust");
    reply(&mut app, 0, req, info_json(false));
    assert!(matches!(app.mode, Mode::Popup(Popup::TrustRepo)));
    let s = screen(&app);
    assert!(s.contains("not trusted") && s.contains("make test"), "{s}");
    assert!(s.contains("ui: ignored"), "{s}");
    app.on_key(KeyEvent::new(Key::Char('y'), Mods::empty()));
    let (req, p) = only(&commands(&mut rxs[0]), "policy.trust");
    assert_eq!(p, json!({"path": "/src/api", "digest": "d1"}));
    reply(
        &mut app,
        0,
        req,
        json!({"repo": "/src/api", "digest": "d1"}),
    );
    // Re-checked; now trusted: palette entry and key binding.
    let (req, _) = only(&commands(&mut rxs[0]), "policy.trust");
    reply(&mut app, 0, req, info_json(true));
    let e = palette_entries(&app);
    assert_eq!(e.len(), 1);
    assert_eq!(e[0].1, "Repo command: tests");
    assert!(
        crate::nav::palette_entries(&app)
            .iter()
            .any(|x| x.desc == "Repo command: tests")
    );
    assert_eq!(
        app.keymap.binding_for("repo:0:W1:0").as_deref(),
        Some("prefix+alt+t")
    );
    app.mode = Mode::Normal;
    app.action("repo:0:W1:0", None);
    // Trust is re-checked with the server before the command runs.
    let cmds = commands(&mut rxs[0]);
    assert!(cmds.iter().all(|c| c.1 != "pane.float"), "{cmds:?}");
    let (req, p) = only(&cmds, "policy.trust");
    assert_eq!(p, json!({"path": "/src/api", "check": true}));
    reply(&mut app, 0, req, info_json(true));
    let (_, p) = only(&commands(&mut rxs[0]), "pane.float");
    assert_eq!(p["command"], json!(["/bin/sh", "-c", "make test"]));
}

/// Review batch 2, finding 8: a trusted repo whose `.vibeke/` changes while the TUI stays open
/// (a checkout rewrites the script a binding runs) needs a fresh trust decision before the
/// binding spawns anything.
#[test]
fn a_changed_trusted_repo_needs_a_fresh_review_before_its_command_runs() {
    let (mut app, mut rxs) = fleet();
    app.on_tick();
    let (req, _) = only(&commands(&mut rxs[0]), "policy.trust");
    reply(&mut app, 0, req, info_json(true));
    assert_eq!(palette_entries(&app).len(), 1, "trusted and bound");
    // The tree changes; the server now reports another digest, untrusted.
    let mut changed = info_json(false);
    changed["digest"] = json!("d2");
    changed["commands"][0]["command"] = json!("sh .vibeke/run.sh");
    app.action("repo:0:W1:0", None);
    let (req, _) = only(&commands(&mut rxs[0]), "policy.trust");
    reply(&mut app, 0, req, changed.clone());
    let cmds = commands(&mut rxs[0]);
    assert!(
        cmds.iter()
            .all(|c| c.1 != "pane.float" && c.1 != "pane.run"),
        "nothing spawned: {cmds:?}"
    );
    assert!(matches!(app.mode, Mode::Popup(Popup::TrustRepo)));
    let s = screen(&app);
    assert!(
        s.contains("changed since it was trusted") && s.contains("d2"),
        "{s}"
    );
    // Same digest reported trusted by the server but for other content: still a new review.
    app.mode = Mode::Normal;
    app.ux
        .trust
        .info
        .get_mut(&(0, "W1".into()))
        .unwrap()
        .trusted = true;
    app.ux.trust.info.get_mut(&(0, "W1".into())).unwrap().digest = Some("d1".into());
    let mut trusted_elsewhere = info_json(true);
    trusted_elsewhere["digest"] = json!("d3");
    app.action("repo:0:W1:0", None);
    let (req, _) = only(&commands(&mut rxs[0]), "policy.trust");
    reply(&mut app, 0, req, trusted_elsewhere);
    assert!(commands(&mut rxs[0]).iter().all(|c| c.1 != "pane.float"));
    // After a fresh `y` on the new content, the binding works again.
    app.on_key(KeyEvent::new(Key::Char('y'), Mods::empty()));
    let (req, p) = only(&commands(&mut rxs[0]), "policy.trust");
    assert_eq!(p, json!({"path": "/src/api", "digest": "d3"}));
    reply(
        &mut app,
        0,
        req,
        json!({"repo": "/src/api", "digest": "d3"}),
    );
    let (req, _) = only(&commands(&mut rxs[0]), "policy.trust");
    let mut now = info_json(true);
    now["digest"] = json!("d3");
    reply(&mut app, 0, req, now.clone());
    app.mode = Mode::Normal;
    app.action("repo:0:W1:0", None);
    let (req, _) = only(&commands(&mut rxs[0]), "policy.trust");
    reply(&mut app, 0, req, now);
    only(&commands(&mut rxs[0]), "pane.float");
}

#[test]
fn a_changed_file_is_refused() {
    let (mut app, mut rxs) = fleet();
    app.action("trust_repo", None);
    let (req, _) = only(&commands(&mut rxs[0]), "policy.trust");
    reply(&mut app, 0, req, info_json(false));
    app.on_key(KeyEvent::new(Key::Char('y'), Mods::empty()));
    let (req, _) = only(&commands(&mut rxs[0]), "policy.trust");
    reply_err(&mut app, 0, req, "conflict", json!({}));
    assert!(screen(&app).contains("not trusted: conflict!"));
    // No file: a toast instead of the view.
    app.mode = Mode::Normal;
    app.ux.trust = Default::default();
    app.action("trust_repo", None);
    let (req, _) = only(&commands(&mut rxs[0]), "policy.trust");
    reply(
        &mut app,
        0,
        req,
        json!({"repo": "/src/api", "file": null, "trusted": false}),
    );
    assert!(matches!(app.mode, Mode::Normal));
    assert!(
        app.toasts
            .last()
            .unwrap()
            .text
            .contains("no .vibeke/config.toml")
    );
}
