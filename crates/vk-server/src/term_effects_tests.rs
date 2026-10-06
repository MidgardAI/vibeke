//! Terminal effects end to end on the server (03 §8): engine effects routed through
//! `Server::pane_effect` into `SessionModel.pane_live`, `pane.read --source last-command`,
//! and the OSC 52 read round trip over a real render stream.

use super::*;
use crate::api::Ctx;
use crate::core::Tx;
use crate::pane::{PaneCmd, PaneRt};
use crate::paths::Paths;
use crate::{Server, ServerOpts};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::mpsc;
use vk_proto::frame::asyncio;
use vk_proto::model::{Pane, ProgressState};
use vk_proto::render::{ClientFrame, ClipSel, PaneRect, ServerFrame};

struct Env {
    _dir: tempfile::TempDir,
    server: Arc<Server>,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let paths = Paths {
        session: "t".into(),
        runtime: root.join("run"),
        state: root.join("state"),
    };
    let opts = ServerOpts {
        session: "t".into(),
        machine: "m".into(),
        bin: "/bin/false".into(),
        hold_args: vec![],
        default_shell: None,
        env: vec![],
        shims: false,
    };
    Env {
        _dir: dir,
        server: Server::new(paths, opts).unwrap(),
    }
}

impl Env {
    /// A pane in the model with a live engine (no holder): returns its command receiver, which
    /// sees every write the server makes to the pane.
    fn pane(&self, id: &str) -> (Arc<PaneRt>, mpsc::UnboundedReceiver<PaneCmd>) {
        let p = Pane {
            id: id.into(),
            handle: id.into(),
            tab: "T".into(),
            workspace: "W".into(),
            title: None,
            auto_title: String::new(),
            cwd: None,
            cols: 40,
            rows: 10,
            child_pid: None,
            fg_cmdline: vec![],
            exited: false,
            exit_code: None,
            unread: false,
            marked_unread: false,
            pinned: false,
            created_by: "user".into(),
            recovered: None,
            isolation: Default::default(),
            browser: None,
        };
        {
            let mut c = self.server.core.lock().unwrap();
            let mut tx = Tx::new();
            tx.pane(p);
            self.server.commit(&mut c, tx).unwrap();
        }
        let (rt, rx) = PaneRt::new(id, 40, 10);
        self.server
            .panes
            .lock()
            .unwrap()
            .insert(id.into(), rt.clone());
        (rt, rx)
    }

    /// What the pane task does with output: feed the engine, route non-reply effects.
    fn output(&self, rt: &PaneRt, bytes: &[u8]) {
        let mut fx = Vec::new();
        rt.screen.lock().unwrap().engine.feed(bytes, &mut fx);
        for e in fx {
            if !matches!(e, vk_term::Effect::Reply(_)) {
                self.server.pane_effect(&rt.id, e, false);
            }
        }
    }

    fn live(&self, pane: &str) -> Option<vk_proto::model::PaneLive> {
        self.server.with_core(|c| c.model.live(pane).cloned())
    }

    async fn call(&self, method: &str, params: Value) -> Value {
        let ctx = Ctx {
            client_id: "c-user".into(),
            kind: "cli".into(),
            pane_scope: None,
            remote: false,
        };
        let line = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        serde_json::from_str(&crate::api::handle_line(&self.server, &ctx, &line.to_string()).await)
            .unwrap()
    }
}

#[test]
fn progress_exit_and_user_vars_reach_the_model() {
    let e = env();
    let (rt, _rx) = e.pane("P1");
    let rev0 = *e.server.model_rev.borrow();
    e.output(&rt, b"\x1b]9;4;1;42\x07");
    let l = e.live("P1").unwrap();
    let p = l.progress.unwrap();
    assert_eq!((p.state, p.pct), (ProgressState::Normal, Some(42)));
    assert!(*e.server.model_rev.borrow() > rev0, "model bumped");
    // Same value again: no model bump.
    let rev1 = *e.server.model_rev.borrow();
    e.output(&rt, b"\x1b]9;4;1;42\x07");
    assert_eq!(*e.server.model_rev.borrow(), rev1);
    e.output(&rt, b"\x1b]9;4;3\x07");
    let p = e.live("P1").unwrap().progress.unwrap();
    assert_eq!((p.state, p.pct), (ProgressState::Indeterminate, None));
    e.output(&rt, b"\x1b]9;4;2;250\x07");
    let p = e.live("P1").unwrap().progress.unwrap();
    assert_eq!((p.state, p.pct), (ProgressState::Error, Some(100)));
    e.output(&rt, b"\x1b]9;4;0\x07");
    assert!(e.live("P1").is_none(), "cleared progress drops the entry");

    // Exit codes: non-zero shows, zero and the next command clear it.
    e.output(
        &rt,
        b"\x1b]133;A\x07$ \x1b]133;B\x07false\r\n\x1b]133;C\x07\x1b]133;D;3\x07",
    );
    assert_eq!(e.live("P1").unwrap().last_exit.unwrap().code, 3);
    e.output(&rt, b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07");
    assert!(e.live("P1").is_none());
    e.output(&rt, b"\x1b]133;D;0\x07");
    assert!(e.live("P1").is_none());

    // User vars: sorted, updated, removed by an empty value, bounded.
    e.output(
        &rt,
        b"\x1b]1337;SetUserVar=zeta=MQ==\x07\x1b]1337;SetUserVar=alpha=Mg==\x07",
    );
    assert_eq!(
        e.live("P1").unwrap().user_vars,
        vec![("alpha".into(), "2".into()), ("zeta".into(), "1".into())]
    );
    e.output(&rt, b"\x1b]1337;SetUserVar=zeta=\x07");
    assert_eq!(
        e.live("P1").unwrap().user_vars,
        vec![("alpha".into(), "2".into())]
    );
    for i in 0..(USER_VARS_MAX + 5) {
        user_var(&e.server, "P1", format!("v{i:03}"), "x".into());
    }
    assert_eq!(e.live("P1").unwrap().user_vars.len(), USER_VARS_MAX);

    // Replayed effects (journal replay after a restart) change nothing.
    e.server.pane_effect(
        "P1",
        vk_term::Effect::Progress {
            state: 1,
            pct: Some(5),
        },
        true,
    );
    assert!(e.live("P1").unwrap().progress.is_none());

    // A closed pane forgets its live state.
    forget(&e.server, "P1");
    assert!(e.live("P1").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn pane_get_and_read_last_command() {
    let e = env();
    let (rt, _rx) = e.pane("P1");
    let r = e
        .call("pane.read", json!({"pane": "P1", "source": "last-command"}))
        .await;
    assert_eq!(r["error"]["data"]["details"]["reason"], "no_marks", "{r}");
    e.output(
        &rt,
        b"\x1b]133;A\x07$ \x1b]133;B\x07make\r\n\x1b]133;C\x07compiling\r\nerror: boom\r\n\x1b]133;D;2\x07\x1b]133;A\x07$ \x1b]133;B\x07",
    );
    let r = e
        .call("pane.read", json!({"pane": "P1", "source": "last-command"}))
        .await;
    let res = &r["result"];
    assert_eq!(res["text"], "compiling\nerror: boom", "{r}");
    assert_eq!(res["exit_code"], 2);
    assert_eq!(res["running"], false);
    assert_eq!(res["source"], "last-command");
    let r = e
        .call(
            "pane.read",
            json!({"pane": "P1", "source": "last-command", "lines": 1}),
        )
        .await;
    assert_eq!(r["result"]["text"], "error: boom");
    let r = e.call("pane.get", json!({"pane": "P1"})).await;
    assert_eq!(r["result"]["live"]["last_exit"]["code"], 2, "{r}");
}

/// OSC 52 read over a real render stream: the query reaches the client showing the pane, a
/// granted reply is written to the pane as an OSC 52 answer, a denial writes nothing, and
/// replies for unknown / other clients' requests are ignored.
#[tokio::test(flavor = "multi_thread")]
async fn clipboard_read_round_trip() {
    let e = env();
    let (rt, mut rx) = e.pane("P1");
    // Nobody shows the pane: denied without asking anyone.
    assert_eq!(clipboard_query(&e.server, "P1", false), None);

    let (client, server_side) = tokio::io::duplex(1 << 20);
    let (srd, swr) = tokio::io::split(server_side);
    let s2 = e.server.clone();
    tokio::spawn(async move {
        let _ = crate::render::serve(s2, srd, swr, "tui-a".into(), false, 60).await;
    });
    let (mut crd, mut cwr) = tokio::io::split(client);
    asyncio::write_frame(
        &mut cwr,
        &ClientFrame::ViewHint {
            panes: vec![PaneRect {
                pane: "P1".into(),
                cols: 40,
                rows: 10,
            }],
            active: true,
        },
    )
    .await
    .unwrap();
    tokio::io::AsyncWriteExt::flush(&mut cwr).await.unwrap();
    // Wait until the session registered the view.
    let t = std::time::Instant::now();
    while !e
        .server
        .clients
        .lock()
        .unwrap()
        .get("tui-a")
        .is_some_and(|c| c.visible.iter().any(|p| p == "P1"))
    {
        assert!(t.elapsed() < std::time::Duration::from_secs(5));
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    // The app asks for the clipboard (OSC 52 read).
    e.output(&rt, b"\x1b]52;c;?\x07");
    let req = loop {
        let f = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            asyncio::read_frame::<_, ServerFrame>(&mut crd),
        )
        .await
        .expect("ClipboardQuery frame")
        .unwrap();
        if let ServerFrame::ClipboardQuery {
            req,
            pane,
            selection,
        } = f
        {
            assert_eq!(pane, "P1");
            assert_eq!(selection, ClipSel::Clipboard);
            break req;
        }
    };
    // A reply from the wrong pane is ignored; the right one is written to the pane.
    assert_eq!(
        clipboard_reply(&e.server, "tui-a", req, "P2", Some(b"x".to_vec())),
        None
    );
    assert_eq!(
        clipboard_reply(&e.server, "tui-b", req, "P1", Some(b"x".to_vec())),
        None
    );
    asyncio::write_frame(
        &mut cwr,
        &ClientFrame::ClipboardReply {
            req,
            pane: "P1".into(),
            data: Some(b"secret".to_vec()),
        },
    )
    .await
    .unwrap();
    tokio::io::AsyncWriteExt::flush(&mut cwr).await.unwrap();
    let written = loop {
        match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("pane write")
            .unwrap()
        {
            PaneCmd::Input { bytes, .. } => break bytes,
            _ => continue,
        }
    };
    assert_eq!(written, b"\x1b]52;c;c2VjcmV0\x1b\\");
    // Answered once: a second reply for the same request writes nothing.
    assert_eq!(
        clipboard_reply(&e.server, "tui-a", req, "P1", Some(b"again".to_vec())),
        None
    );
    // A denied read writes nothing.
    let req2 = clipboard_query(&e.server, "P1", true).unwrap();
    assert_eq!(clipboard_reply(&e.server, "tui-a", req2, "P1", None), None);
    assert!(rx.try_recv().is_err(), "nothing written for a denial");
    // An app spamming reads gets at most two open queries.
    assert!(clipboard_query(&e.server, "P1", false).is_some());
    assert!(clipboard_query(&e.server, "P1", false).is_some());
    assert_eq!(clipboard_query(&e.server, "P1", false), None);
    assert_eq!(osc52_reply(b"hi", true), b"\x1b]52;p;aGk=\x1b\\");
}

/// Inbound kitty graphics over a real render stream (03 §9): the pixels go once per hash,
/// placements after the cell frame, and an unchanged image is not resent when it moves.
#[tokio::test(flavor = "multi_thread")]
async fn kitty_images_reach_the_client_once_per_hash() {
    use base64::Engine as _;
    let e = env();
    let (rt, _rx) = e.pane("P1");
    let (client, server_side) = tokio::io::duplex(1 << 22);
    let (srd, swr) = tokio::io::split(server_side);
    let s2 = e.server.clone();
    tokio::spawn(async move {
        let _ = crate::render::serve(s2, srd, swr, "tui-a".into(), false, 60).await;
    });
    let (mut crd, mut cwr) = tokio::io::split(client);
    asyncio::write_frame(
        &mut cwr,
        &ClientFrame::ViewHint {
            panes: vec![PaneRect {
                pane: "P1".into(),
                cols: 40,
                rows: 10,
            }],
            active: true,
        },
    )
    .await
    .unwrap();
    tokio::io::AsyncWriteExt::flush(&mut cwr).await.unwrap();
    let px = vec![200u8; 4 * 2 * 4];
    let b64 = base64::engine::general_purpose::STANDARD.encode(&px);
    let img = format!("\x1b[3;5H\x1b_Ga=T,i=1,f=32,s=4,v=2,c=3,r=2,q=2;{b64}\x1b\\");
    e.output(&rt, img.as_bytes());
    rt.rev_tx.send_modify(|r| *r += 1);
    e.server.screen_dirty.notify_waiters();
    let mut images = Vec::new();
    let mut places = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while places.is_none() {
        assert!(std::time::Instant::now() < deadline, "no PaneImages");
        let f = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            asyncio::read_frame::<_, ServerFrame>(&mut crd),
        )
        .await
        .unwrap()
        .unwrap();
        match f {
            ServerFrame::Image {
                hash,
                width,
                height,
                rgba_z,
            } => {
                assert_eq!((width, height), (4, 2));
                assert_eq!(vk_browser::kitty::unzlib(&rgba_z).unwrap(), px);
                images.push(hash);
            }
            ServerFrame::PaneImages {
                pane, places: p, ..
            } => {
                assert_eq!(pane, "P1");
                places = Some(p);
            }
            ServerFrame::PaneFull {
                pane, epoch, rev, ..
            } => {
                asyncio::write_frame(&mut cwr, &ClientFrame::Ack { pane, epoch, rev })
                    .await
                    .unwrap();
                tokio::io::AsyncWriteExt::flush(&mut cwr).await.unwrap();
            }
            _ => {}
        }
    }
    let places = places.unwrap();
    assert_eq!(images.len(), 1);
    assert_eq!(places.len(), 1);
    let p = &places[0];
    assert_eq!(
        (p.hash.as_str(), p.col, p.row, p.cols, p.rows),
        (images[0].as_str(), 4, 2, 3, 2)
    );
    // Scroll: the placement moves up; no second Image frame.
    e.output(&rt, b"\x1b[10;1H\r\n");
    rt.rev_tx.send_modify(|r| *r += 1);
    e.server.screen_dirty.notify_waiters();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        assert!(std::time::Instant::now() < deadline, "no moved placement");
        let f = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            asyncio::read_frame::<_, ServerFrame>(&mut crd),
        )
        .await
        .unwrap()
        .unwrap();
        match f {
            ServerFrame::Image { .. } => panic!("image resent"),
            ServerFrame::PaneImages { places, .. } => {
                assert_eq!(places[0].row, 1);
                break;
            }
            ServerFrame::PaneDiff {
                pane, epoch, rev, ..
            } => {
                asyncio::write_frame(&mut cwr, &ClientFrame::Ack { pane, epoch, rev })
                    .await
                    .unwrap();
                tokio::io::AsyncWriteExt::flush(&mut cwr).await.unwrap();
            }
            _ => {}
        }
    }
}

#[test]
fn pane_cwd_falls_back_to_the_process_cwd() {
    let e = env();
    let (rt, _rx) = e.pane("P1");
    // No OSC 7, no status: the model cwd (none here).
    assert_eq!(e.server.pane_cwd("P1"), None);
    // The holder reported a foreground process: its cwd is read live (this test process).
    let here = std::env::current_dir()
        .unwrap()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    *rt.status.lock().unwrap() = Some(vk_proto::holder::ProcStatus {
        child_pid: std::process::id(),
        fg_pgid: None,
        fg_cmdline: vec![],
        fg_exe: None,
        fg_cwd: Some("/stale".into()),
        exited: false,
        exit_code: None,
        signal: None,
    });
    let got = e.server.pane_cwd("P1").unwrap();
    assert_eq!(
        std::path::Path::new(&got).canonicalize().unwrap(),
        std::path::Path::new(&here)
    );
    // A dead pid: the holder's last report.
    rt.status.lock().unwrap().as_mut().unwrap().child_pid = 0;
    rt.status.lock().unwrap().as_mut().unwrap().fg_pgid = Some(u32::MAX - 7);
    assert_eq!(e.server.pane_cwd("P1").as_deref(), Some("/stale"));
    // OSC 7 wins.
    e.output(&rt, b"\x1b]7;file://h/tmp/osc7\x07");
    assert_eq!(e.server.pane_cwd("P1").as_deref(), Some("/tmp/osc7"));
}
