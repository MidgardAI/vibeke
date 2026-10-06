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
    let (_, p) = only(&commands(&mut rxs[0]), "pane.float");
    assert_eq!(p["command"], json!(["/bin/sh", "-c", "make test"]));
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
