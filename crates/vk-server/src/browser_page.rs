//! Browser pane page I/O (06 B3.2), the parts beyond pixels and input:
//!
//! - **Console/network split**: each page's `Runtime.consoleAPICalled`, `Runtime.exceptionThrown`,
//!   `Log.entryAdded` and `Network.*` events go into the agent browser's ring shapes
//!   ([`vk_browser::capture`]), redacted and with control characters escaped, each tagged with
//!   the frame origin and document it came from. `browser.console {pane}` /
//!   `browser.network {pane}` read them (`vibeke browser console --pane p --follow`);
//!   `browser.pane.console` toggles a split under the browser pane running that follower. The
//!   split's output is not archived or indexed and only the readers `browser.console` allows may
//!   read it. When the page is rendered by another machine's server (the laptop renders devbox's
//!   pane), that media host relays entries to the owner (`browser.pane.console_push`, at most
//!   once a second) so the follower in the owner's layout sees them: only entries whose own
//!   frame is loopback, captured while the top-level document was loopback, in the same
//!   navigation (decided when each entry is captured).
//! - **Page clipboard → OSC 52**: `navigator.clipboard.writeText`/`write` (after the native
//!   call succeeded) and trusted, user-activated `copy`/`cut` events reach a per-page CDP
//!   binding; a call from the current top-level document's main world within a few seconds of
//!   a click or key press in the pane (not text, not API input; cleared by navigation) goes to
//!   the pane's viewers as `ServerFrame::Clipboard` (the TUI applies the `clipboard` config).
//!   The host clipboard is never read into the page.
//! - **Files into the page**: `BrowserCmd::DropFiles` names copies the TUI made from the files
//!   the user confirmed (uploaded with `blob.* {stage: "browser"}` into this server's private
//!   drop directory); they go to an open file chooser (`DOM.setFileInputFiles`), else are
//!   dropped at the last pointer position (`Input.dispatchDragEvent`). Nothing outside the drop
//!   directory is handed to Chromium; staged copies are removed 10 minutes after delivery.
//! - **Pinned viewports**: `BrowserPane.device`/`viewport` pin the CSS viewport (and DPR, mobile,
//!   touch, user agent for presets); frames are fitted into a centred rectangle on a neutral
//!   fill ([`vk_browser::devices::Letterbox`]) and pointer input is mapped back through the
//!   frame's page scale ([`vk_browser::devices::FrameMeta`]).

use super::{Geom, TState, Target};
use crate::Server;
use crate::api::{Ctx, R, b, err, invalid, not_found, resolve_pane, s, u};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use vk_browser::capture::{Capture, Captured, Filter, Kind};
use vk_browser::cdp::{Event, Page};
use vk_browser::devices::{FrameMeta, Letterbox, Pin};
use vk_browser::frame::Rgba;
use vk_proto::layout::Direction;
use vk_proto::model::{BrowserPane, Pane};
use vk_proto::rpc::{ErrorKind, RpcError};

/// Page clipboard writes are forwarded only this soon after user input to the pane.
pub const CLIP_WINDOW: Duration = Duration::from_secs(5);
/// Largest page clipboard write forwarded (the TUI applies `clipboard.remote_write_max_bytes`).
pub const CLIP_MAX: usize = 1 << 20;
/// Largest file dropped into a page, and files per drop.
pub const DROP_MAX: u64 = 50 << 20;
pub const DROP_FILES_MAX: usize = 16;
/// A file chooser the page opened takes the next drop for this long.
const CHOOSER_TTL: Duration = Duration::from_secs(600);
/// Staged drop copies live this long after their upload or delivery (Chromium reads a dropped
/// file when the page does, not when it is dropped).
pub const DROP_TTL: Duration = Duration::from_secs(600);
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
    /// What the last screencast frame showed (page scale, device size).
    pub meta: Option<FrameMeta>,
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
            meta: None,
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

/// Redacted, control characters escaped (page text is untrusted terminal input), bounded.
fn redact(s: &str) -> String {
    let r = vk_browser::capture::escape_controls(&vk_redact::redact(s));
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
/// and removes it from the global object, wraps `navigator.clipboard.writeText`/`write` (text
/// only) so the text is reported only after Chromium's own call succeeded (its permission and
/// activation checks; a rejection reaches the page unchanged), and reports `copy`/`cut`
/// selections of trusted, user-activated events only. The server still checks where the call
/// came from (the top-level document's main world) and that the user just clicked or typed.
fn clipboard_script(binding: &str) -> String {
    format!(
        r#"(() => {{
  const b = globalThis[{name}];
  if (typeof b !== 'function') return;
  try {{ delete globalThis[{name}]; }} catch (_) {{}}
  const post = (t) => {{ try {{ if (typeof t === 'string' && t.length) b(t); }} catch (_) {{}} }};
  const active = () => {{ try {{ return !!(navigator.userActivation && navigator.userActivation.isActive); }} catch (_) {{ return false; }} }};
  const c = globalThis.navigator && navigator.clipboard;
  if (c) {{
    const wt = c.writeText && c.writeText.bind(c);
    if (wt) c.writeText = function (t) {{ const s = String(t); return wt(t).then((v) => {{ post(s); return v; }}); }};
    const w = c.write && c.write.bind(c);
    if (w) c.write = function (items) {{
      let text = null;
      try {{
        for (const it of items || []) {{
          if (it && it.types && it.types.includes('text/plain')) {{ text = it.getType('text/plain').then((x) => x.text()); break; }}
        }}
      }} catch (_) {{}}
      return w(items).then(async (v) => {{ try {{ if (text) post(await text); }} catch (_) {{}} return v; }});
    }};
  }}
  for (const ev of ['copy', 'cut']) {{
    globalThis.addEventListener(ev, (e) => {{
      if (!e.isTrusted || !active()) return;
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
    // Headless Chromium denies `clipboard-write` by default; a desktop browser grants it to
    // the focused page (sanitized text, user activation still required), and the hook only
    // reports writes Chromium accepted.
    let _ = page.send(
        "Browser.setPermission",
        json!({"permission": {"name": "clipboard-write"}, "setting": "granted"}),
    );
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
            let mut st = t.st.lock().unwrap();
            if st.io.capture.on_page_event("Page.frameNavigated", p) {
                // A new top-level document: no file chooser, and the previous document's user
                // activation does not carry over to it.
                st.io.chooser = None;
                st.io.last_input = None;
            }
        }
        m @ ("Runtime.executionContextCreated"
        | "Runtime.executionContextDestroyed"
        | "Runtime.executionContextsCleared") => {
            t.st.lock().unwrap().io.capture.on_page_event(m, p);
        }
        m if m.starts_with("Runtime.") || m.starts_with("Log.") || m.starts_with("Network.") => {
            let mut st = t.st.lock().unwrap();
            if let Some(c) = st.io.capture.on_event(m, p, vk_store::now_ms(), &redact) {
                let relay = !t.owner.is_empty() && relay_eligible(&st.io.capture, &c);
                let e = st.io.capture.push(c.kind, c.entry);
                if relay {
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

fn is_loopback(url: &str) -> bool {
    vk_browser::policy::parse_open_url(url).is_some_and(|u| vk_browser::policy::is_loopback_url(&u))
}

/// May a captured entry go to a remote owner (decided once, when it is captured)? Only the
/// owner's own app: the frame that produced it is loopback, the top-level document is
/// loopback, and the entry belongs to the current top-level navigation (a request started on
/// an earlier document, or a context of one, does not).
pub(super) fn relay_eligible(cap: &Capture, c: &Captured) -> bool {
    c.nav > 0
        && c.nav == cap.nav
        && !c.origin.is_empty()
        && is_loopback(&c.origin)
        && is_loopback(&cap.top_url)
}

fn on_binding(t: &Arc<Target>, p: &Value) {
    let mut st = t.st.lock().unwrap();
    if st.io.binding.is_empty() || p["name"].as_str() != Some(st.io.binding.as_str()) {
        return;
    }
    let Some(text) = p["payload"].as_str().filter(|x| !x.is_empty()) else {
        return;
    };
    // The binding is installed in every context (iframes, isolated worlds); only the current
    // top-level document's main world may write.
    let top = p["executionContextId"]
        .as_i64()
        .is_some_and(|c| st.io.capture.is_top_main_world(c));
    let recent = st
        .io
        .last_input
        .is_some_and(|at| at.elapsed() <= CLIP_WINDOW);
    if !top || !recent || text.len() > CLIP_MAX || st.subs.is_empty() {
        st.io.clips_blocked += 1;
        tracing::debug!(pane = %t.pane, top, recent, len = text.len(), "browser pane: page clipboard write not forwarded");
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

/// The user pressed a key or a mouse button in the pane (forwarded from a client): clipboard
/// writes may follow. Text (pastes, `browser.command text`), drops and API input never arm it.
pub(super) fn note_input(st: &mut TState) {
    st.io.last_input = Some(Instant::now());
}

/// A screencast frame's metadata (page scale, device size) for input mapping.
pub(super) fn on_frame_meta(t: &Target, m: &Value) {
    if let Some(meta) = FrameMeta::from_json(m) {
        t.st.lock().unwrap().io.meta = Some(meta);
    }
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
///
/// Chromium takes input in CSS px of the page's layout and applies the page scale itself, so
/// the frame position goes through the last frame's metadata ([`FrameMeta`]): a mobile page
/// without a meta viewport is 980 px wide at page scale ~0.4. Metadata of another viewport
/// (a frame from before a pin change) is ignored.
pub(super) fn map_point(st: &TState, x: f32, y: f32, clamp: bool) -> Option<(f64, f64)> {
    let lb = st.io.letterbox.filter(|_| st.io.pin.is_some());
    let same = |w: u32| move |m: &&FrameMeta| (m.device_width - w as f64).abs() < 1.0;
    let Some(lb) = lb else {
        let s = st
            .io
            .meta
            .as_ref()
            .filter(same(st.css.0))
            .map_or(1.0, |m| m.page_scale);
        return Some((x as f64 / s, y as f64 / s));
    };
    let dpr = st.applied.or(st.want).map(|g| g.dpr as f64).unwrap_or(1.0);
    let meta = st.io.meta.as_ref().filter(same(lb.css.0));
    lb.to_input(x as f64 * dpr, y as f64 * dpr, clamp, meta)
}

// ---- files into the page ---------------------------------------------------------------------

/// This server's private drop directory (`<state>/browser-drops`, 0700): `blob.commit
/// {stage: "browser"}` puts the TUI's copies of confirmed files there, content-addressed
/// (`<12 hex>/<name>`), and only files there are handed to Chromium.
pub fn drops_root(server: &Server) -> PathBuf {
    server.browser.drops_root()
}

/// Create the drop directory (0700) and return it.
pub fn ensure_drops_root(server: &Server) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let root = drops_root(server);
    std::fs::create_dir_all(&root)?;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    Ok(root)
}

/// A path the page may get: a staged copy inside `root` (`<root>/<12 hex>/<name>`, no links
/// at either level, directories owned by us), a regular file ≤ 50 MiB. Anything else — a path
/// the user's file system names, a link, a file swapped in — is refused.
pub fn check_drop_path(root: &Path, p: &str) -> Result<PathBuf, String> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::Component;
    let path = Path::new(p);
    if !path.is_absolute() {
        return Err("not an absolute path".into());
    }
    let staged =
        || "not a staged copy (files reach a page only through the drop prompt)".to_string();
    let rel = path.strip_prefix(root).map_err(|_| staged())?;
    let parts: Vec<Component> = rel.components().collect();
    let hash = match parts.as_slice() {
        [Component::Normal(h), Component::Normal(_)]
            if h.len() == 12
                && h.to_str()
                    .is_some_and(|h| h.bytes().all(|c| c.is_ascii_hexdigit())) =>
        {
            *h
        }
        _ => return Err(staged()),
    };
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    for dir in [root.to_path_buf(), root.join(hash)] {
        let md = std::fs::symlink_metadata(&dir).map_err(|e| e.to_string())?;
        if !md.file_type().is_dir() || md.uid() != uid {
            return Err(staged());
        }
    }
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| format!("not readable: {e}"))?;
    let md = f.metadata().map_err(|e| e.to_string())?;
    if !md.is_file() {
        return Err("not a regular file".into());
    }
    if md.len() > DROP_MAX {
        return Err(format!("larger than {} MiB", DROP_MAX >> 20));
    }
    Ok(path.to_path_buf())
}

/// Remove staged drop copies older than [`DROP_TTL`] (by the directory's mtime, which a
/// delivery refreshes). Called from the once-a-second gc.
pub fn sweep_drops(server: &Server) {
    let root = drops_root(server);
    let Ok(rd) = std::fs::read_dir(&root) else {
        return;
    };
    for e in rd.flatten() {
        let Ok(md) = e.path().symlink_metadata() else {
            continue;
        };
        let old = md
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age > DROP_TTL);
        if old {
            if md.is_dir() {
                let _ = std::fs::remove_dir_all(e.path());
            } else {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// A staged copy was handed to the page: keep it [`DROP_TTL`] from now (or remove it at once
/// when the page did not get it).
fn after_delivery(files: &[String], delivered: bool) {
    for f in files {
        let Some(dir) = Path::new(f).parent() else {
            continue;
        };
        if delivered {
            if let Ok(d) = std::fs::File::open(dir) {
                let _ = d.set_modified(std::time::SystemTime::now());
            }
        } else {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn notice(t: &Target, msg: String) {
    let mut st = t.st.lock().unwrap();
    st.notice = Some(msg);
    Target::mark_state(&mut st);
}

/// `BrowserCmd::DropFiles`: validate (staged copies only), then the open file chooser or a
/// drop at the pointer.
pub fn drop_files(server: &Server, t: &Arc<Target>, paths: Vec<String>) {
    let t = t.clone();
    let root = drops_root(server);
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
            match check_drop_path(&root, p) {
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
            let chooser = st
                .io
                .chooser
                .take()
                .filter(|(_, at)| at.elapsed() < CHOOSER_TTL);
            (st.page.clone(), chooser, st.io.last_mouse, st.css)
        };
        let Some(page) = page else {
            after_delivery(&files, false);
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
        after_delivery(&files, res.is_ok());
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

/// The browser pane a console split belongs to (`created_by = "browser-console:<id>"`, set only
/// by the server).
pub fn split_of(p: &Pane) -> Option<&str> {
    p.created_by.strip_prefix("browser-console:")
}

/// A console split's output is neither archived nor indexed (it is the page's in-memory
/// capture, printed): the pane is `no_archive`.
pub fn no_archive(server: &Server, pane: &str) -> bool {
    server.with_core(|c| c.pane(pane).is_some_and(|p| split_of(p).is_some()))
}

/// May this caller read pane `pane`'s output (`pane.read`, `pane.wait_output`, search)? For a
/// console split the `browser.console` rule applies: full scope, the split itself, or the agent
/// that opened its browser pane. Other panes: yes (the ordinary read scope applies).
pub fn may_read_output(server: &Server, ctx: &Ctx, pane: &str) -> bool {
    let Some(scope) = &ctx.pane_scope else {
        return true;
    };
    let bp = server.with_core(|c| c.pane(pane).and_then(|p| split_of(p).map(str::to_string)));
    match bp {
        None => true,
        Some(bp) => scope == pane || may_read(server, ctx, &bp),
    }
}

/// `pane.read` / `pane.wait_output` of a console split from a pane (called from
/// `api::dispatch` next to the read-scope check).
pub fn authorize_output_read(
    server: &Server,
    ctx: &Ctx,
    method: &str,
    p: &Value,
) -> Result<(), RpcError> {
    if ctx.pane_scope.is_none() || !matches!(method, "pane.read" | "pane.wait_output") {
        return Ok(());
    }
    let Ok(pane) = resolve_pane(server, ctx, Some(s(p, "pane").unwrap_or("@current"))) else {
        return Ok(()); // not a live pane: splits keep no archive to read
    };
    if may_read_output(server, ctx, &pane.id) {
        return Ok(());
    }
    Err(err(
        ErrorKind::PermissionDenied,
        format!("{method}: a browser console split is readable only by itself, the agent that opened its browser pane, or full scope"),
    )
    .details(json!({"scope": "pane"})))
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
    // Everything in the response is redacted and terminal-safe: the entries (captured that
    // way; relayed or older copies again) and the page's current URL.
    let url = url
        .or_else(|| model.and_then(|m| m.browser.map(|b| b.url)))
        .map(|u| redact(&u));
    let mut entries = Value::Array(entries);
    vk_browser::capture::escape_value(&mut entries);
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
            Kind::Console => &[
                "ts", "level", "text", "source", "url", "line", "origin", "document",
            ],
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
                "origin",
                "document",
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

/// Relay queued entries of pages rendered here to their remote owners (called once a second).
/// Only entries [`relay_eligible`] judged to be the owner's app when they were captured are
/// queued; nothing captured elsewhere is ever in the queue.
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
    sweep_drops(server);
    server
        .browser
        .relayed
        .lock()
        .unwrap()
        .retain(|k, _| live.contains(k));
}
