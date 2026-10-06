//! Previews and the browser route, server side (06 B2, B3.1, B3.3, B3.4).
//!
//! - **Discovery** (every server): every 2 s (and on `pane.process_changed`) the LISTEN
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
];

/// `[preview]` keys used here (06 Part C). Unknown keys are ignored.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PreviewConfig {
    /// suggest | promote | off
    pub auto_discover: String,
    /// pane | window — Stage 1 builds the window only.
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
        }
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
    pub accepted: AtomicU64,
    pub rejected: AtomicU64,
}

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
            accepted: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
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

    /// Test hook / embedding: how links to machines are made.
    pub fn set_link_factory(&self, f: LinkFactory) {
        *self.link_factory.lock().unwrap() = Some(f);
        self.links.lock().unwrap().clear();
    }

    /// Hot path (pane feed): split into lines, queue the ones with `://`.
    pub fn on_output(&self, pane: &str, data: &[u8]) {
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

    fn link(&self, server: &Server, machine: &str) -> Option<Link> {
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
                .any(|a| a == &format!("--user-data-dir={}", b.dir));
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
pub fn status_json(server: &Server) -> Value {
    let port = server.previews.socks_port.try_lock().ok().and_then(|g| *g);
    json!({
        "socks_port": port,
        "browsers": server.previews.browsers.lock().unwrap().len(),
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

fn task_of_pane(c: &Core, pane: &str) -> Option<String> {
    let p = c.pane(pane)?;
    c.ws(&p.workspace).and_then(|w| w.task.clone())
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

async fn discovery_loop(server: Arc<Server>) {
    let mut events = server.events.subscribe();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut cfg = PreviewConfig::load();
    let mut cfg_at = Instant::now();
    let mut last = Instant::now() - TICK;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            ev = events.recv() => match ev {
                Ok(e) if e.kind == "pane.process_changed" || e.kind == "pane.closed" => {
                    // Foreground change: rescan now (debounced).
                    if last.elapsed() < Duration::from_millis(300) { continue }
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            }
        }
        if cfg_at.elapsed() > Duration::from_secs(10) {
            cfg = PreviewConfig::load();
            cfg_at = Instant::now();
        }
        last = Instant::now();
        discover(&server, &cfg).await;
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
                let pids: Vec<u32> = vk_hold::procinfo::tree(root, 8)
                    .into_iter()
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
        let probes = futures::future::join_all(
            candidates
                .iter()
                .map(|(_, l)| probe::is_http(Some(l.addr), l.port, Duration::from_secs(1))),
        )
        .await;
        let promote = cfg.auto_discover == "promote";
        let mut items = Vec::new();
        for ((pane, l), http) in candidates.iter().zip(probes) {
            if !http {
                continue; // other TCP is hidden (06 B2)
            }
            let mut p = new_preview(server, l.port, PreviewSource::Listener, now);
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
                probe::tcp_alive(None, f.port).await
            } else {
                probe::is_http(None, f.port, Duration::from_secs(1)).await
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
            if promote {
                p.status = PreviewStatus::Up;
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
            Addr::V6(a) => a.is_unspecified(),
            Addr::Domain(_) => false,
        }
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
                    if sockets::owner_of_connection(&pids, peer.port(), local.port()).is_some() {
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
            if let Some(a) = addrs
                .iter()
                .find(|a| a.ip().is_loopback() || a.ip().is_unspecified())
            {
                let d = Dest {
                    addr: match a.ip() {
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

/// `http(s)://host[:port]/…` → host (brackets stripped), or None if not http(s).
pub(crate) fn url_host(url: &str) -> Option<String> {
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let auth = rest.split(['/', '?', '#']).next()?;
    let auth = auth.rsplit('@').next()?;
    let host = if let Some(r) = auth.strip_prefix('[') {
        r.split(']').next()?.to_string()
    } else {
        auth.split(':').next()?.to_string()
    };
    (!host.is_empty()).then_some(host)
}

pub(crate) fn open_url_of(p: &Preview) -> String {
    let u = p.url.replace("://0.0.0.0", "://localhost");
    if url_host(&u).is_some() {
        u
    } else {
        format!("http://localhost:{}{}", p.port, p.path)
    }
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
    // Pane scope (09 §5.2): agents act on their own machine's previews only.
    if ctx.pane_scope.is_some()
        && matches!(
            method,
            "preview.open" | "preview.forget" | "preview.dismiss" | "preview.promote"
        )
    {
        let remote = s(p, "machine").is_some_and(|m| !is_local_machine(server, m))
            || s(p, "preview").is_some_and(|t| !t.contains("://") && t.contains('/'));
        if remote {
            return Some(Err(err(
                ErrorKind::PermissionDenied,
                format!("{method} on another machine is not allowed from a pane"),
            )
            .details(json!({"scope": "pane"}))));
        }
    }
    Some(match method {
        "preview.declare" => declare(server, ctx, p),
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
        "preview.promote" => promote(server, p).await,
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
            pv.map(|x| json!({"remote_url": x.url, "profile_url": open_url_of(&x)}))
        }
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

fn declare(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let port = port_param(p)?;
    let pane = match s(p, "pane") {
        Some(t) => Some(resolve_pane(server, ctx, Some(t))?.id),
        None => ctx.pane_scope.clone(),
    };
    let mut path = s(p, "path").unwrap_or("/").to_string();
    if !path.starts_with('/') {
        path.insert(0, '/');
    }
    if path.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(invalid("path must not contain whitespace"));
    }
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
            if pane.is_some() {
                x.pane = pane;
            }
            if task.is_some() {
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
    Ok(
        json!({"preview": server.with_core(|c| preview_json(c, &pv)), "cursor": crate::api::cursor(server, None)}),
    )
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

async fn promote(server: &Arc<Server>, p: &Value) -> R {
    let t = s(p, "preview").ok_or_else(|| invalid("missing param `preview`"))?;
    let (m, t) = split_target(server, p, t);
    if !m.is_empty() {
        return remote_call(server, &m, "preview.promote", json!({"preview": t})).await;
    }
    let mut x = find_local(server, &t)?;
    if lifecycle::promote(&mut x) {
        commit_previews(server, vec![(x.clone(), Some("preview.up"))]);
    }
    Ok(json!({"preview": server.with_core(|c| preview_json(c, &x))}))
}

async fn forget(server: &Arc<Server>, p: &Value) -> R {
    let t = s(p, "preview").ok_or_else(|| invalid("missing param `preview`"))?;
    let (m, t) = split_target(server, p, t);
    if !m.is_empty() {
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
    let window =
        b(p, "window").unwrap_or(false) || (cfg.mode == "window" && s(p, "split").is_none());
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
            if lifecycle::promote(&mut x) {
                commit_previews(server, vec![(x.clone(), Some("preview.up"))]);
            }
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
    if !window {
        return crate::browser_pane::open_pane(server, ctx, p, &machine, preview.as_ref(), &url)
            .await;
    }
    // Open-URL rules (09 §7): http(s) only; from a pane, only loopback.
    let host = url_host(&url).ok_or_else(|| invalid("only http(s) URLs can be opened"))?;
    if ctx.pane_scope.is_some() && !vk_remote::is_loopback_host(&host) {
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
    let profile = s(p, "profile")
        .filter(|x| ctx.pane_scope.is_none() && browser::valid_profile_name(x))
        .map(str::to_string)
        .unwrap_or_else(|| profile_for(&cfg, &machine_label, task.as_deref()));
    let route = if cfg.profile_route == "remote" {
        "remote"
    } else {
        "loopback"
    };
    let bin = browser::find_browser(Some(cfg.browser.as_str()).filter(|b| !b.is_empty()))
        .ok_or_else(|| {
            err(
                ErrorKind::Unsupported,
                "no Chromium-family browser found (set [preview] browser = \"/path/to/chrome\")",
            )
            .details(json!({"fallback": "install Chromium or Chrome"}))
        })?;
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
    let spec = browser::LaunchSpec {
        bin: bin.path.clone(),
        profile_dir: dir.clone(),
        socks_port,
        url: url.clone(),
        headless,
        extra_args: vec![],
    };
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
    fn url_hosts() {
        assert_eq!(
            url_host("http://localhost:5173/x").as_deref(),
            Some("localhost")
        );
        assert_eq!(url_host("https://[::1]:8443/").as_deref(), Some("::1"));
        assert_eq!(
            url_host("http://u:p@127.0.0.1/").as_deref(),
            Some("127.0.0.1")
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
