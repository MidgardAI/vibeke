//! Devices in the TUI: a full pane-area view (`devices` in the palette, `prefix+alt+d`) of the
//! phones and apps paired with one machine, reached through that machine's server with
//! `gateway.call` (like [`crate::sharing`]). Three stages:
//!
//! 1. **List** (`devices.list`, only `kind == "device"`): name, platform, scope, how long ago it
//!    paired, 🔔 when it takes push. `n` pairs a new one, `x` revokes the selected one after a
//!    confirm (`devices.revoke`), `r` reads the list again.
//! 2. **Pick scope**: full (default), approve or view. Enter creates the link (`pair.create`).
//!    `pair_phone` / `phone_pairing` open the view here.
//! 3. **Pairing**: the link with a QR code, the scope and a countdown to when the link stops
//!    working. `pair.status` is polled about once a second (one request in flight): the phone
//!    claims the link (its fingerprint shows here and in the confirm prompt), then the pairing
//!    is done ("Paired ✓") and the list is read again. Esc cancels a pending pairing
//!    (`share.revoke {id: pid}`, best effort).
//!
//! Without a gateway (or without a relay/app URL) the server answers with a message that the view
//! shows as it is.

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};
use vk_proto::render::Style;

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::handoff::epoch_ms;
use crate::inbox::fmt_age;
use crate::screen::Grid;
use crate::sharing::{bridge_error, draw_link_qr, gw_call_pending, unreachable};

/// How often `pair.status` is asked while a pairing link is open.
pub const POLL: Duration = Duration::from_secs(1);

/// (scope, name, what it allows), the default first.
const SCOPES: [(&str, &str, &str); 3] = [
    ("full", "Full", "everything, including typing into agents"),
    (
        "approve",
        "Approve",
        "watch your agents and approve or deny their requests",
    ),
    ("view", "View", "read-only"),
];

fn now_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---- state ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceRow {
    pub id: String,
    pub name: String,
    pub platform: String,
    /// "view", "approve" or "full".
    pub scope: String,
    /// Unix seconds.
    pub paired_at: i64,
    pub fingerprint: String,
    pub push: bool,
}

fn s_of(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl DeviceRow {
    /// A `devices.list` entry; None for shares and peers (those live in Sharing & handoff).
    fn from_value(v: &Value) -> Option<DeviceRow> {
        if v.get("kind").and_then(Value::as_str).unwrap_or("device") != "device" {
            return None;
        }
        Some(DeviceRow {
            id: s_of(v, "id")?,
            name: s_of(v, "name").unwrap_or_default(),
            platform: s_of(v, "platform").unwrap_or_default(),
            scope: s_of(v, "scope").unwrap_or_default().to_lowercase(),
            paired_at: v["paired_at"].as_i64().unwrap_or(0),
            fingerprint: s_of(v, "fingerprint").unwrap_or_default(),
            push: v["push"].as_bool().unwrap_or(false),
        })
    }

    fn label(&self) -> String {
        let n = if self.name.is_empty() {
            &self.id
        } else {
            &self.name
        };
        crate::plugins::sanitize(n, 40)
    }
}

/// `x` asks first.
#[derive(Debug, Clone, PartialEq)]
pub struct Confirm {
    pub id: String,
    pub label: String,
}

/// Where a pairing link stands (`pair.status`).
#[derive(Debug, Clone, PartialEq)]
pub enum PairStatus {
    /// Nobody has opened the link.
    Pending,
    /// A phone opened it and is asking to pair: confirm in the prompt.
    Claimed {
        name: String,
        platform: String,
        fingerprint: String,
    },
    /// Declined; the link stays valid until it expires.
    Rejected,
    /// Gone or past `open_by`: polling stopped.
    Expired,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pairing {
    pub pid: String,
    pub link: String,
    /// Unix seconds: the link must be opened before this.
    pub open_by: i64,
    pub status: PairStatus,
    pub scope: String,
    polled_at: Option<Instant>,
    inflight: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    List,
    PickScope { sel: usize },
    Pairing(Pairing),
}

#[derive(Debug, Clone)]
pub struct View {
    pub mi: usize,
    pub rows: Vec<DeviceRow>,
    pub sel: usize,
    pub loading: bool,
    /// The gateway can't be reached (or the server has no bridge): shown at the top.
    pub error: Option<String>,
    /// The last action's outcome.
    pub notice: Option<String>,
    /// A change in flight.
    pub busy: Option<String>,
    pub confirm: Option<Confirm>,
    pub stage: Stage,
}

impl View {
    fn new(mi: usize) -> View {
        View {
            mi,
            rows: Vec::new(),
            sel: 0,
            loading: false,
            error: None,
            notice: None,
            busy: None,
            confirm: None,
            stage: Stage::List,
        }
    }

    fn clamp(&mut self) {
        self.sel = self.sel.min(self.rows.len().saturating_sub(1));
    }
}

/// Replies routed back here (through `ux::Reply::Devices`).
#[derive(Debug, Clone)]
pub enum Reply {
    List,
    Revoke {
        name: String,
    },
    Create,
    Status {
        pid: String,
    },
    /// `share.revoke` of an abandoned link: nothing to show.
    Cancel,
}

fn gw(app: &mut App, mi: usize, method: &str, params: Value, r: Reply) {
    gw_call_pending(
        app,
        mi,
        method,
        params,
        Pending::Ux(crate::ux::Reply::Devices(r)),
    );
}

fn view_mut(app: &mut App) -> Option<&mut View> {
    app.ux.devices.as_mut()
}

// ---- open / refresh ------------------------------------------------------------------------------

fn open_on(app: &mut App, mi: usize, stage: Stage) {
    if !app.machines.get(mi).is_some_and(|m| m.connected()) {
        app.toast("that machine is offline");
        return;
    }
    let mut v = View::new(mi);
    v.stage = stage;
    app.ux.devices = Some(v);
    app.mode = Mode::Popup(Popup::Devices);
    refresh(app);
}

fn refresh(app: &mut App) {
    let Some(v) = view_mut(app) else {
        return;
    };
    v.loading = true;
    v.error = None;
    let mi = v.mi;
    gw(app, mi, "devices.list", json!({}), Reply::List);
}

pub fn action(app: &mut App, action: &str) -> bool {
    let mi = app.cur;
    match action {
        "devices" => open_on(app, mi, Stage::List),
        "pair_phone" | "phone_pairing" => open_on(app, mi, Stage::PickScope { sel: 0 }),
        _ => return false,
    }
    true
}

fn close(app: &mut App) {
    app.ux.devices = None;
    app.mode = Mode::Normal;
}

// ---- polling -------------------------------------------------------------------------------------

pub fn tick(app: &mut App) {
    if !matches!(app.mode, Mode::Popup(Popup::Devices)) {
        return;
    }
    let now = Instant::now();
    let secs = now_s();
    let Some(v) = view_mut(app) else {
        return;
    };
    let mi = v.mi;
    let Stage::Pairing(p) = &mut v.stage else {
        return;
    };
    if matches!(p.status, PairStatus::Pending | PairStatus::Rejected) && secs > p.open_by {
        p.status = PairStatus::Expired;
        app.dirty = true;
        return;
    }
    if p.status == PairStatus::Expired
        || p.inflight
        || p.polled_at.is_some_and(|t| now.duration_since(t) < POLL)
    {
        return;
    }
    p.polled_at = Some(now);
    p.inflight = true;
    let pid = p.pid.clone();
    gw(
        app,
        mi,
        "pair.status",
        json!({"pid": pid}),
        Reply::Status { pid },
    );
}

pub fn deadlines(app: &App, now: Instant, d: &mut crate::deadline::Deadlines) {
    if let (Mode::Popup(Popup::Devices), Some(v)) = (&app.mode, &app.ux.devices)
        && let Stage::Pairing(p) = &v.stage
        && p.status != PairStatus::Expired
        && !p.inflight
    {
        d.at("devices.poll", p.polled_at.map_or(now, |t| t + POLL));
    }
}

// ---- actions -------------------------------------------------------------------------------------

/// `x` on the selected row: ask first.
fn ask_revoke(v: &mut View) {
    match v.rows.get(v.sel) {
        Some(d) => {
            v.confirm = Some(Confirm {
                id: d.id.clone(),
                label: d.label(),
            })
        }
        None => v.notice = Some("nothing selected".into()),
    }
}

fn revoke(app: &mut App, c: Confirm) {
    let Some(v) = view_mut(app) else {
        return;
    };
    v.busy = Some(format!("revoking {}…", c.label));
    let mi = v.mi;
    gw(
        app,
        mi,
        "devices.revoke",
        json!({"device": c.id}),
        Reply::Revoke { name: c.label },
    );
}

fn create(app: &mut App, scope: &str) {
    let Some(v) = view_mut(app) else {
        return;
    };
    v.busy = Some("creating a pairing link…".into());
    v.notice = None;
    let mi = v.mi;
    gw(
        app,
        mi,
        "pair.create",
        json!({"scope": scope}),
        Reply::Create,
    );
}

/// Best effort: the link stops working; the answer is ignored.
fn cancel_link(app: &mut App, mi: usize, pid: &str) {
    gw(app, mi, "share.revoke", json!({"id": pid}), Reply::Cancel);
}

// ---- keys ----------------------------------------------------------------------------------------

pub fn key(app: &mut App, ev: KeyEvent) {
    app.mode = Mode::Popup(Popup::Devices);
    if ev.kind == KeyKind::Release {
        return;
    }
    app.dirty = true;
    let Some(v) = view_mut(app) else {
        app.mode = Mode::Normal;
        return;
    };
    let esc = matches!(ev.key, Key::Named(NamedKey::Escape));
    let plain = !ev.mods.ctrl() && !ev.mods.alt();
    let quit = esc || (plain && matches!(ev.key, Key::Char('q')));
    let up = matches!(ev.key, Key::Char('k') | Key::Named(NamedKey::Up));
    let down = matches!(ev.key, Key::Char('j') | Key::Named(NamedKey::Down));
    let enter = matches!(ev.key, Key::Named(NamedKey::Enter));
    match &mut v.stage {
        Stage::List => {
            if let Some(c) = v.confirm.take() {
                if plain && matches!(ev.key, Key::Char('y' | 'x') | Key::Named(NamedKey::Enter)) {
                    revoke(app, c);
                }
                return;
            }
            v.notice = None;
            let n = v.rows.len();
            match ev.key {
                _ if quit => close(app),
                _ if down => v.sel = (v.sel + 1).min(n.saturating_sub(1)),
                _ if up => v.sel = v.sel.saturating_sub(1),
                Key::Char('x') if plain => ask_revoke(v),
                Key::Char('n') if plain => v.stage = Stage::PickScope { sel: 0 },
                Key::Char('r' | 'g') if plain => refresh(app),
                _ => {}
            }
        }
        Stage::PickScope { sel } => {
            if quit {
                v.busy = None;
                v.stage = Stage::List;
            } else if v.busy.is_some() {
            } else if down {
                *sel = (*sel + 1).min(SCOPES.len() - 1);
            } else if up {
                *sel = sel.saturating_sub(1);
            } else if enter {
                let scope = SCOPES[*sel].0;
                create(app, scope);
            }
        }
        Stage::Pairing(p) => {
            if quit {
                let (pid, live) = (p.pid.clone(), p.status != PairStatus::Expired);
                let mi = v.mi;
                v.stage = Stage::List;
                if live {
                    cancel_link(app, mi, &pid);
                }
            } else if plain && matches!(ev.key, Key::Char('c')) {
                let link = p.link.clone();
                app.copy_text(&link);
            } else if plain && matches!(ev.key, Key::Char('n')) && p.status == PairStatus::Expired {
                v.stage = Stage::PickScope { sel: 0 };
            }
        }
    }
}

// ---- replies -------------------------------------------------------------------------------------

/// A failed call: an unreachable gateway is the view's banner, anything else its notice.
fn failed(v: &mut View, e: &RpcErr) {
    let msg = bridge_error(e);
    if unreachable(e) {
        v.error = Some(msg);
    } else {
        v.notice = Some(format!("✗ {msg}"));
    }
}

pub fn on_reply(app: &mut App, mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    app.dirty = true;
    let Some(v) = view_mut(app).filter(|v| v.mi == mi) else {
        return;
    };
    match r {
        Reply::List => {
            v.loading = false;
            match res {
                Ok(x) => {
                    v.rows = x["devices"]
                        .as_array()
                        .map(|a| a.iter().filter_map(DeviceRow::from_value).collect())
                        .unwrap_or_default();
                    v.error = None;
                }
                Err(e) => failed(v, &e),
            }
            v.clamp();
        }
        Reply::Revoke { name } => {
            v.busy = None;
            match res {
                Ok(_) => {
                    v.notice = Some(format!("revoked {name}: it can no longer connect"));
                    refresh(app);
                }
                Err(e) => failed(v, &e),
            }
        }
        Reply::Create => {
            let waiting = matches!(v.stage, Stage::PickScope { .. }) && v.busy.is_some();
            v.busy = None;
            match res {
                Ok(x) => {
                    let pid = s_of(&x, "pid").unwrap_or_default();
                    match s_of(&x, "link") {
                        Some(link) if waiting => {
                            v.notice = None;
                            v.stage = Stage::Pairing(Pairing {
                                pid,
                                link,
                                open_by: x["open_by"].as_i64().unwrap_or(0),
                                status: PairStatus::Pending,
                                scope: s_of(&x, "scope").unwrap_or_default().to_lowercase(),
                                polled_at: None,
                                inflight: false,
                            });
                        }
                        // Backed out while it was being made: the link is not wanted.
                        Some(_) if !pid.is_empty() => cancel_link(app, mi, &pid),
                        Some(_) => {}
                        None => v.notice = Some("✗ the gateway returned no link".into()),
                    }
                }
                Err(e) => failed(v, &e),
            }
        }
        Reply::Status { pid } => {
            let Stage::Pairing(p) = &mut v.stage else {
                return;
            };
            if p.pid != pid {
                return;
            }
            p.inflight = false;
            match res {
                Ok(x) => match x["status"].as_str() {
                    Some("pending") => p.status = PairStatus::Pending,
                    Some("claimed") => {
                        p.status = PairStatus::Claimed {
                            name: s_of(&x, "name").unwrap_or_default(),
                            platform: s_of(&x, "platform").unwrap_or_default(),
                            fingerprint: s_of(&x, "fingerprint").unwrap_or_default(),
                        }
                    }
                    Some("rejected") => p.status = PairStatus::Rejected,
                    Some("gone") => p.status = PairStatus::Expired,
                    Some("done") => {
                        let name = s_of(&x, "name")
                            .or_else(|| match &p.status {
                                PairStatus::Claimed { name, .. } if !name.is_empty() => {
                                    Some(name.clone())
                                }
                                _ => None,
                            })
                            .unwrap_or_else(|| "your phone".into());
                        v.stage = Stage::List;
                        v.notice =
                            Some(format!("Paired ✓ {}", crate::plugins::sanitize(&name, 40)));
                        refresh(app);
                    }
                    _ => {}
                },
                // Keep asking: the next poll may get through.
                Err(e) => failed(v, &e),
            }
        }
        Reply::Cancel => {}
    }
}

// ---- drawing -------------------------------------------------------------------------------------

/// "3d ago", "just now".
fn ago(t: i64, now_ms: i64) -> String {
    let ms = now_ms - epoch_ms(t);
    if t <= 0 {
        "unknown".into()
    } else if ms < 60_000 {
        "just now".into()
    } else {
        format!("{} ago", fmt_age(ms))
    }
}

fn row_line(d: &DeviceRow, now_ms: i64) -> String {
    let mut parts = vec![d.label()];
    if !d.platform.is_empty() {
        parts.push(crate::plugins::sanitize(&d.platform, 16));
    }
    if !d.scope.is_empty() {
        parts.push(d.scope.clone());
    }
    parts.push(format!("paired {}", ago(d.paired_at, now_ms)));
    let mut s = parts.join(" · ");
    if d.push {
        s.push_str(" · 🔔");
    }
    s
}

/// "4m 12s".
fn countdown(secs: i64) -> String {
    let s = secs.max(0);
    if s >= 60 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

pub fn draw(app: &App, g: &mut Grid) {
    let Some(v) = app.ux.devices.as_ref() else {
        return;
    };
    let t = app.theme;
    let label = app.machines.get(v.mi).map_or("", |m| m.label.as_str());
    let mut a = crate::drafts::Area::open(app, g, &format!("Vibeke · Devices · {label}"));
    if let Some(e) = &v.error {
        a.line(&format!("⚠ {e}"), t.bold(t.yellow));
        a.line("", t.text());
    }
    match &v.stage {
        Stage::List => draw_list(app, &mut a, v),
        Stage::PickScope { sel } => draw_pick(app, &mut a, v, *sel),
        Stage::Pairing(p) => draw_pairing(app, &mut a, v, p),
    }
}

/// The busy line, else the notice (red when it is a failure).
fn status_line(app: &App, a: &mut crate::drafts::Area<'_>, v: &View) {
    let t = app.theme;
    if let Some(b) = &v.busy {
        a.line(&format!("⏳ {b}"), t.s(t.yellow));
    } else if let Some(n) = &v.notice {
        a.line(
            n,
            t.s(if n.starts_with('✗') {
                t.red
            } else {
                t.yellow
            }),
        );
    }
}

fn draw_list(app: &App, a: &mut crate::drafts::Area<'_>, v: &View) {
    let t = app.theme;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    a.line("Your paired phones and apps", t.bold(t.fg));
    a.line("", t.text());
    if v.rows.is_empty() {
        a.line(
            if v.loading {
                "  loading…"
            } else {
                "  No phones paired yet — press n to pair one"
            },
            t.dim(),
        );
    }
    for (i, d) in v.rows.iter().enumerate() {
        let on = i == v.sel;
        a.line(
            &format!("{} {}", if on { "›" } else { " " }, row_line(d, now_ms)),
            if on { t.sel(t.fg) } else { t.text() },
        );
    }
    a.line("", t.text());
    if let Some(c) = &v.confirm {
        a.line(
            &format!(
                "Revoke {}? It can no longer connect to this machine. [y] revoke  [n] keep",
                c.label
            ),
            t.bold(t.red),
        );
    } else {
        status_line(app, a, v);
    }
    a.footer(
        "j/k move · n pair a phone · x revoke · r refresh · esc",
        t.dim(),
    );
}

fn draw_pick(app: &App, a: &mut crate::drafts::Area<'_>, v: &View, sel: usize) {
    let t = app.theme;
    a.line("Pair a phone: what may it do?", t.bold(t.fg));
    a.line("", t.text());
    for (i, (_, name, what)) in SCOPES.iter().enumerate() {
        let on = i == sel;
        a.line(
            &format!("{} {:<8} {}", if on { "›" } else { " " }, name, what),
            if on { t.sel(t.fg) } else { t.text() },
        );
    }
    a.line("", t.text());
    status_line(app, a, v);
    a.footer("j/k move · enter create the link · esc back", t.dim());
}

/// `s` broken into lines of the area's width.
fn wrap_lines(a: &mut crate::drafts::Area<'_>, s: &str, st: Style) {
    let w = a.rest().w as usize;
    let chars: Vec<char> = s.chars().collect();
    for chunk in chars.chunks(w.max(20)) {
        a.line(&chunk.iter().collect::<String>(), st);
    }
}

fn draw_pairing(app: &App, a: &mut crate::drafts::Area<'_>, v: &View, p: &Pairing) {
    let t = app.theme;
    let left = p.open_by - now_s();
    a.line(&format!("Pair a phone · {} access", p.scope), t.bold(t.fg));
    a.line(
        "Scan with your phone's camera; check the fingerprint matches when asked",
        t.text(),
    );
    match &p.status {
        PairStatus::Pending => a.line(
            &format!(
                "Waiting for the phone… the link works for {}",
                countdown(left)
            ),
            t.s(t.yellow),
        ),
        PairStatus::Claimed {
            name,
            platform,
            fingerprint,
        } => {
            let who = crate::plugins::sanitize(name, 40);
            let plat = if platform.is_empty() {
                String::new()
            } else {
                format!(" ({})", crate::plugins::sanitize(platform, 16))
            };
            wrap_lines(
                a,
                &format!(
                    "'{who}'{plat} is asking to pair — confirm in the prompt, fingerprint {}",
                    crate::plugins::sanitize(fingerprint, 80)
                ),
                t.bold(t.green),
            );
        }
        PairStatus::Rejected => a.line(
            "Rejected — the link stays valid until it expires",
            t.s(t.red),
        ),
        PairStatus::Expired => a.line("Link expired — press n for a new one", t.bold(t.red)),
    }
    if let Some(n) = &v.notice {
        a.line(n, t.s(t.red));
    }
    a.line("", t.text());
    if p.status != PairStatus::Expired {
        draw_link_qr(app, a, &p.link);
    }
    a.footer(
        if p.status == PairStatus::Expired {
            "n new link · esc back"
        } else {
            "c copy link · esc cancel"
        },
        t.dim(),
    );
}

#[cfg(test)]
#[path = "devices_tests.rs"]
mod tests;
