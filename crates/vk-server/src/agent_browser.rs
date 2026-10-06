//! The agents' scriptable headless browser (spec 06 B5–B7, 07 §2.11 `browser.*`).
//!
//! - **One headless Chromium per machine**, owned by the server that serves the call (so an
//!   agent on the devbox drives a browser on the devbox, next to its dev server). Started lazily
//!   on the first `browser.open`, driven over `--remote-debugging-pipe`, on a fresh Vibeke
//!   profile directory under `<state>/agent-browser/profile`, stopped after `preview.browser_idle`
//!   without sessions.
//! - **Browser sessions** are isolated browser contexts (`Target.createBrowserContext`), each
//!   with one page, owned by the calling pane (and its agent run) or by the user. A pane-scoped
//!   caller can only see and drive its own sessions (and those of panes it created); sessions
//!   close when their pane closes, their run ends, or after `browser_idle` without use.
//! - **Destination filtering** (09 §8): each session's context uses its own filtering proxy
//!   ([`vk_browser::proxy`], resolved-IP checks, resolve once and pin) and a CDP `Fetch` layer
//!   that also blocks non-network schemes (`file:`) and public top-level navigations.
//!   Denials land in the session's network log, as `browser.request_denied` events, and as
//!   `destination_denied` errors for navigations.
//! - **Console/network capture** per session in bounded rings (500 entries each).
//! - **Watch / take-over groundwork** (B7): `human_control` per session; while set, pane-scoped
//!   calls on that session fail with `human_control`. The browser pane (Stage 2) attaches to a
//!   session's screencast with [`AgentBrowsers::attach_screencast`] (latest-wins frames on a
//!   `watch` channel) and forwards the human's input with [`AgentBrowsers::human_input`].

use crate::Server;
use crate::api::{Ctx, R, b, err, invalid, not_found, s, u};
use crate::core::{Tx, ulid};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use vk_browser::cdp::{Cdp, Event};
use vk_browser::policy::{self, AllowRule, External, Kind, Policy, Reason};
use vk_browser::proxy::{self, ProxyHandle, ProxyOptions};
use vk_proto::model::PreviewStatus;
use vk_proto::rpc::{ErrorData, ErrorKind, RpcError};

pub const METHODS: &[(&str, bool)] = &[
    ("browser.open", true),
    ("browser.session_open", true),
    ("browser.navigate", true),
    ("browser.click", true),
    ("browser.type", true),
    ("browser.press", true),
    ("browser.wait", false),
    ("browser.eval", true),
    ("browser.screenshot", true),
    ("browser.snapshot", false),
    ("browser.dom", false),
    ("browser.console", false),
    ("browser.network", false),
    ("browser.close", true),
    ("browser.session_close", true),
    ("browser.list", false),
    ("browser.status", false),
    ("browser.install", true),
    ("browser.take_over", true),
    ("browser.release", true),
    ("browser.attach_screencast", true),
    ("browser.detach_screencast", true),
    ("browser.screencast_frame", false),
];

/// Ring sizes (06 B5).
const RING: usize = 500;
const DENIALS: usize = 100;
/// `browser.eval` results above this are refused.
const MAX_EVAL: usize = 256 * 1024;
/// `browser.snapshot` output bound.
const MAX_SNAPSHOT: usize = 100 * 1024;
/// Screenshots above this are not inlined (`inline: true`); the blob path still works.
const MAX_INLINE: usize = 8 << 20;
/// Full-page screenshots are clipped to this height (CSS px).
const MAX_FULL_PAGE_H: f64 = 16_384.0;
/// `browser.request_denied` events: at most this many per session per window.
const EVENT_BUDGET: u32 = 20;
const EVENT_WINDOW: Duration = Duration::from_secs(10);

// ---- config ---------------------------------------------------------------------------------

/// `[preview]` keys used by the headless browser (06 Part C).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AgentBrowserConfig {
    /// Headless browser binary; empty = discover.
    pub browser_path: String,
    pub browser_idle: String,
    /// deny | subresources | allow
    pub browser_external: String,
    /// CIDRs / hosts (optionally `:port`) the headless browser may reach.
    pub browser_allow_private: Vec<String>,
    /// `WxH` CSS px.
    pub default_viewport: String,
    /// Grants `browser.eval` to pane-scoped callers (capability `browser.script`, 09 §5.2).
    pub browser_script: bool,
    /// `[browser] session_previews`: `own` (default; a session opened from a pane reaches only
    /// that pane's / task's previews) | `machine` (every declared preview of the machine).
    #[serde(skip)]
    pub session_previews: String,
}

impl Default for AgentBrowserConfig {
    fn default() -> Self {
        AgentBrowserConfig {
            browser_path: String::new(),
            browser_idle: "10m".into(),
            browser_external: "subresources".into(),
            browser_allow_private: Vec::new(),
            default_viewport: "1440x900".into(),
            browser_script: false,
            session_previews: "own".into(),
        }
    }
}

impl AgentBrowserConfig {
    pub fn load() -> Self {
        let cfg = vk_config::Config::load(vk_config::config_path())
            .map(|(c, _)| c)
            .unwrap_or_default();
        Self::from_config(&cfg)
    }

    pub fn from_config(cfg: &vk_config::Config) -> Self {
        let mut c: AgentBrowserConfig = cfg
            .extra
            .get("preview")
            .and_then(|t| serde_json::to_value(t).ok())
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        c.session_previews = cfg
            .extra
            .get("browser")
            .and_then(|b| b.get("session_previews"))
            .and_then(|v| v.as_str())
            .filter(|v| matches!(*v, "own" | "machine"))
            .unwrap_or("own")
            .to_string();
        c
    }

    /// Sessions opened from a pane are limited to that pane's / task's previews.
    pub fn own_previews_only(&self) -> bool {
        self.session_previews != "machine"
    }

    pub fn idle(&self) -> Duration {
        vk_config::Dur::parse(&self.browser_idle)
            .map(|d| d.0)
            .unwrap_or(Duration::from_secs(600))
            .max(Duration::from_secs(1))
    }

    pub fn external(&self) -> External {
        External::parse(&self.browser_external).unwrap_or_default()
    }

    pub fn allow_rules(&self) -> Vec<AllowRule> {
        self.browser_allow_private
            .iter()
            .filter_map(|r| {
                let rule = AllowRule::parse(r);
                if rule.is_none() {
                    tracing::warn!(rule = %r, "preview.browser_allow_private: ignoring invalid entry");
                }
                rule
            })
            .collect()
    }

    pub fn viewport(&self) -> (u32, u32) {
        parse_viewport(&self.default_viewport).unwrap_or((1440, 900))
    }
}

pub fn parse_viewport(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once(['x', 'X'])?;
    let (w, h): (u32, u32) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    ((100..=8192).contains(&w) && (100..=8192).contains(&h)).then_some((w, h))
}

// ---- state ----------------------------------------------------------------------------------

/// A started browser (real Chromium or a test fake).
pub struct Launched {
    pub cdp: Arc<Cdp>,
    pub events: Receiver<Event>,
    pub pid: Option<u32>,
    pub product: String,
    pub binary: String,
    pub kind: String,
    pub stop: Box<dyn FnOnce() + Send>,
}

pub struct LaunchCtx {
    pub profile_dir: PathBuf,
    pub configured: Option<String>,
    pub install_root: PathBuf,
    pub stderr_log: PathBuf,
}

pub type Launcher = Arc<dyn Fn(&LaunchCtx) -> anyhow::Result<Launched> + Send + Sync>;

struct Proc {
    cdp: Arc<Cdp>,
    pid: Option<u32>,
    product: String,
    binary: String,
    kind: String,
    stop: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    alive: Arc<AtomicBool>,
    started: Instant,
}

impl Proc {
    fn shutdown(&self) {
        self.alive.store(false, Ordering::SeqCst);
        if let Some(f) = self.stop.lock().unwrap().take() {
            f();
        }
    }
}

/// The previews a pane-opened session may reach (`[browser] session_previews = "own"`): those
/// of the pane's task, of the pane itself, and of panes it created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewScope {
    pub pane: String,
    pub task: Option<String>,
}

impl PreviewScope {
    fn owns(&self, c: &crate::core::Core, pv: &vk_proto::model::Preview) -> bool {
        crate::preview::pane_owns_preview(c, &self.pane, self.task.as_deref(), pv)
    }
}

/// One screencast frame (JPEG as Chromium sent it).
#[derive(Debug, Clone)]
pub struct Frame {
    pub seq: u64,
    pub data: Vec<u8>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub received_ms: i64,
}

#[derive(Debug, Clone)]
struct Denial {
    at: Instant,
    url: String,
    reason: &'static str,
}

pub struct Session {
    pub id: String,
    pub handle: String,
    pub owner_pane: Option<String>,
    pub owner_run: Option<String>,
    pub owner_client: String,
    pub preview: Option<String>,
    pub context_id: String,
    pub target_id: String,
    pub cdp_session: String,
    pub created_ms: i64,
    pub viewport: (u32, u32),
    /// Device scale factor and emulated `prefers-color-scheme` (screenshot environment, B6).
    pub dpr: f64,
    pub color_scheme: Option<String>,
    /// Device preset the session emulates (`iphone-15`), and whether it is a mobile device.
    pub device: Option<String>,
    pub mobile: bool,
    /// Which previews this session may reach (`None` = every declared preview of the machine).
    pub scope: Option<PreviewScope>,
    proxy: Mutex<Option<ProxyHandle>>,
    console: Mutex<VecDeque<Value>>,
    network: Mutex<VecDeque<Value>>,
    inflight: Mutex<HashMap<String, Value>>,
    denials: Mutex<VecDeque<Denial>>,
    human: Mutex<Option<String>>,
    frames: watch::Sender<Option<Arc<Frame>>>,
    frame_seq: AtomicU64,
    screencast_subs: AtomicUsize,
    url: Mutex<String>,
    main_status: Mutex<Option<u16>>,
    last_used: Mutex<Instant>,
    event_budget: Mutex<(Instant, u32)>,
    closed: AtomicBool,
}

impl Session {
    fn touch(&self) {
        *self.last_used.lock().unwrap() = Instant::now();
    }

    pub fn human_control(&self) -> Option<String> {
        self.human.lock().unwrap().clone()
    }

    fn push_ring(ring: &Mutex<VecDeque<Value>>, v: Value) {
        let mut r = ring.lock().unwrap();
        if r.len() >= RING {
            r.pop_front();
        }
        r.push_back(v);
    }

    fn summary(&self, server: &Server) -> Value {
        let pane_handle = self
            .owner_pane
            .as_ref()
            .and_then(|p| server.with_core(|c| c.pane(p).map(|x| x.handle.clone())));
        json!({
            "session": self.handle,
            "session_id": self.id,
            "owner": match &self.owner_pane { Some(p) => json!({"pane": p, "pane_handle": pane_handle, "run": self.owner_run}), None => json!({"user": self.owner_client}) },
            "preview": self.preview,
            "url": *self.url.lock().unwrap(),
            "created_ms": self.created_ms,
            "viewport": {"width": self.viewport.0, "height": self.viewport.1},
            "device": self.device,
            "previews": if self.scope.is_some() { "own" } else { "machine" },
            "human_control": self.human_control().is_some(),
            "screencast": self.screencast_subs.load(Ordering::Relaxed) > 0,
            "proxy_port": self.proxy.lock().unwrap().as_ref().map(|p| p.port),
        })
    }
}

/// A subscription to a session's screencast; dropping it detaches (and stops the screencast
/// when it was the last subscriber).
pub struct ScreencastSub {
    pub session: String,
    pub frames: watch::Receiver<Option<Arc<Frame>>>,
    sess: Arc<Session>,
    cdp: Arc<Cdp>,
}

impl Drop for ScreencastSub {
    fn drop(&mut self) {
        if self.sess.screencast_subs.fetch_sub(1, Ordering::SeqCst) == 1 {
            let _ = self.cdp.send(
                Some(&self.sess.cdp_session),
                "Page.stopScreencast",
                json!({}),
            );
        }
    }
}

#[derive(Default)]
pub struct AgentBrowsers {
    proc_: tokio::sync::Mutex<Option<Arc<Proc>>>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// CDP session id (page and auto-attached children) → browser session.
    routes: Arc<Mutex<HashMap<String, Arc<Session>>>>,
    launcher: Mutex<Option<Launcher>>,
    counter: AtomicU64,
    idle_since: Mutex<Option<Instant>>,
    loop_started: AtomicBool,
    /// Screencast subscriptions held for JSON-RPC clients (`browser.attach_screencast`).
    rpc_subs: Mutex<HashMap<String, ScreencastSub>>,
    /// `[preview]` config, re-read at most every 2 s (the Fetch layer asks per request).
    cfg_cache: Mutex<Option<(Instant, AgentBrowserConfig)>>,
    pub denied: AtomicU64,
    /// `preview.console_error` limiter (shared with browser panes).
    pub console_errors: crate::preview_console::Limiter,
}

impl AgentBrowsers {
    pub fn config(&self) -> AgentBrowserConfig {
        let mut g = self.cfg_cache.lock().unwrap();
        if let Some((at, c)) = g.as_ref()
            && at.elapsed() < Duration::from_secs(2)
        {
            return c.clone();
        }
        let c = AgentBrowserConfig::load();
        *g = Some((Instant::now(), c.clone()));
        c
    }

    /// Test hook / embedding: how the browser is started.
    pub fn set_launcher(&self, l: Launcher) {
        *self.launcher.lock().unwrap() = Some(l);
    }

    fn session(&self, target: &str) -> Option<Arc<Session>> {
        let m = self.sessions.lock().unwrap();
        m.get(target)
            .cloned()
            .or_else(|| m.values().find(|x| x.handle == target).cloned())
    }

    pub fn sessions(&self) -> Vec<Arc<Session>> {
        let mut v: Vec<Arc<Session>> = self.sessions.lock().unwrap().values().cloned().collect();
        v.sort_by_key(|x| x.created_ms);
        v
    }

    /// Attach to a session's screencast (for the Stage 2 browser pane's watch view). Frames
    /// are latest-wins; the screencast runs while at least one subscriber exists.
    pub async fn attach_screencast(&self, target: &str) -> Result<ScreencastSub, RpcError> {
        let sess = self
            .session(target)
            .ok_or_else(|| not_found("browser_session", target))?;
        let cdp = self.cdp().await?;
        if sess.screencast_subs.fetch_add(1, Ordering::SeqCst) == 0 {
            let (w, h) = sess.viewport;
            if let Err(e) = call(
                &cdp,
                Some(&sess.cdp_session),
                "Page.startScreencast",
                json!({"format": "jpeg", "quality": 80, "maxWidth": w, "maxHeight": h, "everyNthFrame": 1}),
            )
            .await
            {
                sess.screencast_subs.fetch_sub(1, Ordering::SeqCst);
                return Err(e);
            }
        }
        Ok(ScreencastSub {
            session: sess.handle.clone(),
            frames: sess.frames.subscribe(),
            sess,
            cdp,
        })
    }

    /// Forward the human's input to a session they have taken over.
    pub async fn human_input(
        &self,
        target: &str,
        cmds: &[vk_browser::input::CdpInput],
    ) -> Result<(), RpcError> {
        let sess = self
            .session(target)
            .ok_or_else(|| not_found("browser_session", target))?;
        let cdp = self.cdp().await?;
        for c in cmds {
            let (m, p) = c.to_command();
            call(&cdp, Some(&sess.cdp_session), m, p).await?;
        }
        Ok(())
    }

    async fn cdp(&self) -> Result<Arc<Cdp>, RpcError> {
        self.proc_
            .lock()
            .await
            .as_ref()
            .filter(|p| p.alive.load(Ordering::SeqCst))
            .map(|p| p.cdp.clone())
            .ok_or_else(|| err(ErrorKind::Conflict, "the headless browser is not running"))
    }
}

// ---- errors ---------------------------------------------------------------------------------

fn custom(kind: ErrorKind, name: &str, msg: impl Into<String>, details: Value) -> RpcError {
    RpcError {
        code: kind.code(),
        message: msg.into(),
        data: ErrorData {
            kind: name.into(),
            details,
            retryable: name == "human_control",
        },
    }
}

fn destination_denied(url: &str, reason: &str) -> RpcError {
    custom(
        ErrorKind::PermissionDenied,
        "destination_denied",
        format!(
            "destination_denied: {url} ({reason}); only this machine's declared previews (and preview.browser_allow_private) are reachable"
        ),
        json!({"url": url, "reason": reason}),
    )
}

fn human_control_err(sess: &Session) -> RpcError {
    custom(
        ErrorKind::Conflict,
        "human_control",
        format!(
            "human_control: a human has taken over browser session {}; wait until they release it",
            sess.handle
        ),
        json!({"session": sess.handle}),
    )
}

fn pane_denied(method: &str, why: &str) -> RpcError {
    err(
        ErrorKind::PermissionDenied,
        format!("{method} is not allowed from a pane ({why})"),
    )
    .details(json!({"scope": "pane"}))
}

fn cdp_err(e: anyhow::Error) -> RpcError {
    let s = format!("{e:#}");
    if s.contains("connection closed") {
        return err(ErrorKind::Conflict, format!("headless browser exited: {s}"));
    }
    err(ErrorKind::Internal, s)
}

async fn call(cdp: &Arc<Cdp>, session: Option<&str>, method: &str, params: Value) -> R {
    call_timeout(cdp, session, method, params, vk_browser::cdp::CALL_TIMEOUT).await
}

async fn call_timeout(
    cdp: &Arc<Cdp>,
    session: Option<&str>,
    method: &str,
    params: Value,
    timeout: Duration,
) -> R {
    let cdp = cdp.clone();
    let session = session.map(str::to_string);
    let method = method.to_string();
    tokio::task::spawn_blocking(move || {
        cdp.call_timeout(session.as_deref(), &method, params, timeout)
    })
    .await
    .map_err(|e| err(ErrorKind::Internal, e.to_string()))?
    .map_err(cdp_err)
}

// ---- launch, idle, teardown -----------------------------------------------------------------

fn profile_dir(server: &Server) -> PathBuf {
    server.paths.state.join("agent-browser").join("profile")
}

pub fn install_root() -> PathBuf {
    crate::paths::data_root().join("browsers")
}

fn default_launcher() -> Launcher {
    Arc::new(|ctx: &LaunchCtx| {
        let bin = vk_browser::headless::discover(ctx.configured.as_deref(), &ctx.install_root)
            .ok_or_else(|| anyhow::anyhow!("no Chromium found (preview.browser_path, Playwright cache, system Chromium/Chrome); run `vibeke browser install`"))?;
        let opts = vk_browser::headless::launch_options(
            &bin,
            &ctx.profile_dir,
            Some(ctx.stderr_log.clone()),
        );
        let mut browser = vk_browser::cdp::Browser::launch(&opts)?;
        let events = browser.take_events();
        let product = browser
            .version()
            .ok()
            .and_then(|v| v["product"].as_str().map(str::to_string))
            .unwrap_or_default();
        let pid = browser.pid();
        let cdp = browser.cdp.clone();
        let holder = Mutex::new(Some(browser));
        Ok(Launched {
            cdp,
            events,
            pid: Some(pid),
            product,
            binary: bin.path.display().to_string(),
            kind: bin.kind,
            stop: Box::new(move || {
                if let Some(b) = holder.lock().unwrap().take() {
                    let _ = b.close();
                }
            }),
        })
    })
}

async fn ensure_proc(server: &Arc<Server>) -> Result<Arc<Proc>, RpcError> {
    let ab = &server.agent_browser;
    let mut g = ab.proc_.lock().await;
    if let Some(p) = g.as_ref()
        && p.alive.load(Ordering::SeqCst)
    {
        return Ok(p.clone());
    }
    let cfg = server.agent_browser.config();
    let dir = profile_dir(server);
    // A fresh profile each launch: sessions are ephemeral contexts anyway.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(crate::api::internal)?;
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some(parent) = dir.parent() {
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let ctx = LaunchCtx {
        profile_dir: dir.clone(),
        configured: Some(cfg.browser_path.clone()).filter(|p| !p.is_empty()),
        install_root: install_root(),
        stderr_log: server.paths.logs().join("agent-browser.log"),
    };
    let launcher = ab
        .launcher
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(default_launcher);
    let launched = tokio::task::spawn_blocking(move || launcher(&ctx))
        .await
        .map_err(|e| err(ErrorKind::Internal, e.to_string()))?
        .map_err(|e| {
            let s = format!("{e:#}");
            if s.contains("no Chromium found") {
                err(ErrorKind::Unsupported, s)
                    .details(json!({"fallback": "vibeke browser install"}))
            } else {
                err(
                    ErrorKind::Internal,
                    format!("starting the headless browser: {s}"),
                )
            }
        })?;
    let alive = Arc::new(AtomicBool::new(true));
    let p = Arc::new(Proc {
        cdp: launched.cdp.clone(),
        pid: launched.pid,
        product: launched.product,
        binary: launched.binary,
        kind: launched.kind,
        stop: Mutex::new(Some(launched.stop)),
        alive: alive.clone(),
        started: Instant::now(),
    });
    let rt = tokio::runtime::Handle::current();
    let srv = server.clone();
    let cdp = launched.cdp.clone();
    let events = launched.events;
    std::thread::Builder::new()
        .name("agent-browser-events".into())
        .spawn(move || event_loop(srv, cdp, events, alive, rt))
        .map_err(crate::api::internal)?;
    tracing::info!(pid = ?p.pid, product = %p.product, binary = %p.binary, "agent headless browser started");
    *g = Some(p.clone());
    *ab.idle_since.lock().unwrap() = None;
    if !ab.loop_started.swap(true, Ordering::SeqCst) {
        tokio::spawn(idle_loop(server.clone()));
    }
    Ok(p)
}

async fn idle_loop(server: Arc<Server>) {
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let cfg = server.agent_browser.config();
        let idle = cfg.idle();
        for sess in server.agent_browser.sessions() {
            let why = server.with_core(|c| {
                if let Some(p) = &sess.owner_pane
                    && c.pane(p).is_none()
                {
                    return Some("owner_pane_closed");
                }
                if let Some(r) = &sess.owner_run
                    && c.run(r).is_none_or(|r| r.ended_at_ms.is_some())
                {
                    return Some("owner_run_ended");
                }
                None
            });
            let why = why.or_else(|| {
                let unused = sess.last_used.lock().unwrap().elapsed() > idle;
                (unused
                    && sess.screencast_subs.load(Ordering::Relaxed) == 0
                    && sess.human_control().is_none())
                .then_some("idle")
            });
            if let Some(why) = why {
                close_session(&server, &sess, why).await;
            }
        }
        let ab = &server.agent_browser;
        let mut g = ab.proc_.lock().await;
        let Some(p) = g.clone() else { continue };
        if !p.alive.load(Ordering::SeqCst) {
            *g = None;
            continue;
        }
        if !ab.sessions.lock().unwrap().is_empty() {
            *ab.idle_since.lock().unwrap() = None;
            continue;
        }
        let since = *ab
            .idle_since
            .lock()
            .unwrap()
            .get_or_insert_with(Instant::now);
        if since.elapsed() >= idle {
            tracing::info!("agent headless browser idle for {idle:?}: stopping");
            *g = None;
            drop(g);
            tokio::task::spawn_blocking(move || p.shutdown());
        }
    }
}

/// A client connection went away (EOF, crash, error): drop the screencast subscriptions it
/// held, so the idle collector can close their sessions. (A take-over is a human decision
/// that outlives the CLI call that made it; `browser.release` ends it.)
pub fn client_gone(server: &Server, client_id: &str) {
    let prefix = format!("{client_id}:");
    let gone: Vec<ScreencastSub> = {
        let mut subs = server.agent_browser.rpc_subs.lock().unwrap();
        let keys: Vec<String> = subs
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect();
        keys.iter().filter_map(|k| subs.remove(k)).collect()
    };
    // Dropping detaches (and stops the screencast after the last subscriber).
    drop(gone);
}

async fn close_session(server: &Arc<Server>, sess: &Arc<Session>, why: &str) {
    if sess.closed.swap(true, Ordering::SeqCst) {
        return;
    }
    let ab = &server.agent_browser;
    ab.sessions.lock().unwrap().remove(&sess.id);
    ab.routes.lock().unwrap().retain(|_, v| v.id != sess.id);
    ab.rpc_subs
        .lock()
        .unwrap()
        .retain(|_, v| v.sess.id != sess.id);
    if let Ok(cdp) = ab.cdp().await {
        let _ = call_timeout(
            &cdp,
            None,
            "Target.disposeBrowserContext",
            json!({"browserContextId": sess.context_id}),
            Duration::from_secs(5),
        )
        .await;
    }
    sess.proxy.lock().unwrap().take();
    if ab.sessions.lock().unwrap().is_empty() {
        *ab.idle_since.lock().unwrap() = Some(Instant::now());
    }
    emit(
        server,
        "browser.session_closed",
        sess,
        json!({"reason": why}),
    );
}

fn emit(server: &Server, kind: &str, sess: &Session, data: Value) {
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.event(
        kind,
        json!({"browser_session": sess.handle, "session_id": sess.id, "pane": sess.owner_pane, "machine": server.opts.machine}),
        data,
    );
    let _ = server.commit(&mut c, tx);
}

// ---- policy & denials -----------------------------------------------------------------------

/// The live policy for this machine: declared previews' ports + config.
pub fn current_policy(server: &Server, cfg: &AgentBrowserConfig) -> Policy {
    session_policy(server, cfg, None)
}

/// The policy for one session: with a [`PreviewScope`] only that scope's previews are
/// reachable, the machine's other previews are `foreign_preview`.
pub fn session_policy(
    server: &Server,
    cfg: &AgentBrowserConfig,
    scope: Option<&PreviewScope>,
) -> Policy {
    let (own, foreign) = server.with_core(|c| {
        let mut own = std::collections::BTreeSet::new();
        let mut foreign = std::collections::BTreeSet::new();
        for p in c.model.previews.iter().filter(|p| {
            matches!(
                p.status,
                PreviewStatus::Declared | PreviewStatus::Up | PreviewStatus::Down
            )
        }) {
            if scope.is_none_or(|s| s.owns(c, p)) {
                own.insert(p.port);
            } else {
                foreign.insert(p.port);
            }
        }
        foreign.retain(|port| !own.contains(port));
        (own, foreign)
    });
    Policy {
        preview_ports: own,
        foreign_ports: foreign,
        allow: cfg.allow_rules(),
        external: cfg.external(),
    }
}

fn record_denial(
    server: &Server,
    sess: &Session,
    url: &str,
    reason: &'static str,
    layer: &str,
    resource_type: Option<&str>,
) {
    server.agent_browser.denied.fetch_add(1, Ordering::Relaxed);
    tracing::info!(session = %sess.handle, %url, reason, layer, "browser: request denied");
    Session::push_ring(
        &sess.network,
        json!({"ts": vk_store::now_ms(), "method": null, "url": url, "type": resource_type, "status": null,
               "error": "blocked by Vibeke destination policy", "blocked_by_policy": reason, "layer": layer}),
    );
    {
        let mut d = sess.denials.lock().unwrap();
        if d.len() >= DENIALS {
            d.pop_front();
        }
        d.push_back(Denial {
            at: Instant::now(),
            url: url.to_string(),
            reason,
        });
    }
    let send = {
        let mut b = sess.event_budget.lock().unwrap();
        if b.0.elapsed() > EVENT_WINDOW {
            *b = (Instant::now(), 0);
        }
        b.1 += 1;
        b.1 <= EVENT_BUDGET
    };
    if send {
        emit(
            server,
            "browser.request_denied",
            sess,
            json!({"session": sess.handle, "url": url, "reason": reason, "layer": layer, "resource_type": resource_type}),
        );
    }
}

/// The Fetch layer's decision for one paused request. `None` = continue.
async fn fetch_decision(
    server: &Server,
    scope: Option<&PreviewScope>,
    url: &str,
    resource_type: &str,
    main_frame: bool,
) -> Option<&'static str> {
    if policy::is_local_scheme(url) {
        return None;
    }
    let Some(t) = policy::parse_target(url) else {
        return Some(Reason::Scheme.as_str());
    };
    let cfg = server.agent_browser.config();
    let pol = session_policy(server, &cfg, scope);
    let kind = if main_frame && resource_type == "Document" {
        Kind::Navigation
    } else {
        Kind::Subresource
    };
    let literal = t.host.parse::<std::net::IpAddr>().is_ok() || policy::is_localhost_name(&t.host);
    // Names are resolved (and pinned) by the proxy; here only when the answer decides between
    // a public top-level navigation and an internal one.
    if !literal && !(kind == Kind::Navigation && pol.external == External::Subresources) {
        return None;
    }
    let ips = proxy::resolve_with(&proxy::SystemResolver, &t.host, t.port)
        .await
        .unwrap_or_default();
    let d = pol.decide(&t.host, t.port, &ips, kind);
    (!d.allow).then_some(d.reason.as_str())
}

// ---- CDP events -----------------------------------------------------------------------------

fn event_loop(
    server: Arc<Server>,
    cdp: Arc<Cdp>,
    events: Receiver<Event>,
    alive: Arc<AtomicBool>,
    rt: tokio::runtime::Handle,
) {
    let routes = server.agent_browser.routes.clone();
    while let Ok(ev) = events.recv() {
        let Some(sid) = ev.session_id.clone() else {
            continue;
        };
        let sess = routes.lock().unwrap().get(&sid).cloned();
        let Some(sess) = sess else {
            if ev.method == "Fetch.requestPaused" {
                // Not a session we know (shouldn't happen): fail closed.
                let _ = cdp.send(
                    Some(&sid),
                    "Fetch.failRequest",
                    json!({"requestId": ev.params["requestId"], "errorReason": "BlockedByClient"}),
                );
            }
            continue;
        };
        on_event(&server, &cdp, &routes, &sess, &sid, ev, &rt);
    }
    alive.store(false, Ordering::SeqCst);
    tracing::info!("agent headless browser: event stream closed");
    let srv = server.clone();
    rt.spawn(async move {
        for sess in srv.agent_browser.sessions() {
            close_session(&srv, &sess, "browser_exited").await;
        }
    });
}

fn console_level(t: &str) -> &'static str {
    match t {
        "error" | "assert" => "error",
        "warning" | "warn" => "warn",
        "debug" | "verbose" => "debug",
        "info" => "info",
        _ => "log",
    }
}

fn remote_object_text(o: &Value) -> String {
    if let Some(v) = o.get("value") {
        return match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
    }
    o.get("description")
        .or_else(|| o.get("unserializableValue"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| o["type"].as_str().unwrap_or(""))
        .to_string()
}

/// The preview a session is looking at: the one it was opened on, else the one whose port its
/// current page is served from.
fn session_preview(server: &Server, sess: &Session) -> Option<String> {
    if let Some(p) = &sess.preview {
        return Some(p.clone());
    }
    let url = sess.url.lock().unwrap().clone();
    let t = policy::parse_target(&url)?;
    if !policy::is_localhost_name(&t.host) && t.host != "127.0.0.1" && t.host != "::1" {
        return None;
    }
    server.with_core(|c| {
        c.model
            .previews
            .iter()
            .find(|p| p.port == t.port && p.status != PreviewStatus::Gone)
            .map(|p| p.handle.clone())
    })
}

/// `preview.console_error` for an uncaught exception / `console.error` on a preview.
fn report_console_error(server: &Server, sess: &Session, method: &str, params: &Value) {
    let Some((source, text, url, line)) = crate::preview_console::parse_event(method, params)
    else {
        return;
    };
    let Some(preview) = session_preview(server, sess) else {
        return;
    };
    crate::preview_console::report(
        server,
        &preview,
        sess.owner_pane.as_deref(),
        Some(&sess.handle),
        &crate::preview_console::ConsoleError {
            source,
            text: &text,
            url: url.as_deref(),
            line,
        },
    );
}

fn on_event(
    server: &Arc<Server>,
    cdp: &Arc<Cdp>,
    routes: &Arc<Mutex<HashMap<String, Arc<Session>>>>,
    sess: &Arc<Session>,
    sid: &str,
    ev: Event,
    rt: &tokio::runtime::Handle,
) {
    let p = &ev.params;
    match ev.method.as_str() {
        "Fetch.requestPaused" => {
            let url = p["request"]["url"].as_str().unwrap_or("").to_string();
            let rtype = p["resourceType"].as_str().unwrap_or("").to_string();
            let main = p["frameId"].as_str() == Some(sess.target_id.as_str());
            let req_id = p["requestId"].clone();
            let (srv, cdp, sess, sid) =
                (server.clone(), cdp.clone(), sess.clone(), sid.to_string());
            rt.spawn(async move {
                match fetch_decision(&srv, sess.scope.as_ref(), &url, &rtype, main).await {
                    None => {
                        let _ = cdp.send(
                            Some(&sid),
                            "Fetch.continueRequest",
                            json!({"requestId": req_id}),
                        );
                    }
                    Some(reason) => {
                        let _ = cdp.send(
                            Some(&sid),
                            "Fetch.failRequest",
                            json!({"requestId": req_id, "errorReason": "BlockedByClient"}),
                        );
                        record_denial(&srv, &sess, &url, reason, "fetch", Some(&rtype));
                    }
                }
            });
        }
        "Target.attachedToTarget" => {
            let child = p["sessionId"].as_str().unwrap_or("").to_string();
            let ctx = p["targetInfo"]["browserContextId"].as_str().unwrap_or("");
            if child.is_empty() {
                return;
            }
            if ctx == sess.context_id {
                routes.lock().unwrap().insert(child.clone(), sess.clone());
                let c = Some(child.as_str());
                let _ = cdp.send(
                    c,
                    "Fetch.enable",
                    json!({"patterns": [{"urlPattern": "*"}]}),
                );
                let _ = cdp.send(c, "Runtime.enable", json!({}));
                let _ = cdp.send(c, "Network.enable", json!({}));
                let _ = cdp.send(
                    c,
                    "Target.setAutoAttach",
                    json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true}),
                );
                let _ = cdp.send(c, "Runtime.runIfWaitingForDebugger", json!({}));
            } else {
                // Not in this session's context: let it run, stop watching it.
                let _ = cdp.send(Some(&child), "Runtime.runIfWaitingForDebugger", json!({}));
                let _ = cdp.send(
                    Some(sid),
                    "Target.detachFromTarget",
                    json!({"sessionId": child}),
                );
            }
        }
        "Target.detachedFromTarget" => {
            if let Some(child) = p["sessionId"].as_str()
                && child != sess.cdp_session
            {
                routes.lock().unwrap().remove(child);
            }
        }
        "Runtime.consoleAPICalled" => {
            let text = p["args"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(remote_object_text)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            let frame = &p["stackTrace"]["callFrames"][0];
            Session::push_ring(
                &sess.console,
                json!({"ts": vk_store::now_ms(), "level": console_level(p["type"].as_str().unwrap_or("log")), "text": text,
                       "source": "console", "url": frame["url"], "line": frame["lineNumber"]}),
            );
            report_console_error(server, sess, &ev.method, p);
        }
        "Runtime.exceptionThrown" => {
            let d = &p["exceptionDetails"];
            let text = d["exception"]["description"]
                .as_str()
                .or_else(|| d["text"].as_str())
                .unwrap_or("exception")
                .to_string();
            Session::push_ring(
                &sess.console,
                json!({"ts": vk_store::now_ms(), "level": "error", "text": text, "source": "exception", "url": d["url"], "line": d["lineNumber"]}),
            );
            report_console_error(server, sess, &ev.method, p);
        }
        "Log.entryAdded" => {
            let e = &p["entry"];
            Session::push_ring(
                &sess.console,
                json!({"ts": vk_store::now_ms(), "level": console_level(e["level"].as_str().unwrap_or("info")), "text": e["text"],
                       "source": e["source"], "url": e["url"], "line": e["lineNumber"]}),
            );
        }
        "Network.requestWillBeSent" => {
            let id = p["requestId"].as_str().unwrap_or("").to_string();
            let mut inflight = sess.inflight.lock().unwrap();
            if inflight.len() > 2000 {
                inflight.clear();
            }
            inflight.insert(
                id,
                json!({"ts": vk_store::now_ms(), "method": p["request"]["method"], "url": p["request"]["url"],
                       "type": p["type"], "status": null, "error": null}),
            );
            if p["type"] == "Document" && p["frameId"].as_str() == Some(sess.target_id.as_str()) {
                *sess.main_status.lock().unwrap() = None;
            }
        }
        "Network.responseReceived" => {
            let id = p["requestId"].as_str().unwrap_or("");
            let status = p["response"]["status"].as_u64();
            if let Some(e) = sess.inflight.lock().unwrap().get_mut(id) {
                e["status"] = json!(status);
                e["mime"] = p["response"]["mimeType"].clone();
            }
            if p["type"] == "Document" && p["frameId"].as_str() == Some(sess.target_id.as_str()) {
                *sess.main_status.lock().unwrap() = status.map(|s| s as u16);
            }
        }
        "Network.loadingFinished" | "Network.loadingFailed" => {
            let id = p["requestId"].as_str().unwrap_or("");
            let mut e = sess
                .inflight
                .lock()
                .unwrap()
                .remove(id)
                .unwrap_or_else(|| json!({"ts": vk_store::now_ms(), "type": p["type"]}));
            if ev.method == "Network.loadingFailed" {
                e["error"] = p["errorText"].clone();
                if p["blockedReason"].is_string() {
                    e["blocked_reason"] = p["blockedReason"].clone();
                }
            }
            e["duration_ms"] = json!(vk_store::now_ms() - e["ts"].as_i64().unwrap_or(0));
            Session::push_ring(&sess.network, e);
        }
        "Page.frameNavigated" => {
            let f = &p["frame"];
            // Error pages (`chrome-error://`) keep the last real URL for relative paths.
            if f.get("parentId").is_none()
                && let Some(u) = f["url"].as_str()
                && !u.starts_with("chrome-error:")
            {
                *sess.url.lock().unwrap() = u.to_string();
            }
        }
        "Page.javascriptDialogOpening" => {
            Session::push_ring(
                &sess.console,
                json!({"ts": vk_store::now_ms(), "level": "warn", "text": format!("{} dialog dismissed: {}", p["type"].as_str().unwrap_or("js"), p["message"].as_str().unwrap_or("")), "source": "dialog"}),
            );
            let _ = cdp.send(
                Some(sid),
                "Page.handleJavaScriptDialog",
                json!({"accept": false}),
            );
        }
        "Page.fileChooserOpened" => {
            Session::push_ring(
                &sess.console,
                json!({"ts": vk_store::now_ms(), "level": "warn", "text": "file chooser cancelled (file uploads are not available to agents)", "source": "dialog"}),
            );
        }
        "Inspector.targetCrashed" => {
            Session::push_ring(
                &sess.console,
                json!({"ts": vk_store::now_ms(), "level": "error", "text": "page crashed", "source": "browser"}),
            );
        }
        "Page.screencastFrame" => {
            let _ = cdp.send(
                Some(sid),
                "Page.screencastFrameAck",
                json!({"sessionId": p["sessionId"]}),
            );
            if let Ok(f) = vk_browser::cdp::ScreencastFrame::from_event(&ev) {
                let seq = sess.frame_seq.fetch_add(1, Ordering::Relaxed) + 1;
                let md = &p["metadata"];
                let _ = sess.frames.send(Some(Arc::new(Frame {
                    seq,
                    data: f.data,
                    width: md["deviceWidth"].as_f64().map(|w| w as u32),
                    height: md["deviceHeight"].as_f64().map(|h| h as u32),
                    received_ms: vk_store::now_ms(),
                })));
            }
        }
        _ => {}
    }
}

// ---- API ------------------------------------------------------------------------------------

fn owns(server: &Server, ctx: &Ctx, sess: &Session) -> bool {
    let Some(scope) = &ctx.pane_scope else {
        return true;
    };
    let Some(owner) = &sess.owner_pane else {
        return false;
    };
    owner == scope
        || server.with_core(|c| {
            c.pane(owner)
                .is_some_and(|p| p.created_by == format!("agent:{scope}"))
        })
}

fn session_param(p: &Value) -> Option<&str> {
    s(p, "session")
        .or_else(|| s(p, "browser_session"))
        .or_else(|| s(p, "target"))
}

/// Find the session and check the caller may drive it.
fn target_session(
    server: &Server,
    ctx: &Ctx,
    method: &str,
    p: &Value,
) -> Result<Arc<Session>, RpcError> {
    let t = session_param(p).ok_or_else(|| invalid("missing param `session`"))?;
    let sess = server
        .agent_browser
        .session(t)
        .ok_or_else(|| not_found("browser_session", t))?;
    if !owns(server, ctx, &sess) {
        // Don't reveal other agents' sessions.
        return Err(not_found("browser_session", t));
    }
    if ctx.pane_scope.is_some() && sess.human_control().is_some() {
        return Err(human_control_err(&sess));
    }
    let _ = method;
    sess.touch();
    Ok(sess)
}

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    if !method.starts_with("browser.") {
        return None;
    }
    if ctx.pane_scope.is_some() {
        match method {
            "browser.install"
            | "browser.take_over"
            | "browser.release"
            | "browser.attach_screencast"
            | "browser.detach_screencast"
            | "browser.screencast_frame" => {
                return Some(Err(pane_denied(method, "human-only action")));
            }
            "browser.eval" if !server.agent_browser.config().browser_script => {
                return Some(Err(err(
                    ErrorKind::PermissionDenied,
                    "browser.eval needs the browser.script capability (preview.browser_script = true)",
                )
                .details(json!({"scope": "pane", "capability": "browser.script"}))));
            }
            _ => {}
        }
    }
    Some(match method {
        "browser.open" | "browser.session_open" => open(server, ctx, p).await,
        "browser.navigate" => match target_session(server, ctx, method, p) {
            Ok(sess) => {
                let to = match s(p, "url").or_else(|| s(p, "path")) {
                    Some(t) => t.to_string(),
                    None => return Some(Err(invalid("missing param `url`"))),
                };
                navigate(server, &sess, &to, p).await
            }
            Err(e) => Err(e),
        },
        "browser.click" => with_session(server, ctx, method, p, click).await,
        "browser.type" => with_session(server, ctx, method, p, type_text).await,
        "browser.press" => with_session(server, ctx, method, p, press).await,
        "browser.wait" => with_session(server, ctx, method, p, wait).await,
        "browser.eval" => with_session(server, ctx, method, p, eval).await,
        "browser.screenshot" if session_param(p).is_none() => {
            one_shot_screenshot(server, ctx, p).await
        }
        "browser.screenshot" => match target_session(server, ctx, method, p) {
            Ok(sess) => screenshot(server, ctx, &sess, p).await,
            Err(e) => Err(e),
        },
        "browser.snapshot" | "browser.dom" => with_session(server, ctx, method, p, snapshot).await,
        "browser.console" => target_session(server, ctx, method, p).map(|x| console(&x, p)),
        "browser.network" => target_session(server, ctx, method, p).map(|x| network(&x, p)),
        "browser.close" | "browser.session_close" => match target_session(server, ctx, method, p) {
            Ok(sess) => {
                close_session(server, &sess, "closed").await;
                Ok(json!({"session": sess.handle, "closed": true}))
            }
            Err(e) => Err(e),
        },
        "browser.list" => Ok(list(server, ctx).await),
        "browser.status" => Ok(status(server).await),
        "browser.install" => install(p).await,
        "browser.take_over" | "browser.release" => {
            let t = match session_param(p) {
                Some(t) => t,
                None => return Some(Err(invalid("missing param `session`"))),
            };
            match server.agent_browser.session(t) {
                None => Err(not_found("browser_session", t)),
                Some(sess) => {
                    let on = method == "browser.take_over";
                    *sess.human.lock().unwrap() = on.then(|| ctx.client_id.clone());
                    sess.touch();
                    emit(
                        server,
                        if on {
                            "browser.taken_over"
                        } else {
                            "browser.released"
                        },
                        &sess,
                        json!({"by": ctx.client_id}),
                    );
                    Ok(json!({"session": sess.handle, "human_control": on}))
                }
            }
        }
        "browser.attach_screencast" => {
            let t = match session_param(p) {
                Some(t) => t.to_string(),
                None => return Some(Err(invalid("missing param `session`"))),
            };
            match server.agent_browser.attach_screencast(&t).await {
                Ok(sub) => {
                    let (w, h) = sub.sess.viewport;
                    let handle = sub.session.clone();
                    let key = format!("{}:{}", ctx.client_id, handle);
                    server
                        .agent_browser
                        .rpc_subs
                        .lock()
                        .unwrap()
                        .insert(key, sub);
                    Ok(
                        json!({"session": handle, "delivery": "internal+poll", "width": w, "height": h,
                              "poll": "browser.screencast_frame"}),
                    )
                }
                Err(e) => Err(e),
            }
        }
        "browser.detach_screencast" => {
            let t = session_param(p).unwrap_or("");
            let prefix = format!("{}:", ctx.client_id);
            server
                .agent_browser
                .rpc_subs
                .lock()
                .unwrap()
                .retain(|k, v| !(k.starts_with(&prefix) && (v.session == t || v.sess.id == t)));
            Ok(json!({"session": t, "detached": true}))
        }
        "browser.screencast_frame" => {
            let t = session_param(p).unwrap_or("");
            match server.agent_browser.session(t) {
                None => Err(not_found("browser_session", t)),
                Some(sess) => {
                    use base64::Engine as _;
                    let f = sess.frames.borrow().clone();
                    Ok(match f {
                        Some(f) if u(p, "after_seq").is_none_or(|a| f.seq > a) => json!({
                            "session": sess.handle, "seq": f.seq, "mime": "image/jpeg",
                            "width": f.width, "height": f.height, "received_ms": f.received_ms,
                            "data_b64": base64::engine::general_purpose::STANDARD.encode(&f.data),
                        }),
                        _ => {
                            json!({"session": sess.handle, "seq": f.map(|f| f.seq), "data_b64": null})
                        }
                    })
                }
            }
        }
        _ => Err(err(
            ErrorKind::MethodNotFound,
            format!("unknown method {method}"),
        )),
    })
}

async fn with_session<F, Fut>(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value, f: F) -> R
where
    F: FnOnce(Arc<Cdp>, Arc<Session>, Value) -> Fut,
    Fut: std::future::Future<Output = R>,
{
    let sess = target_session(server, ctx, method, p)?;
    let cdp = server.agent_browser.cdp().await?;
    f(cdp, sess, p.clone()).await
}

/// The URL a preview opens at (`0.0.0.0` → `localhost`).
fn preview_url(pv: &vk_proto::model::Preview) -> String {
    let scheme = if pv.scheme == "https" {
        "https"
    } else {
        "http"
    };
    format!("{scheme}://localhost:{}{}", pv.port, pv.path)
}

/// The preview scope for a session opened by `ctx` (`None` = machine-wide: full-scope callers,
/// or `[browser] session_previews = "machine"`).
fn scope_for(server: &Server, ctx: &Ctx, cfg: &AgentBrowserConfig) -> Option<PreviewScope> {
    let pane = ctx.pane_scope.as_ref()?;
    cfg.own_previews_only().then(|| PreviewScope {
        pane: pane.clone(),
        task: server.with_core(|c| crate::preview::task_of_pane(c, pane)),
    })
}

async fn open(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let cfg = server.agent_browser.config();
    let scope = scope_for(server, ctx, &cfg);
    // What to open.
    let mut preview_id = None;
    let url = match (s(p, "preview"), s(p, "url")) {
        (Some(t), _) if !t.contains("://") => {
            if t.contains('/')
                || s(p, "machine")
                    .is_some_and(|m| !m.is_empty() && m != "local" && m != server.opts.machine)
            {
                return Err(invalid(format!(
                    "browser sessions run on the machine of the dev server: use `vibeke --machine <m> browser open {}`",
                    t.rsplit('/').next().unwrap_or(t)
                )));
            }
            let pv = server
                .with_core(|c| {
                    c.model
                        .previews
                        .iter()
                        .find(|x| x.id == t || x.handle == t)
                        .cloned()
                })
                .ok_or_else(|| not_found("preview", t))?;
            if pv.status == PreviewStatus::Gone {
                return Err(not_found("preview", t));
            }
            // Someone else's preview: refused before anything (a suggestion is not promoted).
            if let Some(sc) = &scope
                && !server.with_core(|c| sc.owns(c, &pv))
            {
                return Err(destination_denied(
                    &preview_url(&pv),
                    Reason::ForeignPreview.as_str(),
                ));
            }
            if pv.status == PreviewStatus::Suggested {
                // Opening a suggestion confirms it (as `preview.open` does).
                if let Some(Err(e)) =
                    crate::preview::api(server, ctx, "preview.promote", &json!({"preview": pv.id}))
                        .await
                {
                    return Err(e);
                }
            }
            preview_id = Some(pv.handle.clone());
            Some(preview_url(&pv))
        }
        (Some(u), _) | (None, Some(u)) => Some(u.to_string()),
        (None, None) => None,
    };
    if let Some(u) = &url
        && policy::parse_target(u).is_none_or(|t| t.scheme != "http" && t.scheme != "https")
    {
        return Err(destination_denied(u, Reason::Scheme.as_str()));
    }
    // Device preset: viewport, DPR, mobile emulation and user agent; explicit `viewport` and
    // `dpr` override its size and ratio.
    let device = match s(p, "device").filter(|d| !d.is_empty()) {
        Some(name) => Some(vk_browser::devices::preset(name).ok_or_else(|| {
            invalid(format!(
                "unknown device `{name}` (presets: {})",
                vk_browser::devices::names().join(", ")
            ))
        })?),
        None => None,
    };
    let viewport = p
        .get("viewport")
        .and_then(|v| match v {
            Value::String(s) => parse_viewport(s),
            Value::Object(o) => Some((
                o.get("width").or(o.get("w")).and_then(Value::as_u64)? as u32,
                o.get("height").or(o.get("h")).and_then(Value::as_u64)? as u32,
            )),
            _ => None,
        })
        .or(device.map(|d| (d.width, d.height)))
        .unwrap_or_else(|| cfg.viewport());
    let dpr = p
        .get("dpr")
        .and_then(Value::as_f64)
        .or(device.map(|d| d.dpr))
        .unwrap_or(1.0);
    // Pre-check before starting anything (clean error, nothing left behind).
    if let Some(u) = &url
        && let Some(t) = policy::parse_target(u)
    {
        let pol = session_policy(server, &cfg, scope.as_ref());
        let ips = proxy::resolve_with(&proxy::SystemResolver, &t.host, t.port)
            .await
            .unwrap_or_default();
        let d = pol.decide(&t.host, t.port, &ips, Kind::Navigation);
        if !d.allow {
            return Err(destination_denied(u, d.reason.as_str()));
        }
    }
    let proc_ = ensure_proc(server).await?;
    let cdp = proc_.cdp.clone();
    let ab = &server.agent_browser;
    let n = ab.counter.fetch_add(1, Ordering::SeqCst) + 1;
    let id = ulid();
    let handle = format!("b{n}");
    // The session's filtering proxy.
    let srv = server.clone();
    let proxy_scope = scope.clone();
    let mut po = ProxyOptions::new(Arc::new(move || {
        session_policy(&srv, &srv.agent_browser.config(), proxy_scope.as_ref())
    }));
    if let Some(root) = proc_.pid {
        po.peer_check = Some(Arc::new(move |peer, local| {
            Box::pin(async move {
                tokio::task::spawn_blocking(move || {
                    let pids: Vec<u32> = vk_hold::procinfo::tree(root, 6)
                        .into_iter()
                        .map(|i| i.pid)
                        .collect();
                    vk_preview::sockets::owner_of_connection(&pids, peer, local).is_some()
                })
                .await
                .unwrap_or(false)
            })
        }));
    }
    po.on_rejected_peer = Arc::new(|peer| {
        tracing::warn!(%peer, "browser proxy: rejected a connection not owned by the headless browser");
    });
    let slot: Arc<Mutex<Option<std::sync::Weak<Session>>>> = Arc::default();
    {
        let slot = slot.clone();
        let srv = server.clone();
        po.on_event = Arc::new(move |e| {
            if e.decision.allow {
                return;
            }
            let Some(sess) = slot.lock().unwrap().as_ref().and_then(|w| w.upgrade()) else {
                return;
            };
            let url = if e.method.eq_ignore_ascii_case("CONNECT") {
                format!("{}:{}", e.host, e.port)
            } else {
                e.target.clone()
            };
            record_denial(&srv, &sess, &url, e.decision.reason.as_str(), "proxy", None);
        });
    }
    let proxy = proxy::start(po).await.map_err(crate::api::internal)?;
    let ctx_id = call(
        &cdp,
        None,
        "Target.createBrowserContext",
        json!({"proxyServer": proxy.url(), "proxyBypassList": "<-loopback>", "disposeOnDetach": false}),
    )
    .await?["browserContextId"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let _ = call(
        &cdp,
        None,
        "Browser.setDownloadBehavior",
        json!({"behavior": "deny", "browserContextId": ctx_id}),
    )
    .await;
    let target = call(
        &cdp,
        None,
        "Target.createTarget",
        json!({"url": "about:blank", "browserContextId": ctx_id, "width": viewport.0, "height": viewport.1}),
    )
    .await?["targetId"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let cdp_session = call(
        &cdp,
        None,
        "Target.attachToTarget",
        json!({"targetId": target, "flatten": true}),
    )
    .await?["sessionId"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let (owner_pane, owner_run) = match &ctx.pane_scope {
        Some(pane) => (
            Some(pane.clone()),
            server.with_core(|c| {
                c.run_for_pane(pane)
                    .filter(|r| r.ended_at_ms.is_none())
                    .map(|r| r.id.clone())
            }),
        ),
        None => (None, None),
    };
    let (frames, _) = watch::channel(None);
    let sess = Arc::new(Session {
        id: id.clone(),
        handle: handle.clone(),
        owner_pane,
        owner_run,
        owner_client: ctx.client_id.clone(),
        preview: preview_id,
        context_id: ctx_id,
        target_id: target,
        cdp_session: cdp_session.clone(),
        created_ms: vk_store::now_ms(),
        viewport,
        dpr,
        color_scheme: s(p, "color_scheme")
            .or(b(p, "dark").and_then(|d| d.then_some("dark")))
            .map(str::to_string),
        device: device.map(|d| d.name.to_string()),
        mobile: device.is_some_and(|d| d.mobile),
        scope,
        proxy: Mutex::new(Some(proxy)),
        console: Mutex::default(),
        network: Mutex::default(),
        inflight: Mutex::default(),
        denials: Mutex::default(),
        human: Mutex::new(None),
        frames,
        frame_seq: AtomicU64::new(0),
        screencast_subs: AtomicUsize::new(0),
        url: Mutex::new("about:blank".into()),
        main_status: Mutex::new(None),
        last_used: Mutex::new(Instant::now()),
        event_budget: Mutex::new((Instant::now(), 0)),
        closed: AtomicBool::new(false),
    });
    *slot.lock().unwrap() = Some(Arc::downgrade(&sess));
    ab.routes
        .lock()
        .unwrap()
        .insert(cdp_session.clone(), sess.clone());
    ab.sessions.lock().unwrap().insert(id, sess.clone());
    let cs = Some(cdp_session.as_str());
    let setup = async {
        call(
            &cdp,
            cs,
            "Fetch.enable",
            json!({"patterns": [{"urlPattern": "*"}]}),
        )
        .await?;
        call(
            &cdp,
            cs,
            "Target.setAutoAttach",
            json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true}),
        )
        .await?;
        call(&cdp, cs, "Page.enable", json!({})).await?;
        call(&cdp, cs, "Runtime.enable", json!({})).await?;
        call(&cdp, cs, "Network.enable", json!({})).await?;
        let _ = call(&cdp, cs, "Log.enable", json!({})).await;
        let _ = call(
            &cdp,
            cs,
            "Page.setInterceptFileChooserDialog",
            json!({"enabled": true}),
        )
        .await;
        call(
            &cdp,
            cs,
            "Emulation.setDeviceMetricsOverride",
            json!({"width": viewport.0, "height": viewport.1, "deviceScaleFactor": sess.dpr, "mobile": sess.mobile}),
        )
        .await?;
        if let Some(d) = device {
            let _ = call(
                &cdp,
                cs,
                "Emulation.setUserAgentOverride",
                json!({"userAgent": d.user_agent}),
            )
            .await;
            if d.mobile {
                let _ = call(
                    &cdp,
                    cs,
                    "Emulation.setTouchEmulationEnabled",
                    json!({"enabled": true, "maxTouchPoints": 5}),
                )
                .await;
            }
        }
        if let Some(scheme) =
            s(p, "color_scheme").or(b(p, "dark").and_then(|d| d.then_some("dark")))
        {
            let _ = call(
                &cdp,
                cs,
                "Emulation.setEmulatedMedia",
                json!({"features": [{"name": "prefers-color-scheme", "value": scheme}]}),
            )
            .await;
        }
        Ok::<(), RpcError>(())
    };
    if let Err(e) = setup.await {
        close_session(server, &sess, "setup_failed").await;
        return Err(e);
    }
    emit(
        server,
        "browser.session_opened",
        &sess,
        json!({"url": url, "preview": sess.preview, "owner_pane": sess.owner_pane}),
    );
    let mut out = sess.summary(server);
    out["machine"] = json!(server.opts.machine);
    out["environment"] = environment(server, &proc_);
    out["environment"]["device"] = json!(sess.device);
    out["environment"]["viewport"] = json!({"width": sess.viewport.0, "height": sess.viewport.1});
    out["environment"]["dpr"] = json!(sess.dpr);
    if let Some(u) = url {
        match navigate(server, &sess, &u, p).await {
            Ok(nav) => {
                for k in ["status", "final_url", "title"] {
                    out[k] = nav[k].clone();
                }
            }
            Err(e) => {
                close_session(server, &sess, "open_failed").await;
                return Err(e);
            }
        }
    }
    Ok(out)
}

fn environment(server: &Server, proc_: &Proc) -> Value {
    json!({
        "kind": "remote_headless",
        "machine": server.opts.machine,
        "runner": "host",
        "browser": proc_.product,
        "fresh_context": true,
    })
}

async fn navigate(server: &Arc<Server>, sess: &Arc<Session>, to: &str, p: &Value) -> R {
    let cdp = server.agent_browser.cdp().await?;
    // Relative paths resolve against the current origin.
    let url = if to.starts_with('/') {
        let cur = sess.url.lock().unwrap().clone();
        let t = policy::parse_target(&cur).ok_or_else(|| {
            invalid("a path needs a page with an http(s) origin; pass a full URL")
        })?;
        let host = if t.host.contains(':') {
            format!("[{}]", t.host)
        } else {
            t.host.clone()
        };
        format!("{}://{host}:{}{to}", t.scheme, t.port)
    } else {
        to.to_string()
    };
    let Some(t) = policy::parse_target(&url).filter(|t| t.scheme == "http" || t.scheme == "https")
    else {
        record_denial(
            server,
            sess,
            &url,
            Reason::Scheme.as_str(),
            "api",
            Some("Document"),
        );
        return Err(destination_denied(&url, Reason::Scheme.as_str()));
    };
    let cfg = server.agent_browser.config();
    let pol = session_policy(server, &cfg, sess.scope.as_ref());
    let ips = proxy::resolve_with(&proxy::SystemResolver, &t.host, t.port)
        .await
        .unwrap_or_default();
    let d = pol.decide(&t.host, t.port, &ips, Kind::Navigation);
    if !d.allow {
        record_denial(
            server,
            sess,
            &url,
            d.reason.as_str(),
            "api",
            Some("Document"),
        );
        return Err(destination_denied(&url, d.reason.as_str()));
    }
    let started = Instant::now();
    let timeout = Duration::from_millis(u(p, "timeout_ms").unwrap_or(15_000).clamp(100, 120_000));
    *sess.main_status.lock().unwrap() = None;
    let r = call_timeout(
        &cdp,
        Some(&sess.cdp_session),
        "Page.navigate",
        json!({"url": url}),
        timeout,
    )
    .await?;
    let denial_since = |since: Instant| -> Option<Denial> {
        sess.denials
            .lock()
            .unwrap()
            .iter()
            .find(|d| d.at >= since)
            .cloned()
    };
    if let Some(e) = r["errorText"].as_str() {
        if let Some(d) = denial_since(started) {
            return Err(destination_denied(&d.url, d.reason));
        }
        return Err(custom(
            ErrorKind::Conflict,
            "navigation_failed",
            format!("navigation to {url} failed: {e}"),
            json!({"url": url, "error": e}),
        ));
    }
    let wait = s(p, "wait").unwrap_or("load");
    if wait != "none" && wait != "commit" {
        let want = if wait == "domcontentloaded" {
            "interactive"
        } else {
            "complete"
        };
        loop {
            let st = call(
                &cdp,
                Some(&sess.cdp_session),
                "Runtime.evaluate",
                json!({"expression": "document.readyState", "returnByValue": true}),
            )
            .await
            .ok()
            .and_then(|v| v["result"]["value"].as_str().map(str::to_string))
            .unwrap_or_default();
            if st == "complete" || st == want {
                break;
            }
            if started.elapsed() > timeout {
                return Err(
                    err(ErrorKind::Timeout, format!("{url} did not finish loading"))
                        .details(json!({"url": url, "ready_state": st})),
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let info = call(
        &cdp,
        Some(&sess.cdp_session),
        "Runtime.evaluate",
        json!({"expression": "location.href + '\\n' + document.title", "returnByValue": true}),
    )
    .await
    .ok()
    .and_then(|v| v["result"]["value"].as_str().map(str::to_string))
    .unwrap_or_default();
    let (final_url, title) = info.split_once('\n').unwrap_or((info.as_str(), ""));
    let final_url = if final_url.is_empty() {
        url.clone()
    } else {
        final_url.to_string()
    };
    *sess.url.lock().unwrap() = final_url.clone();
    // A denied redirect hop or a proxy refusal of the document itself.
    if let Some(d) = sess
        .denials
        .lock()
        .unwrap()
        .iter()
        .find(|d| d.at >= started && (d.url == final_url || final_url.starts_with("chrome-error:")))
        .cloned()
    {
        return Err(destination_denied(&d.url, d.reason));
    }
    Ok(
        json!({"session": sess.handle, "status": *sess.main_status.lock().unwrap(), "final_url": final_url, "title": title}),
    )
}

/// JS locating an element: CSS selector or `text=…`; scrolls it into view and returns its
/// centre in CSS px.
fn locate_js(selector: &str) -> String {
    let sel = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into());
    format!(
        r#"(function(sel){{
  let el = null;
  if (sel.startsWith('text=')) {{
    const t = sel.slice(5).trim().toLowerCase();
    const cands = [...document.querySelectorAll('button,a,[role=button],[role=link],[role=tab],[role=menuitem],input[type=submit],input[type=button],label,summary,[onclick]')];
    const txt = e => ((e.innerText || e.value || e.getAttribute('aria-label') || '') + '').trim().toLowerCase();
    el = cands.find(e => txt(e) === t) || cands.find(e => txt(e).includes(t));
    if (!el && document.body) {{
      const w = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
      let n; while ((n = w.nextNode())) {{ if (n.textContent.toLowerCase().includes(t)) {{ el = n.parentElement; break; }} }}
    }}
  }} else {{
    el = document.querySelector(sel);
  }}
  if (!el) return null;
  el.scrollIntoView({{block: 'center', inline: 'center', behavior: 'instant'}});
  const r = el.getBoundingClientRect();
  return {{x: r.left + r.width / 2, y: r.top + r.height / 2, width: r.width, height: r.height,
          tag: el.tagName.toLowerCase(), text: ((el.innerText || el.value || '') + '').slice(0, 80)}};
}})({sel})"#
    )
}

async fn evaluate(cdp: &Arc<Cdp>, sess: &Session, expr: &str) -> Result<Value, RpcError> {
    let r = call(
        cdp,
        Some(&sess.cdp_session),
        "Runtime.evaluate",
        json!({"expression": expr, "returnByValue": true, "awaitPromise": true, "userGesture": true}),
    )
    .await?;
    if let Some(ex) = r.get("exceptionDetails") {
        let text = ex["exception"]["description"]
            .as_str()
            .or_else(|| ex["text"].as_str())
            .unwrap_or("exception")
            .to_string();
        return Err(invalid(format!("script error: {text}")).details(json!({"exception": text})));
    }
    Ok(r["result"]["value"].clone())
}

async fn locate(cdp: &Arc<Cdp>, sess: &Session, selector: &str, timeout: Duration) -> R {
    let started = Instant::now();
    loop {
        let v = evaluate(cdp, sess, &locate_js(selector)).await?;
        if v.is_object()
            && v["width"].as_f64().unwrap_or(0.0) + v["height"].as_f64().unwrap_or(0.0) > 0.0
        {
            return Ok(v);
        }
        if started.elapsed() >= timeout {
            return Err(not_found("element", selector).details(
                json!({"object": "element", "selector": selector, "visible": v.is_object()}),
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn wait_timeout(p: &Value) -> Duration {
    Duration::from_millis(u(p, "timeout_ms").unwrap_or(5_000).clamp(0, 60_000))
}

async fn click(cdp: Arc<Cdp>, sess: Arc<Session>, p: Value) -> R {
    let (x, y, el) = match (
        s(&p, "selector").or(s(&p, "text").map(|_| "")),
        p.get("x"),
        p.get("y"),
    ) {
        (Some(sel), _, _) => {
            let sel = if sel.is_empty() {
                format!("text={}", s(&p, "text").unwrap_or(""))
            } else {
                sel.to_string()
            };
            let el = locate(&cdp, &sess, &sel, wait_timeout(&p)).await?;
            (
                el["x"].as_f64().unwrap_or(0.0),
                el["y"].as_f64().unwrap_or(0.0),
                el,
            )
        }
        (None, Some(x), Some(y)) => (
            x.as_f64().ok_or_else(|| invalid("x must be a number"))?,
            y.as_f64().ok_or_else(|| invalid("y must be a number"))?,
            Value::Null,
        ),
        _ => {
            return Err(invalid(
                "browser.click needs `selector` (CSS or text=…) or `x` and `y`",
            ));
        }
    };
    let cs = Some(sess.cdp_session.as_str());
    call(
        &cdp,
        cs,
        "Input.dispatchMouseEvent",
        json!({"type": "mouseMoved", "x": x, "y": y}),
    )
    .await?;
    let clicks = u(&p, "click_count").unwrap_or(1);
    for t in ["mousePressed", "mouseReleased"] {
        call(
            &cdp,
            cs,
            "Input.dispatchMouseEvent",
            json!({"type": t, "x": x, "y": y, "button": "left", "clickCount": clicks}),
        )
        .await?;
    }
    Ok(json!({"session": sess.handle, "x": x, "y": y, "element": el}))
}

async fn type_text(cdp: Arc<Cdp>, sess: Arc<Session>, p: Value) -> R {
    let text = s(&p, "text")
        .ok_or_else(|| invalid("missing param `text`"))?
        .to_string();
    if let Some(sel) = s(&p, "selector") {
        locate(&cdp, &sess, sel, wait_timeout(&p)).await?;
        let clear = b(&p, "clear").unwrap_or(false);
        let js = format!(
            "(function(sel){{const el = sel.startsWith('text=') ? null : document.querySelector(sel); if (!el) return false; el.focus(); if ({clear} && el.select) el.select(); return true;}})({})",
            serde_json::to_string(sel).unwrap_or_default()
        );
        evaluate(&cdp, &sess, &js).await?;
    }
    call(
        &cdp,
        Some(&sess.cdp_session),
        "Input.insertText",
        json!({"text": text}),
    )
    .await?;
    if b(&p, "submit").unwrap_or(false) {
        dispatch_key(&cdp, &sess, "enter").await?;
    }
    Ok(json!({"session": sess.handle, "typed": text.chars().count()}))
}

/// Accept Playwright-style names too (`ArrowDown`, `Control+A`, `Meta+K`).
fn normalize_key(k: &str) -> String {
    if k.chars().count() == 1 {
        return k.to_string();
    }
    k.split('+')
        .map(|part| {
            let l = part.to_ascii_lowercase();
            match l.as_str() {
                "arrowup" => "up".into(),
                "arrowdown" => "down".into(),
                "arrowleft" => "left".into(),
                "arrowright" => "right".into(),
                "control" => "ctrl".into(),
                "meta" | "command" => "cmd".into(),
                "return" => "enter".into(),
                "" => "+".into(),
                _ if part.chars().count() == 1 => part.to_string(),
                _ => l,
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

async fn dispatch_key(cdp: &Arc<Cdp>, sess: &Session, key: &str) -> Result<(), RpcError> {
    let ev = vk_term::keygrammar::parse_key(&normalize_key(key))
        .map_err(|e| err(ErrorKind::InvalidKey, e.to_string()).details(json!({"key": key})))?;
    let cmds = vk_browser::input::map_key_with_release(
        &ev,
        vk_browser::input::MapOptions {
            host_reports_text: false,
            mac_commands: cfg!(target_os = "macos"),
        },
    );
    for c in cmds {
        let (m, p) = c.to_command();
        call(cdp, Some(&sess.cdp_session), m, p).await?;
    }
    Ok(())
}

async fn press(cdp: Arc<Cdp>, sess: Arc<Session>, p: Value) -> R {
    let key = s(&p, "key")
        .ok_or_else(|| invalid("missing param `key`"))?
        .to_string();
    dispatch_key(&cdp, &sess, &key).await?;
    Ok(json!({"session": sess.handle, "key": key}))
}

async fn wait(cdp: Arc<Cdp>, sess: Arc<Session>, p: Value) -> R {
    let what = s(&p, "for").unwrap_or("load").to_string();
    let timeout = Duration::from_millis(u(&p, "timeout_ms").unwrap_or(15_000).clamp(0, 120_000));
    if let Some(ms) = what.strip_prefix("ms:").and_then(|m| m.parse::<u64>().ok()) {
        tokio::time::sleep(Duration::from_millis(ms.min(60_000))).await;
        return Ok(json!({"session": sess.handle, "waited": what}));
    }
    if let Some(sel) = what.strip_prefix("selector:") {
        locate(&cdp, &sess, sel, timeout).await?;
        return Ok(json!({"session": sess.handle, "waited": what}));
    }
    let started = Instant::now();
    loop {
        let st = evaluate(&cdp, &sess, "document.readyState").await?;
        let idle = what != "networkidle" || sess.inflight.lock().unwrap().is_empty();
        if st == "complete" && idle {
            return Ok(json!({"session": sess.handle, "waited": what}));
        }
        if started.elapsed() > timeout {
            return Err(err(
                ErrorKind::Timeout,
                format!("wait for {what} timed out"),
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn eval(cdp: Arc<Cdp>, sess: Arc<Session>, p: Value) -> R {
    let expr = s(&p, "expression")
        .or_else(|| s(&p, "js"))
        .ok_or_else(|| invalid("missing param `expression`"))?;
    let v = evaluate(&cdp, &sess, expr).await?;
    let size = serde_json::to_vec(&v).map(|b| b.len()).unwrap_or(0);
    if size > MAX_EVAL {
        return Err(invalid(format!(
            "result is {size} bytes (limit {MAX_EVAL}); return less (e.g. a summary or a slice)"
        ))
        .details(json!({"bytes": size, "limit": MAX_EVAL})));
    }
    Ok(json!({"session": sess.handle, "value": v}))
}

pub(crate) fn png_size(png: &[u8]) -> (u32, u32) {
    if png.len() >= 24 && &png[12..16] == b"IHDR" {
        let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
        let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
        return (w, h);
    }
    (0, 0)
}

/// Content-addressed blob under `<state>/blobs/<h2>/<blake3>.<ext>` (+ `.json` metadata): the
/// unified store (`blob_store`, `vk_store::blobs`). The metadata replaces an earlier sidecar.
pub fn store_blob(
    server: &Server,
    data: &[u8],
    ext: &str,
    meta: &Value,
) -> std::io::Result<(String, PathBuf)> {
    crate::blob_store::store(server).put(data, ext, meta, vk_store::blobs::MetaMode::Replace)
}

async fn screenshot(server: &Arc<Server>, ctx: &Ctx, sess: &Arc<Session>, p: &Value) -> R {
    use base64::Engine as _;
    let cdp = server.agent_browser.cdp().await?;
    let cs = Some(sess.cdp_session.as_str());
    let full_page = b(p, "full_page").unwrap_or(false);
    let mut params = json!({"format": "png"});
    if let Some(sel) = s(p, "selector") {
        let el = locate(&cdp, sess, sel, wait_timeout(p)).await?;
        let (w, h) = (
            el["width"].as_f64().unwrap_or(1.0),
            el["height"].as_f64().unwrap_or(1.0),
        );
        params["clip"] = json!({"x": el["x"].as_f64().unwrap_or(0.0) - w / 2.0, "y": el["y"].as_f64().unwrap_or(0.0) - h / 2.0, "width": w, "height": h, "scale": 1});
    } else if full_page {
        let m = call(&cdp, cs, "Page.getLayoutMetrics", json!({})).await?;
        let size = if m["cssContentSize"].is_object() {
            &m["cssContentSize"]
        } else {
            &m["contentSize"]
        };
        let w = size["width"]
            .as_f64()
            .unwrap_or(sess.viewport.0 as f64)
            .max(1.0);
        let h = size["height"]
            .as_f64()
            .unwrap_or(sess.viewport.1 as f64)
            .clamp(1.0, MAX_FULL_PAGE_H);
        params["clip"] = json!({"x": 0, "y": 0, "width": w, "height": h, "scale": 1});
        params["captureBeyondViewport"] = json!(true);
    }
    // The document's own identity, read right before and right after the pixels (a
    // navigation in between makes the capture illustrative).
    let doc_before = evaluate(&cdp, sess, crate::screenshots::DOCUMENT_IDENTITY_JS)
        .await
        .ok();
    let r = call_timeout(
        &cdp,
        cs,
        "Page.captureScreenshot",
        params,
        Duration::from_secs(60),
    )
    .await?;
    let png = base64::engine::general_purpose::STANDARD
        .decode(r["data"].as_str().unwrap_or(""))
        .map_err(|e| err(ErrorKind::Internal, format!("screenshot data: {e}")))?;
    let doc_after = evaluate(&cdp, sess, crate::screenshots::DOCUMENT_IDENTITY_JS)
        .await
        .ok();
    let document =
        crate::screenshots::DocumentCapture::from_reads(doc_before.as_ref(), doc_after.as_ref());
    let proc_ = server.agent_browser.proc_.lock().await.clone();
    let url = sess.url.lock().unwrap().clone();
    // Where the page actually is (after redirects) and its title.
    let (final_url, title) = match document.as_ref().and_then(|d| d.identity()) {
        Some(d) => (Some(d.href.clone()), d.title.clone()),
        None => (
            doc_after
                .as_ref()
                .and_then(|v| v["href"].as_str().map(str::to_string)),
            doc_after
                .as_ref()
                .and_then(|v| v["title"].as_str().map(str::to_string)),
        ),
    };
    let taken_by = match &ctx.pane_scope {
        Some(pane) => crate::screenshots::Requester {
            kind: "agent".into(),
            pane: Some(pane.clone()),
            run: sess.owner_run.clone(),
            client: None,
        },
        None => crate::screenshots::Requester {
            kind: "user".into(),
            pane: sess.owner_pane.clone(),
            run: sess.owner_run.clone(),
            client: Some(ctx.client_id.clone()),
        },
    };
    let product = proc_
        .as_ref()
        .map(|p| p.product.clone())
        .unwrap_or_default();
    let environment = crate::screenshots::Environment {
        kind: crate::screenshots::EnvKind::RemoteHeadless,
        machine: server.opts.machine.clone(),
        runner: "host".into(),
        browser_version: product.split_once('/').map(|(_, v)| v.to_string()),
        browser: product,
        viewport: crate::screenshots::Viewport {
            width: sess.viewport.0,
            height: sess.viewport.1,
        },
        dpr: sess.dpr,
        color_scheme: sess.color_scheme.clone(),
        device: sess.device.clone(),
        fresh_context: true,
        profile: None,
    };
    let meta = crate::screenshots::record_screenshot(
        server,
        &png,
        crate::screenshots::ShotInputs {
            environment,
            url: url.clone(),
            final_url,
            title,
            preview: sess.preview.clone(),
            session: Some(sess.handle.clone()),
            taken_by,
            full_page,
            selector: s(p, "selector").map(str::to_string),
            checkout: None,
            runtime: None,
            probe_runtime: true,
            document,
        },
    )
    .await?;
    let (hash, path, width, height) = (
        meta.blob.clone(),
        meta.path(server),
        meta.width,
        meta.height,
    );
    emit(
        server,
        "browser.screenshot",
        sess,
        json!({"blob": hash, "url": url, "width": width, "height": height, "screenshot": meta.id}),
    );
    let mut out = json!({
        "session": sess.handle,
        "id": meta.id,
        "handle": meta.handle,
        "blob": hash,
        "path_on_machine": path,
        "width": width,
        "height": height,
        "bytes": png.len(),
        "binding": meta.binding,
        "label": meta.label,
        "meta": meta,
    });
    if b(p, "inline").unwrap_or(false) {
        if png.len() <= MAX_INLINE {
            out["data_b64"] = json!(base64::engine::general_purpose::STANDARD.encode(&png));
            out["mime"] = json!("image/png");
        } else {
            out["inline_skipped"] = json!(format!("{} bytes > {MAX_INLINE}", png.len()));
        }
    }
    Ok(out)
}

/// `browser.screenshot {url|preview, device?, viewport?, full_page?, …}` without a session:
/// a fresh context (same destination policy and preview scope as `browser.open`), one capture,
/// closed again.
async fn one_shot_screenshot(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    if s(p, "url").is_none() && s(p, "preview").is_none() {
        return Err(invalid(
            "browser.screenshot needs `session`, or `url` / `preview` for a one-shot capture",
        ));
    }
    let opened = open(server, ctx, p).await?;
    let handle = opened["session"].as_str().unwrap_or("").to_string();
    let Some(sess) = server.agent_browser.session(&handle) else {
        return Err(not_found("browser_session", &handle));
    };
    let r = screenshot(server, ctx, &sess, p).await;
    close_session(server, &sess, "one_shot").await;
    let mut out = r?;
    out["one_shot"] = json!(true);
    out["session"] = Value::Null;
    out["opened_session"] = json!(handle);
    for k in ["status", "final_url", "title"] {
        out[k] = opened[k].clone();
    }
    Ok(out)
}

async fn snapshot(cdp: Arc<Cdp>, sess: Arc<Session>, p: Value) -> R {
    let format = s(&p, "format").unwrap_or("a11y").to_string();
    let max = u(&p, "max_bytes")
        .map(|m| m as usize)
        .unwrap_or(MAX_SNAPSHOT)
        .min(1 << 20);
    let sel_js = |prop: &str| {
        let sel = s(&p, "selector").map(|x| serde_json::to_string(x).unwrap_or_default());
        match sel {
            Some(sel) => format!("(document.querySelector({sel}) || {{}}).{prop} || ''"),
            None if prop == "outerHTML" => "document.documentElement.outerHTML".to_string(),
            None => "document.body ? document.body.innerText : ''".to_string(),
        }
    };
    let (content, truncated) = match format.as_str() {
        "text" => {
            let v = evaluate(&cdp, &sess, &sel_js("innerText")).await?;
            vk_browser::snapshot::bound(v.as_str().unwrap_or(""), max)
        }
        "html" => {
            let v = evaluate(&cdp, &sess, &sel_js("outerHTML")).await?;
            vk_browser::snapshot::bound(v.as_str().unwrap_or(""), max)
        }
        "a11y" | "accessibility" => {
            let r = call(
                &cdp,
                Some(&sess.cdp_session),
                "Accessibility.getFullAXTree",
                json!({}),
            )
            .await?;
            let nodes = r["nodes"].as_array().cloned().unwrap_or_default();
            vk_browser::snapshot::format_ax(&nodes, max)
        }
        other => {
            return Err(invalid(format!(
                "format must be a11y|text|html, not {other}"
            )));
        }
    };
    Ok(
        json!({"session": sess.handle, "url": *sess.url.lock().unwrap(), "format": format, "content": content, "truncated": truncated}),
    )
}

fn since_filter(p: &Value) -> Option<i64> {
    u(p, "since_ms")
        .map(|ms| vk_store::now_ms() - ms as i64)
        .or_else(|| {
            s(p, "since")
                .and_then(|d| vk_config::Dur::parse(d).ok())
                .map(|d| vk_store::now_ms() - d.0.as_millis() as i64)
        })
}

fn console(sess: &Session, p: &Value) -> Value {
    let level = s(p, "level").unwrap_or("all");
    let since = since_filter(p);
    let limit = u(p, "limit").unwrap_or(200) as usize;
    let entries: Vec<Value> = sess
        .console
        .lock()
        .unwrap()
        .iter()
        .filter(|e| since.is_none_or(|t| e["ts"].as_i64().unwrap_or(0) >= t))
        .filter(|e| match level {
            "error" => e["level"] == "error",
            "warn" | "warning" => e["level"] == "error" || e["level"] == "warn",
            _ => true,
        })
        .cloned()
        .collect();
    let skip = entries.len().saturating_sub(limit);
    json!({"session": sess.handle, "entries": entries[skip..]})
}

fn network(sess: &Session, p: &Value) -> Value {
    let failed = b(p, "failed_only").or(b(p, "failed")).unwrap_or(false);
    let since = since_filter(p);
    let limit = u(p, "limit").unwrap_or(200) as usize;
    let entries: Vec<Value> = sess
        .network
        .lock()
        .unwrap()
        .iter()
        .filter(|e| since.is_none_or(|t| e["ts"].as_i64().unwrap_or(0) >= t))
        .filter(|e| {
            !failed
                || !e["error"].is_null()
                || !e["blocked_by_policy"].is_null()
                || e["status"].as_u64().is_some_and(|s| s >= 400)
        })
        .cloned()
        .collect();
    let skip = entries.len().saturating_sub(limit);
    json!({"session": sess.handle, "entries": entries[skip..]})
}

async fn list(server: &Arc<Server>, ctx: &Ctx) -> Value {
    let sessions: Vec<Value> = server
        .agent_browser
        .sessions()
        .into_iter()
        .filter(|x| owns(server, ctx, x))
        .map(|x| x.summary(server))
        .collect();
    let browser = status(server).await;
    json!({"sessions": sessions, "browser": browser, "machine": server.opts.machine})
}

async fn status(server: &Arc<Server>) -> Value {
    let p = server.agent_browser.proc_.lock().await.clone();
    let cfg = server.agent_browser.config();
    match p.filter(|p| p.alive.load(Ordering::SeqCst)) {
        Some(p) => json!({
            "running": true, "pid": p.pid, "product": p.product, "binary": p.binary, "kind": p.kind,
            "uptime_ms": p.started.elapsed().as_millis() as u64,
            "sessions": server.agent_browser.sessions.lock().unwrap().len(),
            "denied": server.agent_browser.denied.load(Ordering::Relaxed),
            "profile_dir": profile_dir(server), "idle_timeout_ms": cfg.idle().as_millis() as u64,
        }),
        None => {
            let found = vk_browser::headless::discover(
                Some(cfg.browser_path.as_str()).filter(|p| !p.is_empty()),
                &install_root(),
            );
            json!({
                "running": false,
                "sessions": 0,
                "denied": server.agent_browser.denied.load(Ordering::Relaxed),
                "binary": found.as_ref().map(|b| b.path.display().to_string()),
                "kind": found.map(|b| b.kind),
                "profile_dir": profile_dir(server),
                "idle_timeout_ms": cfg.idle().as_millis() as u64,
            })
        }
    }
}

async fn install(p: &Value) -> R {
    let root = install_root();
    let plan = vk_browser::install::plan(&root, s(p, "version"), s(p, "url"), s(p, "sha256"))
        .map_err(|e| invalid(format!("{e:#}")))?;
    if !b(p, "confirm").unwrap_or(false) {
        return Ok(json!({"plan": plan.to_json(), "confirm_required": true}));
    }
    if plan.sha256.is_none() {
        return Err(invalid(format!(
            "no recorded SHA-256 for chrome-headless-shell {} ({}); pass `sha256` after verifying the download",
            plan.version, plan.platform
        ))
        .details(json!({"plan": plan.to_json()})));
    }
    let pl = plan.clone();
    let bin = tokio::task::spawn_blocking(move || {
        vk_browser::install::install(&pl, &vk_browser::install::curl_fetch)
    })
    .await
    .map_err(|e| err(ErrorKind::Internal, e.to_string()))?
    .map_err(|e| err(ErrorKind::Internal, format!("{e:#}")))?;
    Ok(json!({"installed": true, "binary": bin, "plan": plan.to_json()}))
}

// ---- watch / take-over hooks for the browser pane's watch view (06 B7) ----------------------

/// What the watch view (`browser_pane::watch`) needs to know about a session.
#[derive(Debug, Clone)]
pub struct WatchInfo {
    pub handle: String,
    pub id: String,
    pub owner_pane: Option<String>,
    pub url: String,
    pub viewport: (u32, u32),
    /// Who holds human control (`None` = the agent drives).
    pub human: Option<String>,
}

impl AgentBrowsers {
    /// The session by handle or id, for the watch view.
    pub fn watch_info(&self, target: &str) -> Option<WatchInfo> {
        let sess = self.session(target)?;
        if sess.closed.load(Ordering::SeqCst) {
            return None;
        }
        Some(WatchInfo {
            handle: sess.handle.clone(),
            id: sess.id.clone(),
            owner_pane: sess.owner_pane.clone(),
            url: sess.url.lock().unwrap().clone(),
            viewport: sess.viewport,
            human: sess.human_control(),
        })
    }

    /// The newest open session owned by `pane` (the agent's peek "watch" action).
    pub fn session_of_pane(&self, pane: &str) -> Option<String> {
        self.sessions()
            .into_iter()
            .rev()
            .find(|x| x.owner_pane.as_deref() == Some(pane) && !x.closed.load(Ordering::SeqCst))
            .map(|x| x.handle.clone())
    }
}

/// Set (`by = Some(holder)`) or clear human control on a session, with the same events as
/// `browser.take_over` / `browser.release`. Returns whether anything changed.
pub fn set_human_control(
    server: &Arc<Server>,
    target: &str,
    by: Option<String>,
) -> Result<bool, RpcError> {
    let sess = server
        .agent_browser
        .session(target)
        .ok_or_else(|| not_found("browser_session", target))?;
    let on = by.is_some();
    let changed = {
        let mut h = sess.human.lock().unwrap();
        let changed = h.is_some() != on;
        *h = by.clone();
        changed
    };
    sess.touch();
    if changed {
        emit(
            server,
            if on {
                "browser.taken_over"
            } else {
                "browser.released"
            },
            &sess,
            json!({"by": by, "via": "watch_pane"}),
        );
    }
    Ok(changed)
}

#[cfg(test)]
#[path = "agent_browser_tests.rs"]
mod api_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_and_keys() {
        assert_eq!(parse_viewport("390x844"), Some((390, 844)));
        assert_eq!(parse_viewport("1440X900"), Some((1440, 900)));
        assert_eq!(parse_viewport("10x10"), None);
        assert_eq!(parse_viewport("x"), None);
        assert_eq!(normalize_key("ArrowDown"), "down");
        assert_eq!(normalize_key("Control+A"), "ctrl+A");
        assert_eq!(normalize_key("Meta+k"), "cmd+k");
        assert_eq!(normalize_key("Enter"), "enter");
        assert_eq!(normalize_key("a"), "a");
        assert!(vk_term::keygrammar::parse_key(&normalize_key("Escape")).is_ok());
        assert!(vk_term::keygrammar::parse_key(&normalize_key("Shift+Tab")).is_ok());
    }

    #[test]
    fn config_defaults_and_parsing() {
        let c = AgentBrowserConfig::default();
        assert_eq!(c.idle(), Duration::from_secs(600));
        assert_eq!(c.external(), External::Subresources);
        assert!(!c.browser_script);
        let c = AgentBrowserConfig {
            browser_idle: "2s".into(),
            browser_external: "deny".into(),
            browser_allow_private: vec!["10.0.0.0/8".into(), "bad host".into()],
            ..Default::default()
        };
        assert_eq!(c.idle(), Duration::from_secs(2));
        assert_eq!(c.external(), External::Deny);
        assert_eq!(c.allow_rules().len(), 1);
    }

    #[test]
    fn png_dimensions() {
        assert_eq!(png_size(vk_browser::fake::PNG_1X1), (1, 1));
        assert_eq!(png_size(b"nope"), (0, 0));
    }
}
