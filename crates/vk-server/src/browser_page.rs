//! Browser pane page I/O (06 B3.2), the parts beyond pixels and input:
//!
//! - **Console/network split**: each page's `Runtime.consoleAPICalled`, `Runtime.exceptionThrown`,
//!   `Log.entryAdded` and `Network.*` events go into the agent browser's ring shapes
//!   ([`vk_browser::capture`]), redacted. `browser.console {pane}` / `browser.network {pane}`
//!   read them (`vibeke browser console --pane p --follow`); `browser.pane.console` toggles a
//!   split under the browser pane running that follower. When the page is rendered by another
//!   machine's server (the laptop renders devbox's pane), that media host relays new entries to
//!   the owner (`browser.pane.console_push`, at most once a second, only while the page is on a
//!   loopback URL) so the follower in the owner's layout sees them.
//! - **Page clipboard → OSC 52**: `navigator.clipboard.writeText`/`write` and `copy`/`cut`
//!   events reach a per-page CDP binding; writes within a few seconds of user input to the pane
//!   go to the pane's viewers as `ServerFrame::Clipboard` (the TUI applies the `clipboard`
//!   config). The host clipboard is never read into the page.
//! - **Files into the page**: `BrowserCmd::DropFiles` (paths the user confirmed) go to an open
//!   file chooser (`DOM.setFileInputFiles`), else are dropped at the last pointer position
//!   (`Input.dispatchDragEvent`). Readable regular files ≤ 50 MiB only.
//! - **Pinned viewports**: `BrowserPane.device`/`viewport` pin the CSS viewport (and DPR, mobile,
//!   touch, user agent for presets); frames are fitted into a centred rectangle on a neutral
//!   fill ([`vk_browser::devices::Letterbox`]) and pointer input is mapped back.

use super::{Geom, TState, Target};
use crate::Server;
use crate::api::{Ctx, R, b, err, invalid, not_found, resolve_pane, s, u};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use vk_browser::capture::{Capture, Filter, Kind};
use vk_browser::cdp::{Event, Page};
use vk_browser::devices::{Letterbox, Pin};
use vk_browser::frame::Rgba;
use vk_proto::layout::Direction;
use vk_proto::model::BrowserPane;
use vk_proto::rpc::ErrorKind;

/// Page clipboard writes are forwarded only this soon after user input to the pane.
pub const CLIP_WINDOW: Duration = Duration::from_secs(5);
/// Largest page clipboard write forwarded (the TUI applies `clipboard.remote_write_max_bytes`).
pub const CLIP_MAX: usize = 1 << 20;
/// Largest file dropped into a page, and files per drop.
pub const DROP_MAX: u64 = 50 << 20;
pub const DROP_FILES_MAX: usize = 16;
/// A file chooser the page opened takes the next drop for this long.
const CHOOSER_TTL: Duration = Duration::from_secs(600);
/// Entries per relay batch, and per entry text.
const RELAY_BATCH: usize = 200;
const TEXT_MAX: usize = 8 * 1024;
/// The console split's share of the browser pane.
const SPLIT_RATIO: f32 = 0.3;

/// Page-side state of one browser pane target (inside `TState`).
#[derive(Debug)]
pub struct PageIo {
    pub capture: Capture,
    /// Entries not yet relayed to a remote owner.
    pub(super) relay: Vec<Value>,
    /// The page's clipboard binding (`Runtime.addBinding`), random per page.
    pub binding: String,
    pub last_input: Option<Instant>,
    /// `Page.fileChooserOpened`: (backendNodeId, when).
    pub chooser: Option<(i64, Instant)>,
    /// Last pointer position in page CSS px (drops land there).
    pub last_mouse: Option<(f64, f64)>,
    pub pin: Option<Pin>,
    /// Letterbox of the applied pinned viewport.
    pub letterbox: Option<Letterbox>,
    /// The user agent was overridden on the current page (restored when unpinned).
    ua_overridden: bool,
    touch: bool,
    pub clips: u64,
    pub clips_blocked: u64,
}

impl Default for PageIo {
    fn default() -> Self {
        let mut capture = Capture::default();
        // Sequence numbers stay increasing across targets re-created for the same pane, so a
        // follower's `after` never hides a new page's entries.
        capture.seq = (vk_store::now_ms().max(0) as u64) * 1000;
        PageIo {
            capture,
            relay: Vec::new(),
            binding: String::new(),
            last_input: None,
            chooser: None,
            last_mouse: None,
            pin: None,
            letterbox: None,
            ua_overridden: false,
            touch: false,
            clips: 0,
            clips_blocked: 0,
        }
    }
}

impl PageIo {
    pub fn with_pin(spec: &BrowserPane) -> PageIo {
        PageIo {
            pin: pin_of(spec),
            ..Default::default()
        }
    }
}

pub fn pin_of(spec: &BrowserPane) -> Option<Pin> {
    Pin::from_spec(spec.device.as_deref(), spec.viewport.as_deref())
}

fn redact(s: &str) -> String {
    let r = vk_redact::redact(s).into_owned();
    if r.len() > TEXT_MAX {
        let mut cut = TEXT_MAX;
        while !r.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…", &r[..cut])
    } else {
        r
    }
}

// ---- page setup and events ------------------------------------------------------------------

/// The clipboard hook, run in every document before page scripts. It captures the binding
/// and removes it from the global object, wraps `navigator.clipboard.writeText`/`write`
/// (text only; the page still gets its promise), and reports `copy`/`cut` selections.
fn clipboard_script(binding: &str) -> String {
    format!(
        r#"(() => {{
  const b = globalThis[{name}];
  if (typeof b !== 'function') return;
  try {{ delete globalThis[{name}]; }} catch (_) {{}}
  const post = (t) => {{ try {{ if (typeof t === 'string' && t.length) b(t); }} catch (_) {{}} }};
  const c = globalThis.navigator && navigator.clipboard;
  if (c) {{
    const wt = c.writeText && c.writeText.bind(c);
    if (wt) c.writeText = function (t) {{ post(String(t)); return wt(t).catch(() => undefined); }};
    const w = c.write && c.write.bind(c);
    if (w) c.write = async function (items) {{
      try {{
        for (const it of items || []) {{
          if (it && it.types && it.types.includes('text/plain')) {{ post(await (await it.getType('text/plain')).text()); break; }}
        }}
      }} catch (_) {{}}
      return w(items).catch(() => undefined);
    }};
  }}
  for (const ev of ['copy', 'cut']) {{
    globalThis.addEventListener(ev, (e) => {{
      let t = '';
      try {{ if (e.clipboardData) t = e.clipboardData.getData('text/plain'); }} catch (_) {{}}
      if (!t) {{ try {{ const s = document.getSelection(); t = s ? String(s) : ''; }} catch (_) {{}} }}
      if (!t) {{
        const a = document.activeElement;
        if (a && typeof a.selectionStart === 'number' && typeof a.value === 'string') t = a.value.slice(a.selectionStart, a.selectionEnd);
      }}
      post(t);
    }});
  }}
}})();"#,
        name = serde_json::to_string(binding).unwrap_or_default()
    )
}

/// Enable capture, the clipboard hook and file-chooser interception on a new page (before it
/// navigates; commands on one session are processed in order).
pub fn setup_page(page: &Page, t: &Target) {
    let binding = format!(
        "__vk_clip_{}",
        crate::core::ulid()[16..].to_ascii_lowercase()
    );
    t.st.lock().unwrap().io.binding = binding.clone();
    let _ = page.send("Runtime.enable", json!({}));
    let _ = page.send("Log.enable", json!({}));
    let _ = page.send("Network.enable", json!({}));
    let _ = page.send("Runtime.addBinding", json!({"name": binding}));
    let _ = page.send(
        "Page.addScriptToEvaluateOnNewDocument",
        json!({"source": clipboard_script(&binding)}),
    );
    let _ = page.send(
        "Page.setInterceptFileChooserDialog",
        json!({"enabled": true}),
    );
}

/// Capture, clipboard and file-chooser events of a page.
pub fn on_event(t: &Arc<Target>, ev: &Event) {
    let p = &ev.params;
    match ev.method.as_str() {
        "Runtime.bindingCalled" => on_binding(t, p),
        "Page.fileChooserOpened" => {
            let node = p["backendNodeId"].as_i64();
            let mut st = t.st.lock().unwrap();
            if let Some(n) = node {
                st.io.chooser = Some((n, Instant::now()));
                st.notice = Some(
                    "the page opened a file chooser — drop or paste a file path into the pane (prefix+shift+v: clipboard image)"
                        .into(),
                );
                Target::mark_state(&mut st);
            }
        }
        "Page.frameNavigated" => {
            if ev.params["frame"]
                .get("parentId")
                .is_none_or(|p| p.is_null())
            {
                t.st.lock().unwrap().io.chooser = None;
            }
        }
        m if m.starts_with("Runtime.") || m.starts_with("Log.") || m.starts_with("Network.") => {
            let mut st = t.st.lock().unwrap();
            if let Some((kind, e)) = st.io.capture.on_event(m, p, vk_store::now_ms(), &redact) {
                let e = st.io.capture.push(kind, e);
                if !t.owner.is_empty() {
                    st.io.relay.push(e);
                    let n = st.io.relay.len();
                    if n > RELAY_KEEP {
                        st.io.relay.drain(..n - RELAY_KEEP);
                    }
                }
            }
        }
        _ => {}
    }
}

/// Pending relay entries kept at most (older ones are dropped when the owner is unreachable).
pub const RELAY_KEEP: usize = 500;

fn on_binding(t: &Arc<Target>, p: &Value) {
    let mut st = t.st.lock().unwrap();
    if st.io.binding.is_empty() || p["name"].as_str() != Some(st.io.binding.as_str()) {
        return;
    }
    let Some(text) = p["payload"].as_str().filter(|x| !x.is_empty()) else {
        return;
    };
    let recent = st
        .io
        .last_input
        .is_some_and(|at| at.elapsed() <= CLIP_WINDOW);
    if !recent || text.len() > CLIP_MAX || st.subs.is_empty() {
        st.io.clips_blocked += 1;
        tracing::debug!(pane = %t.pane, recent, len = text.len(), "browser pane: page clipboard write not forwarded");
        return;
    }
    st.io.clips += 1;
    let data = text.as_bytes().to_vec();
    for s in st.subs.values_mut() {
        s.clip = Some(data.clone());
        s.notify.notify_one();
    }
}

/// A page clipboard write waiting for subscriber `sub`.
pub fn take_clip(t: &Target, sub: u64) -> Option<Vec<u8>> {
    t.st.lock().unwrap().subs.get_mut(&sub)?.clip.take()
}

/// User input reached the pane (keys, text, clicks): clipboard writes may follow.
pub(super) fn note_input(st: &mut TState) {
    st.io.last_input = Some(Instant::now());
}

// ---- pinned viewports -------------------------------------------------------------------------

/// The owner's spec changed: adopt its pin. Returns true when the viewport must be re-applied.
pub(super) fn update_pin(st: &mut TState, spec: &BrowserPane) -> bool {
    let pin = pin_of(spec);
    if pin == st.io.pin {
        return false;
    }
    st.io.pin = pin;
    st.applied = None;
    st.want_at = Instant::now();
    true
}

/// `Page.startScreencast` params: pinned pages are bounded by their letterbox rectangle.
pub(super) fn screencast_params(st: &TState) -> Value {
    let mut p = json!({"format": "jpeg", "quality": 80, "everyNthFrame": 1});
    if let Some(lb) = st.io.letterbox.filter(|_| st.io.pin.is_some()) {
        p["maxWidth"] = json!(lb.rect.2);
        p["maxHeight"] = json!(lb.rect.3);
    }
    p
}

/// Size the page for `g`: the pane's own size, or the pinned device letterboxed in it.
/// Returns the CSS viewport; restarts a running screencast when its bounds changed.
pub fn set_viewport(page: &Page, t: &Target, g: Geom) -> anyhow::Result<(u32, u32)> {
    let (pin, had_ua, had_touch, old_lb) = {
        let st = t.st.lock().unwrap();
        (
            st.io.pin.clone(),
            st.io.ua_overridden,
            st.io.touch,
            st.io.letterbox,
        )
    };
    let dpr = g.dpr as f64;
    let (css, lb, ua, touch) = match &pin {
        None => {
            let css = page.set_viewport_for_cells(
                g.cols as u32,
                g.rows as u32,
                g.cell_w as u32,
                g.cell_h as u32,
                dpr,
            )?;
            (css, None, None, false)
        }
        Some(pin) => {
            page.call("Emulation.setDeviceMetricsOverride", pin.metrics(dpr))?;
            let area = (
                g.cols as u32 * g.cell_w as u32,
                g.rows as u32 * g.cell_h as u32,
            );
            let lb = Letterbox::fit(area, (pin.width, pin.height), dpr);
            (
                (pin.width, pin.height),
                Some(lb),
                pin.user_agent.clone(),
                pin.touch,
            )
        }
    };
    let mut ua_now = had_ua;
    match ua {
        Some(ua) => {
            page.send("Emulation.setUserAgentOverride", json!({"userAgent": ua}))?;
            ua_now = true;
        }
        None if had_ua => {
            // Back to the browser's own user agent.
            if let Ok(v) = page.call("Browser.getVersion", json!({}))
                && let Some(own) = v["userAgent"].as_str()
            {
                page.send("Emulation.setUserAgentOverride", json!({"userAgent": own}))?;
            }
            ua_now = false;
        }
        None => {}
    }
    if touch != had_touch {
        page.send(
            "Emulation.setTouchEmulationEnabled",
            json!({"enabled": touch, "maxTouchPoints": if touch { 5 } else { 1 }}),
        )?;
    }
    let restart = {
        let mut st = t.st.lock().unwrap();
        st.io.letterbox = lb;
        st.io.ua_overridden = ua_now;
        st.io.touch = touch;
        st.screencast && old_lb.map(|l| l.rect) != lb.map(|l| l.rect)
    };
    if restart {
        let params = screencast_params(&t.st.lock().unwrap());
        let _ = page.send("Page.stopScreencast", json!({}));
        let _ = page.send("Page.startScreencast", params);
    }
    Ok(css)
}

/// A decoded frame, placed for the pane (letterboxed when pinned).
pub fn fit_frame(t: &Target, img: Rgba) -> Rgba {
    let lb = {
        let st = t.st.lock().unwrap();
        st.io.letterbox.filter(|_| st.io.pin.is_some())
    };
    match lb {
        Some(lb) if !(lb.is_full() && (img.width, img.height) == lb.area) => lb.compose(&img),
        _ => img,
    }
}

/// Map a pointer position (CSS px of the pane's content area, as the TUI computes it from
/// device px / host DPR) to page CSS px. `None` = outside a letterboxed page (`clamp` keeps
/// drags and releases that left it on the page edge).
pub(super) fn map_point(st: &TState, x: f32, y: f32, clamp: bool) -> Option<(f64, f64)> {
    let lb = st.io.letterbox.filter(|_| st.io.pin.is_some());
    let Some(lb) = lb else {
        return Some((x as f64, y as f64));
    };
    let dpr = st.applied.or(st.want).map(|g| g.dpr as f64).unwrap_or(1.0);
    let (dx, dy) = (x as f64 * dpr, y as f64 * dpr);
    if clamp {
        Some(lb.to_page_clamped(dx, dy))
    } else {
        lb.to_page(dx, dy)
    }
}

// ---- files into the page ---------------------------------------------------------------------

/// A path the page may get: absolute, a readable regular file (symlinks resolved), ≤ 50 MiB.
pub fn check_drop_path(p: &str) -> Result<PathBuf, String> {
    let path = Path::new(p);
    if !path.is_absolute() {
        return Err("not an absolute path".into());
    }
    let canon = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
    let md = std::fs::metadata(&canon).map_err(|e| e.to_string())?;
    if !md.is_file() {
        return Err("not a regular file".into());
    }
    if md.len() > DROP_MAX {
        return Err(format!("larger than {} MiB", DROP_MAX >> 20));
    }
    std::fs::File::open(&canon).map_err(|e| format!("not readable: {e}"))?;
    Ok(canon)
}

fn notice(t: &Target, msg: String) {
    let mut st = t.st.lock().unwrap();
    st.notice = Some(msg);
    Target::mark_state(&mut st);
}

/// `BrowserCmd::DropFiles`: validate, then the open file chooser or a drop at the pointer.
pub fn drop_files(t: &Arc<Target>, paths: Vec<String>) {
    let t = t.clone();
    let run = move || {
        if paths.is_empty() {
            return;
        }
        if paths.len() > DROP_FILES_MAX {
            notice(
                &t,
                format!("not dropped: at most {DROP_FILES_MAX} files at once"),
            );
            return;
        }
        let mut files = Vec::new();
        for p in &paths {
            match check_drop_path(p) {
                Ok(c) => files.push(c.to_string_lossy().into_owned()),
                Err(e) => {
                    let name = Path::new(p)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| p.clone());
                    notice(&t, format!("not dropped: {name}: {e}"));
                    return;
                }
            }
        }
        let (page, chooser, pos, css) = {
            let mut st = t.st.lock().unwrap();
            note_input(&mut st);
            let chooser = st
                .io
                .chooser
                .take()
                .filter(|(_, at)| at.elapsed() < CHOOSER_TTL);
            (st.page.clone(), chooser, st.io.last_mouse, st.css)
        };
        let Some(page) = page else {
            notice(&t, "not dropped: the page is not running".into());
            return;
        };
        let n = files.len();
        let res = match chooser {
            Some((node, _)) => page
                .call(
                    "DOM.setFileInputFiles",
                    json!({"files": files, "backendNodeId": node}),
                )
                .map(|_| format!("attached {n} file(s) to the page's file chooser")),
            None => {
                let (x, y) = pos.unwrap_or((css.0 as f64 / 2.0, css.1 as f64 / 2.0));
                let data = json!({"items": [], "files": files, "dragOperationsMask": 1});
                ["dragEnter", "dragOver", "drop"]
                    .iter()
                    .try_for_each(|ty| {
                        page.call(
                            "Input.dispatchDragEvent",
                            json!({"type": ty, "x": x, "y": y, "data": data, "modifiers": 0}),
                        )
                        .map(|_| ())
                    })
                    .map(|_| format!("dropped {n} file(s) on the page"))
            }
        };
        notice(&t, res.unwrap_or_else(|e| format!("not dropped: {e:#}")));
    };
    match tokio::runtime::Handle::try_current() {
        Ok(h) => {
            h.spawn_blocking(run);
        }
        Err(_) => run(),
    }
}

// ---- console API, relay, split ---------------------------------------------------------------

/// May this caller read browser pane `id`'s console? Full scope always; a pane only when it is
/// that pane's console split or opened that browser pane.
fn may_read(server: &Server, ctx: &Ctx, id: &str) -> bool {
    let Some(scope) = &ctx.pane_scope else {
        return true;
    };
    server.with_core(|c| {
        let caller_is_split = c
            .pane(scope)
            .is_some_and(|p| p.created_by == format!("browser-console:{id}"));
        let opened_it = c
            .pane(id)
            .is_some_and(|p| p.browser.is_some() && p.created_by == format!("agent:{scope}"));
        caller_is_split || opened_it
    })
}

fn filter_of(method: &str, p: &Value) -> Filter {
    let kind = s(p, "kind").unwrap_or(if method == "browser.network" {
        "network"
    } else {
        "all"
    });
    let level = s(p, "level").unwrap_or("all");
    Filter {
        console: kind != "network",
        network: kind != "console",
        errors: level == "error"
            || b(p, "errors").unwrap_or(false)
            || b(p, "failed").or(b(p, "failed_only")).unwrap_or(false),
        after: u(p, "after").unwrap_or(0),
        since_ms: u(p, "since_ms")
            .map(|ms| vk_store::now_ms() - ms as i64)
            .or_else(|| {
                s(p, "since")
                    .and_then(|d| vk_config::Dur::parse(d).ok())
                    .map(|d| vk_store::now_ms() - d.0.as_millis() as i64)
            }),
        limit: u(p, "limit").unwrap_or(200).clamp(1, 1000) as usize,
    }
}

/// `browser.console {pane, kind?: all|console|network, level?: error, errors?, after?, since?,
/// limit?}` (and `browser.network {pane, failed?}`): the browser pane's capture, from the page
/// rendered here or, for a pane in this layout rendered elsewhere, from the relayed copy.
pub fn console_api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> R {
    let raw = s(p, "pane").ok_or_else(|| invalid("missing param `pane`"))?;
    let model = resolve_pane(server, ctx, Some(raw)).ok();
    let id = model
        .as_ref()
        .map(|m| m.id.clone())
        .unwrap_or_else(|| raw.to_string());
    if model.as_ref().is_some_and(|m| m.browser.is_none()) {
        return Err(invalid(format!("{raw} is not a browser pane")));
    }
    if !may_read(server, ctx, &id) || (ctx.pane_scope.is_some() && model.is_none()) {
        return Err(err(
            ErrorKind::PermissionDenied,
            "from a pane, only the browser pane's own console split (or the agent that opened it) may read its console",
        )
        .details(json!({"scope": "pane"})));
    }
    let f = filter_of(method, p);
    let local = server
        .browser
        .target(&id)
        .filter(|t| t.st.lock().unwrap().watch.is_none());
    let (entries, last, source, url) = if let Some(t) = local {
        let st = t.st.lock().unwrap();
        (
            st.io.capture.query(&f),
            st.io.capture.seq,
            "local",
            Some(st.url.clone()),
        )
    } else if let Some(c) = server.browser.relayed.lock().unwrap().get(&id) {
        (c.query(&f), c.seq, "relayed", None)
    } else if model.is_some() {
        (Vec::new(), 0, "none", None)
    } else {
        return Err(not_found("browser pane", raw));
    };
    let url = url.or_else(|| model.and_then(|m| m.browser.map(|b| b.url)));
    Ok(json!({"pane": id, "entries": entries, "last_seq": last, "source": source, "url": url}))
}

/// `browser.pane.console_push {pane, entries}` (from the media host rendering one of this
/// server's browser panes; full scope only): store relayed entries for followers here.
pub fn push_api(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let raw = s(p, "pane").ok_or_else(|| invalid("missing param `pane`"))?;
    let pane = resolve_pane(server, ctx, Some(raw))?;
    if pane.browser.is_none() {
        return Err(invalid(format!("{raw} is not a browser pane")));
    }
    let entries = p
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("missing param `entries`"))?;
    let mut relayed = server.browser.relayed.lock().unwrap();
    let cap = relayed
        .entry(pane.id.clone())
        .or_insert_with(|| PageIo::default().capture);
    let mut n = 0;
    for e in entries.iter().take(vk_browser::capture::RING) {
        let Some(obj) = e.as_object() else { continue };
        let kind = if obj.get("kind").and_then(Value::as_str) == Some("network") {
            Kind::Network
        } else {
            Kind::Console
        };
        // Keep the known fields only, re-redacted and bounded (relayed input is untrusted).
        let keep: &[&str] = match kind {
            Kind::Console => &["ts", "level", "text", "source", "url", "line"],
            Kind::Network => &[
                "ts",
                "method",
                "url",
                "type",
                "status",
                "mime",
                "error",
                "blocked_reason",
                "duration_ms",
                "canceled",
            ],
        };
        let mut clean = serde_json::Map::new();
        for k in keep {
            if let Some(v) = obj.get(*k) {
                let v = match v {
                    Value::String(x) => Value::String(redact(x)),
                    Value::Number(_) | Value::Bool(_) | Value::Null => v.clone(),
                    _ => continue,
                };
                clean.insert((*k).to_string(), v);
            }
        }
        cap.push(kind, Value::Object(clean));
        n += 1;
    }
    Ok(json!({"pane": pane.id, "stored": n}))
}

/// Relay new entries of pages rendered here for remote owners (called once a second). Only
/// while the page is on a loopback URL: the owner's app, not wherever the user browsed to.
pub fn relay(server: &Arc<Server>) {
    let targets: Vec<Arc<Target>> = server
        .browser
        .inner
        .lock()
        .unwrap()
        .targets
        .values()
        .filter(|t| !t.owner.is_empty())
        .cloned()
        .collect();
    for t in targets {
        let batch: Vec<Value> = {
            let mut st = t.st.lock().unwrap();
            if st.io.relay.is_empty() {
                continue;
            }
            let loopback = st.url == "about:blank"
                || crate::preview::canonical_open_url(&st.url).is_some_and(|(_, lb)| lb);
            if !loopback {
                st.io.relay.clear();
                continue;
            }
            let n = st.io.relay.len().min(RELAY_BATCH);
            st.io.relay.drain(..n).collect()
        };
        let (server, owner, pane) = (server.clone(), t.owner.clone(), t.pane.clone());
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        tokio::spawn(async move {
            if let Err(e) = crate::preview::remote_call(
                &server,
                &owner,
                "browser.pane.console_push",
                json!({"pane": pane, "entries": batch}),
            )
            .await
            {
                tracing::debug!(owner = %owner, error = %e.message, "browser pane: console relay failed");
            }
        });
    }
}

/// `browser.pane.console {pane, toggle?}`: open (or, with `toggle`, close) the console split
/// under a browser pane: a pane running `vibeke browser console --pane <id> --follow`.
pub fn console_split(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let bp = resolve_pane(server, ctx, s(p, "pane"))?;
    if bp.browser.is_none() {
        return Err(invalid(format!("{} is not a browser pane", bp.handle)));
    }
    let marker = format!("browser-console:{}", bp.id);
    let existing = server.with_core(|c| {
        c.model
            .panes
            .iter()
            .find(|x| x.created_by == marker)
            .cloned()
    });
    if let Some(x) = existing {
        if b(p, "toggle").unwrap_or(true) {
            server.close_pane(&x.id);
            return Ok(json!({"closed": x.id, "pane_handle": x.handle, "browser_pane": bp.id}));
        }
        return Ok(
            json!({"pane": x.id, "pane_handle": x.handle, "browser_pane": bp.id, "existing": true}),
        );
    }
    let cmd = vec![
        server.opts.bin.to_string_lossy().into_owned(),
        "browser".into(),
        "console".into(),
        "--pane".into(),
        bp.id.clone(),
        "--follow".into(),
    ];
    let focus = b(p, "focus")
        .unwrap_or(false)
        .then_some(ctx.client_id.as_str());
    let pane = server
        .split_pane(
            &bp.id,
            Direction::Down,
            SPLIT_RATIO,
            None,
            Some(cmd),
            Some(format!("console · {}", bp.handle)),
            focus,
            &marker,
        )
        .map_err(|e| err(ErrorKind::Internal, format!("{e:#}")))?;
    Ok(json!({"pane": pane.id, "pane_handle": pane.handle, "browser_pane": bp.id}))
}

/// Console splits whose browser pane is gone are closed (gc, once a second); relayed copies
/// of panes no longer in the layout are forgotten.
pub fn gc_splits(server: &Arc<Server>) {
    let (orphans, live): (Vec<String>, std::collections::HashSet<String>) = server.with_core(|c| {
        let live: std::collections::HashSet<String> = c
            .model
            .panes
            .iter()
            .filter(|p| p.browser.is_some())
            .map(|p| p.id.clone())
            .collect();
        let orphans = c
            .model
            .panes
            .iter()
            .filter(|p| {
                p.created_by
                    .strip_prefix("browser-console:")
                    .is_some_and(|b| !live.contains(b))
            })
            .map(|p| p.id.clone())
            .collect();
        (orphans, live)
    });
    for o in orphans {
        server.close_pane(&o);
    }
    server
        .browser
        .relayed
        .lock()
        .unwrap()
        .retain(|k, _| live.contains(k));
}
