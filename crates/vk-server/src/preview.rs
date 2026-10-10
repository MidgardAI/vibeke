//! Previews and the browser route, server side (06 B2, B3.1, B3.3, B3.4).
//!
//! - **Discovery** (every server): every 2 s while panes are active or previews live, backing
//!   off to 30 s when idle (and at once on `pane.process_changed`, or on output after a
//!   back-off), the LISTEN
//!   sockets of panes whose foreground process is not a shell; loopback/wildcard binds that
//!   answer HTTP become suggestions. Pane output is split into lines on the feed path and
//!   lines containing `://` are parsed here, off that path.
//! - **Route** (the viewing machine's server): a SOCKS5 listener on 127.0.0.1 that accepts
//!   only connections owned by a managed browser's process tree, and routes loopback
//!   destinations to the profile's machine (`tcp:` channels over a bridge link this server
//!   opens on demand) and everything else per `preview.profile_route`.
//! - **Window** (B3.3): a headful Chromium-family browser on a Vibeke profile.

use crate::Server;
use crate::api::{Ctx, R, b, err, invalid, not_found, resolve_pane, s, u};
use crate::core::{Core, Tx, ulid};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use vk_preview::socks::{self, Addr, Dest, reply};
use vk_preview::{browser, lifecycle, probe, scan, sockets};
use vk_proto::model::*;
use vk_proto::rpc::{ErrorKind, RpcError};
use vk_remote::{Link, Target};

const TICK: Duration = Duration::from_secs(2);
/// Bound on previews per server; discovery stops suggesting beyond it.
const MAX_PREVIEWS: usize = 64;
/// Output-URL candidate lines queued for parsing (dropped when full).
const LINE_QUEUE: usize = 256;

pub const METHODS: &[(&str, bool)] = &[
    ("preview.declare", true),
    ("preview.list", false),
    ("preview.get", false),
    ("preview.promote", true),
    ("preview.forget", true),
    ("preview.dismiss", true),
    ("preview.open", true),
    ("preview.url", false),
    ("preview.status", false),
    ("preview.profile", true),
    ("preview.profile.list", false),
    ("preview.profile.reset", true),
    ("preview.mirror", true),
    ("preview.unmirror", true),
];

/// `[preview]` keys used here (06 Part C). Unknown keys are ignored.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PreviewConfig {
    /// suggest | promote | off
    pub auto_discover: String,
    /// pane | window | proxy (B4).
    pub mode: String,
    /// machine | task
    pub profile_scope: String,
    /// loopback | remote
    pub profile_route: String,
    /// Remote egress for `remote`-routed profiles (local side) / accepted by the bridge.
    pub allow_remote_egress: bool,
    /// Browser binary for the window (`preview.browser`; `profile_browser` path accepted too).
    pub browser: String,
    /// profile | default (local-machine previews in the system browser).
    pub local_browser: String,
    /// Headless Chromium for browser panes (06 B3.2); "" = Playwright headless shell.
    pub pane_browser: String,
    /// Window browser: auto | chrome | chromium | edge | brave | firefox | an absolute path.
    pub profile_browser: String,
    /// Reverse proxy listener port (B4), machine-wide: busy → proxy mode is refused (no
    /// fallback; pick another port for this session). 0 = ephemeral.
    pub proxy_port: u16,
    /// Serve proxy-mode previews over https with the local CA (B4 `tls_origin`): the global
    /// default; a repo `[previews] tls_origin`, a task preview entry, `preview.declare` and
    /// `preview.open` override it.
    pub tls_origin: bool,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        PreviewConfig {
            auto_discover: "suggest".into(),
            mode: "pane".into(),
            profile_scope: "machine".into(),
            profile_route: "loopback".into(),
            allow_remote_egress: true,
            browser: String::new(),
            local_browser: "profile".into(),
            pane_browser: String::new(),
            profile_browser: "auto".into(),
            proxy_port: 47800,
            tls_origin: false,
        }
    }
}

impl PreviewConfig {
    /// The window uses Firefox (`profile_browser = "firefox"`, or a Firefox binary configured).
    pub fn wants_firefox(&self) -> bool {
        self.profile_browser == "firefox"
            || (!self.browser.is_empty()
                && browser::is_firefox_path(std::path::Path::new(&self.browser)))
    }
}

impl PreviewConfig {
    pub fn load() -> Self {
        let cfg = vk_config::Config::load(vk_config::config_path())
            .map(|(c, _)| c)
            .unwrap_or_default();
        Self::from_config(&cfg)
    }

    pub fn from_config(cfg: &vk_config::Config) -> Self {
        let Some(v) = cfg
            .extra
            .get("preview")
            .and_then(|t| serde_json::to_value(t).ok())
        else {
            return PreviewConfig::default();
        };
        let mut c: PreviewConfig = serde_json::from_value(v.clone()).unwrap_or_default();
        if c.browser.is_empty()
            && let Some(p) = v.get("profile_browser").and_then(Value::as_str)
            && p.starts_with('/')
        {
            c.browser = p.to_string();
        }
        c
    }
}

/// Produces the bridge link for a machine label (default: `[[remote.machine]]` or the label
/// as an ssh destination). Tests inject a piped `vibeke bridge`.
pub type LinkFactory = Arc<dyn Fn(&str) -> Option<Link> + Send + Sync>;
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;
/// (pane id, raw output line) queued for URL parsing.
type PaneLine = (String, Vec<u8>);

/// A browser process tree whose sockets may use the SOCKS listener.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManagedBrowser {
    profile: String,
    machine: String,
    route: String,
    root_pid: u32,
    /// procinfo start time (pid-reuse guard).
    start: u64,
    dir: String,
    /// A headless browser driving browser panes (06 B3.2). Not re-adopted after a restart:
    /// its debugging pipe ended with the old server.
    #[serde(default)]
    headless: bool,
}

/// What a successful peer check grants.
#[derive(Debug, Clone)]
pub struct Grant {
    pub profile: String,
    pub machine: String,
    pub route: String,
}

pub struct Previews {
    scanners: Mutex<HashMap<String, scan::LineScanner>>,
    lines_tx: mpsc::Sender<PaneLine>,
    lines_rx: Mutex<Option<mpsc::Receiver<PaneLine>>>,
    /// Forgotten ports → the pid that owned them (re-suggested only for a new process).
    dismissed: Mutex<HashMap<u16, Option<u32>>>,
    probing: Mutex<HashSet<u16>>,
    socks_port: tokio::sync::Mutex<Option<u16>>,
    browsers: Mutex<HashMap<String, ManagedBrowser>>,
    links: Mutex<HashMap<String, Link>>,
    link_factory: Mutex<Option<LinkFactory>>,
    clock: Mutex<Clock>,
    test_hooks: bool,
    /// Pane activity pacing the discovery poll (fast while recent, backing off when idle).
    pub(crate) pace: crate::timers::Activity,
    pub accepted: AtomicU64,
    pub rejected: AtomicU64,
    /// The B4 reverse proxy (started on the first proxy open).
    pub(crate) proxy: tokio::sync::Mutex<Option<Arc<vk_preview::proxy::Proxy>>>,
    /// The same proxy for synchronous paths (route revocation from commits).
    pub(crate) proxy_handle: Mutex<Option<Arc<vk_preview::proxy::Proxy>>>,
    /// Test hook: the proxy port to use instead of `[preview] proxy_port`.
    pub(crate) proxy_port_override: Mutex<Option<u16>>,
    /// The per-user local CA once a `tls_origin` preview needed it (B4).
    /// Held as a [`vk_preview::ca::CaStore`]: a CA renewed by another process is reloaded.
    pub(crate) tls_ca: Mutex<Option<Arc<vk_preview::ca::CaStore>>>,
    /// Explicit mirrors by local port (B4; never persisted, never automatic).
    pub(crate) mirrors: Mutex<HashMap<u16, crate::preview_fabric::Mirror>>,
    /// Ports the user approved for a pane to declare although no pane listens on them
    /// ([`declare_api`]): pane → (its child process when approved, ports). In memory only; a
    /// restarted pane (new child process) asks again.
    approved_ports: Mutex<HashMap<String, ApprovedPorts>>,
}

/// A pane's approved ports: its child process when approved, and the ports.
type ApprovedPorts = (Option<u32>, HashSet<u16>);

impl Default for Previews {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel(LINE_QUEUE);
        Previews {
            scanners: Mutex::default(),
            lines_tx: tx,
            lines_rx: Mutex::new(Some(rx)),
            dismissed: Mutex::default(),
            probing: Mutex::default(),
            socks_port: tokio::sync::Mutex::new(None),
            browsers: Mutex::default(),
            links: Mutex::default(),
            link_factory: Mutex::new(None),
            clock: Mutex::new(Arc::new(vk_store::now_ms)),
            test_hooks: std::env::var("VIBEKE_TEST_HOOKS").is_ok_and(|v| v == "1"),
            pace: Default::default(),
            accepted: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            proxy: tokio::sync::Mutex::new(None),
            proxy_handle: Mutex::default(),
            proxy_port_override: Mutex::default(),
            tls_ca: Mutex::default(),
            mirrors: Mutex::default(),
            approved_ports: Mutex::default(),
        }
    }
}

fn alive(pid: u32, start: u64) -> bool {
    // SAFETY: signal 0 only checks existence/permission.
    if pid == 0 || unsafe { libc::kill(pid as i32, 0) } != 0 {
        return false;
    }
    start == 0 || vk_hold::procinfo::info(pid).is_some_and(|i| i.start == start)
}

impl Previews {
    pub fn now(&self) -> i64 {
        (self.clock.lock().unwrap())()
    }

    /// Test hook: replace the clock used for lifecycle timing.
    pub fn set_clock(&self, c: Clock) {
        *self.clock.lock().unwrap() = c;
    }

    /// Test hook / embedding: the proxy port to use instead of `[preview] proxy_port`.
    pub fn set_proxy_port(&self, port: u16) {
        *self.proxy_port_override.lock().unwrap() = Some(port);
    }

    /// Test hook / embedding: how links to machines are made.
    pub fn set_link_factory(&self, f: LinkFactory) {
        *self.link_factory.lock().unwrap() = Some(f);
        self.links.lock().unwrap().clear();
    }

    /// Hot path (pane feed): split into lines, queue the ones with `://`.
    pub fn on_output(&self, pane: &str, data: &[u8]) {
        self.pace.touch();
        let mut lines = Vec::new();
        {
            let mut sc = self.scanners.lock().unwrap();
            match sc.get_mut(pane) {
                Some(s) => s.feed(data, &mut lines),
                None => {
                    let mut s = scan::LineScanner::default();
                    s.feed(data, &mut lines);
                    sc.insert(pane.to_string(), s);
                }
            }
        }
        for l in lines {
            let _ = self.lines_tx.try_send((pane.to_string(), l));
        }
    }

    pub(crate) fn link(&self, server: &Server, machine: &str) -> Option<Link> {
        if let Some(l) = self.links.lock().unwrap().get(machine) {
            return Some(l.clone());
        }
        let f = self.link_factory.lock().unwrap().clone();
        let l = match f {
            Some(f) => f(machine)?,
            None => default_link(server, machine)?,
        };
        self.links
            .lock()
            .unwrap()
            .insert(machine.to_string(), l.clone());
        Some(l)
    }

    fn browsers_snapshot(&self) -> Vec<ManagedBrowser> {
        self.browsers.lock().unwrap().values().cloned().collect()
    }

    fn running(&self, profile: &str) -> Option<ManagedBrowser> {
        let b = self.browsers.lock().unwrap().get(profile).cloned()?;
        if alive(b.root_pid, b.start) {
            Some(b)
        } else {
            self.browsers.lock().unwrap().remove(profile);
            None
        }
    }
}

fn default_link(server: &Server, machine: &str) -> Option<Link> {
    if vk_remote::link::check_session(&server.opts.session).is_err() {
        return None;
    }
    let cfg = vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| c)
        .unwrap_or_default();
    let target = match cfg.remote.machine.iter().find(|m| m.label == machine) {
        Some(m) => Target::parse(&m.label, &m.address),
        None => Target::parse(machine, machine),
    };
    Some(Link::new(target, &server.opts.session))
}

/// Track a browser-pane Chromium (06 B3.2) so the SOCKS peer check accepts its process tree.
pub(crate) fn register_browser(
    server: &Server,
    profile: &str,
    machine: &str,
    route: &str,
    pid: u32,
    dir: &std::path::Path,
) {
    let start = vk_hold::procinfo::info(pid).map(|i| i.start).unwrap_or(0);
    server.previews.browsers.lock().unwrap().insert(
        profile.to_string(),
        ManagedBrowser {
            profile: profile.to_string(),
            machine: machine.to_string(),
            route: route.to_string(),
            root_pid: pid,
            start,
            dir: dir.to_string_lossy().into_owned(),
            headless: true,
        },
    );
}

pub(crate) fn unregister_browser(server: &Server, profile: &str, pid: u32) {
    let mut b = server.previews.browsers.lock().unwrap();
    if b.get(profile).is_some_and(|x| x.root_pid == pid) {
        b.remove(profile);
    }
}

/// The live browser on `profile`: (root pid, headless).
pub(crate) fn running_browser(server: &Server, profile: &str) -> Option<(u32, bool)> {
    server
        .previews
        .running(profile)
        .map(|b| (b.root_pid, b.headless))
}

pub(crate) fn is_local_machine(server: &Server, m: &str) -> bool {
    m.is_empty() || m == "local" || m == server.opts.machine
}

fn persist_browsers(server: &Server) {
    let v: Vec<ManagedBrowser> = server.previews.browsers_snapshot();
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.m.kv(
        "preview",
        "browsers",
        Some(serde_json::to_string(&v).unwrap_or_default()),
    );
    let _ = c.commit(tx);
}

// ---- startup --------------------------------------------------------------------------------

pub fn start(server: &Arc<Server>) {
    let (browsers, socks_port) = {
        let mut c = server.core.lock().unwrap();
        let ps: Vec<Preview> = c.store.load("preview").unwrap_or_default();
        c.model.previews = ps;
        let b: Vec<ManagedBrowser> = c
            .store
            .kv_get("preview", "browsers")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let port: Option<u16> = c
            .store
            .kv_get("preview", "socks_port")
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok());
        (b, port)
    };
    server.bump_model();
    // Re-adopt browsers launched by a previous server (Chromium is not holder-owned): same
    // pid, same start time, and still running on our profile directory.
    let mut adopted = false;
    for b in browsers {
        let ok = !b.headless
            && alive(b.root_pid, b.start)
            && vk_hold::procinfo::argv(b.root_pid)
                .iter()
                // Chromium: `--user-data-dir=<dir>`; Firefox: `-profile <dir>`.
                .any(|a| {
                    a == &format!("--user-data-dir={}", b.dir) || (!b.dir.is_empty() && a == &b.dir)
                });
        if ok {
            adopted = true;
            server
                .previews
                .browsers
                .lock()
                .unwrap()
                .insert(b.profile.clone(), b);
        }
    }
    if adopted {
        // Running browsers were started with the old SOCKS port: bring it back.
        let srv = server.clone();
        tokio::spawn(async move {
            let _ = ensure_socks(&srv, socks_port).await;
        });
    }
    let rx = server.previews.lines_rx.lock().unwrap().take();
    if let Some(rx) = rx {
        tokio::spawn(output_loop(server.clone(), rx));
    }
    tokio::spawn(discovery_loop(server.clone()));
}

/// Fields shown in `server.status`.
/// The binaries this server would launch (`preview.status.available_browsers`): `pane` for
/// browser panes it hosts media for (06 B3.2), `window` for headful profile windows (B3.4).
/// `null` = none found; `vibeke browser install` fixes both where it installs the full browser
/// (a machine with a display), the pane one only where it installs the headless shell (B5).
pub fn available_browsers(cfg: &PreviewConfig) -> Value {
    let pane = crate::browser_pane::find_pane_browser(
        Some(cfg.pane_browser.as_str()),
        &crate::agent_browser::install_root(),
    )
    .map(|b| json!({"binary": b.path, "kind": b.kind}));
    let window = if cfg.wants_firefox() {
        browser::find_firefox(Some(cfg.browser.as_str()))
    } else {
        let installed =
            vk_browser::install::installed_full(&crate::agent_browser::install_root());
        browser::find_browser_with(
            Some(cfg.browser.as_str()).filter(|b| !b.is_empty()),
            installed.as_deref(),
        )
    }
    .map(|b| json!({"binary": b.path, "kind": b.kind}));
    json!({"pane": pane, "window": window})
}

pub fn status_json(server: &Server) -> Value {
    let port = server.previews.socks_port.try_lock().ok().and_then(|g| *g);
    json!({
        "socks_port": port,
        "browsers": server.previews.browsers.lock().unwrap().len(),
        "proxy_port": crate::preview_fabric::proxy_port(server),
        "mirrors": server.previews.mirrors.lock().unwrap().len(),
        "previews": server.with_core(|c| c.model.previews.len()),
    })
}

// ---- persistence ----------------------------------------------------------------------------

fn subject(p: &Preview) -> Value {
    json!({"preview": p.id, "preview_handle": p.handle, "pane": p.pane, "task": p.task, "machine": p.machine})
}

fn put(tx: &mut Tx, p: &Preview) {
    if p.status == PreviewStatus::Gone {
        tx.m.close("preview", &p.id, Some(&p.handle), p);
    } else {
        tx.m.put("preview", &p.id, Some(&p.handle), p);
    }
}

fn apply_model(c: &mut Core, p: Preview) {
    if p.status == PreviewStatus::Gone {
        c.model.previews.retain(|x| x.id != p.id);
    } else {
        match c.model.previews.iter_mut().find(|x| x.id == p.id) {
            Some(x) => *x = p,
            None => c.model.previews.push(p),
        }
    }
}

/// Persist changed previews (with optional events) and apply them to the model.
pub(crate) fn commit_previews(server: &Server, items: Vec<(Preview, Option<&'static str>)>) {
    if items.is_empty() {
        return;
    }
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    for (p, ev) in &items {
        put(&mut tx, p);
        if let Some(ev) = ev {
            tx.event(ev, subject(p), json!({"preview": p}));
        }
    }
    if server.commit(&mut c, tx).is_ok() {
        for (p, _) in items {
            // A gone preview loses its proxy origin and sessions, whatever retired it.
            if p.status == PreviewStatus::Gone {
                crate::preview_fabric::revoke_route(server, "local", &p.id);
            }
            apply_model(&mut c, p);
        }
    }
}

/// Allocate handles and create previews. Returns them as stored.
fn create_previews(server: &Server, items: Vec<(Preview, &'static str)>) -> Vec<Preview> {
    if items.is_empty() {
        return vec![];
    }
    let mut c = server.core.lock().unwrap();
    let mut n: u64 = c
        .store
        .kv_get("preview", "counter")
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut tx = Tx::new();
    let mut out = Vec::new();
    for (mut p, ev) in items {
        if c.model
            .previews
            .iter()
            .any(|x| x.port == p.port && x.status != PreviewStatus::Gone)
            || out.iter().any(|x: &Preview| x.port == p.port)
        {
            continue; // found meanwhile by another path
        }
        n += 1;
        p.handle = format!("v{n}");
        put(&mut tx, &p);
        tx.event(ev, subject(&p), json!({"preview": p}));
        out.push(p);
    }
    tx.m.kv("preview", "counter", Some(n.to_string()));
    if server.commit(&mut c, tx).is_err() {
        return vec![];
    }
    for p in &out {
        apply_model(&mut c, p.clone());
    }
    out
}

fn new_preview(server: &Server, port: u16, source: PreviewSource, now: i64) -> Preview {
    Preview {
        id: ulid(),
        handle: String::new(),
        machine: server.opts.machine.clone(),
        pane: None,
        task: None,
        port,
        path: "/".into(),
        label: None,
        url: format!("http://localhost:{port}/"),
        scheme: "http".into(),
        status: PreviewStatus::Suggested,
        source,
        pid: None,
        first_seen_ms: now,
        last_seen_ms: now,
    }
}

pub(crate) fn task_of_pane(c: &Core, pane: &str) -> Option<String> {
    let p = c.pane(pane)?;
    c.ws(&p.workspace).and_then(|w| w.task.clone())
}

/// Does the pane-scoped caller `pane` (whose workspace task is `task`) own `pv`? Its own
/// task's previews, its own pane's and those of panes it created (06 B5 "own previews only").
/// The one ownership rule shared by `preview.declare` and the browser session policy.
pub(crate) fn pane_owns_preview(c: &Core, pane: &str, task: Option<&str>, pv: &Preview) -> bool {
    if pv.task.is_some() && pv.task.as_deref() == task {
        return true;
    }
    let Some(owner) = &pv.pane else { return false };
    owner == pane
        || c.pane(owner)
            .is_some_and(|p| p.created_by == format!("agent:{pane}"))
}

/// The pane whose process tree listens on `port` (loopback or wildcard bind), if any. Blocking:
/// one procinfo walk per live pane.
fn listener_pane(server: &Server, port: u16) -> Option<String> {
    let roots: Vec<(String, u32)> = server.with_core(|c| {
        c.model
            .panes
            .iter()
            .filter(|p| !p.exited)
            .filter_map(|p| p.child_pid.map(|pid| (p.id.clone(), pid)))
            .collect()
    });
    roots.into_iter().find_map(|(pane, root)| {
        let pids: Vec<u32> = vk_hold::procinfo::tree(root, 8)
            .into_iter()
            .map(|i| i.pid)
            .collect();
        sockets::listeners(&pids)
            .iter()
            .any(|l| l.port == port && sockets::is_local_bind(&l.addr))
            .then_some(pane)
    })
}

fn significant(a: &Preview, b: &Preview) -> bool {
    a.status != b.status
        || a.pid != b.pid
        || a.pane != b.pane
        || a.label != b.label
        || a.path != b.path
        || a.url != b.url
        || a.task != b.task
        || a.source != b.source
}

// ---- discovery ------------------------------------------------------------------------------

/// Discovery stays on the [`TICK`] period this long after the last pane output / foreground
/// change, then backs off (doubling) to [`DISCOVERY_SLOW_MAX`] while nothing happens.
const DISCOVERY_FAST_WINDOW: Duration = Duration::from_secs(30);
const DISCOVERY_SLOW_MAX: Duration = Duration::from_secs(30);

async fn discovery_loop(server: Arc<Server>) {
    let mut events = server.events.subscribe();
    let mut cfg = PreviewConfig::load();
    let mut cfg_at = Instant::now();
    let mut last = Instant::now() - TICK;
    // Paced by activity (spec 10 §1.3 wakeup budget): pane output, foreground changes and pane
    // lifecycle keep the 2 s period (and output wakes a backed-off loop at once); with no
    // activity and no live previews the period doubles up to 30 s.
    let window = std::env::var("VIBEKE_PREVIEW_FAST_WINDOW_MS")
        .ok()
        .filter(|_| server.previews.test_hooks)
        .and_then(|v| v.parse().ok())
        .map_or(DISCOVERY_FAST_WINDOW, Duration::from_millis);
    let mut pace = crate::timers::Backoff::new(TICK, window, DISCOVERY_SLOW_MAX);
    let act = &server.previews.pace;
    let mut every = TICK;
    loop {
        if every > TICK {
            act.park();
        }
        tokio::select! {
            _ = tokio::time::sleep(every.saturating_sub(last.elapsed())) => {}
            // Output while backed off: scan now.
            _ = act.woken(), if every > TICK => {}
            ev = events.recv() => match ev {
                Ok(e) if matches!(e.kind.as_str(), "pane.process_changed" | "pane.closed" | "pane.created") => {
                    // Foreground change: rescan now (debounced) and stay fast for a while.
                    act.note();
                    every = pace.reset();
                    if last.elapsed() < Duration::from_millis(300) { continue }
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            }
        }
        act.unpark();
        if cfg_at.elapsed() > Duration::from_secs(10) {
            cfg = PreviewConfig::load();
            cfg_at = Instant::now();
        }
        last = Instant::now();
        discover(&server, &cfg).await;
        // Live previews need presence checks on the fast period (lifecycle timing).
        let busy = server.with_core(|c| {
            c.model
                .previews
                .iter()
                .any(|p| p.status != PreviewStatus::Gone)
        });
        every = pace.next(act.since_touch(), busy);
        act.pass_done(every);
    }
}

/// One discovery pass: listener scan, HTTP classification of new ports, presence and
/// lifecycle of existing previews.
pub async fn discover(server: &Arc<Server>, cfg: &PreviewConfig) {
    let now = server.previews.now();
    let (panes, pane_ids, existing) = server.with_core(|c| {
        let panes: Vec<(String, u32, Option<String>, bool)> = c
            .model
            .panes
            .iter()
            .filter(|p| !p.exited)
            .filter_map(|p| {
                let fg_shell = p.fg_cmdline.first().is_none_or(|a| vk_preview::is_shell(a));
                p.child_pid
                    .map(|pid| (p.id.clone(), pid, task_of_pane(c, &p.id), fg_shell))
            })
            .collect();
        let ids: HashSet<String> = c.model.panes.iter().map(|p| p.id.clone()).collect();
        (panes, ids, c.model.previews.clone())
    });
    // Panes worth a scan: a non-shell foreground process, or a shell with children (the
    // foreground report lags for programs that print nothing, and `pnpm dev &` is common).
    // A shell with no children costs one libproc/procfs call and nothing else.
    let scan: Vec<(String, u32, Option<String>)> = tokio::task::spawn_blocking(move || {
        panes
            .into_iter()
            .filter(|(_, pid, _, fg_shell)| {
                !*fg_shell || !vk_hold::procinfo::children(*pid).is_empty()
            })
            .map(|(id, pid, task, _)| (id, pid, task))
            .collect()
    })
    .await
    .unwrap_or_default();
    server
        .previews
        .scanners
        .lock()
        .unwrap()
        .retain(|k, _| pane_ids.contains(k));
    let scanned: HashSet<String> = scan.iter().map(|(p, _, _)| p.clone()).collect();
    let roots: Vec<(String, u32)> = scan.iter().map(|(p, pid, _)| (p.clone(), *pid)).collect();
    let found: Vec<(String, sockets::Listener)> = if cfg.auto_discover == "off" || roots.is_empty()
    {
        vec![]
    } else {
        tokio::task::spawn_blocking(move || {
            let mut out: Vec<(String, sockets::Listener)> = Vec::new();
            for (pane, root) in roots {
                // The agent harness's own listeners (Codex app-server, Claude Code's IDE
                // bridge) are not dev servers; processes it starts still are.
                let pids: Vec<u32> = vk_hold::procinfo::tree(root, 8)
                    .into_iter()
                    .filter(|i| !vk_preview::is_harness_process(&i.argv))
                    .map(|i| i.pid)
                    .collect();
                for l in sockets::listeners(&pids) {
                    if sockets::is_local_bind(&l.addr) && !out.iter().any(|(_, x)| x.port == l.port)
                    {
                        out.push((pane.clone(), l));
                    }
                }
            }
            out
        })
        .await
        .unwrap_or_default()
    };

    // New listeners → HTTP probe → suggestions.
    let known: HashSet<u16> = existing
        .iter()
        .filter(|p| p.status != PreviewStatus::Gone)
        .map(|p| p.port)
        .collect();
    let candidates: Vec<&(String, sockets::Listener)> = {
        let mut dismissed = server.previews.dismissed.lock().unwrap();
        // A forgotten port that stopped listening may come back as a new suggestion.
        if cfg.auto_discover != "off" {
            dismissed.retain(|port, _| found.iter().any(|(_, l)| l.port == *port));
        }
        found
            .iter()
            .filter(|(_, l)| !known.contains(&l.port))
            .filter(|(_, l)| match dismissed.get(&l.port) {
                // Same listener as when forgotten (or unknown pid): stay hidden.
                Some(d) => d.is_some() && *d != Some(l.pid),
                None => true,
            })
            .collect()
    };
    if !candidates.is_empty() && existing.len() < MAX_PREVIEWS {
        // Plain HTTP first; a listener that isn't gets a TLS probe (self-signed accepted, no
        // credentials sent) and becomes an `https` suggestion if a page answers over TLS.
        let probes = futures::future::join_all(candidates.iter().map(|(_, l)| async move {
            let t = Duration::from_secs(1);
            if probe::is_web_page(Some(l.addr), l.port, t).await {
                Some("http")
            } else if vk_preview::tls::is_web_page(Some(l.addr), l.port, t).await {
                Some("https")
            } else {
                None
            }
        }))
        .await;
        let promote = cfg.auto_discover == "promote";
        let mut items = Vec::new();
        for ((pane, l), scheme) in candidates.iter().zip(probes) {
            let Some(scheme) = scheme else {
                continue; // other TCP is hidden (06 B2)
            };
            let mut p = new_preview(server, l.port, PreviewSource::Listener, now);
            if scheme == "https" {
                p.scheme = "https".into();
                p.url = format!("https://localhost:{}/", l.port);
            }
            p.pane = Some(pane.clone());
            p.task = scan
                .iter()
                .find(|(x, _, _)| x == pane)
                .and_then(|(_, _, t)| t.clone());
            p.pid = Some(l.pid);
            if promote {
                p.status = PreviewStatus::Up;
            }
            items.push((p, "preview.discovered"));
        }
        create_previews(server, items);
    }

    // Presence + lifecycle for existing previews.
    let live: Vec<Preview> = existing
        .into_iter()
        .filter(|p| p.status != PreviewStatus::Gone)
        .collect();
    let needs_probe: Vec<bool> = live
        .iter()
        .map(|p| {
            !found.iter().any(|(_, l)| l.port == p.port)
                && !(p.source == PreviewSource::Listener
                    && p.pane.as_ref().is_some_and(|x| scanned.contains(x)))
        })
        .collect();
    let alive_probes = futures::future::join_all(live.iter().zip(&needs_probe).map(|(p, need)| {
        let port = p.port;
        let need = *need;
        async move { need && probe::tcp_alive(None, port).await }
    }))
    .await;
    let mut changes = Vec::new();
    let mut quiet = Vec::new();
    for ((p, need), probed) in live.iter().zip(needs_probe).zip(alive_probes) {
        let mut q = p.clone();
        let ev = if q.pane.as_ref().is_some_and(|x| !pane_ids.contains(x))
            && q.source != PreviewSource::Declared
        {
            lifecycle::retire(&mut q)
        } else {
            let hit = found.iter().find(|(_, l)| l.port == q.port);
            if let Some((pane, l)) = hit {
                q.pid = Some(l.pid);
                if q.pane.is_none() {
                    q.pane = Some(pane.clone());
                }
            }
            let present = hit.is_some() || (need && probed);
            lifecycle::observe(&mut q, present, now)
        };
        if ev.is_some() || significant(p, &q) {
            changes.push((q, ev));
        } else if q.last_seen_ms != p.last_seen_ms {
            quiet.push(q);
        }
    }
    commit_previews(server, changes);
    // last_seen only: in memory, no event or model bump.
    if !quiet.is_empty() {
        let mut c = server.core.lock().unwrap();
        for q in quiet {
            if let Some(x) = c.model.previews.iter_mut().find(|x| x.id == q.id) {
                x.last_seen_ms = q.last_seen_ms;
            }
        }
    }
}

async fn output_loop(server: Arc<Server>, mut rx: mpsc::Receiver<PaneLine>) {
    let mut cfg = PreviewConfig::load();
    let mut cfg_at = Instant::now();
    while let Some((pane, line)) = rx.recv().await {
        if cfg_at.elapsed() > Duration::from_secs(10) {
            cfg = PreviewConfig::load();
            cfg_at = Instant::now();
        }
        if cfg.auto_discover == "off" {
            continue;
        }
        for f in scan::parse_line(&line) {
            on_found_url(&server, &cfg, &pane, f);
        }
    }
}

fn normalize_url(f: &scan::Found) -> String {
    if f.host == "0.0.0.0" {
        format!("{}://localhost:{}{}", f.scheme, f.port, f.path)
    } else {
        f.url.clone()
    }
}

fn on_found_url(server: &Arc<Server>, cfg: &PreviewConfig, pane: &str, f: scan::Found) {
    let existing = server.with_core(|c| {
        c.model
            .previews
            .iter()
            .find(|p| p.port == f.port && p.status != PreviewStatus::Gone)
            .cloned()
    });
    if let Some(p) = existing {
        // Enrich what the listener scan found with the banner's label and path.
        let mut q = p.clone();
        if f.label.is_some() && q.label.is_none() {
            q.label = f.label.clone();
        }
        if q.path == "/" && f.path != "/" && q.source != PreviewSource::Declared {
            q.path = f.path.clone();
            q.url = normalize_url(&f);
        }
        if q.source == PreviewSource::Listener && f.banner {
            q.url = normalize_url(&f);
        }
        if q.pane.is_none() {
            q.pane = Some(pane.to_string());
        }
        if significant(&p, &q) {
            commit_previews(server, vec![(q, None)]);
        }
        return;
    }
    if server
        .previews
        .dismissed
        .lock()
        .unwrap()
        .contains_key(&f.port)
        || server.with_core(|c| c.model.previews.len()) >= MAX_PREVIEWS
        || !server.previews.probing.lock().unwrap().insert(f.port)
    {
        return;
    }
    let srv = server.clone();
    let pane = pane.to_string();
    let promote = cfg.auto_discover == "promote";
    tokio::spawn(async move {
        // The URL is usually printed right after bind; give slow starters a few seconds.
        let mut up = false;
        for _ in 0..10 {
            up = if f.scheme == "https" {
                // TLS handshake (self-signed accepted) + HTTP probe, no credentials.
                vk_preview::tls::is_web_page(None, f.port, Duration::from_secs(1)).await
            } else {
                probe::is_web_page(None, f.port, Duration::from_secs(1)).await
            };
            if up {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        if up {
            let now = srv.previews.now();
            let source = if f.banner {
                PreviewSource::Banner
            } else {
                PreviewSource::OutputUrl
            };
            let mut p = new_preview(&srv, f.port, source, now);
            p.pane = Some(pane.clone());
            p.task = srv.with_core(|c| task_of_pane(c, &pane));
            p.path = f.path.clone();
            p.url = normalize_url(&f);
            p.scheme = f.scheme.clone();
            p.label = f.label.clone();
            // A printed URL proves nothing about who listens: promote only when the listener
            // is in this pane's own process tree, otherwise leave a suggestion.
            if promote {
                let (s2, port) = (srv.clone(), f.port);
                let owner = tokio::task::spawn_blocking(move || listener_pane(&s2, port))
                    .await
                    .ok()
                    .flatten();
                if owner.as_deref() == Some(pane.as_str()) {
                    p.status = PreviewStatus::Up;
                }
            }
            create_previews(&srv, vec![(p, "preview.discovered")]);
        }
        srv.previews.probing.lock().unwrap().remove(&f.port);
    });
}

// ---- SOCKS route ----------------------------------------------------------------------------

struct Router {
    server: Arc<Server>,
}

/// Loopback host text for a `tcp:` channel / local connect (`0.0.0.0` means this host).
fn loopback_target(dest: &Dest) -> String {
    match &dest.addr {
        Addr::Domain(d) => format!("{d}:{}", dest.port),
        Addr::V4(a) if a.is_unspecified() => format!("127.0.0.1:{}", dest.port),
        Addr::V4(a) => format!("{a}:{}", dest.port),
        Addr::V6(a) => match a.to_ipv4_mapped() {
            Some(v4) if v4.is_unspecified() => format!("127.0.0.1:{}", dest.port),
            Some(v4) => format!("{v4}:{}", dest.port),
            None if a.is_unspecified() => format!("[::1]:{}", dest.port),
            None => format!("[{a}]:{}", dest.port),
        },
    }
}

fn dest_is_local(dest: &Dest) -> bool {
    dest.is_loopback()
        || match &dest.addr {
            Addr::V4(a) => a.is_unspecified(),
            Addr::V6(a) => IpAddr::V6(*a).to_canonical().is_unspecified(),
            Addr::Domain(_) => false,
        }
}

/// A DNS answer that means "the profile's machine" (loopback or unspecified), canonical:
/// `::ffff:127.0.0.1` is 127.0.0.1, not a v6 address to connect to directly from here (which
/// would reach **this** machine's loopback instead of the remote's).
fn machine_local_answer(addrs: &[SocketAddr]) -> Option<IpAddr> {
    addrs
        .iter()
        .map(|a| a.ip().to_canonical())
        .find(|ip| ip.is_loopback() || ip.is_unspecified())
}

async fn to_machine(
    server: &Arc<Server>,
    g: &Grant,
    dest: &Dest,
) -> Result<Box<dyn socks::Stream>, u8> {
    let target = loopback_target(dest);
    if is_local_machine(server, &g.machine) {
        let (h, p) = vk_remote::split_host_port(&target).ok_or(reply::GENERAL_FAILURE)?;
        let s = vk_remote::connect_loopback(&h, p)
            .await
            .map_err(|_| reply::CONNECTION_REFUSED)?;
        return Ok(Box::new(s));
    }
    let link = server
        .previews
        .link(server, &g.machine)
        .ok_or(reply::NETWORK_UNREACHABLE)?;
    match link.open_kind(&format!("tcp:{target}")).await {
        Ok(s) => Ok(Box::new(s)),
        Err(e) => {
            tracing::debug!(machine = %g.machine, %target, error = %format!("{e:#}"), "preview route: tcp channel failed");
            Err(reply::CONNECTION_REFUSED)
        }
    }
}

impl socks::Router for Router {
    type Grant = Grant;

    fn authorize(&self, peer: SocketAddr, local: SocketAddr) -> socks::BoxFuture<Option<Grant>> {
        let browsers = self.server.previews.browsers_snapshot();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                for b in browsers {
                    if !alive(b.root_pid, b.start) {
                        continue;
                    }
                    let pids: Vec<u32> = vk_hold::procinfo::tree(b.root_pid, 6)
                        .into_iter()
                        .map(|i| i.pid)
                        .collect();
                    if sockets::owner_of_connection(&pids, peer, local).is_some() {
                        return Some(Grant {
                            profile: b.profile,
                            machine: b.machine,
                            route: b.route,
                        });
                    }
                }
                None
            })
            .await
            .ok()
            .flatten()
        })
    }

    fn connect(
        &self,
        g: Grant,
        dest: Dest,
    ) -> socks::BoxFuture<Result<Box<dyn socks::Stream>, u8>> {
        let server = self.server.clone();
        server.previews.accepted.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            if dest_is_local(&dest) {
                return to_machine(&server, &g, &dest).await;
            }
            if g.route == "remote" && !is_local_machine(&server, &g.machine) {
                if !PreviewConfig::load().allow_remote_egress {
                    return Err(reply::NOT_ALLOWED);
                }
                let link = server
                    .previews
                    .link(&server, &g.machine)
                    .ok_or(reply::NETWORK_UNREACHABLE)?;
                return link
                    .open_kind(&format!("egress:{}", dest.channel_target()))
                    .await
                    .map(|s| Box::new(s) as Box<dyn socks::Stream>)
                    .map_err(|_| reply::HOST_UNREACHABLE);
            }
            // Direct from this machine. A name that resolves to loopback still means the
            // profile's machine, never this machine's loopback.
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((dest.host(), dest.port))
                .await
                .map_err(|_| reply::HOST_UNREACHABLE)?
                .collect();
            if let Some(ip) = machine_local_answer(&addrs) {
                let d = Dest {
                    addr: match ip {
                        IpAddr::V4(v) => Addr::V4(v),
                        IpAddr::V6(v) => Addr::V6(v),
                    },
                    port: dest.port,
                };
                return to_machine(&server, &g, &d).await;
            }
            let mut last = reply::HOST_UNREACHABLE;
            for a in addrs {
                match tokio::time::timeout(
                    Duration::from_secs(10),
                    tokio::net::TcpStream::connect(a),
                )
                .await
                {
                    Ok(Ok(s)) => {
                        let _ = s.set_nodelay(true);
                        return Ok(Box::new(s) as Box<dyn socks::Stream>);
                    }
                    Ok(Err(e)) => last = socks::reply_for(&e),
                    Err(_) => last = reply::HOST_UNREACHABLE,
                }
            }
            Err(last)
        })
    }

    fn rejected(&self, peer: SocketAddr) {
        self.server
            .previews
            .rejected
            .fetch_add(1, Ordering::Relaxed);
        tracing::warn!(%peer, "SOCKS: rejected a connection not owned by a managed browser");
    }
}

/// Start the SOCKS listener once (127.0.0.1, ephemeral or the previous server's port).
pub async fn ensure_socks(server: &Arc<Server>, prefer: Option<u16>) -> std::io::Result<u16> {
    let mut g = server.previews.socks_port.lock().await;
    if let Some(p) = *g {
        return Ok(p);
    }
    let prefer = prefer.or_else(|| {
        server.with_core(|c| {
            c.store
                .kv_get("preview", "socks_port")
                .ok()
                .flatten()
                .and_then(|s| s.parse().ok())
        })
    });
    let l = match prefer {
        Some(p) => match socks::bind_loopback(p).await {
            Ok(l) => l,
            Err(_) => socks::bind_loopback(0).await?,
        },
        None => socks::bind_loopback(0).await?,
    };
    let port = l.local_addr()?.port();
    tokio::spawn(socks::serve(
        l,
        Arc::new(Router {
            server: server.clone(),
        }),
    ));
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.kv("preview", "socks_port", Some(port.to_string()));
        let _ = c.commit(tx);
    }
    tracing::info!(port, "preview SOCKS listener on 127.0.0.1");
    *g = Some(port);
    Ok(port)
}

// ---- API ------------------------------------------------------------------------------------

fn preview_json(c: &Core, p: &Preview) -> Value {
    let mut v = serde_json::to_value(p).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.insert(
            "pane_handle".into(),
            json!(
                p.pane
                    .as_ref()
                    .and_then(|x| c.pane(x))
                    .map(|x| x.handle.clone())
            ),
        );
        o.insert(
            "task_handle".into(),
            json!(
                p.task
                    .as_ref()
                    .and_then(|x| c.task(x))
                    .map(|x| x.handle.clone())
            ),
        );
    }
    v
}

pub(crate) fn find_local(server: &Server, target: &str) -> Result<Preview, RpcError> {
    server
        .with_core(|c| {
            c.model
                .previews
                .iter()
                .find(|p| p.id == target || p.handle == target)
                .cloned()
        })
        .ok_or_else(|| not_found("preview", target))
}

fn port_param(p: &Value) -> Result<u16, RpcError> {
    let port = u(p, "port")
        .or_else(|| s(p, "port").and_then(|x| x.parse().ok()))
        .ok_or_else(|| invalid("missing param `port`"))?;
    u16::try_from(port)
        .ok()
        .filter(|p| *p != 0)
        .ok_or_else(|| invalid("port must be 1-65535"))
}

/// `machine/v4` or `{machine, preview}` → (machine label, preview target).
fn split_target(server: &Server, p: &Value, target: &str) -> (String, String) {
    if let Some((m, t)) = target.split_once('/')
        && !m.is_empty()
        && !t.is_empty()
    {
        return (m.to_string(), t.to_string());
    }
    let m = s(p, "machine").unwrap_or("").to_string();
    let m = if is_local_machine(server, &m) {
        String::new()
    } else {
        m
    };
    (m, target.to_string())
}

/// Call a method on a remote machine's server over this server's link.
pub(crate) async fn remote_call(server: &Server, machine: &str, method: &str, params: Value) -> R {
    let unavailable =
        |e: String| err(ErrorKind::RemoteUnavailable, e).details(json!({"machine": machine}));
    let link = server
        .previews
        .link(server, machine)
        .ok_or_else(|| unavailable(format!("no link to {machine}")))?;
    let fut = async {
        let s = link
            .open()
            .await
            .map_err(|e| unavailable(format!("{e:#}")))?;
        let (rd, mut wr) = tokio::io::split(s);
        let req = vk_proto::rpc::Request::new(1, method, params);
        let mut line = serde_json::to_string(&req).map_err(|e| invalid(e.to_string()))?;
        line.push('\n');
        wr.write_all(line.as_bytes())
            .await
            .map_err(|e| unavailable(e.to_string()))?;
        let mut rd = tokio::io::BufReader::new(rd);
        let mut out = String::new();
        rd.read_line(&mut out)
            .await
            .map_err(|e| unavailable(e.to_string()))?;
        let resp: vk_proto::rpc::Response =
            serde_json::from_str(&out).map_err(|e| unavailable(format!("bad reply: {e}")))?;
        match (resp.result, resp.error) {
            (_, Some(e)) => Err(e),
            (Some(v), None) => Ok(v),
            _ => Err(unavailable("empty reply".into())),
        }
    };
    tokio::time::timeout(Duration::from_secs(20), fut)
        .await
        .map_err(|_| unavailable(format!("machine {machine} did not answer")))?
}

/// `http(s)://host[:port]/…` → host as the browser parses it (WHATWG; brackets stripped), or
/// None if not an openable http(s) URL (09 §7: no other schemes, no credentials).
pub(crate) fn url_host(url: &str) -> Option<String> {
    vk_browser::policy::parse_open_url(url).and_then(|u| vk_browser::policy::host_of(&u))
}

/// An openable URL in the canonical form Chromium will load, and whether its host is this
/// machine's loopback. Every open/navigate passes this canonical string on, never the raw input.
pub(crate) fn canonical_open_url(raw: &str) -> Option<(String, bool)> {
    let u = vk_browser::policy::parse_open_url(raw)?;
    let lb = vk_browser::policy::is_loopback_url(&u);
    Some((u.as_str().to_string(), lb))
}

pub(crate) fn open_url_of(p: &Preview) -> String {
    let u = p.url.replace("://0.0.0.0", "://localhost");
    match canonical_open_url(&u) {
        Some((c, _)) => c,
        None => format!("http://localhost:{}{}", p.port, p.path),
    }
}

/// Pane scope (09 §5.2): agents open, create and manage previews and browser panes on their own
/// machine only. Called from the shared authorization point (`api::authorize`) for every
/// method that can pick a machine, so no dispatch path (`browser.pane.create` is dispatched
/// before `preview.*`) can skip it.
pub(crate) fn authorize_pane_machine(
    server: &Server,
    ctx: &Ctx,
    method: &str,
    p: &Value,
) -> Result<(), RpcError> {
    if ctx.pane_scope.is_none()
        || !matches!(
            method,
            "preview.open"
                | "preview.forget"
                | "preview.dismiss"
                | "preview.promote"
                | "browser.pane.create"
        )
    {
        return Ok(());
    }
    let remote = s(p, "machine").is_some_and(|m| !m.is_empty() && !is_local_machine(server, m))
        || s(p, "preview").is_some_and(|t| !t.contains("://") && t.contains('/'));
    if remote {
        return Err(err(
            ErrorKind::PermissionDenied,
            format!("{method} on another machine is not allowed from a pane"),
        )
        .details(json!({"scope": "pane"})));
    }
    Ok(())
}

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    if method.starts_with("browser.")
        && let Some(r) = crate::browser_pane::api(server, ctx, method, p).await
    {
        return Some(r);
    }
    if !method.starts_with("preview.") {
        return None;
    }
    Some(match method {
        "preview.declare" => declare_api(server, ctx, p).await,
        "preview.list" => list(server, p).await,
        "preview.get" => {
            let t = match s(p, "preview") {
                Some(t) => t,
                None => return Some(Err(invalid("missing param `preview`"))),
            };
            let (m, t) = split_target(server, p, t);
            if !m.is_empty() {
                return Some(remote_call(server, &m, "preview.get", json!({"preview": t})).await);
            }
            find_local(server, &t)
                .map(|x| json!({"preview": server.with_core(|c| preview_json(c, &x))}))
        }
        "preview.promote" => promote(server, ctx, p).await,
        "preview.forget" | "preview.dismiss" => forget(server, p).await,
        "preview.url" => {
            let t = match s(p, "preview") {
                Some(t) => t,
                None => return Some(Err(invalid("missing param `preview`"))),
            };
            let (m, t) = split_target(server, p, t);
            let pv = if m.is_empty() {
                find_local(server, &t)
            } else {
                remote_call(server, &m, "preview.get", json!({"preview": t}))
                    .await
                    .and_then(|v| {
                        serde_json::from_value::<Preview>(v["preview"].clone())
                            .map_err(|e| invalid(e.to_string()))
                    })
            };
            let machine = if m.is_empty() { "local".to_string() } else { m };
            match pv {
                Ok(x) => {
                    // No side effects: `proxy_url` only for an origin that already exists,
                    // and never with a credential.
                    let proxy_url =
                        crate::preview_fabric::existing_proxy_url(server, ctx, &machine, &x).await;
                    Ok(
                        json!({"remote_url": x.url, "profile_url": open_url_of(&x), "proxy_url": proxy_url}),
                    )
                }
                Err(e) => Err(e),
            }
        }
        "preview.mirror" => crate::preview_fabric::mirror(server, ctx, p).await,
        "preview.unmirror" => crate::preview_fabric::unmirror(server, ctx, p),
        "preview.open" => open(server, ctx, p).await,
        "preview.status" => {
            let port = *server.previews.socks_port.lock().await;
            let links: Vec<(String, Link)> = server
                .previews
                .links
                .lock()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let mut lv = Vec::new();
            for (m, l) in links {
                let bytes = l.bytes().await;
                lv.push(json!({"machine": m, "connected": bytes.is_some(), "bytes_in": bytes.map(|b| b.0), "bytes_out": bytes.map(|b| b.1), "rtt_ms": l.rtt_ms().await}));
            }
            let browsers: Vec<Value> = server
                .previews
                .browsers_snapshot()
                .into_iter()
                .map(|b| json!({"profile": b.profile, "machine": b.machine, "route": b.route, "pid": b.root_pid, "running": alive(b.root_pid, b.start)}))
                .collect();
            Ok(json!({
                "socks_port": port,
                "browsers": browsers,
                "links": lv,
                "accepted": server.previews.accepted.load(Ordering::Relaxed),
                "rejected": server.previews.rejected.load(Ordering::Relaxed),
                "proxy": crate::preview_fabric::proxy_status(server, ctx).await,
                "mirrors": crate::preview_fabric::mirrors_status(server),
                "available_browsers": available_browsers(&PreviewConfig::load()),
                "browser_install": server.agent_browser.install_job().map(|j| j.json()),
            }))
        }
        "preview.profile" if s(p, "action").unwrap_or("list") == "list" => Ok(profile_list(server)),
        "preview.profile.list" | "preview.profile_list" => Ok(profile_list(server)),
        "preview.profile" | "preview.profile.reset" | "preview.profile_reset" => {
            if method == "preview.profile" && s(p, "action") != Some("reset") {
                return Some(Err(invalid(
                    "vibeke preview profile list | reset <profile>",
                )));
            }
            if ctx.pane_scope.is_some() {
                return Some(Err(err(
                    ErrorKind::PermissionDenied,
                    "preview.profile.reset is not allowed from a pane",
                )
                .details(json!({"scope": "pane"}))));
            }
            profile_reset(server, p)
        }
        "preview.test.register_browser" if server.previews.test_hooks => {
            test_register(server, p).await
        }
        _ => Err(err(
            ErrorKind::MethodNotFound,
            format!("unknown method {method}"),
        )),
    })
}

/// How a pane-scoped declare treats a port no pane's process listens on (06 B5): anything
/// could be listening there (a database, a local admin service), and the declared port joins
/// the pane's agent-browser loopback allowlist.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Unattributed {
    /// Needs the user's confirmation unless it was approved for this pane already.
    Ask,
    /// Run the checks only; change nothing (validating an approval request).
    Check,
    /// A port Vibeke leased to the task itself.
    Leased,
}

enum Declared {
    Done(Value),
    NeedsConfirmation,
}

/// Default wait for the user's decision on a confirmation (as `auth.elevate`).
const CONFIRM_WAIT_MS: u64 = 120_000;

fn confirmation_required(port: u16) -> RpcError {
    err(
        ErrorKind::PermissionDenied,
        format!(
            "preview.declare from a pane on port {port}, which no pane's process listens on, needs the user's confirmation; call preview.declare from the pane to ask"
        ),
    )
    .details(json!({"scope": "pane", "reason": "confirmation_required", "port": port}))
}

/// Whether the user approved `port` for `pane`'s current process.
fn port_approved(server: &Server, pane: &str, port: u16) -> bool {
    let pid = server.with_core(|c| c.pane(pane).map(|p| p.child_pid));
    let mut m = server.previews.approved_ports.lock().unwrap();
    match (m.get(pane), pid) {
        (Some((at, ports)), Some(now)) if *at == now => ports.contains(&port),
        (Some(_), _) => {
            // The pane restarted or is gone: its approvals ended with it.
            m.remove(pane);
            false
        }
        _ => false,
    }
}

fn remember_port(server: &Server, pane: &str, child_pid: Option<u32>, port: u16) {
    let mut m = server.previews.approved_ports.lock().unwrap();
    let e = m
        .entry(pane.to_string())
        .or_insert_with(|| (child_pid, HashSet::new()));
    if e.0 != child_pid {
        *e = (child_pid, HashSet::new());
    }
    e.1.insert(port);
}

/// What a pane-scoped caller can show for a port ([`pane_port_claim`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PortClaim {
    /// Its own process tree (or that of a pane it created; with `task_peers`, of a pane in its
    /// task) listens there, or the user approved the port for the pane's process.
    Verified,
    /// Another pane's process listens there.
    Foreign,
    /// No pane's process listens there and the user has not approved it.
    Unattributed,
}

/// The verified claim of pane `caller` on `port` (06 B5): from the listener's process tree or a
/// user approval, never from which pane printed a URL. Blocking (procinfo walk).
pub(crate) fn pane_port_claim(
    server: &Server,
    caller: &str,
    port: u16,
    task_peers: bool,
) -> PortClaim {
    if let Some(l) = listener_pane(server, port) {
        let ok = server.with_core(|c| {
            let task = task_of_pane(c, caller);
            l == caller
                || c.pane(&l)
                    .is_some_and(|q| q.created_by == format!("agent:{caller}"))
                || (task_peers && task.is_some() && task_of_pane(c, &l) == task)
        });
        return if ok {
            PortClaim::Verified
        } else {
            PortClaim::Foreign
        };
    }
    if port_approved(server, caller, port) {
        PortClaim::Verified
    } else {
        PortClaim::Unattributed
    }
}

/// Promote the local suggestion `x` (Suggested → Up) on behalf of `ctx`, committing the change.
/// From a pane, confirming a suggestion needs a verified claim on its port: a URL the pane
/// printed attributes the suggestion to it, but whatever listens there may be another program.
pub(crate) fn promote_for(server: &Server, ctx: &Ctx, x: &mut Preview) -> Result<bool, RpcError> {
    if x.status != PreviewStatus::Suggested {
        return Ok(false);
    }
    if let Some(caller) = &ctx.pane_scope {
        match pane_port_claim(server, caller, x.port, true) {
            PortClaim::Verified => {}
            PortClaim::Foreign => {
                return Err(err(
                    ErrorKind::PermissionDenied,
                    format!(
                        "confirming the suggestion on port {} is not allowed from a pane (the port is not your listener)",
                        x.port
                    ),
                )
                .details(json!({"scope": "pane", "reason": "foreign_preview", "port": x.port})));
            }
            PortClaim::Unattributed => return Err(confirmation_required(x.port)),
        }
    }
    let changed = lifecycle::promote(x);
    if changed {
        commit_previews(server, vec![(x.clone(), Some("preview.up"))]);
    }
    Ok(changed)
}

/// `preview.declare` for synchronous callers: a pane-scoped declare that needs the user's
/// confirmation is refused (`confirmation_required`).
pub(crate) fn declare(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    match declare_with(server, ctx, p, Unattributed::Ask)? {
        Declared::Done(v) => Ok(v),
        Declared::NeedsConfirmation => Err(confirmation_required(port_param(p)?)),
    }
}

/// `preview.declare` of a port leased to the task by Vibeke (`task.create` repo previews).
pub(crate) fn declare_leased(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    match declare_with(server, ctx, p, Unattributed::Leased)? {
        Declared::Done(v) => Ok(v),
        Declared::NeedsConfirmation => Err(confirmation_required(port_param(p)?)),
    }
}

/// The `preview.declare` method. From a pane, a port no pane's process listens on is declared
/// only after the user confirms it outside the pane, through the approved-call machinery
/// (`auth.approve`, decided with `auth.approve.decide`): the call waits for the decision
/// (`timeout_ms`, default 2 min), `wait: false` returns the pending request, and `request`
/// resumes waiting on it. Approval also remembers the port for the pane until its process
/// restarts; denial or timeout refuses.
async fn declare_api(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    async fn approve(server: &Arc<Server>, ctx: &Ctx, q: Value) -> R {
        crate::approve::api(server, ctx, "auth.approve", &q)
            .await
            .unwrap_or_else(|| Err(crate::api::internal("auth.approve is unavailable")))
    }
    let mut wait = json!({"timeout_ms": u(p, "timeout_ms").unwrap_or(CONFIRM_WAIT_MS)});
    if let Some(w) = b(p, "wait") {
        wait["wait"] = json!(w);
    }
    if ctx.pane_scope.is_some()
        && let Some(id) = s(p, "request")
    {
        wait["request"] = json!(id);
        return approve(server, ctx, wait).await;
    }
    match declare_with(server, ctx, p, Unattributed::Ask)? {
        Declared::Done(v) => Ok(v),
        Declared::NeedsConfirmation => {
            let mut params = p.clone();
            if let Some(o) = params.as_object_mut() {
                for k in ["request", "wait", "timeout_ms", "reason"] {
                    o.remove(k);
                }
            }
            wait["method"] = json!("preview.declare");
            wait["params"] = params;
            if let Some(r) = s(p, "reason") {
                wait["reason"] = json!(r);
            }
            approve(server, ctx, wait).await
        }
    }
}

/// `auth.approve {method: "preview.declare"}` (crate::approve): validate the pane's ask as the
/// declare would and summarise it from server facts. Returns (frozen params, summary, facts).
pub(crate) async fn approval_request(
    server: &Arc<Server>,
    ctx: &Ctx,
    me: &str,
    params: &Value,
) -> Result<(Value, String, Value), RpcError> {
    if ctx.pane_scope.as_deref() != Some(me) {
        return Err(invalid("preview.declare approvals are asked from the pane"));
    }
    let port = port_param(params)?;
    // Every refusal a direct declare would give applies to the ask too.
    declare_with(server, ctx, params, Unattributed::Check)?;
    let mut frozen = json!({"port": port});
    for k in ["path", "scheme", "label", "pane", "task", "tls_origin"] {
        if let Some(v) = params.get(k).filter(|v| !v.is_null()) {
            frozen[k] = v.clone();
        }
    }
    let target = match s(params, "pane") {
        Some(t) => resolve_pane(server, ctx, Some(t))?.id,
        None => me.to_string(),
    };
    frozen["pane"] = json!(target);
    let handle = |id: &str| {
        server
            .with_core(|c| c.pane(id).map(|x| x.handle.clone()))
            .unwrap_or_else(|| id.to_string())
    };
    let (me_handle, target_handle) = (handle(me), handle(&target));
    let listening = probe::tcp_alive(None, port).await;
    let summary = format!(
        "Let pane {me_handle} declare a preview on localhost:{port}{}. No pane's process listens on that port ({}); once declared, the agent browser of pane {target_handle} can reach it until the pane restarts",
        if target == me {
            String::new()
        } else {
            format!(" for pane {target_handle}")
        },
        if listening {
            "another program on this machine is listening there now"
        } else {
            "nothing listens there yet"
        }
    );
    let facts =
        json!({"port": port, "listening": listening, "pane": target, "pane_handle": target_handle});
    Ok((frozen, summary, facts))
}

/// Run an approved `preview.declare` for `pane` (crate::approve): remember the port for the
/// pane's process, then declare with the pane's own scope, so every other check still applies.
pub(crate) fn declare_approved(
    server: &Arc<Server>,
    approver: &Ctx,
    pane: &str,
    child_pid: Option<u32>,
    params: &Value,
) -> R {
    let port = port_param(params)?;
    remember_port(server, pane, child_pid, port);
    let ctx = Ctx {
        client_id: approver.client_id.clone(),
        kind: approver.kind.clone(),
        pane_scope: Some(pane.to_string()),
        remote: approver.remote,
    };
    match declare_with(server, &ctx, params, Unattributed::Ask)? {
        Declared::Done(v) => Ok(v),
        Declared::NeedsConfirmation => Err(err(
            ErrorKind::Conflict,
            "the pane restarted since it asked; nothing was declared",
        )),
    }
}

fn declare_with(
    server: &Arc<Server>,
    ctx: &Ctx,
    p: &Value,
    unattributed: Unattributed,
) -> Result<Declared, RpcError> {
    let port = port_param(p)?;
    let pane = match s(p, "pane") {
        Some(t) => Some(resolve_pane(server, ctx, Some(t))?.id),
        None => ctx.pane_scope.clone(),
    };
    let path = vk_tasks::normalize_preview_path(s(p, "path").unwrap_or("/")).map_err(invalid)?;
    let scheme = s(p, "scheme").unwrap_or("http");
    if scheme != "http" && scheme != "https" {
        return Err(invalid("scheme must be http or https"));
    }
    let task = match s(p, "task") {
        Some(t) => Some(
            server
                .with_core(|c| c.task(t).map(|x| x.id.clone()))
                .ok_or_else(|| not_found("task", t))?,
        ),
        None => pane
            .as_ref()
            .and_then(|x| server.with_core(|c| task_of_pane(c, x))),
    };
    let label = s(p, "label").map(str::to_string);
    let url = format!("{scheme}://localhost:{port}{path}");
    let existing = server.with_core(|c| {
        c.model
            .previews
            .iter()
            .find(|x| x.port == port && x.status != PreviewStatus::Gone)
            .cloned()
    });
    // Pane scope (06 B5): a pane declares only for itself, and never takes over a preview that
    // belongs to another pane or task: the browser pre-check, the session proxy and the Fetch
    // layer all trust this ownership record. Ownership moves only from full scope.
    let pane_scoped = ctx.pane_scope.is_some();
    if let Some(caller) = &ctx.pane_scope {
        let refuse = |why: &str| {
            Err(err(
                ErrorKind::PermissionDenied,
                format!("preview.declare is not allowed from a pane ({why})"),
            )
            .details(json!({"scope": "pane", "reason": "foreign_preview", "port": port})))
        };
        let is_mine = |c: &Core, x: &str| {
            x == caller
                || c.pane(x)
                    .is_some_and(|q| q.created_by == format!("agent:{caller}"))
        };
        let (caller_task, pane_ok, owns_existing) = server.with_core(|c| {
            let t = task_of_pane(c, caller);
            let pane_ok = pane.as_ref().is_none_or(|x| is_mine(c, x));
            let owns = existing
                .as_ref()
                .is_some_and(|x| pane_owns_preview(c, caller, t.as_deref(), x));
            (t, pane_ok, owns)
        });
        if !pane_ok {
            return refuse("the pane is not yours");
        }
        if task.is_some() && task != caller_task {
            return refuse("the task is not yours");
        }
        let unowned = existing
            .as_ref()
            .is_none_or(|x| x.pane.is_none() && x.task.is_none());
        if !owns_existing && !unowned {
            return refuse("the preview on that port belongs to another pane or task");
        }
        if owns_existing {
            // Output discovery attributes a printed URL to the printing pane, which proves
            // nothing about who listens there: ownership counts only with the caller's own
            // listener, the user's approval of the port, or a preview already confirmed as
            // declared (by a previous declare or by the user).
            let confirmed = existing.as_ref().is_some_and(|x| {
                x.source == PreviewSource::Declared && x.status != PreviewStatus::Suggested
            });
            match pane_port_claim(server, caller, port, true) {
                PortClaim::Verified => {}
                PortClaim::Foreign => return refuse("the port is not your listener"),
                PortClaim::Unattributed if confirmed || unattributed == Unattributed::Leased => {}
                PortClaim::Unattributed => return Ok(Declared::NeedsConfirmation),
            }
        } else {
            // A new or machine-level preview: never on another pane's listener, and an
            // existing machine-level one only on the caller's own.
            let listener = listener_pane(server, port);
            let mine = listener
                .as_deref()
                .map(|l| server.with_core(|c| is_mine(c, l)));
            match (existing.is_some(), mine) {
                (_, Some(true)) => {}
                // No pane's process listens there (nothing yet, or a program outside every
                // pane): the user confirms it once for this pane's process.
                (false, None) => {
                    if unattributed != Unattributed::Leased && !port_approved(server, caller, port)
                    {
                        return Ok(Declared::NeedsConfirmation);
                    }
                }
                _ => return refuse("the port is not your listener"),
            }
        }
    }
    if unattributed == Unattributed::Check {
        return Ok(Declared::Done(Value::Null));
    }
    server.previews.dismissed.lock().unwrap().remove(&port);
    let pv = match existing {
        Some(mut x) => {
            lifecycle::promote(&mut x);
            x.source = PreviewSource::Declared;
            if s(p, "path").is_some() || x.path == "/" {
                x.path = path;
                x.url = url;
            }
            x.scheme = scheme.into();
            if label.is_some() {
                x.label = label;
            }
            // From a pane, only an unowned preview (on the caller's own listener, checked
            // above) gets an owner; an owned one keeps its attribution.
            let reattribute = !pane_scoped || (x.pane.is_none() && x.task.is_none());
            if reattribute && pane.is_some() {
                x.pane = pane;
            }
            if reattribute && task.is_some() {
                x.task = task;
            }
            commit_previews(server, vec![(x.clone(), Some("preview.declared"))]);
            x
        }
        None => {
            let now = server.previews.now();
            let mut x = new_preview(server, port, PreviewSource::Declared, now);
            x.status = PreviewStatus::Declared;
            x.path = path;
            x.url = url;
            x.scheme = scheme.into();
            x.label = label;
            x.pane = pane;
            x.task = task;
            let created = create_previews(server, vec![(x, "preview.declared")]);
            let x = created.into_iter().next().ok_or_else(|| {
                err(
                    ErrorKind::Conflict,
                    "a preview on that port appeared meanwhile; retry",
                )
            })?;
            // Probe right away so `declared` turns `up`/`down` without waiting for the tick.
            let srv = server.clone();
            let id = x.id.clone();
            tokio::spawn(async move {
                let present = probe::tcp_alive(None, port).await;
                let cur = srv.with_core(|c| c.model.previews.iter().find(|p| p.id == id).cloned());
                if let Some(mut q) = cur {
                    let now = srv.previews.now();
                    if let Some(ev) = lifecycle::observe(&mut q, present, now) {
                        commit_previews(&srv, vec![(q, Some(ev))]);
                    }
                }
            });
            x
        }
    };
    if let Some(t) = b(p, "tls_origin") {
        crate::preview_fabric::set_tls_origin(server, &pv.id, t);
    }
    Ok(Declared::Done(
        json!({"preview": server.with_core(|c| preview_json(c, &pv)), "cursor": crate::api::cursor(server, None)}),
    ))
}

async fn list(server: &Arc<Server>, p: &Value) -> R {
    let machine = s(p, "machine").unwrap_or("");
    if !is_local_machine(server, machine) && machine != "*" {
        let mut q = p.clone();
        if let Some(o) = q.as_object_mut() {
            o.remove("machine");
        }
        return remote_call(server, machine, "preview.list", q).await;
    }
    let status = s(p, "status");
    let all = b(p, "all").unwrap_or(false) || matches!(status, Some("all"));
    let task = s(p, "task").map(str::to_string);
    let pane = s(p, "pane").map(str::to_string);
    Ok(server.with_core(|c| {
        let task_id = task
            .as_ref()
            .map(|t| c.task(t).map(|x| x.id.clone()).unwrap_or(t.clone()));
        let pane_id = pane
            .as_ref()
            .map(|t| c.pane(t).map(|x| x.id.clone()).unwrap_or(t.clone()));
        let v: Vec<Value> = c
            .model
            .previews
            .iter()
            .filter(|x| match status {
                Some("all") | None => all || x.status != PreviewStatus::Suggested,
                Some(st) => x.status.as_str() == st,
            })
            .filter(|x| task_id.as_ref().is_none_or(|t| x.task.as_ref() == Some(t)))
            .filter(|x| pane_id.as_ref().is_none_or(|t| x.pane.as_ref() == Some(t)))
            .map(|x| preview_json(c, x))
            .collect();
        json!({"previews": v, "machine": server.opts.machine})
    }))
}

async fn promote(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let t = s(p, "preview").ok_or_else(|| invalid("missing param `preview`"))?;
    let (m, t) = split_target(server, p, t);
    if !m.is_empty() {
        return remote_call(server, &m, "preview.promote", json!({"preview": t})).await;
    }
    let mut x = find_local(server, &t)?;
    promote_for(server, ctx, &mut x)?;
    Ok(json!({"preview": server.with_core(|c| preview_json(c, &x))}))
}

async fn forget(server: &Arc<Server>, p: &Value) -> R {
    let t = s(p, "preview").ok_or_else(|| invalid("missing param `preview`"))?;
    let (m, t) = split_target(server, p, t);
    if !m.is_empty() {
        // Revoke this server's origins for it first: the old link must not reach whatever
        // listens on that remote port next, even if the remote call fails.
        crate::preview_fabric::forget_remote_routes(server, &m, &t);
        return remote_call(server, &m, "preview.forget", json!({"preview": t})).await;
    }
    let mut x = find_local(server, &t)?;
    // Don't re-suggest the same listener; a new process on the port is a new suggestion.
    server
        .previews
        .dismissed
        .lock()
        .unwrap()
        .insert(x.port, x.pid);
    lifecycle::retire(&mut x);
    crate::preview_fabric::forget_route(server, "local", &x.id).await;
    commit_previews(server, vec![(x, Some("preview.gone"))]);
    Ok(json!({}))
}

pub(crate) fn profiles_root() -> PathBuf {
    browser::profiles_root(&crate::paths::state_root())
}

pub(crate) fn profile_for(cfg: &PreviewConfig, machine_label: &str, task: Option<&str>) -> String {
    match (cfg.profile_scope.as_str(), task) {
        ("task", Some(t)) => browser::profile_name(&format!("task-{t}")),
        _ => browser::profile_name(machine_label),
    }
}

pub(crate) async fn open(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let cfg = PreviewConfig::load();
    let explicit_view = b(p, "window").is_some() || s(p, "split").is_some();
    // Proxy mode (B4): `--proxy`, `mode: "proxy"`, or `preview.mode = "proxy"` without an
    // explicit window/pane request.
    let proxy = b(p, "proxy").unwrap_or(false)
        || s(p, "mode") == Some("proxy")
        || (cfg.mode == "proxy" && !explicit_view && s(p, "mode").is_none());
    let window = !proxy
        && (b(p, "window").unwrap_or(false)
            || s(p, "mode") == Some("window")
            || (cfg.mode == "window" && s(p, "split").is_none()));
    // Resolve what to open and on which machine (`vibeke preview open <url>` passes the URL
    // positionally as `preview`).
    let url_param = s(p, "url").or_else(|| {
        s(p, "preview").filter(|t| t.starts_with("http://") || t.starts_with("https://"))
    });
    let preview_param = s(p, "preview").filter(|_| s(p, "url").is_none() && url_param.is_none());
    let (machine, preview, url) = if let Some(t) = preview_param {
        let (m, t) = split_target(server, p, t);
        let pv = if m.is_empty() {
            let mut x = find_local(server, &t)?;
            promote_for(server, ctx, &mut x)?;
            x
        } else {
            let v = remote_call(server, &m, "preview.promote", json!({"preview": t})).await?;
            serde_json::from_value::<Preview>(v["preview"].clone())
                .map_err(|e| invalid(format!("remote preview: {e}")))?
        };
        let url = open_url_of(&pv);
        (m, Some(pv), url)
    } else if let Some(u) = url_param {
        let m = s(p, "machine").unwrap_or("").to_string();
        let m = if is_local_machine(server, &m) {
            String::new()
        } else {
            m
        };
        (m, None, u.to_string())
    } else {
        return Err(invalid("preview.open needs `preview` or `url`"));
    };
    if proxy {
        let pv = preview
            .ok_or_else(|| invalid("proxy mode opens a preview (v4 or devbox/v4), not a URL"))?;
        return crate::preview_fabric::open_proxy(server, ctx, p, &cfg, &machine, &pv).await;
    }
    if !window {
        return crate::browser_pane::open_pane(server, ctx, p, &machine, preview.as_ref(), &url)
            .await;
    }
    // Open-URL rules (09 §7): http(s) only; from a pane, only loopback. The check and the
    // browser both use the canonical (WHATWG) form.
    let (url, loopback) =
        canonical_open_url(&url).ok_or_else(|| invalid("only http(s) URLs can be opened"))?;
    if ctx.pane_scope.is_some() && !loopback {
        return Err(err(
            ErrorKind::PermissionDenied,
            "from a pane, preview.open only opens loopback URLs",
        )
        .details(json!({"scope": "pane"})));
    }
    let local = machine.is_empty();
    let machine_label = if local {
        "local".to_string()
    } else {
        machine.clone()
    };
    if local && cfg.local_browser == "default" {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        std::process::Command::new(opener)
            .arg(&url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| err(ErrorKind::Unsupported, format!("{opener}: {e}")))?;
        return Ok(json!({"opened_in": "default_browser", "url": url}));
    }
    let task = preview.as_ref().and_then(|x| x.task.clone());
    // The browser pane's window handover passes its profile (task profiles included).
    let mut profile = s(p, "profile")
        .filter(|x| ctx.pane_scope.is_none() && browser::valid_profile_name(x))
        .map(str::to_string)
        .unwrap_or_else(|| profile_for(&cfg, &machine_label, task.as_deref()));
    let route = if cfg.profile_route == "remote" {
        "remote"
    } else {
        "loopback"
    };
    let firefox = cfg.wants_firefox();
    let bin = if firefox {
        browser::find_firefox(Some(cfg.browser.as_str())).ok_or_else(|| {
            err(
                ErrorKind::Unsupported,
                "Firefox not found (set [preview] browser = \"/path/to/firefox\")",
            )
            .details(json!({"fallback": "profile_browser = \"auto\" (Chromium)"}))
        })?
    } else {
        let installed = vk_browser::install::installed_full(&crate::agent_browser::install_root());
        browser::find_browser_with(
            Some(cfg.browser.as_str()).filter(|b| !b.is_empty()),
            installed.as_deref(),
        )
        .ok_or_else(|| {
            err(
                ErrorKind::Unsupported,
                "no browser that can open a window on this machine: run `vibeke browser install --full` (or set [preview] browser = \"/path/to/chrome\")",
            )
            .details(json!({"fallback": "vibeke browser install --full"}))
        })?
    };
    if firefox {
        // A Firefox profile never shares a directory with the Chromium profile (browser panes
        // keep using Chromium on the plain name).
        let mut f = profile.clone();
        f.truncate(56);
        profile = format!("{f}-firefox");
    }
    let socks_port = if local {
        None
    } else {
        Some(
            ensure_socks(server, None)
                .await
                .map_err(|e| err(ErrorKind::Internal, format!("SOCKS listener: {e}")))?,
        )
    };
    let root = profiles_root();
    let dir = root.join(&profile);
    browser::check_profile_dir(&dir, &root).map_err(|e| invalid(format!("{e:#}")))?;
    let headless = server.previews.test_hooks && b(p, "headless").unwrap_or(false);
    let log = server.paths.logs().join(format!("browser-{profile}.log"));
    // A headless browser-pane Chromium on this profile hands it over to the window (06 B3.3:
    // a profile can be open in only one process).
    if server
        .previews
        .running(&profile)
        .is_some_and(|r| r.headless)
    {
        crate::browser_pane::release_profile(server, &profile).await;
    }
    let running = server.previews.running(&profile);
    let reused = running.is_some();
    if let Some(r) = &running
        && (r.machine != machine_label || r.route != route)
    {
        return Err(err(
            ErrorKind::Conflict,
            format!(
                "profile {profile} is open for {} ({}); close that browser first",
                r.machine, r.route
            ),
        ));
    }
    let spec = browser::LaunchSpec {
        bin: bin.path.clone(),
        profile_dir: dir.clone(),
        socks_port,
        url: url.clone(),
        headless,
        extra_args: vec![],
        firefox,
        reuse: reused,
    };
    let mut child = browser::launch(&spec, Some(&log))
        .map_err(|e| err(ErrorKind::Unsupported, format!("{e:#}")))?;
    let pid = child.id().unwrap_or(0);
    let root_pid = match running {
        Some(r) => {
            // The new process hands the URL to the running instance and exits.
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
            r.root_pid
        }
        None => {
            let start = vk_hold::procinfo::info(pid).map(|i| i.start).unwrap_or(0);
            server.previews.browsers.lock().unwrap().insert(
                profile.clone(),
                ManagedBrowser {
                    profile: profile.clone(),
                    machine: machine_label.clone(),
                    route: route.into(),
                    root_pid: pid,
                    start,
                    dir: dir.to_string_lossy().into_owned(),
                    headless: false,
                },
            );
            persist_browsers(server);
            let srv = server.clone();
            let prof = profile.clone();
            tokio::spawn(async move {
                let _ = child.wait().await;
                let removed = {
                    let mut b = srv.previews.browsers.lock().unwrap();
                    if b.get(&prof).is_some_and(|x| x.root_pid == pid) {
                        b.remove(&prof);
                        true
                    } else {
                        false
                    }
                };
                if removed {
                    persist_browsers(&srv);
                }
            });
            pid
        }
    };
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        let subj = match &preview {
            Some(x) if local => subject(x),
            Some(x) => {
                json!({"machine": machine_label, "preview_handle": x.handle, "preview": x.id})
            }
            None => json!({"machine": machine_label}),
        };
        tx.event(
            "preview.opened",
            subj,
            json!({"url": url, "opened_in": "window", "profile": profile, "browser": bin.kind}),
        );
        let _ = server.commit(&mut c, tx);
    }
    Ok(json!({
        "opened_in": "window",
        "url": url,
        "machine": machine_label,
        "profile": profile,
        "profile_dir": dir,
        "browser": bin.path,
        "browser_kind": bin.kind,
        "pid": root_pid,
        "reused": reused,
        "socks_port": socks_port,
        "route": if local { "none" } else { route },
    }))
}

fn dir_size(p: &std::path::Path, budget: &mut u32) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            if *budget == 0 {
                break;
            }
            *budget -= 1;
            match e.file_type() {
                Ok(t) if t.is_dir() => total += dir_size(&e.path(), budget),
                Ok(_) => total += e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => {}
            }
        }
    }
    total
}

fn profile_list(server: &Server) -> Value {
    let root = profiles_root();
    let mut names: Vec<String> = std::fs::read_dir(&root)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| browser::valid_profile_name(n))
                .collect()
        })
        .unwrap_or_default();
    for b in server.previews.browsers_snapshot() {
        if !names.contains(&b.profile) {
            names.push(b.profile);
        }
    }
    names.sort();
    let v: Vec<Value> = names
        .into_iter()
        .map(|n| {
            let running = server.previews.running(&n);
            let mut budget = 20_000;
            json!({
                "name": n,
                "path": root.join(&n),
                "running": running.is_some(),
                "pid": running.as_ref().map(|r| r.root_pid),
                "machine": running.as_ref().map(|r| r.machine.clone()),
                "route": running.as_ref().map(|r| r.route.clone()),
                "bytes": dir_size(&root.join(&n), &mut budget),
            })
        })
        .collect();
    json!({"profiles": v, "root": root})
}

fn profile_reset(server: &Server, p: &Value) -> R {
    let name = s(p, "profile")
        .or_else(|| s(p, "machine"))
        .ok_or_else(|| invalid("missing param `profile`"))?;
    if !browser::valid_profile_name(name) {
        return Err(invalid(format!("invalid profile name {name:?}")));
    }
    if let Some(r) = server.previews.running(name) {
        return Err(err(
            ErrorKind::Conflict,
            format!(
                "profile {name} is open in a browser (pid {}); close it first",
                r.root_pid
            ),
        ));
    }
    let root = profiles_root();
    let dir = root.join(name);
    browser::check_profile_dir(&dir, &root).map_err(|e| invalid(format!("{e:#}")))?;
    let existed = dir.exists();
    if existed {
        std::fs::remove_dir_all(&dir).map_err(|e| err(ErrorKind::Internal, e.to_string()))?;
    }
    Ok(json!({"profile": name, "removed": existed}))
}

/// `VIBEKE_TEST_HOOKS=1` only: treat `pid`'s process tree as the managed browser of
/// `profile` (routes to `machine`). Starts the SOCKS listener.
async fn test_register(server: &Arc<Server>, p: &Value) -> R {
    let pid = u(p, "pid").ok_or_else(|| invalid("missing param `pid`"))? as u32;
    let profile = s(p, "profile").unwrap_or("test").to_string();
    let machine = s(p, "machine").unwrap_or("local").to_string();
    let route = s(p, "route").unwrap_or("loopback").to_string();
    let start = vk_hold::procinfo::info(pid).map(|i| i.start).unwrap_or(0);
    let port = ensure_socks(server, None)
        .await
        .map_err(|e| err(ErrorKind::Internal, e.to_string()))?;
    server.previews.browsers.lock().unwrap().insert(
        profile.clone(),
        ManagedBrowser {
            profile,
            machine,
            route,
            root_pid: pid,
            start,
            dir: String::new(),
            headless: false,
        },
    );
    Ok(json!({"socks_port": port}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapped_dns_answers_route_to_the_profiles_machine() {
        let sa = |s: &str| -> SocketAddr { s.parse().unwrap() };
        assert_eq!(
            machine_local_answer(&[sa("[::ffff:127.0.0.1]:80")]),
            Some("127.0.0.1".parse().unwrap())
        );
        assert_eq!(
            machine_local_answer(&[sa("93.184.216.34:80"), sa("[::ffff:127.0.0.2]:80")]),
            Some("127.0.0.2".parse().unwrap())
        );
        assert_eq!(
            machine_local_answer(&[sa("[::ffff:0.0.0.0]:80")]),
            Some("0.0.0.0".parse().unwrap())
        );
        assert_eq!(
            machine_local_answer(&[sa("[::1]:80")]),
            Some("::1".parse().unwrap())
        );
        assert_eq!(
            machine_local_answer(&[sa("[::ffff:93.184.216.34]:80")]),
            None
        );
        assert_eq!(machine_local_answer(&[sa("10.0.0.1:80")]), None);
        // Literal destinations: a mapped unspecified/loopback address is local too.
        let d = |addr: Addr| Dest { addr, port: 80 };
        assert!(dest_is_local(&d(Addr::V6(
            "::ffff:0.0.0.0".parse().unwrap()
        ))));
        assert!(dest_is_local(&d(Addr::V6(
            "::ffff:127.0.0.1".parse().unwrap()
        ))));
        assert!(!dest_is_local(&d(Addr::V6(
            "::ffff:10.0.0.1".parse().unwrap()
        ))));
    }

    #[test]
    fn url_hosts() {
        assert_eq!(
            url_host("http://localhost:5173/x").as_deref(),
            Some("localhost")
        );
        assert_eq!(url_host("https://[::1]:8443/").as_deref(), Some("::1"));
        // Credentials are not opened (09 §7); the Codex counterexample is the LAN host.
        assert_eq!(url_host("http://u:p@127.0.0.1/"), None);
        assert_eq!(
            url_host("http://192.168.1.10\\@localhost:5173/").as_deref(),
            Some("192.168.1.10")
        );
        assert_eq!(
            canonical_open_url("http://192.168.1.10\\@localhost:5173/"),
            Some(("http://192.168.1.10/@localhost:5173/".into(), false))
        );
        assert_eq!(
            canonical_open_url("http://2130706433:5173/"),
            Some(("http://127.0.0.1:5173/".into(), true))
        );
        assert_eq!(
            url_host("http://example.com").as_deref(),
            Some("example.com")
        );
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "vscode://x",
            "http://",
            "http://a b/",
        ] {
            assert_eq!(url_host(bad), None, "{bad}");
        }
    }

    #[test]
    fn loopback_targets() {
        let d = |addr: Addr| Dest { addr, port: 80 };
        assert_eq!(
            loopback_target(&d(Addr::Domain("localhost".into()))),
            "localhost:80"
        );
        assert_eq!(
            loopback_target(&d(Addr::V4("0.0.0.0".parse().unwrap()))),
            "127.0.0.1:80"
        );
        assert_eq!(
            loopback_target(&d(Addr::V6("::1".parse().unwrap()))),
            "[::1]:80"
        );
        assert_eq!(
            loopback_target(&d(Addr::V6("::ffff:127.0.0.1".parse().unwrap()))),
            "127.0.0.1:80"
        );
        assert!(dest_is_local(&d(Addr::V4("0.0.0.0".parse().unwrap()))));
        assert!(!dest_is_local(&d(Addr::V4("10.1.1.1".parse().unwrap()))));
        for t in ["localhost:80", "127.0.0.1:80", "[::1]:80"] {
            assert!(
                vk_remote::ChannelKind::parse(&format!("tcp:{t}")).is_ok(),
                "{t}"
            );
        }
    }

    #[test]
    fn config_from_extra() {
        let (cfg, _) = vk_config::Config::parse(
            "[preview]\nauto_discover = \"promote\"\nprofile_route = \"remote\"\nbrowser = \"/x/chrome\"\nunknown_key = 1\n",
            std::path::Path::new("/tmp/c.toml"),
        )
        .unwrap();
        let c = PreviewConfig::from_config(&cfg);
        assert_eq!(c.auto_discover, "promote");
        assert_eq!(c.profile_route, "remote");
        assert_eq!(c.browser, "/x/chrome");
        assert!(c.allow_remote_egress);
        assert_eq!(
            PreviewConfig::from_config(&vk_config::Config::default()).auto_discover,
            "suggest"
        );
    }
}
