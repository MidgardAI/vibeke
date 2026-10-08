//! The holder event loop (01 §1.2, 07 §4). Single-threaded, `polling`-based, no async runtime.

use crate::ring::{Item, Ring};
use crate::scan::{Scanner, Seq, params};
use crate::{procinfo, pty};
use anyhow::{Context, Result};
use hmac::{Hmac, Mac};
use polling::{Event, Events, PollMode, Poller};
use rustix::fd::AsFd;
// Only `getpeereid` (macOS/BSD) takes a raw fd; Linux reads SO_PEERCRED through rustix.
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
use rustix::fd::AsRawFd;
use sha2::Sha256;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use vk_proto::frame::{FrameBuf, encode};
use vk_proto::holder::*;

const KEY_LISTENER: usize = 0;
/// PTY master, or the child's stdout in pipe mode.
const KEY_MASTER: usize = 1;
const KEY_SIGNAL: usize = 2;
const KEY_USR1: usize = 3;
/// Pipe mode: the child's stderr (readable) and stdin (writable while input is queued).
const KEY_STDERR: usize = 4;
const KEY_STDIN: usize = 5;
const KEY_CONN_BASE: usize = 16;
/// A server that stops reading is dropped once this much output is queued for it; it will
/// re-attach from its last processed offset.
const MAX_WBUF: usize = 32 * 1024 * 1024;
const QUEUED_QUERY_MAX_AGE: Duration = Duration::from_secs(5);
/// After the foreground process group changes, its leader's argv is re-read on output for this
/// long: the group often changes between `fork` and `exec` (the shell hands the terminal over
/// first), and a status taken then shows the shell, or an empty argv mid-`exec` (Linux
/// `/proc/<pid>/cmdline`), with no later group change to correct it.
const FG_SETTLE: Duration = Duration::from_secs(2);
/// Connections that have not acquired the lease yet: at most this many at once, each closed
/// after [`PRE_AUTH_IDLE`] (a server says hello and acquires within milliseconds).
const MAX_PRE_AUTH: usize = 8;
const PRE_AUTH_IDLE: Duration = Duration::from_secs(5);
/// After `accept` fails (EMFILE and the like), the listener rests this long instead of
/// spinning on a level-triggered readable socket.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

struct Conn {
    stream: UnixStream,
    rbuf: FrameBuf,
    wbuf: Vec<u8>,
    nonce: Vec<u8>,
    acquired: bool,
    attached: bool,
    want_write: bool,
    /// When the connection was accepted (pre-auth idle timeout).
    opened: Instant,
}

/// Bytes waiting to be written to the PTY master. Inputs from the server carry their id and
/// are acknowledged only once their last byte was written (01 §1.2, 07 §4); holder-generated
/// query replies carry none.
struct PendingWrite {
    bytes: Vec<u8>,
    pos: usize,
    input_id: Option<u64>,
}

struct Modes {
    bracketed_paste: bool,
    focus: bool,
    sync: bool,
    kitty: Vec<u32>,
}

pub struct Holder {
    spec: SpawnSpec,
    mode: Mode,
    poller: Poller,
    listener: UnixListener,
    /// PTY mode: the master (read and write). Pipe mode: the child's stdout (read only).
    master: Option<rustix::fd::OwnedFd>,
    /// Pipe mode only: the child's stdin (written) and stderr (read).
    stdin: Option<rustix::fd::OwnedFd>,
    stderr: Option<rustix::fd::OwnedFd>,
    /// Pipe mode: stream of the most recent ring bytes (a `Stream` marker on every switch).
    cur_stream: Option<Stream>,
    master_queue: VecDeque<PendingWrite>,
    child: std::process::Child,
    child_pid: u32,
    sig_rx: UnixStream,
    usr1_rx: UnixStream,
    ring: Ring,
    scanner: Scanner,
    conns: HashMap<usize, Conn>,
    next_key: usize,
    epoch: u64,
    seen_inputs: HashSet<u64>,
    input_order: VecDeque<u64>,
    exit: Option<(Option<i32>, Option<i32>)>,
    last_checkpoint: Option<u64>,
    checkpoint_requested_at: Option<u64>,
    queued: Vec<(Instant, u64, Vec<u8>)>,
    modes: Modes,
    last_fg: Option<u32>,
    /// Until when, and with which argv, the new foreground leader is re-checked
    /// ([`FG_SETTLE`]).
    fg_settle: Option<(Instant, Vec<String>)>,
    started_at_ms: i64,
    should_exit: bool,
    /// The listener is paused after an `accept` failure until then ([`ACCEPT_BACKOFF`]).
    accept_paused_until: Option<Instant>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn hmac(key: &[u8], nonce: &[u8], epoch: u64) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac key");
    mac.update(&acquire_message(nonce, epoch));
    mac.finalize().into_bytes().to_vec()
}

fn peer_uid_ok(s: &UnixStream) -> bool {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    #[cfg(any(target_os = "macos", target_os = "freebsd"))]
    // SAFETY: getpeereid writes to the two out-params.
    let ok = unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) } == 0;
    #[cfg(target_os = "linux")]
    let ok = {
        let _ = &mut gid;
        match rustix::net::sockopt::socket_peercred(s) {
            Ok(c) => {
                uid = c.uid.as_raw();
                true
            }
            Err(_) => false,
        }
    };
    // SAFETY: getuid has no preconditions.
    ok && uid == unsafe { libc::getuid() }
}

/// Bind the holder socket, refusing if another live holder already listens there.
pub fn bind_socket(path: &Path) -> Result<UnixListener> {
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            anyhow::bail!("a holder is already listening on {}", path.display());
        }
        std::fs::remove_file(path).ok();
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let l = UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
    rustix::fs::chmod(path, rustix::fs::Mode::from_raw_mode(0o600))?;
    l.set_nonblocking(true)?;
    Ok(l)
}

impl Holder {
    pub fn new(spec: SpawnSpec, listener: UnixListener) -> Result<Self> {
        let mode = spec.mode;
        // Linux: become the child subreaper, so a descendant that daemonizes (double fork,
        // `setsid`) reparents to this holder instead of init and stays attributable to the
        // pane (the server's ancestry walk counts the holder as the pane). Adopted orphans
        // are reaped in `on_sigchld`.
        become_subreaper();
        let (master, stdin, stderr, child) = match mode {
            Mode::Pty => {
                let (pty, child) = pty::spawn(
                    &spec.argv,
                    Path::new(&spec.cwd),
                    &spec.env,
                    spec.cols,
                    spec.rows,
                )?;
                (pty.master, None, None, child)
            }
            Mode::Pipe => {
                let (p, child) = pty::spawn_pipe(&spec.argv, Path::new(&spec.cwd), &spec.env)?;
                (p.stdout, Some(p.stdin), Some(p.stderr), child)
            }
        };
        let child_pid = child.id();
        let (sig_rx, sig_tx) = UnixStream::pair()?;
        sig_rx.set_nonblocking(true)?;
        sig_tx.set_nonblocking(true)?;
        signal_hook::low_level::pipe::register(signal_hook::consts::SIGCHLD, sig_tx)?;
        // Chaos/diagnostic hook: SIGUSR1 drops every server connection (the holder and child
        // keep running), simulating a connection loss without a process loss (10 §5).
        let (usr1_rx, usr1_tx) = UnixStream::pair()?;
        usr1_rx.set_nonblocking(true)?;
        usr1_tx.set_nonblocking(true)?;
        signal_hook::low_level::pipe::register(signal_hook::consts::SIGUSR1, usr1_tx)?;
        // SAFETY: ignoring SIGPIPE so writes to a dead server return EPIPE instead of killing us.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_IGN);
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
        }
        let poller = Poller::new()?;
        // SAFETY: the registered fds outlive their registration (deleted before drop).
        unsafe {
            poller.add_with_mode(&listener, Event::readable(KEY_LISTENER), PollMode::Level)?;
            poller.add_with_mode(&master, Event::readable(KEY_MASTER), PollMode::Level)?;
            poller.add_with_mode(&sig_rx, Event::readable(KEY_SIGNAL), PollMode::Level)?;
            poller.add_with_mode(&usr1_rx, Event::readable(KEY_USR1), PollMode::Level)?;
            if let Some(e) = &stderr {
                poller.add_with_mode(e, Event::readable(KEY_STDERR), PollMode::Level)?;
            }
            if let Some(i) = &stdin {
                poller.add_with_mode(i, Event::none(KEY_STDIN), PollMode::Level)?;
            }
        }
        let mut ring = Ring::new(spec.ring_bytes as usize);
        if mode == Mode::Pty {
            ring.marker(MarkerKind::Resize {
                cols: spec.cols,
                rows: spec.rows,
                px_w: 0,
                px_h: 0,
            });
        }
        ring.mark_cut(0);
        Ok(Holder {
            spec,
            mode,
            poller,
            listener,
            master: Some(master),
            stdin,
            stderr,
            cur_stream: None,
            master_queue: VecDeque::new(),
            child,
            child_pid,
            sig_rx,
            usr1_rx,
            ring,
            scanner: Scanner::default(),
            conns: HashMap::new(),
            next_key: KEY_CONN_BASE,
            epoch: 0,
            seen_inputs: HashSet::new(),
            input_order: VecDeque::new(),
            exit: None,
            last_checkpoint: None,
            checkpoint_requested_at: None,
            queued: Vec::new(),
            modes: Modes {
                bracketed_paste: false,
                focus: false,
                sync: false,
                kitty: vec![],
            },
            last_fg: None,
            fg_settle: None,
            started_at_ms: now_ms(),
            should_exit: false,
            accept_paused_until: None,
        })
    }

    pub fn child_pid(&self) -> u32 {
        self.child_pid
    }

    pub fn run(mut self) -> Result<()> {
        let mut events = Events::new();
        while !self.should_exit {
            events.clear();
            let timeout = self.next_deadline();
            match self.poller.wait(&mut events, timeout) {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
            let keys: Vec<(usize, bool, bool)> = events
                .iter()
                .map(|e| (e.key, e.readable, e.writable))
                .collect();
            for (key, r, w) in keys {
                match key {
                    KEY_LISTENER => self.accept(),
                    KEY_MASTER => {
                        if r {
                            self.read_master()
                        }
                        if w {
                            self.flush_master()
                        }
                    }
                    KEY_SIGNAL => self.on_sigchld(),
                    KEY_USR1 => self.on_usr1(),
                    KEY_STDERR => self.read_stderr(),
                    KEY_STDIN => self.flush_master(),
                    k => {
                        if w {
                            self.flush_conn(k);
                        }
                        if r {
                            self.read_conn(k);
                        }
                    }
                }
            }
            self.sweep_timers();
        }
        // Drain queued writes to the acquiring server before exiting.
        for c in self.conns.values_mut() {
            let _ = c.stream.set_nonblocking(false);
            let _ = c.stream.write_all(&c.wbuf);
        }
        Ok(())
    }

    /// How long the poll may sleep: until the next pre-auth timeout or listener re-arm.
    fn next_deadline(&self) -> Option<Duration> {
        let now = Instant::now();
        let pre_auth = self
            .conns
            .values()
            .filter(|c| !c.acquired)
            .map(|c| c.opened + PRE_AUTH_IDLE)
            .min();
        [pre_auth, self.accept_paused_until]
            .into_iter()
            .flatten()
            .min()
            .map(|t| t.saturating_duration_since(now) + Duration::from_millis(1))
    }

    /// Close pre-auth connections idle past [`PRE_AUTH_IDLE`]; re-arm a paused listener.
    fn sweep_timers(&mut self) {
        let now = Instant::now();
        let stale: Vec<usize> = self
            .conns
            .iter()
            .filter(|(_, c)| !c.acquired && now.duration_since(c.opened) >= PRE_AUTH_IDLE)
            .map(|(k, _)| *k)
            .collect();
        for k in stale {
            self.drop_conn(k);
        }
        if self.accept_paused_until.is_some_and(|t| now >= t) {
            self.accept_paused_until = None;
            let _ = self.poller.modify_with_mode(
                &self.listener,
                Event::readable(KEY_LISTENER),
                PollMode::Level,
            );
        }
    }

    fn accept(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((s, _)) => {
                    if !peer_uid_ok(&s) || s.set_nonblocking(true).is_err() {
                        continue;
                    }
                    if self.conns.values().filter(|c| !c.acquired).count() >= MAX_PRE_AUTH {
                        // Too many unauthenticated peers: refuse this one (closed on drop).
                        continue;
                    }
                    let key = self.next_key;
                    self.next_key += 1;
                    // SAFETY: stream stays owned by `conns` until deleted from the poller.
                    if unsafe {
                        self.poller
                            .add_with_mode(&s, Event::readable(key), PollMode::Level)
                    }
                    .is_err()
                    {
                        continue;
                    }
                    self.conns.insert(
                        key,
                        Conn {
                            stream: s,
                            rbuf: FrameBuf::default(),
                            wbuf: Vec::new(),
                            nonce: Vec::new(),
                            acquired: false,
                            attached: false,
                            want_write: false,
                            opened: Instant::now(),
                        },
                    );
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => {
                    // EMFILE/ENFILE and the like: keep serving the connections we have and
                    // retry accepting shortly, instead of spinning on the readable listener.
                    let _ = self.poller.modify_with_mode(
                        &self.listener,
                        Event::none(KEY_LISTENER),
                        PollMode::Level,
                    );
                    self.accept_paused_until = Some(Instant::now() + ACCEPT_BACKOFF);
                    break;
                }
            }
        }
    }

    fn on_usr1(&mut self) {
        let mut buf = [0u8; 64];
        while matches!(self.usr1_rx.read(&mut buf), Ok(n) if n > 0) {}
        let keys: Vec<usize> = self.conns.keys().copied().collect();
        for k in keys {
            self.drop_conn(k);
        }
    }

    fn drop_conn(&mut self, key: usize) {
        if let Some(c) = self.conns.remove(&key) {
            let _ = self.poller.delete(&c.stream);
            if c.attached {
                self.ring.marker(MarkerKind::ServerDetached);
            }
        }
    }

    fn send(&mut self, key: usize, msg: &FromHolder) {
        let Ok(bytes) = encode(msg) else { return };
        let Some(c) = self.conns.get_mut(&key) else {
            return;
        };
        c.wbuf.extend_from_slice(&bytes);
        if c.wbuf.len() > MAX_WBUF {
            self.drop_conn(key);
            return;
        }
        self.flush_conn(key);
    }

    /// The connection holding the lease (at most one: `Acquire` fences the others).
    fn acquired_key(&self) -> Option<usize> {
        self.conns.iter().find(|(_, c)| c.acquired).map(|(k, _)| *k)
    }

    fn attached_key(&self) -> Option<usize> {
        self.conns.iter().find(|(_, c)| c.attached).map(|(k, _)| *k)
    }

    fn flush_conn(&mut self, key: usize) {
        let Some(c) = self.conns.get_mut(&key) else {
            return;
        };
        let mut dead = false;
        while !c.wbuf.is_empty() {
            match c.stream.write(&c.wbuf) {
                Ok(0) => {
                    dead = true;
                    break;
                }
                Ok(n) => {
                    c.wbuf.drain(..n);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => {
                    dead = true;
                    break;
                }
            }
        }
        if dead {
            self.drop_conn(key);
            return;
        }
        let want = !c.wbuf.is_empty();
        if want != c.want_write {
            c.want_write = want;
            let ev = if want {
                Event::all(key)
            } else {
                Event::readable(key)
            };
            let _ = self.poller.modify_with_mode(&c.stream, ev, PollMode::Level);
        }
    }

    fn read_conn(&mut self, key: usize) {
        let mut buf = [0u8; 65536];
        let mut eof = false;
        if let Some(c) = self.conns.get_mut(&key) {
            loop {
                match c.stream.read(&mut buf) {
                    Ok(0) => {
                        eof = true;
                        break;
                    }
                    Ok(n) => c.rbuf.push(&buf[..n]),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => {
                        eof = true;
                        break;
                    }
                }
            }
        }
        loop {
            let msg = match self
                .conns
                .get_mut(&key)
                .map(|c| c.rbuf.next_frame::<ToHolder>())
            {
                Some(Ok(Some(m))) => m,
                Some(Ok(None)) | None => break,
                Some(Err(_)) => {
                    eof = true;
                    break;
                }
            };
            self.handle(key, msg);
        }
        if eof {
            self.drop_conn(key);
        }
    }

    fn handle(&mut self, key: usize, msg: ToHolder) {
        let acquired = self.conns.get(&key).is_some_and(|c| c.acquired);
        let epoch_ok = |e: u64, this: &Self| acquired && e == this.epoch;
        match msg {
            ToHolder::Hello { proto_max, .. } => {
                let min = if self.mode == Mode::Pipe {
                    PROTO_PIPE
                } else {
                    PROTO_MIN
                };
                if proto_max < min {
                    self.send(
                        key,
                        &FromHolder::Rejected {
                            reason: format!(
                                "holder speaks holder/{min}..{PROTO} ({:?} mode)",
                                self.mode
                            ),
                        },
                    );
                    return;
                }
                let nonce: Vec<u8> = (0..32).map(|_| rand::random::<u8>()).collect();
                if let Some(c) = self.conns.get_mut(&key) {
                    c.nonce = nonce.clone();
                }
                let msg = FromHolder::HelloOk {
                    // The highest version both sides speak (an older server gets its own).
                    proto: PROTO.min(proto_max),
                    holder_version: vk_proto::VERSION.to_string(),
                    pane_id: self.spec.pane_id.clone(),
                    mode: self.mode,
                    child_pid: self.child_pid,
                    started_at_ms: self.started_at_ms,
                    ring: RingInfo {
                        start_offset: self.ring.start(),
                        end_offset: self.ring.end(),
                        capacity: self.ring.capacity() as u64,
                    },
                    last_checkpoint: self.last_checkpoint,
                    nonce,
                    epoch: self.epoch,
                };
                self.send(key, &msg);
            }
            ToHolder::Acquire {
                epoch, hmac: mac, ..
            } => {
                let nonce = self
                    .conns
                    .get(&key)
                    .map(|c| c.nonce.clone())
                    .unwrap_or_default();
                let expected = hmac(&self.spec.key, &nonce, epoch);
                if nonce.is_empty() || mac != expected {
                    self.send(
                        key,
                        &FromHolder::Rejected {
                            reason: "bad hmac".into(),
                        },
                    );
                    return;
                }
                if epoch <= self.epoch {
                    self.send(
                        key,
                        &FromHolder::Rejected {
                            reason: format!("stale epoch {epoch} <= {}", self.epoch),
                        },
                    );
                    return;
                }
                // Fencing: the previous lease holder is cut off.
                let others: Vec<usize> = self
                    .conns
                    .iter()
                    .filter(|(k, c)| **k != key && c.acquired)
                    .map(|(k, _)| *k)
                    .collect();
                for k in others {
                    self.drop_conn(k);
                }
                self.epoch = epoch;
                if let Some(c) = self.conns.get_mut(&key) {
                    c.acquired = true;
                }
                self.send(key, &FromHolder::Acquired { epoch });
            }
            ToHolder::Attach { epoch, from_offset } => {
                if !epoch_ok(epoch, self) {
                    return;
                }
                self.attach(key, from_offset);
            }
            ToHolder::Input {
                epoch,
                input_id,
                bytes,
            } => {
                if !epoch_ok(epoch, self) {
                    return;
                }
                let status = if self.seen_inputs.contains(&input_id) {
                    if self
                        .master_queue
                        .iter()
                        .any(|w| w.input_id == Some(input_id))
                    {
                        // Still being written: the ack follows when the original completes.
                        return;
                    }
                    InputStatus::Duplicate
                } else if self.exit.is_some() || !self.writable() {
                    InputStatus::ChildExited
                } else {
                    self.remember_input(input_id);
                    self.master_queue.push_back(PendingWrite {
                        bytes,
                        pos: 0,
                        input_id: Some(input_id),
                    });
                    // Acks (Written or Failed) are sent from `flush_master`.
                    self.flush_master();
                    return;
                };
                let at = self.ring.end();
                self.send(
                    key,
                    &FromHolder::InputAck {
                        input_id,
                        offset_at_write: at,
                        status,
                    },
                );
            }
            ToHolder::Resize {
                epoch,
                cols,
                rows,
                px_w,
                px_h,
            } => {
                if !epoch_ok(epoch, self) || self.mode == Mode::Pipe {
                    // Pipe mode has no terminal size.
                    return;
                }
                if let Some(m) = &self.master {
                    let _ = pty::set_size(m, cols, rows, px_w, px_h);
                }
                let kind = MarkerKind::Resize {
                    cols,
                    rows,
                    px_w,
                    px_h,
                };
                let offset = self.ring.marker(kind.clone());
                if let Some(k) = self.attached_key() {
                    self.send(k, &FromHolder::Marker { offset, kind });
                }
            }
            ToHolder::Signal { epoch, sig, target } => {
                if !epoch_ok(epoch, self) {
                    return;
                }
                let signo = match sig {
                    Sig::Int => libc::SIGINT,
                    Sig::Term => libc::SIGTERM,
                    Sig::Hup => libc::SIGHUP,
                    Sig::Kill => libc::SIGKILL,
                    Sig::Winch => libc::SIGWINCH,
                    Sig::Cont => libc::SIGCONT,
                    Sig::Stop => libc::SIGSTOP,
                };
                let pid = match target {
                    SigTarget::Child => self.child_pid as i32,
                    SigTarget::FgPgrp => -(self.fg().unwrap_or(self.child_pid) as i32),
                };
                // SAFETY: plain kill(2).
                unsafe { libc::kill(pid, signo) };
            }
            ToHolder::StatusQuery => {
                // Process details (argv, cwd, pids) only for the authenticated lease holder.
                if !acquired {
                    self.send(
                        key,
                        &FromHolder::Rejected {
                            reason: "status needs an acquired lease".into(),
                        },
                    );
                    return;
                }
                let st = self.status();
                self.send(key, &FromHolder::Status(st));
            }
            ToHolder::AckExit { epoch } => {
                if epoch_ok(epoch, self) && self.exit.is_some() {
                    self.should_exit = true;
                }
            }
            ToHolder::Checkpoint { epoch, offset } => {
                if epoch_ok(epoch, self) {
                    self.last_checkpoint = Some(offset);
                    self.checkpoint_requested_at = None;
                }
            }
            ToHolder::Ping { nonce } => self.send(key, &FromHolder::Pong { nonce }),
            ToHolder::Pong { .. } => {}
        }
    }

    fn attach(&mut self, key: usize, from_offset: u64) {
        // Detach any other attachment of this lease holder.
        for c in self.conns.values_mut() {
            c.attached = false;
        }
        let mut frames = Vec::new();
        if from_offset < self.ring.start() {
            frames.push(FromHolder::Gap {
                requested: from_offset,
                available_from: self.ring.start(),
            });
        }
        // Pipe mode: bytes carry the stream of the last `Stream` marker before them.
        let mut stream = match self.mode {
            Mode::Pty => Stream::Pty,
            Mode::Pipe => self.ring.stream_at(from_offset).unwrap_or(Stream::Stdout),
        };
        for item in self.ring.read_from(from_offset) {
            match item {
                Item::Bytes(off, a, b) => {
                    let mut bytes = a.to_vec();
                    bytes.extend_from_slice(b);
                    for (i, chunk) in bytes.chunks(64 * 1024).enumerate() {
                        frames.push(FromHolder::Output {
                            offset: off + (i * 64 * 1024) as u64,
                            stream,
                            bytes: chunk.to_vec(),
                            replay: true,
                        });
                    }
                }
                Item::Marker(_, MarkerKind::Stream { stream: s }) => stream = *s,
                Item::Marker(off, kind) => frames.push(FromHolder::Marker {
                    offset: off,
                    kind: kind.clone(),
                }),
            }
        }
        let now = Instant::now();
        let queued_queries = self
            .queued
            .drain(..)
            .filter(|(at, _, _)| now.duration_since(*at) <= QUEUED_QUERY_MAX_AGE)
            .map(|(at, offset, bytes)| QueuedQuery {
                offset,
                age_ms: now.duration_since(at).as_millis() as u64,
                bytes,
            })
            .collect();
        frames.push(FromHolder::ReplayDone {
            offset: self.ring.end(),
            queued_queries,
        });
        for f in &frames {
            self.send(key, f);
        }
        let epoch = self.epoch;
        self.ring.marker(MarkerKind::ServerAttached { epoch });
        if let Some(c) = self.conns.get_mut(&key) {
            c.attached = true;
        }
        if let Some((code, sig)) = self.exit {
            self.send(
                key,
                &FromHolder::ChildExited {
                    exit_code: code,
                    signal: sig,
                },
            );
        }
    }

    fn remember_input(&mut self, input_id: u64) {
        self.seen_inputs.insert(input_id);
        self.input_order.push_back(input_id);
        if self.input_order.len() > INPUT_DEDUPE_WINDOW
            && let Some(old) = self.input_order.pop_front()
        {
            self.seen_inputs.remove(&old);
        }
    }

    /// Holder-originated bytes (query replies): written in order with inputs, never acked.
    fn write_master(&mut self, bytes: &[u8]) {
        self.master_queue.push_back(PendingWrite {
            bytes: bytes.to_vec(),
            pos: 0,
            input_id: None,
        });
        self.flush_master();
    }

    /// Ack an input to whichever server holds the lease now (the one that sent it may be
    /// gone; a successor resending the same id is waiting for exactly this ack).
    fn ack_input(&mut self, input_id: u64, status: InputStatus) {
        let at = self.ring.end();
        if status == InputStatus::Written {
            self.ring.marker(MarkerKind::InputWritten { input_id });
        } else {
            // Not (fully) written: a retry must not be reported as a duplicate.
            self.seen_inputs.remove(&input_id);
            self.input_order.retain(|i| *i != input_id);
        }
        if let Some(k) = self.acquired_key() {
            self.send(
                k,
                &FromHolder::InputAck {
                    input_id,
                    offset_at_write: at,
                    status,
                },
            );
        }
    }

    /// Write queued bytes until the PTY would block. Partial writes keep the remainder (and
    /// its ack) queued; the master is polled for writability until the queue drains. A
    /// write error (EIO: the slave side is gone) fails every queued input explicitly.
    fn flush_master(&mut self) {
        let mut done = Vec::new();
        let mut failed = false;
        // Pipe mode journals what reached the child's stdin (holder/2 `Stream::Stdin`).
        let mut written: Vec<u8> = Vec::new();
        let pipe = self.mode == Mode::Pipe;
        let fd = if pipe {
            self.stdin.as_ref()
        } else {
            self.master.as_ref()
        };
        if let Some(m) = fd {
            while let Some(w) = self.master_queue.front_mut() {
                if w.pos >= w.bytes.len() {
                    if let Some(id) = w.input_id {
                        done.push((id, written.len()));
                    }
                    self.master_queue.pop_front();
                    continue;
                }
                match rustix::io::write(m, &w.bytes[w.pos..]) {
                    Ok(n) => {
                        if pipe {
                            written.extend_from_slice(&w.bytes[w.pos..w.pos + n]);
                        }
                        w.pos += n
                    }
                    Err(rustix::io::Errno::AGAIN) => break,
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(_) => {
                        failed = true;
                        break;
                    }
                }
            }
            let _ = if pipe {
                let ev = if self.master_queue.is_empty() {
                    Event::none(KEY_STDIN)
                } else {
                    Event::writable(KEY_STDIN)
                };
                self.poller.modify_with_mode(m, ev, PollMode::Level)
            } else {
                let ev = if self.master_queue.is_empty() {
                    Event::readable(KEY_MASTER)
                } else {
                    Event::all(KEY_MASTER)
                };
                self.poller.modify_with_mode(m, ev, PollMode::Level)
            };
        } else {
            failed = true;
        }
        // Journal each input's bytes before its `InputWritten` marker, so a replay shows the
        // marker after the request it confirms.
        let mut from = 0;
        for (id, upto) in done {
            if upto > from {
                self.on_pipe_output(Stream::Stdin, &written[from..upto]);
                from = upto;
            }
            self.ack_input(id, InputStatus::Written);
        }
        if written.len() > from {
            self.on_pipe_output(Stream::Stdin, &written[from..]);
        }
        if failed {
            if pipe {
                // EPIPE: the child closed stdin (or exited); nothing more can be written.
                if let Some(i) = self.stdin.take() {
                    let _ = self.poller.delete(&i);
                }
            }
            self.fail_pending();
        }
    }

    /// Can input still be written to the child (PTY master or stdin pipe open)?
    fn writable(&self) -> bool {
        match self.mode {
            Mode::Pty => self.master.is_some(),
            Mode::Pipe => self.stdin.is_some(),
        }
    }

    /// The PTY can no longer be written: report every queued input as failed (never drop
    /// one silently).
    fn fail_pending(&mut self) {
        let ids: Vec<u64> = self
            .master_queue
            .drain(..)
            .filter_map(|w| w.input_id)
            .collect();
        for id in ids {
            self.ack_input(id, InputStatus::Failed);
        }
    }

    fn fg(&self) -> Option<u32> {
        match self.mode {
            Mode::Pty => self.master.as_ref().and_then(pty::fg_pgrp),
            // No terminal: the child leads its own session and process group.
            Mode::Pipe => self.exit.is_none().then_some(self.child_pid),
        }
    }

    fn status(&self) -> ProcStatus {
        let fg = self.fg();
        let info = fg.and_then(procinfo::info);
        ProcStatus {
            child_pid: self.child_pid,
            fg_pgid: fg,
            fg_cmdline: info.as_ref().map(|i| i.argv.clone()).unwrap_or_default(),
            fg_exe: info.as_ref().and_then(|i| i.exe.clone()),
            fg_cwd: info.and_then(|i| i.cwd),
            exited: self.exit.is_some(),
            exit_code: self.exit.and_then(|e| e.0),
            signal: self.exit.and_then(|e| e.1),
        }
    }

    fn read_master(&mut self) {
        let mut buf = vec![0u8; 65536];
        loop {
            let Some(m) = &self.master else { return };
            match rustix::io::read(m, &mut buf) {
                Ok(0) => {
                    self.close_master();
                    return;
                }
                Ok(n) if self.mode == Mode::Pipe => {
                    let chunk = buf[..n].to_vec();
                    self.on_pipe_output(Stream::Stdout, &chunk)
                }
                Ok(n) => self.on_output(&buf[..n]),
                Err(rustix::io::Errno::AGAIN) => break,
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => {
                    // EIO: every slave fd closed (child and its descendants gone).
                    self.close_master();
                    return;
                }
            }
        }
        if self.mode == Mode::Pipe {
            return;
        }
        let fg = self.fg();
        let changed = if fg != self.last_fg {
            self.last_fg = fg;
            self.fg_settle = fg.map(|p| (Instant::now() + FG_SETTLE, procinfo::argv(p)));
            true
        } else if let (Some(p), Some((until, seen))) = (fg, &mut self.fg_settle) {
            if Instant::now() > *until {
                self.fg_settle = None;
                false
            } else {
                let argv = procinfo::argv(p);
                let exec = argv != *seen;
                *seen = argv;
                exec
            }
        } else {
            false
        };
        if changed && let Some(k) = self.attached_key() {
            self.send(k, &FromHolder::FgChanged);
        }
    }

    fn close_master(&mut self) {
        if let Some(m) = self.master.take() {
            let _ = self.poller.delete(&m);
        }
        // Pipe mode: stdout closing doesn't close stdin; the child may still read.
        if self.mode == Mode::Pty {
            self.fail_pending();
        }
        self.on_sigchld();
    }

    /// Pipe mode: the child's stderr.
    fn read_stderr(&mut self) {
        let mut buf = vec![0u8; 65536];
        loop {
            let Some(e) = &self.stderr else { return };
            match rustix::io::read(e, &mut buf) {
                Ok(n) if n > 0 => {
                    let chunk = buf[..n].to_vec();
                    self.on_pipe_output(Stream::Stderr, &chunk)
                }
                Err(rustix::io::Errno::AGAIN) => break,
                Err(rustix::io::Errno::INTR) => continue,
                _ => {
                    if let Some(e) = self.stderr.take() {
                        let _ = self.poller.delete(&e);
                    }
                    return;
                }
            }
        }
    }

    /// Pipe mode journal (01 §1.2): raw protocol bytes per stream, a `Stream` marker on every
    /// switch, and a safe cut point after each complete line (JSONL frames), so a replay or a
    /// trimmed ring starts on a frame boundary.
    fn on_pipe_output(&mut self, stream: Stream, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if self.cur_stream != Some(stream) {
            self.ring.marker(MarkerKind::Stream { stream });
            self.cur_stream = Some(stream);
        }
        let start = self.ring.end();
        self.ring.push(bytes);
        if bytes.ends_with(b"\n") {
            self.ring.mark_cut(self.ring.end());
        }
        if let Some(k) = self.attached_key() {
            self.send(
                k,
                &FromHolder::Output {
                    offset: start,
                    stream,
                    bytes: bytes.to_vec(),
                    replay: false,
                },
            );
        }
    }

    fn on_output(&mut self, bytes: &[u8]) {
        let start = self.ring.end();
        let attached = self.attached_key();
        let mut seqs = Vec::new();
        let mut cuts = Vec::new();
        self.scanner
            .feed(bytes, |s| seqs.push(s), |i| cuts.push(start + i as u64));
        self.ring.push(bytes);
        for c in cuts {
            self.ring.mark_cut(c);
        }
        for s in seqs {
            self.track_modes(&s);
            if attached.is_none() {
                self.answer_query(&s, start);
            }
        }
        if let Some(k) = attached {
            self.send(
                k,
                &FromHolder::Output {
                    offset: start,
                    stream: Stream::Pty,
                    bytes: bytes.to_vec(),
                    replay: false,
                },
            );
            let since = self.ring.end() - self.last_checkpoint.unwrap_or(0).max(self.ring.start());
            if since as usize >= self.ring.capacity() / 2 && self.checkpoint_requested_at.is_none()
            {
                let offset = self.ring.end();
                self.checkpoint_requested_at = Some(offset);
                self.send(k, &FromHolder::CheckpointWanted { offset });
            }
        }
    }

    fn track_modes(&mut self, s: &Seq) {
        let Seq::Csi {
            private,
            params: p,
            inter,
            fin,
        } = s
        else {
            return;
        };
        if !inter.is_empty() {
            return;
        }
        match (private, fin) {
            (Some(b'?'), b'h' | b'l') => {
                let on = *fin == b'h';
                for n in params(p) {
                    match n {
                        2004 => self.modes.bracketed_paste = on,
                        1004 => self.modes.focus = on,
                        2026 => self.modes.sync = on,
                        _ => {}
                    }
                }
            }
            (Some(b'>'), b'u') => self
                .modes
                .kitty
                .push(params(p).first().copied().unwrap_or(0)),
            (Some(b'<'), b'u') => {
                let n = params(p).first().copied().unwrap_or(1).max(1) as usize;
                let len = self.modes.kitty.len();
                self.modes.kitty.truncate(len.saturating_sub(n));
            }
            (Some(b'='), b'u') => {
                let ps = params(p);
                let flags = ps.first().copied().unwrap_or(0);
                let mode = ps.get(1).copied().unwrap_or(1);
                let cur = self.modes.kitty.last().copied().unwrap_or(0);
                let new = match mode {
                    2 => cur | flags,
                    3 => cur & !flags,
                    _ => flags,
                };
                match self.modes.kitty.last_mut() {
                    Some(l) => *l = new,
                    None => self.modes.kitty.push(new),
                }
            }
            _ => {}
        }
    }

    /// Answer the screen-independent queries while no server is attached; queue the rest.
    fn answer_query(&mut self, s: &Seq, offset: u64) {
        let reply: Option<String> = match s {
            Seq::Csi {
                private,
                params: p,
                inter,
                fin,
            } => match (private, inter.as_slice(), fin) {
                (None, b"", b'c') if params(p).iter().all(|&x| x == 0) => {
                    Some(vk_proto::ident::DA1.into())
                }
                (Some(b'>'), b"", b'c') => Some(vk_proto::ident::DA2.into()),
                (Some(b'='), b"", b'c') => Some(vk_proto::ident::DA3.into()),
                (Some(b'>'), b"", b'q') => Some(vk_proto::ident::xtversion()),
                (None, b"", b'n') if params(p) == [5] => Some("\x1b[0n".into()),
                (Some(b'?'), b"", b'u') => Some(format!(
                    "\x1b[?{}u",
                    self.modes.kitty.last().copied().unwrap_or(0)
                )),
                (Some(b'?'), b"$", b'p') => {
                    let n = params(p).first().copied().unwrap_or(0);
                    let v = match n {
                        2004 => Some(self.modes.bracketed_paste),
                        1004 => Some(self.modes.focus),
                        2026 => Some(self.modes.sync),
                        _ => None,
                    };
                    match v {
                        Some(on) => Some(format!("\x1b[?{n};{}$y", if on { 1 } else { 2 })),
                        None => {
                            self.queue(offset, s);
                            None
                        }
                    }
                }
                (None, b"", b'n') | (Some(b'?'), b"", b'n') | (None, b"", b't') => {
                    self.queue(offset, s);
                    None
                }
                _ => None,
            },
            Seq::Osc(body) => {
                if body.ends_with(b";?") {
                    self.queue(offset, s);
                }
                None
            }
        };
        if let Some(r) = reply {
            self.write_master(r.as_bytes());
        }
    }

    fn queue(&mut self, offset: u64, s: &Seq) {
        let bytes = match s {
            Seq::Csi {
                private,
                params,
                inter,
                fin,
            } => {
                let mut b = b"\x1b[".to_vec();
                if let Some(p) = private {
                    b.push(*p);
                }
                b.extend_from_slice(params);
                b.extend_from_slice(inter);
                b.push(*fin);
                b
            }
            Seq::Osc(body) => [b"\x1b]".as_slice(), body, b"\x1b\\"].concat(),
        };
        if self.queued.len() < 64 {
            self.queued.push((Instant::now(), offset, bytes));
        }
    }

    fn on_sigchld(&mut self) {
        let mut buf = [0u8; 64];
        while matches!(self.sig_rx.read(&mut buf), Ok(n) if n > 0) {}
        self.on_child_status();
        reap_adopted(self.child_pid, self.exit.is_some());
    }

    fn on_child_status(&mut self) {
        if self.exit.is_some() {
            return;
        }
        if let Ok(Some(st)) = self.child.try_wait() {
            use std::os::unix::process::ExitStatusExt;
            self.exit = Some((st.code(), st.signal()));
            if self.mode == Mode::Pipe {
                // Final output still in the pipes (stop at EAGAIN: a grandchild may keep
                // them open). Nothing more can be written.
                self.read_master();
                self.read_stderr();
                if let Some(i) = self.stdin.take() {
                    let _ = self.poller.delete(&i);
                }
                self.fail_pending();
            } else if self.master.is_some() {
                // Drain any final output still in the PTY.
                let mut b = vec![0u8; 65536];
                while let Some(m) = &self.master {
                    match rustix::io::read(m, &mut b) {
                        Ok(n) if n > 0 => {
                            let chunk = b[..n].to_vec();
                            self.on_output(&chunk);
                        }
                        _ => break,
                    }
                }
            }
            if let Some(k) = self.attached_key() {
                let (code, sig) = self.exit.unwrap();
                self.send(
                    k,
                    &FromHolder::ChildExited {
                        exit_code: code,
                        signal: sig,
                    },
                );
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn become_subreaper() {
    // SAFETY: plain prctl(2) on the calling process.
    unsafe {
        libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1 as libc::c_ulong, 0, 0, 0);
    }
}

#[cfg(not(target_os = "linux"))]
fn become_subreaper() {}

/// Reap exited orphans this holder adopted as child subreaper (Linux), never the pane's own
/// child while its status is still unclaimed (`child_reaped == false`): that one belongs to
/// `Child::try_wait`. Each ready child is peeked with `WNOWAIT` first and reaped by pid only
/// when it is not the pane child.
#[cfg(target_os = "linux")]
pub(crate) fn reap_adopted(child_pid: u32, child_reaped: bool) {
    for _ in 0..1024 {
        // SAFETY: zeroed siginfo is a valid out-param for waitid.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waitid writes into `info`; WNOWAIT leaves the child waitable.
        let r = unsafe {
            libc::waitid(
                libc::P_ALL,
                0,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if r != 0 {
            return;
        }
        // SAFETY: waitid filled a SIGCHLD siginfo (si_pid 0 = nothing ready).
        let pid = unsafe { info.si_pid() };
        if pid <= 0 {
            return;
        }
        if pid as u32 == child_pid && !child_reaped {
            return;
        }
        let mut status = 0;
        // SAFETY: reaps exactly `pid`, which is a zombie child of this process.
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn reap_adopted(_child_pid: u32, _child_reaped: bool) {}

/// Read and delete the spawn spec file.
pub fn read_spec(path: &Path) -> Result<SpawnSpec> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let _ = std::fs::remove_file(path);
    Ok(vk_proto::frame::decode(&bytes)?)
}

pub fn write_spec(path: &Path, spec: &SpawnSpec) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let body = postcard::to_stdvec(spec)?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(&body)?;
    Ok(())
}

impl AsFd for Holder {
    fn as_fd(&self) -> rustix::fd::BorrowedFd<'_> {
        self.listener.as_fd()
    }
}
