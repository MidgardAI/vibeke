//! Per-pane runtime: holder connection, VT engine, recovery (01 §1.2, 03 §4), effects,
//! snapshots and scrollback archiving.

use crate::Server;
use crate::agents::headless::{Act, Session};
use anyhow::{Context, Result, bail};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{mpsc, oneshot, watch};
use vk_proto::frame::asyncio;
use vk_proto::holder::*;
use vk_store::archive::ArchivedRow;
use vk_term::{Effect, Engine};

/// Default and bounds for `terminal.scrollback_lines` (lines kept in each pane's engine).
pub const SCROLLBACK: usize = 10_000;
const SCROLLBACK_MIN: usize = 100;
const SCROLLBACK_MAX: usize = 1_000_000;

/// `terminal.scrollback_lines` from the current config, clamped to sane bounds. Applies to
/// engines created or restored from now on.
fn scrollback_lines() -> usize {
    clamp_scrollback(crate::config_api::current().terminal.scrollback_lines)
}

fn clamp_scrollback(lines: u32) -> usize {
    (lines as usize).clamp(SCROLLBACK_MIN, SCROLLBACK_MAX)
}

/// First argv word that asks [`crate::Server`]'s pane spawn for a pipe-mode holder (01 §1.2):
/// headless harnesses get stdio pipes instead of a PTY. Stripped before the spawn, so it never
/// reaches the child, a sandbox wrapper or the stored pane command.
pub const PIPE_ARGV0: &str = "vibeke:pipe";

/// Split a pane command into its holder mode and the argv to run.
pub fn holder_mode(argv: &[String]) -> (Mode, &[String]) {
    match argv.split_first() {
        Some((first, rest)) if first == PIPE_ARGV0 => (Mode::Pipe, rest),
        _ => (Mode::Pty, argv),
    }
}

/// A fresh VT engine whose colour-query answers follow the session appearance (theme auto),
/// with the `[graphics]` limits and `terminal.allow_passthrough` of the current config.
fn new_engine(cols: u16, rows: u16) -> Engine {
    let cfg = graphics_config();
    let mut e = Engine::new(cols, rows, scrollback_lines());
    e.set_allow_passthrough(cfg);
    crate::theme::apply_query_palette(&mut e);
    e
}

/// Apply the config's `[graphics]` limits (for engines created or restored from now on, 03 §9)
/// and return `terminal.allow_passthrough` (03 §8).
fn graphics_config() -> bool {
    let cfg = crate::config_api::current();
    vk_term::engine::set_graphics_limits(
        cfg.graphics.max_image_bytes.0,
        cfg.graphics.max_total_per_pane.0,
    );
    cfg.terminal.allow_passthrough
}
const SNAPSHOT_IDLE: Duration = Duration::from_secs(2);
const SNAPSHOT_MAX_INTERVAL: Duration = Duration::from_secs(30);
/// How long a caller awaiting a holder ack waits. Acks now follow the PTY write, so a
/// child that isn't reading can legitimately delay them.
const INPUT_ACK_TIMEOUT: Duration = Duration::from_secs(30);
/// Reconnect backoff cap while a holder is alive but its connection keeps failing.
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(5);

pub struct Screen {
    pub engine: Engine,
    /// Bumped on every damage batch (load-bearing for `pane.wait_output {since_revision}`).
    pub rev: u64,
    /// Bumped whenever incremental client state must be discarded (recovery, resize).
    pub epoch: u32,
    pub fed_offset: u64,
    pub last_output: Instant,
    pub archived_upto: u64,
    pub recovering: bool,
}

pub enum PaneCmd {
    Input {
        id: u64,
        bytes: Vec<u8>,
        ack: Option<oneshot::Sender<InputStatus>>,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    Signal {
        sig: Sig,
        target: SigTarget,
    },
    Status(oneshot::Sender<Option<ProcStatus>>),
    /// Close: SIGHUP the child, then acknowledge its exit.
    Close,
    Snapshot,
    /// The pane's snapshot deadline (armed in the server-wide [`crate::timers::Scheduler`])
    /// came due: snapshot if the output is idle enough, otherwise re-arm.
    SnapshotDue,
    /// Pipe-mode panes: a command for the headless adapter (01 §3.3).
    Headless(crate::agents::headless::Cmd),
}

pub struct PaneRt {
    pub id: String,
    pub screen: Mutex<Screen>,
    pub rev_tx: watch::Sender<u64>,
    pub cmd_tx: mpsc::UnboundedSender<PaneCmd>,
    pub status: Mutex<Option<ProcStatus>>,
    /// Size most recently requested from the holder.
    pub want_size: Mutex<(u16, u16)>,
    pub last_input: Mutex<Option<Instant>>,
    /// The holder runs in pipe mode (headless harness; learned from `HelloOk`).
    pipe: std::sync::atomic::AtomicBool,
}

impl PaneRt {
    pub fn new(id: &str, cols: u16, rows: u16) -> (Arc<PaneRt>, mpsc::UnboundedReceiver<PaneCmd>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (rev_tx, _) = watch::channel(0);
        let rt = Arc::new(PaneRt {
            id: id.to_string(),
            screen: Mutex::new(Screen {
                engine: new_engine(cols, rows),
                rev: 1,
                epoch: 1,
                fed_offset: 0,
                last_output: Instant::now(),
                archived_upto: 0,
                recovering: false,
            }),
            rev_tx,
            cmd_tx,
            status: Mutex::new(None),
            want_size: Mutex::new((cols, rows)),
            last_input: Mutex::new(None),
            pipe: std::sync::atomic::AtomicBool::new(false),
        });
        (rt, cmd_rx)
    }

    /// A pipe-mode pane: its screen is Vibeke's transcript of a headless harness, and typed
    /// input goes to the adapter's line editor, never raw to the harness's stdin.
    pub fn is_pipe(&self) -> bool {
        self.pipe.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn send(&self, cmd: PaneCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let cols = cols.max(2);
        let rows = rows.max(1);
        let mut w = self.want_size.lock().unwrap();
        if *w != (cols, rows) {
            *w = (cols, rows);
            self.send(PaneCmd::Resize { cols, rows });
        }
    }

    pub async fn input(&self, id: u64, bytes: Vec<u8>) -> InputStatus {
        let (tx, rx) = oneshot::channel();
        *self.last_input.lock().unwrap() = Some(Instant::now());
        self.send(PaneCmd::Input {
            id,
            bytes,
            ack: Some(tx),
        });
        tokio::time::timeout(INPUT_ACK_TIMEOUT, rx)
            .await
            .ok()
            .and_then(|r| r.ok())
            // No ack in time (or the pane task is gone): the input may or may not reach the
            // program, reported as `input_unconfirmed` (01 §1.2); not "exited".
            .unwrap_or(InputStatus::Unconfirmed)
    }

    pub fn rev(&self) -> u64 {
        *self.rev_tx.borrow()
    }
}

/// HMAC over the holder nonce (07 §4).
fn acquire_hmac(key: &[u8], nonce: &[u8], epoch: u64) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(key).expect("hmac key");
    mac.update(&acquire_message(nonce, epoch));
    mac.finalize().into_bytes().to_vec()
}

pub struct HolderConn {
    pub socket: String,
    pub key: Vec<u8>,
    pub epoch: u64,
    /// True for a pane spawned by this server (no snapshot to restore).
    pub fresh: bool,
}

/// Identity of one holder process: a VT snapshot's ring offset only means something for the
/// holder whose ring it was taken from (01 §1.2). Derived from `HelloOk` (child pid + holder
/// start time), so holder/1 needs no new field.
pub fn holder_incarnation(child_pid: u32, started_at_ms: i64) -> String {
    format!("{child_pid}:{started_at_ms}")
}

/// Inputs sent to the holder and not yet acknowledged. It outlives a holder connection: after
/// re-acquiring, every entry is resent with its original id. The holder dedupes ids it
/// already wrote (and acks a still-pending one when its write completes), so a dropped
/// connection neither loses nor duplicates input.
#[derive(Default)]
struct Ledger {
    order: VecDeque<u64>,
    entries: HashMap<u64, (Vec<u8>, Vec<oneshot::Sender<InputStatus>>)>,
}

impl Ledger {
    /// Record an input; returns false if the id is already pending (not sent again).
    fn add(&mut self, id: u64, bytes: &[u8], ack: Option<oneshot::Sender<InputStatus>>) -> bool {
        if let Some((_, waiters)) = self.entries.get_mut(&id) {
            waiters.extend(ack);
            return false;
        }
        self.order.push_back(id);
        self.entries
            .insert(id, (bytes.to_vec(), ack.into_iter().collect()));
        true
    }

    fn complete(&mut self, id: u64, status: InputStatus) {
        if let Some((_, waiters)) = self.entries.remove(&id) {
            self.order.retain(|i| *i != id);
            for w in waiters {
                let _ = w.send(status);
            }
        }
    }

    /// The holder is gone: every pending input ends `status`; returns their ids.
    fn fail_all(&mut self, status: InputStatus) -> Vec<u64> {
        let ids: Vec<u64> = self.order.drain(..).collect();
        for id in &ids {
            if let Some((_, waiters)) = self.entries.remove(id) {
                for w in waiters {
                    let _ = w.send(status);
                }
            }
        }
        ids
    }

    fn pending(&self) -> Vec<(u64, Vec<u8>)> {
        self.order
            .iter()
            .filter_map(|id| self.entries.get(id).map(|(b, _)| (*id, b.clone())))
            .collect()
    }
}

/// What one connection attempt learned, kept across reconnects.
#[derive(Default)]
struct Attempt {
    /// Epoch this attempt acquired the lease with (None: failed before acquiring).
    acquired: Option<u64>,
    /// Incarnation of the holder seen on the previous successful connection.
    incarnation: Option<String>,
}

/// Run a pane until it closes. Recovers from a stored snapshot when not fresh.
///
/// A failed holder connection is not a lost holder (review finding 6): while the holder
/// process is alive the pane reconnects with a higher epoch (backoff capped at 5 s) and is
/// never respawned or removed. Only a holder that is gone goes to `holder_lost`.
pub async fn run(
    server: Arc<Server>,
    rt: Arc<PaneRt>,
    mut cmd_rx: mpsc::UnboundedReceiver<PaneCmd>,
    mut conn: HolderConn,
) {
    let id = rt.id.clone();
    let mut ledger = Ledger::default();
    let mut attempt = Attempt::default();
    // Pipe mode: the headless adapter, kept across reconnects like the ledger.
    let mut session: Option<Session> = None;
    let mut failures: u32 = 0;
    // Set once this task held the lease; only then can a newer epoch mean "superseded".
    let mut had_lease = false;
    loop {
        attempt.acquired = None;
        let res = run_inner(
            &server,
            &rt,
            &mut cmd_rx,
            &conn,
            &mut ledger,
            &mut attempt,
            &mut session,
        )
        .await;
        let e = match res {
            Ok(reason) => {
                tracing::info!(pane = %id, reason, "pane ended");
                server.pane_ended(&id, &reason);
                return;
            }
            Err(e) => e,
        };
        if let Some(ep) = attempt.acquired {
            conn.epoch = ep;
            failures = 0;
            had_lease = true;
        }
        let rec = server
            .with_core(|c| c.store.holders())
            .ok()
            .and_then(|hs| hs.into_iter().find(|h| h.pane == id));
        if had_lease
            && let Some(r) = &rec
            && r.epoch > conn.epoch
        {
            // Fenced by a newer server that took the lease: it owns the pane now.
            tracing::info!(pane = %id, ours = conn.epoch, theirs = r.epoch, "holder lease superseded");
            return;
        }
        let holder_pid = rec.as_ref().and_then(|r| r.holder_pid);
        if vk_hold::holder_alive(std::path::Path::new(&conn.socket), holder_pid) {
            failures += 1;
            let backoff =
                Duration::from_millis(50u64 << failures.min(7)).min(RECONNECT_BACKOFF_MAX);
            tracing::warn!(pane = %id, error = %format!("{e:#}"), attempt = failures, ?backoff,
                "holder connection lost but the holder is alive; reconnecting");
            tokio::time::sleep(backoff).await;
            conn.fresh = false;
            continue;
        }
        // The holder died without reporting a child exit (crash, kill, reboot): keep the
        // layout slot with a fresh shell and offer the agent for resume (10 §5.1).
        tracing::warn!(pane = %id, error = %format!("{e:#}"), "holder lost");
        // Inputs in flight may or may not have reached the program (01 §1.2).
        let lost = ledger.fail_all(InputStatus::Unconfirmed);
        report_unconfirmed(&server, &id, &lost, "holder_lost");
        server.holder_lost(&id);
        return;
    }
}

/// `pane.input_unconfirmed` (01 §1.2): inputs that may or may not have reached the program.
/// They are never replayed automatically.
/// Whether `pid` is still the process that had start time `started` (from
/// `vk_hold::procinfo`). Unknown identity (`None`) or a gone/reused pid: false.
pub(crate) fn same_process(pid: u32, started: Option<u64>) -> bool {
    pid > 1 && started.is_some_and(|s| vk_hold::procinfo::info(pid).is_some_and(|i| i.start == s))
}

pub(crate) fn report_unconfirmed(server: &Server, pane: &str, ids: &[u64], reason: &str) {
    if ids.is_empty() {
        return;
    }
    let mut c = server.core.lock().unwrap();
    let mut tx = crate::core::Tx::new();
    for id in ids {
        tx.event(
            "pane.input_unconfirmed",
            serde_json::json!({"pane": pane}),
            serde_json::json!({"input_id": id.to_string(), "reason": reason}),
        );
    }
    let _ = server.commit(&mut c, tx);
}

async fn run_inner(
    server: &Arc<Server>,
    rt: &Arc<PaneRt>,
    cmd_rx: &mut mpsc::UnboundedReceiver<PaneCmd>,
    conn: &HolderConn,
    ledger: &mut Ledger,
    attempt: &mut Attempt,
    session: &mut Option<Session>,
) -> Result<String> {
    let stream = connect_retry(&conn.socket)
        .await
        .context("connect holder")?;
    let (rd, wr) = stream.into_split();
    let mut wr = BufWriter::new(wr);
    let mut rd = tokio::io::BufReader::new(rd);
    asyncio::write_frame(
        &mut wr,
        &ToHolder::Hello {
            proto_min: PROTO_MIN,
            proto_max: PROTO,
            server_pid: std::process::id(),
            server_boot_id: server.boot_id.clone(),
        },
    )
    .await?;
    wr.flush().await?;
    let hello: FromHolder = asyncio::read_frame(&mut rd).await?;
    let FromHolder::HelloOk {
        nonce,
        epoch: holder_epoch,
        ring,
        child_pid,
        started_at_ms,
        mode,
        ..
    } = hello
    else {
        bail!("unexpected hello reply: {hello:?}")
    };
    let pipe = mode == Mode::Pipe;
    rt.pipe.store(pipe, std::sync::atomic::Ordering::Relaxed);
    let epoch = conn.epoch.max(holder_epoch) + 1;
    asyncio::write_frame(
        &mut wr,
        &ToHolder::Acquire {
            epoch,
            server_pid: std::process::id(),
            hmac: acquire_hmac(&conn.key, &nonce, epoch),
        },
    )
    .await?;
    wr.flush().await?;
    match asyncio::read_frame::<_, FromHolder>(&mut rd).await? {
        FromHolder::Acquired { .. } => {}
        other => bail!("acquire rejected: {other:?}"),
    }
    attempt.acquired = Some(epoch);
    server.holder_epoch(&rt.id, epoch);
    let incarnation = holder_incarnation(child_pid, started_at_ms);
    let same_holder = attempt.incarnation.as_deref() == Some(incarnation.as_str());
    attempt.incarnation = Some(incarnation.clone());
    let in_ring = |o: u64| o >= ring.start_offset && o <= ring.end_offset;

    // Recovery: snapshot + journal replay (03 §4).
    let mut from = 0;
    let mut method = "fresh";
    let fed = rt.screen.lock().unwrap().fed_offset;
    if pipe {
        // Pipe mode (01 §1.2): the screen is a transcript the adapter derives from the
        // journal, so recovery is a journal replay through the adapter, never a VT snapshot.
        let resume_at = session
            .as_ref()
            .map(|s| s.seen())
            .filter(|a| same_holder && in_ring(*a));
        if conn.fresh {
            // Bound to its record by `Cmd::Attach` from the launcher.
        } else if let Some(at) = resume_at {
            method = "reconnect";
            from = at;
            if let Some(s) = session.as_mut() {
                s.begin_reconnect();
            }
        } else {
            method = "journal";
            {
                let mut sc = rt.screen.lock().unwrap();
                let (c, r) = (sc.engine.cols(), sc.engine.rows());
                sc.engine = new_engine(c, r);
                sc.fed_offset = 0;
            }
            if session.is_none() {
                *session = Session::load(server, &rt.id);
            }
            if let Some(s) = session.as_mut() {
                s.begin_replay();
            }
            from = ring.start_offset;
        }
        if !conn.fresh {
            rt.screen.lock().unwrap().recovering = true;
        }
    } else if !conn.fresh && same_holder && in_ring(fed) {
        // Reconnect to the holder we were attached to: our screen is exact up to `fed`, so
        // only the bytes we missed are replayed.
        method = "reconnect";
        from = fed;
        let mut sc = rt.screen.lock().unwrap();
        sc.recovering = true;
        sc.engine.set_replaying(true);
    } else if !conn.fresh {
        method = "ring_only";
        {
            // Start from a blank screen (a reconnect to a different holder must not mix
            // screens).
            let mut sc = rt.screen.lock().unwrap();
            let (c, r) = (sc.engine.cols(), sc.engine.rows());
            sc.engine = new_engine(c, r);
            sc.fed_offset = 0;
        }
        let snap = server
            .with_core(|c| c.store.snapshot_for(&rt.id))
            .ok()
            .flatten();
        // Output between the last snapshot of this holder and the ring's start was never seen
        // by any server: it is gone. A ring that merely wrapped while a server was attached
        // lost nothing (that output was archived).
        // No snapshot at all means no server ever processed this holder's output past its
        // first idle moment (one is taken at least every 30 s while attached).
        let gap = match &snap {
            Some(s) if s.incarnation.as_deref() == Some(incarnation.as_str()) => {
                s.offset < ring.start_offset
            }
            Some(_) => false,
            None => ring.start_offset > 0,
        };
        // A snapshot is only valid for the holder incarnation it was taken from, at an
        // offset that this holder's ring can continue from.
        if let Some(s) = snap
            && s.version == vk_term::engine::ENGINE_VERSION
            && s.incarnation.as_deref() == Some(incarnation.as_str())
            && in_ring(s.offset)
            && let Ok(e) = {
                let _ = graphics_config();
                Engine::restore(&s.blob, scrollback_lines())
            }
        {
            let mut sc = rt.screen.lock().unwrap();
            sc.engine = e;
            sc.engine.set_allow_passthrough(graphics_config());
            crate::theme::apply_query_palette(&mut sc.engine);
            sc.fed_offset = s.offset;
            from = s.offset;
            method = "snapshot+replay";
        }
        let mut sc = rt.screen.lock().unwrap();
        sc.recovering = true;
        sc.engine.set_replaying(true);
        sc.archived_upto = server.archive_last_line(&rt.id).map(|l| l + 1).unwrap_or(0);
        if method == "ring_only" {
            from = ring.start_offset;
            if gap {
                method = "lost";
            }
        }
    }
    asyncio::write_frame(
        &mut wr,
        &ToHolder::Attach {
            epoch,
            from_offset: from,
        },
    )
    .await?;
    // Resend every unacknowledged input with its original id (the holder dedupes).
    for (input_id, bytes) in ledger.pending() {
        asyncio::write_frame(
            &mut wr,
            &ToHolder::Input {
                epoch,
                input_id,
                bytes,
            },
        )
        .await?;
    }
    wr.flush().await?;

    let (frame_tx, mut frame_rx) = mpsc::unbounded_channel::<FromHolder>();
    let reader = tokio::spawn(async move {
        while let Ok(f) = asyncio::read_frame::<_, FromHolder>(&mut rd).await {
            if frame_tx.send(f).is_err() {
                break;
            }
        }
    });

    let mut p = PaneLoop {
        server: server.clone(),
        rt: rt.clone(),
        wr,
        epoch,
        ledger: std::mem::take(ledger),
        incarnation,
        exited: None,
        closing: false,
        last_snapshot: Instant::now(),
        snapshot_dirty: false,
        snapshot_armed: false,
        replaying: !conn.fresh,
        method: method.to_string(),
        child_pid,
        effects: Vec::new(),
        pipe,
        session: session.take(),
        prebuf: Vec::new(),
        awaiting_attach: pipe && conn.fresh,
    };
    if conn.fresh {
        server.mark_recovered(&rt.id, None);
    }
    // No per-pane timer: snapshot deadlines are armed in the server-wide scheduler only while
    // output is pending (spec 10 §1.3), so an idle pane costs no wakeups.
    let result: Result<String> = async {
        loop {
            tokio::select! {
                f = frame_rx.recv() => {
                    let Some(f) = f else { return Err(anyhow::anyhow!("holder connection closed")) };
                    if let Some(reason) = p.on_frame(f).await? { return Ok(reason) }
                }
                c = cmd_rx.recv() => {
                    let Some(c) = c else { return Ok("dropped".to_string()) };
                    p.on_cmd(c).await?;
                }
            }
        }
    }
    .await;
    reader.abort();
    // Unacknowledged inputs survive into the next connection attempt.
    *ledger = std::mem::take(&mut p.ledger);
    *session = p.session.take();
    result
}

async fn connect_retry(path: &str) -> Result<UnixStream> {
    let mut last = None;
    for _ in 0..20 {
        match UnixStream::connect(path).await {
            Ok(s) => return Ok(s),
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    Err(last
        .map(Into::into)
        .unwrap_or_else(|| anyhow::anyhow!("connect failed")))
}

struct PaneLoop {
    server: Arc<Server>,
    rt: Arc<PaneRt>,
    wr: BufWriter<OwnedWriteHalf>,
    epoch: u64,
    ledger: Ledger,
    /// Holder incarnation this connection is attached to (stored with snapshots).
    incarnation: String,
    exited: Option<(Option<i32>, Option<i32>)>,
    closing: bool,
    last_snapshot: Instant,
    snapshot_dirty: bool,
    /// A snapshot deadline is armed in the server scheduler for this pane.
    snapshot_armed: bool,
    replaying: bool,
    method: String,
    child_pid: u32,
    effects: Vec<Effect>,
    /// Pipe mode: the holder journals stdio and the screen is the adapter's transcript.
    pipe: bool,
    session: Option<Session>,
    /// Pipe output that arrived before `Cmd::Attach` bound a fresh pane to its record.
    prebuf: Vec<(Stream, u64, Vec<u8>)>,
    awaiting_attach: bool,
}

/// Cap on pipe output buffered before `Cmd::Attach` (a harness speaks only when spoken to).
const PREBUF_MAX: usize = 4 << 20;

/// Rows that entered the engine's history since the last call, for the archive (03 §11.2).
fn take_archive(sc: &mut Screen) -> Vec<ArchivedRow> {
    let mut archive = Vec::new();
    let total = sc.engine.scrolled_total();
    let hist = sc.engine.history_len() as u64;
    let first_in_mem = total.saturating_sub(hist);
    let from = sc.archived_upto.max(first_in_mem);
    for abs in from..total {
        if let Some(row) = sc.engine.history_row((abs - first_in_mem) as usize) {
            archive.push(ArchivedRow {
                n: abs,
                t: row.text().trim_end().to_string(),
                w: row.wrapped,
            });
        }
    }
    sc.archived_upto = sc.archived_upto.max(total);
    archive
}

impl PaneLoop {
    async fn send(&mut self, m: &ToHolder) -> Result<()> {
        asyncio::write_frame(&mut self.wr, m).await?;
        self.wr.flush().await?;
        Ok(())
    }

    async fn on_frame(&mut self, f: FromHolder) -> Result<Option<String>> {
        if self.pipe
            && let Some(r) = self.on_pipe_frame(&f).await?
        {
            return Ok(r);
        }
        match f {
            FromHolder::Output {
                offset,
                bytes,
                replay,
                ..
            } => {
                self.feed(offset, &bytes, replay).await?;
            }
            FromHolder::Marker {
                kind: MarkerKind::Resize { cols, rows, .. },
                ..
            } => {
                let mut sc = self.rt.screen.lock().unwrap();
                if (sc.engine.cols(), sc.engine.rows()) != (cols, rows) {
                    sc.engine.resize(cols, rows);
                    sc.epoch += 1;
                    sc.rev += 1;
                    let rev = sc.rev;
                    drop(sc);
                    self.rt.rev_tx.send_replace(rev);
                    self.server.screen_dirty.notify_waiters();
                    self.server.pane_resized(&self.rt.id, cols, rows);
                }
            }
            FromHolder::Marker { .. } => {}
            FromHolder::Gap { .. } => {
                // Journal overflowed past our snapshot: reset and replay what remains.
                let mut sc = self.rt.screen.lock().unwrap();
                let (c, r) = (sc.engine.cols(), sc.engine.rows());
                sc.engine = new_engine(c, r);
                sc.engine.set_replaying(true);
                self.method = "lost".into();
            }
            FromHolder::ReplayDone { queued_queries, .. } => {
                if self.replaying {
                    self.replaying = false;
                    {
                        let mut sc = self.rt.screen.lock().unwrap();
                        sc.engine.set_replaying(false);
                        sc.recovering = false;
                        sc.epoch += 1;
                        sc.rev += 1;
                    }
                    // Answer screen-dependent queries the holder queued while we were away.
                    for q in queued_queries {
                        let mut fx = Vec::new();
                        self.rt
                            .screen
                            .lock()
                            .unwrap()
                            .engine
                            .feed(&q.bytes, &mut fx);
                        for e in fx {
                            if let Effect::Reply(b) = e {
                                let id = self.server.next_internal_input_id();
                                self.send(&ToHolder::Input {
                                    epoch: self.epoch,
                                    input_id: id,
                                    bytes: b,
                                })
                                .await?;
                            }
                        }
                    }
                    self.server.mark_recovered(&self.rt.id, Some(&self.method));
                    if self.method != "reconnect" {
                        self.nudge().await?;
                    }
                    let rev = self.rt.screen.lock().unwrap().rev;
                    self.rt.rev_tx.send_replace(rev);
                    self.server.screen_dirty.notify_waiters();
                }
                let _ = self.send(&ToHolder::StatusQuery).await;
            }
            FromHolder::InputAck {
                input_id, status, ..
            } => self.ledger.complete(input_id, status),
            FromHolder::Status(st) => {
                self.server.pane_status(&self.rt.id, &st);
                *self.rt.status.lock().unwrap() = Some(st);
                // A `cd` in a shell without OSC 7 shows up only in the live process cwd.
                self.server.refresh_auto_name(&self.rt.id);
            }
            FromHolder::FgChanged => {
                self.send(&ToHolder::StatusQuery).await?;
            }
            FromHolder::ChildExited { exit_code, signal } => {
                self.exited = Some((exit_code, signal));
                self.server.pane_exited(&self.rt.id, exit_code, signal);
                self.send(&ToHolder::AckExit { epoch: self.epoch }).await?;
                return Ok(Some(format!(
                    "exited:{}",
                    exit_code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| format!("sig{}", signal.unwrap_or(0)))
                )));
            }
            FromHolder::CheckpointWanted { .. } => {
                self.maybe_snapshot(true).await?;
            }
            FromHolder::Ping { nonce } => self.send(&ToHolder::Pong { nonce }).await?,
            FromHolder::Rejected { reason } => bail!("holder rejected: {reason}"),
            FromHolder::HelloOk { .. } | FromHolder::Acquired { .. } | FromHolder::Pong { .. } => {}
        }
        Ok(None)
    }

    /// Pipe-mode frames (01 §1.2): journal bytes go to the headless adapter. Returns `None` for
    /// frames the generic handler takes (status, exit, pings).
    async fn on_pipe_frame(&mut self, f: &FromHolder) -> Result<Option<Option<String>>> {
        let server = self.server.clone();
        match f {
            FromHolder::Output {
                offset,
                stream,
                bytes,
                ..
            } => {
                if self.awaiting_attach && self.session.is_none() {
                    let held: usize = self.prebuf.iter().map(|(_, _, b)| b.len()).sum();
                    if held + bytes.len() <= PREBUF_MAX {
                        self.prebuf.push((*stream, *offset, bytes.clone()));
                    }
                    return Ok(Some(None));
                }
                let acts = match self.session.as_mut() {
                    Some(s) => s.on_output(&server, *stream, *offset, bytes),
                    // No record (e.g. a pipe pane without an adapter): show the raw streams.
                    None if *stream != Stream::Stdin => {
                        vec![Act::Render(String::from_utf8_lossy(bytes).into_owned())]
                    }
                    None => vec![],
                };
                self.pipe_acts(acts).await?;
                Ok(Some(None))
            }
            FromHolder::Marker {
                kind: MarkerKind::InputWritten { input_id },
                ..
            } => {
                if let Some(s) = self.session.as_mut() {
                    s.on_input_written(*input_id);
                }
                Ok(Some(None))
            }
            FromHolder::Marker { .. } | FromHolder::CheckpointWanted { .. } => Ok(Some(None)),
            FromHolder::Gap { available_from, .. } => {
                if let Some(s) = self.session.as_mut() {
                    s.on_gap(*available_from);
                }
                self.method = "journal_gap".into();
                Ok(Some(None))
            }
            FromHolder::ReplayDone { .. } => {
                if self.replaying {
                    self.replaying = false;
                    {
                        let mut sc = self.rt.screen.lock().unwrap();
                        sc.recovering = false;
                        sc.epoch += 1;
                        sc.rev += 1;
                    }
                    let acts = match self.session.as_mut() {
                        Some(s) => s.replay_done(&server),
                        None => vec![],
                    };
                    self.pipe_acts(acts).await?;
                    self.server.mark_recovered(&self.rt.id, Some(&self.method));
                    let rev = self.rt.screen.lock().unwrap().rev;
                    self.rt.rev_tx.send_replace(rev);
                    self.server.screen_dirty.notify_waiters();
                }
                let _ = self.send(&ToHolder::StatusQuery).await;
                Ok(Some(None))
            }
            FromHolder::InputAck {
                input_id, status, ..
            } => {
                self.ledger.complete(*input_id, *status);
                let acts = match self.session.as_mut() {
                    Some(s) => s.on_ack(&server, *input_id, *status),
                    None => vec![],
                };
                self.pipe_acts(acts).await?;
                Ok(Some(None))
            }
            FromHolder::ChildExited { exit_code, signal } => {
                if let Some(s) = self.session.as_mut() {
                    s.on_exit(&server, *exit_code, *signal);
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Carry out what the headless adapter asked for.
    async fn pipe_acts(&mut self, acts: Vec<Act>) -> Result<()> {
        for a in acts {
            match a {
                Act::Render(t) => self.render_view(&t),
                Act::Write { id, bytes } => {
                    if self.exited.is_some() {
                        continue;
                    }
                    if self.ledger.add(id, &bytes, None) {
                        self.send(&ToHolder::Input {
                            epoch: self.epoch,
                            input_id: id,
                            bytes,
                        })
                        .await?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Pipe mode: show transcript text in the pane (the engine renders it; nothing here is a
    /// terminal query, so effects are dropped).
    fn render_view(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let bytes = text
            .replace("\r\n", "\n")
            .replace('\n', "\r\n")
            .into_bytes();
        let (rev, archive) = {
            let mut sc = self.rt.screen.lock().unwrap();
            self.effects.clear();
            sc.engine.feed(&bytes, &mut self.effects);
            self.effects.clear();
            sc.last_output = Instant::now();
            let _ = sc.engine.take_damage();
            sc.rev += 1;
            (sc.rev, take_archive(&mut sc))
        };
        if !archive.is_empty() {
            self.server.archive_rows(&self.rt.id, archive);
        }
        if !self.replaying {
            self.rt.rev_tx.send_replace(rev);
            self.server.screen_dirty.notify_waiters();
            self.server.pane_output(&self.rt.id);
        }
    }

    async fn feed(&mut self, offset: u64, bytes: &[u8], replay: bool) -> Result<()> {
        let mut replies = Vec::new();
        let mut archive = Vec::new();
        let rev;
        let effects;
        let skip;
        {
            let mut sc = self.rt.screen.lock().unwrap();
            let end = offset + bytes.len() as u64;
            if end <= sc.fed_offset && sc.fed_offset != 0 {
                return Ok(()); // already applied (snapshot covers it)
            }
            skip = sc.fed_offset.saturating_sub(offset) as usize;
            let data = &bytes[skip.min(bytes.len())..];
            self.effects.clear();
            sc.engine.feed(data, &mut self.effects);
            sc.fed_offset = end;
            sc.last_output = Instant::now();
            let _ = sc.engine.take_damage();
            sc.rev += 1;
            rev = sc.rev;
            archive.extend(take_archive(&mut sc));
            effects = std::mem::take(&mut self.effects);
        }
        self.snapshot_dirty = true;
        self.arm_snapshot();
        if !archive.is_empty() {
            self.server.archive_rows(&self.rt.id, archive);
        }
        let replaying = replay || self.replaying;
        for e in effects {
            match e {
                Effect::Reply(b) if !replaying => replies.push(b),
                Effect::Reply(_) => {}
                other => self.server.pane_effect(&self.rt.id, other, replaying),
            }
        }
        for b in replies {
            let id = self.server.next_internal_input_id();
            self.send(&ToHolder::Input {
                epoch: self.epoch,
                input_id: id,
                bytes: b,
            })
            .await?;
        }
        if !replaying {
            // Preview URL/banner detection (06 B2): line split only; parsing is off this path.
            self.server
                .previews
                .on_output(&self.rt.id, &bytes[skip.min(bytes.len())..]);
            self.rt.rev_tx.send_replace(rev);
            self.server.screen_dirty.notify_waiters();
            self.server.pane_output(&self.rt.id);
            self.server.agents.on_screen(&self.server, &self.rt.id);
        }
        Ok(())
    }

    /// Resize nudge (01 §1.2): two SIGWINCHs so TUI apps repaint after recovery.
    async fn nudge(&mut self) -> Result<()> {
        let (alt, cols, rows) = {
            let sc = self.rt.screen.lock().unwrap();
            (
                sc.engine.modes().alt_screen,
                sc.engine.cols(),
                sc.engine.rows(),
            )
        };
        let agent = self
            .server
            .with_core(|c| c.run_for_pane(&self.rt.id).is_some());
        if alt || agent {
            self.send(&ToHolder::Resize {
                epoch: self.epoch,
                cols: cols.saturating_sub(1).max(2),
                rows,
                px_w: 0,
                px_h: 0,
            })
            .await?;
            tokio::time::sleep(Duration::from_millis(50)).await;
            self.send(&ToHolder::Resize {
                epoch: self.epoch,
                cols,
                rows,
                px_w: 0,
                px_h: 0,
            })
            .await?;
        }
        Ok(())
    }

    /// Pipe mode: typed input goes to the adapter's line editor and resizes are local (there
    /// is no terminal on the holder side). Returns the command when the generic path owns it.
    async fn on_pipe_cmd(&mut self, c: PaneCmd) -> Result<Option<PaneCmd>> {
        let server = self.server.clone();
        match c {
            PaneCmd::Input { bytes, ack, .. } => {
                // Consumed by the line editor, not written to the harness.
                let acts = match self.session.as_mut() {
                    Some(s) if self.exited.is_none() => s.on_keys(&server, &bytes),
                    _ => vec![],
                };
                if let Some(a) = ack {
                    let _ = a.send(if self.exited.is_some() {
                        InputStatus::ChildExited
                    } else {
                        InputStatus::Written
                    });
                }
                self.pipe_acts(acts).await?;
                Ok(None)
            }
            PaneCmd::Resize { cols, rows } => {
                let mut sc = self.rt.screen.lock().unwrap();
                if (sc.engine.cols(), sc.engine.rows()) != (cols, rows) {
                    sc.engine.resize(cols, rows);
                    sc.epoch += 1;
                    sc.rev += 1;
                    let rev = sc.rev;
                    drop(sc);
                    self.rt.rev_tx.send_replace(rev);
                    self.server.screen_dirty.notify_waiters();
                    self.server.pane_resized(&self.rt.id, cols, rows);
                }
                Ok(None)
            }
            PaneCmd::Snapshot | PaneCmd::SnapshotDue => {
                // No VT snapshots: recovery replays the journal through the adapter.
                self.snapshot_armed = false;
                self.snapshot_dirty = false;
                Ok(None)
            }
            PaneCmd::Headless(cmd) => {
                let acts = match (cmd, self.session.as_mut()) {
                    (crate::agents::headless::Cmd::Attach(rec), None) => {
                        let mut s = Session::new(&self.rt.id, *rec);
                        let mut acts = s.start(&server);
                        for (stream, off, b) in std::mem::take(&mut self.prebuf) {
                            acts.extend(s.on_output(&server, stream, off, &b));
                        }
                        self.session = Some(s);
                        self.awaiting_attach = false;
                        acts
                    }
                    (crate::agents::headless::Cmd::Prompt { ack: Some(a), .. }, None) => {
                        let _ = a.send(Err("the headless adapter is not attached".into()));
                        vec![]
                    }
                    (cmd, Some(s)) => s.on_cmd(&server, cmd),
                    (_, None) => vec![],
                };
                self.pipe_acts(acts).await?;
                Ok(None)
            }
            other => Ok(Some(other)),
        }
    }

    async fn on_cmd(&mut self, c: PaneCmd) -> Result<()> {
        let c = if self.pipe {
            match self.on_pipe_cmd(c).await? {
                Some(c) => c,
                None => return Ok(()),
            }
        } else {
            c
        };
        match c {
            PaneCmd::Headless(cmd) => {
                // Not a pipe pane: nothing to drive.
                if let crate::agents::headless::Cmd::Prompt { ack: Some(a), .. } = cmd {
                    let _ = a.send(Err("not a headless pane".into()));
                }
            }
            PaneCmd::Input { id, bytes, ack } => {
                if self.exited.is_some() {
                    if let Some(a) = ack {
                        let _ = a.send(InputStatus::ChildExited);
                    }
                    return Ok(());
                }
                if self.ledger.add(id, &bytes, ack) {
                    self.send(&ToHolder::Input {
                        epoch: self.epoch,
                        input_id: id,
                        bytes,
                    })
                    .await?;
                }
            }
            PaneCmd::Resize { cols, rows } => {
                self.send(&ToHolder::Resize {
                    epoch: self.epoch,
                    cols,
                    rows,
                    px_w: 0,
                    px_h: 0,
                })
                .await?;
            }
            PaneCmd::Signal { sig, target } => {
                self.send(&ToHolder::Signal {
                    epoch: self.epoch,
                    sig,
                    target,
                })
                .await?
            }
            PaneCmd::Status(tx) => {
                self.send(&ToHolder::StatusQuery).await?;
                let _ = tx.send(self.rt.status.lock().unwrap().clone());
            }
            PaneCmd::Close => {
                self.closing = true;
                self.send(&ToHolder::Signal {
                    epoch: self.epoch,
                    sig: Sig::Hup,
                    target: SigTarget::Child,
                })
                .await?;
                self.send(&ToHolder::Signal {
                    epoch: self.epoch,
                    sig: Sig::Hup,
                    target: SigTarget::FgPgrp,
                })
                .await?;
                let pid = self.child_pid;
                // The child's identity now (pid + start time): the escalation below must not
                // hit an unrelated process that reused the pid after the child exited.
                let started = (pid > 1)
                    .then(|| vk_hold::procinfo::info(pid).map(|i| i.start))
                    .flatten();
                // Escalate if the child ignores SIGHUP.
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    if same_process(pid, started) {
                        // SAFETY: plain kill(2) of our holder's child, re-identified above.
                        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                    }
                });
            }
            PaneCmd::Snapshot => self.maybe_snapshot(true).await?,
            PaneCmd::SnapshotDue => {
                self.snapshot_armed = false;
                if self.snapshot_dirty {
                    self.maybe_snapshot(false).await?;
                    // Still busy (or replaying): come back when it can be due.
                    if self.snapshot_dirty {
                        self.arm_snapshot();
                    }
                }
            }
        }
        Ok(())
    }

    /// When the pending output can next be snapshotted: 2 s after the last output, or 30 s
    /// after the last snapshot while output never pauses; never sooner than 500 ms from now
    /// (the old poll period) so a pane that cannot snapshot yet (replaying) doesn't spin.
    fn snapshot_due(&self) -> Instant {
        let last_output = self.rt.screen.lock().unwrap().last_output;
        (last_output + SNAPSHOT_IDLE)
            .min(self.last_snapshot + SNAPSHOT_MAX_INTERVAL)
            .max(Instant::now() + Duration::from_millis(500))
    }

    /// Arm this pane's snapshot deadline once per pending batch, not per output chunk (the
    /// deadline handler re-arms if output continued).
    fn arm_snapshot(&mut self) {
        if self.snapshot_armed {
            return;
        }
        let due = tokio::time::Instant::from_std(self.snapshot_due());
        self.server.timers.arm(&self.rt.id, due, &self.rt.cmd_tx);
        self.snapshot_armed = true;
    }

    /// Snapshot when output has been idle 2 s, at most every 30 s while busy, or when forced
    /// (holder journal half full, shutdown).
    async fn maybe_snapshot(&mut self, force: bool) -> Result<()> {
        let (idle, offset) = {
            let sc = self.rt.screen.lock().unwrap();
            (sc.last_output.elapsed(), sc.fed_offset)
        };
        if !force && idle < SNAPSHOT_IDLE && self.last_snapshot.elapsed() < SNAPSHOT_MAX_INTERVAL {
            return Ok(());
        }
        if self.replaying {
            return Ok(());
        }
        let blob = self.rt.screen.lock().unwrap().engine.snapshot();
        let id = self.rt.id.clone();
        let server = self.server.clone();
        let inc = self.incarnation.clone();
        let ok =
            tokio::task::spawn_blocking(move || server.store_snapshot(&id, offset, blob, &inc))
                .await
                .unwrap_or(false);
        if ok {
            self.send(&ToHolder::Checkpoint {
                epoch: self.epoch,
                offset,
            })
            .await?;
        }
        self.last_snapshot = Instant::now();
        self.snapshot_dirty = false;
        if self.snapshot_armed {
            self.server.timers.cancel(&self.rt.id);
            self.snapshot_armed = false;
        }
        Ok(())
    }
}

#[cfg(test)]
mod scrollback_tests {
    #[test]
    fn scrollback_setting_is_clamped() {
        assert_eq!(super::clamp_scrollback(10_000), 10_000);
        assert_eq!(super::clamp_scrollback(1_500), 1_500);
        assert_eq!(super::clamp_scrollback(0), super::SCROLLBACK_MIN);
        assert_eq!(super::clamp_scrollback(u32::MAX), super::SCROLLBACK_MAX);
    }
}
