//! Channel multiplexer over one byte stream (SSH stdio), 06 A4.
//!
//! Frames: `u32 len | postcard(Frame)`. Each channel is a bidirectional byte stream; a channel
//! of kind `socket` is connected by the bridge to the remote server's Unix socket, so the
//! control and render protocols run over it unchanged. Data is chunked (≤ 16 KiB) and the
//! writer serves channels round-robin, so a bulk upload never queues keystrokes behind it.
//! Per-channel credit windows bound memory on both ends.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::sync::{Notify, mpsc, oneshot};
use vk_proto::frame::asyncio;

pub const PROTO: u32 = 1;
const CHUNK: usize = 16 * 1024;
const WINDOW: u32 = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Frame {
    Hello {
        proto: u32,
        version: String,
        role: String,
    },
    Open {
        ch: u32,
        kind: String,
    },
    OpenOk {
        ch: u32,
    },
    OpenErr {
        ch: u32,
        msg: String,
    },
    Data {
        ch: u32,
        bytes: Vec<u8>,
    },
    /// Receiver consumed `bytes`; sender may send that much more.
    Window {
        ch: u32,
        bytes: u32,
    },
    Close {
        ch: u32,
    },
    Ping {
        ts: u64,
    },
    Pong {
        ts: u64,
    },
}

/// Stats for the status bar and bandwidth budgets (06 A7, 10 §1.5).
#[derive(Default)]
pub struct Stats {
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    pub rtt_us: AtomicU64,
}

/// Per-channel flow-control and lifecycle state shared by the channel tasks and the reader.
struct ChanState {
    /// Bytes we may still send (peer's grant to us).
    credit: AtomicU32,
    credit_wake: Notify,
    /// Bytes the peer may still send (what we granted minus what it has sent).
    inbound: AtomicU64,
    /// Channel torn down (protocol violation or link loss): tasks end without clean EOF.
    aborted: AtomicBool,
    kill: Notify,
    sent_close: AtomicBool,
    recv_close: AtomicBool,
}

impl ChanState {
    fn new() -> ChanState {
        ChanState {
            credit: AtomicU32::new(WINDOW),
            credit_wake: Notify::new(),
            inbound: AtomicU64::new(WINDOW as u64),
            aborted: AtomicBool::new(false),
            kill: Notify::new(),
            sent_close: AtomicBool::new(false),
            recv_close: AtomicBool::new(false),
        }
    }

    fn abort(&self) {
        self.aborted.store(true, Ordering::SeqCst);
        // `notify_one` stores a permit, so a task that is not waiting yet still wakes.
        self.kill.notify_one();
        self.credit_wake.notify_one();
    }
}

struct Chan {
    /// `None` once the peer sent Close (dropping the sender lets the link->local task drain
    /// everything queued, then shut the local write half down).
    to_local: Option<mpsc::Sender<Vec<u8>>>,
    st: Arc<ChanState>,
}

struct Shared {
    chans: Mutex<HashMap<u32, Chan>>,
    pending_open: Mutex<HashMap<u32, oneshot::Sender<Result<(), String>>>>,
    out: mpsc::UnboundedSender<Frame>,
    next: AtomicU32,
    stats: Arc<Stats>,
    closed: Notify,
    writer_stop: Notify,
    /// Set once the link is gone; checked after every registration to close the race with
    /// the teardown sweep.
    link_down: AtomicBool,
}

/// Handle to a running multiplexer.
#[derive(Clone)]
pub struct Mux {
    shared: Arc<Shared>,
    pub remote_version: Arc<Mutex<Option<String>>>,
}

/// Called on the accepting side for each `Open{kind}`; returns the stream to bridge to.
pub type Acceptor =
    Arc<dyn Fn(String) -> futures_util::BoxFuture<Result<Box<dyn Stream>>> + Send + Sync>;
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

pub mod futures_util {
    pub type BoxFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>;
}

impl Mux {
    /// Start a mux over `rd`/`wr`. `role` is "client" (opens channels) or "bridge" (accepts).
    pub fn start<R, W>(rd: R, wr: W, role: &str, acceptor: Option<Acceptor>) -> Mux
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (out_tx, out_rx) = mpsc::unbounded_channel::<Frame>();
        let shared = Arc::new(Shared {
            chans: Mutex::new(HashMap::new()),
            pending_open: Mutex::new(HashMap::new()),
            out: out_tx,
            next: AtomicU32::new(if role == "client" { 1 } else { 2 }),
            stats: Arc::new(Stats::default()),
            closed: Notify::new(),
            writer_stop: Notify::new(),
            link_down: AtomicBool::new(false),
        });
        let mux = Mux {
            shared: shared.clone(),
            remote_version: Arc::new(Mutex::new(None)),
        };
        let _ = shared.out.send(Frame::Hello {
            proto: PROTO,
            version: vk_proto::VERSION.into(),
            role: role.into(),
        });
        tokio::spawn(writer(wr, out_rx, shared.clone()));
        let m2 = mux.clone();
        tokio::spawn(async move {
            let _ = reader(rd, m2.clone(), acceptor).await;
            link_down(&m2.shared);
        });
        // Keepalive + RTT (06 A4: ping every 5 s).
        let m3 = mux.clone();
        tokio::spawn(async move {
            let t0 = Instant::now();
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if m3.shared.link_down.load(Ordering::SeqCst) {
                    break;
                }
                if m3
                    .shared
                    .out
                    .send(Frame::Ping {
                        ts: t0.elapsed().as_micros() as u64,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        mux
    }

    pub fn stats(&self) -> Arc<Stats> {
        self.shared.stats.clone()
    }

    pub fn is_closed(&self) -> bool {
        self.shared.link_down.load(Ordering::SeqCst) || self.shared.out.is_closed()
    }

    pub async fn closed(&self) {
        if self.is_closed() {
            return;
        }
        self.shared.closed.notified().await
    }

    /// Open a channel of `kind` on the remote side; returns a local byte stream.
    pub async fn open(&self, kind: &str) -> Result<DuplexStream> {
        if self.is_closed() {
            return Err(anyhow!("link closed"));
        }
        let ch = self.shared.next.fetch_add(2, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.shared.pending_open.lock().unwrap().insert(ch, tx);
        if self.shared.link_down.load(Ordering::SeqCst) {
            self.shared.pending_open.lock().unwrap().remove(&ch);
            return Err(anyhow!("link closed"));
        }
        let (local, remote_end) = tokio::io::duplex(WINDOW as usize);
        attach(&self.shared, ch, remote_end);
        self.shared
            .out
            .send(Frame::Open {
                ch,
                kind: kind.into(),
            })
            .map_err(|_| anyhow!("link closed"))?;
        match tokio::time::timeout(Duration::from_secs(10), rx).await {
            Ok(Ok(Ok(()))) => Ok(local),
            Ok(Ok(Err(e))) => {
                self.shared.chans.lock().unwrap().remove(&ch);
                Err(anyhow!("open {kind}: {e}"))
            }
            _ => {
                self.shared.chans.lock().unwrap().remove(&ch);
                Err(anyhow!("open {kind}: timed out"))
            }
        }
    }
}

/// Link is gone: end every channel task, fail pending opens, stop the writer.
fn link_down(shared: &Arc<Shared>) {
    shared.link_down.store(true, Ordering::SeqCst);
    let chans: Vec<Chan> = shared
        .chans
        .lock()
        .unwrap()
        .drain()
        .map(|(_, c)| c)
        .collect();
    for c in chans {
        c.st.abort(); // wakes tasks blocked on credit or on the local read
    } // dropping `to_local` ends the link->local tasks
    for (_, tx) in shared.pending_open.lock().unwrap().drain() {
        let _ = tx.send(Err("link closed".into()));
    }
    shared.closed.notify_waiters();
    shared.writer_stop.notify_one();
}

/// Tear one channel down after a protocol violation by the peer.
fn abort_channel(shared: &Arc<Shared>, ch: u32, why: &str) {
    tracing::warn!("mux: channel {ch}: {why}; closing");
    let c = shared.chans.lock().unwrap().remove(&ch);
    if let Some(c) = c {
        c.st.abort();
        if !c.st.sent_close.swap(true, Ordering::SeqCst) {
            let _ = shared.out.send(Frame::Close { ch });
        }
    }
}

/// Wire a duplex end into channel `ch`: bytes the local user writes go out as Data frames
/// (respecting the peer's credit window); incoming Data is written into it.
fn attach(shared: &Arc<Shared>, ch: u32, end: DuplexStream) {
    let (mut rd, mut wr) = tokio::io::split(end);
    // Bounded by construction: the reader admits at most WINDOW unconsumed bytes and rejects
    // empty frames, so at most WINDOW frames can ever be queued.
    let (to_local, mut from_link) = mpsc::channel::<Vec<u8>>(WINDOW as usize);
    let st = Arc::new(ChanState::new());
    shared.chans.lock().unwrap().insert(
        ch,
        Chan {
            to_local: Some(to_local),
            st: st.clone(),
        },
    );
    if shared.link_down.load(Ordering::SeqCst) {
        shared.chans.lock().unwrap().remove(&ch);
        st.abort();
    }
    let out = shared.out.clone();
    // link -> local
    let out2 = out.clone();
    let st2 = st.clone();
    tokio::spawn(async move {
        while let Some(b) = from_link.recv().await {
            if st2.aborted.load(Ordering::SeqCst) {
                return;
            }
            let n = b.len() as u64;
            if wr.write_all(&b).await.is_err() {
                st2.abort();
                return;
            }
            st2.inbound.fetch_add(n, Ordering::SeqCst);
            let _ = out2.send(Frame::Window {
                ch,
                bytes: n as u32,
            });
        }
        // Sender dropped (peer Close, after everything queued was written): clean EOF.
        if !st2.aborted.load(Ordering::SeqCst) {
            let _ = wr.shutdown().await;
        }
    });
    // local -> link
    let shared2 = shared.clone();
    tokio::spawn(async move {
        let mut buf = vec![0u8; CHUNK];
        'outer: loop {
            // Wait for credit (or teardown).
            while st.credit.load(Ordering::Acquire) == 0 {
                if st.aborted.load(Ordering::SeqCst) {
                    break 'outer;
                }
                st.credit_wake.notified().await;
            }
            if st.aborted.load(Ordering::SeqCst) {
                break;
            }
            let allowed = st.credit.load(Ordering::Acquire).min(CHUNK as u32) as usize;
            let n = tokio::select! {
                r = rd.read(&mut buf[..allowed]) => match r {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                },
                _ = st.kill.notified() => break,
            };
            st.credit.fetch_sub(n as u32, Ordering::AcqRel);
            if out
                .send(Frame::Data {
                    ch,
                    bytes: buf[..n].to_vec(),
                })
                .is_err()
            {
                break;
            }
        }
        // Close goes through the same per-channel queue as Data, so EOF follows every byte.
        if !st.sent_close.swap(true, Ordering::SeqCst) {
            let _ = out.send(Frame::Close { ch });
        }
        remove_if_done(&shared2, ch, &st);
    });
}

/// Forget a channel once both directions have ended.
fn remove_if_done(shared: &Shared, ch: u32, st: &ChanState) {
    if st.sent_close.load(Ordering::SeqCst) && st.recv_close.load(Ordering::SeqCst) {
        shared.chans.lock().unwrap().remove(&ch);
    }
}

async fn writer<W: AsyncWrite + Unpin>(
    wr: W,
    mut rx: mpsc::UnboundedReceiver<Frame>,
    shared: Arc<Shared>,
) {
    let stats = shared.stats.clone();
    let mut wr = tokio::io::BufWriter::with_capacity(64 * 1024, wr);
    // Round-robin over channels with queued data; other control frames go first. Close is
    // queued behind its channel's Data so it can never overtake it.
    let mut queues: VecDeque<(u32, VecDeque<Frame>)> = VecDeque::new();
    loop {
        let first = if queues.is_empty() {
            tokio::select! {
                f = rx.recv() => match f {
                    Some(f) => Some(f),
                    None => break,
                },
                _ = shared.writer_stop.notified() => break,
            }
        } else {
            None
        };
        if shared.link_down.load(Ordering::SeqCst) {
            break;
        }
        let mut incoming: Vec<Frame> = first.into_iter().collect();
        while let Ok(f) = rx.try_recv() {
            incoming.push(f);
        }
        for f in incoming {
            match &f {
                Frame::Data { ch, .. } | Frame::Close { ch } => {
                    let ch = *ch;
                    match queues.iter_mut().find(|(c, _)| *c == ch) {
                        Some((_, q)) => q.push_back(f),
                        None => queues.push_back((ch, VecDeque::from([f]))),
                    }
                }
                _ => {
                    if write_one(&mut wr, &f, &stats).await.is_err() {
                        return;
                    }
                }
            }
        }
        // One data frame per channel per round.
        let rounds = queues.len();
        for _ in 0..rounds {
            if let Some((ch, mut q)) = queues.pop_front() {
                if let Some(f) = q.pop_front()
                    && write_one(&mut wr, &f, &stats).await.is_err()
                {
                    return;
                }
                if !q.is_empty() {
                    queues.push_back((ch, q));
                }
            }
        }
        if wr.flush().await.is_err() {
            return;
        }
    }
}

async fn write_one<W: AsyncWrite + Unpin>(wr: &mut W, f: &Frame, stats: &Stats) -> Result<()> {
    let bytes = vk_proto::frame::encode(f)?;
    stats
        .bytes_out
        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
    wr.write_all(&bytes).await?;
    Ok(())
}

async fn reader<R: AsyncRead + Unpin>(rd: R, mux: Mux, acceptor: Option<Acceptor>) -> Result<()> {
    let mut rd = tokio::io::BufReader::with_capacity(64 * 1024, rd);
    let t0 = Instant::now();
    loop {
        let body = asyncio::read_body(&mut rd).await?;
        mux.shared
            .stats
            .bytes_in
            .fetch_add(body.len() as u64 + 4, Ordering::Relaxed);
        let f: Frame = vk_proto::frame::decode(&body)?;
        match f {
            Frame::Hello { version, proto, .. } => {
                if proto != PROTO {
                    return Err(anyhow!("bridge protocol {proto} != {PROTO}"));
                }
                *mux.remote_version.lock().unwrap() = Some(version);
            }
            Frame::Open { ch, kind } => {
                let Some(acc) = acceptor.clone() else {
                    let _ = mux.shared.out.send(Frame::OpenErr {
                        ch,
                        msg: "not accepting channels".into(),
                    });
                    continue;
                };
                let m = mux.clone();
                tokio::spawn(async move {
                    match acc(kind).await {
                        Ok(stream) => {
                            let (a, b) = tokio::io::duplex(WINDOW as usize);
                            attach(&m.shared, ch, b);
                            let _ = m.shared.out.send(Frame::OpenOk { ch });
                            let mut a = a;
                            let mut stream = stream;
                            let _ = tokio::io::copy_bidirectional(&mut a, &mut stream).await;
                        }
                        Err(e) => {
                            let _ = m.shared.out.send(Frame::OpenErr {
                                ch,
                                msg: format!("{e:#}"),
                            });
                        }
                    }
                });
            }
            Frame::OpenOk { ch } => {
                if let Some(tx) = mux.shared.pending_open.lock().unwrap().remove(&ch) {
                    let _ = tx.send(Ok(()));
                }
            }
            Frame::OpenErr { ch, msg } => {
                if let Some(tx) = mux.shared.pending_open.lock().unwrap().remove(&ch) {
                    let _ = tx.send(Err(msg));
                }
            }
            Frame::Data { ch, bytes } => {
                let violation = {
                    let mut chans = mux.shared.chans.lock().unwrap();
                    match chans.get_mut(&ch) {
                        // Late data for a channel we already tore down.
                        None => None,
                        Some(c) => {
                            let n = bytes.len() as u64;
                            match &c.to_local {
                                None => Some("data after close"),
                                Some(_) if n == 0 => Some("empty data frame"),
                                Some(_) if n > c.st.inbound.load(Ordering::SeqCst) => {
                                    Some("exceeded the granted credit window")
                                }
                                Some(tx) => {
                                    c.st.inbound.fetch_sub(n, Ordering::SeqCst);
                                    tx.try_send(bytes).err().map(|_| "receive queue overflow")
                                }
                            }
                        }
                    }
                };
                if let Some(why) = violation {
                    abort_channel(&mux.shared, ch, why);
                }
            }
            Frame::Window { ch, bytes } => {
                if let Some(c) = mux.shared.chans.lock().unwrap().get(&ch) {
                    let mut cur = c.st.credit.load(Ordering::Acquire);
                    while let Err(v) = c.st.credit.compare_exchange_weak(
                        cur,
                        cur.saturating_add(bytes),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        cur = v;
                    }
                    c.st.credit_wake.notify_one();
                }
            }
            Frame::Close { ch } => {
                let mut chans = mux.shared.chans.lock().unwrap();
                if let Some(c) = chans.get_mut(&ch) {
                    // Drop the sender: the link->local task drains what is queued, then EOFs.
                    c.to_local = None;
                    c.st.recv_close.store(true, Ordering::SeqCst);
                    if c.st.sent_close.load(Ordering::SeqCst) {
                        chans.remove(&ch);
                    }
                }
            }
            Frame::Ping { ts } => {
                let _ = mux.shared.out.send(Frame::Pong { ts });
            }
            Frame::Pong { ts } => {
                let now = t0.elapsed().as_micros() as u64;
                mux.shared
                    .stats
                    .rtt_us
                    .store(now.saturating_sub(ts), Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn echo_channels_interleave() {
        let (a, b) = tokio::io::duplex(1 << 20);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        // Bridge side: each channel is an echo server.
        let acc: Acceptor = Arc::new(|kind: String| {
            Box::pin(async move {
                let (x, y) = tokio::io::duplex(1 << 16);
                tokio::spawn(async move {
                    let (mut r, mut w) = tokio::io::split(y);
                    let _ = w.write_all(format!("hello {kind}\n").as_bytes()).await;
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
                Ok(Box::new(x) as Box<dyn Stream>)
            })
        });
        let _bridge = Mux::start(br, bw, "bridge", Some(acc));
        let client = Mux::start(ar, aw, "client", None);
        let mut c1 = client.open("socket").await.unwrap();
        let c2 = client.open("blob").await.unwrap();
        let mut buf = vec![0u8; 13];
        c1.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello socket\n");
        // A large transfer on c2 does not block a small round trip on c1.
        let big = vec![7u8; 2 << 20];
        let big2 = big.clone();
        let (mut r2, mut w2) = tokio::io::split(c2);
        let mut hello = vec![0u8; 11];
        r2.read_exact(&mut hello).await.unwrap();
        let sender = tokio::spawn(async move { w2.write_all(&big2).await.unwrap() });
        c1.write_all(b"ping").await.unwrap();
        let mut p = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(2), c1.read_exact(&mut p))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&p, b"ping");
        let mut got = vec![0u8; big.len()];
        tokio::time::timeout(Duration::from_secs(10), r2.read_exact(&mut got))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got, big);
        sender.await.unwrap();
        assert!(client.stats().bytes_out.load(Ordering::Relaxed) > 2 << 20);
    }

    async fn send_raw<W: AsyncWrite + Unpin>(w: &mut W, f: &Frame) {
        w.write_all(&vk_proto::frame::encode(f).unwrap())
            .await
            .unwrap();
    }

    async fn recv_raw<R: AsyncRead + Unpin>(r: &mut R) -> Option<Frame> {
        let body = asyncio::read_body(r).await.ok()?;
        vk_proto::frame::decode(&body).ok()
    }

    fn sink_acceptor() -> Acceptor {
        // The accepted stream is never read, so the channel's receive path backs up.
        Arc::new(|_k: String| {
            Box::pin(async move {
                let (x, y) = tokio::io::duplex(1024);
                tokio::spawn(async move {
                    let _keep = y;
                    std::future::pending::<()>().await;
                });
                Ok(Box::new(x) as Box<dyn Stream>)
            })
        })
    }

    #[tokio::test]
    async fn data_precedes_eof_under_load() {
        let (a, b) = tokio::io::duplex(1 << 20);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let (got_tx, got_rx) = oneshot::channel::<Vec<u8>>();
        let got_tx = Arc::new(Mutex::new(Some(got_tx)));
        let acc: Acceptor = Arc::new(move |_k: String| {
            let got_tx = got_tx.clone();
            Box::pin(async move {
                let (x, mut y) = tokio::io::duplex(1 << 16);
                tokio::spawn(async move {
                    let mut all = Vec::new();
                    let _ = y.read_to_end(&mut all).await; // returns only on EOF
                    if let Some(t) = got_tx.lock().unwrap().take() {
                        let _ = t.send(all);
                    }
                });
                Ok(Box::new(x) as Box<dyn Stream>)
            })
        });
        let _bridge = Mux::start(br, bw, "bridge", Some(acc));
        let client = Mux::start(ar, aw, "client", None);
        let mut c = client.open("socket").await.unwrap();
        let mut expect = Vec::new();
        for i in 0..400u32 {
            let chunk: Vec<u8> = (0..(1 + (i as usize * 37) % 9000))
                .map(|j| (i as usize + j) as u8)
                .collect();
            c.write_all(&chunk).await.unwrap();
            expect.extend_from_slice(&chunk);
        }
        c.shutdown().await.unwrap(); // EOF right behind the last bytes
        let got = tokio::time::timeout(Duration::from_secs(20), got_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.len(), expect.len());
        assert!(got == expect);
    }

    async fn raw_open<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(r: &mut R, w: &mut W, ch: u32) {
        send_raw(
            w,
            &Frame::Open {
                ch,
                kind: "x".into(),
            },
        )
        .await;
        loop {
            if let Some(Frame::OpenOk { ch: c }) = recv_raw(r).await
                && c == ch
            {
                break;
            }
        }
    }

    #[tokio::test]
    async fn credit_violation_closes_channel() {
        let (a, b) = tokio::io::duplex(8 << 20);
        let (mut hr, mut hw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let _bridge = Mux::start(br, bw, "bridge", Some(sink_acceptor()));
        raw_open(&mut hr, &mut hw, 1).await;
        // Hostile peer: ignore the window and keep sending until the bridge closes the channel.
        let mut sent = 0usize;
        let closed = loop {
            send_raw(
                &mut hw,
                &Frame::Data {
                    ch: 1,
                    bytes: vec![1; CHUNK],
                },
            )
            .await;
            sent += CHUNK;
            let mut saw_close = false;
            while let Ok(Some(f)) =
                tokio::time::timeout(Duration::from_millis(1), recv_raw(&mut hr)).await
            {
                if f == (Frame::Close { ch: 1 }) {
                    saw_close = true;
                }
            }
            if saw_close {
                break true;
            }
            if sent > 16 * WINDOW as usize {
                break false;
            }
        };
        assert!(closed, "bridge never closed the channel");
        // Bytes can only sit in the queue (<= WINDOW) and one duplex hop (<= WINDOW).
        assert!(sent <= 3 * WINDOW as usize + 1024, "accepted {sent} bytes");
        // A single oversized frame is refused immediately on a fresh channel.
        raw_open(&mut hr, &mut hw, 3).await;
        send_raw(
            &mut hw,
            &Frame::Data {
                ch: 3,
                bytes: vec![0; WINDOW as usize + 1],
            },
        )
        .await;
        loop {
            match tokio::time::timeout(Duration::from_secs(5), recv_raw(&mut hr)).await {
                Ok(Some(Frame::Close { ch: 3 })) => break,
                Ok(Some(_)) => {}
                _ => panic!("no Close for oversized frame"),
            }
        }
    }

    #[tokio::test]
    async fn link_drop_wakes_writer_blocked_on_credit() {
        let (a, b) = tokio::io::duplex(8 << 20);
        let (ar, aw) = tokio::io::split(a);
        let (mut hr, mut hw) = tokio::io::split(b);
        let client = Mux::start(ar, aw, "client", None);
        let c2 = client.clone();
        let open = tokio::spawn(async move { c2.open("socket").await });
        let ch = loop {
            if let Some(Frame::Open { ch, .. }) = recv_raw(&mut hr).await {
                break ch;
            }
        };
        send_raw(&mut hw, &Frame::OpenOk { ch }).await;
        let mut c = open.await.unwrap().unwrap();
        // The peer never grants more credit: the writer stalls after WINDOW bytes.
        let writer = tokio::spawn(async move {
            let data = vec![9u8; 4 << 20];
            c.write_all(&data).await
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!writer.is_finished());
        drop(hr);
        drop(hw); // link drops
        let r = tokio::time::timeout(Duration::from_secs(5), writer)
            .await
            .expect("writer still blocked after link drop")
            .unwrap();
        assert!(r.is_err());
        client.closed().await;
        assert!(client.open("socket").await.is_err());
    }
}
