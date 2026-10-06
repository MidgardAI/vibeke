//! Channel multiplexer over one byte stream (SSH stdio), 06 A4.
//!
//! Frames: `u32 len | postcard(Frame)`. Each channel is a bidirectional byte stream; a channel
//! of kind `socket` is connected by the bridge to the remote server's Unix socket, so the
//! control and render protocols run over it unchanged. Data is chunked (≤ 16 KiB) and the
//! writer serves channels by **priority class** (control/input > render > events > forwards >
//! blobs) with a weighted scheduler, so a bulk upload never queues keystrokes behind it.
//! Per-channel credit windows bound memory on both ends.
//!
//! Capabilities ride in the `Hello.role` string (`client;caps=prio,zstd:<dict-id>`), which
//! older peers ignore. A peer that advertised `prio` gets the opener's class as a `#<class>`
//! suffix on `Open.kind` (so both directions of the channel are scheduled by it); a peer that
//! advertised the same zstd dictionary gets `DataZ` frames (zstd with the shared dictionary,
//! [`crate::dict`]) on render and blob channels when this side has compression enabled
//! ([`MuxOpts::compress`]; off for loopback links).

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
/// Frames shorter than this are never compressed (the zstd header eats the gain).
const MIN_COMPRESS: usize = 64;
/// How long `open` waits for the peer's `Hello` (its capabilities) before opening without a
/// class hint. Every peer version sends `Hello` first, so this only matters for a peer that
/// is not a Vibeke mux at all.
const HELLO_WAIT: Duration = Duration::from_secs(5);

/// Scheduling class of a channel (06 A4): control/input > render > events > forwards > blobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Class {
    Control = 0,
    Render = 1,
    Events = 2,
    Forward = 3,
    Blob = 4,
}

impl Class {
    pub const ALL: [Class; 5] = [
        Class::Control,
        Class::Render,
        Class::Events,
        Class::Forward,
        Class::Blob,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Class::Control => "control",
            Class::Render => "render",
            Class::Events => "events",
            Class::Forward => "forward",
            Class::Blob => "blob",
        }
    }

    pub fn parse(s: &str) -> Option<Class> {
        Class::ALL.into_iter().find(|c| c.name() == s)
    }

    /// Frames a class may send per scheduling round while other classes also have data
    /// queued. A class alone on the link always gets the whole link.
    pub fn weight(self) -> u32 {
        match self {
            Class::Control => 16,
            Class::Render => 8,
            Class::Events => 4,
            Class::Forward => 2,
            Class::Blob => 1,
        }
    }

    /// The class of a channel kind opened without an explicit class.
    pub fn for_kind(kind: &str) -> Class {
        if kind.starts_with("tcp:") || kind.starts_with("egress:") {
            Class::Forward
        } else if kind == "blob" {
            Class::Blob
        } else {
            Class::Control
        }
    }

    /// zstd applies to render and blob channels (06 A4).
    pub fn compressible(self) -> bool {
        matches!(self, Class::Render | Class::Blob)
    }
}

/// Split `kind#class` (as sent to a peer that advertised `prio`) into the kind the acceptor
/// sees and the class. A kind without a valid suffix keeps its default class.
pub fn split_class(kind: &str) -> (String, Class) {
    if let Some((k, c)) = kind.rsplit_once('#')
        && let Some(class) = Class::parse(c)
    {
        return (k.to_string(), class);
    }
    (kind.to_string(), Class::for_kind(kind))
}

/// Per-mux options.
#[derive(Debug, Clone, Copy, Default)]
pub struct MuxOpts {
    /// Compress render and blob channel data with zstd when the peer supports the same
    /// dictionary, and ask the peer to do the same (advertise `zstd:<id>`). Off for loopback
    /// links (06 A4). The bridge turns it on, so it follows whatever the client advertises.
    pub compress: bool,
}

/// What the peer's `Hello` advertised.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerCaps {
    /// Understands `#class` suffixes on `Open.kind`.
    pub prio: bool,
    /// Decodes `DataZ` with our dictionary (same id) and wants compressed data.
    pub zstd: bool,
}

/// Our `Hello.role` with capability tokens.
pub fn role_with_caps(role: &str, opts: MuxOpts) -> String {
    let mut caps = vec!["prio".to_string()];
    if opts.compress {
        caps.push(format!("zstd:{}", crate::dict::id()));
    }
    format!("{role};caps={}", caps.join(","))
}

/// Parse a peer's `Hello.role` (`bridge`, or `bridge;caps=prio,zstd:<id>`).
pub fn parse_caps(role: &str) -> PeerCaps {
    let mut pc = PeerCaps::default();
    for part in role.split(';').skip(1) {
        if let Some(list) = part.strip_prefix("caps=") {
            for c in list.split(',') {
                if c == "prio" {
                    pc.prio = true;
                } else if let Some(id) = c.strip_prefix("zstd:") {
                    pc.zstd = id == crate::dict::id();
                }
            }
        }
    }
    pc
}

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
    /// `Data` compressed with zstd and the shared dictionary ([`crate::dict`]). Appended, so
    /// earlier discriminants are unchanged; only sent to a peer that advertised the same
    /// dictionary id. Credit counts the decompressed bytes.
    DataZ {
        ch: u32,
        bytes: Vec<u8>,
    },
}

/// Stats for the status bar and bandwidth budgets (06 A7, 10 §1.5).
#[derive(Default)]
pub struct Stats {
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    pub rtt_us: AtomicU64,
    /// Channel payload bytes sent before compression (`bytes_out` is what hit the wire).
    pub payload_out: AtomicU64,
    /// Data frames sent compressed.
    pub zstd_frames_out: AtomicU64,
    /// Wall clock (unix ms) of the last frame received: "last seen" and loss detection (A7).
    pub last_rx_ms: AtomicU64,
    /// Wall clock (unix ms) of the oldest Ping still waiting for its Pong (0 = none).
    pub ping_outstanding_ms: AtomicU64,
}

pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
    class: Class,
}

struct Shared {
    chans: Mutex<HashMap<u32, Chan>>,
    pending_open: Mutex<HashMap<u32, oneshot::Sender<Result<(), String>>>>,
    /// Frames for the writer, each tagged with its channel's scheduling class. The class
    /// travels with the frame (rather than being looked up when the writer dequeues it) so
    /// a channel's Data and Close keep their class after the channel leaves `chans`: a
    /// Close can never be filed under another class and overtake queued data.
    out: mpsc::UnboundedSender<(Frame, Class)>,
    next: AtomicU32,
    stats: Arc<Stats>,
    closed: Notify,
    writer_stop: Notify,
    /// Set once the link is gone; checked after every registration to close the race with
    /// the teardown sweep.
    link_down: AtomicBool,
    opts: MuxOpts,
    /// The peer's capabilities, once its `Hello` arrived.
    peer: Mutex<Option<PeerCaps>>,
    hello: Notify,
}

impl Shared {
    /// Queue a frame that is not channel data (written ahead of all scheduled data).
    fn send(&self, f: Frame) -> Result<(), mpsc::error::SendError<(Frame, Class)>> {
        self.out.send((f, Class::Control))
    }

    /// Compress data sent on a channel of `class`?
    fn compress(&self, class: Class) -> bool {
        self.opts.compress
            && class.compressible()
            && self.peer.lock().unwrap().as_ref().is_some_and(|p| p.zstd)
    }
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
        Mux::start_with(rd, wr, role, acceptor, MuxOpts::default())
    }

    /// [`Mux::start`] with options (compression).
    pub fn start_with<R, W>(
        rd: R,
        wr: W,
        role: &str,
        acceptor: Option<Acceptor>,
        opts: MuxOpts,
    ) -> Mux
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (out_tx, out_rx) = mpsc::unbounded_channel::<(Frame, Class)>();
        let shared = Arc::new(Shared {
            chans: Mutex::new(HashMap::new()),
            pending_open: Mutex::new(HashMap::new()),
            out: out_tx,
            next: AtomicU32::new(if role == "client" { 1 } else { 2 }),
            stats: Arc::new(Stats::default()),
            closed: Notify::new(),
            writer_stop: Notify::new(),
            link_down: AtomicBool::new(false),
            opts,
            peer: Mutex::new(None),
            hello: Notify::new(),
        });
        let mux = Mux {
            shared: shared.clone(),
            remote_version: Arc::new(Mutex::new(None)),
        };
        shared
            .stats
            .last_rx_ms
            .store(now_unix_ms(), Ordering::Relaxed);
        let _ = shared.send(Frame::Hello {
            proto: PROTO,
            version: vk_proto::VERSION.into(),
            role: role_with_caps(role, opts),
        });
        tokio::spawn(writer(wr, out_rx, shared.clone()));
        let m2 = mux.clone();
        tokio::spawn(async move {
            let _ = reader(rd, m2.clone(), acceptor).await;
            link_down(&m2.shared);
        });
        // Keepalive + RTT (06 A4: ping every 5 s, the first one right away so the RTT that
        // sets the render frame cap is known before the first attach).
        let m3 = mux.clone();
        tokio::spawn(async move {
            let t0 = Instant::now();
            let mut first = true;
            loop {
                if !first {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                first = false;
                if m3.shared.link_down.load(Ordering::SeqCst) {
                    break;
                }
                if m3
                    .shared
                    .send(Frame::Ping {
                        ts: t0.elapsed().as_micros() as u64,
                    })
                    .is_err()
                {
                    break;
                }
                // Keep the oldest unanswered ping: a link that stops answering ages it.
                let _ = m3.shared.stats.ping_outstanding_ms.compare_exchange(
                    0,
                    now_unix_ms(),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
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

    /// Tear the link down from this side (`machine disconnect`): every channel ends.
    pub fn shutdown(&self) {
        link_down(&self.shared);
    }

    /// The peer's capabilities (`None` until its `Hello` arrived).
    pub fn peer_caps(&self) -> Option<PeerCaps> {
        self.shared.peer.lock().unwrap().clone()
    }

    /// Wait (bounded) for the peer's `Hello`.
    async fn wait_hello(&self) -> Option<PeerCaps> {
        let deadline = tokio::time::Instant::now() + HELLO_WAIT;
        loop {
            let notified = self.shared.hello.notified();
            if let Some(p) = self.peer_caps() {
                return Some(p);
            }
            if self.is_closed() {
                return None;
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep_until(deadline) => return self.peer_caps(),
            }
        }
    }

    /// Open a channel of `kind` on the remote side; returns a local byte stream. The class
    /// follows the kind (`tcp:`/`egress:` forwards, else control); see [`Mux::open_class`].
    pub async fn open(&self, kind: &str) -> Result<DuplexStream> {
        self.open_class(kind, Class::for_kind(kind)).await
    }

    /// Open a channel with an explicit scheduling class (a render stream or a bulk upload
    /// over a `socket` channel). The class goes to peers that understand it, so both
    /// directions of the channel are scheduled by it.
    pub async fn open_class(&self, kind: &str, class: Class) -> Result<DuplexStream> {
        if self.is_closed() {
            return Err(anyhow!("link closed"));
        }
        let wire_kind = match self.wait_hello().await {
            Some(p) if p.prio && class != Class::for_kind(kind) => {
                format!("{kind}#{}", class.name())
            }
            _ => kind.to_string(),
        };
        let ch = self.shared.next.fetch_add(2, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.shared.pending_open.lock().unwrap().insert(ch, tx);
        if self.shared.link_down.load(Ordering::SeqCst) {
            self.shared.pending_open.lock().unwrap().remove(&ch);
            return Err(anyhow!("link closed"));
        }
        let (local, remote_end) = tokio::io::duplex(WINDOW as usize);
        attach(&self.shared, ch, remote_end, class);
        self.shared
            .send(Frame::Open {
                ch,
                kind: wire_kind,
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
    shared.hello.notify_waiters();
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
            let _ = shared.out.send((Frame::Close { ch }, c.class));
        }
    }
}

/// Wire a duplex end into channel `ch`: bytes the local user writes go out as Data frames
/// (respecting the peer's credit window); incoming Data is written into it.
fn attach(shared: &Arc<Shared>, ch: u32, end: DuplexStream, class: Class) {
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
            class,
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
            let _ = out2.send((
                Frame::Window {
                    ch,
                    bytes: n as u32,
                },
                Class::Control,
            ));
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
                .send((
                    Frame::Data {
                        ch,
                        bytes: buf[..n].to_vec(),
                    },
                    class,
                ))
                .is_err()
            {
                break;
            }
        }
        // Close goes through the same per-channel queue as Data, so EOF follows every byte.
        if !st.sent_close.swap(true, Ordering::SeqCst) {
            let _ = out.send((Frame::Close { ch }, class));
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

/// One channel's queued Data/Close frames.
struct ChanQueue {
    ch: u32,
    compress: bool,
    frames: VecDeque<Frame>,
}

/// Weighted priority scheduler over channel queues (06 A4). Within a class, channels are
/// served round-robin one frame at a time; across classes, the highest class with data and
/// quantum left goes next, and quanta ([`Class::weight`]) refill once every class with data
/// has used its share. So control frames overtake everything, while a bulk class still gets
/// 1 frame in 31 under full contention and the whole link when alone.
#[derive(Default)]
pub(crate) struct Scheduler {
    classes: [VecDeque<ChanQueue>; 5],
    quantum: [u32; 5],
}

impl Scheduler {
    pub(crate) fn is_empty(&self) -> bool {
        self.classes.iter().all(VecDeque::is_empty)
    }

    fn push(&mut self, f: Frame, ch: u32, class: Class, compress: bool) {
        let q = &mut self.classes[class as usize];
        match q.iter_mut().find(|c| c.ch == ch) {
            Some(c) => c.frames.push_back(f),
            None => q.push_back(ChanQueue {
                ch,
                compress,
                frames: VecDeque::from([f]),
            }),
        }
    }

    /// The next frame to write and whether to compress it.
    fn next(&mut self) -> Option<(Frame, bool)> {
        for _ in 0..2 {
            for c in Class::ALL {
                let i = c as usize;
                if self.classes[i].is_empty() || self.quantum[i] == 0 {
                    continue;
                }
                self.quantum[i] -= 1;
                let mut cq = self.classes[i].pop_front()?;
                let f = cq.frames.pop_front();
                let compress = cq.compress;
                if !cq.frames.is_empty() {
                    self.classes[i].push_back(cq);
                }
                if let Some(f) = f {
                    return Some((f, compress));
                }
            }
            for c in Class::ALL {
                self.quantum[c as usize] = c.weight();
            }
        }
        None
    }
}

/// zstd with the shared dictionary, one context per writer/reader.
struct Codec {
    comp: Option<zstd::bulk::Compressor<'static>>,
    decomp: Option<zstd::bulk::Decompressor<'static>>,
}

impl Codec {
    fn new() -> Codec {
        Codec {
            comp: None,
            decomp: None,
        }
    }

    /// Compressed bytes when that saves at least 1/8; `None` otherwise.
    fn compress(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        if data.len() < MIN_COMPRESS {
            return None;
        }
        if self.comp.is_none() {
            self.comp = zstd::bulk::Compressor::with_dictionary(3, crate::dict::bytes()).ok();
        }
        let z = self.comp.as_mut()?.compress(data).ok()?;
        (z.len() < data.len() - data.len() / 8).then_some(z)
    }

    /// Decompress at most one chunk (anything larger is a protocol violation).
    fn decompress(&mut self, z: &[u8]) -> Option<Vec<u8>> {
        if self.decomp.is_none() {
            self.decomp = zstd::bulk::Decompressor::with_dictionary(crate::dict::bytes()).ok();
        }
        self.decomp.as_mut()?.decompress(z, CHUNK).ok()
    }
}

async fn writer<W: AsyncWrite + Unpin>(
    wr: W,
    mut rx: mpsc::UnboundedReceiver<(Frame, Class)>,
    shared: Arc<Shared>,
) {
    let stats = shared.stats.clone();
    // About one data frame: a larger buffer would hold bulk bytes ahead of a keystroke that
    // the scheduler already put first.
    let mut wr = tokio::io::BufWriter::with_capacity(CHUNK + 64, wr);
    let mut sched = Scheduler::default();
    let mut codec = Codec::new();
    loop {
        // Nothing queued: flush and block for the next frame.
        let first = if sched.is_empty() {
            if wr.flush().await.is_err() {
                return;
            }
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
        // Take everything that arrived since the last frame, so a keystroke queued during a
        // bulk transfer is scheduled ahead of the bulk's next chunk. Close is queued behind
        // its channel's Data so it can never overtake it.
        let mut incoming: Vec<(Frame, Class)> = first.into_iter().collect();
        while let Ok(f) = rx.try_recv() {
            incoming.push(f);
        }
        for (f, class) in incoming {
            match &f {
                Frame::Data { ch, .. } | Frame::Close { ch } => {
                    let ch = *ch;
                    let compress = shared.compress(class);
                    sched.push(f, ch, class, compress);
                }
                _ => {
                    if write_one(&mut wr, &f, &stats).await.is_err() {
                        return;
                    }
                }
            }
        }
        // One data frame, then look for newly queued frames again.
        if let Some((f, compress)) = sched.next() {
            let f = match f {
                Frame::Data { ch, bytes } => {
                    stats
                        .payload_out
                        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    match compress.then(|| codec.compress(&bytes)).flatten() {
                        Some(z) => {
                            stats.zstd_frames_out.fetch_add(1, Ordering::Relaxed);
                            Frame::DataZ { ch, bytes: z }
                        }
                        None => Frame::Data { ch, bytes },
                    }
                }
                f => f,
            };
            if write_one(&mut wr, &f, &stats).await.is_err() {
                return;
            }
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

/// Incoming channel data (already decompressed).
fn on_data(mux: &Mux, ch: u32, bytes: Vec<u8>) {
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

async fn reader<R: AsyncRead + Unpin>(rd: R, mux: Mux, acceptor: Option<Acceptor>) -> Result<()> {
    let mut rd = tokio::io::BufReader::with_capacity(64 * 1024, rd);
    let t0 = Instant::now();
    let mut codec = Codec::new();
    loop {
        let body = asyncio::read_body(&mut rd).await?;
        mux.shared
            .stats
            .bytes_in
            .fetch_add(body.len() as u64 + 4, Ordering::Relaxed);
        mux.shared
            .stats
            .last_rx_ms
            .store(now_unix_ms(), Ordering::Relaxed);
        let f: Frame = vk_proto::frame::decode(&body)?;
        match f {
            Frame::Hello {
                version,
                proto,
                role,
            } => {
                if proto != PROTO {
                    return Err(anyhow!("bridge protocol {proto} != {PROTO}"));
                }
                *mux.remote_version.lock().unwrap() = Some(version);
                *mux.shared.peer.lock().unwrap() = Some(parse_caps(&role));
                mux.shared.hello.notify_waiters();
            }
            Frame::Open { ch, kind } => {
                let Some(acc) = acceptor.clone() else {
                    let _ = mux.shared.send(Frame::OpenErr {
                        ch,
                        msg: "not accepting channels".into(),
                    });
                    continue;
                };
                let (kind, class) = split_class(&kind);
                let m = mux.clone();
                tokio::spawn(async move {
                    match acc(kind).await {
                        Ok(stream) => {
                            let (a, b) = tokio::io::duplex(WINDOW as usize);
                            attach(&m.shared, ch, b, class);
                            let _ = m.shared.send(Frame::OpenOk { ch });
                            let mut a = a;
                            let mut stream = stream;
                            let _ = tokio::io::copy_bidirectional(&mut a, &mut stream).await;
                        }
                        Err(e) => {
                            let _ = m.shared.send(Frame::OpenErr {
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
            Frame::Data { ch, bytes } => on_data(&mux, ch, bytes),
            Frame::DataZ { ch, bytes } => match codec.decompress(&bytes) {
                Some(raw) => on_data(&mux, ch, raw),
                None => abort_channel(&mux.shared, ch, "undecodable compressed frame"),
            },
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
                let _ = mux.shared.send(Frame::Pong { ts });
            }
            Frame::Pong { ts } => {
                let now = t0.elapsed().as_micros() as u64;
                mux.shared
                    .stats
                    .rtt_us
                    .store(now.saturating_sub(ts), Ordering::Relaxed);
                mux.shared
                    .stats
                    .ping_outstanding_ms
                    .store(0, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
#[path = "mux_tests.rs"]
mod tests;
