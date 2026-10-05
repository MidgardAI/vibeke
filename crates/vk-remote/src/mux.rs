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
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
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

struct Chan {
    to_local: mpsc::UnboundedSender<Vec<u8>>,
    credit: Arc<(AtomicU32, Notify)>,
}

struct Shared {
    chans: Mutex<HashMap<u32, Chan>>,
    pending_open: Mutex<HashMap<u32, oneshot::Sender<Result<(), String>>>>,
    out: mpsc::UnboundedSender<Frame>,
    next: AtomicU32,
    stats: Arc<Stats>,
    closed: Notify,
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
        tokio::spawn(writer(wr, out_rx, shared.stats.clone()));
        let m2 = mux.clone();
        tokio::spawn(async move {
            let _ = reader(rd, m2.clone(), acceptor).await;
            // Link down: close every channel.
            let chans: Vec<u32> = m2.shared.chans.lock().unwrap().keys().copied().collect();
            for c in chans {
                m2.shared.chans.lock().unwrap().remove(&c);
            }
            for (_, tx) in m2.shared.pending_open.lock().unwrap().drain() {
                let _ = tx.send(Err("link closed".into()));
            }
            m2.shared.closed.notify_waiters();
        });
        // Keepalive + RTT (06 A4: ping every 5 s).
        let m3 = mux.clone();
        tokio::spawn(async move {
            let t0 = Instant::now();
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
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
        self.shared.out.is_closed()
    }

    pub async fn closed(&self) {
        if self.is_closed() {
            return;
        }
        self.shared.closed.notified().await
    }

    /// Open a channel of `kind` on the remote side; returns a local byte stream.
    pub async fn open(&self, kind: &str) -> Result<DuplexStream> {
        let ch = self.shared.next.fetch_add(2, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.shared.pending_open.lock().unwrap().insert(ch, tx);
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

/// Wire a duplex end into channel `ch`: bytes the local user writes go out as Data frames
/// (respecting the peer's credit window); incoming Data is written into it.
fn attach(shared: &Arc<Shared>, ch: u32, end: DuplexStream) {
    let (mut rd, mut wr) = tokio::io::split(end);
    let (to_local, mut from_link) = mpsc::unbounded_channel::<Vec<u8>>();
    let credit = Arc::new((AtomicU32::new(WINDOW), Notify::new()));
    shared.chans.lock().unwrap().insert(
        ch,
        Chan {
            to_local,
            credit: credit.clone(),
        },
    );
    let out = shared.out.clone();
    // link → local
    let out2 = out.clone();
    tokio::spawn(async move {
        while let Some(b) = from_link.recv().await {
            let n = b.len() as u32;
            if wr.write_all(&b).await.is_err() {
                break;
            }
            let _ = out2.send(Frame::Window { ch, bytes: n });
        }
        let _ = wr.shutdown().await;
    });
    // local → link
    let shared2 = shared.clone();
    tokio::spawn(async move {
        let mut buf = vec![0u8; CHUNK];
        loop {
            // Wait for credit.
            while credit.0.load(Ordering::Acquire) == 0 {
                credit.1.notified().await;
            }
            let allowed = credit.0.load(Ordering::Acquire).min(CHUNK as u32) as usize;
            let n = match rd.read(&mut buf[..allowed]).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            credit.0.fetch_sub(n as u32, Ordering::AcqRel);
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
        let _ = out.send(Frame::Close { ch });
        shared2.chans.lock().unwrap().remove(&ch);
    });
}

async fn writer<W: AsyncWrite + Unpin>(
    wr: W,
    mut rx: mpsc::UnboundedReceiver<Frame>,
    stats: Arc<Stats>,
) {
    let mut wr = tokio::io::BufWriter::with_capacity(64 * 1024, wr);
    // Round-robin over channels with queued data; control frames go first.
    let mut queues: VecDeque<(u32, VecDeque<Frame>)> = VecDeque::new();
    loop {
        let first = if queues.is_empty() {
            match rx.recv().await {
                Some(f) => Some(f),
                None => break,
            }
        } else {
            None
        };
        let mut incoming: Vec<Frame> = first.into_iter().collect();
        while let Ok(f) = rx.try_recv() {
            incoming.push(f);
        }
        for f in incoming {
            match &f {
                Frame::Data { ch, .. } => {
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
                if let Some(c) = mux.shared.chans.lock().unwrap().get(&ch) {
                    let _ = c.to_local.send(bytes);
                }
            }
            Frame::Window { ch, bytes } => {
                if let Some(c) = mux.shared.chans.lock().unwrap().get(&ch) {
                    c.credit.0.fetch_add(bytes, Ordering::AcqRel);
                    c.credit.1.notify_one();
                }
            }
            Frame::Close { ch } => {
                // Dropping the sender ends the link→local task, which shuts the write half.
                mux.shared.chans.lock().unwrap().remove(&ch);
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
}
