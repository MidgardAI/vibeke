//! Event push on the render stream (07 §3): subscription filters, replay, live delivery and
//! the `client.attached` / `client.detached` events.

use super::*;
use crate::ServerOpts;
use crate::paths::Paths;
use serde_json::json;

fn server() -> (tempfile::TempDir, Arc<Server>) {
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
        gateway: None,
    };
    (dir, Server::new(paths, opts).unwrap())
}

fn emit(server: &Server, kind: &str, confirm: &str) {
    let mut c = server.core.lock().unwrap();
    let mut tx = crate::core::Tx::new();
    tx.event(kind, json!({"confirm": confirm}), json!({"title": "t"}));
    server.commit(&mut c, tx).unwrap();
}

#[test]
fn subscription_filters_and_replays() {
    let (_d, srv) = server();
    emit(&srv, "client.confirm_requested", "old");
    emit(&srv, "pane.focused", "x");
    let mut p = EventPush::default();
    // Replay after 0: only matching kinds.
    let back = p.subscribe(
        &srv,
        vec!["client.confirm_*".into(), "interaction.*".into()],
        Some(0),
    );
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].kind, "client.confirm_requested");
    let v: serde_json::Value = serde_json::from_str(&back[0].json).unwrap();
    assert_eq!(v["subject"]["confirm"], "old");
    assert_eq!(v["type"], "client.confirm_requested");
    // Live: filtered, and an event seen twice (replay/live overlap) is delivered once.
    emit(&srv, "interaction.opened", "i1");
    emit(&srv, "tab.created", "t1");
    emit(&srv, "client.confirm_resolved", "old");
    let mut live = Vec::new();
    let rx = p.rx.as_mut().unwrap();
    while let Ok(e) = rx.try_recv() {
        live.push(e);
    }
    let mut dup = live.clone();
    dup.insert(1, live[0].clone());
    let got = p.filter(&dup);
    let kinds: Vec<&str> = got.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds, ["interaction.opened", "client.confirm_resolved"]);
    // Without `after`: live only. Empty types: unsubscribed.
    assert!(p.subscribe(&srv, vec!["client.*".into()], None).is_empty());
    assert!(p.subscribe(&srv, vec![], None).is_empty());
    assert!(p.rx.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn render_stream_pushes_subscribed_events() {
    let (_d, srv) = server();
    let (client, server_side) = tokio::io::duplex(1 << 20);
    let (srd, swr) = tokio::io::split(server_side);
    let s2 = srv.clone();
    tokio::spawn(async move {
        let _ = serve(s2, srd, swr, "tui-a".into(), false, 60).await;
    });
    let (mut crd, mut cwr) = tokio::io::split(client);
    asyncio::write_frame(
        &mut cwr,
        &ClientFrame::Subscribe {
            types: vec!["client.*".into()],
            after: None,
        },
    )
    .await
    .unwrap();
    tokio::io::AsyncWriteExt::flush(&mut cwr).await.unwrap();
    // Hello and Model come first; give the session a moment to apply the subscription.
    tokio::time::sleep(Duration::from_millis(200)).await;
    emit(&srv, "pane.focused", "ignored");
    emit(&srv, "client.confirm_requested", "c1");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got = Vec::new();
    while got.is_empty() {
        assert!(Instant::now() < deadline, "no Events frame");
        let f = tokio::time::timeout(
            Duration::from_secs(5),
            asyncio::read_frame::<_, ServerFrame>(&mut crd),
        )
        .await
        .unwrap()
        .unwrap();
        if let ServerFrame::Events { events, lagged } = f {
            assert!(!lagged);
            got = events;
        }
    }
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].kind, "client.confirm_requested");
    // The attach itself was an event.
    let attached = srv.with_core(|c| {
        c.store
            .events_after(0, 100, &["client.attached".to_string()])
            .unwrap()
    });
    assert_eq!(attached.len(), 1);
    assert_eq!(attached[0].subject["client"], "tui-a");
    asyncio::write_frame(&mut cwr, &ClientFrame::Detach)
        .await
        .unwrap();
    tokio::io::AsyncWriteExt::flush(&mut cwr).await.unwrap();
    let t = Instant::now();
    loop {
        let n = srv.with_core(|c| {
            c.store
                .events_after(0, 100, &["client.detached".to_string()])
                .unwrap()
                .len()
        });
        if n == 1 {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(5), "detached event");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
