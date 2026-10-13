//! The review device allow-list (spec 20 §3.2, §9): allowed calls, refusals, another preview,
//! another session and param rewriting.

use super::*;
use crate::state::{GithubId, Limit, Scope, StateDir};

fn gateway(t: &tempfile::TempDir) -> Arc<Gateway> {
    let st = StateDir::open(t.path().join("gw")).unwrap();
    Gateway::new(st, crate::server::Server::new(t.path().join("none.sock"))).unwrap()
}

fn reviewer(notes: &str, github: bool) -> Device {
    let mut d: Device = serde_json::from_value(json!({
        "id": "r1", "name": "@alice", "public": "k", "scope": "view", "paired_at": 0,
        "kind": "share", "expires_at": u64::MAX,
    }))
    .unwrap();
    d.limit = Some(Limit {
        workspace: None,
        pane: None,
        preview: Some("pv1".into()),
    });
    d.review = Some(Review {
        preview: "pv1".into(),
        sid: "abcdefghijklmnop".into(),
        notes: notes.into(),
        ..Default::default()
    });
    d.invitation = Some("pid1".into());
    if github {
        d.github = Some(GithubId {
            id: 42,
            login: "alice".into(),
        });
    }
    d
}

fn kind(r: Result<Value, ApiError>) -> String {
    r.err().map(|e| e.kind).unwrap_or_else(|| "ok".into())
}

#[test]
fn everything_outside_the_table_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let gw = gateway(&t);
    let d = reviewer("direct", true);
    for m in [
        "dashboard.get",
        "events.subscribe",
        "pane.read",
        "pane.send_text",
        "agent.prompt",
        "agent.transcript",
        "fs.read",
        "fs.list",
        "git.diff",
        "preview.list",
        "preview.get",
        "screenshot.get",
        "interaction.list",
        "tui.attach",
        "tunnel.open",
        "browser.attach_screencast",
        "browser.take_over",
        "browser.click",
        "browser.list",
        "browser.annotate",
        "browser.annotations_release",
        "browser.annotations_discard",
        "share.create",
        "share.list",
        "devices.list",
        "push.subscribe",
        "prefs.set",
        "handoff.link.create",
        "nope",
    ] {
        assert_eq!(kind(authorize(&gw, &d, m, json!({}))), "forbidden", "{m}");
    }
    for m in ["hello", "ping", "relay.ticket", "review.info"] {
        assert_eq!(
            authorize(&gw, &d, m, json!({"x": 1})).unwrap(),
            json!({}),
            "{m}"
        );
    }
    // A device without `review` never passes, and neither does an expired one.
    let mut plain = reviewer("direct", false);
    plain.review = None;
    assert_eq!(kind(authorize(&gw, &plain, "ping", json!({}))), "forbidden");
    let mut old = reviewer("direct", false);
    old.expires_at = Some(1);
    assert_eq!(kind(authorize(&gw, &old, "ping", json!({}))), "forbidden");
}

#[test]
fn preview_open_is_its_preview_and_confined() {
    let t = tempfile::tempdir().unwrap();
    let gw = gateway(&t);
    let d = reviewer("direct", false);
    let ok = json!({"viewport": {"width": 800, "height": 600}, "dpr": 2.0, "dark": true,
                    "url": "http://localhost:9999", "op_id": "o1", "mobile": false});
    assert_eq!(
        authorize(&gw, &d, "browser.preview_open", ok.clone()).unwrap(),
        json!({"preview": "pv1", "viewport": {"width": 800, "height": 600}, "dpr": 2.0,
               "dark": true, "mobile": false, "confine": true, "op_id": "o1"})
    );
    let mut same = ok.clone();
    same["preview"] = json!("pv1");
    assert!(authorize(&gw, &d, "browser.preview_open", same).is_ok());
    for (k, v) in [
        ("preview", json!("pv2")),
        ("preview", json!(7)),
        ("machine", json!("other")),
    ] {
        let mut p = ok.clone();
        p[k] = v;
        assert_eq!(
            kind(authorize(&gw, &d, "browser.preview_open", p)),
            "forbidden",
            "{k}"
        );
    }
    let mut p = ok;
    p["dpr"] = json!(100);
    assert_eq!(
        kind(authorize(&gw, &d, "browser.preview_open", p)),
        "invalid_params"
    );
}

#[test]
fn sessions_must_be_its_own() {
    let t = tempfile::tempdir().unwrap();
    let gw = gateway(&t);
    let d = reviewer("direct", false);
    gw.screencasts.set_opened("b1", "r1");
    gw.screencasts.set_opened("b2", "someone-else");
    for m in [
        "browser.screencast_frame",
        "browser.session_close",
        "browser.annotation_pick",
    ] {
        assert!(
            authorize(&gw, &d, m, json!({"session": "b1"})).is_ok(),
            "{m}"
        );
        assert_eq!(
            kind(authorize(&gw, &d, m, json!({"session": "b2"}))),
            "forbidden",
            "{m}"
        );
        assert_eq!(
            kind(authorize(&gw, &d, m, json!({"session": "b9"}))),
            "forbidden",
            "{m}"
        );
        assert_eq!(
            kind(authorize(&gw, &d, m, json!({}))),
            "invalid_params",
            "{m}"
        );
    }
    // Input: only harmless event types, history only back/forward/reload.
    let ev = |e: Value| json!({"session": "b1", "events": [e], "op_id": "o"});
    assert!(
        authorize(
            &gw,
            &d,
            "browser.input",
            ev(json!({"type": "tap", "x": 1, "y": 2}))
        )
        .is_ok()
    );
    assert!(
        authorize(
            &gw,
            &d,
            "browser.input",
            ev(json!({"type": "history", "action": "back"}))
        )
        .is_ok()
    );
    for bad in [
        json!({"type": "history", "action": "goto"}),
        json!({"type": "navigate", "url": "http://localhost:22"}),
        json!({"x": 1}),
    ] {
        assert_eq!(
            kind(authorize(&gw, &d, "browser.input", ev(bad.clone()))),
            "forbidden",
            "{bad}"
        );
    }
    let mut other = ev(json!({"type": "tap", "x": 1, "y": 2}));
    other["session"] = json!("b2");
    assert_eq!(
        kind(authorize(&gw, &d, "browser.input", other)),
        "forbidden"
    );
    // Navigation: only an action, never a URL.
    assert_eq!(
        authorize(
            &gw,
            &d,
            "browser.navigate",
            json!({"session": "b1", "action": "reload", "wait": "load", "op_id": "o"})
        )
        .unwrap(),
        json!({"session": "b1", "action": "reload", "op_id": "o"})
    );
    for p in [
        json!({"session": "b1", "url": "http://localhost:5432"}),
        json!({"session": "b1", "action": "back", "path": "/admin"}),
        json!({"session": "b2", "action": "back"}),
    ] {
        assert_eq!(
            kind(authorize(&gw, &d, "browser.navigate", p.clone())),
            "forbidden",
            "{p}"
        );
    }
    assert_eq!(
        kind(authorize(
            &gw,
            &d,
            "browser.navigate",
            json!({"session": "b1", "action": "goto"})
        )),
        "invalid_params"
    );
}

#[test]
fn annotations_get_a_forced_target_and_an_author() {
    let t = tempfile::tempdir().unwrap();
    let gw = gateway(&t);
    let gh = reviewer("direct", true);
    let author = json!({"kind": "github", "id": "42", "label": "alice"});
    let r = authorize(
        &gw,
        &gh,
        "browser.annotation_add",
        json!({"kind": "element", "selector": "#a", "url": "http://x/", "note": "n",
               "image_b64": "AAAA", "author": {"kind": "github", "id": "1", "label": "eve"},
               "op_id": "o"}),
    )
    .unwrap();
    assert_eq!(r["target"], json!({"preview": "pv1"}));
    assert_eq!(r["author"], author, "the device cannot choose its author");
    assert_eq!(
        r["within"], "pv1",
        "the server checks every draft against it"
    );
    assert!(r.get("image_b64").is_none());
    assert_eq!(r["op_id"], "o");
    assert_eq!(
        kind(authorize(
            &gw,
            &gh,
            "browser.annotation_add",
            json!({"target": {"pane": "p1"}, "kind": "page"})
        )),
        "forbidden"
    );
    assert_eq!(
        kind(authorize(
            &gw,
            &gh,
            "browser.annotation_add",
            json!({"target": {"preview": "pv2"}, "kind": "page"})
        )),
        "forbidden"
    );
    assert!(
        authorize(
            &gw,
            &gh,
            "browser.annotation_add",
            json!({"target": {"preview": "pv1"}, "kind": "page"})
        )
        .is_ok()
    );
    // Reading: always its preview and its own drafts.
    assert_eq!(
        authorize(
            &gw,
            &gh,
            "browser.annotations",
            json!({"pane": "p1", "session": "b2", "draft": "d1"})
        )
        .unwrap(),
        json!({"draft": "d1", "preview": "pv1", "author": author, "within": "pv1"})
    );
    for m in [
        "browser.annotation_update",
        "browser.annotation_remove",
        "browser.annotations_clear",
    ] {
        let r = authorize(&gw, &gh, m, json!({"draft": "d1", "n": 1})).unwrap();
        assert_eq!(r["author"], author, "{m}");
        assert_eq!(
            kind(authorize(&gw, &gh, m, json!({}))),
            "invalid_params",
            "{m}"
        );
    }
    // A share opener without GitHub is named by its device.
    let link = reviewer("held", false);
    let r = authorize(&gw, &link, "browser.annotations", json!({})).unwrap();
    assert_eq!(
        r["author"],
        json!({"kind": "share", "id": "r1", "label": "@alice"})
    );
}

#[test]
fn sending_holds_or_delivers_but_never_chooses_the_agent() {
    let t = tempfile::tempdir().unwrap();
    let gw = gateway(&t);
    let direct = reviewer("direct", true);
    let r = authorize(
        &gw,
        &direct,
        "browser.annotations_send",
        json!({"draft": "d1", "op_id": "o", "copy_only": false}),
    )
    .unwrap();
    assert!(r.get("hold").is_none());
    assert!(r.get("copy_only").is_none());
    // "Send now" is the owner's: a reviewer's `force` never reaches the server.
    let r = authorize(
        &gw,
        &direct,
        "browser.annotations_send",
        json!({"draft": "d1", "force": true}),
    )
    .unwrap();
    assert!(r.get("force").is_none(), "{r}");
    let held = reviewer("held", false);
    let r = authorize(
        &gw,
        &held,
        "browser.annotations_send",
        json!({"draft": "d1"}),
    )
    .unwrap();
    assert_eq!(r["hold"], true);
    // Anything but an explicit `direct` holds.
    let odd = reviewer("", false);
    let r = authorize(
        &gw,
        &odd,
        "browser.annotations_send",
        json!({"draft": "d1"}),
    )
    .unwrap();
    assert_eq!(r["hold"], true);
    for p in [
        json!({"draft": "d1", "agent": "%3"}),
        json!({"draft": "d1", "copy_only": true}),
    ] {
        assert_eq!(
            kind(authorize(
                &gw,
                &direct,
                "browser.annotations_send",
                p.clone()
            )),
            "forbidden",
            "{p}"
        );
    }
}

#[test]
fn hello_names_the_preview_and_the_account() {
    let t = tempfile::tempdir().unwrap();
    let gw = gateway(&t);
    let h = hello(&gw, &reviewer("direct", true));
    assert_eq!(h["kind"], "share");
    assert_eq!(
        h["review"],
        json!({"preview": "pv1", "sid": "abcdefghijklmnop"})
    );
    assert_eq!(h["github"], json!({"id": 42, "login": "alice"}));
    assert_eq!(h["scope"], json!(Scope::View));
    for m in ["browser.annotation_add", "browser.annotations_send"] {
        assert!(is_mutating(m));
    }
    for m in [
        "review.info",
        "browser.annotations",
        "browser.screencast_frame",
    ] {
        assert!(!is_mutating(m));
    }
}
