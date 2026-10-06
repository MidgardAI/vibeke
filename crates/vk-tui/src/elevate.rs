//! Elevation approval in the TUI chrome (09 §3.2; 08 §8).
//!
//! A program inside a pane asks for a time-boxed full-scope token with `vibeke auth elevate`
//! (`auth.elevate`). The server opens a request, emits `auth.elevate_requested` and waits for the
//! user. This module is the out-of-band half: it learns of requests from the pushed events (and
//! `auth.list` after every connect, for requests made before the subscription), shows a
//! non-modal notice in the tab bar's right cluster — never over a pane — and, only when the user
//! opens it (`elevation_requests`, default `prefix+shift+e`, or the palette), a review view that
//! **replaces the pane area**: the requesting pane's content is not on screen while the prompt
//! is, so nothing the pane draws can pose as the prompt or sit next to it pretending to be part
//! of it. The view names the pane, quotes its reason as unverified text, and shows how long the
//! request stays open and what approval grants (full API access for 10 minutes, from that pane
//! only). `y` approves and `n` denies through `auth.elevate.decide`; nothing is ever approved
//! automatically, keys typed in the first 600 ms after the view opens are ignored (they were
//! meant for the pane), and modified keys never decide. A client that is itself inside a pane
//! (not full scope) cannot decide; the view says so and points at the CLI outside Vibeke.

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::screen::Grid;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::time::{Duration, Instant};
use vk_proto::input::{Key, KeyEvent, KeyKind, Mods, NamedKey};

/// Keys right after the view opens are swallowed: they were typed for the pane.
pub const ARM_DELAY: Duration = Duration::from_millis(600);
/// The server forgets an undecided request after 30 minutes (`auth.rs` `prune`).
pub const REQUEST_TTL_MS: i64 = 30 * 60 * 1000;
/// What an approval grants.
pub const GRANT_MINUTES: i64 = 10;

/// One open request.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub machine: usize,
    pub id: String,
    pub pane: String,
    pub reason: String,
    pub created_at_ms: i64,
}

impl Request {
    pub fn expires_at_ms(&self) -> i64 {
        self.created_at_ms + REQUEST_TTL_MS
    }
}

#[derive(Debug, Clone)]
pub struct View {
    pub sel: usize,
    pub opened_at: Instant,
    pub notice: Option<String>,
}

#[derive(Debug, Default)]
pub struct State {
    pub requests: Vec<Request>,
    pub view: Option<View>,
    /// Decisions in flight: (machine, request).
    pub deciding: HashSet<(usize, String)>,
    /// Tests: pretend this client is (not) full scope.
    pub scoped_override: Option<bool>,
}

#[derive(Debug, Clone)]
pub enum Reply {
    List,
    Decide { id: String, approve: bool },
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Whether this client may decide on machine `mi`: a TUI started inside a Vibeke pane of that
/// (local) server holds that pane's scope, and the server refuses `auth.elevate.decide` from it.
pub fn can_decide(app: &App, mi: usize) -> bool {
    if let Some(scoped) = app.ux.elevate.scoped_override {
        return !scoped;
    }
    !(app.machines[mi].local
        && (std::env::var_os("VIBEKE_PANE_TOKEN").is_some()
            || std::env::var_os("VIBEKE_ELEVATED_TOKEN").is_some()))
}

/// After (re)connecting: requests opened before the event subscription.
pub fn on_connected(app: &mut App, mi: usize) {
    app.command_on(
        mi,
        "auth.list",
        json!({}),
        Pending::Ux(crate::ux::Reply::Elevate(Reply::List)),
    );
}

fn add(app: &mut App, r: Request) {
    let s = &mut app.ux.elevate;
    if s.requests
        .iter()
        .any(|x| x.machine == r.machine && x.id == r.id)
    {
        return;
    }
    s.requests.push(r);
    s.requests.sort_by_key(|r| r.created_at_ms);
    app.dirty = true;
}

fn remove(app: &mut App, mi: usize, id: &str) {
    app.ux
        .elevate
        .requests
        .retain(|r| !(r.machine == mi && r.id == id));
    app.ux.elevate.deciding.remove(&(mi, id.to_string()));
    clamp(app);
    app.dirty = true;
}

fn clamp(app: &mut App) {
    let n = app.ux.elevate.requests.len();
    if let Some(v) = app.ux.elevate.view.as_mut() {
        v.sel = v.sel.min(n.saturating_sub(1));
    }
}

/// Pushed `auth.elevate_*` events.
pub fn on_event(app: &mut App, mi: usize, kind: &str, v: &Value) {
    let Some(id) = v["subject"]["request"].as_str() else {
        return;
    };
    match kind {
        "auth.elevate_requested" => {
            let r = Request {
                machine: mi,
                id: id.to_string(),
                pane: v["subject"]["pane"].as_str().unwrap_or("").to_string(),
                reason: v["data"]["reason"].as_str().unwrap_or("").to_string(),
                created_at_ms: v["ts"].as_i64().unwrap_or_else(now_ms),
            };
            add(app, r);
        }
        "auth.elevate_granted" | "auth.elevate_denied" => remove(app, mi, id),
        _ => {}
    }
}

pub fn on_reply(app: &mut App, mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    match r {
        Reply::List => {
            // Older servers (`method_not_found`) and scoped clients have nothing to show.
            let Ok(v) = res else {
                return;
            };
            let listed: Vec<Request> = v["pending"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter(|x| x["status"].as_str().is_none_or(|s| s == "pending"))
                        .filter_map(|x| {
                            Some(Request {
                                machine: mi,
                                id: x["request"].as_str()?.to_string(),
                                pane: x["pane"].as_str().unwrap_or("").to_string(),
                                reason: x["reason"].as_str().unwrap_or("").to_string(),
                                created_at_ms: x["created_at_ms"].as_i64().unwrap_or_else(now_ms),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            app.ux.elevate.requests.retain(|r| r.machine != mi);
            for r in listed {
                add(app, r);
            }
            clamp(app);
            app.dirty = true;
        }
        Reply::Decide { id, approve } => {
            app.ux.elevate.deciding.remove(&(mi, id.clone()));
            match res {
                Ok(v) => {
                    let pane = label_of(app, mi, v["pane"].as_str().unwrap_or(""));
                    remove(app, mi, &id);
                    let msg = if approve {
                        let until = v["expires_at_ms"]
                            .as_i64()
                            .map(|t| format!(" until {}", crate::statusbar::clock(t)))
                            .unwrap_or_default();
                        format!("approved elevation for {pane}{until}")
                    } else {
                        format!("denied elevation for {pane}")
                    };
                    app.toast(msg);
                }
                Err(e) => {
                    let why = if e.is_method_not_found() {
                        "this server has no auth.elevate.decide (upgrade it)".to_string()
                    } else if e.kind == "permission_denied" {
                        format!(
                            "not full scope: run `vibeke auth decide {id} {}` outside Vibeke",
                            if approve { "approve" } else { "deny" }
                        )
                    } else if e.kind == "not_found" || e.kind == "conflict" {
                        remove(app, mi, &id);
                        format!(
                            "{id}: {} (withdrawn, expired or decided elsewhere)",
                            e.message
                        )
                    } else {
                        format!("✗ {}", e.message)
                    };
                    if let Some(v) = app.ux.elevate.view.as_mut() {
                        v.notice = Some(why.clone());
                    } else {
                        app.toast(why);
                    }
                }
            }
            app.dirty = true;
        }
    }
}

/// Drop requests the server has forgotten (and those of machines that went away).
pub fn tick(app: &mut App) {
    let now = now_ms();
    let n = app.ux.elevate.requests.len();
    let machines = app.machines.len();
    app.ux
        .elevate
        .requests
        .retain(|r| r.expires_at_ms() > now && r.machine < machines);
    if app.ux.elevate.requests.len() != n {
        clamp(app);
        app.dirty = true;
    }
    // The view closed by any route: forget its state.
    if app.ux.elevate.view.is_some() && !matches!(app.mode, Mode::Popup(Popup::Elevate)) {
        app.ux.elevate.view = None;
    }
}

pub fn deadlines(app: &App, d: &mut crate::deadline::Deadlines) {
    let now = now_ms();
    if let Some(t) = app
        .ux
        .elevate
        .requests
        .iter()
        .map(|r| r.expires_at_ms())
        .min()
    {
        let ms = (t - now).max(0) as u64;
        d.at("elevate.expire", Instant::now() + Duration::from_millis(ms));
    }
    // The arm delay ends: redraw so the key hint shows as live.
    if let Some(v) = &app.ux.elevate.view {
        let armed = v.opened_at + ARM_DELAY;
        if armed > Instant::now() {
            d.redraw("elevate.arm", armed);
        }
    }
}

/// `w1:p3 (claude · api)` for a pane of machine `mi`.
pub fn label_of(app: &App, mi: usize, pane: &str) -> String {
    let Some(m) = app.machines.get(mi) else {
        return pane.to_string();
    };
    let Some(p) = m.model.panes.iter().find(|p| p.id == pane) else {
        return if pane.is_empty() {
            "a pane".into()
        } else {
            pane.to_string()
        };
    };
    let agent = m
        .model
        .runs
        .iter()
        .find(|r| r.pane == p.id && r.ended_at_ms.is_none())
        .map(|r| r.name.clone().unwrap_or_else(|| r.harness.clone()));
    let ws = m
        .model
        .workspaces
        .iter()
        .find(|w| w.id == p.workspace)
        .map(|w| w.display_name().to_string());
    let what: Vec<String> = [agent, ws].into_iter().flatten().collect();
    let on = if app.machines.len() > 1 {
        format!("{}/", m.label)
    } else {
        String::new()
    };
    if what.is_empty() {
        format!("{on}{}", p.handle)
    } else {
        format!("{on}{} ({})", p.handle, what.join(" · "))
    }
}

/// The tab bar's notice while requests are open (chrome only, never over a pane).
pub fn notice(app: &App) -> Option<String> {
    let s = &app.ux.elevate;
    let first = s.requests.first()?;
    let key = app
        .keymap
        .binding_for("elevation_requests")
        .unwrap_or_else(|| ":elevation_requests".into());
    let pane = label_of(app, first.machine, &first.pane);
    let more = match s.requests.len() {
        1 => String::new(),
        n => format!(" +{}", n - 1),
    };
    Some(format!(
        " ⚿ {} asks for elevated access{more} — {key} ",
        crate::draw::truncate(&pane, 28)
    ))
}

pub fn open(app: &mut App) {
    if app.ux.elevate.requests.is_empty() {
        app.toast("no elevation requests");
        return;
    }
    app.ux.elevate.view = Some(View {
        sel: 0,
        opened_at: Instant::now(),
        notice: None,
    });
    app.mode = Mode::Popup(Popup::Elevate);
}

pub fn action(app: &mut App, action: &str) -> bool {
    if matches!(action, "elevation_requests" | "elevate") {
        open(app);
        return true;
    }
    false
}

fn decide(app: &mut App, approve: bool) {
    let Some(sel) = app.ux.elevate.view.as_ref().map(|v| v.sel) else {
        return;
    };
    let Some(r) = app.ux.elevate.requests.get(sel).cloned() else {
        return;
    };
    if !can_decide(app, r.machine) {
        if let Some(v) = app.ux.elevate.view.as_mut() {
            v.notice = Some(format!(
                "inside a Vibeke pane: run `vibeke auth decide {} {}` outside Vibeke",
                r.id,
                if approve { "approve" } else { "deny" }
            ));
        }
        return;
    }
    if !app.ux.elevate.deciding.insert((r.machine, r.id.clone())) {
        return;
    }
    app.command_on(
        r.machine,
        "auth.elevate.decide",
        json!({"request": r.id, "decision": if approve { "approve" } else { "deny" }}),
        Pending::Ux(crate::ux::Reply::Elevate(Reply::Decide {
            id: r.id.clone(),
            approve,
        })),
    );
    if let Some(v) = app.ux.elevate.view.as_mut() {
        v.notice = Some(format!(
            "{}…",
            if approve { "approving" } else { "denying" }
        ));
    }
}

pub fn key(app: &mut App, ev: KeyEvent) {
    let keep = |app: &mut App| app.mode = Mode::Popup(Popup::Elevate);
    let Some(opened) = app.ux.elevate.view.as_ref().map(|v| v.opened_at) else {
        return;
    };
    if ev.kind == KeyKind::Release {
        return keep(app);
    }
    // Esc always closes (nothing decided); everything else waits for the arm delay and must be
    // a plain key.
    if matches!(ev.key, Key::Named(NamedKey::Escape)) {
        app.ux.elevate.view = None;
        return;
    }
    if opened.elapsed() < ARM_DELAY
        || ev.mods.contains(Mods::CTRL)
        || ev.mods.contains(Mods::ALT)
        || ev.mods.contains(Mods::SUPER)
    {
        return keep(app);
    }
    let n = app.ux.elevate.requests.len();
    match ev.key {
        Key::Char('q') => {
            app.ux.elevate.view = None;
            return;
        }
        Key::Char('j') | Key::Named(NamedKey::Down) => {
            if let Some(v) = app.ux.elevate.view.as_mut() {
                v.sel = (v.sel + 1).min(n.saturating_sub(1));
                v.notice = None;
            }
        }
        Key::Char('k') | Key::Named(NamedKey::Up) => {
            if let Some(v) = app.ux.elevate.view.as_mut() {
                v.sel = v.sel.saturating_sub(1);
                v.notice = None;
            }
        }
        Key::Char('y') => decide(app, true),
        Key::Char('n') => decide(app, false),
        Key::Char('o') => {
            let sel = app.ux.elevate.view.as_ref().map(|v| v.sel).unwrap_or(0);
            if let Some(r) = app.ux.elevate.requests.get(sel).cloned()
                && app.machines[r.machine]
                    .model
                    .panes
                    .iter()
                    .any(|p| p.id == r.pane)
            {
                app.ux.elevate.view = None;
                app.focus_pane(r.machine, &r.pane);
                return;
            }
        }
        _ => {}
    }
    if app.ux.elevate.requests.is_empty() {
        app.ux.elevate.view = None;
        return;
    }
    keep(app);
}

fn wrap(s: &str, w: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > w {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub fn draw(app: &App, g: &mut Grid) {
    let Some(v) = &app.ux.elevate.view else {
        return;
    };
    let t = app.theme;
    let reqs = &app.ux.elevate.requests;
    let mut a = crate::drafts::Area::open(
        app,
        g,
        &format!(
            "Vibeke · elevation request{} — drawn by Vibeke, not by any pane",
            if reqs.len() > 1 {
                format!("s ({})", reqs.len())
            } else {
                String::new()
            }
        ),
    );
    let now = now_ms();
    if reqs.len() > 1 {
        for (i, r) in reqs.iter().enumerate() {
            let st = if i == v.sel {
                t.sel(t.accent)
            } else {
                t.text()
            };
            let mark = if i == v.sel { "▸" } else { " " };
            a.line(
                &format!(
                    "{mark} {} · {} · {} ago",
                    r.id,
                    label_of(app, r.machine, &r.pane),
                    crate::inbox::fmt_age(now - r.created_at_ms)
                ),
                st,
            );
        }
        a.line("", t.text());
    }
    let Some(r) = reqs.get(v.sel) else {
        a.line("no open requests", t.dim());
        return;
    };
    let width = (app.pane_area().w as usize)
        .saturating_sub(8)
        .clamp(20, 100);
    a.line(
        &format!(
            "Pane {} asks for elevated access",
            label_of(app, r.machine, &r.pane)
        ),
        t.bold(t.fg),
    );
    a.line(
        &format!(
            "machine {} · request {} · asked {} ago",
            app.machines[r.machine].label,
            r.id,
            crate::inbox::fmt_age(now - r.created_at_ms)
        ),
        t.dim(),
    );
    a.line("", t.text());
    a.line("Reason (written by the pane; unverified):", t.dim());
    let reason = crate::plugins::sanitize(&r.reason, 500);
    if reason.is_empty() {
        a.line("  (no reason given)", t.dim());
    } else {
        for (i, l) in wrap(&reason, width).iter().take(6).enumerate() {
            let q = if i == 0 { "“" } else { " " };
            a.line(&format!("  {q}{l}"), t.text());
        }
    }
    a.line("", t.text());
    let left = r.expires_at_ms() - now;
    a.line(
        &format!(
            "Expiry: the request stays open until {} (in {}).",
            crate::statusbar::clock(r.expires_at_ms()),
            crate::inbox::fmt_age(left)
        ),
        if left < 2 * 60 * 1000 {
            t.s(t.red)
        } else {
            t.text()
        },
    );
    a.line(
        &format!(
            "Approving grants full API access for {GRANT_MINUTES} minutes, from that pane only."
        ),
        t.text(),
    );
    a.line("", t.text());
    if let Some(n) = &v.notice {
        a.line(n, t.bold(t.yellow));
    }
    if !can_decide(app, r.machine) {
        a.line(
            "This client runs inside a Vibeke pane and can't decide.",
            t.bold(t.yellow),
        );
        a.line(
            &format!(
                "Outside Vibeke run: vibeke auth decide {} approve|deny",
                r.id
            ),
            t.bold(t.yellow),
        );
    }
    let armed = v.opened_at.elapsed() >= ARM_DELAY;
    let deciding = app.ux.elevate.deciding.contains(&(r.machine, r.id.clone()));
    let keys = if deciding {
        "deciding…".to_string()
    } else if armed {
        let more = if reqs.len() > 1 { "  [j/k] select" } else { "" };
        format!("[y] approve   [n] deny   [o] go to the pane   [esc] later{more}")
    } else {
        "…".to_string()
    };
    a.footer(&keys, if armed { t.bold(t.fg) } else { t.dim() });
}

#[cfg(test)]
#[path = "elevate_tests.rs"]
mod tests;
