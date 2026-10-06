//! In-process tests for the M4 parity pieces that need a `Server` but no panes: the
//! notification pipeline with an injected notifier, theme propagation, groups.

use crate::api::Ctx;
use crate::core::Tx;
use crate::notify::{MemNotifier, set_notifier};
use crate::paths::Paths;
use crate::{ClientState, Server, ServerOpts};
use serde_json::json;
use std::sync::{Arc, Once};
use vk_proto::model::*;

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Same values as the review/sandbox tests: one consistent root per test binary.
        let base = std::env::temp_dir().join(format!("vk-review-tests-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        // SAFETY: identical values to the other writers; set before servers read them.
        unsafe {
            std::env::set_var("VIBEKE_RUNTIME_DIR", base.join("run"));
            std::env::set_var("VIBEKE_STATE_DIR", base.join("state"));
            std::env::set_var("VIBEKE_CONFIG", base.join("config.toml"));
        }
    });
}

struct Env {
    server: Arc<Server>,
    _dir: tempfile::TempDir,
}

fn env() -> Env {
    init_env();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let paths = Paths {
        session: "t".into(),
        runtime: root.join("run"),
        state: root.join("state"),
    };
    let opts = ServerOpts {
        session: "t".into(),
        machine: "testbox".into(),
        bin: "/bin/false".into(),
        hold_args: vec![],
        default_shell: None,
        env: vec![("TERM_PROGRAM".into(), "ghostty".into())],
        shims: false,
    };
    Env {
        server: Server::new(paths, opts).unwrap(),
        _dir: dir,
    }
}

fn user() -> Ctx {
    Ctx {
        client_id: "c-user".into(),
        kind: "cli".into(),
        pane_scope: None,
        remote: false,
    }
}

#[test]
fn native_pipeline_coalesces_and_respects_presence() {
    let e = env();
    // Headless: no client reported a host terminal → never a native notification.
    let n = e.server.notify("plugin", Some("p1"), "t", "b", "normal");
    assert!(
        n.channels.iter().any(|c| c == "native:headless"),
        "{:?}",
        n.channels
    );
    let mem = Arc::new(MemNotifier::default());
    set_notifier(&e.server, Some(mem.clone()));
    let n = e.server.notify("plugin", Some("p2"), "first", "", "normal");
    assert_eq!(n.channels, vec!["toast", "native"]);
    // Same pane inside coalesce_ms (3000 by default): merged, no second popup.
    let n = e
        .server
        .notify("plugin", Some("p2"), "second", "", "normal");
    assert!(
        n.channels.contains(&"coalesced".to_string()),
        "{:?}",
        n.channels
    );
    assert_eq!(mem.notes.lock().unwrap().len(), 1);
    // Focused + visible in a host-focused client: toast only.
    e.server.clients.lock().unwrap().insert(
        "tui1".into(),
        ClientState {
            kind: "tui".into(),
            focus: ClientFocus {
                pane: Some("p3".into()),
                ..Default::default()
            },
            visible: vec!["p3".into()],
            host_focused: true,
            ..Default::default()
        },
    );
    let n = e.server.notify("plugin", Some("p3"), "here", "", "normal");
    assert_eq!(n.channels, vec!["suppressed:focused", "toast"]);
    assert_eq!(mem.notes.lock().unwrap().len(), 1);
    // Bell notifications are off by default (notifications.on.bell = false).
    let n = e.server.notify("bell", Some("p4"), "bell", "", "normal");
    assert!(n.channels.contains(&"filtered:bell".to_string()));
    // The stored copy carries the decision too (notification.list).
    let stored = e
        .server
        .with_core(|c| c.notifications.iter().find(|x| x.id == n.id).cloned())
        .unwrap();
    assert_eq!(stored.channels, n.channels);
}

#[test]
fn client_focus_raises_host_from_server_env() {
    let e = env();
    let mem = Arc::new(MemNotifier::default());
    set_notifier(&e.server, Some(mem.clone()));
    let r = crate::notify::api(
        &e.server,
        &user(),
        "client.focus",
        &json!({"url": "vibeke://focus?session=other&pane=x"}),
    )
    .unwrap();
    assert!(r.unwrap_err().message.contains("session other"));
    let pane_ctx = Ctx {
        pane_scope: Some("p".into()),
        ..user()
    };
    let r =
        crate::notify::api(&e.server, &pane_ctx, "client.focus", &json!({"pane": "x"})).unwrap();
    assert_eq!(r.unwrap_err().data.kind, "permission_denied");
    // Host metadata recorded at attach wins over the server env.
    crate::notify::record_host(
        &e.server,
        "tui9",
        Some(&json!({"bundle_id": "com.example.term"})),
    );
    assert_eq!(
        crate::notify::hosts(&e.server)
            .get("tui9")
            .and_then(|h| h.bundle()),
        Some("com.example.term".into())
    );
    assert!(mem.raised.lock().unwrap().is_empty());
}

#[test]
fn theme_reports_propagate_to_model_events_and_pane_env() {
    let e = env();
    let a = e.server.theme.current();
    assert!(!a.known);
    let mut envv = vec![];
    crate::theme::pane_env(&e.server, &mut envv);
    assert!(!envv.iter().any(|(k, _)| k == "COLORFGBG"));
    let r = crate::theme::api(
        &e.server,
        &user(),
        "client.appearance",
        &json!({"dark": false, "source": "osc11"}),
    )
    .unwrap()
    .unwrap();
    assert_eq!(r["colorfgbg"], "0;15");
    let model = e.server.with_core(|c| c.model.appearance.clone());
    assert!(model.known && !model.dark);
    assert_eq!(model.theme, "catppuccin-latte");
    let mut envv = vec![];
    crate::theme::pane_env(&e.server, &mut envv);
    assert!(envv.contains(&("COLORFGBG".into(), "0;15".into())));
    assert!(envv.contains(&("VIBEKE_THEME".into(), "light".into())));
    let evs = e
        .server
        .with_core(|c| c.store.events_after(0, 100, &["theme.changed".into()]))
        .unwrap();
    assert_eq!(evs.len(), 1);
    // A forced mode wins over reports; unchanged recomputes emit nothing.
    crate::theme::api(
        &e.server,
        &user(),
        "theme.set_mode",
        &json!({"mode": "dark"}),
    )
    .unwrap()
    .unwrap();
    crate::theme::api(&e.server, &user(), "theme.get", &json!({}))
        .unwrap()
        .unwrap();
    let evs = e
        .server
        .with_core(|c| c.store.events_after(0, 100, &["theme.changed".into()]))
        .unwrap();
    assert_eq!(evs.len(), 2);
    assert!(e.server.theme.current().dark);
    assert!(
        crate::theme::api(
            &e.server,
            &user(),
            "theme.set_mode",
            &json!({"mode": "sepia"})
        )
        .unwrap()
        .is_err()
    );
}

fn put_ws(e: &Env, id: &str) {
    let mut c = e.server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.ws(Workspace {
        id: id.into(),
        handle: format!("w-{id}"),
        name: Some(id.into()),
        auto_name: id.into(),
        root_path: "/tmp".into(),
        task: None,
        order: 1.0,
        branch: None,
    });
    e.server.commit(&mut c, tx).unwrap();
}

#[tokio::test]
async fn groups_lifecycle_and_scope() {
    let e = env();
    put_ws(&e, "a");
    put_ws(&e, "b");
    let call = |m: &str, p: serde_json::Value| {
        let s = e.server.clone();
        let m = m.to_string();
        async move { crate::parity::api(&s, &user(), &m, &p).await.unwrap() }
    };
    let g = call("group.create", json!({"name": "clients"}))
        .await
        .unwrap();
    let gid = g["group"]["id"].as_str().unwrap().to_string();
    assert_eq!(g["group"]["handle"], "g1");
    assert!(
        call("group.create", json!({"name": "clients"}))
            .await
            .is_err()
    );
    let sub = call("group.create", json!({"name": "sub", "parent": "clients"}))
        .await
        .unwrap();
    call("group.add", json!({"group": "clients", "workspace": "a"}))
        .await
        .unwrap();
    call(
        "workspace.move",
        json!({"workspace": "b", "group": sub["group"]["id"]}),
    )
    .await
    .unwrap();
    let l = call("group.list", json!({})).await.unwrap();
    assert_eq!(l["groups"][0]["workspaces"], json!(["a"]));
    assert_eq!(l["ungrouped"], json!([]));
    // A group can't move under its own child.
    assert!(
        call("group.move", json!({"group": "clients", "parent": "sub"}))
            .await
            .is_err()
    );
    let c = call("group.collapse", json!({"group": gid})).await.unwrap();
    assert_eq!(c["group"]["collapsed"], true);
    // Deleting `sub` moves `b` up into `clients`.
    call("group.delete", json!({"group": "sub"})).await.unwrap();
    let g2 = e.server.with_core(|c| c.group("clients").cloned()).unwrap();
    assert_eq!(g2.workspaces, vec!["a", "b"]);
    // Persisted: a reload sees the groups.
    let reloaded = e
        .server
        .with_core(|c| c.store.load::<Group>("group").unwrap());
    assert_eq!(reloaded.len(), 1);
    // Agents can't reorganise the user's workspaces.
    let pane_ctx = Ctx {
        pane_scope: Some("p".into()),
        ..user()
    };
    let r = crate::parity::api(&e.server, &pane_ctx, "group.create", &json!({"name": "x"}))
        .await
        .unwrap();
    assert_eq!(r.unwrap_err().data.kind, "permission_denied");
}
