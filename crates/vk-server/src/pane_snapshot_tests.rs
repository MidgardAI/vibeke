//! Snapshot completion must not stall holder I/O or consume newer output.

use super::*;
use crate::ServerOpts;
use crate::core::Tx;
use crate::paths::Paths;

struct Env {
    _dir: tempfile::TempDir,
    pane: PaneLoop,
    holder: UnixStream,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let server = Server::new(
            Paths {
                session: "snapshot-test".into(),
                runtime: root.join("run"),
                state: root.join("state"),
            },
            ServerOpts {
                session: "snapshot-test".into(),
                machine: "testbox".into(),
                bin: "/bin/false".into(),
                hold_args: vec![],
                default_shell: None,
                env: vec![],
                shims: false,
                gateway: None,
            },
        )
        .unwrap();
        let mut tx = Tx::new();
        tx.m.holder("p", "/tmp/test.sock", &[], 7, Some(10), Some(11));
        server.core.lock().unwrap().commit(tx).unwrap();
        let (rt, _rx) = PaneRt::new("p", 40, 10);
        let (stream, holder) = UnixStream::pair().unwrap();
        let (_, wr) = stream.into_split();
        Self {
            _dir: dir,
            holder,
            pane: PaneLoop {
                server,
                rt,
                wr: BufWriter::new(wr),
                epoch: 7,
                ledger: Ledger::default(),
                incarnation: "11:123".into(),
                exited: None,
                closing: false,
                last_snapshot: Instant::now(),
                snapshot_dirty: false,
                snapshot_armed: false,
                snapshot: None,
                snapshot_again: false,
                snapshot_waiters: Vec::new(),
                replaying: false,
                method: "fresh".into(),
                child_pid: 11,
                effects: Vec::new(),
                pipe: false,
                session: None,
                prebuf: Vec::new(),
                awaiting_attach: false,
            },
        }
    }

    /// Stop only the first worker, without holding `core`, until the test releases it.
    fn pause_commit(&self) -> (oneshot::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (entered_tx, entered_rx) = oneshot::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let entered_tx = Mutex::new(Some(entered_tx));
        let resume_rx = Mutex::new(resume_rx);
        *self.pane.server.before_snapshot_commit.lock().unwrap() = Some(Box::new(move || {
            if let Some(tx) = entered_tx.lock().unwrap().take() {
                let _ = tx.send(());
                let _ = resume_rx.lock().unwrap().recv();
            }
        }));
        (entered_rx, resume_tx)
    }

    fn pause_inside_commit(&self) -> (oneshot::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (entered, resume) = self.pause_commit();
        *self.pane.server.inside_snapshot_commit.lock().unwrap() = self
            .pane
            .server
            .before_snapshot_commit
            .lock()
            .unwrap()
            .take();
        (entered, resume)
    }

    async fn finish(&mut self) -> bool {
        let ok = (&mut self.pane.snapshot.as_mut().unwrap().worker)
            .await
            .unwrap();
        self.pane.finish_snapshot(ok).await.unwrap();
        ok
    }

    async fn checkpoint(&mut self, offset: u64) {
        let frame: ToHolder = asyncio::read_frame(&mut self.holder).await.unwrap();
        assert!(
            matches!(frame, ToHolder::Checkpoint { epoch: 7, offset: got } if got == offset),
            "{frame:?}"
        );
    }
}

#[tokio::test]
async fn input_and_output_continue_while_snapshot_commit_is_pending() {
    let mut e = Env::new();
    e.pane.feed(0, b"old", true).await.unwrap();
    let (entered, resume) = e.pause_commit();
    e.pane.maybe_snapshot(true).await.unwrap();
    entered.await.unwrap();
    e.pane
        .on_cmd(PaneCmd::Input {
            id: 99,
            bytes: b"x".to_vec(),
            ack: None,
        })
        .await
        .unwrap();
    let frame: ToHolder = asyncio::read_frame(&mut e.holder).await.unwrap();
    assert!(matches!(frame, ToHolder::Input { input_id: 99, .. }));
    e.pane.feed(3, b"new", true).await.unwrap();
    assert!(e.pane.snapshot_dirty);
    assert_eq!(e.pane.rt.screen.lock().unwrap().fed_offset, 6);
    resume.send(()).unwrap();
    assert!(e.finish().await);
    e.checkpoint(3).await;
    assert!(
        e.pane.snapshot_dirty,
        "completion must preserve newer output"
    );
    assert!(e.pane.snapshot_armed, "newer output needs another snapshot");
    let saved = e
        .pane
        .server
        .with_core(|c| c.store.snapshot_for("p"))
        .unwrap()
        .unwrap();
    assert_eq!(saved.offset, 3);
    assert_eq!(saved.incarnation.as_deref(), Some("11:123"));
}

#[tokio::test]
async fn shutdown_barrier_waits_for_a_snapshot_after_the_request() {
    let mut e = Env::new();
    e.pane.feed(0, b"old", true).await.unwrap();
    let (entered, resume) = e.pause_commit();
    e.pane.maybe_snapshot(true).await.unwrap();
    entered.await.unwrap();
    e.pane.feed(3, b"new", true).await.unwrap();
    let (done, mut waiting) = oneshot::channel();
    let permit = e.pane.server.snapshot_permit().unwrap();
    e.pane
        .on_cmd(PaneCmd::SnapshotAndWait(permit, done))
        .await
        .unwrap();
    assert_eq!(
        e.pane.snapshot.as_ref().unwrap().point.offset,
        3,
        "only one worker may run"
    );
    assert!(e.pane.snapshot_again);
    assert!(matches!(
        waiting.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    resume.send(()).unwrap();
    assert!(e.finish().await);
    e.checkpoint(3).await;
    assert_eq!(e.pane.snapshot.as_ref().unwrap().point.offset, 6);
    assert!(matches!(
        waiting.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert!(e.finish().await);
    e.checkpoint(6).await;
    assert!(waiting.await.unwrap());
    assert!(!e.pane.snapshot_dirty);
}

#[tokio::test]
async fn replaced_holder_rejects_a_stale_snapshot_and_checkpoint() {
    let mut e = Env::new();
    let (entered, resume) = e.pause_commit();
    e.pane.maybe_snapshot(true).await.unwrap();
    entered.await.unwrap();
    // Replace the holder and remove its old snapshot in the same transaction.
    let mut tx = Tx::new();
    tx.m.holder("p", "/tmp/new.sock", &[], 8, Some(20), Some(21));
    tx.m.snapshot_delete("p");
    e.pane.server.core.lock().unwrap().commit(tx).unwrap();
    resume.send(()).unwrap();
    assert!(!e.finish().await);
    assert!(
        e.pane
            .server
            .with_core(|c| c.store.snapshot_for("p"))
            .unwrap()
            .is_none()
    );
    let mut byte = [0];
    assert_eq!(
        e.holder.try_read(&mut byte).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(e.pane.snapshot_dirty, "failed snapshots must be retried");
}

#[tokio::test]
async fn completion_never_checkpoints_a_different_connection() {
    let mut e = Env::new();
    e.pane.maybe_snapshot(true).await.unwrap();
    let ok = (&mut e.pane.snapshot.as_mut().unwrap().worker)
        .await
        .unwrap();
    assert!(ok);
    e.pane.epoch += 1;
    e.pane.incarnation = "21:456".into();
    e.pane.finish_snapshot(ok).await.unwrap();
    let mut byte = [0];
    assert_eq!(
        e.holder.try_read(&mut byte).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn forced_snapshot_during_replay_waits_until_replay_is_complete() {
    let mut e = Env::new();
    e.pane.replaying = true;
    let (done, mut waiting) = oneshot::channel();
    let permit = e.pane.server.snapshot_permit().unwrap();
    e.pane
        .on_cmd(PaneCmd::SnapshotAndWait(permit, done))
        .await
        .unwrap();
    assert!(e.pane.snapshot.is_none());
    assert!(matches!(
        waiting.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    e.pane.replaying = false;
    e.pane.maybe_snapshot(e.pane.snapshot_again).await.unwrap();
    assert!(e.finish().await);
    e.checkpoint(0).await;
    assert!(waiting.await.unwrap());
}

#[tokio::test]
async fn shutdown_seal_blocks_late_snapshot_and_a_failed_restart_can_resume() {
    let mut e = Env::new();
    let shutdown = e.pane.server.begin_snapshot_shutdown().unwrap();
    let (entered, resume) = e.pause_commit();
    let (done, waiting) = oneshot::channel();
    e.pane
        .on_cmd(PaneCmd::SnapshotAndWait(shutdown, done))
        .await
        .unwrap();
    entered.await.unwrap();
    // This is the timeout path: seal without waiting for a stalled blocking worker.
    e.pane.server.seal_snapshot_shutdown(shutdown);
    assert!(e.pane.server.shutdown_housekeeping());
    assert!(e.pane.server.begin_snapshot_shutdown().is_none());
    // Failed exec reopens persistence before the late worker returns. Its generation must
    // still be rejected, and a later shutdown must be able to acquire a fresh permit.
    e.pane.server.resume_snapshot_persistence(shutdown);
    resume.send(()).unwrap();
    assert!(!e.finish().await);
    assert!(!waiting.await.unwrap());
    assert!(
        e.pane
            .server
            .with_core(|c| c.store.snapshot_for("p"))
            .unwrap()
            .is_none()
    );
    let mut byte = [0];
    assert_eq!(
        e.holder.try_read(&mut byte).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    e.pane.maybe_snapshot(true).await.unwrap();
    assert!(e.finish().await);
    e.checkpoint(0).await;
    assert!(e.pane.server.begin_snapshot_shutdown().is_some());
}

#[tokio::test]
async fn output_after_final_flush_cannot_create_another_snapshot_or_checkpoint() {
    let mut e = Env::new();
    e.pane.feed(0, b"old", true).await.unwrap();
    let shutdown = e.pane.server.begin_snapshot_shutdown().unwrap();
    let (done, waiting) = oneshot::channel();
    e.pane
        .on_cmd(PaneCmd::SnapshotAndWait(shutdown, done))
        .await
        .unwrap();
    assert!(e.finish().await);
    assert!(waiting.await.unwrap());
    e.checkpoint(3).await;
    e.pane.server.seal_snapshot_shutdown(shutdown);
    assert!(e.pane.server.shutdown_housekeeping());
    let mut offset = 3;
    let chunk = b"after shutdown\r\n".repeat(500);
    for _ in 0..22 {
        e.pane.feed(offset, &chunk, true).await.unwrap();
        offset += chunk.len() as u64;
    }
    e.pane.on_cmd(PaneCmd::SnapshotDue).await.unwrap();
    e.pane.maybe_snapshot(true).await.unwrap();
    assert!(e.pane.snapshot.is_none());
    assert!(!e.pane.snapshot_armed);
    assert_eq!(
        e.pane
            .server
            .with_core(|c| c.store.snapshot_for("p"))
            .unwrap()
            .unwrap()
            .offset,
        3
    );
    let mut byte = [0];
    assert_eq!(
        e.holder.try_read(&mut byte).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn failed_epoch_persistence_is_reported_and_a_new_lease_can_snapshot() {
    let mut e = Env::new();
    e.pane
        .server
        .with_core(|c| c.store.set_query_only(true).unwrap());
    assert!(e.pane.server.holder_epoch("p", 8).is_err());
    assert_eq!(
        e.pane.server.with_core(|c| c.store.holders()).unwrap()[0].epoch,
        7
    );
    e.pane
        .server
        .with_core(|c| c.store.set_query_only(false).unwrap());
    // run_inner propagates the error and acquires a newer lease on its next attempt.
    e.pane.server.holder_epoch("p", 9).unwrap();
    e.pane.epoch = 9;
    e.pane.maybe_snapshot(true).await.unwrap();
    assert!(e.finish().await);
    let frame: ToHolder = asyncio::read_frame(&mut e.holder).await.unwrap();
    assert!(matches!(frame, ToHolder::Checkpoint { epoch: 9, .. }));
}

#[tokio::test]
async fn a_gap_during_snapshot_persistence_cannot_checkpoint_the_reset_screen() {
    let mut e = Env::new();
    e.pane.feed(0, b"old", true).await.unwrap();
    let (entered, resume) = e.pause_commit();
    e.pane.maybe_snapshot(true).await.unwrap();
    entered.await.unwrap();
    e.pane
        .on_frame(FromHolder::Gap {
            requested: 0,
            available_from: 3,
        })
        .await
        .unwrap();
    resume.send(()).unwrap();
    assert!(e.finish().await);
    assert!(e.pane.replaying);
    assert!(e.pane.rt.screen.lock().unwrap().recovering);
    let mut byte = [0];
    assert_eq!(
        e.holder.try_read(&mut byte).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(e.pane.snapshot_dirty);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_pane_can_feed_while_a_snapshot_holds_the_commit_gate() {
    let mut e = Env::new();
    let (entered, resume) = e.pause_inside_commit();
    e.pane.maybe_snapshot(true).await.unwrap();
    entered.await.unwrap();
    let mut other = Env::new();
    other.pane.server = e.pane.server.clone();
    let feed = tokio::spawn(async move {
        other.pane.feed(0, b"responsive", true).await.unwrap();
        other
    });
    // Bound this from a separate runtime worker: a synchronous gate regression cannot
    // prevent the deadline from releasing the paused worker and reporting the failure.
    let mut feed = feed;
    let result = tokio::time::timeout(Duration::from_millis(500), &mut feed).await;
    resume.send(()).unwrap();
    assert!(e.finish().await);
    let other = result
        .expect("feed blocked on another pane's snapshot commit")
        .unwrap();
    assert_eq!(other.pane.rt.screen.lock().unwrap().fed_offset, 10);
    assert!(other.pane.snapshot_armed);
}

#[tokio::test]
async fn archive_outage_retains_evicted_rows_until_recovery_can_checkpoint() {
    let mut e = Env::new();
    let prefix = b"before outage\r\n".repeat(30);
    e.pane.feed(0, &prefix, true).await.unwrap();
    let root = e.pane.server.archive.lock().unwrap().root().to_path_buf();
    let saved = root.with_extension("saved");
    std::fs::rename(&root, &saved).unwrap();
    std::fs::write(&root, b"not a directory").unwrap();
    e.pane.server.housekeeping();
    assert!(e.pane.server.archive_failure.lock().unwrap().is_some());
    let mut offset = prefix.len() as u64;
    for batch in 0..24 {
        let text: String = (batch * 500..(batch + 1) * 500)
            .map(|n| format!("outage_{n}\r\n"))
            .collect();
        e.pane.feed(offset, text.as_bytes(), true).await.unwrap();
        offset += text.len() as u64;
    }
    let suffix = b"after outage\r\n".repeat(20);
    e.pane.feed(offset, &suffix, true).await.unwrap();
    offset += suffix.len() as u64;
    assert!(e.pane.rt.screen.lock().unwrap().engine.scrolled_total() > 10_000);
    e.pane.maybe_snapshot(true).await.unwrap();
    assert!(
        !e.finish().await,
        "no checkpoint while the archive is unavailable"
    );
    std::fs::remove_file(&root).unwrap();
    std::fs::rename(&saved, &root).unwrap();
    e.pane
        .server
        .archive_failure
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .retry_at = Instant::now();
    e.pane.server.housekeeping();
    assert!(e.pane.server.archive_failure.lock().unwrap().is_none());
    let rows = e
        .pane
        .server
        .archive
        .lock()
        .unwrap()
        .read("p", 0, u64::MAX)
        .unwrap();
    let numbered: Vec<u64> = rows
        .iter()
        .filter_map(|r| r.t.strip_prefix("outage_"))
        .map(|n| n.parse().unwrap())
        .collect();
    assert_eq!(numbered, (0..12_000).collect::<Vec<_>>());
    e.pane.maybe_snapshot(true).await.unwrap();
    assert!(e.finish().await);
    e.checkpoint(offset).await;
}
