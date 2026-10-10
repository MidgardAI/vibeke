//! Preview manager (06 B2/B4, 08 §6): one popup for every preview on every machine and the
//! ways to view it, plus the inline action chips under a sidebar preview row.
//!
//! - **Popup** (`preview_list`, default **`prefix+shift+o`**): a row per live preview (not
//!   gone) — status dot, `machine/handle` (machine prefix only with more than one machine, as
//!   in goto), port and path, label, the owning pane, and markers: `▣ pane` (a browser pane
//!   shows it), `◎ proxy` (the local proxy has an origin for it), `⇄ :port` (mirrored) and
//!   `!N` (console errors). The focused workspace's previews come first. Under the list a
//!   detail strip for the selected one: URL, machine, status age, source, the proxy origin
//!   (plain URL only: a one-time `open_url` is never shown or copied) and the mirror with its
//!   ⚠. Keys: `↑/↓ j/k`, `enter` open as configured, `w` window, `p` proxy, `m` mirror /
//!   unmirror (mirroring asks first: it opens an unauthenticated port), `y` copy the plain
//!   URL, `g` go to the owning pane, `d` forget (asks), `/` filter, `i` install a browser on
//!   the media host (asks), `esc`/`q` close. Click selects, double-click opens.
//! - **Data**: `preview.status` from the media host — the local machine's server (it runs
//!   browser panes, windows, the proxy and mirrors); in the plain-SSH topology (no local
//!   machine) the current one — once on open and every 2 s while open (a deadline, spec 10
//!   §1.3.1). A server without `available_browsers` shows no banner.
//! - **Browser banner**: from `preview.status.available_browsers.pane` of the media host:
//!   `✗ no browser on <machine> …` with `i install`, else a dim `browser on <machine>: <kind>`. The
//!   install runs `browser.install {confirm, background}` (about 100 MB) and the poll keeps
//!   going until `preview.status.browser_install` reports the outcome as a toast.
//! - **Sidebar chips**: selecting a sidebar preview row (left click, or `enter` in navigate
//!   mode) expands `[pane] [window] [proxy] [mirror|unmirror] [copy]` under it (wrapped to the
//!   sidebar width); one row at a time, collapsed by a second click, `esc` in navigate mode or
//!   when the preview goes away. Double-click opens directly; right-click still opens the
//!   palette's preview actions.

use crate::app::{Action, App, Mode, Pending, Popup};
use crate::browser::{Reply, mirror_of};
use crate::screen::{Grid, Rect as SRect};
use crossterm::event::{MouseButton as CtButton, MouseEvent, MouseEventKind};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};
use vk_proto::model::{Preview, PreviewSource, PreviewStatus};
use vk_proto::render::Style;

/// `preview.status` poll interval while the popup is open (or an install runs).
pub const POLL: Duration = Duration::from_secs(2);
/// Two clicks on the same row within this are a double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// List rows shown at most (the list scrolls past that).
const MAX_ROWS: usize = 12;

/// Something the popup asks about before doing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    /// Mirror (machine, preview id): opens an unauthenticated loopback port.
    Mirror(usize, String),
    /// `preview.forget` (machine, preview id).
    Forget(usize, String),
    /// `browser.install` on machine `mi`.
    Install(usize),
}

/// A sidebar action chip under an expanded preview row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chip {
    Pane,
    Window,
    Proxy,
    Mirror,
    Unmirror,
    Copy,
}

impl Chip {
    fn label(self) -> &'static str {
        match self {
            Chip::Pane => "[pane]",
            Chip::Window => "[window]",
            Chip::Proxy => "[proxy]",
            Chip::Mirror => "[mirror]",
            Chip::Unmirror => "[unmirror]",
            Chip::Copy => "[copy]",
        }
    }
}

#[derive(Debug, Default)]
pub struct State {
    pub sel: usize,
    pub filter: String,
    /// Typing into the filter.
    pub typing: bool,
    pub confirm: Option<Confirm>,
    /// The media host's last `preview.status` result.
    pub status: Option<Value>,
    /// When the last poll was sent.
    pub polled: Option<Instant>,
    /// A background `browser.install` on this machine is running.
    pub installing: Option<usize>,
    /// Last click on a popup row: (when, row index).
    last_click: Option<(Instant, usize)>,
    /// The sidebar preview row showing its action chips: (machine, preview id).
    pub expanded: Option<(usize, String)>,
    /// Last left click on a sidebar preview row: (when, machine, preview id).
    side_click: Option<(Instant, usize, String)>,
}

/// One popup row.
#[derive(Debug, Clone)]
pub struct Row {
    pub mi: usize,
    pub preview: Preview,
    /// `devbox/v14` (prefix with more than one machine) or `v14`.
    pub name: String,
    pub label: String,
    /// `w8:p15 claude`, empty without an owning pane.
    pub owner: String,
    pub pane_open: bool,
    /// Plain proxy origin (`http://v14-….vibeke.localhost:47800/`), never a token.
    pub proxy: Option<String>,
    /// Mirrored on this local port.
    pub mirror: Option<u16>,
    pub errors: String,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The media host: the local machine (its server runs browser panes, windows, the proxy and
/// mirrors), else the current machine (plain SSH: the owner hosts the media itself).
pub fn host(app: &App) -> usize {
    app.machines.iter().position(|m| m.local).unwrap_or(app.cur)
}

/// Numeric part of `v14` for sorting.
fn handle_num(h: &str) -> u64 {
    h.trim_start_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .unwrap_or(u64::MAX)
}

/// The plain proxy origin of preview `p` of machine `mi` from the media host's status.
fn proxy_origin(app: &App, mi: usize, p: &Preview) -> Option<String> {
    let st = app.preview_mgr.status.as_ref()?;
    let proxy = st.get("proxy")?;
    let port = proxy.get("port")?.as_u64()?;
    let label = &app.machines.get(mi)?.label;
    let local = app.machines[mi].local || mi == host(app);
    proxy.get("routes")?.as_array()?.iter().find_map(|r| {
        let m = r.get("machine")?.as_str()?;
        let same_machine = m == label || (m == "local" && local);
        let same = same_machine
            && (r.get("preview")?.as_str()? == p.id
                || r.get("handle").and_then(Value::as_str) == Some(p.handle.as_str()));
        if !same {
            return None;
        }
        let host = r.get("host")?.as_str()?;
        let scheme = if r.get("tls").and_then(Value::as_bool) == Some(true) {
            "https"
        } else {
            "http"
        };
        Some(format!("{scheme}://{host}:{port}/"))
    })
}

/// Every machine's live previews, the focused workspace's first, then by machine and handle.
pub fn rows(app: &App) -> Vec<Row> {
    let multi = app.machines.len() > 1;
    let focused_ws = app.focused_ws().map(|w| w.id);
    let mut v: Vec<(bool, Row)> = Vec::new();
    for (mi, m) in app.machines.iter().enumerate() {
        for p in m
            .model
            .previews
            .iter()
            .filter(|p| p.status != PreviewStatus::Gone)
        {
            let pane = p
                .pane
                .as_ref()
                .and_then(|id| m.model.panes.iter().find(|x| &x.id == id));
            let in_focus =
                mi == app.cur && pane.is_some_and(|x| Some(&x.workspace) == focused_ws.as_ref());
            let name = if multi {
                format!("{}/{}", m.label, p.handle)
            } else {
                p.handle.clone()
            };
            // Labels and titles come from dev-server output and terminals: escape controls.
            let label = p
                .label
                .as_deref()
                .map(|l| vk_proto::text::escape_controls(l).into_owned())
                .unwrap_or_default();
            let owner = pane
                .map(|x| {
                    format!(
                        "{} {}",
                        x.handle,
                        vk_proto::text::escape_controls(&x.display_title())
                    )
                })
                .unwrap_or_default();
            let pane_open = m.model.panes.iter().any(|x| {
                x.browser
                    .as_ref()
                    .is_some_and(|b| b.preview.as_deref() == Some(p.id.as_str()))
            });
            v.push((
                in_focus,
                Row {
                    mi,
                    name,
                    label,
                    owner,
                    pane_open,
                    proxy: proxy_origin(app, mi, p),
                    mirror: mirror_of(app, mi, p).map(|x| x.port),
                    errors: crate::preview_ui::badge_text(app, mi, &p.handle)
                        .trim()
                        .to_string(),
                    preview: p.clone(),
                },
            ));
        }
    }
    v.sort_by(|(fa, a), (fb, b)| {
        fb.cmp(fa)
            .then(a.mi.cmp(&b.mi))
            .then(handle_num(&a.preview.handle).cmp(&handle_num(&b.preview.handle)))
    });
    v.into_iter().map(|(_, r)| r).collect()
}

/// Rows matching the filter (case-insensitive, every word somewhere in the row).
pub fn filtered(app: &App) -> Vec<Row> {
    let f = app.preview_mgr.filter.to_lowercase();
    rows(app)
        .into_iter()
        .filter(|r| {
            let hay = format!(
                "{} :{}{} {} {} {}",
                r.name, r.preview.port, r.preview.path, r.label, r.owner, r.preview.url
            )
            .to_lowercase();
            f.split_whitespace().all(|w| hay.contains(w))
        })
        .collect()
}

pub fn open(app: &mut App) {
    let st = &mut app.preview_mgr;
    st.sel = 0;
    st.filter.clear();
    st.typing = false;
    st.confirm = None;
    st.last_click = None;
    app.mode = Mode::Popup(Popup::Previews);
    poll(app);
}

pub fn action(app: &mut App, action: &str) -> bool {
    if action == "preview_list" {
        open(app);
        return true;
    }
    false
}

fn is_open(app: &App) -> bool {
    matches!(app.mode, Mode::Popup(Popup::Previews))
}

/// Ask the media host for `preview.status`.
pub fn poll(app: &mut App) {
    let h = host(app);
    if !app.machines.get(h).is_some_and(|m| m.connected()) {
        return;
    }
    app.preview_mgr.polled = Some(Instant::now());
    app.command_on(
        h,
        "preview.status",
        json!({}),
        Pending::Preview(Reply::Status),
    );
}

/// The `preview.status` reply: keep it, refresh the mirror list, report a finished install.
pub fn on_status(app: &mut App, mi: usize, v: Value) {
    if mi != host(app) {
        return;
    }
    if let Some(a) = v["mirrors"].as_array() {
        app.browser.mirrors = a
            .iter()
            .filter_map(crate::browser::MirrorInfo::parse)
            .collect();
    }
    if let Some(im) = app.preview_mgr.installing
        && v["browser_install"]["running"] == false
    {
        app.preview_mgr.installing = None;
        let m = app.machines[im].label.clone();
        match v["browser_install"]["error"].as_str() {
            Some(e) => app.toast(format!("✗ browser install on {m} failed: {e}")),
            None => app.toast(format!(
                "✓ browser installed on {m}: browser panes can open now"
            )),
        }
    }
    app.preview_mgr.status = Some(v);
}

/// Poll every [`POLL`] while the popup is open or an install is running.
pub(crate) fn deadlines(app: &App, now: Instant, d: &mut crate::deadline::Deadlines) {
    if (is_open(app) || app.preview_mgr.installing.is_some())
        && app.machines.get(host(app)).is_some_and(|m| m.connected())
    {
        d.at(
            "previews.poll",
            app.preview_mgr.polled.map_or(now, |t| t + POLL),
        );
    }
}

pub fn tick(app: &mut App) {
    if (is_open(app) || app.preview_mgr.installing.is_some())
        && app.preview_mgr.polled.is_none_or(|t| t.elapsed() >= POLL)
    {
        poll(app);
    }
    // The expanded sidebar row goes with its preview.
    if let Some((mi, id)) = app.preview_mgr.expanded.clone()
        && find(app, mi, &id).is_none()
    {
        app.preview_mgr.expanded = None;
        app.dirty = true;
    }
}

fn find(app: &App, mi: usize, id: &str) -> Option<Preview> {
    app.machines
        .get(mi)?
        .model
        .previews
        .iter()
        .find(|p| p.id == id && p.status != PreviewStatus::Gone)
        .cloned()
}

/// What the media host would launch for browser panes: `Some(Some(kind))` available,
/// `Some(None)` none, `None` unknown (no status yet, or an older server).
pub fn pane_browser(app: &App) -> Option<Option<String>> {
    let b = app.preview_mgr.status.as_ref()?.get("available_browsers")?;
    Some(b.get("pane")?["kind"].as_str().map(str::to_string))
}

// ---- actions ------------------------------------------------------------------------------

/// Mirror without asking (after the confirmation).
pub fn mirror_confirmed(app: &mut App, mi: usize, id: &str) {
    match find(app, mi, id) {
        Some(p) => crate::browser::preview_mirror(app, mi, &p),
        None => app.toast("that preview is gone"),
    }
}

/// `m`/`[mirror]`: unmirror a mirrored preview; mirroring a remote one asks first (the popup
/// asks inline, the sidebar with a confirm popup). A local one gets the browser's own refusal.
fn mirror_toggle(app: &mut App, mi: usize, p: &Preview, in_popup: bool) {
    if mirror_of(app, mi, p).is_some() {
        return crate::browser::preview_unmirror(app, mi, p);
    }
    if crate::browser::local_target(app, mi, p).0 == mi {
        return crate::browser::preview_mirror(app, mi, p);
    }
    if in_popup {
        app.preview_mgr.confirm = Some(Confirm::Mirror(mi, p.id.clone()));
    } else {
        app.mode = Mode::Popup(Popup::Confirm {
            message: mirror_question(app, mi, p),
            action: Box::new(Action::MirrorPreview {
                machine: mi,
                preview: p.id.clone(),
            }),
        });
    }
}

fn mirror_question(app: &App, mi: usize, p: &Preview) -> String {
    format!(
        "Mirror {}/{} to localhost:{}? It opens an unauthenticated port on this machine.",
        app.machines[mi].label, p.handle, p.port
    )
}

/// The plain preview URL to the clipboard (never a proxy token).
fn copy_url(app: &mut App, p: &Preview) {
    app.copy_text(&p.url);
}

fn forget(app: &mut App, mi: usize, id: &str) {
    let Some(p) = find(app, mi, id) else {
        return app.toast("that preview is gone");
    };
    // Through the viewing server, which drops its proxy origins for it too.
    let (local, target) = crate::browser::local_target(app, mi, &p);
    app.command_on(
        local,
        "preview.forget",
        json!({"preview": target}),
        Pending::Toast(format!("forgot {}", p.handle)),
    );
}

fn install(app: &mut App, mi: usize) {
    let m = app.machines[mi].label.clone();
    app.command_on(
        mi,
        "browser.install",
        json!({"confirm": true, "background": true}),
        Pending::Preview(Reply::Install),
    );
    app.toast(format!(
        "installing a browser on {m} (downloads about 100 MB)…"
    ));
}

/// The `browser.install` reply.
pub fn on_install(app: &mut App, mi: usize, res: Result<Value, crate::app::RpcErr>) {
    let m = app.machines[mi].label.clone();
    match res {
        Err(e) => app.toast(format!("✗ browser install on {m}: {}", e.message)),
        Ok(v) if v["started"] == true || v["running"] == true => {
            app.preview_mgr.installing = Some(mi);
            poll(app);
        }
        // An older server installs before it answers.
        Ok(v) if v["installed"] == true => {
            app.toast(format!("✓ browser installed on {m}"));
            poll(app);
        }
        Ok(_) => app.toast(format!("✗ browser install on {m}: unexpected reply")),
    }
}

/// The toast for a browser pane that found no browser on its media host `mi`.
pub fn no_browser_toast(app: &mut App, mi: usize) {
    let m = app
        .machines
        .get(mi)
        .map(|m| m.label.clone())
        .unwrap_or_default();
    app.toast(format!(
        "✗ no browser on {m} for browser panes: run `vibeke browser install` there (or prefix+shift+o, then i)"
    ));
}

// ---- keys ---------------------------------------------------------------------------------

pub fn key(app: &mut App, ev: KeyEvent) {
    app.mode = Mode::Popup(Popup::Previews);
    if ev.kind == KeyKind::Release {
        return;
    }
    let list = filtered(app);
    let n = list.len();
    let st = &mut app.preview_mgr;
    st.sel = st.sel.min(n.saturating_sub(1));
    if let Some(c) = st.confirm.clone() {
        match ev.key {
            Key::Char('y' | 'Y') | Key::Named(NamedKey::Enter) => {
                app.preview_mgr.confirm = None;
                match c {
                    Confirm::Mirror(mi, id) => mirror_confirmed(app, mi, &id),
                    Confirm::Forget(mi, id) => forget(app, mi, &id),
                    Confirm::Install(mi) => install(app, mi),
                }
            }
            Key::Char('n' | 'N' | 'q') | Key::Named(NamedKey::Escape) => {
                app.preview_mgr.confirm = None
            }
            _ => {}
        }
        return;
    }
    if st.typing {
        match ev.key {
            Key::Named(NamedKey::Escape) => {
                st.filter.clear();
                st.typing = false;
            }
            Key::Named(NamedKey::Enter) => st.typing = false,
            Key::Named(NamedKey::Backspace) => {
                st.filter.pop();
            }
            Key::Named(NamedKey::Down) => st.sel = (st.sel + 1).min(n.saturating_sub(1)),
            Key::Named(NamedKey::Up) => st.sel = st.sel.saturating_sub(1),
            Key::Char(c) if !ev.mods.ctrl() && !ev.mods.alt() => {
                st.filter.push(c);
                st.sel = 0;
            }
            _ => {}
        }
        return;
    }
    let cur = list.get(st.sel).cloned();
    match ev.key {
        Key::Named(NamedKey::Escape) if !st.filter.is_empty() => {
            st.filter.clear();
            st.sel = 0;
        }
        Key::Named(NamedKey::Escape) | Key::Char('q') => app.mode = Mode::Normal,
        Key::Named(NamedKey::Down) | Key::Char('j') => {
            st.sel = (st.sel + 1).min(n.saturating_sub(1))
        }
        Key::Named(NamedKey::Up) | Key::Char('k') => st.sel = st.sel.saturating_sub(1),
        Key::Char('/') => st.typing = true,
        Key::Char('i') => ask_install(app),
        _ => {
            let Some(r) = cur else {
                return;
            };
            let (mi, p) = (r.mi, r.preview);
            match ev.key {
                Key::Named(NamedKey::Enter) => {
                    app.mode = Mode::Normal;
                    crate::browser::open_preview(app, mi, &p, p.pane.clone());
                }
                Key::Char('w') => crate::browser::preview_window(app, mi, &p),
                Key::Char('p') => crate::browser::preview_proxy(app, mi, &p),
                Key::Char('m') => mirror_toggle(app, mi, &p, true),
                Key::Char('y') => copy_url(app, &p),
                Key::Char('g') => match p.pane.clone() {
                    Some(pane) if app.machines[mi].model.panes.iter().any(|x| x.id == pane) => {
                        app.mode = Mode::Normal;
                        app.focus_pane(mi, &pane);
                    }
                    _ => app.toast(format!("no pane owns {}", p.handle)),
                },
                Key::Char('d') => app.preview_mgr.confirm = Some(Confirm::Forget(mi, p.id.clone())),
                _ => {}
            }
        }
    }
}

/// `i`: install a browser on the media host when it has none (asks first).
fn ask_install(app: &mut App) {
    let h = host(app);
    match pane_browser(app) {
        Some(None) if app.preview_mgr.installing.is_some() => {
            app.toast("a browser install is already running")
        }
        Some(None) => app.preview_mgr.confirm = Some(Confirm::Install(h)),
        Some(Some(kind)) => app.toast(format!(
            "{} already has a browser for panes ({kind})",
            app.machines[h].label
        )),
        None => app.toast(format!(
            "{} did not say which browsers it has (older server?): run `vibeke browser install` there",
            app.machines[h].label
        )),
    }
}

// ---- drawing ------------------------------------------------------------------------------

/// Popup geometry: the box, the list's first row and height, and the first row shown.
struct Geo {
    r: SRect,
    list_y: u16,
    list_h: u16,
    skip: usize,
}

fn geo(app: &App, n: usize) -> Geo {
    let area = app.pane_area();
    let w = 100.min(area.w.saturating_sub(2)).max(40);
    let list_h = n.clamp(1, MAX_ROWS) as u16;
    // Borders 2, banner 1, separators 2, detail 3, footer 2.
    let h = (list_h + 10).min(area.h.saturating_sub(1)).max(10);
    let list_h = list_h.min(h.saturating_sub(10)).max(1);
    let x = area.x + area.w.saturating_sub(w) / 2;
    let y = area.y + area.h.saturating_sub(h) / 3;
    let sel = app.preview_mgr.sel;
    Geo {
        r: SRect { x, y, w, h },
        list_y: y + 2,
        list_h,
        skip: sel.saturating_sub(list_h.saturating_sub(1) as usize),
    }
}

fn pad(s: &str, w: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw > w {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push_str(&" ".repeat(w.saturating_sub(used)));
    out
}

/// `:5173`, or `:5173/app` with a path.
fn port_path(p: &Preview) -> String {
    match p.path.as_str() {
        "" | "/" => format!(":{}", p.port),
        path => format!(":{}{path}", p.port),
    }
}

fn status_text(p: &Preview) -> (String, i64) {
    let now = now_ms();
    match p.status {
        PreviewStatus::Up => ("up".into(), now - p.first_seen_ms),
        PreviewStatus::Down => ("down".into(), now - p.last_seen_ms),
        PreviewStatus::Declared => ("declared".into(), now - p.first_seen_ms),
        PreviewStatus::Suggested => ("suggested".into(), now - p.first_seen_ms),
        PreviewStatus::Gone => ("gone".into(), now - p.last_seen_ms),
    }
}

fn source_text(s: &PreviewSource) -> &'static str {
    match s {
        PreviewSource::Declared => "declared",
        PreviewSource::Listener => "listening socket",
        PreviewSource::OutputUrl => "printed URL",
        PreviewSource::Banner => "dev-server banner",
    }
}

/// Row segments (without the selection marker).
fn row_segs(app: &App, r: &Row) -> Vec<(String, Style)> {
    let t = &app.theme;
    let p = &r.preview;
    let (dot, color) = match p.status {
        PreviewStatus::Up | PreviewStatus::Declared => ("●", t.green),
        PreviewStatus::Down => ("○", t.red),
        _ => ("◌", t.muted),
    };
    let mut segs = vec![
        (format!("{dot} "), t.s(color)),
        (format!("{} ", pad(&r.name, 14)), t.bold(t.fg)),
        (format!("{} ", pad(&port_path(p), 9)), t.text()),
        (format!("{} ", pad(&r.label, 12)), t.text()),
    ];
    if p.status == PreviewStatus::Down {
        let (_, age) = status_text(p);
        segs.push((
            format!(
                "{} ",
                pad(&format!("(down {})", crate::inbox::fmt_age(age)), 18)
            ),
            t.dim(),
        ));
    } else {
        segs.push((format!("{} ", pad(&r.owner, 18)), t.dim()));
    }
    if r.pane_open {
        segs.push(("▣ pane ".into(), t.s(t.accent)));
    }
    if r.proxy.is_some() {
        segs.push(("◎ proxy ".into(), t.s(t.accent)));
    }
    if let Some(port) = r.mirror {
        segs.push((format!("⇄ :{port} "), t.bold(t.yellow)));
    }
    if !r.errors.is_empty() {
        segs.push((r.errors.clone(), t.bold(t.red)));
    }
    segs
}

/// The banner line: whether the media host can show browser panes.
fn banner(app: &App) -> (String, Style) {
    let t = &app.theme;
    let h = host(app);
    let m = app.machines.get(h).map(|m| m.label.as_str()).unwrap_or("");
    if app.preview_mgr.installing.is_some() {
        return (
            format!("⟳ installing a browser on {m} for browser panes…"),
            t.s(t.yellow),
        );
    }
    match pane_browser(app) {
        Some(None) => (
            format!(
                "✗ no browser on {m} for browser panes: run `vibeke browser install` there · i install"
            ),
            t.bold(t.red),
        ),
        Some(Some(kind)) => {
            let window = app
                .preview_mgr
                .status
                .as_ref()
                .and_then(|s| s["available_browsers"]["window"]["kind"].as_str())
                .map(|k| format!(" · window: {k}"))
                .unwrap_or_else(|| " · window: none".into());
            (format!("browser on {m}: {kind}{window}"), t.dim())
        }
        None if app.preview_mgr.status.is_none() => ("checking browsers…".into(), t.dim()),
        None => (String::new(), t.dim()),
    }
}

pub fn draw(app: &App, g: &mut Grid) {
    let t = app.theme;
    let list = filtered(app);
    let total = rows(app).len();
    let mirrored = app.browser.mirrors.len();
    let st = &app.preview_mgr;
    let geo = geo(app, list.len());
    let SRect { x, y, w, h } = geo.r;
    let title = if st.typing || !st.filter.is_empty() {
        format!("Previews · /{}", st.filter)
    } else {
        "Previews".into()
    };
    drop(crate::popups::frame_at(app, g, geo.r, &title));
    // Counts at the right end of the top border.
    let counts = if mirrored > 0 {
        format!(" {total} · {mirrored} mirrored ")
    } else {
        format!(" {total} ")
    };
    let cw = UnicodeWidthStr::width(counts.as_str()) as u16;
    if cw + 4 < w {
        g.put_str(x + w - cw - 2, y, &counts, t.dim(), cw);
    }
    let inner = w.saturating_sub(4);
    let (bt, bs) = banner(app);
    g.put_str(x + 2, y + 1, &bt, bs, inner);
    for (row, (i, r)) in list
        .iter()
        .enumerate()
        .skip(geo.skip)
        .take(geo.list_h as usize)
        .enumerate()
    {
        let yy = geo.list_y + row as u16;
        let selected = i == st.sel;
        if selected {
            g.fill(
                SRect {
                    x: x + 1,
                    y: yy,
                    w: w.saturating_sub(2),
                    h: 1,
                },
                t.sel(t.fg),
            );
        }
        let mut cx = x + 2;
        let marker = if selected { "› " } else { "  " };
        let segs = std::iter::once((marker.to_string(), t.bold(t.accent))).chain(row_segs(app, r));
        for (s, mut sty) in segs {
            if selected {
                sty.bg = t.selection;
            }
            let used = cx - (x + 2);
            cx += g.put_str(cx, yy, &s, sty, inner.saturating_sub(used));
        }
    }
    if list.is_empty() {
        let msg = if total == 0 {
            "no previews: start a dev server in a pane, or `vibeke preview declare <port>`"
        } else {
            "nothing matches"
        };
        g.put_str(x + 2, geo.list_y, msg, t.dim(), inner);
    }
    // Separators.
    let sep = |g: &mut Grid, yy: u16| {
        let b = t.border(true);
        g.put_str(x, yy, "├", b, 1);
        for i in x + 1..x + w - 1 {
            g.put_str(i, yy, "─", b, 1);
        }
        g.put_str(x + w - 1, yy, "┤", b, 1);
    };
    let sep1 = geo.list_y + geo.list_h;
    sep(g, sep1);
    // Detail strip.
    if let Some(r) = list.get(st.sel) {
        let p = &r.preview;
        let (status, age) = status_text(p);
        let line1 = format!(
            "{}  ·  {}  ·  {status} {}  ·  {}",
            p.url,
            app.machines[r.mi].label,
            crate::inbox::fmt_age(age),
            source_text(&p.source)
        );
        g.put_str(x + 2, sep1 + 1, &line1, t.text(), inner);
        match &r.proxy {
            Some(o) => g.put_str(x + 2, sep1 + 2, &format!("proxy  {o}"), t.text(), inner),
            None => g.put_str(x + 2, sep1 + 2, "proxy  –  (p opens one)", t.dim(), inner),
        };
        let remote = crate::browser::local_target(app, r.mi, p).0 != r.mi;
        match r.mirror {
            Some(port) => {
                let n = g.put_str(
                    x + 2,
                    sep1 + 3,
                    &format!("mirror localhost:{port}  "),
                    t.text(),
                    inner,
                );
                g.put_str(
                    x + 2 + n,
                    sep1 + 3,
                    "⚠ unauthenticated",
                    t.bold(t.yellow),
                    inner.saturating_sub(n),
                );
            }
            None if remote => {
                g.put_str(x + 2, sep1 + 3, "mirror –  (m mirrors)", t.dim(), inner);
            }
            None => {
                g.put_str(
                    x + 2,
                    sep1 + 3,
                    "mirror –  (on this machine already)",
                    t.dim(),
                    inner,
                );
            }
        }
    }
    let sep2 = sep1 + 4;
    sep(g, sep2);
    // Footer: keys, or the pending question.
    let (f1, f2, fs) = match &st.confirm {
        Some(Confirm::Mirror(mi, id)) => (
            find(app, *mi, id)
                .map(|p| mirror_question(app, *mi, &p))
                .unwrap_or_default(),
            "y mirror   n cancel".to_string(),
            t.bold(t.yellow),
        ),
        Some(Confirm::Forget(mi, id)) => (
            format!(
                "Forget {}? It disappears until the port is found again.",
                find(app, *mi, id).map(|p| p.handle).unwrap_or_default()
            ),
            "y forget   n cancel".to_string(),
            t.bold(t.yellow),
        ),
        Some(Confirm::Install(mi)) => (
            format!(
                "Install Chrome for Testing's headless shell on {} (downloads about 100 MB)?",
                app.machines[*mi].label
            ),
            "y install   n cancel".to_string(),
            t.bold(t.yellow),
        ),
        None if st.typing => (
            "type to filter · enter keep · esc clear".to_string(),
            String::new(),
            t.dim(),
        ),
        None => (
            "⏎ pane  w window  p proxy  m mirror/unmirror  y copy url".to_string(),
            "g go to pane  d forget  / filter  i install browser  esc close".to_string(),
            t.dim(),
        ),
    };
    g.put_str(x + 2, sep2 + 1, &f1, fs, inner);
    if sep2 + 2 < y + h - 1 {
        g.put_str(x + 2, sep2 + 2, &f2, fs, inner);
    }
}

// ---- mouse --------------------------------------------------------------------------------

/// Clicks on the popup: a row selects, a second click on it opens. True when consumed.
pub fn on_mouse(app: &mut App, me: &MouseEvent) -> bool {
    if !is_open(app) {
        return false;
    }
    if !matches!(me.kind, MouseEventKind::Down(CtButton::Left)) {
        return matches!(me.kind, MouseEventKind::Down(_));
    }
    let list = filtered(app);
    let geo = geo(app, list.len());
    let SRect { x, y, w, h } = geo.r;
    let (cx, cy) = (me.column, me.row);
    if cx < x || cx >= x + w || cy < y || cy >= y + h {
        // A click outside closes it.
        app.mode = Mode::Normal;
        return true;
    }
    if cy >= geo.list_y && cy < geo.list_y + geo.list_h {
        let i = geo.skip + (cy - geo.list_y) as usize;
        if let Some(r) = list.get(i) {
            let now = Instant::now();
            let double = app
                .preview_mgr
                .last_click
                .is_some_and(|(t, j)| j == i && now.duration_since(t) < DOUBLE_CLICK);
            app.preview_mgr.sel = i;
            app.preview_mgr.last_click = Some((now, i));
            if double {
                app.preview_mgr.last_click = None;
                app.mode = Mode::Normal;
                let p = r.preview.clone();
                crate::browser::open_preview(app, r.mi, &p, p.pane.clone());
            }
        }
    }
    app.dirty = true;
    true
}

// ---- sidebar chips ------------------------------------------------------------------------

/// The chips of preview `p` of machine `mi`.
pub fn chips(app: &App, mi: usize, p: &Preview) -> Vec<Chip> {
    let mirror = if mirror_of(app, mi, p).is_some() {
        Chip::Unmirror
    } else {
        Chip::Mirror
    };
    vec![Chip::Pane, Chip::Window, Chip::Proxy, mirror, Chip::Copy]
}

/// Sidebar rows for the expanded preview's chips, wrapped to `width`: per row its segments
/// and each chip's x range (relative to the sidebar's left edge).
pub fn chip_rows(
    app: &App,
    mi: usize,
    p: &Preview,
    width: u16,
) -> Vec<(Vec<(String, Style)>, Vec<(Chip, u16, u16)>)> {
    let t = &app.theme;
    const INDENT: u16 = 4;
    let mut out = Vec::new();
    let mut segs: Vec<(String, Style)> = vec![(" ".repeat(INDENT as usize), t.text())];
    let mut hits = Vec::new();
    let mut x = INDENT;
    for c in chips(app, mi, p) {
        let w = UnicodeWidthStr::width(c.label()) as u16;
        if x > INDENT && x + w > width {
            out.push((std::mem::take(&mut segs), std::mem::take(&mut hits)));
            segs.push((" ".repeat(INDENT as usize), t.text()));
            x = INDENT;
        }
        let st = match c {
            Chip::Unmirror => t.bold(t.yellow),
            _ => t.bold(t.accent),
        };
        segs.push((c.label().to_string(), st));
        hits.push((c, x, x + w));
        segs.push((" ".into(), t.text()));
        x += w + 1;
    }
    out.push((segs, hits));
    out
}

/// Run a chip's action.
pub fn run_chip(app: &mut App, mi: usize, p: &Preview, c: Chip) {
    match c {
        Chip::Pane => crate::browser::open_preview(app, mi, p, p.pane.clone()),
        Chip::Window => crate::browser::preview_window(app, mi, p),
        Chip::Proxy => crate::browser::preview_proxy(app, mi, p),
        Chip::Mirror | Chip::Unmirror => mirror_toggle(app, mi, p, false),
        Chip::Copy => copy_url(app, p),
    }
}

/// Expand (or collapse) a sidebar preview row's chips.
pub fn toggle_expanded(app: &mut App, mi: usize, id: &str) {
    let key = (mi, id.to_string());
    app.preview_mgr.expanded = if app.preview_mgr.expanded.as_ref() == Some(&key) {
        None
    } else {
        Some(key)
    };
    app.dirty = true;
}

/// A left click on a sidebar preview row: expand/collapse; a double-click opens it.
pub fn sidebar_click(app: &mut App, mi: usize, p: &Preview) {
    let now = Instant::now();
    let double = app
        .preview_mgr
        .side_click
        .as_ref()
        .is_some_and(|(t, m, id)| *m == mi && *id == p.id && now.duration_since(*t) < DOUBLE_CLICK);
    if double {
        app.preview_mgr.side_click = None;
        app.preview_mgr.expanded = None;
        crate::browser::open_preview(app, mi, p, p.pane.clone());
        return;
    }
    app.preview_mgr.side_click = Some((now, mi, p.id.clone()));
    toggle_expanded(app, mi, &p.id);
}

/// The chip under sidebar column `x` (relative to the sidebar) on host row `y`.
pub fn chip_hit(app: &App, x: u16, y: u16) -> Option<(usize, Preview, Chip)> {
    let rows = crate::draw::sidebar_rows(app);
    let r = rows.get((y as usize).checked_sub(1)?)?;
    let (mi, id) = r.preview.clone()?;
    let c = r
        .chips
        .iter()
        .find(|(_, a, b)| x >= *a && x < *b)
        .map(|(c, _, _)| *c)?;
    Some((mi, find(app, mi, &id)?, c))
}

/// Navigate-mode keys on a sidebar preview row: `enter`/`space` expand or collapse, `esc`
/// collapses, `o` opens, `w`/`p`/`m`/`y` as in the popup. Other keys are swallowed so pane
/// actions never get a preview id; moving, `/` and leaving still work.
pub fn navigate_key(app: &mut App, ev: &KeyEvent, sel: usize) -> bool {
    if ev.kind == KeyKind::Release || app.ux.nav.typing {
        return false;
    }
    let rows = crate::draw::sidebar_rows(app);
    let Some((mi, id)) = rows
        .iter()
        .filter(|r| r.selectable())
        .nth(sel)
        .and_then(|r| r.preview.clone())
    else {
        return false;
    };
    let Some(p) = find(app, mi, &id) else {
        return false;
    };
    let stay = |app: &mut App| app.mode = Mode::Navigate { sel };
    match ev.key {
        Key::Named(NamedKey::Enter) | Key::Char(' ') => {
            toggle_expanded(app, mi, &id);
            stay(app);
        }
        Key::Named(NamedKey::Escape)
            if app.preview_mgr.expanded.as_ref() == Some(&(mi, id.clone())) =>
        {
            app.preview_mgr.expanded = None;
            stay(app);
        }
        Key::Char('o') => {
            app.preview_mgr.expanded = None;
            crate::browser::open_preview(app, mi, &p, p.pane.clone());
        }
        Key::Char('w') => {
            crate::browser::preview_window(app, mi, &p);
            stay(app);
        }
        Key::Char('p') => {
            crate::browser::preview_proxy(app, mi, &p);
            stay(app);
        }
        Key::Char('m') => {
            stay(app);
            mirror_toggle(app, mi, &p, false);
        }
        Key::Char('y') => {
            copy_url(app, &p);
            stay(app);
        }
        Key::Named(NamedKey::Escape | NamedKey::Down | NamedKey::Up)
        | Key::Char('j' | 'k' | 'q' | '/') => return false,
        Key::Char(c) if c.is_ascii_digit() => return false,
        _ => stay(app),
    }
    true
}

#[cfg(test)]
#[path = "preview_manager_tests.rs"]
mod tests;
