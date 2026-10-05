//! The holder event loop (01 §1.2, 07 §4). Single-threaded, `polling`-based, no async runtime.

use crate::ring::{Item, Ring};
use crate::scan::{Scanner, Seq, params};
use crate::{procinfo, pty};
use anyhow::{Context, Result};
use hmac::{Hmac, Mac};
use polling::{Event, Events, PollMode, Poller};
use rustix::fd::{AsFd, AsRawFd};
use sha2::Sha256;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use vk_proto::frame::{FrameBuf, encode};
use vk_proto::holder::*;

const KEY_LISTENER: usize = 0;
const KEY_MASTER: usize = 1;
const KEY_SIGNAL: usize = 2;
const KEY_CONN_BASE: usize = 16;
/// A server that stops reading is dropped once this much output is queued for it; it will
/// re-attach from its last processed offset.
const MAX_WBUF: usize = 32 * 1024 * 1024;
const QUEUED_QUERY_MAX_AGE: Duration = Duration::from_secs(5);

struct Conn {
    stream: UnixStream,
    rbuf: FrameBuf,
    wbuf: Vec<u8>,
    nonce: Vec<u8>,
    acquired: bool,
    attached: bool,
    want_write: bool,
}

struct Modes {
    bracketed_paste: bool,
    focus: bool,
    sync: bool,
    kitty: Vec<u32>,
}

pub struct Holder {
    spec: SpawnSpec,
    poller: Poller,
    listener: UnixListener,
    master: Option<rustix::fd::OwnedFd>,
    master_pending: Vec<u8>,
    child: std::process::Child,
    child_pid: u32,
    sig_rx: UnixStream,
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
    started_at_ms: i64,
    should_exit: bool,
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
        let (pty, child) = pty::spawn(
            &spec.argv,
            Path::new(&spec.cwd),
            &spec.env,
            spec.cols,
            spec.rows,
        )?;
        let child_pid = child.id();
        let (sig_rx, sig_tx) = UnixStream::pair()?;
        sig_rx.set_nonblocking(true)?;
        sig_tx.set_nonblocking(true)?;
        signal_hook::low_level::pipe::register(signal_hook::consts::SIGCHLD, sig_tx)?;
        // SAFETY: ignoring SIGPIPE so writes to a dead server return EPIPE instead of killing us.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_IGN);
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
        }
        let poller = Poller::new()?;
        // SAFETY: the registered fds outlive their registration (deleted before drop).
        unsafe {
            poller.add_with_mode(&listener, Event::readable(KEY_LISTENER), PollMode::Level)?;
            poller.add_with_mode(&pty.master, Event::readable(KEY_MASTER), PollMode::Level)?;
            poller.add_with_mode(&sig_rx, Event::readable(KEY_SIGNAL), PollMode::Level)?;
        }
        let mut ring = Ring::new(spec.ring_bytes as usize);
        ring.marker(MarkerKind::Resize {
            cols: spec.cols,
            rows: spec.rows,
            px_w: 0,
            px_h: 0,
        });
        ring.mark_cut(0);
        Ok(Holder {
            spec,
            poller,
            listener,
            master: Some(pty.master),
            master_pending: Vec::new(),
            child,
            child_pid,
            sig_rx,
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
            started_at_ms: now_ms(),
            should_exit: false,
        })
    }

    pub fn child_pid(&self) -> u32 {
        self.child_pid
    }

    pub fn run(mut self) -> Result<()> {
        let mut events = Events::new();
        while !self.should_exit {
            events.clear();
            match self.poller.wait(&mut events, None) {
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
        }
        // Drain queued writes to the acquiring server before exiting.
        for c in self.conns.values_mut() {
            let _ = c.stream.set_nonblocking(false);
            let _ = c.stream.write_all(&c.wbuf);
        }
        Ok(())
    }

    fn accept(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((s, _)) => {
                    if !peer_uid_ok(&s) || s.set_nonblocking(true).is_err() {
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
                        },
                    );
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
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
                if proto_max < PROTO_MIN {
                    self.send(
                        key,
                        &FromHolder::Rejected {
                            reason: format!("holder speaks holder/{PROTO}"),
                        },
                    );
                    return;
                }
                let nonce: Vec<u8> = (0..32).map(|_| rand::random::<u8>()).collect();
                if let Some(c) = self.conns.get_mut(&key) {
                    c.nonce = nonce.clone();
                }
                let msg = FromHolder::HelloOk {
                    proto: PROTO,
                    holder_version: vk_proto::VERSION.to_string(),
                    pane_id: self.spec.pane_id.clone(),
                    mode: Mode::Pty,
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
                let status = if self.exit.is_some() || self.master.is_none() {
                    InputStatus::ChildExited
                } else if !self.seen_inputs.insert(input_id) {
                    InputStatus::Duplicate
                } else {
                    self.input_order.push_back(input_id);
                    if self.input_order.len() > INPUT_DEDUPE_WINDOW
                        && let Some(old) = self.input_order.pop_front()
                    {
                        self.seen_inputs.remove(&old);
                    }
                    self.write_master(&bytes);
                    InputStatus::Written
                };
                let at = self.ring.end();
                if status == InputStatus::Written {
                    self.ring.marker(MarkerKind::InputWritten { input_id });
                }
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
                if !epoch_ok(epoch, self) {
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
        for item in self.ring.read_from(from_offset) {
            match item {
                Item::Bytes(off, a, b) => {
                    let mut bytes = a.to_vec();
                    bytes.extend_from_slice(b);
                    for (i, chunk) in bytes.chunks(64 * 1024).enumerate() {
                        frames.push(FromHolder::Output {
                            offset: off + (i * 64 * 1024) as u64,
                            stream: Stream::Pty,
                            bytes: chunk.to_vec(),
                            replay: true,
                        });
                    }
                }
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

    fn write_master(&mut self, bytes: &[u8]) {
        self.master_pending.extend_from_slice(bytes);
        self.flush_master();
    }

    fn flush_master(&mut self) {
        let Some(m) = &self.master else {
            self.master_pending.clear();
            return;
        };
        while !self.master_pending.is_empty() {
            match rustix::io::write(m, &self.master_pending) {
                Ok(n) => {
                    self.master_pending.drain(..n);
                }
                Err(rustix::io::Errno::AGAIN) => break,
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => {
                    self.master_pending.clear();
                    break;
                }
            }
        }
        let ev = if self.master_pending.is_empty() {
            Event::readable(KEY_MASTER)
        } else {
            Event::all(KEY_MASTER)
        };
        let _ = self.poller.modify_with_mode(m, ev, PollMode::Level);
    }

    fn fg(&self) -> Option<u32> {
        self.master.as_ref().and_then(pty::fg_pgrp)
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
        let fg = self.fg();
        if fg != self.last_fg {
            self.last_fg = fg;
            if let Some(k) = self.attached_key() {
                self.send(k, &FromHolder::FgChanged);
            }
        }
    }

    fn close_master(&mut self) {
        if let Some(m) = self.master.take() {
            let _ = self.poller.delete(&m);
        }
        self.on_sigchld();
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
        if self.exit.is_some() {
            return;
        }
        if let Ok(Some(st)) = self.child.try_wait() {
            use std::os::unix::process::ExitStatusExt;
            self.exit = Some((st.code(), st.signal()));
            // Drain any final output still in the PTY.
            if self.master.is_some() {
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
