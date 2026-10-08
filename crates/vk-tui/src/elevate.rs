//! Requests from panes in the TUI chrome (09 §3.2; 08 §8): elevation and approved calls.
//!
//! A program inside a pane asks the user for something it may not do itself:
//!
//! - **elevation** (`vibeke auth elevate`, `auth.elevate`): a time-boxed full-scope token;
//! - **an approved call** (`vibeke handoff send|cancel|redeem` inside a pane, `auth.approve`):
//!   exactly one frozen call the server summarised from its own facts.
//!
//! The server opens a request, emits `auth.elevate_requested` / `auth.approval_requested` and
//! waits for the user. This module is the out-of-band half: it learns of requests from the
//! pushed events (and `auth.list` after every connect, for requests made before the
//! subscription), shows a non-modal notice in the tab bar's right cluster — never over a pane —
//! and a review view that **replaces the pane area**: the requesting pane's content is not on
//! screen while the prompt is, so nothing the pane draws can pose as the prompt or sit next to
//! it pretending to be part of it. The view names the pane, shows the server's summary of an
//! approved call, quotes the pane's reason as unverified text, and shows how long the request
//! stays open and what approval grants. Elevation: `y` approves and `n` denies through
//! `auth.elevate.decide`. Approved calls: `y` runs the call once, `a` (only when the server
//! allows it) also allows the same call from that pane to that peer until the pane's process
//! restarts, `n` denies, through `auth.approve.decide`. Nothing is ever approved automatically,
//! keys typed in the first 600 ms after the view opens are ignored (they were meant for the
//! pane), and modified keys never decide. A client that is itself inside a pane (not full scope)
//! cannot decide; the view says so and points at the CLI outside Vibeke.
//!
//! The view opens only when the user opens it (`elevation_requests`, default `prefix+shift+e`,
//! the notice, or the palette), with one exception for approved calls ([`auto_open_ok`]): the
//! request comes from the **focused** pane of this client's current machine, that pane has no
//! agent run, and its foreground process (the model's `fg_cmdline`, which the server keeps
//! current from the pane's process group) is the `vibeke` CLI. Then the user just typed the
//! command in a shell and the review view is the expected next step; it still replaces the pane
//! area and still waits out the arm delay. Every other request only shows the notice.

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::screen::Grid;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::time::{Duration, Instant};
use vk_proto::input::{Key, KeyEvent, KeyKind, Mods, NamedKey};

/// Keys right after the view opens are swallowed: they were typed for the pane.
pub const ARM_DELAY: Duration = Duration::from_millis(600);
/// The server forgets an undecided request after 30 minutes (`auth.rs` / `approve.rs` `prune`).
pub const REQUEST_TTL_MS: i64 = 30 * 60 * 1000;
/// What an elevation grants.
pub const GRANT_MINUTES: i64 = 10;
/// An approved-call request may open the view by itself only this soon after it arrived: the
/// pane's foreground process can be reported a moment after the request.
pub const AUTO_OPEN_WINDOW: Duration = Duration::from_secs(5);

/// What a pane asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// `auth.elevate`: full scope for [`GRANT_MINUTES`] minutes.
    Elevation,
    /// `auth.approve`: one frozen call.
    Approval(Approval),
}

/// An approved call as the server describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Approval {
    /// `handoff.send`, `handoff.cancel` or `gateway.call` (peer.redeem).
    pub method: String,
    /// Computed by the server from its own facts, never from the pane's text.
    pub summary: String,
    /// Whether `always` may be chosen (not for peer.redeem).
    pub always_allowed: bool,
    /// The peer's name (`auth.list`) or id (the event), when the call goes to one.
    pub peer: Option<String>,
}

/// One open request.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub machine: usize,
    pub id: String,
    pub pane: String,
    pub reason: String,
    pub created_at_ms: i64,
    pub kind: Kind,
}

impl Request {
    pub fn expires_at_ms(&self) -> i64 {
        self.created_at_ms + REQUEST_TTL_MS
    }
    pub fn approval(&self) -> Option<&Approval> {
        match &self.kind {
            Kind::Approval(a) => Some(a),
            Kind::Elevation => None,
        }
    }
}

/// A decision the user can make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    /// Approved calls only: also a standing grant for (pane, method, peer).
    Always,
    Deny,
}

impl Decision {
    pub fn wire(self) -> &'static str {
        match self {
            Decision::Approve => "approve",
            Decision::Always => "always",
            Decision::Deny => "deny",
        }
    }
}

#[derive(Debug, Clone)]
pub struct View {
    pub sel: usize,
    pub opened_at: Instant,
    pub notice: Option<String>,
    /// Opened by itself for a command just typed in the focused shell pane.
    pub auto: bool,
}

#[derive(Debug, Default)]
pub struct State {
    pub requests: Vec<Request>,
    pub view: Option<View>,
    /// Decisions in flight: (machine, request).
    pub deciding: HashSet<(usize, String)>,
    /// Approved-call requests that may still open the view by itself: (machine, request,
    /// when it arrived). See [`auto_open_ok`].
    pub auto: Vec<(usize, String, Instant)>,
    /// Tests: pretend this client is (not) full scope.
    pub scoped_override: Option<bool>,
}

#[derive(Debug, Clone)]
pub enum Reply {
    List,
    Decide {
        id: String,
        decision: Decision,
        /// An approved call (`auth.approve.decide`) rather than elevation.
        approval: bool,
    },
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Whether this client may decide on machine `mi`: a TUI started inside a Vibeke pane of that
/// (local) server holds that pane's scope, and the server refuses `auth.elevate.decide` and
/// `auth.approve.decide` from it.
pub fn can_decide(app: &App, mi: usize) -> bool {
    if let Some(scoped) = app.ux.elevate.scoped_override {
        return !scoped;
    }
    !(app.machines[mi].local
        && (std::env::var_os("VIBEKE_PANE_TOKEN").is_some()
            || std::env::var_os("VIBEKE_ELEVATED_TOKEN").is_some()))
}

/// What an approved call does, for the notice and the view's heading.
fn verb(method: &str) -> &'static str {
    match method {
        "handoff.send" => "send a handoff",
        "handoff.cancel" => "cancel a handoff",
        "gateway.call" => "redeem a peer invitation",
        _ => "run a call",
    }
}

/// The CLI command that decides request `r` outside Vibeke.
fn cli_for(r: &Request, decision: &str) -> String {
    match r.kind {
        Kind::Elevation => format!("vibeke auth decide {} {decision}", r.id),
        Kind::Approval(_) => format!("vibeke auth approval {} {decision}", r.id),
    }
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
    app.ux
        .elevate
        .auto
        .retain(|(m, x, _)| !(*m == mi && x == id));
    clamp(app);
    app.dirty = true;
}

fn clamp(app: &mut App) {
    let n = app.ux.elevate.requests.len();
    if let Some(v) = app.ux.elevate.view.as_mut() {
        v.sel = v.sel.min(n.saturating_sub(1));
    }
}

fn approval_of(x: &Value) -> Approval {
    // `auth.list` gives the peer as {id, name, owner}; the event gives its id.
    let peer = match &x["peer"] {
        Value::String(s) => Some(s.clone()),
        Value::Object(_) => x["peer"]["name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| x["peer"]["id"].as_str())
            .map(str::to_string),
        _ => None,
    };
    Approval {
        method: x["method"].as_str().unwrap_or("").to_string(),
        summary: x["summary"].as_str().unwrap_or("").to_string(),
        always_allowed: x["always_allowed"].as_bool().unwrap_or(false),
        peer,
    }
}

/// Pushed `auth.elevate_*` and `auth.approval_*` events.
pub fn on_event(app: &mut App, mi: usize, kind: &str, v: &Value) {
    let Some(id) = v["subject"]["request"].as_str() else {
        return;
    };
    let base = |k: Kind| Request {
        machine: mi,
        id: id.to_string(),
        pane: v["subject"]["pane"].as_str().unwrap_or("").to_string(),
        reason: v["data"]["reason"].as_str().unwrap_or("").to_string(),
        created_at_ms: v["ts"].as_i64().unwrap_or_else(now_ms),
        kind: k,
    };
    match kind {
        "auth.elevate_requested" => add(app, base(Kind::Elevation)),
        "auth.approval_requested" => {
            let r = base(Kind::Approval(approval_of(&v["data"])));
            add(app, r);
            app.ux
                .elevate
                .auto
                .push((mi, id.to_string(), Instant::now()));
            auto_open(app);
        }
        "auth.elevate_granted"
        | "auth.elevate_denied"
        | "auth.approval_granted"
        | "auth.approval_denied"
        | "auth.approval_withdrawn" => remove(app, mi, id),
        _ => {}
    }
}

/// The auto-open exception (08 §8): an approved-call request opens the review view without a
/// key press only when it comes from the **focused** pane of this client's current machine,
/// that pane has **no agent run**, and its foreground process is the **`vibeke` CLI** (the
/// model's `fg_cmdline`, which the server updates on `pane.process_changed` from the pane's
/// foreground process group), and nothing else is open (normal mode) and this client may
/// decide. Then the user has just typed the command in a shell. Elevation never opens by itself.
pub fn auto_open_ok(app: &App, r: &Request) -> bool {
    if r.approval().is_none()
        || r.machine != app.cur
        || !matches!(app.mode, Mode::Normal)
        || app.ux.elevate.view.is_some()
        || !can_decide(app, r.machine)
    {
        return false;
    }
    if app.focused_pane().as_deref() != Some(r.pane.as_str()) {
        return false;
    }
    let Some(m) = app.machines.get(r.machine) else {
        return false;
    };
    if m.model
        .runs
        .iter()
        .any(|x| x.pane == r.pane && x.ended_at_ms.is_none())
    {
        return false;
    }
    m.model
        .panes
        .iter()
        .find(|p| p.id == r.pane)
        .and_then(|p| p.fg_cmdline.first())
        .is_some_and(|a| is_vibeke_cli(a))
}

/// `vibeke`, `/usr/local/bin/vibeke` (not `vibeke-gateway`, not a script that runs it).
fn is_vibeke_cli(argv0: &str) -> bool {
    argv0
        .rsplit('/')
        .next()
        .unwrap_or(argv0)
        .trim_start_matches('-')
        == "vibeke"
}

/// Open the view for a fresh request that meets [`auto_open_ok`] (checked again after every
/// wakeup for [`AUTO_OPEN_WINDOW`], since the pane's foreground process may arrive later).
fn auto_open(app: &mut App) {
    if app.ux.elevate.auto.is_empty() {
        return;
    }
    app.ux
        .elevate
        .auto
        .retain(|(_, _, at)| at.elapsed() < AUTO_OPEN_WINDOW);
    let cands = app.ux.elevate.auto.clone();
    for (mi, id, _) in cands {
        let Some(i) = app
            .ux
            .elevate
            .requests
            .iter()
            .position(|r| r.machine == mi && r.id == id)
        else {
            continue;
        };
        if auto_open_ok(app, &app.ux.elevate.requests[i]) {
            app.ux
                .elevate
                .auto
                .retain(|(m, x, _)| !(*m == mi && *x == id));
            open_at(app, i, true);
            return;
        }
    }
}

fn listed(mi: usize, x: &Value, kind: Kind) -> Option<Request> {
    Some(Request {
        machine: mi,
        id: x["request"].as_str()?.to_string(),
        pane: x["pane"].as_str().unwrap_or("").to_string(),
        reason: x["reason"].as_str().unwrap_or("").to_string(),
        created_at_ms: x["created_at_ms"].as_i64().unwrap_or_else(now_ms),
        kind,
    })
}

fn pending(x: &Value) -> bool {
    x["status"].as_str().is_none_or(|s| s == "pending")
}

pub fn on_reply(app: &mut App, mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    match r {
        Reply::List => {
            // Older servers (`method_not_found`) and scoped clients have nothing to show.
            let Ok(v) = res else {
                return;
            };
            let mut all: Vec<Request> = v["pending"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter(|x| pending(x))
                        .filter_map(|x| listed(mi, x, Kind::Elevation))
                        .collect()
                })
                .unwrap_or_default();
            // Approved calls (servers before them have no `approvals`).
            if let Some(a) = v["approvals"].as_array() {
                all.extend(
                    a.iter()
                        .filter(|x| pending(x))
                        .filter_map(|x| listed(mi, x, Kind::Approval(approval_of(x)))),
                );
            }
            app.ux.elevate.requests.retain(|r| r.machine != mi);
            for r in all {
                add(app, r);
            }
            clamp(app);
            app.dirty = true;
        }
        Reply::Decide {
            id,
            decision,
            approval,
        } => {
            app.ux.elevate.deciding.remove(&(mi, id.clone()));
            match res {
                Ok(v) => {
                    let pane = label_of(app, mi, v["pane"].as_str().unwrap_or(""));
                    remove(app, mi, &id);
                    let msg = if approval {
                        approval_done(&pane, decision, &v)
                    } else if decision != Decision::Deny {
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
                    let method = if approval {
                        "auth.approve.decide"
                    } else {
                        "auth.elevate.decide"
                    };
                    let cli = if approval {
                        format!("vibeke auth approval {id} {}", decision.wire())
                    } else {
                        format!("vibeke auth decide {id} {}", decision.wire())
                    };
                    let why = if e.is_method_not_found() {
                        format!("this server has no {method} (upgrade it)")
                    } else if e.kind == "permission_denied" {
                        format!("not full scope: run `{cli}` outside Vibeke")
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

/// The toast after `auth.approve.decide` answered: `{decision, grant, ok, result, error}`.
fn approval_done(pane: &str, decision: Decision, v: &Value) -> String {
    if v["decision"].as_str() == Some("denied") || decision == Decision::Deny {
        return format!("denied the request of {pane}");
    }
    let always = if v["grant"].as_str() == Some("always") {
        " (and always from this pane until it restarts)"
    } else {
        ""
    };
    if v["ok"].as_bool() == Some(false) {
        let why = v["error"]["message"]
            .as_str()
            .or_else(|| v["error"].as_str())
            .unwrap_or("failed");
        return format!("approved for {pane}{always}, but the call failed: {why}");
    }
    let what = match v
        .pointer("/result/job/id")
        .and_then(Value::as_str)
        .or_else(|| v.pointer("/result/peer/name").and_then(Value::as_str))
    {
        Some(x) => format!(": {x}"),
        None => String::new(),
    };
    format!("approved for {pane}{always}{what}")
}

/// Drop requests the server has forgotten (and those of machines that went away); open the
/// view for a fresh request that meets the auto-open rule.
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
    auto_open(app);
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
    let what = match &first.kind {
        Kind::Elevation => "asks for elevated access".to_string(),
        Kind::Approval(a) => format!("asks to {}", verb(&a.method)),
    };
    Some(format!(
        " ⚿ {} {what}{more} — {key} ",
        crate::draw::truncate(&pane, 28)
    ))
}

pub fn open(app: &mut App) {
    if app.ux.elevate.requests.is_empty() {
        app.toast("no elevation or approval requests");
        return;
    }
    open_at(app, 0, false);
}

fn open_at(app: &mut App, sel: usize, auto: bool) {
    app.ux.elevate.view = Some(View {
        sel,
        opened_at: Instant::now(),
        notice: None,
        auto,
    });
    app.mode = Mode::Popup(Popup::Elevate);
    app.dirty = true;
}

pub fn action(app: &mut App, action: &str) -> bool {
    if matches!(
        action,
        "elevation_requests" | "elevate" | "approval_requests" | "requests"
    ) {
        open(app);
        return true;
    }
    false
}

fn decide(app: &mut App, decision: Decision) {
    let Some(sel) = app.ux.elevate.view.as_ref().map(|v| v.sel) else {
        return;
    };
    let Some(r) = app.ux.elevate.requests.get(sel).cloned() else {
        return;
    };
    let approval = r.approval().cloned();
    if decision == Decision::Always {
        match &approval {
            // Elevation has no "always".
            None => return,
            Some(a) if !a.always_allowed => {
                if let Some(v) = app.ux.elevate.view.as_mut() {
                    v.notice = Some("this call can only be approved once: [y] or [n]".into());
                }
                return;
            }
            Some(_) => {}
        }
    }
    if !can_decide(app, r.machine) {
        if let Some(v) = app.ux.elevate.view.as_mut() {
            v.notice = Some(format!(
                "inside a Vibeke pane: run `{}` outside Vibeke",
                cli_for(&r, decision.wire())
            ));
        }
        return;
    }
    if !app.ux.elevate.deciding.insert((r.machine, r.id.clone())) {
        return;
    }
    let method = if approval.is_some() {
        "auth.approve.decide"
    } else {
        "auth.elevate.decide"
    };
    app.command_on(
        r.machine,
        method,
        json!({"request": r.id, "decision": decision.wire()}),
        Pending::Ux(crate::ux::Reply::Elevate(Reply::Decide {
            id: r.id.clone(),
            decision,
            approval: approval.is_some(),
        })),
    );
    if let Some(v) = app.ux.elevate.view.as_mut() {
        v.notice = Some(format!(
            "{}…",
            if decision == Decision::Deny {
                "denying"
            } else {
                "approving"
            }
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
        Key::Char('y') => decide(app, Decision::Approve),
        Key::Char('a') => decide(app, Decision::Always),
        Key::Char('n') => decide(app, Decision::Deny),
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
    let approvals = reqs.iter().filter(|r| r.approval().is_some()).count();
    let what = if approvals == 0 {
        "elevation request"
    } else if approvals == reqs.len() {
        "approval request"
    } else {
        "request"
    };
    let mut a = crate::drafts::Area::open(
        app,
        g,
        &format!(
            "Vibeke · {what}{} — drawn by Vibeke, not by any pane",
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
            let kind = match &r.kind {
                Kind::Elevation => "elevation".to_string(),
                Kind::Approval(x) => verb(&x.method).to_string(),
            };
            a.line(
                &format!(
                    "{mark} {} · {kind} · {} · {} ago",
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
    let pane = label_of(app, r.machine, &r.pane);
    let heading = match &r.kind {
        Kind::Elevation => format!("Pane {pane} asks for elevated access"),
        Kind::Approval(x) => format!("Pane {pane} asks to {}", verb(&x.method)),
    };
    a.line(&heading, t.bold(t.fg));
    a.line(
        &format!(
            "machine {} · request {} · asked {} ago",
            app.machines[r.machine].label,
            r.id,
            crate::inbox::fmt_age(now - r.created_at_ms)
        ),
        t.dim(),
    );
    if v.auto {
        for l in wrap(
            "Opened because you just ran vibeke in this pane; nothing is decided until you press a key.",
            width,
        ) {
            a.line(&l, t.dim());
        }
    }
    a.line("", t.text());
    if let Some(x) = r.approval() {
        a.line(
            "What will happen (worked out by Vibeke from its own facts):",
            t.dim(),
        );
        let summary = crate::plugins::sanitize(&x.summary, 1000);
        if summary.is_empty() {
            a.line("  (no summary)", t.dim());
        } else {
            for l in wrap(&summary, width).iter().take(8) {
                a.line(&format!("  {l}"), t.bold(t.fg));
            }
        }
        a.line("", t.text());
    }
    a.line(
        "Reason (written by the pane; unverified — Vibeke has not checked it):",
        t.dim(),
    );
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
    match r.approval() {
        None => a.line(
            &format!(
                "Approving grants full API access for {GRANT_MINUTES} minutes, from that pane only."
            ),
            t.text(),
        ),
        Some(x) => {
            a.line(
                "[y] runs exactly this call once, as you; nothing else is granted.",
                t.text(),
            );
            if x.always_allowed {
                let peer = x
                    .peer
                    .as_deref()
                    .map(|p| format!(" to {}", crate::plugins::sanitize(p, 60)))
                    .unwrap_or_default();
                let text = format!(
                    "[a] also runs the same call from this pane{peer} without asking, until the pane's process restarts."
                );
                for l in wrap(&text, width) {
                    a.line(&l, t.text());
                }
            } else {
                a.line("This call can only be approved once.", t.dim());
            }
        }
    }
    a.line("", t.text());
    if let Some(n) = &v.notice {
        a.line(n, t.bold(t.yellow));
    }
    if !can_decide(app, r.machine) {
        a.line(
            "This client runs inside a Vibeke pane and can't decide.",
            t.bold(t.yellow),
        );
        let choices = match r.approval() {
            None => "approve|deny",
            Some(x) if x.always_allowed => "approve|always|deny",
            Some(_) => "approve|deny",
        };
        a.line(
            &format!("Outside Vibeke run: {}", cli_for(r, choices)),
            t.bold(t.yellow),
        );
    }
    let armed = v.opened_at.elapsed() >= ARM_DELAY;
    let deciding = app.ux.elevate.deciding.contains(&(r.machine, r.id.clone()));
    let keys = if deciding {
        "deciding…".to_string()
    } else if armed {
        let more = if reqs.len() > 1 { "  [j/k] select" } else { "" };
        match r.approval() {
            None => {
                format!("[y] approve   [n] deny   [o] go to the pane   [esc] later{more}")
            }
            Some(x) => format!(
                "[y] approve once   {}[n] deny   [o] go to the pane   [esc] later{more}",
                if x.always_allowed {
                    "[a] always   "
                } else {
                    ""
                }
            ),
        }
    } else {
        "…".to_string()
    };
    a.footer(&keys, if armed { t.bold(t.fg) } else { t.dim() });
}

#[cfg(test)]
#[path = "elevate_tests.rs"]
mod tests;
