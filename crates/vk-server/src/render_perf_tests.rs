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
        pending_reads: Default::default(),
        read_commands: 0,
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
    let ServerFrame::History { lines, .. } = history.finish(&s.server).frame else {
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
    for req in 1..=12 {
        asyncio::write_frame(
            &mut wr,
            &ClientFrame::FetchHistory {
                req,
                pane: "P".into(),
                start: 0,
                count: 1,
            },
        )
        .await
        .unwrap();
    }
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
                ServerFrame::Goodbye { reason } => panic!("queued reads disconnected: {reason}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    release_tx.send(()).unwrap();
    blocker.await.unwrap();
    let mut replies = std::collections::HashSet::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while replies.len() != 12 {
            match asyncio::read_frame::<_, ServerFrame>(&mut rd)
                .await
                .unwrap()
            {
                ServerFrame::History { req, .. } => {
                    assert!(replies.insert(req));
                }
                ServerFrame::Goodbye { reason } => panic!("queued reads disconnected: {reason}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
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

#[tokio::test]
async fn history_keeps_capacity_when_command_workers_are_busy() {
    let (_d, server) = server();
    let mut s = session(server);
    runtime(&mut s);
    // Model six repository handlers waiting on I/O, without relying on filesystem timing.
    for _ in 0..6 {
        s.reads
            .spawn(std::future::pending::<Result<read::ReadOutput>>());
    }
    s.read_commands = 6;
    let mut out = Vec::new();
    s.on_client(
        ClientFrame::Command {
            req: 99,
            json: serde_json::json!({"jsonrpc":"2.0","id":99,"method":"fs.browse","params":{}})
                .to_string(),
        },
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(s.pending_reads.len(), 1);
    s.on_client(
        ClientFrame::FetchHistory {
            req: 100,
            pane: "P".into(),
            start: 0,
            count: 0,
        },
        &mut out,
    )
    .await
    .unwrap();
    s.on_client(ClientFrame::Ping { nonce: 101 }, &mut out)
        .await
        .unwrap();
    let mut bytes = out.as_slice();
    assert!(matches!(
        asyncio::read_frame::<_, ServerFrame>(&mut bytes)
            .await
            .unwrap(),
        ServerFrame::Pong { nonce: 101, .. }
    ));
    let result = tokio::time::timeout(Duration::from_secs(5), s.reads.join_next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        result.frame,
        ServerFrame::History { req: 100, .. }
    ));
    assert_eq!(
        s.pending_reads.len(),
        1,
        "queued commands do not consume history capacity"
    );
}

#[tokio::test]
async fn forgetting_another_pane_preserves_slow_history_reply() {
    for forget_before_finish in [false, true] {
        let (_d, server) = server();
        let mut s = session(server);
        let rt = runtime(&mut s);
        {
            let mut sc = rt.screen.lock().unwrap();
            sc.engine = vk_term::Engine::new(40, 2, 1);
            sc.engine
                .feed(b"one\r\ntwo\r\nthree\r\nfour\r\nfive", &mut vec![]);
            assert_eq!(sc.engine.scrolled_total(), 3);
            assert_eq!(sc.engine.history_len(), 1);
        }
        {
            let mut archive = s.server.archive.lock().unwrap();
            let rows: Vec<_> = ["one", "two"]
                .into_iter()
                .enumerate()
                .map(|(n, text)| vk_store::archive::ArchivedRow {
                    n: n as u64,
                    t: text.into(),
                    w: false,
                })
                .collect();
            archive.append("P", &rows).unwrap();
            archive.flush().unwrap();
            // B exists only in buffered archive data: forgetting it still needs a fence.
            archive
                .append(
                    "B",
                    &[vk_store::archive::ArchivedRow {
                        n: 0,
                        t: "forgotten".into(),
                        w: false,
                    }],
                )
                .unwrap();
        }
        let captured = s.capture_history(7, "P", 0, 20);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let read_server = s.server.clone();
        let worker = tokio::task::spawn_blocking(move || {
            // Hold the queued read after memory capture, as a slow archive worker would.
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            captured.finish(&read_server)
        });
        ready_rx.await.unwrap();
        if forget_before_finish {
            s.server.archive.lock().unwrap().remove_pane("B");
        }
        release_tx.send(()).unwrap();
        let result = worker.await.unwrap();
        if !forget_before_finish {
            // Also cover the delivery fence after archive decoding already completed.
            s.server.archive.lock().unwrap().remove_pane("B");
        }
        let mut out = Vec::new();
        s.write_read(result, &mut out).await.unwrap();
        let mut bytes = out.as_slice();
        let ServerFrame::History {
            req,
            start,
            total,
            lines,
            ..
        } = asyncio::read_frame::<_, ServerFrame>(&mut bytes)
            .await
            .unwrap()
        else {
            panic!("expected history");
        };
        assert_eq!((req, start, total), (7, 0, 3));
        assert_eq!(
            lines.iter().map(Row::text).collect::<Vec<_>>(),
            ["one", "two", "three"],
            "forget before finish: {forget_before_finish}"
        );
    }
}

#[tokio::test]
async fn forgotten_or_closed_history_is_not_emitted_after_capture() {
    for forget_before_finish in [false, true] {
        let (_d, server) = server();
        let mut s = session(server);
        let rt = runtime(&mut s);
        rt.screen.lock().unwrap().engine.feed(b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\nseven\r\neight\r\nnine\r\nten\r\neleven", &mut vec![]);
        let captured = s.capture_history(7, "P", 0, 20);
        if forget_before_finish {
            s.server.archive.lock().unwrap().remove_pane("P");
        }
        let result = captured.finish(&s.server);
        if !forget_before_finish {
            s.server.archive.lock().unwrap().remove_pane("P");
        }
        let mut out = Vec::new();
        s.write_read(result, &mut out).await.unwrap();
        let mut bytes = out.as_slice();
        assert!(
            matches!(asyncio::read_frame::<_, ServerFrame>(&mut bytes).await.unwrap(),
            ServerFrame::History { req: 7, total: 0, lines, .. } if lines.is_empty())
        );
        // Closing the pane also invalidates captured styled memory, independently of archive purge.
        let captured = s.capture_history(8, "P", 0, 20).finish(&s.server);
        s.server.panes.lock().unwrap().remove("P");
        out.clear();
        s.write_read(captured, &mut out).await.unwrap();
        let mut bytes = out.as_slice();
        assert!(
            matches!(asyncio::read_frame::<_, ServerFrame>(&mut bytes).await.unwrap(),
            ServerFrame::History { req: 8, total: 0, lines, .. } if lines.is_empty())
        );
    }
}
