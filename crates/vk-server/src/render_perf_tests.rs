use super::*;

fn view(now: Instant) -> PaneView {
    PaneView {
        epoch: 1,
        rev: 1,
        rows: Arc::from([]),
        cursor: Cursor::default(),
        modes: PaneModes::default(),
        title: String::new(),
        unacked: 0,
        last_sent: now,
        screen_rev: 1,
        deferred: None,
        scrolled: 0,
        dirty_since_ack: false,
        images: vec![],
    }
}

#[test]
fn spinner_deadline_tracks_observed_revision_and_yields_to_input() {
    let now = Instant::now();
    let normal = Duration::from_millis(33);
    let mut v = view(now);
    for delay in [250, 1000] {
        let at = now + Duration::from_millis(delay);
        v.deferred = Some((2, at));
        assert_eq!(v.eligible_at(2, normal, false, false, now), at);
        // New output must be examined, even if its Notify arrived while writing a frame.
        assert_eq!(v.eligible_at(3, normal, false, false, now), now + normal);
        assert_eq!(v.eligible_at(2, normal, true, false, now), now + normal);
        assert_eq!(v.eligible_at(2, normal, false, true, now), now);
    }
}

use crate::ServerOpts;
use crate::paths::Paths;
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

fn session(server: Arc<Server>) -> Session {
    Session {
        media: crate::browser_pane::MediaSession::new(&server, false),
        server,
        client_id: "perf-test".into(),
        views: HashMap::new(),
        visible: vec![],
        focused: None,
        model_rev: 0,
        max_fps: 60,
        bg_fps: 4,
        remote: false,
        acks: mpsc::unbounded_channel().0,
        pending_sub: None,
        images_sent: Default::default(),
        auth: Auth::default(),
        reads: tokio::task::JoinSet::new(),
    }
}

fn runtime(s: &mut Session) -> Arc<crate::pane::PaneRt> {
    let (rt, _rx) = crate::pane::PaneRt::new("P", 40, 10);
    s.server
        .panes
        .lock()
        .unwrap()
        .insert("P".into(), rt.clone());
    s.visible.push(PaneRect {
        pane: "P".into(),
        cols: 40,
        rows: 10,
    });
    rt
}

#[tokio::test]
async fn blocked_ack_and_recovery_have_no_retry_timer() {
    let (_d, server) = server();
    let mut s = session(server);
    let rt = runtime(&mut s);
    let now = Instant::now();
    let mut v = view(now - Duration::from_secs(2));
    v.unacked = MAX_UNACKED;
    s.views.insert("P".into(), v);
    rt.rev_tx.send_replace(2);
    assert!(s.next_deadline().into_std() > now + Duration::from_secs(3500));
    // A resize while blocked must still become a keyframe after the ACK.
    rt.screen.lock().unwrap().epoch = 2;
    s.on_client(
        ClientFrame::Ack {
            pane: "P".into(),
            epoch: 1,
            rev: 1,
        },
        &mut tokio::io::sink(),
    )
    .await
    .unwrap();
    assert!(s.next_deadline().into_std() < now + Duration::from_secs(1));
    s.send_panes(&mut tokio::io::sink()).await.unwrap();
    assert_eq!(s.views["P"].epoch, 2);
    rt.rev_tx.send_replace(3);
    s.views.get_mut("P").unwrap().unacked = 0;
    rt.screen.lock().unwrap().recovering = true;
    assert!(s.next_deadline().into_std() > now + Duration::from_secs(3500));
}

#[tokio::test]
async fn repeated_spinner_wakes_reuse_projection_until_eligible() {
    let (_d, server) = server();
    let mut s = session(server);
    let rt = runtime(&mut s);
    s.send_panes(&mut tokio::io::sink()).await.unwrap();
    {
        let v = s.views.get_mut("P").unwrap();
        v.unacked = 0;
        v.last_sent = Instant::now() - Duration::from_millis(40);
    }
    rt.screen
        .lock()
        .unwrap()
        .engine
        .feed(b"spinner", &mut vec![]);
    rt.rev_tx.send_replace(1);
    s.send_panes(&mut tokio::io::sink()).await.unwrap();
    assert!(s.views["P"].deferred.is_some());
    let builds = rt.screen.lock().unwrap().engine.row_projection_builds();
    for _ in 0..20 {
        s.send_panes(&mut tokio::io::sink()).await.unwrap();
    }
    assert_eq!(
        rt.screen.lock().unwrap().engine.row_projection_builds(),
        builds
    );
    // Mirrored input updates this pane's last_input too: it bypasses the held deadline.
    *rt.last_input.lock().unwrap() = Some(Instant::now());
    s.send_panes(&mut tokio::io::sink()).await.unwrap();
    assert_eq!(s.views["P"].screen_rev, 1);
    assert!(s.views["P"].deferred.is_none());
}

#[tokio::test]
async fn history_captures_memory_before_ring_rollover() {
    let (_d, server) = server();
    let mut s = session(server);
    let rt = runtime(&mut s);
    {
        let mut sc = rt.screen.lock().unwrap();
        sc.engine = vk_term::Engine::new(40, 2, 4);
        sc.engine
            .feed(b"one\r\ntwo\r\nthree\r\nfour\r\nfive", &mut vec![]);
    }
    let history = s.capture_history(7, "P", 0, 10);
    let expected: Vec<_> = {
        let sc = rt.screen.lock().unwrap();
        (0..sc.engine.history_len())
            .map(|i| sc.engine.history_row(i).unwrap())
            .collect()
    };
    rt.screen
        .lock()
        .unwrap()
        .engine
        .feed(b"\r\nsix\r\nseven\r\neight\r\nnine", &mut vec![]);
    let ServerFrame::History { lines, .. } = history.finish(&s.server) else {
        panic!()
    };
    assert_eq!(lines, expected);
}

#[tokio::test]
async fn slow_archive_read_does_not_block_ping_on_the_same_connection() {
    let (_d, server) = server();
    let (rt, _rx) = crate::pane::PaneRt::new("P", 40, 2);
    {
        let mut sc = rt.screen.lock().unwrap();
        sc.engine = vk_term::Engine::new(40, 2, 1);
        sc.engine
            .feed(b"one\r\ntwo\r\nthree\r\nfour\r\nfive", &mut vec![]);
    }
    server.panes.lock().unwrap().insert("P".into(), rt);
    let (client, peer) = tokio::io::duplex(1 << 20);
    let (rd, wr) = tokio::io::split(peer);
    let serving = tokio::spawn(serve(server.clone(), rd, wr, "read-test".into(), false, 60));
    let (mut rd, mut wr) = tokio::io::split(client);
    // Keep the archive worker deterministically blocked while the session handles Ping.
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let held_server = server.clone();
    let blocker = tokio::task::spawn_blocking(move || {
        let _guard = held_server.archive.lock().unwrap();
        let _ = ready_tx.send(());
        let _ = release_rx.recv();
    });
    ready_rx.await.unwrap();
    asyncio::write_frame(
        &mut wr,
        &ClientFrame::FetchHistory {
            req: 1,
            pane: "P".into(),
            start: 0,
            count: 1,
        },
    )
    .await
    .unwrap();
    asyncio::write_frame(&mut wr, &ClientFrame::Ping { nonce: 42 })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match asyncio::read_frame::<_, ServerFrame>(&mut rd)
                .await
                .unwrap()
            {
                ServerFrame::Pong { nonce: 42, .. } => break,
                ServerFrame::History { .. } => panic!("archive should still be blocked"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    release_tx.send(()).unwrap();
    blocker.await.unwrap();
    asyncio::write_frame(&mut wr, &ClientFrame::Detach)
        .await
        .unwrap();
    serving.await.unwrap().unwrap();
}

#[tokio::test]
async fn read_targets_are_frozen_before_focus_changes() {
    use serde_json::json;
    let (_d, server) = server();
    let s = session(server);
    let pane = vk_proto::model::Pane {
        id: "P".into(),
        handle: "P".into(),
        tab: "T".into(),
        workspace: "W".into(),
        title: None,
        auto_title: String::new(),
        cwd: Some("/tmp".into()),
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
    s.server.with_core(|c| {
        let mut tx = crate::core::Tx::new();
        tx.pane(pane);
        c.commit(tx).unwrap();
    });
    s.server
        .clients
        .lock()
        .unwrap()
        .entry(s.client_id.clone())
        .or_default()
        .focus
        .pane = Some("P".into());
    let request =
        json!({"jsonrpc":"2.0", "id":73, "method":"git.status", "params":{"pane":"@focused"}});
    let prepared = s.prepare_read(&request.to_string()).unwrap();
    s.server
        .clients
        .lock()
        .unwrap()
        .get_mut(&s.client_id)
        .unwrap()
        .focus
        .pane = Some("other".into());
    let v: serde_json::Value = serde_json::from_str(&prepared).unwrap();
    assert_eq!(v["params"]["pane"], "P");
    assert_eq!(v["id"], 73);
    for method in [
        "pane.focus",
        "workspace.create",
        "pane.write",
        "theme.set",
        "not.a.method",
    ] {
        let request = json!({"jsonrpc":"2.0", "id":1, "method":method, "params":{}});
        assert!(s.prepare_read(&request.to_string()).is_none());
    }
}

#[tokio::test]
async fn two_clients_share_projection_but_keep_independent_frame_revisions() {
    let (_d, server) = server();
    let mut first = session(server.clone());
    let mut second = session(server);
    let rt = runtime(&mut first);
    second.visible = first.visible.clone();
    first.send_panes(&mut tokio::io::sink()).await.unwrap();
    second.send_panes(&mut tokio::io::sink()).await.unwrap();
    assert_eq!(rt.screen.lock().unwrap().engine.row_projection_builds(), 1);
    // Only the first client ACKs. Both still retain their independent old rows.
    first.views.get_mut("P").unwrap().unacked = 0;
    rt.screen
        .lock()
        .unwrap()
        .engine
        .feed(b"updated", &mut vec![]);
    rt.rev_tx.send_replace(1);
    *rt.last_input.lock().unwrap() = Some(Instant::now());
    first.send_panes(&mut tokio::io::sink()).await.unwrap();
    assert_ne!(first.views["P"].rows, second.views["P"].rows);
    second.send_panes(&mut tokio::io::sink()).await.unwrap();
    assert_eq!(first.views["P"].rows, second.views["P"].rows);
    assert_eq!(rt.screen.lock().unwrap().engine.row_projection_builds(), 2);
}
