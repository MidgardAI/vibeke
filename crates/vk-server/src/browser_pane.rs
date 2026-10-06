//! Browser panes (06 B3.2, B3.3): a non-PTY pane kind whose content is a live Chromium
//! viewport.
//!
//! Two roles, usually on different servers:
//! - **Owner**: the server whose layout holds the pane (next to the agent pane it was opened
//!   for). It persists [`BrowserPane`] (URL, history, profile inputs) and answers
//!   `browser.pane.create` / `browser.pane.update`.
//! - **Media host**: the server on the viewing client's machine (the laptop's local server in
//!   the recommended topology; the owner itself in the plain-SSH topology). It runs one
//!   headless Chromium per profile (at the host's DPR) with one CDP target per visible pane,
//!   decodes screencast frames, diffs cell-aligned tiles and publishes changed tiles on the
//!   render stream's media channel ([`MediaSession`]): latest-wins per pane, after cell
//!   frames, only for panes that client reports visible (`ClientFrame::MediaView`). A pane no
//!   client shows stops its screencast; an unviewed target is closed after a while and
//!   re-created at its last URL when viewed again.

use crate::Server;
use crate::api::{Ctx, R, b, err, invalid, not_found, resolve_pane, s, u};
use crate::core::{Tx, subject_pane, ulid};
use crate::preview::{self, PreviewConfig};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::io::AsyncWrite;
use tokio::sync::Notify;
use vk_browser::cdp::{Cdp, Event, Page, ScreencastFrame};
use vk_browser::frame::{Rgba, TileDiffer, TileRect};
use vk_browser::input::{self, MapOptions};
use vk_proto::frame::asyncio;
use vk_proto::input::{KeyKind, MouseButton, MouseKind};
use vk_proto::layout::{self, Direction};
use vk_proto::model::*;
use vk_proto::render::{
    BrowserCmd, BrowserStatus, MediaFrame, MediaPane, MediaTile, ServerFrame, TileData,
};
use vk_proto::rpc::{ErrorKind, RpcError};

pub const METHODS: &[(&str, bool)] = &[
    ("browser.pane.create", true),
    ("browser.pane.update", true),
    ("browser.pane.list", false),
    ("browser.pane.status", false),
    ("browser.command", true),
];

/// Tiles are `TILE_COLS × TILE_ROWS` host cells (64×64 device px for 16×32 cells).
pub const TILE_COLS: u16 = 4;
pub const TILE_ROWS: u16 = 2;
/// Viewport changes are applied once the pane geometry is stable this long (06 B3.2).
pub const RESIZE_DEBOUNCE: Duration = Duration::from_millis(100);
const MAX_HISTORY: usize = 50;
/// Unacked media frames per pane per client (local / remote client).
const WINDOW_LOCAL: u32 = 2;
const WINDOW_REMOTE: u32 = 1;

// ---- launching --------------------------------------------------------------------------------

/// What to launch for a profile.
#[derive(Debug, Clone)]
pub struct LaunchReq {
    pub profile: String,
    pub profile_dir: PathBuf,
    pub dpr: f64,
    /// SOCKS5 port for remote-machine profiles (06 B3.1); `None` = no proxy.
    pub socks_port: Option<u16>,
    pub log: Option<PathBuf>,
}

/// A running browser driven over CDP.
pub struct Launched {
    pub cdp: Arc<Cdp>,
    pub events: Receiver<Event>,
    /// Root pid (registered with the SOCKS peer check).
    pub pid: Option<u32>,
    /// Dropped when the browser is closed (the real launcher's `Browser` closes Chromium).
    pub guard: Box<dyn Send>,
}

pub trait Launcher: Send + Sync {
    fn launch(&self, req: &LaunchReq) -> Result<Launched>;
}

/// Headless Chromium over `--remote-debugging-pipe` (Playwright's headless shell preferred).
pub struct ChromiumLauncher {
    pub binary: Option<PathBuf>,
}

/// `[preview] pane_browser`, `$VIBEKE_CHROMIUM`, Playwright headless shell, then any
/// Chromium-family browser found for the window.
pub fn find_pane_browser(configured: Option<&str>) -> Option<PathBuf> {
    if let Some(c) = configured.filter(|c| !c.is_empty()) {
        let p = PathBuf::from(c);
        return p.is_file().then_some(p);
    }
    vk_browser::cdp::discover_chromium(true)
        .or_else(|| vk_preview::browser::find_browser(None).map(|b| b.path))
}

impl Launcher for ChromiumLauncher {
    fn launch(&self, req: &LaunchReq) -> Result<Launched> {
        let bin = self
            .binary
            .clone()
            .or_else(|| find_pane_browser(None))
            .ok_or_else(|| anyhow!("no Chromium found for the browser pane (install Playwright's chromium-headless-shell or set VIBEKE_CHROMIUM)"))?;
        tracing::info!(bin = %bin.display(), profile = %req.profile, "browser pane: launching");
        let mut o = vk_browser::cdp::LaunchOptions::new(&bin, &req.profile_dir);
        let name = bin
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        o.headless_new = !name.contains("headless");
        o.device_scale_factor = Some(req.dpr);
        o.stderr_log = req.log.clone();
        // Wheel deltas from the host are already pixel-smooth (Stage 0: cut the scroll p95).
        o.extra_args.push("--disable-smooth-scrolling".into());
        if let Some(p) = req.socks_port {
            o.extra_args
                .push(format!("--proxy-server=socks5://127.0.0.1:{p}"));
            o.extra_args.push("--proxy-bypass-list=<-loopback>".into());
        }
        let mut b = vk_browser::cdp::Browser::launch(&o)?;
        let events = b.take_events();
        Ok(Launched {
            cdp: b.cdp.clone(),
            events,
            pid: Some(b.pid()),
            guard: Box::new(b),
        })
    }
}

/// In-process fake Chromium (tests).
#[derive(Default)]
pub struct FakeLauncher {
    pub launched: Mutex<Vec<(LaunchReq, Arc<Mutex<vk_browser::fake_chromium::State>>)>>,
}

impl Launcher for FakeLauncher {
    fn launch(&self, req: &LaunchReq) -> Result<Launched> {
        let (cdp, events, state) = vk_browser::fake_chromium::spawn_pair(req.dpr);
        self.launched.lock().unwrap().push((req.clone(), state));
        Ok(Launched {
            cdp,
            events,
            pid: None,
            guard: Box::new(()),
        })
    }
}

// ---- state ------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geom {
    pub cols: u16,
    pub rows: u16,
    pub cell_w: u16,
    pub cell_h: u16,
    pub dpr: f32,
}

impl Geom {
    fn valid(&self) -> bool {
        self.cols > 0 && self.rows > 0 && self.cell_w > 0 && self.cell_h > 0 && self.dpr > 0.0
    }
}

/// Where a pane's traffic goes and which profile it uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// Machine label (`local` for this machine).
    pub machine: String,
    pub profile: String,
    /// `none` (local) | `loopback` | `remote`.
    pub route: String,
}

impl Route {
    fn local(&self) -> bool {
        self.machine == "local"
    }
}

struct Proc {
    profile: String,
    dpr: f64,
    cdp: Arc<Cdp>,
    pid: Option<u32>,
    guard: Mutex<Option<Box<dyn Send>>>,
    sessions: Mutex<HashMap<String, Weak<Target>>>,
    dead: AtomicBool,
    launched_at: Instant,
}

#[derive(Default)]
struct Sub {
    notify: Arc<Notify>,
    dirty: BTreeSet<u32>,
    reset: bool,
    state_dirty: bool,
}

struct TState {
    route: Route,
    page: Option<Page>,
    proc: Option<Arc<Proc>>,
    creating: bool,
    closed: bool,
    url: String,
    title: String,
    loading: bool,
    history: Vec<String>,
    history_index: usize,
    error: Option<String>,
    notice: Option<String>,
    env: String,
    want: Option<Geom>,
    want_at: Instant,
    applied: Option<Geom>,
    css: (u32, u32),
    screencast: bool,
    frame: Option<Arc<Rgba>>,
    seq: u64,
    differ: TileDiffer,
    tile_cell: (u16, u16),
    subs: HashMap<u64, Sub>,
    last_viewed: Instant,
    frames_in: u64,
    frame_times: Vec<Instant>,
    decode_ms: f64,
}

pub struct Target {
    pub pane: String,
    pub owner: String,
    st: Mutex<TState>,
}

impl Target {
    fn status(st: &TState, windowed: bool) -> BrowserStatus {
        BrowserStatus {
            url: st.url.clone(),
            title: st.title.clone(),
            loading: st.loading,
            can_back: st.history_index > 0,
            can_forward: st.history_index + 1 < st.history.len(),
            env: st.env.clone(),
            windowed,
            error: st.error.clone(),
            notice: st.notice.clone(),
            css_w: st.css.0,
            css_h: st.css.1,
        }
    }

    fn mark_state(st: &mut TState) {
        for s in st.subs.values_mut() {
            s.state_dirty = true;
            s.notify.notify_one();
        }
    }

    pub fn page(&self) -> Option<Page> {
        self.st.lock().unwrap().page.clone()
    }

    /// A decoded frame arrived: diff against the previous one and mark changed tiles dirty for
    /// every subscriber (latest-wins: subscribers later read the *current* frame).
    pub fn on_frame(&self, img: Rgba) {
        let mut st = self.st.lock().unwrap();
        let cell = st
            .applied
            .or(st.want)
            .map(|g| (g.cell_w.max(1), g.cell_h.max(1)))
            .unwrap_or((16, 32));
        let mut reset = false;
        if st.tile_cell != cell || st.frame.is_none() {
            st.tile_cell = cell;
            st.differ = TileDiffer::cell_aligned(
                cell.0 as u32,
                cell.1 as u32,
                TILE_COLS as u32,
                TILE_ROWS as u32,
            );
            reset = true;
        }
        if let Some(prev) = &st.frame
            && (prev.width != img.width || prev.height != img.height)
        {
            reset = true;
        }
        let changed = st.differ.diff(&img);
        st.seq += 1;
        st.frames_in += 1;
        let now = Instant::now();
        st.frame_times.push(now);
        st.frame_times
            .retain(|t| now.duration_since(*t) < Duration::from_secs(2));
        st.frame = Some(Arc::new(img));
        if changed.is_empty() && !reset {
            return;
        }
        for s in st.subs.values_mut() {
            if reset {
                s.reset = true;
                s.dirty.clear();
            } else {
                s.dirty.extend(changed.iter().map(|t| t.index as u32));
            }
            s.notify.notify_one();
        }
    }

    /// Tiles to send to subscriber `sub`: everything after a reset, else the tiles that changed
    /// since its last take, read from the current frame.
    fn take(&self, sub: u64) -> Option<TakenFrame> {
        let mut st = self.st.lock().unwrap();
        let frame = st.frame.clone()?;
        let seq = st.seq;
        let cell = st.tile_cell;
        let all = st.differ.tiles(frame.width, frame.height);
        let s = st.subs.get_mut(&sub)?;
        if !s.reset && s.dirty.is_empty() {
            return None;
        }
        let reset = std::mem::take(&mut s.reset);
        let dirty = std::mem::take(&mut s.dirty);
        let tiles: Vec<TileRect> = if reset {
            all
        } else {
            all.into_iter()
                .filter(|t| dirty.contains(&(t.index as u32)))
                .collect()
        };
        Some(TakenFrame {
            frame,
            tiles,
            reset,
            seq,
            cell,
        })
    }

    fn take_state(&self, sub: u64, windowed: bool) -> Option<BrowserStatus> {
        let mut st = self.st.lock().unwrap();
        let s = st.subs.get_mut(&sub)?;
        if !s.state_dirty {
            return None;
        }
        s.state_dirty = false;
        let status = Target::status(&st, windowed);
        // Notices are one-shot: cleared once every subscriber has seen them.
        if st.subs.values().all(|s| !s.state_dirty) {
            st.notice = None;
        }
        Some(status)
    }
}

struct TakenFrame {
    frame: Arc<Rgba>,
    tiles: Vec<TileRect>,
    reset: bool,
    seq: u64,
    cell: (u16, u16),
}

#[derive(Default)]
struct Inner {
    procs: HashMap<String, Arc<Proc>>,
    targets: HashMap<String, Arc<Target>>,
    /// Profiles handed to a headful window (06 B3.3).
    windowed: HashSet<String>,
    /// Profile → when its browser last died (crash-loop backoff).
    died: HashMap<String, Instant>,
}

/// Media host state (one per server).
pub struct Host {
    inner: Mutex<Inner>,
    launch_lock: Mutex<()>,
    launcher: Mutex<Option<Arc<dyn Launcher>>>,
    profiles_root: Mutex<Option<PathBuf>>,
    next_sub: AtomicU64,
    gc_started: AtomicBool,
    /// Idle timeouts (tests shorten them).
    pub idle_target: Mutex<Duration>,
    pub idle_proc: Mutex<Duration>,
    pub tiles_sent: AtomicU64,
    pub media_bytes: AtomicU64,
}

impl Default for Host {
    fn default() -> Self {
        Host {
            inner: Mutex::default(),
            launch_lock: Mutex::new(()),
            launcher: Mutex::new(None),
            profiles_root: Mutex::new(None),
            next_sub: AtomicU64::new(1),
            gc_started: AtomicBool::new(false),
            idle_target: Mutex::new(Duration::from_secs(120)),
            idle_proc: Mutex::new(Duration::from_secs(30)),
            tiles_sent: AtomicU64::new(0),
            media_bytes: AtomicU64::new(0),
        }
    }
}

impl Host {
    /// Test hook / embedding: how browsers are launched.
    pub fn set_launcher(&self, l: Arc<dyn Launcher>) {
        *self.launcher.lock().unwrap() = Some(l);
    }

    /// Test hook: where profiles live (default `<state>/browser-profiles`).
    pub fn set_profiles_root(&self, p: PathBuf) {
        *self.profiles_root.lock().unwrap() = Some(p);
    }

    fn profiles_root(&self) -> PathBuf {
        self.profiles_root
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(preview::profiles_root)
    }

    fn launcher(&self) -> Arc<dyn Launcher> {
        if let Some(l) = self.launcher.lock().unwrap().clone() {
            return l;
        }
        let cfg = PreviewConfig::load();
        Arc::new(ChromiumLauncher {
            binary: find_pane_browser(Some(cfg.pane_browser.as_str()).filter(|b| !b.is_empty())),
        })
    }

    pub fn target(&self, pane: &str) -> Option<Arc<Target>> {
        self.inner.lock().unwrap().targets.get(pane).cloned()
    }

    fn is_windowed(&self, profile: &str) -> bool {
        self.inner.lock().unwrap().windowed.contains(profile)
    }

    pub fn new_sub(&self) -> u64 {
        self.next_sub.fetch_add(1, Ordering::Relaxed)
    }
}

// ---- routes, URLs -------------------------------------------------------------------------------

fn route_for(server: &Server, cfg: &PreviewConfig, mp: &MediaPane) -> Route {
    let machine = if !mp.owner.is_empty() {
        mp.owner.clone()
    } else if preview::is_local_machine(server, &mp.spec.machine) {
        "local".to_string()
    } else {
        mp.spec.machine.clone()
    };
    let profile = preview::profile_for(cfg, &machine, mp.spec.task.as_deref());
    let route = if machine == "local" {
        "none"
    } else if cfg.profile_route == "remote" {
        "remote"
    } else {
        "loopback"
    };
    Route {
        machine,
        profile,
        route: route.to_string(),
    }
}

fn env_label(server: &Server, r: &Route) -> String {
    let me = if server.opts.machine.is_empty() {
        "local".to_string()
    } else {
        server.opts.machine.clone()
    };
    if r.local() {
        format!("{me} chromium → local")
    } else {
        format!("{me} chromium → {} {}", r.machine, r.route)
    }
}

/// Normalise what the user typed into the address bar: `localhost:5173/x` → `http://…`.
pub fn normalize_url(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() || t.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    if t == "about:blank" {
        return Some(t.to_string());
    }
    let u = if t.starts_with("http://") || t.starts_with("https://") {
        t.to_string()
    } else if t.contains("://") {
        return None;
    } else {
        // `host[:port][/path]`; `javascript:…`, `data:…` and friends are not URLs we open.
        let auth = t.split(['/', '?', '#']).next().unwrap_or("");
        if let Some((_, port)) = auth.rsplit_once(':')
            && (port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()))
            && !auth.starts_with('[')
        {
            return None;
        }
        format!("http://{t}")
    };
    preview::url_host(&u).map(|_| u)
}

/// Initial URL check: http(s) or about:blank; for panes owned by another machine, only that
/// machine's loopback (a remote server can't point the laptop browser at the internet).
fn initial_url_ok(url: &str, remote_owner: bool) -> bool {
    if url == "about:blank" {
        return true;
    }
    match preview::url_host(url) {
        Some(h) => !remote_owner || vk_remote::is_loopback_host(&h) || h == "0.0.0.0",
        None => false,
    }
}

// ---- views (subscriptions) ----------------------------------------------------------------------

/// Replace subscriber `sub`'s visible browser panes.
pub fn view(server: &Arc<Server>, sub: u64, notify: &Arc<Notify>, panes: &[MediaPane]) {
    let host = &server.browser;
    start_gc(server);
    let cfg = PreviewConfig::load();
    let wanted: HashSet<&str> = panes.iter().map(|p| p.pane.as_str()).collect();
    // Unsubscribe from panes no longer shown.
    let all: Vec<Arc<Target>> = host
        .inner
        .lock()
        .unwrap()
        .targets
        .values()
        .cloned()
        .collect();
    for t in &all {
        if wanted.contains(t.pane.as_str()) {
            continue;
        }
        let stop = {
            let mut st = t.st.lock().unwrap();
            if st.subs.remove(&sub).is_some() {
                st.last_viewed = Instant::now();
            }
            st.subs.is_empty() && st.screencast
        };
        if stop {
            set_screencast(t, false);
        }
    }
    for mp in panes {
        let route = route_for(server, &cfg, mp);
        let geom = Geom {
            cols: mp.cols,
            rows: mp.rows,
            cell_w: mp.cell_w,
            cell_h: mp.cell_h,
            dpr: mp.dpr,
        };
        let t = {
            let mut inner = host.inner.lock().unwrap();
            if let Some(t) = inner.targets.get(&mp.pane) {
                t.clone()
            } else {
                let remote_owner = !mp.owner.is_empty();
                // A remote owner's record may only start the page on that machine's loopback
                // (fall back to the latest loopback URL in its history).
                let (url, error) = if initial_url_ok(&mp.spec.url, remote_owner) {
                    (mp.spec.url.clone(), None)
                } else if let Some(u) = mp
                    .spec
                    .history
                    .iter()
                    .rev()
                    .find(|u| *u != "about:blank" && initial_url_ok(u, remote_owner))
                {
                    (u.clone(), None)
                } else {
                    (
                        "about:blank".to_string(),
                        Some(format!("refused to open {}", mp.spec.url)),
                    )
                };
                let t = Arc::new(Target {
                    pane: mp.pane.clone(),
                    owner: mp.owner.clone(),
                    st: Mutex::new(TState {
                        env: env_label(server, &route),
                        route: route.clone(),
                        page: None,
                        proc: None,
                        creating: false,
                        closed: false,
                        url: url.clone(),
                        title: mp.spec.title.clone(),
                        loading: true,
                        history: if mp.spec.history.is_empty() {
                            vec![url]
                        } else {
                            mp.spec.history.clone()
                        },
                        history_index: mp.spec.history_index as usize,
                        error,
                        notice: None,
                        want: None,
                        want_at: Instant::now(),
                        applied: None,
                        css: (0, 0),
                        screencast: false,
                        frame: None,
                        seq: 0,
                        differ: TileDiffer::new(64, 64),
                        tile_cell: (0, 0),
                        subs: HashMap::new(),
                        last_viewed: Instant::now(),
                        frames_in: 0,
                        frame_times: Vec::new(),
                        decode_ms: 0.0,
                    }),
                });
                inner.targets.insert(mp.pane.clone(), t.clone());
                t
            }
        };
        let (start, resize) = {
            let mut st = t.st.lock().unwrap();
            st.last_viewed = Instant::now();
            let new = !st.subs.contains_key(&sub);
            let s = st.subs.entry(sub).or_insert_with(|| Sub {
                notify: notify.clone(),
                ..Default::default()
            });
            if new {
                s.reset = true;
                s.state_dirty = true;
                s.notify.notify_one();
            }
            let resize = geom.valid() && st.want != Some(geom);
            if resize {
                st.want = Some(geom);
                st.want_at = Instant::now();
            }
            (st.page.is_some() && !st.screencast, resize)
        };
        if start {
            set_screencast(&t, true);
        }
        if resize {
            schedule_resize(server, &t);
        }
        kick(server, &t);
    }
}

/// Drop subscriber `sub` everywhere (client gone).
pub fn unsubscribe(server: &Arc<Server>, sub: u64) {
    let all: Vec<Arc<Target>> = server
        .browser
        .inner
        .lock()
        .unwrap()
        .targets
        .values()
        .cloned()
        .collect();
    for t in all {
        let stop = {
            let mut st = t.st.lock().unwrap();
            st.subs.remove(&sub);
            st.last_viewed = Instant::now();
            st.subs.is_empty() && st.screencast
        };
        if stop {
            set_screencast(&t, false);
        }
    }
}

fn set_screencast(t: &Arc<Target>, on: bool) {
    let page = {
        let mut st = t.st.lock().unwrap();
        let Some(p) = st.page.clone() else {
            st.screencast = false;
            return;
        };
        if st.screencast == on {
            return;
        }
        st.screencast = on;
        p
    };
    if on {
        let _ = page.send(
            "Page.startScreencast",
            json!({"format": "jpeg", "quality": 80, "everyNthFrame": 1}),
        );
    } else {
        let _ = page.send("Page.stopScreencast", json!({}));
    }
}

fn schedule_resize(server: &Arc<Server>, t: &Arc<Target>) {
    let server = server.clone();
    let t = t.clone();
    tokio::spawn(async move {
        tokio::time::sleep(RESIZE_DEBOUNCE).await;
        let due = {
            let st = t.st.lock().unwrap();
            st.want_at.elapsed() >= RESIZE_DEBOUNCE && st.want != st.applied && st.page.is_some()
        };
        if due {
            let _ = tokio::task::spawn_blocking(move || apply_viewport(&server, &t)).await;
        }
    });
}

/// Apply the wanted geometry: `Emulation.setDeviceMetricsOverride` (CSS = device px / DPR);
/// a DPR change relaunches the profile's browser (`--force-device-scale-factor` is a launch
/// flag; Stage 0).
fn apply_viewport(server: &Arc<Server>, t: &Arc<Target>) {
    let (page, geom, proc) = {
        let st = t.st.lock().unwrap();
        match (st.page.clone(), st.want) {
            (Some(p), Some(g)) => (p, g, st.proc.clone()),
            _ => return,
        }
    };
    if let Some(proc) = &proc
        && (proc.dpr - geom.dpr as f64).abs() > 0.01
    {
        tracing::info!(profile = %proc.profile, from = proc.dpr, to = geom.dpr, "browser pane: DPR changed, relaunching");
        close_proc(server, proc, "dpr changed");
        return;
    }
    match page.set_viewport_for_cells(
        geom.cols as u32,
        geom.rows as u32,
        geom.cell_w as u32,
        geom.cell_h as u32,
        geom.dpr as f64,
    ) {
        Ok(css) => {
            let mut st = t.st.lock().unwrap();
            st.applied = Some(geom);
            st.css = css;
            Target::mark_state(&mut st);
        }
        Err(e) => tracing::debug!(error = %format!("{e:#}"), "setDeviceMetricsOverride failed"),
    }
}

/// Make sure a viewed target has a page (launching the profile's browser if needed).
fn kick(server: &Arc<Server>, t: &Arc<Target>) {
    let profile = t.st.lock().unwrap().route.profile.clone();
    if server.browser.is_windowed(&profile) {
        return;
    }
    if let Some(at) = server.browser.inner.lock().unwrap().died.get(&profile)
        && at.elapsed() < Duration::from_secs(3)
    {
        return;
    }
    {
        let mut st = t.st.lock().unwrap();
        if st.page.is_some() || st.creating || st.subs.is_empty() || st.closed {
            return;
        }
        st.creating = true;
    }
    let server = server.clone();
    let t = t.clone();
    tokio::spawn(async move {
        let res = ensure(&server, &t).await;
        let mut st = t.st.lock().unwrap();
        st.creating = false;
        if let Err(e) = res {
            tracing::warn!(pane = %t.pane, error = %format!("{e:#}"), "browser pane: could not start");
            st.error = Some(format!("{e:#}"));
            st.loading = false;
            Target::mark_state(&mut st);
        }
    });
}

async fn ensure(server: &Arc<Server>, t: &Arc<Target>) -> Result<()> {
    let route = t.st.lock().unwrap().route.clone();
    let socks = if route.local() {
        None
    } else {
        Some(
            preview::ensure_socks(server, None)
                .await
                .context("SOCKS listener")?,
        )
    };
    let server = server.clone();
    let t = t.clone();
    tokio::task::spawn_blocking(move || ensure_blocking(&server, &t, socks)).await?
}

fn ensure_blocking(server: &Arc<Server>, t: &Arc<Target>, socks: Option<u16>) -> Result<()> {
    let (route, url, geom) = {
        let st = t.st.lock().unwrap();
        (st.route.clone(), st.url.clone(), st.want)
    };
    let dpr = geom.map(|g| g.dpr as f64).unwrap_or(1.0);
    let proc = proc_for(server, &route, dpr, socks)?;
    let r = proc
        .cdp
        .call(None, "Target.createTarget", json!({"url": "about:blank"}))?;
    let target_id = r["targetId"]
        .as_str()
        .ok_or_else(|| anyhow!("createTarget: no targetId"))?
        .to_string();
    let r = proc.cdp.call(
        None,
        "Target.attachToTarget",
        json!({"targetId": target_id, "flatten": true}),
    )?;
    let session = r["sessionId"]
        .as_str()
        .ok_or_else(|| anyhow!("attachToTarget: no sessionId"))?
        .to_string();
    proc.sessions
        .lock()
        .unwrap()
        .insert(session.clone(), Arc::downgrade(t));
    let page = Page::attached(proc.cdp.clone(), session, target_id);
    page.call("Page.enable", json!({}))?;
    if let Some(g) = geom
        && g.valid()
    {
        let css = page.set_viewport_for_cells(
            g.cols as u32,
            g.rows as u32,
            g.cell_w as u32,
            g.cell_h as u32,
            g.dpr as f64,
        )?;
        let mut st = t.st.lock().unwrap();
        st.applied = Some(g);
        st.css = css;
    }
    let _ = page.send("Page.navigate", json!({"url": url}));
    let screencast = {
        let mut st = t.st.lock().unwrap();
        st.page = Some(page.clone());
        st.proc = Some(proc.clone());
        st.error = None;
        st.screencast = !st.subs.is_empty();
        Target::mark_state(&mut st);
        st.screencast
    };
    if screencast {
        page.call(
            "Page.startScreencast",
            json!({"format": "jpeg", "quality": 80, "everyNthFrame": 1}),
        )?;
    }
    // Geometry may have changed while we were creating.
    let stale = {
        let st = t.st.lock().unwrap();
        st.want != st.applied
    };
    if stale {
        apply_viewport(server, t);
    }
    Ok(())
}

fn proc_for(
    server: &Arc<Server>,
    route: &Route,
    dpr: f64,
    socks: Option<u16>,
) -> Result<Arc<Proc>> {
    let host = &server.browser;
    let _g = host.launch_lock.lock().unwrap();
    let existing = host
        .inner
        .lock()
        .unwrap()
        .procs
        .get(&route.profile)
        .cloned();
    if let Some(p) = existing {
        if !p.dead.load(Ordering::Relaxed) && (p.dpr - dpr).abs() < 0.01 {
            return Ok(p);
        }
        close_proc(server, &p, "relaunch");
    }
    if let Some((pid, headless)) = preview::running_browser(server, &route.profile)
        && !headless
    {
        bail!(
            "profile {} is open in a browser window (pid {pid}); close it or use \"back to pane\"",
            route.profile
        );
    }
    let root = host.profiles_root();
    let dir = root.join(&route.profile);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    vk_preview::browser::check_profile_dir(&dir, &root)?;
    let req = LaunchReq {
        profile: route.profile.clone(),
        profile_dir: dir.clone(),
        dpr,
        socks_port: socks,
        log: Some(
            server
                .paths
                .logs()
                .join(format!("browser-pane-{}.log", route.profile)),
        ),
    };
    let l = host.launcher().launch(&req)?;
    let proc = Arc::new(Proc {
        profile: route.profile.clone(),
        dpr,
        cdp: l.cdp,
        pid: l.pid,
        guard: Mutex::new(Some(l.guard)),
        sessions: Mutex::new(HashMap::new()),
        dead: AtomicBool::new(false),
        launched_at: Instant::now(),
    });
    if let Some(pid) = l.pid {
        preview::register_browser(
            server,
            &route.profile,
            &route.machine,
            &route.route,
            pid,
            &dir,
        );
    }
    tracing::info!(profile = %route.profile, dpr, pid = ?l.pid, machine = %route.machine, "browser pane: launched headless Chromium");
    let weak_server = Arc::downgrade(server);
    let p2 = proc.clone();
    let events = l.events;
    std::thread::Builder::new()
        .name(format!("browser-pane-{}", route.profile))
        .spawn(move || pump(weak_server, p2, events))?;
    host.inner
        .lock()
        .unwrap()
        .procs
        .insert(route.profile.clone(), proc.clone());
    Ok(proc)
}

/// Close a profile's browser; its targets lose their pages (re-created when viewed again).
fn close_proc(server: &Arc<Server>, proc: &Arc<Proc>, why: &str) {
    {
        let mut inner = server.browser.inner.lock().unwrap();
        if inner
            .procs
            .get(&proc.profile)
            .is_some_and(|p| Arc::ptr_eq(p, proc))
        {
            inner.procs.remove(&proc.profile);
        }
    }
    proc.dead.store(true, Ordering::Relaxed);
    detach_targets(proc);
    if let Some(pid) = proc.pid {
        preview::unregister_browser(server, &proc.profile, pid);
    }
    let guard = proc.guard.lock().unwrap().take();
    tracing::info!(profile = %proc.profile, why, "browser pane: closing headless Chromium");
    if let Some(g) = guard {
        // Dropping a real `Browser` sends Browser.close and waits for the process.
        let _ = std::thread::Builder::new()
            .name("browser-pane-close".into())
            .spawn(move || drop(g));
    }
}

fn detach_targets(proc: &Arc<Proc>) {
    let sessions: Vec<Weak<Target>> = proc
        .sessions
        .lock()
        .unwrap()
        .drain()
        .map(|(_, t)| t)
        .collect();
    for t in sessions.into_iter().filter_map(|w| w.upgrade()) {
        let mut st = t.st.lock().unwrap();
        st.page = None;
        st.proc = None;
        st.screencast = false;
        st.applied = None;
        Target::mark_state(&mut st);
    }
}

// ---- CDP events ---------------------------------------------------------------------------------

fn pump(server: Weak<Server>, proc: Arc<Proc>, events: Receiver<Event>) {
    while let Ok(ev) = events.recv() {
        if proc.dead.load(Ordering::Relaxed) {
            break;
        }
        let Some(sid) = ev.session_id.clone() else {
            continue;
        };
        let t = proc
            .sessions
            .lock()
            .unwrap()
            .get(&sid)
            .and_then(|w| w.upgrade());
        let Some(t) = t else { continue };
        on_event(&server, &proc, &t, ev);
    }
    // The browser went away (crash, pipe closed, or we closed it).
    let was_dead = proc.dead.swap(true, Ordering::Relaxed);
    detach_targets(&proc);
    if let Some(server) = server.upgrade() {
        if let Some(pid) = proc.pid {
            preview::unregister_browser(&server, &proc.profile, pid);
        }
        let mut inner = server.browser.inner.lock().unwrap();
        if inner
            .procs
            .get(&proc.profile)
            .is_some_and(|p| Arc::ptr_eq(p, &proc))
        {
            inner.procs.remove(&proc.profile);
        }
        if !was_dead {
            tracing::warn!(profile = %proc.profile, up_ms = proc.launched_at.elapsed().as_millis() as u64, "browser pane: Chromium exited");
            inner.died.insert(proc.profile.clone(), Instant::now());
        }
        let targets: Vec<Arc<Target>> = inner.targets.values().cloned().collect();
        drop(inner);
        // Re-create viewed targets (after the crash-loop backoff), unless we closed it on
        // purpose for a window handover.
        let server2 = server.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                tokio::time::sleep(Duration::from_secs(3)).await;
                for t in targets {
                    kick(&server2, &t);
                }
            });
        }
    }
}

fn on_event(server: &Weak<Server>, proc: &Arc<Proc>, t: &Arc<Target>, ev: Event) {
    match ev.method.as_str() {
        "Page.screencastFrame" => {
            let Ok(f) = ScreencastFrame::from_event(&ev) else {
                return;
            };
            // Ack on receipt (Stage 0: best latency), then decode.
            let _ = proc.cdp.send(
                ev.session_id.as_deref(),
                "Page.screencastFrameAck",
                json!({"sessionId": f.session_id}),
            );
            let t0 = Instant::now();
            match vk_browser::frame::decode(&f.data) {
                Ok(img) => {
                    let ms = t0.elapsed().as_secs_f64() * 1000.0;
                    t.st.lock().unwrap().decode_ms = ms;
                    t.on_frame(img);
                }
                Err(e) => tracing::debug!(error = %format!("{e:#}"), "frame decode failed"),
            }
        }
        "Page.frameNavigated" => {
            let frame = &ev.params["frame"];
            if frame.get("parentId").is_some_and(|p| !p.is_null()) {
                return;
            }
            let url = frame["url"].as_str().unwrap_or("").to_string();
            navigated(server, t, url);
        }
        "Page.navigatedWithinDocument" => {
            let url = ev.params["url"].as_str().unwrap_or("").to_string();
            navigated(server, t, url);
        }
        "Page.frameStartedLoading" | "Page.frameStoppedLoading" | "Page.loadEventFired" => {
            let loading = ev.method == "Page.frameStartedLoading";
            let mut st = t.st.lock().unwrap();
            if st.loading != loading {
                st.loading = loading;
                Target::mark_state(&mut st);
            }
            if ev.method == "Page.loadEventFired" {
                drop(st);
                refresh_title(t);
            }
        }
        "Inspector.targetCrashed" => {
            let mut st = t.st.lock().unwrap();
            st.error = Some("page crashed — reload (prefix+.)".into());
            Target::mark_state(&mut st);
        }
        "Inspector.detached" | "Target.detachedFromTarget" => {
            let mut st = t.st.lock().unwrap();
            st.page = None;
            st.screencast = false;
            Target::mark_state(&mut st);
        }
        _ => {}
    }
}

fn refresh_title(t: &Arc<Target>) {
    let Some(page) = t.page() else { return };
    if let Ok(v) = page.call(
        "Runtime.evaluate",
        json!({"expression": "document.title", "returnByValue": true}),
    ) && let Some(title) = v["result"]["value"].as_str()
    {
        let mut st = t.st.lock().unwrap();
        if st.title != title {
            st.title = title.to_string();
            Target::mark_state(&mut st);
        }
    }
}

fn navigated(server: &Weak<Server>, t: &Arc<Target>, url: String) {
    if url.is_empty() {
        return;
    }
    let page = t.page();
    let hist = page
        .as_ref()
        .and_then(|p| p.call("Page.getNavigationHistory", json!({})).ok());
    {
        let mut st = t.st.lock().unwrap();
        if url != "about:blank" || st.url == "about:blank" {
            st.url = url.clone();
        }
        if let Some(h) = hist {
            let entries: Vec<String> = h["entries"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|e| e["url"].as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let idx = h["currentIndex"].as_u64().unwrap_or(0) as usize;
            // Skip the about:blank the target was created with.
            let skip = entries
                .iter()
                .take_while(|u| *u == "about:blank")
                .count()
                .min(idx);
            let mut e: Vec<String> = entries.into_iter().skip(skip).collect();
            let mut i = idx - skip;
            if e.len() > MAX_HISTORY {
                let cut = e.len() - MAX_HISTORY;
                e.drain(..cut);
                i = i.saturating_sub(cut);
            }
            if !e.is_empty() {
                st.history = e;
                st.history_index = i.min(st.history.len() - 1);
            }
        }
        Target::mark_state(&mut st);
    }
    if let Some(server) = server.upgrade() {
        persist(&server, t);
    }
}

/// Owner-local panes: write the URL and history back to the pane record.
fn persist(server: &Arc<Server>, t: &Arc<Target>) {
    if !t.owner.is_empty() {
        return;
    }
    let (url, title, history, idx) = {
        let st = t.st.lock().unwrap();
        (
            st.url.clone(),
            st.title.clone(),
            st.history.clone(),
            st.history_index,
        )
    };
    update_pane_record(
        server,
        &t.pane,
        &url,
        Some(&title),
        Some(history),
        Some(idx),
    );
}

fn update_pane_record(
    server: &Server,
    pane: &str,
    url: &str,
    title: Option<&str>,
    history: Option<Vec<String>>,
    idx: Option<usize>,
) -> bool {
    let mut c = server.core.lock().unwrap();
    let Some(mut p) = c.pane(pane).cloned() else {
        return false;
    };
    let Some(b) = p.browser.as_mut() else {
        return false;
    };
    let before = b.clone();
    if !url.is_empty() && url != "about:blank" {
        b.url = url.to_string();
    }
    if let Some(t) = title {
        b.title = t.to_string();
    }
    if let Some(h) = history.filter(|h| !h.is_empty()) {
        b.history = h;
    }
    if let Some(i) = idx {
        b.history_index = (i as u32).min(b.history.len().saturating_sub(1) as u32);
    }
    if *b == before {
        return true;
    }
    let new_url = b.url.clone();
    let auto = browser_title(b);
    if p.title.is_none() {
        p.auto_title = auto;
    }
    let mut tx = Tx::new();
    tx.event(
        "browser.navigated",
        subject_pane(&p),
        json!({"url": new_url}),
    );
    tx.pane(p);
    let _ = server.commit(&mut c, tx);
    true
}

fn browser_title(b: &BrowserPane) -> String {
    let host = preview::url_host(&b.url).unwrap_or_default();
    let port = b
        .url
        .split("://")
        .nth(1)
        .and_then(|r| r.split(['/', '?', '#']).next())
        .and_then(|a| a.rsplit_once(':'))
        .map(|(_, p)| p.to_string());
    match port {
        Some(p) if !host.is_empty() => format!("◉ {host}:{p}"),
        _ if !host.is_empty() => format!("◉ {host}"),
        _ => "◉ browser".into(),
    }
}

// ---- commands -------------------------------------------------------------------------------------

/// Input and chrome commands from a client for a pane rendered here.
pub fn command(server: &Arc<Server>, pane: &str, cmd: BrowserCmd, key_releases: bool) {
    let Some(t) = server.browser.target(pane) else {
        return;
    };
    let page = t.page();
    match cmd {
        BrowserCmd::Key(ev) => {
            let Some(page) = page else { return };
            if !key_releases && ev.kind == KeyKind::Release {
                return;
            }
            let opts = MapOptions {
                host_reports_text: false,
                ..Default::default()
            };
            let cmds = if key_releases {
                input::map_key(&ev, opts)
            } else {
                input::map_key_with_release(&ev, opts)
            };
            let _ = page.dispatch(&cmds);
        }
        BrowserCmd::Text(text) => {
            if let Some(page) = page {
                let (m, p) = input::map_paste(&text).to_command();
                let _ = page.send(m, p);
            }
        }
        BrowserCmd::Mouse {
            kind,
            button,
            x,
            y,
            mods,
            clicks,
        } => {
            let Some(page) = page else { return };
            let ty = match kind {
                MouseKind::Press => "mousePressed",
                MouseKind::Release => "mouseReleased",
                MouseKind::Drag | MouseKind::Move => "mouseMoved",
            };
            let (name, bit) = match button {
                MouseButton::Left => ("left", 1),
                MouseButton::Right => ("right", 2),
                MouseButton::Middle => ("middle", 4),
                _ => ("none", 0),
            };
            let buttons = match kind {
                MouseKind::Press | MouseKind::Drag => bit,
                _ => 0,
            };
            let mut p = json!({
                "type": ty, "x": x, "y": y, "modifiers": input::cdp_modifiers(mods),
                "button": if kind == MouseKind::Move { "none" } else { name },
                "buttons": buttons,
            });
            if matches!(kind, MouseKind::Press | MouseKind::Release) {
                p["clickCount"] = json!(clicks.max(1));
            }
            let _ = page.mouse(p);
        }
        BrowserCmd::Wheel { x, y, dx, dy, mods } => {
            if let Some(page) = page {
                let _ = page.mouse(json!({"type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy, "modifiers": input::cdp_modifiers(mods)}));
            }
        }
        BrowserCmd::Navigate(raw) => match normalize_url(&raw) {
            Some(url) => {
                {
                    let mut st = t.st.lock().unwrap();
                    st.url = url.clone();
                    st.loading = true;
                    st.error = None;
                    Target::mark_state(&mut st);
                }
                match page {
                    Some(p) => {
                        let _ = p.send("Page.navigate", json!({"url": url}));
                    }
                    None => kick(server, &t),
                }
            }
            None => {
                let mut st = t.st.lock().unwrap();
                st.notice = Some(format!("not an http(s) URL: {raw}"));
                Target::mark_state(&mut st);
            }
        },
        BrowserCmd::Back | BrowserCmd::Forward => {
            let back = matches!(cmd, BrowserCmd::Back);
            if let Some(page) = page {
                tokio::task::spawn_blocking(move || history_step(&page, back));
            }
        }
        BrowserCmd::Reload { hard } => {
            if let Some(p) = page {
                let _ = p.send("Page.reload", json!({"ignoreCache": hard}));
            } else {
                {
                    let mut st = t.st.lock().unwrap();
                    st.error = None;
                }
                server.browser.inner.lock().unwrap().died.clear();
                kick(server, &t);
            }
        }
        BrowserCmd::Stop => {
            if let Some(p) = page {
                let _ = p.send("Page.stopLoading", json!({}));
            }
        }
        BrowserCmd::Window(open) => {
            let server = server.clone();
            let pane = pane.to_string();
            tokio::spawn(async move {
                let r = if open {
                    to_window(&server, &pane).await
                } else {
                    to_pane(&server, &pane).await
                };
                if let Err(e) = r
                    && let Some(t) = server.browser.target(&pane)
                {
                    let mut st = t.st.lock().unwrap();
                    st.notice = Some(e.message.clone());
                    Target::mark_state(&mut st);
                }
            });
        }
        BrowserCmd::Screenshot => {
            let server = server.clone();
            tokio::task::spawn_blocking(move || {
                let msg = match screenshot(&server, &t) {
                    Ok(p) => format!("screenshot saved: {}", p.display()),
                    Err(e) => format!("screenshot failed: {e:#}"),
                };
                let mut st = t.st.lock().unwrap();
                st.notice = Some(msg);
                Target::mark_state(&mut st);
            });
        }
    }
}

fn history_step(page: &Page, back: bool) {
    let Ok(h) = page.call("Page.getNavigationHistory", json!({})) else {
        return;
    };
    let idx = h["currentIndex"].as_i64().unwrap_or(0);
    let j = if back { idx - 1 } else { idx + 1 };
    if let Some(e) = h["entries"]
        .as_array()
        .and_then(|a| a.get(j.max(0) as usize))
        && j >= 0
        && e["url"].as_str() != Some("about:blank")
    {
        let _ = page.send("Page.navigateToHistoryEntry", json!({"entryId": e["id"]}));
    }
}

/// Pane screenshot (B6 groundwork; `environment = LocalPane`). Metadata/evidence is Stage 4.
fn screenshot(server: &Arc<Server>, t: &Arc<Target>) -> Result<PathBuf> {
    let page = t.page().ok_or_else(|| anyhow!("the page is not running"))?;
    let png = page.capture_screenshot(true, 100)?;
    let dir = server.paths.state.join("screenshots");
    std::fs::create_dir_all(&dir)?;
    let ts = vk_store::now_ms();
    let path = dir.join(format!(
        "pane-{}-{ts}.png",
        &t.pane[t.pane.len().saturating_sub(6)..]
    ));
    std::fs::write(&path, png)?;
    Ok(path)
}

fn internal_ctx() -> Ctx {
    Ctx {
        client_id: "browser-pane".into(),
        kind: "internal".into(),
        pane_scope: None,
        remote: false,
    }
}

/// Close the headless browser on `profile` and keep it closed while a window has it (06 B3.3).
pub async fn release_profile(server: &Arc<Server>, profile: &str) {
    let proc = {
        let mut inner = server.browser.inner.lock().unwrap();
        inner.windowed.insert(profile.to_string());
        inner.procs.get(profile).cloned()
    };
    mark_profile_state(server, profile);
    if let Some(p) = proc {
        let guard = p.guard.lock().unwrap().take();
        close_proc(server, &p, "handover to window");
        if let Some(g) = guard {
            let _ = tokio::task::spawn_blocking(move || drop(g)).await;
        }
    }
    // Wait for the process to be gone (a profile can be open in only one Chromium).
    for _ in 0..50 {
        match preview::running_browser(server, profile) {
            Some((_, true)) => tokio::time::sleep(Duration::from_millis(100)).await,
            _ => break,
        }
    }
}

fn mark_profile_state(server: &Arc<Server>, profile: &str) {
    let targets: Vec<Arc<Target>> = server
        .browser
        .inner
        .lock()
        .unwrap()
        .targets
        .values()
        .cloned()
        .collect();
    for t in targets {
        let mut st = t.st.lock().unwrap();
        if st.route.profile == profile {
            Target::mark_state(&mut st);
        }
    }
}

async fn to_window(server: &Arc<Server>, pane: &str) -> Result<Value, RpcError> {
    let t = server
        .browser
        .target(pane)
        .ok_or_else(|| not_found("browser pane", pane))?;
    let (route, url) = {
        let st = t.st.lock().unwrap();
        (st.route.clone(), st.url.clone())
    };
    release_profile(server, &route.profile).await;
    let machine = if route.local() {
        String::new()
    } else {
        route.machine.clone()
    };
    let r = preview::open(
        server,
        &internal_ctx(),
        &json!({"url": url, "machine": machine, "window": true, "profile": route.profile}),
    )
    .await;
    if let Err(e) = &r {
        tracing::warn!(error = %e.message, "browser pane: window handover failed");
        back_to_pane(server, &route.profile);
        return r;
    }
    // Hand back when the window closes.
    let server2 = server.clone();
    let profile = route.profile.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if !server2.browser.is_windowed(&profile) {
                return;
            }
            if preview::running_browser(&server2, &profile).is_none() {
                back_to_pane(&server2, &profile);
                return;
            }
        }
    });
    r
}

async fn to_pane(server: &Arc<Server>, pane: &str) -> Result<Value, RpcError> {
    let t = server
        .browser
        .target(pane)
        .ok_or_else(|| not_found("browser pane", pane))?;
    let profile = t.st.lock().unwrap().route.profile.clone();
    if let Some((pid, false)) = preview::running_browser(server, &profile) {
        // SAFETY: signalling the window browser we launched on this profile.
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        for _ in 0..50 {
            if preview::running_browser(server, &profile).is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    back_to_pane(server, &profile);
    Ok(json!({"pane": pane, "profile": profile}))
}

fn back_to_pane(server: &Arc<Server>, profile: &str) {
    let targets: Vec<Arc<Target>> = {
        let mut inner = server.browser.inner.lock().unwrap();
        inner.windowed.remove(profile);
        inner.targets.values().cloned().collect()
    };
    mark_profile_state(server, profile);
    for t in targets {
        if t.st.lock().unwrap().route.profile == profile {
            kick(server, &t);
        }
    }
}

// ---- GC -------------------------------------------------------------------------------------------

fn start_gc(server: &Arc<Server>) {
    if server.browser.gc_started.swap(true, Ordering::SeqCst) {
        return;
    }
    let weak = Arc::downgrade(server);
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let Some(server) = weak.upgrade() else { return };
            gc(&server);
        }
    });
}

/// Close unviewed targets after `idle_target`, targets of closed local panes at once, and
/// browsers without targets after `idle_proc`.
pub fn gc(server: &Arc<Server>) {
    let idle_t = *server.browser.idle_target.lock().unwrap();
    let idle_p = *server.browser.idle_proc.lock().unwrap();
    let local_panes: HashSet<String> =
        server.with_core(|c| c.model.panes.iter().map(|p| p.id.clone()).collect());
    let mut close = Vec::new();
    {
        let mut inner = server.browser.inner.lock().unwrap();
        inner.targets.retain(|pane, t| {
            let st = t.st.lock().unwrap();
            let gone = t.owner.is_empty() && !local_panes.contains(pane);
            let idle = st.subs.is_empty() && st.last_viewed.elapsed() > idle_t;
            if gone || idle {
                close.push(t.clone());
                false
            } else {
                true
            }
        });
    }
    for t in close {
        let page = {
            let mut st = t.st.lock().unwrap();
            st.closed = true;
            st.screencast = false;
            st.proc.take();
            st.page.take()
        };
        if let Some(p) = page {
            let _ = p.send("Target.closeTarget", json!({"targetId": p.target_id}));
        }
    }
    let procs: Vec<Arc<Proc>> = server
        .browser
        .inner
        .lock()
        .unwrap()
        .procs
        .values()
        .cloned()
        .collect();
    for p in procs {
        let live: Vec<Arc<Target>> = p
            .sessions
            .lock()
            .unwrap()
            .values()
            .filter_map(|w| w.upgrade())
            .filter(|t| !t.st.lock().unwrap().closed)
            .collect();
        if live.is_empty() && p.launched_at.elapsed() > idle_p {
            close_proc(server, &p, "idle");
        }
    }
}

// ---- render stream ------------------------------------------------------------------------------

/// One render session's view of the media channel.
pub struct MediaSession {
    pub id: u64,
    pub notify: Arc<Notify>,
    panes: Vec<String>,
    unacked: HashMap<String, Vec<(u64, Vec<String>)>>,
    shm: bool,
    remote: bool,
    key_releases: bool,
    active: bool,
}

impl MediaSession {
    pub fn new(server: &Server, remote: bool) -> Self {
        MediaSession {
            id: server.browser.new_sub(),
            notify: Arc::new(Notify::new()),
            panes: Vec::new(),
            unacked: HashMap::new(),
            shm: false,
            remote,
            key_releases: false,
            active: false,
        }
    }

    pub fn on_view(&mut self, server: &Arc<Server>, panes: Vec<MediaPane>, shm: bool, keys: bool) {
        self.shm = shm && !self.remote;
        self.key_releases = keys;
        self.active = true;
        self.panes = panes.iter().map(|p| p.pane.clone()).collect();
        self.unacked
            .retain(|k, _| panes.iter().any(|p| &p.pane == k));
        view(server, self.id, &self.notify, &panes);
    }

    pub fn on_ack(&mut self, pane: &str, seq: u64) {
        if let Some(v) = self.unacked.get_mut(pane) {
            v.retain(|(s, _)| *s > seq);
        }
        self.notify.notify_one();
    }

    pub fn on_cmd(&self, server: &Arc<Server>, pane: &str, cmd: BrowserCmd) {
        command(server, pane, cmd, self.key_releases);
    }

    /// Write pending chrome state and media frames (call after cell frames: lower priority).
    pub async fn flush<W: AsyncWrite + Unpin>(
        &mut self,
        server: &Arc<Server>,
        wr: &mut W,
    ) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        let window = if self.remote {
            WINDOW_REMOTE
        } else {
            WINDOW_LOCAL
        };
        for pane in self.panes.clone() {
            let Some(t) = server.browser.target(&pane) else {
                continue;
            };
            let profile = t.st.lock().unwrap().route.profile.clone();
            let windowed = server.browser.is_windowed(&profile);
            if let Some(state) = t.take_state(self.id, windowed) {
                asyncio::write_frame(
                    wr,
                    &ServerFrame::BrowserState {
                        pane: pane.clone(),
                        state,
                    },
                )
                .await?;
            }
            if self.unacked.get(&pane).map_or(0, Vec::len) as u32 >= window {
                continue;
            }
            let Some(tf) = t.take(self.id) else { continue };
            let (frame, names) = encode_frame(&pane, &tf, self.shm);
            let bytes = frame
                .tiles
                .iter()
                .map(|t| match &t.data {
                    TileData::Shm { .. } => 32,
                    TileData::ZlibRgba(b) | TileData::Rgba(b) => b.len() as u64,
                })
                .sum::<u64>();
            server
                .browser
                .tiles_sent
                .fetch_add(frame.tiles.len() as u64, Ordering::Relaxed);
            server
                .browser
                .media_bytes
                .fetch_add(bytes, Ordering::Relaxed);
            self.unacked
                .entry(pane.clone())
                .or_default()
                .push((frame.seq, names));
            asyncio::write_frame(wr, &ServerFrame::Media(Box::new(frame))).await?;
        }
        Ok(())
    }

    /// The client went away: unsubscribe and remove shm objects it never handed to its host.
    pub fn close(&mut self, server: &Arc<Server>) {
        unsubscribe(server, self.id);
        for (_, v) in self.unacked.drain() {
            for (_, names) in v {
                for n in names {
                    vk_browser::kitty::shm::unlink(&n);
                }
            }
        }
    }
}

/// Encode taken tiles: shm objects for a same-machine client, else zlib RGBA.
fn encode_frame(pane: &str, tf: &TakenFrame, shm: bool) -> (MediaFrame, Vec<String>) {
    let (cw, ch) = (tf.cell.0.max(1) as u32, tf.cell.1.max(1) as u32);
    let tw = cw * TILE_COLS as u32;
    let th = ch * TILE_ROWS as u32;
    let mut names = Vec::new();
    let mut tiles = Vec::with_capacity(tf.tiles.len());
    for r in &tf.tiles {
        let px = tf.frame.extract(r);
        let data = if shm {
            let name = vk_browser::kitty::shm::new_name();
            match vk_browser::kitty::shm::write(&name, &px) {
                Ok(()) => {
                    names.push(name.clone());
                    TileData::Shm {
                        name,
                        len: px.len() as u32,
                    }
                }
                Err(_) => TileData::ZlibRgba(vk_browser::kitty::zlib(&px, 1)),
            }
        } else {
            TileData::ZlibRgba(vk_browser::kitty::zlib(&px, 1))
        };
        tiles.push(MediaTile {
            index: r.index as u32,
            col: (r.col * TILE_COLS as u32) as u16,
            row: (r.row * TILE_ROWS as u32) as u16,
            cols: r.w.div_ceil(cw) as u16,
            rows: r.h.div_ceil(ch) as u16,
            w: r.w,
            h: r.h,
            data,
        });
    }
    (
        MediaFrame {
            pane: pane.to_string(),
            seq: tf.seq,
            width: tf.frame.width,
            height: tf.frame.height,
            cell_w: tf.cell.0,
            cell_h: tf.cell.1,
            tile_cols: TILE_COLS,
            tile_rows: TILE_ROWS,
            grid_cols: tf.frame.width.div_ceil(tw) as u16,
            grid_rows: tf.frame.height.div_ceil(th) as u16,
            reset: tf.reset,
            tiles,
        },
        names,
    )
}

// ---- owner side: creating and updating browser panes ---------------------------------------------

fn split_dir(s: &str) -> Option<Option<Direction>> {
    Some(match s {
        "right" | "float" | "" => Some(Direction::Right),
        "down" => Some(Direction::Down),
        "left" => Some(Direction::Left),
        "up" => Some(Direction::Up),
        "tab" => None,
        _ => return None,
    })
}

/// `browser.pane.create {pane?, preview? | url?, split?, machine?, focus?, focus_client?}`:
/// a browser pane in this server's layout next to `pane` (default: the preview's pane, else
/// the focused pane). `split = tab` opens it in a new tab; `float` is a right split until
/// floating panes exist.
pub fn create_pane(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let (url, preview_id, task, preview_pane) = match s(p, "preview").filter(|t| !t.contains("://"))
    {
        Some(t) => {
            let mut pv = preview::find_local(server, t)?;
            if vk_preview::lifecycle::promote(&mut pv) {
                preview::commit_previews(server, vec![(pv.clone(), Some("preview.up"))]);
            }
            (
                preview::open_url_of(&pv),
                Some(pv.id.clone()),
                pv.task.clone(),
                pv.pane.clone(),
            )
        }
        None => {
            let u = s(p, "url")
                .or_else(|| s(p, "preview"))
                .ok_or_else(|| invalid("browser.pane.create needs `preview` or `url`"))?;
            (
                normalize_url(u).ok_or_else(|| invalid("only http(s) URLs can be opened"))?,
                None,
                None,
                None,
            )
        }
    };
    if let Some(scope) = &ctx.pane_scope {
        let host = preview::url_host(&url).unwrap_or_default();
        if !vk_remote::is_loopback_host(&host) {
            return Err(err(
                ErrorKind::PermissionDenied,
                "from a pane, only loopback URLs can be opened",
            )
            .details(json!({"scope": "pane"})));
        }
        let _ = scope;
    }
    let split = s(p, "split").unwrap_or("right");
    let dir =
        split_dir(split).ok_or_else(|| invalid("split must be right|down|left|up|tab|float"))?;
    let source = match s(p, "pane") {
        Some(t) => Some(resolve_pane(server, ctx, Some(t))?.id),
        None => preview_pane
            .filter(|x| server.with_core(|c| c.pane(x).is_some()))
            .or_else(|| ctx.pane_scope.clone())
            .or_else(|| server.focused_pane())
            .or_else(|| server.with_core(|c| c.model.panes.first().map(|p| p.id.clone()))),
    };
    let Some(source) = source else {
        return Err(invalid(
            "no pane to open the browser next to (create a workspace first)",
        ));
    };
    if let Some(scope) = &ctx.pane_scope {
        let owned = server.with_core(|c| {
            c.pane(&source)
                .is_some_and(|x| &x.id == scope || x.created_by == format!("agent:{scope}"))
        });
        if !owned {
            return Err(err(
                ErrorKind::PermissionDenied,
                "browser.pane.create next to a pane that is not yours",
            )
            .details(json!({"scope": "pane"})));
        }
    }
    let machine = s(p, "machine")
        .filter(|m| !preview::is_local_machine(server, m))
        .unwrap_or("")
        .to_string();
    let spec = BrowserPane {
        url: url.clone(),
        machine,
        task,
        preview: preview_id,
        source_pane: Some(source.clone()),
        history: vec![url.clone()],
        history_index: 0,
        title: String::new(),
    };
    let created_by = ctx
        .pane_scope
        .as_ref()
        .map(|x| format!("agent:{x}"))
        .unwrap_or_else(|| "user".into());
    let pane = {
        let mut c = server.core.lock().unwrap();
        let sp = c
            .pane(&source)
            .cloned()
            .ok_or_else(|| not_found("pane", &source))?;
        let ws = c
            .ws(&sp.workspace)
            .cloned()
            .ok_or_else(|| not_found("workspace", &sp.workspace))?;
        let mut tx = Tx::new();
        let (tab_id, tab_handle, new_tab) = match dir {
            Some(_) => {
                let t = c
                    .tab(&sp.tab)
                    .cloned()
                    .ok_or_else(|| not_found("tab", &sp.tab))?;
                (t.id.clone(), t.handle.clone(), None)
            }
            None => {
                let id = ulid();
                let number = c.next_tab_number(&ws.id);
                let order = c
                    .tabs_of(&ws.id)
                    .iter()
                    .map(|t| t.order)
                    .fold(0.0, f64::max)
                    + 1.0;
                let handle = format!("{}:t{number}", ws.handle);
                (
                    id.clone(),
                    handle.clone(),
                    Some((id, number, order, handle)),
                )
            }
        };
        let _ = tab_handle;
        let id = ulid();
        let handle = c.next_pane_handle(&ws.handle);
        let pane = Pane {
            id: id.clone(),
            handle,
            tab: tab_id.clone(),
            workspace: ws.id.clone(),
            title: None,
            auto_title: browser_title(&spec),
            cwd: None,
            cols: sp.cols,
            rows: sp.rows,
            child_pid: None,
            fg_cmdline: vec![],
            exited: false,
            exit_code: None,
            unread: false,
            marked_unread: false,
            pinned: false,
            created_by: created_by.clone(),
            recovered: None,
            browser: Some(spec.clone()),
            isolation: Default::default(),
        };
        tx.counters = true;
        tx.pane(pane.clone());
        tx.event(
            "pane.created",
            subject_pane(&pane),
            json!({"kind": "browser", "url": url, "source_pane": source, "split": split}),
        );
        match (dir, new_tab) {
            (Some(d), _) => {
                let mut tab = c
                    .tab(&tab_id)
                    .cloned()
                    .ok_or_else(|| not_found("tab", &tab_id))?;
                layout::split(&mut tab.layout, &source, &id, d, 0.5);
                tab.zoomed_pane = None;
                tx.event("tab.layout_changed", json!({"tab": tab.id}), json!({}));
                tx.tab(tab);
            }
            (None, Some((tid, number, order, handle))) => {
                let tab = Tab {
                    id: tid.clone(),
                    handle,
                    workspace: ws.id.clone(),
                    title: None,
                    number,
                    layout: LayoutNode::Leaf { pane: id.clone() },
                    focused_pane: Some(id.clone()),
                    zoomed_pane: None,
                    order,
                };
                tx.event(
                    "tab.created",
                    json!({"tab": tid, "workspace": ws.id}),
                    json!({"number": number}),
                );
                tx.tab(tab);
            }
            (None, None) => unreachable!(),
        }
        server
            .commit(&mut c, tx)
            .map_err(|e| err(ErrorKind::Internal, format!("{e:#}")))?;
        pane
    };
    let focus = ctx.pane_scope.is_none() && b(p, "focus").unwrap_or(true);
    if focus {
        let client = s(p, "focus_client").unwrap_or(&ctx.client_id).to_string();
        server.focus_pane(&client, &pane.id);
    }
    Ok(json!({
        "opened_in": "pane",
        "pane": pane.id,
        "pane_handle": pane.handle,
        "tab": pane.tab,
        "url": url,
        "machine": server.opts.machine,
        "source_pane": source,
    }))
}

/// `preview.open` without `--window` (06 B3.2): the pane goes into the layout of the machine
/// that owns the preview (next to its pane); the viewing client's local server renders it.
pub async fn open_pane(
    server: &Arc<Server>,
    ctx: &Ctx,
    p: &Value,
    machine: &str,
    pv: Option<&Preview>,
    url: &str,
) -> R {
    let split = s(p, "split").unwrap_or("right");
    if split_dir(split).is_none() {
        return Err(invalid("split must be right|down|left|up|tab|float"));
    }
    let mut params = json!({"split": split, "focus": b(p, "focus").unwrap_or(true)});
    match pv {
        Some(x) => params["preview"] = json!(x.id),
        None => params["url"] = json!(url),
    }
    if let Some(src) = s(p, "pane") {
        params["pane"] = json!(src);
    }
    let r = if machine.is_empty() {
        create_pane(server, ctx, &params)?
    } else {
        params["focus_client"] = json!(ctx.client_id);
        preview::remote_call(server, machine, "browser.pane.create", params).await?
    };
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        let subj = match pv {
            Some(x) => {
                json!({"machine": if machine.is_empty() { "local" } else { machine }, "preview": x.id, "preview_handle": x.handle})
            }
            None => json!({"machine": if machine.is_empty() { "local" } else { machine }}),
        };
        tx.event(
            "preview.opened",
            subj,
            json!({"url": url, "opened_in": "pane", "pane": r["pane"]}),
        );
        let _ = server.commit(&mut c, tx);
    }
    let mut out = r;
    out["machine"] = json!(if machine.is_empty() { "local" } else { machine });
    Ok(out)
}

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "browser.pane.create" => create_pane(server, ctx, p),
        "browser.pane.update" | "browser.command" if ctx.pane_scope.is_some() => Err(err(
            ErrorKind::PermissionDenied,
            format!("{method} is not allowed from a pane"),
        )
        .details(json!({"scope": "pane"}))),
        "browser.pane.update" => {
            let pane = match s(p, "pane") {
                Some(x) => x,
                None => return Some(Err(invalid("missing param `pane`"))),
            };
            let url = s(p, "url").unwrap_or("");
            if !url.is_empty() && normalize_url(url).is_none() {
                return Some(Err(invalid("only http(s) URLs")));
            }
            let history: Option<Vec<String>> = p.get("history").and_then(|h| {
                h.as_array().map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str())
                        .filter(|x| normalize_url(x).is_some())
                        .take(MAX_HISTORY)
                        .map(str::to_owned)
                        .collect()
                })
            });
            let target = match resolve_pane(server, ctx, Some(pane)) {
                Ok(x) => x,
                Err(e) => return Some(Err(e)),
            };
            if !update_pane_record(
                server,
                &target.id,
                url,
                s(p, "title"),
                history,
                u(p, "history_index").map(|x| x as usize),
            ) {
                return Some(Err(invalid("not a browser pane")));
            }
            Ok(json!({"pane": target.id}))
        }
        "browser.pane.list" => {
            let v: Vec<Value> = server.with_core(|c| {
                c.model
                    .panes
                    .iter()
                    .filter(|p| p.browser.is_some())
                    .map(|p| json!({"pane": p.id, "handle": p.handle, "tab": p.tab, "browser": p.browser}))
                    .collect()
            });
            Ok(json!({"panes": v}))
        }
        "browser.pane.status" => Ok(status_json(server)),
        "browser.command" => {
            let pane = match s(p, "pane") {
                Some(x) => x.to_string(),
                None => return Some(Err(invalid("missing param `pane`"))),
            };
            let pane = resolve_pane(server, ctx, Some(&pane))
                .map(|x| x.id)
                .unwrap_or(pane);
            if server.browser.target(&pane).is_none() {
                return Some(Err(not_found("browser pane rendered here", &pane)));
            }
            let cmd = match s(p, "cmd").unwrap_or("") {
                "back" => BrowserCmd::Back,
                "forward" => BrowserCmd::Forward,
                "reload" => BrowserCmd::Reload {
                    hard: b(p, "hard").unwrap_or(false),
                },
                "stop" => BrowserCmd::Stop,
                "navigate" => match s(p, "url") {
                    Some(u) => BrowserCmd::Navigate(u.to_string()),
                    None => return Some(Err(invalid("navigate needs `url`"))),
                },
                "screenshot" => BrowserCmd::Screenshot,
                "window" => return Some(to_window(server, &pane).await),
                "pane" => return Some(to_pane(server, &pane).await),
                "text" => BrowserCmd::Text(s(p, "text").unwrap_or("").to_string()),
                other => return Some(Err(invalid(format!("unknown browser command {other:?}")))),
            };
            command(server, &pane, cmd, false);
            Ok(json!({"pane": pane}))
        }
        // Other `browser.*` methods belong to the agent browser (agent_browser.rs).
        _ => return None,
    })
}

/// `browser.pane.status`: browsers, targets, frame rates.
pub fn status_json(server: &Server) -> Value {
    let inner = server.browser.inner.lock().unwrap();
    let procs: Vec<Value> = inner
        .procs
        .values()
        .map(|p| {
            json!({"profile": p.profile, "dpr": p.dpr, "pid": p.pid,
                   "targets": p.sessions.lock().unwrap().len(), "up_ms": p.launched_at.elapsed().as_millis() as u64})
        })
        .collect();
    let targets: Vec<Value> = inner
        .targets
        .values()
        .map(|t| {
            let st = t.st.lock().unwrap();
            let fps = if st.frame_times.len() >= 2 {
                let span = st
                    .frame_times
                    .last()
                    .unwrap()
                    .duration_since(st.frame_times[0])
                    .as_secs_f64();
                if span > 0.0 {
                    (st.frame_times.len() - 1) as f64 / span
                } else {
                    0.0
                }
            } else {
                0.0
            };
            json!({
                "pane": t.pane, "owner": t.owner, "url": st.url, "title": st.title,
                "profile": st.route.profile, "machine": st.route.machine, "route": st.route.route,
                "running": st.page.is_some(), "screencast": st.screencast,
                "viewers": st.subs.len(), "frames": st.frames_in, "fps": (fps * 10.0).round() / 10.0,
                "decode_ms": (st.decode_ms * 100.0).round() / 100.0,
                "css": [st.css.0, st.css.1],
                "frame": st.frame.as_ref().map(|f| json!([f.width, f.height])),
                "history": st.history, "error": st.error,
            })
        })
        .collect();
    json!({
        "browsers": procs,
        "targets": targets,
        "windowed": inner.windowed.iter().collect::<Vec<_>>(),
        "tiles_sent": server.browser.tiles_sent.load(Ordering::Relaxed),
        "media_bytes": server.browser.media_bytes.load(Ordering::Relaxed),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba(w: u32, h: u32, px: [u8; 4]) -> Rgba {
        let mut i = Rgba::new(w, h);
        i.fill_rect(0, 0, w, h, px);
        i
    }

    fn bare_target() -> Target {
        Target {
            pane: "P".into(),
            owner: String::new(),
            st: Mutex::new(TState {
                route: Route {
                    machine: "local".into(),
                    profile: "local".into(),
                    route: "none".into(),
                },
                page: None,
                proc: None,
                creating: false,
                closed: false,
                url: "about:blank".into(),
                title: String::new(),
                loading: false,
                history: vec![],
                history_index: 0,
                error: None,
                notice: None,
                env: String::new(),
                want: Some(Geom {
                    cols: 8,
                    rows: 4,
                    cell_w: 16,
                    cell_h: 32,
                    dpr: 2.0,
                }),
                want_at: Instant::now(),
                applied: None,
                css: (0, 0),
                screencast: false,
                frame: None,
                seq: 0,
                differ: TileDiffer::new(64, 64),
                tile_cell: (0, 0),
                subs: HashMap::new(),
                last_viewed: Instant::now(),
                frames_in: 0,
                frame_times: vec![],
                decode_ms: 0.0,
            }),
        }
    }

    fn sub(t: &Target, id: u64) {
        let mut st = t.st.lock().unwrap();
        st.subs.insert(
            id,
            Sub {
                notify: Arc::new(Notify::new()),
                reset: true,
                state_dirty: true,
                ..Default::default()
            },
        );
    }

    #[test]
    fn latest_wins_union_of_dirty_tiles() {
        let t = bare_target();
        sub(&t, 1);
        // 8×4 cells of 16×32 → 128×128 px → 2×2 tiles of 64×64.
        t.on_frame(rgba(128, 128, [255, 255, 255, 255]));
        let f = t.take(1).unwrap();
        assert!(f.reset);
        assert_eq!(f.tiles.len(), 4);
        assert!(t.take(1).is_none(), "nothing new");
        // Two frames before the client can take: tile 0 changes, then tile 3 changes.
        let mut a = rgba(128, 128, [255, 255, 255, 255]);
        a.fill_rect(0, 0, 10, 10, [0, 0, 0, 255]);
        t.on_frame(a.clone());
        let mut b = a.clone();
        b.fill_rect(100, 100, 10, 10, [9, 9, 9, 255]);
        t.on_frame(b.clone());
        let f = t.take(1).unwrap();
        assert!(!f.reset);
        let idx: Vec<usize> = f.tiles.iter().map(|r| r.index).collect();
        assert_eq!(idx, vec![0, 3]);
        assert_eq!(f.seq, 3);
        // Pixels come from the latest frame.
        assert_eq!(f.frame.pixel(105, 105), [9, 9, 9, 255]);
        // Encoding: zlib tiles with cell geometry.
        let (mf, names) = encode_frame("P", &f, false);
        assert!(names.is_empty());
        assert_eq!((mf.grid_cols, mf.grid_rows), (2, 2));
        assert_eq!(mf.tiles[1].col, 4);
        assert_eq!(mf.tiles[1].row, 2);
        assert_eq!((mf.tiles[1].cols, mf.tiles[1].rows), (4, 2));
        let TileData::ZlibRgba(z) = &mf.tiles[1].data else {
            panic!()
        };
        let back = vk_browser::kitty::unzlib(z).expect("inflate");
        assert_eq!(back.len(), 64 * 64 * 4);
        assert_eq!(&back[back.len() - 4..], &[255, 255, 255, 255]);
    }

    #[test]
    fn subscriber_isolation_and_resize_reset() {
        let t = bare_target();
        sub(&t, 1);
        t.on_frame(rgba(128, 128, [1, 1, 1, 255]));
        assert!(t.take(1).unwrap().reset);
        // A second viewer joins later: it gets a full frame, the first only changes.
        sub(&t, 2);
        let mut a = rgba(128, 128, [1, 1, 1, 255]);
        a.fill_rect(70, 0, 4, 4, [200, 0, 0, 255]);
        t.on_frame(a);
        assert_eq!(t.take(1).unwrap().tiles.len(), 1);
        let f2 = t.take(2).unwrap();
        assert!(f2.reset && f2.tiles.len() == 4);
        // A viewer that left accumulates nothing.
        t.st.lock().unwrap().subs.remove(&2);
        // Resize: everyone resets.
        t.on_frame(rgba(192, 128, [1, 1, 1, 255]));
        let f = t.take(1).unwrap();
        assert!(f.reset);
        assert_eq!(f.tiles.len(), 6);
        assert!(t.take(2).is_none());
        // Identical frame: no work.
        t.on_frame(rgba(192, 128, [1, 1, 1, 255]));
        assert!(t.take(1).is_none());
    }

    #[test]
    fn shm_encoding_creates_named_objects() {
        let t = bare_target();
        sub(&t, 1);
        t.on_frame(rgba(64, 64, [5, 6, 7, 255]));
        let f = t.take(1).unwrap();
        let (mf, names) = encode_frame("P", &f, true);
        assert_eq!(names.len(), 1);
        let TileData::Shm { name, len } = &mf.tiles[0].data else {
            panic!()
        };
        assert_eq!(*len, 64 * 64 * 4);
        let back = vk_browser::kitty::shm::read(name, *len as usize).unwrap();
        assert_eq!(&back[..4], &[5, 6, 7, 255]);
        vk_browser::kitty::shm::unlink(name);
    }

    #[test]
    fn urls() {
        assert_eq!(
            normalize_url("localhost:5173/x").as_deref(),
            Some("http://localhost:5173/x")
        );
        assert_eq!(
            normalize_url("https://a.b/").as_deref(),
            Some("https://a.b/")
        );
        assert_eq!(normalize_url("file:///etc/passwd"), None);
        assert_eq!(normalize_url("javascript:alert(1)"), None);
        assert_eq!(normalize_url("a b"), None);
        assert!(initial_url_ok("http://localhost:5173/", true));
        assert!(initial_url_ok("http://127.0.0.1:3000/", true));
        assert!(!initial_url_ok("https://evil.example/", true));
        assert!(initial_url_ok("https://example.com/", false));
        assert!(!initial_url_ok("file:///x", false));
        let b = BrowserPane {
            url: "http://localhost:5173/app".into(),
            ..Default::default()
        };
        assert_eq!(browser_title(&b), "◉ localhost:5173");
    }

    fn init_env() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let base = std::env::temp_dir().join(format!("vk-bp-tests-{}", std::process::id()));
            std::fs::create_dir_all(&base).unwrap();
            // SAFETY: set once, before any server in this test binary reads them.
            unsafe {
                std::env::set_var("VIBEKE_RUNTIME_DIR", base.join("run"));
                std::env::set_var("VIBEKE_STATE_DIR", base.join("state"));
                std::env::set_var("VIBEKE_CONFIG", base.join("config.toml"));
            }
        });
    }

    fn frames(buf: &mut Vec<u8>) -> Vec<ServerFrame> {
        let mut fb = vk_proto::frame::FrameBuf::default();
        fb.push(buf);
        buf.clear();
        let mut v = Vec::new();
        while let Some(f) = fb.next_frame::<ServerFrame>().unwrap() {
            v.push(f);
        }
        v
    }

    async fn next_media(
        server: &Arc<Server>,
        ms: &mut MediaSession,
        want: impl Fn(&MediaFrame) -> bool,
    ) -> (MediaFrame, Vec<BrowserStatus>) {
        let mut states = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let mut buf = Vec::new();
            ms.flush(server, &mut buf).await.unwrap();
            for f in frames(&mut buf) {
                match f {
                    ServerFrame::Media(m) => {
                        ms.on_ack(&m.pane, m.seq);
                        if want(&m) {
                            return (*m, states);
                        }
                    }
                    ServerFrame::BrowserState { state, .. } => states.push(state),
                    _ => {}
                }
            }
            assert!(Instant::now() < deadline, "no matching media frame");
            let _ = tokio::time::timeout(Duration::from_millis(50), ms.notify.notified()).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn media_channel_with_fake_chromium() {
        init_env();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let paths = crate::paths::Paths {
            session: "t".into(),
            runtime: root.join("run"),
            state: root.join("state"),
        };
        let opts = crate::ServerOpts {
            session: "t".into(),
            machine: "laptop".into(),
            bin: "/bin/false".into(),
            hold_args: vec![],
            default_shell: None,
            env: vec![],
            shims: false,
        };
        let server = Server::new(paths, opts).unwrap();
        server.browser.set_profiles_root(root.join("profiles"));
        let fake = Arc::new(FakeLauncher::default());
        server.browser.set_launcher(fake.clone());
        let mut ms = MediaSession::new(&server, false);
        let mp = MediaPane {
            pane: "BP1".into(),
            owner: "devbox".into(),
            spec: BrowserPane {
                url: "http://localhost:5173/".into(),
                ..Default::default()
            },
            cols: 8,
            rows: 4,
            cell_w: 16,
            cell_h: 32,
            dpr: 2.0,
        };
        // Remote owner → SOCKS route; the fake doesn't use it but the listener comes up.
        ms.on_view(&server, vec![mp.clone()], false, true);
        let (m, states) = next_media(&server, &mut ms, |m| m.reset).await;
        assert_eq!((m.width, m.height), (128, 128));
        assert_eq!((m.grid_cols, m.grid_rows), (2, 2));
        assert_eq!(m.tiles.len(), 4);
        assert!(
            matches!(m.tiles[0].data, TileData::ZlibRgba(_)),
            "no shm requested"
        );
        assert!(
            states
                .iter()
                .any(|s| s.env == "laptop chromium → devbox loopback"),
            "{states:?}"
        );
        let fstate = {
            let launched = fake.launched.lock().unwrap();
            assert_eq!(launched.len(), 1);
            assert_eq!(launched[0].0.profile, "devbox");
            assert!(launched[0].0.socks_port.is_some());
            assert!((launched[0].0.dpr - 2.0).abs() < 1e-6);
            launched[0].1.clone()
        };
        assert_eq!(
            fstate.lock().unwrap().urls(),
            vec!["http://localhost:5173/".to_string()]
        );
        assert_eq!(fstate.lock().unwrap().viewports(), vec![(64, 64)]);
        // A key reaches the page and changes its pixels.
        let seq0 = m.seq;
        ms.on_cmd(
            &server,
            "BP1",
            BrowserCmd::Key(vk_proto::input::KeyEvent::ch('a')),
        );
        let (m2, _) = next_media(&server, &mut ms, |m| m.seq > seq0 && !m.tiles.is_empty()).await;
        assert!(!m2.reset);
        let inputs = fstate.lock().unwrap().inputs();
        assert!(
            inputs
                .iter()
                .any(|(m, p)| m == "Input.dispatchKeyEvent" && p["key"] == "a" && p["text"] == "a"),
            "{inputs:?}"
        );
        // Not visible any more → screencast stops.
        ms.on_view(&server, vec![], false, true);
        let t0 = Instant::now();
        while fstate.lock().unwrap().screencasting() > 0 {
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "screencast still running"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Visible again: a full frame (reset) for the returning viewer.
        ms.on_view(&server, vec![mp.clone()], false, true);
        let (m3, _) = next_media(&server, &mut ms, |m| m.reset).await;
        assert_eq!(m3.tiles.len(), 4);
        // Resize (debounced): new viewport, reset frame with more tiles.
        let mut bigger = mp.clone();
        bigger.cols = 12;
        ms.on_view(&server, vec![bigger], false, true);
        let (m4, _) = next_media(&server, &mut ms, |m| m.reset && m.width == 192).await;
        assert_eq!((m4.grid_cols, m4.grid_rows), (3, 2));
        assert!(fstate.lock().unwrap().viewports().contains(&(96, 64)));
        let st = status_json(&server);
        assert_eq!(st["targets"][0]["viewers"], 1);
        ms.close(&server);
        assert_eq!(status_json(&server)["targets"][0]["viewers"], 0);
        // Idle GC closes the target, then the browser.
        *server.browser.idle_target.lock().unwrap() = Duration::ZERO;
        *server.browser.idle_proc.lock().unwrap() = Duration::ZERO;
        gc(&server);
        gc(&server);
        assert_eq!(status_json(&server)["targets"].as_array().unwrap().len(), 0);
        assert_eq!(
            status_json(&server)["browsers"].as_array().unwrap().len(),
            0
        );
    }
}
