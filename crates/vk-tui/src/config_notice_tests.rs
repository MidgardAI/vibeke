//! The config error notice: read after connecting, updated by pushed events, shown as a badge
//! until fixed or looked at.

use super::*;
use crate::drafts::tests::{commands, fleet, only, reply, screen};
use vk_proto::render::PushedEvent;

fn push(app: &mut App, kind: &str, data: Value) {
    let ev = PushedEvent {
        seq: 1,
        kind: kind.into(),
        json: json!({"seq": 1, "type": kind, "subject": {}, "data": data}).to_string(),
    };
    crate::push::on_events(app, 0, vec![ev], false);
}

fn load_error() -> Value {
    json!({"load_error": {"path": "/home/user/.config/vibeke/config.toml",
        "errors": [{"line": 3, "col": 1, "message": "expected a string"}]}})
}

#[test]
fn a_config_that_does_not_load_shows_a_badge_until_looked_at() {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    on_connected(&mut app, 0);
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "config.get");
    reply(&mut app, 0, req, load_error());
    assert!(
        app.toasts.iter().any(|t| t
            .text
            .contains("config.toml has an error (line 3: expected a string)")),
        "{:?}",
        app.toasts.iter().map(|t| &t.text).collect::<Vec<_>>()
    );
    // The badge stays after the toast is gone.
    app.toasts.clear();
    let s = screen(&app);
    assert!(
        s.contains("config.toml error: last good config in use"),
        "{s}"
    );
    // The action opens the details (file, error, what is in use) and dismisses the badge.
    app.action("config_error", None);
    match &app.mode {
        Mode::Popup(Popup::Message { body, .. }) => {
            assert!(
                body.contains("/home/user/.config/vibeke/config.toml"),
                "{body}"
            );
            assert!(body.contains("line 3: expected a string"), "{body}");
            assert!(body.contains("last config that loaded"), "{body}");
        }
        m => panic!("{m:?}"),
    }
    app.mode = Mode::Normal;
    assert!(badge(&app).is_none());
    // The same error again keeps it dismissed; a different one shows it again.
    push(
        &mut app,
        "session.config_rejected",
        json!({"errors": [{"line": 3, "col": 1, "message": "expected a string"}], "source": "watch"}),
    );
    assert!(badge(&app).is_none());
    push(
        &mut app,
        "session.config_rejected",
        json!({"errors": [{"line": 9, "col": 2, "message": "unknown theme"}], "source": "watch"}),
    );
    assert!(badge(&app).is_some());
    // The path learned earlier stays while the new read is on its way.
    assert_eq!(
        app.ux.config.per[&0].path,
        "/home/user/.config/vibeke/config.toml"
    );
    // Fixed: the notice goes.
    push(
        &mut app,
        "session.config_reloaded",
        json!({"changed_keys": [], "new_panes_only": [], "source": "watch"}),
    );
    assert!(badge(&app).is_none());
    assert!(app.ux.config.per.is_empty());
}

#[test]
fn a_config_that_loads_shows_nothing() {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    on_connected(&mut app, 0);
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "config.get");
    reply(&mut app, 0, req, json!({"value": {}, "load_error": null}));
    assert!(badge(&app).is_none());
    assert!(app.toasts.iter().all(|t| !t.text.contains("config")));
}

#[test]
fn errors_read_as_lines() {
    assert_eq!(
        errors_of(
            &json!([{"line": 0, "col": 0, "message": "permission denied"},
                          {"line": 4, "col": 1, "message": "bad\u{1b}[31m value"}])
        ),
        vec![
            "permission denied".to_string(),
            "line 4: bad[31m value".to_string()
        ]
    );
}
