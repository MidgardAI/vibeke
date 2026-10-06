//! Herdr plugin surfaces in the TUI (07 §7.7, 08 §5, §6.3, §6.8; M5):
//!
//! - **Popups and overlays.** `plugin.pane.open {placement: popup|overlay}` starts the plugin's
//!   command in a floating pane whose `created_by` carries a [`PluginSurface`] tag. A popup is
//!   drawn as a session-modal window (centred, sized from the manifest/request `width`/`height`
//!   in cells or `%`, default 80%×80%): it holds the keyboard focus, keys go to its terminal
//!   (direct bindings and prefix actions are suspended, `prefix prefix` still passes the prefix
//!   through), clicks outside it are ignored. An overlay is a full-area layer over the current
//!   tab with a one-row header; nothing under it is drawn, hit or sized. Both close when their
//!   command exits; `esc` after the exit, `prefix+x`/`prefix+esc` on a popup and `close_pane` on
//!   an overlay dismiss them early (`plugin.surface.close`, which also gives the focus back to
//!   the pane that had it). `esc` while the command runs goes to the command.
//! - **Palette entries.** `plugin.action.list` per machine (refreshed when the palette opens and
//!   after a connect): trusted and enabled actions run with `plugin.action.run`; untrusted,
//!   stale or disabled plugins are listed but disabled with the command that fixes them.
//! - **Action contexts.** An action declares where it applies (`global`, `workspace`, `tab`,
//!   `pane`, `selection`); the palette lists only actions whose context holds for the focused
//!   machine, and a key binding that fires elsewhere says why nothing ran.
//! - **Key bindings.** `[[keys.command]] type = "plugin_action"`, `command = "<plugin>.<action>"`,
//!   plus the default bindings plugin manifests declare: the server lists them (conflicts with
//!   the user's keys already skipped) in `plugin.action.list` and they are added to the keymap
//!   whenever that list is refreshed (connect, palette, config reload, `plugin.registry_changed`)
//!   and gone again once the plugin is disabled or unlinked.
//! - **Agent views.** `agent.view.set` lines (`compat.ui.state` / `plugin.agent_view_changed`)
//!   are shown after the run's state in the sidebar and under it in the peek.
//! - **Link handlers.** A hint label (`url_hints`) or Ctrl/Alt+click on a token that matches a
//!   plugin's `[[link_handlers]]` pattern offers the matching handlers next to the default
//!   action; the chosen handler runs with `plugin.link.open`, whose action gets
//!   `HERDR_PLUGIN_CLICKED_URL` and `HERDR_PLUGIN_LINK_HANDLER_ID`.
//! - **Window title.** `client.window_title_changed` (pushed) replaces the outer terminal title
//!   when `ui.title_sync` is on; otherwise it shows in the tab bar.
//! - **Scroll reports.** The copy-mode viewport position is reported with
//!   `ClientFrame::ScrollView` to servers listing `scroll_report`, which emit Herdr's
//!   `pane.scroll_changed`.

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::floats::{MIN_H, MIN_W, inner_rect};
use crate::screen::{Grid, Rect as SRect};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use vk_compat::herdr::manifest::{ActionContext, contexts_apply};
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};
use vk_proto::layout::Rect;
use vk_proto::model::{Pane, PluginSurface, SurfaceKind, Tab};
use vk_proto::render::{ClientFrame, Style};

/// `render.attach` feature of servers that take `ClientFrame::ScrollView`.
pub const SCROLL_FEATURE: &str = "scroll_report";
/// Scroll reports are coalesced to at most one per interval (the tick sends the last one).
const SCROLL_MIN_INTERVAL: Duration = Duration::from_millis(150);
/// At most one pull of focus back to an open popup per this interval.
const REFOCUS_EVERY: Duration = Duration::from_millis(500);
/// Popup size when the manifest and the request give none.
const POPUP_DEFAULT_PCT: f32 = 80.0;

#[derive(Debug, Clone, PartialEq)]
pub struct PluginAction {
    pub plugin: String,
    pub action: String,
    pub qualified: String,
    pub title: String,
    pub description: Option<String>,
    pub available: bool,
    /// `active`, `untrusted`, `stale_trust`, `disabled`, …
    pub status: String,
    /// Declared contexts (`global`, `workspace`, `tab`, `pane`, `selection`).
    pub contexts: Vec<String>,
}

/// A default key binding a plugin manifest declares and the server found free.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginKey {
    pub key: String,
    pub action: String,
}

/// A plugin-provided status line for an agent run.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentView {
    pub plugin: String,
    pub text: String,
    pub detail: Option<String>,
    pub tone: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LinkHandler {
    pub plugin: String,
    pub id: String,
    pub title: String,
    pub pattern: String,
    pub available: bool,
    pub status: String,
}

/// Plugin state of one machine.
#[derive(Debug, Default, Clone)]
pub struct Per {
    pub actions: Vec<PluginAction>,
    pub handlers: Vec<LinkHandler>,
    /// Installed manifest default bindings (`plugin.action.list`).
    pub keys: Vec<PluginKey>,
    /// Agent views by run id (`compat.ui.state`).
    pub views: HashMap<String, AgentView>,
    /// `client.window_title.set` override (sanitized), until `client.window_title.clear`.
    pub window_title: Option<String>,
}

#[derive(Debug, Default)]
pub struct State {
    pub per: HashMap<usize, Per>,
    /// Last `ScrollView` sent per machine: (pane, offset).
    scroll_sent: HashMap<usize, (String, u32)>,
    scroll_at: Option<Instant>,
    /// Surfaces whose dismissal was requested (not resent every frame).
    closing: HashSet<(usize, String)>,
    /// Last time focus was pulled back to an open popup (no ping-pong with other clients).
    refocus_at: Option<Instant>,
}

impl State {
    pub fn per(&self, mi: usize) -> Option<&Per> {
        self.per.get(&mi)
    }
    fn per_mut(&mut self, mi: usize) -> &mut Per {
        self.per.entry(mi).or_default()
    }
}

#[derive(Debug, Clone)]
pub enum Reply {
    Actions,
    Handlers,
    Ui,
    /// A started action/link handler (toast on success or failure).
    Ran(String),
}

/// C0/C1 controls stripped and bidi overrides removed from plugin-provided text shown in
/// chrome (09 §6), at most `max` characters.
pub fn sanitize(s: &str, max: usize) -> String {
    s.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(*c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}')
        })
        .take(max)
        .collect::<String>()
        .trim()
        .to_string()
}

// ---- server data -----------------------------------------------------------------------------

/// Ask machine `mi` for its plugin actions (palette) and link handlers.
pub fn refresh(app: &mut App, mi: usize) {
    if !app.machines[mi].connected() {
        return;
    }
    app.command_on(
        mi,
        "plugin.action.list",
        json!({}),
        Pending::Plugin(Reply::Actions),
    );
    app.command_on(
        mi,
        "plugin.link_handler.list",
        json!({}),
        Pending::Plugin(Reply::Handlers),
    );
}

pub fn on_connected(app: &mut App, mi: usize) {
    refresh(app, mi);
    app.command_on(mi, "compat.ui.state", json!({}), Pending::Plugin(Reply::Ui));
    app.plugins.scroll_sent.remove(&mi);
}

/// `plugin.registry_changed` / `plugin.agent_view_changed` (pushed): re-read actions, bindings
/// and views of machine `mi`.
pub fn on_registry_event(app: &mut App, mi: usize) {
    refresh(app, mi);
    if app.machines[mi].connected() {
        app.command_on(mi, "compat.ui.state", json!({}), Pending::Plugin(Reply::Ui));
    }
}

pub fn parse_keys(v: &Value) -> Vec<PluginKey> {
    v["keybindings"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|k| k["installed"].as_bool() == Some(true))
                .filter_map(|k| {
                    Some(PluginKey {
                        key: k["key"].as_str()?.to_string(),
                        action: k["action"].as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_views(v: &Value) -> HashMap<String, AgentView> {
    v["agent_views"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    Some((
                        x["run"].as_str()?.to_string(),
                        AgentView {
                            plugin: x["plugin_id"].as_str()?.to_string(),
                            text: sanitize(x["text"].as_str()?, 80),
                            detail: x["detail"].as_str().map(|d| sanitize_block(d, 512)),
                            tone: x["tone"].as_str().unwrap_or("info").to_string(),
                        },
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Like [`sanitize`] but keeps newlines (peek detail).
fn sanitize_block(s: &str, max: usize) -> String {
    s.lines()
        .map(|l| sanitize(l, 200))
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .take(max)
        .collect()
}

/// The view a plugin set for run `run` on machine `mi`.
pub fn agent_view<'a>(app: &'a App, mi: usize, run: &str) -> Option<&'a AgentView> {
    app.plugins.per(mi)?.views.get(run)
}

/// Rebuild the keymap from the config and add every machine's installed plugin bindings. A
/// binding the keymap already uses (user keys win) is skipped.
pub fn apply_keys(app: &mut App) {
    let mut km = crate::keymap::Keymap::from_config(&app.config);
    let mut mis: Vec<&usize> = app.plugins.per.keys().collect();
    mis.sort();
    for mi in mis {
        for k in &app.plugins.per[mi].keys {
            km.add_plugin_binding(&format!("plugin:{mi}:{}", k.action), &k.key);
        }
    }
    app.keymap = km;
}

pub fn parse_actions(v: &Value) -> Vec<PluginAction> {
    let s = |x: &Value, k: &str| x[k].as_str().unwrap_or("").to_string();
    v["actions"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|x| x["plugin_id"].is_string() && x["action_id"].is_string())
                .map(|x| PluginAction {
                    plugin: s(x, "plugin_id"),
                    action: s(x, "action_id"),
                    qualified: x["qualified_id"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("{}.{}", s(x, "plugin_id"), s(x, "action_id"))),
                    title: sanitize(&s(x, "title"), 80),
                    description: x["description"].as_str().map(|d| sanitize(d, 120)),
                    available: x["available"].as_bool().unwrap_or(false),
                    status: s(x, "status"),
                    contexts: x["contexts"]
                        .as_array()
                        .map(|c| {
                            c.iter()
                                .filter_map(|c| c.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_handlers(v: &Value) -> Vec<LinkHandler> {
    let s = |x: &Value, k: &str| x[k].as_str().unwrap_or("").to_string();
    v["handlers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|x| x["pattern"].is_string())
                .map(|x| LinkHandler {
                    plugin: s(x, "plugin_id"),
                    id: s(x, "handler_id"),
                    title: sanitize(&s(x, "title"), 80),
                    pattern: s(x, "pattern"),
                    available: x["available"].as_bool().unwrap_or(false),
                    status: s(x, "status"),
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn on_reply(app: &mut App, mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    match r {
        // Older servers (no plugin API) answer method_not_found: nothing to show.
        Reply::Actions => {
            let (actions, keys) = match &res {
                Ok(v) => (parse_actions(v), parse_keys(v)),
                Err(_) => (vec![], vec![]),
            };
            let p = app.plugins.per_mut(mi);
            let keys_changed = p.keys != keys;
            p.actions = actions;
            p.keys = keys;
            if keys_changed {
                apply_keys(app);
            }
        }
        Reply::Handlers => {
            app.plugins.per_mut(mi).handlers = res.map(|v| parse_handlers(&v)).unwrap_or_default()
        }
        Reply::Ui => {
            if let Ok(v) = res {
                set_title(app, mi, v["window_title"].as_str());
                app.plugins.per_mut(mi).views = parse_views(&v);
            }
        }
        Reply::Ran(what) => match res {
            Ok(_) => app.toast(format!("▶ {what}")),
            Err(e) => app.toast(format!("{what}: {}", e.message)),
        },
    }
    app.dirty = true;
}

// ---- window title ----------------------------------------------------------------------------

fn set_title(app: &mut App, mi: usize, title: Option<&str>) {
    let t = title.map(|t| sanitize(t, 120)).filter(|t| !t.is_empty());
    app.plugins.per_mut(mi).window_title = t;
    app.dirty = true;
}

/// A pushed `client.window_title_changed` event.
pub fn on_title_event(app: &mut App, mi: usize, ev: &Value) {
    set_title(app, mi, ev["data"]["title"].as_str());
}

/// The plugin-set window title of the focused machine.
pub fn window_title(app: &App) -> Option<&str> {
    app.plugins.per(app.cur)?.window_title.as_deref()
}

// ---- popups and overlays ---------------------------------------------------------------------

/// A plugin popup/overlay as drawn this frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Surface {
    pub pane: String,
    pub info: PluginSurface,
    pub title: String,
    /// Window (frame/header included) and the terminal's content area.
    pub outer: Rect,
    pub inner: Rect,
    pub exited: bool,
}

/// Window and content rects of a surface inside the pane area.
pub fn geometry(info: &PluginSurface, area: Rect) -> Option<(Rect, Rect)> {
    match info.kind {
        SurfaceKind::Overlay => {
            if area.h < 2 || area.w < 2 {
                return None;
            }
            let inner = Rect {
                x: area.x,
                y: area.y + 1,
                w: area.w,
                h: area.h - 1,
            };
            Some((area, inner))
        }
        SurfaceKind::Popup => {
            if area.w < MIN_W || area.h < MIN_H {
                return None;
            }
            let w = PluginSurface::cells(&info.width, area.w, POPUP_DEFAULT_PCT, MIN_W);
            let h = PluginSurface::cells(&info.height, area.h, POPUP_DEFAULT_PCT, MIN_H);
            let outer = Rect {
                x: area.x + (area.w - w) / 2,
                y: area.y + (area.h - h) / 2,
                w,
                h,
            };
            Some((outer, inner_rect(outer)))
        }
    }
}

/// Surfaces of `tab` (overlays below popups, each kind in z order).
pub fn surfaces_in(panes: &[Pane], tab: &Tab, area: Rect) -> Vec<Surface> {
    let mut v: Vec<(u8, u32, Surface)> = tab
        .floating
        .iter()
        .filter_map(|f| {
            let p = panes.iter().find(|p| p.id == f.pane)?;
            let info = p.plugin_surface()?;
            let (outer, inner) = geometry(&info, area)?;
            Some((
                u8::from(info.is_popup()),
                f.z,
                Surface {
                    pane: p.id.clone(),
                    title: sanitize(p.display_title(), 80),
                    info,
                    outer,
                    inner,
                    exited: p.exited,
                },
            ))
        })
        .collect();
    v.sort_by_key(|(k, z, _)| (*k, *z));
    v.into_iter().map(|(_, _, s)| s).collect()
}

/// Surfaces of the focused tab.
pub fn surfaces(app: &App) -> Vec<Surface> {
    let m = app.m();
    let Some(tid) = m.focus.tab.as_ref() else {
        return vec![];
    };
    match m.model.tabs.iter().find(|t| &t.id == tid) {
        Some(tab) => surfaces_in(&m.model.panes, tab, app.pane_area()),
        None => vec![],
    }
}

/// Whether `pane` (on the focused machine) is a plugin popup/overlay.
pub fn is_surface(app: &App, pane: &str) -> bool {
    app.m()
        .model
        .panes
        .iter()
        .any(|p| p.id == pane && p.plugin_surface().is_some())
}

/// The visible popup of the focused tab (the topmost one).
pub fn popup(app: &App) -> Option<Surface> {
    surfaces(app).into_iter().rfind(|s| s.info.is_popup())
}

/// A live popup anywhere on the focused machine (popups are session-modal).
fn session_popup(app: &App) -> Option<String> {
    app.m()
        .model
        .panes
        .iter()
        .find(|p| !p.exited && p.plugin_surface().is_some_and(|s| s.is_popup()))
        .map(|p| p.id.clone())
}

/// Dismiss a surface: close it on the server, which restores the previous focus.
pub fn close(app: &mut App, mi: usize, pane: &str) {
    if app.plugins.closing.insert((mi, pane.to_string())) {
        app.command_on(
            mi,
            "plugin.surface.close",
            json!({"pane": pane}),
            Pending::Ignore,
        );
    }
}

/// Keys in terminal mode while a popup has the focus: `esc` after its command exited dismisses
/// it; everything else goes to the popup (direct bindings are suspended). True when handled.
pub fn normal_key(app: &mut App, ev: &KeyEvent) -> bool {
    let Some(p) = popup(app) else {
        return false;
    };
    if app.focused_pane().as_deref() != Some(p.pane.as_str()) {
        return false;
    }
    let cur = app.cur;
    if p.exited && matches!(ev.key, Key::Named(NamedKey::Escape)) {
        if ev.kind != KeyKind::Release {
            close(app, cur, &p.pane);
        }
        return true;
    }
    app.send_key(ev.clone());
    true
}

/// The chord after the prefix while a popup is open: `x`, `q` or `esc` (or the `close_pane`
/// binding) dismiss it, anything else is refused with a hint. True when handled.
pub fn prefix_key(app: &mut App, ev: &KeyEvent) -> bool {
    let Some(p) = popup(app) else {
        return false;
    };
    let close_binding = app
        .keymap
        .prefixed(ev)
        .is_some_and(|b| b.action == "close_pane");
    let cur = app.cur;
    if close_binding || matches!(ev.key, Key::Char('x' | 'q') | Key::Named(NamedKey::Escape)) {
        close(app, cur, &p.pane);
    } else {
        app.toast("a plugin popup is open — prefix+x closes it");
    }
    true
}

/// `close_pane` on a focused popup/overlay dismisses it the plugin way (focus restored).
pub fn action(app: &mut App, action: &str) -> bool {
    if action == "close_pane"
        && let Some(p) = app.focused_pane()
        && is_surface(app, &p)
    {
        let cur = app.cur;
        close(app, cur, &p);
        return true;
    }
    if let Some(rest) = action.strip_prefix("plugin:") {
        run_palette(app, rest);
        return true;
    }
    false
}

/// Mouse while a surface is shown: clicks outside an open popup are ignored (modal), overlay
/// headers and popup frames belong to Vibeke. False lets the event reach the pane under it.
pub fn on_mouse(app: &mut App, x: u16, y: u16) -> bool {
    let surf = surfaces(app);
    if surf.is_empty() {
        return false;
    }
    if let Some(p) = surf.iter().rev().find(|s| s.info.is_popup()) {
        return !p.inner.contains(x, y);
    }
    surf.iter()
        .any(|s| s.outer.contains(x, y) && !s.inner.contains(x, y))
}

/// Header (overlay) or frame (popup) of a surface; the caller draws the terminal into `inner`.
pub fn draw_chrome(app: &App, g: &mut Grid, s: &Surface, focused: bool) {
    let t = app.theme;
    let o = s.outer;
    g.fill(
        SRect {
            x: o.x,
            y: o.y,
            w: o.w,
            h: o.h,
        },
        Style::default(),
    );
    let state = if s.exited { " · exited" } else { "" };
    match s.info.kind {
        SurfaceKind::Overlay => {
            g.fill(
                SRect {
                    x: o.x,
                    y: o.y,
                    w: o.w,
                    h: 1,
                },
                t.text(),
            );
            let hint = if s.exited {
                " esc closes "
            } else {
                " prefix+x closes "
            };
            let hw = hint.chars().count() as u16;
            let title = format!(" ⧉ {} · {}{state} ", s.title, sanitize(&s.info.plugin, 60));
            let st = if focused { t.bold(t.accent) } else { t.dim() };
            g.put_str(o.x, o.y, &title, st, o.w.saturating_sub(hw));
            if o.w > hw + 4 {
                g.put_str(o.x + o.w - hw, o.y, hint, t.dim(), hw);
            }
        }
        SurfaceKind::Popup => {
            let b = t.border(true);
            let (x1, y1) = (o.x + o.w - 1, o.y + o.h - 1);
            for x in o.x..=x1 {
                g.put_str(x, o.y, "─", b, 1);
                g.put_str(x, y1, "─", b, 1);
            }
            for y in o.y..=y1 {
                g.put_str(o.x, y, "│", b, 1);
                g.put_str(x1, y, "│", b, 1);
            }
            g.put_str(o.x, o.y, "╭", b, 1);
            g.put_str(x1, o.y, "╮", b, 1);
            g.put_str(o.x, y1, "╰", b, 1);
            g.put_str(x1, y1, "╯", b, 1);
            let title = format!(
                " {} · {}{state} ",
                crate::draw::truncate(&s.title, o.w.saturating_sub(10) as usize / 2),
                sanitize(&s.info.plugin, 40)
            );
            g.put_str(
                o.x + 2,
                o.y,
                &title,
                t.bold(t.accent),
                o.w.saturating_sub(4),
            );
            let hint = if s.exited {
                " esc closes "
            } else {
                " prefix+x closes "
            };
            let hw = hint.chars().count() as u16;
            if o.w > hw + 4 {
                g.put_str(x1 - hw - 1, y1, hint, t.dim(), hw);
            }
        }
    }
}

/// Once per frame: dismiss surfaces whose command exited, keep the focus on an open popup,
/// forget finished dismissals, and report the copy-mode scroll position.
pub fn observe(app: &mut App) {
    let cur = app.cur;
    // Dismissed surfaces that are gone from the model.
    let live: HashSet<(usize, String)> = app
        .machines
        .iter()
        .enumerate()
        .flat_map(|(mi, m)| m.model.panes.iter().map(move |p| (mi, p.id.clone())))
        .collect();
    app.plugins.closing.retain(|k| live.contains(k));
    let exited: Vec<String> = app
        .m()
        .model
        .panes
        .iter()
        .filter(|p| p.exited && p.plugin_surface().is_some())
        .map(|p| p.id.clone())
        .collect();
    for p in exited {
        close(app, cur, &p);
    }
    if let Some(p) = session_popup(app)
        && app.focused_pane().as_deref() != Some(p.as_str())
        && matches!(app.mode, Mode::Normal | Mode::Prefix(_))
        && app
            .plugins
            .refocus_at
            .is_none_or(|t| t.elapsed() > REFOCUS_EVERY)
    {
        app.plugins.refocus_at = Some(Instant::now());
        app.focus_pane(cur, &p);
    }
    report_scroll(app, Instant::now());
}

// ---- scroll reports --------------------------------------------------------------------------

/// The focused machine's copy-mode viewport: (pane, rows above the live screen, rows known).
fn scroll_view(app: &App) -> Option<(String, u32, u32)> {
    match &app.mode {
        Mode::Copy(cm) => {
            let (off, total) = cm.scroll_offset();
            Some((cm.pane.clone(), off, total))
        }
        _ => None,
    }
}

/// Send `ScrollView` when the viewport moved (coalesced; the tick flushes the last change).
pub fn report_scroll(app: &mut App, now: Instant) {
    let Some((pane, offset, total)) = scroll_unsent(app) else {
        return;
    };
    let mi = app.cur;
    if app
        .plugins
        .scroll_at
        .is_some_and(|t| now.duration_since(t) < SCROLL_MIN_INTERVAL)
    {
        return;
    }
    app.plugins.scroll_at = Some(now);
    app.plugins.scroll_sent.insert(mi, (pane.clone(), offset));
    app.machines[mi].send(ClientFrame::ScrollView {
        pane,
        offset,
        total,
    });
}

/// A copy-mode position change in a frame-driven state where a frame may not follow: the
/// coalesced `ScrollView` still waiting out its interval, and the throttled pull of focus back
/// to an open plugin popup (both normally flushed by the next frame/draw).
pub(crate) fn deadlines(app: &App, now: Instant, d: &mut crate::deadline::Deadlines) {
    if scroll_unsent(app).is_some() {
        d.at(
            "plugins.scroll",
            app.plugins
                .scroll_at
                .map_or(now, |t| t + SCROLL_MIN_INTERVAL),
        );
    }
    if let Some(t) = app.plugins.refocus_at
        && matches!(app.mode, Mode::Normal | Mode::Prefix(_))
        && let Some(p) = session_popup(app)
        && app.focused_pane().as_deref() != Some(p.as_str())
        && t + REFOCUS_EVERY > now
    {
        d.redraw("plugins.refocus", t + REFOCUS_EVERY);
    }
}

#[cfg(test)]
pub(crate) fn test_pending_scroll(app: &mut App, at: Instant) {
    app.plugins
        .scroll_sent
        .insert(app.cur, ("p1".to_string(), 5));
    app.plugins.scroll_at = Some(at);
}

/// The scroll position to report for the focused machine, when it differs from the last sent.
fn scroll_unsent(app: &App) -> Option<(String, u32, u32)> {
    let mi = app.cur;
    if !app.machines[mi]
        .features
        .iter()
        .any(|f| f == SCROLL_FEATURE)
    {
        return None;
    }
    let want = match scroll_view(app) {
        Some(v) => Some(v),
        // Left copy mode: back at the bottom.
        None => app
            .plugins
            .scroll_sent
            .get(&mi)
            .filter(|(_, off)| *off != 0)
            .map(|(p, _)| (p.clone(), 0, 0)),
    };
    let (pane, offset, total) = want?;
    if app.plugins.scroll_sent.get(&mi) == Some(&(pane.clone(), offset)) {
        return None;
    }
    Some((pane, offset, total))
}

// ---- palette and key bindings ----------------------------------------------------------------

/// The fix for an unavailable plugin, shown on its disabled palette entries.
pub fn unavailable_hint(plugin: &str, status: &str) -> String {
    match status {
        "disabled" => format!("disabled — enable with vibeke plugin enable {plugin}"),
        "untrusted" | "stale_trust" => {
            format!("untrusted — trust with vibeke plugin trust {plugin} --legacy")
        }
        "" => "unavailable".into(),
        other => format!("{other} — see vibeke plugin list"),
    }
}

/// Palette entries for plugin actions: `(id, description, disabled)`. Ids are
/// `plugin:<machine>:<plugin>.<action>`.
pub fn palette_entries(app: &App) -> Vec<(String, String, bool)> {
    let multi = app.machines.len() > 1;
    let ctx_of = |mi: usize| action_context(app, mi);
    let mut mis: Vec<&usize> = app.plugins.per.keys().collect();
    mis.sort();
    let mut out = Vec::new();
    for mi in mis {
        let on = if multi {
            format!(" [{}]", app.machines[*mi].label)
        } else {
            String::new()
        };
        let ctx = ctx_of(*mi);
        for a in &app.plugins.per[mi].actions {
            // Only actions that apply to what is focused are offered.
            if !contexts_apply(&a.contexts, ctx) {
                continue;
            }
            let mut desc = format!("Plugin: {} ({}){on}", a.title, a.plugin);
            if !a.available {
                desc.push_str(&format!(" — {}", unavailable_hint(&a.plugin, &a.status)));
            }
            out.push((format!("plugin:{mi}:{}", a.qualified), desc, !a.available));
        }
    }
    out
}

/// What is focused on machine `mi`, for action contexts. A selection exists only in copy mode
/// (so a palette, which leaves copy mode, never offers `selection` actions).
pub fn action_context(app: &App, mi: usize) -> ActionContext {
    if mi == app.cur {
        ActionContext {
            workspace: app.focused_ws().is_some(),
            tab: app.focused_tab().is_some(),
            pane: app.focused_pane().is_some(),
            selection: matches!(&app.mode, Mode::Copy(cm) if cm.selection_text().is_some()),
        }
    } else {
        let pane = app.machines[mi].focus.pane.is_some();
        ActionContext {
            workspace: pane,
            tab: pane,
            pane,
            selection: false,
        }
    }
}

/// Binding of a plugin action: the user's `[[keys.command]] type = "plugin_action"` first, then
/// a manifest default the server installed.
pub fn binding_for(app: &App, qualified: &str) -> Option<String> {
    let manifest_default = || {
        app.plugins
            .per
            .values()
            .flat_map(|p| p.keys.iter())
            .find(|k| k.action == qualified)
            .map(|k| k.key.clone())
    };
    app.config
        .keys
        .command
        .iter()
        .find(|c| c.kind == vk_config::CommandType::PluginAction && c.command == qualified)
        .map(|c| c.key.clone())
        .filter(|k| !k.is_empty())
        .or_else(manifest_default)
}

/// Run palette entry `<machine>:<plugin>.<action>`.
fn run_palette(app: &mut App, rest: &str) {
    let Some((mi, q)) = rest.split_once(':') else {
        return;
    };
    let Ok(mi) = mi.parse::<usize>() else { return };
    if mi >= app.machines.len() {
        return;
    }
    let a = app
        .plugins
        .per(mi)
        .and_then(|p| p.actions.iter().find(|a| a.qualified == q))
        .cloned();
    match a {
        Some(a) if !a.available => app.toast(unavailable_hint(&a.plugin, &a.status)),
        Some(a) if !contexts_apply(&a.contexts, action_context(app, mi)) => app.toast(format!(
            "{}: not available here (needs {})",
            a.title,
            a.contexts.join(" or ")
        )),
        Some(a) => run_action(app, mi, &a.plugin, Some(&a.action), &a.title, "palette"),
        None => app.toast(format!("{q}: no such plugin action")),
    }
}

/// `plugin.action.run` for the focused context on machine `mi`. Without `action`, `plugin` is
/// the qualified `<plugin>.<action>` (the server resolves it).
pub fn run_action(
    app: &mut App,
    mi: usize,
    plugin: &str,
    action: Option<&str>,
    title: &str,
    source: &str,
) {
    let pane = if mi == app.cur {
        app.focused_pane()
    } else {
        app.machines[mi].focus.pane.clone()
    };
    let params = match action {
        Some(a) => json!({"plugin": plugin, "action": a, "pane": pane, "source": source}),
        None => json!({"action": plugin, "pane": pane, "source": source}),
    };
    app.command_on(
        mi,
        "plugin.action.run",
        params,
        Pending::Plugin(Reply::Ran(title.to_string())),
    );
}

/// A `[[keys.command]] type = "plugin_action"` binding fired.
pub fn run_key_command(app: &mut App, c: &vk_config::KeyCommand) {
    let cur = app.cur;
    // A binding fired where the action does not apply: say so instead of running it.
    if let Some(a) = app.plugins.per(cur).and_then(|p| {
        p.actions
            .iter()
            .find(|a| a.qualified == c.command || a.action == c.command)
            .cloned()
    }) && !contexts_apply(&a.contexts, action_context(app, cur))
    {
        app.toast(format!(
            "{}: not available here (needs {})",
            a.title,
            a.contexts.join(" or ")
        ));
        return;
    }
    let title = c
        .description
        .clone()
        .or_else(|| c.title.clone())
        .unwrap_or_else(|| c.command.clone());
    let cur = app.cur;
    run_action(app, cur, &c.command, None, &title, "keybinding");
}

// ---- link handlers ---------------------------------------------------------------------------

/// The chooser offered when an activated link matches plugin link handlers.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkChoice {
    pub machine: usize,
    pub pane: String,
    pub url: String,
    /// Matching handlers in matching order.
    pub handlers: Vec<LinkHandler>,
    /// What the last row does: open (URLs) or copy.
    pub default_open: bool,
    pub sel: usize,
}

/// Handlers of machine `mi` whose pattern matches `text`, in matching order.
pub fn matching(app: &App, mi: usize, text: &str) -> Vec<LinkHandler> {
    let Some(per) = app.plugins.per(mi) else {
        return vec![];
    };
    per.handlers
        .iter()
        .filter(|h| regex::Regex::new(&h.pattern).is_ok_and(|re| re.is_match(text)))
        .cloned()
        .collect()
}

/// Offer plugin link handlers for an activated link. False when none matches (the caller
/// does its default).
pub fn offer_link(app: &mut App, mi: usize, pane: &str, text: &str, default_open: bool) -> bool {
    let handlers = matching(app, mi, text);
    if handlers.is_empty() {
        return false;
    }
    app.mode = Mode::Popup(Popup::PluginLink(Box::new(LinkChoice {
        machine: mi,
        pane: pane.to_string(),
        url: text.to_string(),
        handlers,
        default_open,
        sel: 0,
    })));
    true
}

/// Ctrl/Alt+click at a pane-local cell: offer handlers for the token under it. True when
/// handled.
pub fn click(app: &mut App, mi: usize, pane: &str, col: u16, row: u16) -> bool {
    let Some(tok) = token_at(app, mi, pane, col, row) else {
        return false;
    };
    let open = tok.starts_with("http://") || tok.starts_with("https://");
    offer_link(app, mi, pane, &tok, open)
}

/// The whitespace-delimited token under a pane-local cell, without surrounding quotes or
/// brackets and trailing punctuation.
pub fn token_at(app: &App, mi: usize, pane: &str, col: u16, row: u16) -> Option<String> {
    let buf = app.machines[mi].panes.get(pane)?;
    let line = buf.lines.get(row as usize)?;
    let mut cells: Vec<(u16, char)> = Vec::new();
    let mut c = 0u16;
    for span in &line.spans {
        for ch in span.text.chars() {
            cells.push((c, ch));
            c += unicode_width::UnicodeWidthChar::width(ch)
                .unwrap_or(1)
                .max(1) as u16;
        }
    }
    let i = cells.iter().rposition(|(x, _)| *x <= col)?;
    if cells[i].1.is_whitespace() {
        return None;
    }
    let mut a = i;
    while a > 0 && !cells[a - 1].1.is_whitespace() {
        a -= 1;
    }
    let mut b = i;
    while b + 1 < cells.len() && !cells[b + 1].1.is_whitespace() {
        b += 1;
    }
    let tok: String = cells[a..=b].iter().map(|(_, ch)| *ch).collect();
    let tok = tok
        .trim_start_matches(['"', '\'', '`', '<', '(', '['])
        .trim_end_matches(['"', '\'', '`', '>', ')', ']', '.', ',', ';', ':'])
        .to_string();
    (!tok.is_empty()).then_some(tok)
}

pub fn link_key(app: &mut App, ev: KeyEvent, mut c: Box<LinkChoice>) {
    if ev.kind == KeyKind::Release {
        app.mode = Mode::Popup(Popup::PluginLink(c));
        return;
    }
    let n = c.handlers.len() + 1;
    let pick = match ev.key {
        Key::Named(NamedKey::Escape) => return,
        Key::Named(NamedKey::Down | NamedKey::Tab) | Key::Char('j') => {
            c.sel = (c.sel + 1) % n;
            None
        }
        Key::Char('n') if ev.mods.ctrl() => {
            c.sel = (c.sel + 1) % n;
            None
        }
        Key::Char('p') if ev.mods.ctrl() => {
            c.sel = (c.sel + n - 1) % n;
            None
        }
        Key::Named(NamedKey::Up) | Key::Char('k') => {
            c.sel = (c.sel + n - 1) % n;
            None
        }
        Key::Named(NamedKey::Enter) => Some(c.sel),
        Key::Char(d @ '1'..='9') => {
            let i = d as usize - '1' as usize;
            (i < n).then_some(i)
        }
        _ => None,
    };
    let Some(i) = pick else {
        app.mode = Mode::Popup(Popup::PluginLink(c));
        return;
    };
    match c.handlers.get(i) {
        Some(h) if !h.available => app.toast(unavailable_hint(&h.plugin, &h.status)),
        Some(h) => app.command_on(
            c.machine,
            "plugin.link.open",
            json!({"plugin": h.plugin, "handler": h.id, "url": c.url, "pane": c.pane}),
            Pending::Plugin(Reply::Ran(h.title.clone())),
        ),
        None if c.default_open => crate::nav::open_url(app, c.machine, &c.pane, &c.url),
        None => app.set_clipboard(c.url.as_bytes(), false),
    }
}

pub fn draw_link(app: &App, g: &mut Grid, c: &LinkChoice) {
    let t = app.theme;
    let h = (c.handlers.len() as u16 + 5).min(20);
    let mut b = crate::popups::frame(app, g, 96, h, "open with · enter · esc");
    b.line(&crate::draw::truncate(&c.url, 90), t.dim());
    for (i, hd) in c.handlers.iter().enumerate() {
        let mut s = format!("{} {} ({})", i + 1, hd.title, hd.plugin);
        if !hd.available {
            s.push_str(&format!(" — {}", unavailable_hint(&hd.plugin, &hd.status)));
        }
        let st = match (i == c.sel, hd.available) {
            (true, _) => t.sel(t.fg),
            (false, true) => t.text(),
            (false, false) => t.dim(),
        };
        b.line(&s, st);
    }
    let def = format!(
        "{} {}",
        c.handlers.len() + 1,
        if c.default_open {
            "Open (default)"
        } else {
            "Copy (default)"
        }
    );
    let st = if c.sel == c.handlers.len() {
        t.sel(t.fg)
    } else {
        t.text()
    };
    b.line(&def, st);
}

#[cfg(test)]
#[path = "plugins_tests.rs"]
mod tests;
