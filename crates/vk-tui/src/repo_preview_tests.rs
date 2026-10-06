//! A trusted repo's `[preview]`: layered over the user's config for that workspace's previews
//! (mode, pane_split, inline_thumbnails); untrusted files never apply.

use super::*;
use crate::drafts::tests::{commands, fleet, only, reply};
use serde_json::{Value, json};

fn preview() -> Preview {
    serde_json::from_value(json!({
        "id": "PV", "handle": "v4", "machine": "m0", "pane": "p1", "task": null,
        "port": 5173, "path": "/", "label": "vite", "url": "http://localhost:5173/",
        "scheme": "http", "status": "up", "source": "banner", "pid": null,
        "first_seen_ms": 0, "last_seen_ms": 0
    }))
    .unwrap()
}

fn info(trusted: bool, preview_toml: &str) -> Value {
    json!({
        "repo": "/src/api", "file": "/src/api/.vibeke/config.toml", "digest": "d1",
        "trusted": trusted, "text": preview_toml, "warnings": [], "commands": [], "error": null
    })
}

/// Answer the focused workspace's trust check with `v`.
fn checked(
    app: &mut App,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<vk_proto::render::ClientFrame>,
    v: Value,
) {
    app.on_tick();
    let (req, _) = only(&commands(rx), "policy.trust");
    reply(app, 0, req, v);
}

#[test]
fn trusted_repo_preview_keys_layer_over_the_users() {
    let (mut app, mut rx) = fleet();
    app.machines[0].model.previews = vec![preview()];
    checked(
        &mut app,
        &mut rx[0],
        info(
            true,
            "[preview]\nmode = \"window\"\npane_split = \"down\"\ninline_thumbnails = false\n",
        ),
    );
    let p = preview();
    assert_eq!(ws_of_preview(&app, 0, &p).as_deref(), Some("W1"));
    let cfg = for_preview(&app, 0, &p);
    assert_eq!(cfg.mode, vk_config::PreviewMode::Window);
    assert_eq!(cfg.pane_split, vk_config::PaneSplit::Down);
    assert!(!cfg.inline_thumbnails);
    // The user's other keys stay.
    assert_eq!(cfg.default_viewport, app.config.preview().default_viewport);
    // Opening it follows the repo: a profile window, not a browser pane.
    crate::browser::open_preview(&mut app, 0, &p, Some("p1".into()));
    let (_, params) = only(&commands(&mut rx[0]), "preview.open");
    assert_eq!(params["window"], true);
}

#[test]
fn repo_pane_split_applies_to_browser_panes() {
    let (mut app, mut rx) = fleet();
    app.caps.kitty_graphics = true;
    app.machines[0].model.previews = vec![preview()];
    checked(
        &mut app,
        &mut rx[0],
        info(true, "[preview]\npane_split = \"down\"\n"),
    );
    crate::browser::open_preview(&mut app, 0, &preview(), Some("p1".into()));
    let (_, params) = only(&commands(&mut rx[0]), "browser.pane.create");
    assert_eq!(params["split"], "down");
    assert_eq!(params["pane"], "p1");
}

#[test]
fn untrusted_or_absent_preview_sections_never_apply() {
    let (mut app, mut rx) = fleet();
    app.caps.kitty_graphics = true;
    app.machines[0].model.previews = vec![preview()];
    checked(
        &mut app,
        &mut rx[0],
        info(false, "[preview]\nmode = \"window\"\n"),
    );
    let cfg = for_preview(&app, 0, &preview());
    assert_eq!(cfg, app.config.preview());
    crate::browser::open_preview(&mut app, 0, &preview(), Some("p1".into()));
    let (_, params) = only(&commands(&mut rx[0]), "browser.pane.create");
    assert_eq!(params["split"], "right");
    // Trusted but without [preview]: nothing layered.
    let i = crate::trust::Info::from_value(&info(true, "[tasks]\nport_block = 20\n"));
    assert!(layered(&app, &i).is_none());
    // A preview on another machine's workspace that was never checked: the user's config.
    assert_eq!(effective(&app, 0, Some("W9")), app.config.preview());
}
