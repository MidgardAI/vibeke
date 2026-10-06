//! Per-pane runtime: holder connection, VT engine, recovery (01 §1.2, 03 §4), effects,
//! snapshots and scrollback archiving.

use crate::Server;
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
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

pub const SCROLLBACK: usize = 10_000;
const SNAPSHOT_IDLE: Duration = Duration::from_secs(2);
const SNAPSHOT_MAX_INTERVAL: Duration = Duration::from_secs(30);

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
}

impl PaneRt {
    pub fn new(id: &str, cols: u16, rows: u16) -> (Arc<PaneRt>, mpsc::UnboundedReceiver<PaneCmd>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (rev_tx, _) = watch::channel(0);
        let rt = Arc::new(PaneRt {
            id: id.to_string(),
            screen: Mutex::new(Screen {
                engine: Engine::new(cols, rows, SCROLLBACK),
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
        });
        (rt, cmd_rx)
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
        tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or(InputStatus::ChildExited)
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

/// Run a pane until it closes. Recovers from a stored snapshot when not fresh.
pub async fn run(
    server: Arc<Server>,
    rt: Arc<PaneRt>,
    mut cmd_rx: mpsc::UnboundedReceiver<PaneCmd>,
    conn: HolderConn,
) {
    let id = rt.id.clone();
    match run_inner(&server, &rt, &mut cmd_rx, conn).await {
        Ok(reason) => {
            tracing::info!(pane = %id, reason, "pane ended");
            server.pane_ended(&id, &reason);
        }
        Err(e) => {
            // The holder died without reporting a child exit (crash, kill, reboot): keep the
            // layout slot with a fresh shell and offer the agent for resume (10 §5.1).
            tracing::warn!(pane = %id, error = %format!("{e:#}"), "holder lost");
            server.holder_lost(&id);
        }
    }
}

async fn run_inner(
    server: &Arc<Server>,
    rt: &Arc<PaneRt>,
    cmd_rx: &mut mpsc::UnboundedReceiver<PaneCmd>,
    conn: HolderConn,
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
        ..
    } = hello
    else {
        bail!("unexpected hello reply: {hello:?}")
    };
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
    server.holder_epoch(&rt.id, epoch);

    // Recovery: snapshot + journal replay (03 §4).
    let mut from = 0;
    let mut method = "fresh";
    if !conn.fresh {
        method = "ring_only";
        let snap = server
            .with_core(|c| c.store.snapshot_for(&rt.id))
            .ok()
            .flatten();
        if let Some(s) = snap
            && s.version == vk_term::engine::ENGINE_VERSION
            && s.offset >= ring.start_offset
            && let Ok(e) = Engine::restore(&s.blob, SCROLLBACK)
        {
            let mut sc = rt.screen.lock().unwrap();
            sc.engine = e;
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
        acks: HashMap::new(),
        exited: None,
        closing: false,
        last_snapshot: Instant::now(),
        snapshot_dirty: false,
        replaying: !conn.fresh,
        method: method.to_string(),
        child_pid,
        effects: Vec::new(),
    };
    if conn.fresh {
        server.mark_recovered(&rt.id, None);
    }
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let result = loop {
        tokio::select! {
            f = frame_rx.recv() => {
                let Some(f) = f else { break Err(anyhow::anyhow!("holder connection closed")) };
                if let Some(reason) = p.on_frame(f).await? { break Ok(reason) }
            }
            c = cmd_rx.recv() => {
                let Some(c) = c else { break Ok("dropped".to_string()) };
                p.on_cmd(c).await?;
            }
            _ = tick.tick(), if p.snapshot_dirty => {
                p.maybe_snapshot(false).await?;
            }
        }
    };
    reader.abort();
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
    acks: HashMap<u64, oneshot::Sender<InputStatus>>,
    exited: Option<(Option<i32>, Option<i32>)>,
    closing: bool,
    last_snapshot: Instant,
    snapshot_dirty: bool,
    replaying: bool,
    method: String,
    child_pid: u32,
    effects: Vec<Effect>,
}

impl PaneLoop {
    async fn send(&mut self, m: &ToHolder) -> Result<()> {
        asyncio::write_frame(&mut self.wr, m).await?;
        self.wr.flush().await?;
        Ok(())
    }

    async fn on_frame(&mut self, f: FromHolder) -> Result<Option<String>> {
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
                sc.engine = Engine::new(c, r, SCROLLBACK);
                sc.engine.set_replaying(true);
                self.method = "ring_only".into();
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
                    self.nudge().await?;
                    let rev = self.rt.screen.lock().unwrap().rev;
                    self.rt.rev_tx.send_replace(rev);
                    self.server.screen_dirty.notify_waiters();
                }
                let _ = self.send(&ToHolder::StatusQuery).await;
            }
            FromHolder::InputAck {
                input_id, status, ..
            } => {
                if let Some(tx) = self.acks.remove(&input_id) {
                    let _ = tx.send(status);
                }
            }
            FromHolder::Status(st) => {
                self.server.pane_status(&self.rt.id, &st);
                *self.rt.status.lock().unwrap() = Some(st);
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

    async fn feed(&mut self, offset: u64, bytes: &[u8], replay: bool) -> Result<()> {
        let mut replies = Vec::new();
        let mut archive = Vec::new();
        let rev;
        let effects;
        {
            let mut sc = self.rt.screen.lock().unwrap();
            let end = offset + bytes.len() as u64;
            if end <= sc.fed_offset && sc.fed_offset != 0 {
                return Ok(()); // already applied (snapshot covers it)
            }
            let skip = sc.fed_offset.saturating_sub(offset) as usize;
            let data = &bytes[skip.min(bytes.len())..];
            self.effects.clear();
            sc.engine.feed(data, &mut self.effects);
            sc.fed_offset = end;
            sc.last_output = Instant::now();
            let _ = sc.engine.take_damage();
            sc.rev += 1;
            rev = sc.rev;
            // Archive rows that entered history (03 §11.2).
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
            effects = std::mem::take(&mut self.effects);
        }
        self.snapshot_dirty = true;
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

    async fn on_cmd(&mut self, c: PaneCmd) -> Result<()> {
        match c {
            PaneCmd::Input { id, bytes, ack } => {
                if self.exited.is_some() {
                    if let Some(a) = ack {
                        let _ = a.send(InputStatus::ChildExited);
                    }
                    return Ok(());
                }
                if let Some(a) = ack {
                    self.acks.insert(id, a);
                }
                self.send(&ToHolder::Input {
                    epoch: self.epoch,
                    input_id: id,
                    bytes,
                })
                .await?;
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
                // Escalate if the child ignores SIGHUP.
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    // SAFETY: plain kill(2); the pid belongs to our holder's child.
                    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                });
            }
            PaneCmd::Snapshot => self.maybe_snapshot(true).await?,
        }
        Ok(())
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
        let ok = tokio::task::spawn_blocking(move || server.store_snapshot(&id, offset, blob))
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
        Ok(())
    }
}
