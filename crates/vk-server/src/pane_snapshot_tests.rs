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
    e.pane.on_cmd(PaneCmd::SnapshotAndWait(done)).await.unwrap();
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
    e.pane.on_cmd(PaneCmd::SnapshotAndWait(done)).await.unwrap();
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
