//! The gateway supervisor: once the phone gateway is set up, the server runs it as a child
//! process instead of the user keeping `vibeke gateway run` open by hand.
//!
//! - The launch facts come from the binary ([`GatewayLaunch`]: the gateway state dir and whether
//!   it is set up for autostart for this session). Without a `GatewayLaunch` (tests, embedders)
//!   nothing here runs and `gateway.start` / `gateway.stop` answer `not_found`.
//! - The child is `<bin> gateway run --autostart --session <session>` with
//!   `VIBEKE_GATEWAY_SUPERVISED=1`, stdin null, stdout and stderr appended to `gateway.log` in
//!   the state dir (rotated to `gateway.log.1` at spawn time past 5 MiB), in its own process
//!   group, without the pane identity of whatever started the server.
//! - Exit codes: 0 is a clean stop and 4 means "not set up" (neither is restarted); 3 means
//!   another gateway holds `run.lock` (shown as `external`; while ours is wanted the supervisor
//!   looks again every 30 s and takes over once it is gone). Anything else is a crash:
//!   restarted after 1 s, 2 s, 4 s … capped at 60 s, the schedule reset by a run of 60 s; the
//!   10th crash within 10 minutes latches `crashed` until `gateway.start`.
//! - Whether some other gateway runs is decided by `run.lock` alone (probed with a
//!   non-blocking flock that is released at once); `status.json` only adds detail, so a stale
//!   file whose pid was reused never holds a start back.
//! - Once the gateway (the process group leader) has exited, cleanly or not, and on stop, the
//!   rest of its process group is SIGKILLed (right after the leader is reaped: the group id
//!   can't be reused while members remain).
//! - `gateway.start {restart: true}` stops our child first and starts a fresh one (new
//!   connection settings); a hand-run gateway holding `run.lock` is reported as a `conflict`.
//!   `gateway.start` / `gateway.stop` / `gateway.status` take the caller's gateway `dir` and
//!   answer `conflict` when it isn't the dir this server supervises.
//! - While a child runs, the gateway's `status.json` is read every 2 s. An idle supervisor
//!   never wakes: `gateway.status` / `server.status` read the file on demand.
//! - `gateway.stop` latches "don't restart until `gateway.start`". Server stop, a signal and
//!   `server.restart` terminate the child (SIGTERM to its group, SIGKILL after 3 s); a server
//!   that dies without that leaves a child that exits by itself (`--autostart` watches its
//!   parent).
//!
//! Every change of the status emits the `gateway.status` event with the [`GatewayStatus`]
//! object as its data (an ordinary stored event, so render-stream event push delivers it).

use crate::Server;
use crate::api::{Ctx, R, err};
use crate::core::Tx;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use vk_proto::rpc::ErrorKind;

pub const METHODS: &[(&str, bool)] = &[("gateway.start", true), ("gateway.stop", true)];

/// Starting and stopping the gateway are the user's (`gateway.status` stays open).
pub const PANE_FORBIDDEN: &[&str] = &["gateway.start", "gateway.stop"];

/// What the binary tells the server about the gateway (computed from the gateway state dir).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayLaunch {
    /// The gateway state dir (`gateway.toml`, `status.json`, `gateway.log`).
    pub dir: PathBuf,
    /// Set up for autostart for this session: spawn at boot.
    pub autostart: bool,
}

pub const LOG_FILE: &str = "gateway.log";
pub const STATUS_FILE: &str = "status.json";
pub const CONFIG_FILE: &str = "gateway.toml";
/// `gateway.log` is rotated at spawn time once it is larger than this.
pub const LOG_ROTATE_BYTES: u64 = 5 << 20;
const POLL: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// A run at least this long resets the restart delay.
const HEALTHY_RESET: Duration = Duration::from_secs(60);
const CRASH_WINDOW: Duration = Duration::from_secs(600);
/// This many crashes within [`CRASH_WINDOW`] latch `crashed`.
const MAX_CRASHES: usize = 10;
const STOP_GRACE: Duration = Duration::from_secs(3);
/// While another gateway holds `run.lock` and ours is wanted, look again this often.
const EXTERNAL_RETRY: Duration = Duration::from_secs(30);
/// How long `gateway.start` / `gateway.stop` wait for the supervisor.
const API_WAIT: Duration = Duration::from_secs(10);

pub const EXIT_CLEAN: i32 = 0;
pub const EXIT_ALREADY_RUNNING: i32 = 3;
pub const EXIT_NOT_ENABLED: i32 = 4;

/// `status.json` as `vibeke gateway run` writes it.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct StatusFile {
    pub pid: u32,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub relay: Option<String>,
    #[serde(default)]
    pub devices: u32,
    #[serde(default)]
    pub since_ms: u64,
    #[serde(default)]
    pub last_error: Option<String>,
}

/// The `gateway.status` object (also `server.status.gateway` and the `gateway.status` event).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GatewayStatus {
    /// off|starting|connecting|online|offline|local_only|external|crashed
    pub state: String,
    /// The server keeps the gateway running: set up for autostart at boot, or started with
    /// `gateway.start` (cleared by `gateway.stop` and by a "not set up" exit).
    pub autostart: bool,
    /// The running gateway is this server's child.
    pub supervised: bool,
    pub pid: Option<u32>,
    /// Crash restarts since the last `gateway.start` (or boot).
    pub restarts: u32,
    pub relay: Option<String>,
    pub devices: Option<u32>,
    pub since_ms: Option<u64>,
    pub last_error: Option<String>,
    pub log: String,
}

/// The supervisor's own facts, copied out after every step so status reads need no round trip.
#[derive(Debug, Clone, Default)]
struct Snap {
    /// Pid of the live child.
    child: Option<u32>,
    child_since_ms: Option<u64>,
    want: bool,
    crashed: bool,
    /// A crash restart is scheduled.
    pending: bool,
    restarts: u32,
    error: Option<String>,
    /// When the supervisor's own state (off/starting/crashed) last changed.
    since_ms: Option<u64>,
}

type StartReply = oneshot::Sender<Result<GatewayStatus, String>>;

enum Cmd {
    /// `gateway.start` (with a reply; `restart`: replace a running child) or a start kick
    /// (`handoff.send`).
    Start {
        reply: Option<StartReply>,
        restart: bool,
    },
    Stop(StartReply),
    /// The server is going away: terminate the child, spawn nothing until `Resume`. Replies
    /// whether the gateway was wanted.
    Shutdown(oneshot::Sender<bool>),
    /// `server.restart` failed to exec: carry on, restarting the gateway if it was wanted.
    Resume(bool),
}

#[derive(Default)]
struct Shared {
    tx: Option<mpsc::UnboundedSender<Cmd>>,
    snap: Snap,
    /// The last status sent as an event.
    emitted: Option<GatewayStatus>,
}

/// Per-server supervisor state (lives in `server.gateway`).
#[derive(Default)]
pub struct State {
    inner: Mutex<Shared>,
}

fn state(server: &Server) -> &State {
    &server.gateway.supervisor
}

fn launch(server: &Server) -> Option<&GatewayLaunch> {
    server.opts.gateway.as_ref()
}

// ---- pure parts ---------------------------------------------------------------------------

/// The restart delay for the `n`th consecutive crash (1-based): 1 s, 2 s, 4 s … capped at 60 s.
pub fn backoff_delay(n: u32) -> Duration {
    let secs = 1u64 << n.saturating_sub(1).min(16);
    Duration::from_secs(secs).min(BACKOFF_MAX)
}

/// The crash-restart schedule.
#[derive(Debug, Default, Clone)]
pub struct Backoff {
    consecutive: u32,
    crashes: VecDeque<Instant>,
}

impl Backoff {
    /// A crash at `now` after a run of `ran`: the delay before the next start, or `None` to
    /// give up (the [`MAX_CRASHES`]th crash within [`CRASH_WINDOW`]).
    pub fn on_crash(&mut self, now: Instant, ran: Duration) -> Option<Duration> {
        if ran >= HEALTHY_RESET {
            self.consecutive = 0;
        }
        while self
            .crashes
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) > CRASH_WINDOW)
        {
            self.crashes.pop_front();
        }
        self.crashes.push_back(now);
        if self.crashes.len() >= MAX_CRASHES {
            return None;
        }
        self.consecutive += 1;
        Some(backoff_delay(self.consecutive))
    }

    pub fn reset(&mut self) {
        *self = Backoff::default();
    }
}

/// What an exit code means for the supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitKind {
    Clean,
    AlreadyRunning,
    NotEnabled,
    Crash,
}

/// `code` is `None` when the child was killed by a signal (a crash).
pub fn classify_exit(code: Option<i32>) -> ExitKind {
    match code {
        Some(EXIT_CLEAN) => ExitKind::Clean,
        Some(EXIT_ALREADY_RUNNING) => ExitKind::AlreadyRunning,
        Some(EXIT_NOT_ENABLED) => ExitKind::NotEnabled,
        _ => ExitKind::Crash,
    }
}

/// `path` + `.1`.
pub fn rotated_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".1");
    PathBuf::from(s)
}

/// Rename `path` to `path.1` (replacing an older one) when it is larger than `limit`.
pub fn rotate_log(path: &Path, limit: u64) -> bool {
    match std::fs::metadata(path) {
        Ok(m) if m.len() > limit => std::fs::rename(path, rotated_path(path)).is_ok(),
        _ => false,
    }
}

/// Parse `status.json`; anything unreadable counts as absent.
pub fn parse_status_file(bytes: &[u8]) -> Option<StatusFile> {
    serde_json::from_slice::<StatusFile>(bytes)
        .ok()
        .filter(|f| f.pid > 0)
}

fn read_status_file(dir: &Path) -> Option<StatusFile> {
    std::fs::read(dir.join(STATUS_FILE))
        .ok()
        .and_then(|b| parse_status_file(&b))
}

pub const RUN_LOCK_FILE: &str = "run.lock";

/// Whether a gateway runs from `dir`: `Some(pid)` when `run.lock` is held (`pid` is what the
/// holder wrote into it, 0 if unreadable). Probes with a non-blocking exclusive flock that is
/// released at once; a missing file means nobody holds it.
pub fn run_lock_holder(dir: &Path) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let p = dir.join(RUN_LOCK_FILE);
    let f = std::fs::OpenOptions::new().read(true).open(&p).ok()?;
    // SAFETY: flock on an owned, open descriptor; closing it (drop) releases the lock.
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if r == 0 {
        // SAFETY: as above.
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
        return None;
    }
    if std::io::Error::last_os_error().kind() != std::io::ErrorKind::WouldBlock {
        return None;
    }
    Some(
        std::fs::read_to_string(&p)
            .ok()
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(0),
    )
}

/// Paths compare equal when they name the same directory (canonicalized when they exist).
pub fn same_dir(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// `gateway.start` / `stop` / `status` with the caller's gateway `dir`: `Err(message)` when it
/// isn't `ours`.
pub fn check_dir(ours: &Path, p: &Value) -> Result<(), String> {
    match p.get("dir").and_then(Value::as_str) {
        Some(d) if !same_dir(Path::new(d), ours) => Err(format!(
            "this server supervises the gateway in {}, not {d}; stop the server (`vibeke server stop`) and run the command again to use that directory",
            ours.display()
        )),
        _ => Ok(()),
    }
}

const RUNNING_STATES: &[&str] = &[
    "connecting",
    "online",
    "offline",
    "local_only",
    "login_required",
];

fn running_state(s: &str) -> String {
    if RUNNING_STATES.contains(&s) {
        s.to_string()
    } else {
        "connecting".into()
    }
}

/// The status from the supervisor's facts, `status.json` and the `run.lock` holder (`external`:
/// `Some(pid)` when the lock is held; only consulted without a child of ours).
fn derive(
    snap: &Snap,
    file: Option<&StatusFile>,
    external: Option<u32>,
    log: &Path,
) -> GatewayStatus {
    let mut st = GatewayStatus {
        state: "off".into(),
        autostart: snap.want,
        supervised: false,
        pid: None,
        restarts: snap.restarts,
        relay: None,
        devices: None,
        since_ms: snap.since_ms,
        last_error: snap.error.clone(),
        log: log.display().to_string(),
    };
    let from_file = |st: &mut GatewayStatus, f: &StatusFile| {
        st.relay = f.relay.clone();
        st.devices = Some(f.devices);
        st.since_ms = (f.since_ms > 0).then_some(f.since_ms);
        st.last_error = f.last_error.clone();
    };
    if let Some(pid) = snap.child {
        st.supervised = true;
        st.pid = Some(pid);
        match file.filter(|f| f.pid == pid) {
            Some(f) => {
                st.state = running_state(&f.state);
                from_file(&mut st, f);
            }
            None => {
                st.state = "starting".into();
                st.since_ms = snap.child_since_ms.or(snap.since_ms);
            }
        }
        return st;
    }
    if let Some(holder) = external {
        st.state = "external".into();
        // The file only adds detail, and only when it is the lock holder's.
        let f = file.filter(|f| holder == 0 || f.pid == holder);
        st.pid = match (holder, f) {
            (0, Some(f)) => Some(f.pid),
            (0, None) => None,
            (p, _) => Some(p),
        };
        st.last_error = None;
        if let Some(f) = f {
            from_file(&mut st, f);
        }
        return st;
    }
    st.state = if snap.crashed {
        "crashed"
    } else if snap.pending {
        "starting"
    } else {
        "off"
    }
    .into();
    st
}

// ---- status reads -------------------------------------------------------------------------

/// The current status, `None` when the server has no gateway launch facts.
pub fn status(server: &Server) -> Option<GatewayStatus> {
    let l = launch(server)?;
    let (snap, started) = state(server)
        .inner
        .lock()
        .map(|g| (g.snap.clone(), g.tx.is_some()))
        .unwrap_or_default();
    // Supervisor not started (yet): report the boot intent.
    let snap = Snap {
        want: snap.want || (!started && l.autostart),
        ..snap
    };
    let file = read_status_file(&l.dir);
    let external = if snap.child.is_some() {
        None
    } else {
        run_lock_holder(&l.dir)
    };
    Some(derive(
        &snap,
        file.as_ref(),
        external,
        &l.dir.join(LOG_FILE),
    ))
}

/// [`status`] as JSON (`null` without launch facts), for `server.status`.
pub fn status_json(server: &Server) -> Value {
    status(server)
        .and_then(|s| serde_json::to_value(s).ok())
        .unwrap_or(Value::Null)
}

/// The `gateway.status` method: the bridge's `connected` plus the supervisor status.
pub fn status_method(server: &Server, connected: bool) -> Value {
    let mut v = match status_json(server) {
        Value::Object(m) => Value::Object(m),
        _ => json!({}),
    };
    v["configured"] = json!(launch(server).is_some());
    v["connected"] = json!(connected);
    v
}

// ---- API ----------------------------------------------------------------------------------

fn send(server: &Server, cmd: Cmd) -> bool {
    state(server)
        .inner
        .lock()
        .ok()
        .and_then(|g| g.tx.clone())
        .is_some_and(|tx| tx.send(cmd).is_ok())
}

/// The `dir` check for `gateway.status` (no launch facts: nothing to compare).
pub fn check_status_dir(server: &Server, p: &Value) -> Result<(), vk_proto::rpc::RpcError> {
    match launch(server) {
        Some(l) => check_dir(&l.dir, p).map_err(dir_conflict),
        None => Ok(()),
    }
}

fn dir_conflict(msg: String) -> vk_proto::rpc::RpcError {
    err(ErrorKind::Conflict, msg).details(json!({"reason": "gateway_dir"}))
}

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    if !matches!(method, "gateway.start" | "gateway.stop") {
        return None;
    }
    if ctx.pane_scope.is_some() {
        return Some(Err(err(
            ErrorKind::PermissionDenied,
            format!("{method} is not allowed from a pane token"),
        )));
    }
    let Some(l) = launch(server) else {
        return Some(Err(err(
            ErrorKind::NotFound,
            "this server does not manage a gateway",
        )
        .details(json!({"object": "gateway"}))));
    };
    if let Err(m) = check_dir(&l.dir, p) {
        return Some(Err(dir_conflict(m)));
    }
    let restart = p.get("restart").and_then(Value::as_bool).unwrap_or(false);
    let (tx, rx) = oneshot::channel();
    let cmd = if method == "gateway.start" {
        Cmd::Start {
            reply: Some(tx),
            restart,
        }
    } else {
        Cmd::Stop(tx)
    };
    if !send(server, cmd) {
        return Some(Err(err(
            ErrorKind::RemoteUnavailable,
            "the gateway supervisor isn't running",
        )));
    }
    Some(match tokio::time::timeout(API_WAIT, rx).await {
        Ok(Ok(Ok(st))) => serde_json::to_value(st).map_err(crate::api::internal),
        Ok(Ok(Err(m))) => Err(err(ErrorKind::Conflict, m).details(json!({"reason": "external"}))),
        Ok(Err(_)) => Err(err(
            ErrorKind::RemoteUnavailable,
            "the gateway supervisor stopped",
        )),
        Err(_) => Err(err(
            ErrorKind::Timeout,
            format!("{method}: the gateway supervisor did not answer"),
        )),
    })
}

/// Start the gateway if it is set up and not running (`handoff.send` needs one to pick the job
/// up). Never blocks; a gateway that was never set up (no `gateway.toml`) is left alone.
pub fn ensure_running(server: &Server) {
    let Some(l) = launch(server) else {
        return;
    };
    let Some(st) = status(server) else {
        return;
    };
    if st.supervised || st.state == "external" {
        return;
    }
    if !l.dir.join(CONFIG_FILE).exists() {
        return;
    }
    send(
        server,
        Cmd::Start {
            reply: None,
            restart: false,
        },
    );
}

// ---- lifecycle ----------------------------------------------------------------------------

/// Start the supervisor (from `run::serve`); spawns the gateway at once when set up for
/// autostart. Does nothing without launch facts.
pub fn start(server: &Arc<Server>) {
    let Some(l) = launch(server).cloned() else {
        return;
    };
    let (tx, rx) = mpsc::unbounded_channel();
    if let Ok(mut g) = state(server).inner.lock() {
        if g.tx.is_some() {
            return;
        }
        g.tx = Some(tx);
    }
    let sup = Sup {
        server: server.clone(),
        log: l.dir.join(LOG_FILE),
        want: l.autostart,
        launch: l,
        child: None,
        child_pid: None,
        child_started: None,
        child_since_ms: None,
        crashed: false,
        closing: false,
        restart_at: None,
        backoff: Backoff::default(),
        restarts: 0,
        error: None,
        since_ms: Some(now_ms()),
    };
    tokio::spawn(sup.run(rx));
}

/// Terminate the gateway before the server goes away (stop, signal, restart). Returns whether
/// it was wanted (so a failed `server.restart` can [`resume`]). Bounded: about 4 s at most.
pub async fn shutdown(server: &Server) -> bool {
    let (tx, rx) = oneshot::channel();
    if !send(server, Cmd::Shutdown(tx)) {
        return false;
    }
    tokio::time::timeout(STOP_GRACE + Duration::from_secs(2), rx)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or(false)
}

/// Undo [`shutdown`] (the server keeps running after all).
pub fn resume(server: &Server, wanted: bool) {
    send(server, Cmd::Resume(wanted));
}

fn now_ms() -> u64 {
    u64::try_from(vk_store::now_ms()).unwrap_or(0)
}

struct Sup {
    server: Arc<Server>,
    launch: GatewayLaunch,
    log: PathBuf,
    child: Option<tokio::process::Child>,
    child_pid: Option<u32>,
    child_started: Option<Instant>,
    child_since_ms: Option<u64>,
    want: bool,
    crashed: bool,
    closing: bool,
    restart_at: Option<tokio::time::Instant>,
    backoff: Backoff,
    restarts: u32,
    error: Option<String>,
    since_ms: Option<u64>,
}

enum Step {
    Cmd(Option<Cmd>),
    Exit(std::io::Result<std::process::ExitStatus>),
    Restart,
    Tick,
}

async fn wait_child(
    c: &mut Option<tokio::process::Child>,
) -> std::io::Result<std::process::ExitStatus> {
    match c {
        Some(c) => c.wait().await,
        None => std::future::pending().await,
    }
}

async fn sleep_until(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

async fn sleep_for(d: Option<Duration>) {
    match d {
        Some(d) => tokio::time::sleep(d).await,
        None => std::future::pending().await,
    }
}

impl Sup {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        if self.want {
            self.try_start();
        }
        self.publish();
        loop {
            // Poll status.json while our child runs. Idle: no wakeups.
            let poll = self.child.is_some().then_some(POLL);
            let step = tokio::select! {
                c = rx.recv() => Step::Cmd(c),
                r = wait_child(&mut self.child) => Step::Exit(r),
                _ = sleep_until(self.restart_at) => Step::Restart,
                _ = sleep_for(poll) => Step::Tick,
            };
            match step {
                Step::Cmd(None) => break,
                Step::Cmd(Some(c)) => self.on_cmd(c).await,
                Step::Exit(r) => self.on_exit(r),
                Step::Restart => {
                    self.restart_at = None;
                    if self.want && !self.closing && !self.crashed {
                        self.try_start();
                    }
                }
                Step::Tick => {}
            }
            self.publish();
        }
    }

    /// Another gateway holds `run.lock` (we have no child: ours would hold it too).
    fn external(&self) -> Option<u32> {
        if self.child.is_some() {
            return None;
        }
        run_lock_holder(&self.launch.dir)
    }

    /// Spawn unless our child runs; while another gateway holds the lock, look again later
    /// (taking over once it is gone).
    fn try_start(&mut self) {
        if self.child.is_some() {
            return;
        }
        if self.external().is_some() {
            self.restart_at = Some(tokio::time::Instant::now() + EXTERNAL_RETRY);
            return;
        }
        self.restart_at = None;
        self.spawn();
    }

    fn snap(&self) -> Snap {
        Snap {
            child: self.child_pid.filter(|_| self.child.is_some()),
            child_since_ms: self.child_since_ms,
            want: self.want,
            crashed: self.crashed,
            pending: self.restart_at.is_some(),
            restarts: self.restarts,
            error: self.error.clone(),
            since_ms: self.since_ms,
        }
    }

    fn current(&self) -> GatewayStatus {
        let file = read_status_file(&self.launch.dir);
        derive(&self.snap(), file.as_ref(), self.external(), &self.log)
    }

    /// Store the facts for status reads and emit `gateway.status` when the status changed.
    fn publish(&self) {
        let st = self.current();
        let changed = match state(&self.server).inner.lock() {
            Ok(mut g) => {
                g.snap = self.snap();
                if g.emitted.as_ref() == Some(&st) {
                    false
                } else {
                    g.emitted = Some(st.clone());
                    true
                }
            }
            Err(_) => false,
        };
        if changed {
            emit(&self.server, &st);
        }
    }

    fn mark(&mut self) {
        self.since_ms = Some(now_ms());
    }

    async fn on_cmd(&mut self, c: Cmd) {
        match c {
            Cmd::Start { reply, restart } => {
                let mut res = Ok(());
                if !self.closing {
                    self.want = true;
                    if self.crashed {
                        self.crashed = false;
                        self.restarts = 0;
                        self.backoff.reset();
                        self.mark();
                    }
                    if restart && self.child.is_some() {
                        tracing::info!("gateway: restarting for new settings");
                        self.terminate().await;
                        self.restart_at = None;
                        self.backoff.reset();
                        self.error = None;
                        self.mark();
                    }
                    if restart && let Some(pid) = self.external() {
                        let who = if pid > 0 {
                            format!(" (pid {pid})")
                        } else {
                            String::new()
                        };
                        res = Err(format!(
                            "a gateway started by hand is running{who}, so the new settings can't be applied; stop it, then run `vibeke gateway on`"
                        ));
                    }
                    self.try_start();
                }
                if let Some(r) = reply {
                    let _ = r.send(res.map(|()| self.current()));
                }
            }
            Cmd::Stop(reply) => {
                self.want = false;
                self.crashed = false;
                self.restart_at = None;
                self.backoff.reset();
                self.restarts = 0;
                self.error = None;
                self.terminate().await;
                self.mark();
                let _ = reply.send(Ok(self.current()));
            }
            Cmd::Shutdown(reply) => {
                let wanted = self.want;
                self.closing = true;
                self.restart_at = None;
                self.terminate().await;
                let _ = reply.send(wanted);
            }
            Cmd::Resume(wanted) => {
                self.closing = false;
                if wanted && !self.crashed {
                    self.want = true;
                    self.try_start();
                }
            }
        }
    }

    fn on_exit(&mut self, r: std::io::Result<std::process::ExitStatus>) {
        // The leader is reaped; whatever is left of its group (e.g. a transcription process
        // that ignored SIGTERM) goes too.
        kill_group(self.child_pid);
        let ran = self.child_started.map(|t| t.elapsed()).unwrap_or_default();
        self.child = None;
        self.child_pid = None;
        self.child_started = None;
        self.child_since_ms = None;
        self.mark();
        let (code, signal) = match &r {
            Ok(s) => {
                use std::os::unix::process::ExitStatusExt;
                (s.code(), s.signal())
            }
            Err(_) => (None, None),
        };
        match classify_exit(code) {
            ExitKind::Clean => {
                // Stopped from outside (a signal it handles, or `vibeke gateway off`).
                self.want = false;
                self.error = None;
            }
            ExitKind::NotEnabled => {
                self.want = false;
                self.error = Some(
                    "the gateway isn't set up for this session: run `vibeke gateway pair`".into(),
                );
            }
            ExitKind::AlreadyRunning => {
                // Taken over once the other gateway is gone.
                self.error = Some("another gateway is already running".into());
                if self.want && !self.closing {
                    self.restart_at = Some(tokio::time::Instant::now() + EXTERNAL_RETRY);
                }
            }
            ExitKind::Crash => {
                let why = match (code, signal, &r) {
                    (Some(c), _, _) => format!("the gateway exited with code {c}"),
                    (None, Some(s), _) => format!("the gateway was killed by signal {s}"),
                    (_, _, Err(e)) => format!("waiting for the gateway failed: {e}"),
                    _ => "the gateway exited".into(),
                };
                self.crash(why, ran);
            }
        }
    }

    fn crash(&mut self, why: String, ran: Duration) {
        tracing::warn!(error = %why, "gateway: crashed");
        if self.closing || !self.want {
            self.error = Some(why);
            return;
        }
        match self.backoff.on_crash(Instant::now(), ran) {
            Some(d) => {
                self.restarts += 1;
                self.restart_at = Some(tokio::time::Instant::now() + d);
                self.error = Some(format!("{why}; restarting in {} s", d.as_secs()));
            }
            None => {
                self.crashed = true;
                self.restart_at = None;
                self.error = Some(format!(
                    "{why}; it crashed {MAX_CRASHES} times within 10 minutes and is not restarted (see `vibeke gateway logs`, then `vibeke gateway on`)"
                ));
            }
        }
    }

    fn spawn(&mut self) {
        if self.child.is_some() {
            return;
        }
        match self.try_spawn() {
            Ok(child) => {
                self.child_pid = child.id();
                self.child = Some(child);
                self.child_started = Some(Instant::now());
                self.child_since_ms = Some(now_ms());
                self.error = None;
                self.mark();
                tracing::info!(pid = ?self.child_pid, "gateway: started");
            }
            Err(e) => {
                let why = format!("could not start the gateway: {e}");
                self.crash(why, Duration::ZERO);
            }
        }
    }

    fn try_spawn(&self) -> std::io::Result<tokio::process::Child> {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::process::CommandExt;
        std::fs::create_dir_all(&self.launch.dir)?;
        rotate_log(&self.log, LOG_ROTATE_BYTES);
        let out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.log)?;
        let errf = out.try_clone()?;
        let opts = &self.server.opts;
        let mut cmd = std::process::Command::new(&opts.bin);
        cmd.args([
            "gateway",
            "run",
            "--autostart",
            "--session",
            opts.session.as_str(),
        ])
        .env("VIBEKE_GATEWAY_SUPERVISED", "1")
        // Talk to exactly this server.
        .env("VIBEKE_SESSION", &opts.session)
        .env("VIBEKE_SOCKET", self.server.paths.socket())
        // Exactly the dir this supervisor watches.
        .env("VIBEKE_GATEWAY_DIR", &self.launch.dir)
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(errf)
        .process_group(0);
        // A server started from inside a pane must not hand its pane identity on.
        for k in crate::run::PANE_IDENTITY_ENV {
            cmd.env_remove(k);
        }
        let mut cmd = tokio::process::Command::from(cmd);
        cmd.kill_on_drop(false);
        cmd.spawn()
    }

    /// SIGTERM the child's process group, SIGKILL the group after [`STOP_GRACE`] or as soon as
    /// the leader has exited (descendants that ignored SIGTERM).
    async fn terminate(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let pid = self.child_pid.take();
        self.child_started = None;
        self.child_since_ms = None;
        signal_group(pid, libc::SIGTERM);
        if tokio::time::timeout(STOP_GRACE, child.wait())
            .await
            .is_err()
        {
            kill_group(pid);
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
        }
        // The leader is reaped (or lost): the group can't be reused while members remain.
        kill_group(pid);
        tracing::info!("gateway: stopped");
    }
}

/// Signal the process group we created for our child (its pgid is the child's pid).
fn signal_group(pgid: Option<u32>, sig: i32) {
    if let Some(p) = pgid.and_then(|p| i32::try_from(p).ok()).filter(|p| *p > 1) {
        // SAFETY: plain signal delivery to the child's own process group.
        unsafe { libc::killpg(p, sig) };
    }
}

fn kill_group(pgid: Option<u32>) {
    signal_group(pgid, libc::SIGKILL);
}

fn emit(server: &Server, st: &GatewayStatus) {
    let Ok(data) = serde_json::to_value(st) else {
        return;
    };
    let Ok(mut c) = server.core.lock() else {
        return;
    };
    let mut tx = Tx::new();
    tx.event("gateway.status", json!({}), data);
    let _ = server.commit(&mut c, tx);
}

/// Schema registry entries (`api_schema` loads them next to its own tables).
pub const DEFS: &str = r##"
GatewayStatus = {state: off|starting|connecting|online|offline|local_only|login_required|external|crashed, autostart: bool, supervised: bool, pid: int|null, restarts: int, relay: string|null, devices: int|null, since_ms: int|null, last_error: string|null, log: string}
"##;

pub const SHAPES: &str = r##"
# --- the gateway supervisor: the server runs the set-up gateway as its child; full scope, never from a pane ---
# spawn the gateway unless it runs (idempotent); clears a crashed or stopped latch; restart: stop our running gateway first and start a fresh one (conflict when a hand-run gateway holds the lock); dir: the caller's gateway dir, conflict when this server supervises another; not_found when this server doesn't manage a gateway
gateway.start :: {restart?: bool = false, dir?: string} => GatewayStatus
# stop the supervised gateway; it is not restarted until gateway.start; dir as for gateway.start
gateway.stop :: {dir?: string} => GatewayStatus
"##;

pub const EVENTS: &str = r##"
# every change of the gateway supervisor's status; data is the gateway.status object
gateway.status :: {} => GatewayStatus
"##;

#[cfg(test)]
mod tests {
    use super::*;

    fn file(pid: u32, state: &str) -> StatusFile {
        StatusFile {
            pid,
            state: state.into(),
            relay: Some("wss://relay.example".into()),
            devices: 2,
            since_ms: 1234,
            last_error: None,
        }
    }

    fn log() -> PathBuf {
        PathBuf::from("/x/gateway.log")
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let d: Vec<u64> = (1..=8).map(|n| backoff_delay(n).as_secs()).collect();
        assert_eq!(d, vec![1, 2, 4, 8, 16, 32, 60, 60]);
        assert_eq!(backoff_delay(0).as_secs(), 1);
        assert_eq!(backoff_delay(u32::MAX).as_secs(), 60);
    }

    #[test]
    fn backoff_resets_after_healthy_run_and_gives_up() {
        let mut b = Backoff::default();
        let t0 = Instant::now();
        assert_eq!(b.on_crash(t0, Duration::ZERO), Some(Duration::from_secs(1)));
        assert_eq!(
            b.on_crash(t0 + Duration::from_secs(2), Duration::from_secs(1)),
            Some(Duration::from_secs(2))
        );
        // A healthy run resets the delay.
        assert_eq!(
            b.on_crash(t0 + Duration::from_secs(100), Duration::from_secs(90)),
            Some(Duration::from_secs(1))
        );
        // Ten crashes inside ten minutes: give up.
        let mut b = Backoff::default();
        for i in 0..9 {
            assert!(
                b.on_crash(t0 + Duration::from_secs(i), Duration::ZERO)
                    .is_some()
            );
        }
        assert_eq!(
            b.on_crash(t0 + Duration::from_secs(9), Duration::ZERO),
            None
        );
        // Spread over more than ten minutes: keeps going.
        let mut b = Backoff::default();
        for i in 0..20u64 {
            assert!(
                b.on_crash(t0 + Duration::from_secs(i * 120), Duration::from_secs(61))
                    .is_some(),
                "crash {i}"
            );
        }
    }

    #[test]
    fn exit_codes() {
        assert_eq!(classify_exit(Some(0)), ExitKind::Clean);
        assert_eq!(classify_exit(Some(3)), ExitKind::AlreadyRunning);
        assert_eq!(classify_exit(Some(4)), ExitKind::NotEnabled);
        assert_eq!(classify_exit(Some(1)), ExitKind::Crash);
        assert_eq!(classify_exit(Some(101)), ExitKind::Crash);
        assert_eq!(classify_exit(None), ExitKind::Crash);
    }

    #[test]
    fn status_of_our_child() {
        let snap = Snap {
            child: Some(42),
            child_since_ms: Some(10),
            want: true,
            restarts: 1,
            ..Default::default()
        };
        // No status.json yet: starting.
        let st = derive(&snap, None, None, &log());
        assert_eq!(st.state, "starting");
        assert!(st.supervised && st.autostart);
        assert_eq!((st.pid, st.since_ms, st.restarts), (Some(42), Some(10), 1));
        assert_eq!(st.log, "/x/gateway.log");
        // A stale file of an earlier pid: still starting.
        let st = derive(&snap, Some(&file(41, "online")), None, &log());
        assert_eq!(st.state, "starting");
        // Our child's file: copied.
        let st = derive(&snap, Some(&file(42, "online")), Some(99), &log());
        assert_eq!(st.state, "online");
        assert_eq!(st.devices, Some(2));
        assert_eq!(st.since_ms, Some(1234));
        assert_eq!(st.relay.as_deref(), Some("wss://relay.example"));
        let st = derive(&snap, Some(&file(42, "local_only")), None, &log());
        assert_eq!(st.state, "local_only");
        // The relay needs an account login: reported as such, with the gateway's hint.
        let st = derive(
            &snap,
            Some(&StatusFile {
                last_error: Some("run: vibeke login".into()),
                ..file(42, "login_required")
            }),
            None,
            &log(),
        );
        assert_eq!(st.state, "login_required");
        assert_eq!(st.last_error.as_deref(), Some("run: vibeke login"));
        // Unknown states read as connecting.
        let st = derive(&snap, Some(&file(42, "warming")), None, &log());
        assert_eq!(st.state, "connecting");
    }

    #[test]
    fn status_without_child() {
        let snap = Snap::default();
        let st = derive(&snap, None, None, &log());
        assert_eq!(st.state, "off");
        assert!(!st.supervised && !st.autostart);
        assert_eq!((st.pid, st.devices), (None, None));
        // A hand-run gateway holds run.lock: external, with its file's detail.
        let st = derive(&snap, Some(&file(77, "online")), Some(77), &log());
        assert_eq!(st.state, "external");
        assert_eq!(st.pid, Some(77));
        assert_eq!(st.devices, Some(2));
        assert!(!st.supervised);
        // The lock holder wrote no pid: the file's pid stands in.
        let st = derive(&snap, Some(&file(77, "online")), Some(0), &log());
        assert_eq!((st.state.as_str(), st.pid), ("external", Some(77)));
        // A file of another process: external without its detail.
        let st = derive(&snap, Some(&file(77, "online")), Some(78), &log());
        assert_eq!(
            (st.state.as_str(), st.pid, st.devices),
            ("external", Some(78), None)
        );
        // A stale file (its pid may even be alive, reused) but run.lock is free: off.
        let st = derive(
            &snap,
            Some(&file(std::process::id(), "online")),
            None,
            &log(),
        );
        assert_eq!(st.state, "off");
        let crashed = Snap {
            crashed: true,
            error: Some("boom".into()),
            restarts: 9,
            ..Default::default()
        };
        let st = derive(&crashed, Some(&file(77, "online")), None, &log());
        assert_eq!(st.state, "crashed");
        assert_eq!(st.last_error.as_deref(), Some("boom"));
        assert_eq!(st.restarts, 9);
        let pending = Snap {
            pending: true,
            want: true,
            ..Default::default()
        };
        assert_eq!(derive(&pending, None, None, &log()).state, "starting");
    }

    #[test]
    fn status_file_parsing() {
        let f = parse_status_file(
            br#"{"pid": 9, "state": "offline", "relay": null, "devices": 0, "since_ms": 5, "last_error": "dns"}"#,
        )
        .unwrap();
        assert_eq!(f.pid, 9);
        assert_eq!(f.state, "offline");
        assert_eq!(f.last_error.as_deref(), Some("dns"));
        assert!(parse_status_file(b"{").is_none());
        assert!(parse_status_file(br#"{"pid": 0, "state": "online"}"#).is_none());
        // Unknown fields are fine.
        assert!(parse_status_file(br#"{"pid": 3, "state": "online", "extra": 1}"#).is_some());
    }

    #[test]
    fn log_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join(LOG_FILE);
        assert!(!rotate_log(&log, 10), "missing file");
        std::fs::write(&log, b"0123456789").unwrap();
        assert!(!rotate_log(&log, 10), "not over the limit");
        std::fs::write(&log, b"0123456789a").unwrap();
        std::fs::write(rotated_path(&log), b"old").unwrap();
        assert!(rotate_log(&log, 10));
        assert!(!log.exists());
        assert_eq!(
            std::fs::read(rotated_path(&log)).unwrap(),
            b"0123456789a".to_vec()
        );
        assert_eq!(
            rotated_path(&log).file_name().unwrap().to_string_lossy(),
            "gateway.log.1"
        );
    }

    /// Hold `run.lock` the way `gateway run` does (a separate open file description).
    fn hold_lock(dir: &Path, pid: &str) -> std::fs::File {
        use std::os::fd::AsRawFd;
        let p = dir.join(RUN_LOCK_FILE);
        std::fs::write(&p, pid).unwrap();
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&p)
            .unwrap();
        // SAFETY: flock on an owned, open descriptor.
        assert_eq!(
            unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        f
    }

    #[test]
    fn run_lock_is_the_authority() {
        let dir = tempfile::tempdir().unwrap();
        // No lock file: nobody runs.
        assert_eq!(run_lock_holder(dir.path()), None);
        // A stale status.json naming a live pid (ours, as if reused) and a free lock: nobody.
        std::fs::write(
            dir.path().join(STATUS_FILE),
            format!(r#"{{"pid": {}, "state": "online"}}"#, std::process::id()),
        )
        .unwrap();
        std::fs::write(dir.path().join(RUN_LOCK_FILE), "123").unwrap();
        assert_eq!(run_lock_holder(dir.path()), None);
        // Probing doesn't keep the lock: probing twice still finds it free.
        assert_eq!(run_lock_holder(dir.path()), None);
        // Held: the holder's pid from the file.
        let held = hold_lock(dir.path(), "4242");
        assert_eq!(run_lock_holder(dir.path()), Some(4242));
        drop(held);
        assert_eq!(run_lock_holder(dir.path()), None);
        let held = hold_lock(dir.path(), "garbage");
        assert_eq!(run_lock_holder(dir.path()), Some(0));
        drop(held);
    }

    #[test]
    fn dir_param_must_match() {
        let dir = tempfile::tempdir().unwrap();
        let ours = dir.path().join("gw");
        std::fs::create_dir_all(&ours).unwrap();
        assert!(check_dir(&ours, &json!({})).is_ok());
        assert!(check_dir(&ours, &json!({"dir": ours})).is_ok());
        // Another spelling of the same dir.
        let dotted = dir.path().join("gw/../gw");
        assert!(check_dir(&ours, &json!({"dir": dotted})).is_ok());
        let other = dir.path().join("other");
        let e = check_dir(&ours, &json!({"dir": other})).unwrap_err();
        assert!(
            e.contains(&ours.display().to_string()) && e.contains("other"),
            "{e}"
        );
        // Neither exists: compared as given.
        let a = dir.path().join("a");
        assert!(check_dir(&a, &json!({"dir": a})).is_ok());
        assert!(check_dir(&a, &json!({"dir": dir.path().join("b")})).is_err());
    }
}
