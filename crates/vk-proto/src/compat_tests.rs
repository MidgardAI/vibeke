//! Render-stream wire compatibility (Goal 03 Codex review, finding 9).
//!
//! Postcard is positional: appending `Pane.browser` changed where the next pane and every
//! following `SessionModel` field start, even when `browser` is `None` (`#[serde(default)]`
//! only helps self-describing formats such as the store's JSON). These fixtures encode a model
//! with several panes and non-empty following fields in the protocol-1 shape and in the
//! current shape, and decode each with the other: both directions must fail or decode to
//! something else, which is why [`PROTOCOL`] changed and `render.attach` refuses a mismatch.

use crate::model::*;
use crate::render::{PROTOCOL, check_attach_reply};
use serde::{Deserialize, Serialize};

/// `Pane` as protocol 1 encoded it (before browser panes).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PaneV1 {
    id: String,
    handle: String,
    tab: String,
    workspace: String,
    title: Option<String>,
    auto_title: String,
    cwd: Option<String>,
    cols: u16,
    rows: u16,
    child_pid: Option<u32>,
    fg_cmdline: Vec<String>,
    exited: bool,
    exit_code: Option<i32>,
    unread: bool,
    marked_unread: bool,
    pinned: bool,
    created_by: String,
    recovered: Option<String>,
    isolation: Isolation,
}

/// `SessionModel` as protocol 1 encoded it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct SessionModelV1 {
    session: String,
    machine: String,
    server_version: String,
    workspaces: Vec<Workspace>,
    tabs: Vec<Tab>,
    panes: Vec<PaneV1>,
    runs: Vec<AgentRun>,
    interactions: Vec<Interaction>,
    tasks: Vec<Task>,
    degraded: Option<String>,
    previews: Vec<Preview>,
    groups: Vec<Group>,
    appearance: Appearance,
}

fn pane_v1(id: &str) -> PaneV1 {
    PaneV1 {
        id: id.into(),
        handle: format!("w1:{id}"),
        tab: "T".into(),
        workspace: "W".into(),
        title: Some(format!("title {id}")),
        auto_title: "zsh".into(),
        cwd: Some("/src/app".into()),
        cols: 80,
        rows: 24,
        child_pid: Some(4242),
        fg_cmdline: vec!["pnpm".into(), "dev".into()],
        exited: false,
        exit_code: None,
        unread: true,
        marked_unread: false,
        pinned: false,
        created_by: "user".into(),
        recovered: None,
        isolation: Isolation::default(),
    }
}

fn to_current(p: &PaneV1, browser: Option<BrowserPane>) -> Pane {
    Pane {
        id: p.id.clone(),
        handle: p.handle.clone(),
        tab: p.tab.clone(),
        workspace: p.workspace.clone(),
        title: p.title.clone(),
        auto_title: p.auto_title.clone(),
        cwd: p.cwd.clone(),
        cols: p.cols,
        rows: p.rows,
        child_pid: p.child_pid,
        fg_cmdline: p.fg_cmdline.clone(),
        exited: p.exited,
        exit_code: p.exit_code,
        unread: p.unread,
        marked_unread: p.marked_unread,
        pinned: p.pinned,
        created_by: p.created_by.clone(),
        recovered: p.recovered.clone(),
        isolation: p.isolation.clone(),
        browser,
    }
}

fn following() -> (Option<String>, Vec<Preview>, Vec<Group>, Appearance) {
    (
        Some("store degraded: disk full".into()),
        vec![Preview {
            id: "01PREVIEW".into(),
            handle: "v1".into(),
            machine: "laptop".into(),
            pane: Some("p1".into()),
            task: None,
            port: 5173,
            path: "/".into(),
            label: Some("web".into()),
            url: "http://localhost:5173/".into(),
            scheme: "http".into(),
            status: PreviewStatus::Up,
            source: PreviewSource::Declared,
            pid: Some(77),
            first_seen_ms: 1,
            last_seen_ms: 2,
        }],
        vec![Group {
            id: "G".into(),
            handle: "g1".into(),
            name: "frontend".into(),
            parent: None,
            collapsed: false,
            order: 1.0,
            workspaces: vec!["W".into()],
        }],
        Appearance {
            known: true,
            dark: true,
            mode: "auto".into(),
            theme: "vibeke-dark".into(),
            source: "c-1".into(),
        },
    )
}

fn workspace() -> Workspace {
    Workspace {
        id: "W".into(),
        handle: "w1".into(),
        name: None,
        auto_name: "app".into(),
        root_path: "/src/app".into(),
        task: None,
        order: 1.0,
        branch: Some("main".into()),
    }
}

fn v1_model() -> SessionModelV1 {
    let (degraded, previews, groups, appearance) = following();
    SessionModelV1 {
        session: "main".into(),
        machine: "laptop".into(),
        server_version: "0.9.0".into(),
        workspaces: vec![workspace()],
        tabs: vec![],
        panes: vec![pane_v1("p1"), pane_v1("p2"), pane_v1("p3")],
        runs: vec![],
        interactions: vec![],
        tasks: vec![],
        degraded,
        previews,
        groups,
        appearance,
    }
}

fn current_model(browser_on_p2: bool) -> SessionModel {
    let v1 = v1_model();
    let browser = browser_on_p2.then(|| BrowserPane {
        url: "http://localhost:5173/".into(),
        history: vec!["http://localhost:5173/".into()],
        ..Default::default()
    });
    SessionModel {
        session: v1.session.clone(),
        machine: v1.machine.clone(),
        server_version: v1.server_version.clone(),
        workspaces: v1.workspaces.clone(),
        tabs: vec![],
        panes: vec![
            to_current(&v1.panes[0], None),
            to_current(&v1.panes[1], browser),
            to_current(&v1.panes[2], None),
        ],
        runs: vec![],
        interactions: vec![],
        tasks: vec![],
        degraded: v1.degraded.clone(),
        previews: v1.previews.clone(),
        groups: v1.groups.clone(),
        appearance: v1.appearance.clone(),
        pane_live: vec![],
    }
}

fn encode<T: Serialize>(v: &T) -> Vec<u8> {
    postcard::to_stdvec(v).unwrap()
}

#[test]
fn current_roundtrips_with_several_panes_and_following_fields() {
    for browser in [false, true] {
        let m = current_model(browser);
        let back: SessionModel = postcard::from_bytes(&encode(&m)).unwrap();
        assert_eq!(back, m);
        let framed = crate::frame::encode(&m).unwrap();
        let back: SessionModel = crate::frame::decode(&framed[4..]).unwrap();
        assert_eq!(back, m);
    }
}

/// An old (protocol 1) server's model read by a current client: never the same model.
#[test]
fn old_model_does_not_decode_as_current() {
    let old = v1_model();
    let bytes = encode(&old);
    let expected = current_model(false);
    match postcard::from_bytes::<SessionModel>(&bytes) {
        Err(_) => {}
        Ok(m) => assert_ne!(m, expected, "protocol 1 bytes must not pass as protocol 2"),
    }
}

/// A current server's model read by an old (protocol 1) client: never the same model, with
/// or without a browser pane in it.
#[test]
fn current_model_does_not_decode_as_old() {
    let expected = v1_model();
    for browser in [false, true] {
        let bytes = encode(&current_model(browser));
        match postcard::from_bytes::<SessionModelV1>(&bytes) {
            Err(_) => {}
            Ok(m) => assert_ne!(m, expected, "browser={browser}"),
        }
    }
}

/// So the versions must not mix: the protocol number moved past the protocol-1 shape and the
/// client refuses a server that answers `render.attach` with another number (or none).
#[test]
fn negotiation_refuses_mixed_versions() {
    const { assert!(PROTOCOL >= 2) };
    let ok = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {"protocol": PROTOCOL, "client_id": "c"}});
    assert!(check_attach_reply(&ok).is_ok());
    let old =
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {"protocol": 1, "client_id": "c"}});
    let e = check_attach_reply(&old).unwrap_err();
    assert!(
        e.starts_with("version_mismatch") && e.contains("upgrade"),
        "{e}"
    );
    let none = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {"client_id": "c"}});
    assert!(check_attach_reply(&none).is_err());
    let refused = serde_json::json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32007, "message": "version_mismatch: …", "data": {"kind": "version_mismatch"}}});
    assert!(
        check_attach_reply(&refused)
            .unwrap_err()
            .contains("version_mismatch")
    );
}

/// Protocol 4 appended `SessionModel.pane_live` and `Row.mark`/`Row.links`: a protocol-3 peer
/// would misread a model with live pane state, so the number moved again.
#[test]
fn protocol_4_terminal_effects_change_the_shape() {
    const { assert!(PROTOCOL >= 4) };
    let mut m = current_model(false);
    m.pane_live = vec![PaneLive {
        pane: "p1".into(),
        progress: Some(Progress {
            state: ProgressState::Normal,
            pct: Some(40),
        }),
        last_exit: Some(ExitMark { code: 2, at_ms: 5 }),
        user_vars: vec![("k".into(), "v".into())],
    }];
    let back: SessionModel = postcard::from_bytes(&encode(&m)).unwrap();
    assert_eq!(back, m);
    assert_eq!(back.live("p1").unwrap().progress.unwrap().pct, Some(40));
    // Encoded with and without live state, the bytes differ in length (positional tail).
    assert_ne!(encode(&m).len(), encode(&current_model(false)).len());
}
