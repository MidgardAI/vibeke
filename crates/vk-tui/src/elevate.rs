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
//! restarts, `n` denies, through `auth.approve.decide`. Nothing is ever approved automatically.
//!
//! What a key decides is guarded:
//!
//! - the selection is the request's (machine, id), never a list index: when the request under
//!   review goes away (decided, withdrawn, expired) nothing is selected until the user selects
//!   again, so a request that slides into its place is never decided by a key meant for the
//!   other one;
//! - keys typed in the first 600 ms after the view opens **or the selection changes** are
//!   ignored (they were meant for the pane, or for the previous request);
//! - only a fresh key press with no modifier at all decides: never a repeat or a release, and
//!   (with the kitty keyboard protocol, which reports repeats and releases) never a key that was
//!   already held while the view was not armed, until it has been released. Without the
//!   protocol the arm delay is the guard.
//!
//! A client that is itself inside a pane (not full scope) cannot decide; the view says so and
//! points at the CLI outside Vibeke.
//!
//! The view opens only when the user opens it (`elevation_requests`, default `prefix+shift+e`,
//! the notice, or the palette), for approved calls exactly like elevation: a request never
//! opens it by itself, whatever the pane runs, since a pane controls its own argv (and so what
//! its foreground process looks like).

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::screen::Grid;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::time::{Duration, Instant};
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};

/// Keys right after the view opens are swallowed: they were typed for the pane.
pub const ARM_DELAY: Duration = Duration::from_millis(600);
/// The server forgets an undecided request after 30 minutes (`auth.rs` / `approve.rs` `prune`).
pub const REQUEST_TTL_MS: i64 = 30 * 60 * 1000;
/// What an elevation grants.
pub const GRANT_MINUTES: i64 = 10;

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
    /// The request under review as (machine, request id): `None` once it went away, until the
    /// user selects again.
    pub sel: Option<(usize, String)>,
    /// When the view opened or the selection last changed: keys count only [`ARM_DELAY`] later.
    pub opened_at: Instant,
    pub notice: Option<String>,
    /// Kitty keyboard protocol: keys seen pressed while not armed, or repeating, and not seen
    /// released since. Their next press does not decide.
    pub held: HashSet<Key>,
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
    reconcile(app);
    app.dirty = true;
}

/// The index of the selected request, if it is still open.
fn sel_index(app: &App) -> Option<usize> {
    let (mi, id) = app.ux.elevate.view.as_ref()?.sel.as_ref()?;
    app.ux
        .elevate
        .requests
        .iter()
        .position(|r| r.machine == *mi && r.id == *id)
}

/// Select request `i` (or nothing). A different request than before re-arms the view.
fn select(app: &mut App, i: Option<usize>) {
    let sel = i
        .and_then(|i| app.ux.elevate.requests.get(i))
        .map(|r| (r.machine, r.id.clone()));
    if let Some(v) = app.ux.elevate.view.as_mut() {
        if v.sel != sel {
            v.opened_at = Instant::now();
        }
        v.sel = sel;
        v.notice = None;
    }
}

/// The request under review went away (decided, withdrawn, expired, its machine gone): select
/// nothing, so no key meant for it decides another one; the user selects again (and waits out
/// the arm delay again).
fn reconcile(app: &mut App) {
    let gone = app
        .ux
        .elevate
        .view
        .as_ref()
        .is_some_and(|v| v.sel.is_some())
        && sel_index(app).is_none();
    if gone && let Some(v) = app.ux.elevate.view.as_mut() {
        v.sel = None;
        v.opened_at = Instant::now();
        v.notice = Some(
            "that request is gone (decided, withdrawn or expired): select one with j/k".into(),
        );
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
        // Like elevation, an approved call only shows the notice: it never opens the view by
        // itself (08 §8), whatever the pane runs.
        "auth.approval_requested" => add(app, base(Kind::Approval(approval_of(&v["data"])))),
        "auth.elevate_granted"
        | "auth.elevate_denied"
        | "auth.approval_granted"
        | "auth.approval_denied"
        | "auth.approval_withdrawn" => remove(app, mi, id),
        _ => {}
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
            reconcile(app);
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
        reconcile(app);
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
    let r = &app.ux.elevate.requests[0];
    app.ux.elevate.view = Some(View {
        sel: Some((r.machine, r.id.clone())),
        opened_at: Instant::now(),
        notice: None,
        held: HashSet::new(),
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
    // Nothing selected (the request under review went away): nothing to decide.
    let Some(sel) = sel_index(app) else {
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
        if let Some(v) = app.ux.elevate.view.as_mut() {
            v.held.remove(&ev.key);
        }
        return keep(app);
    }
    // Esc always closes (nothing decided); everything else waits for the arm delay and must be
    // a fresh press of a key with no modifier at all.
    if matches!(ev.key, Key::Named(NamedKey::Escape)) {
        app.ux.elevate.view = None;
        return;
    }
    let armed = opened.elapsed() >= ARM_DELAY;
    let kitty = app.kitty;
    let was_held = match app.ux.elevate.view.as_mut() {
        Some(v) => {
            let was = v.held.contains(&ev.key);
            // With the kitty protocol a release will come: remember a key pressed while not
            // armed, or repeating (held since before the view opened), until it is released.
            if kitty && (!armed || ev.kind == KeyKind::Repeat) {
                v.held.insert(ev.key);
            }
            was
        }
        None => false,
    };
    if ev.kind != KeyKind::Press || !ev.mods.is_empty() {
        return keep(app);
    }
    // Moving around never decides (and a new selection re-arms), so it needs no arming; a
    // decision needs the arm delay and a key not held since before it.
    let decides = matches!(ev.key, Key::Char('y' | 'a' | 'n'));
    if decides && !armed {
        return keep(app);
    }
    if decides && was_held {
        // Pressed again without a release we saw: this press does not count, the next one does.
        if let Some(v) = app.ux.elevate.view.as_mut() {
            v.held.remove(&ev.key);
        }
        return keep(app);
    }
    let n = app.ux.elevate.requests.len();
    match ev.key {
        Key::Char('q') => {
            app.ux.elevate.view = None;
            return;
        }
        Key::Char('j') | Key::Named(NamedKey::Down) => {
            let i = match sel_index(app) {
                Some(i) => (i + 1).min(n.saturating_sub(1)),
                None => 0,
            };
            select(app, (n > 0).then_some(i));
        }
        Key::Char('k') | Key::Named(NamedKey::Up) => {
            let i = sel_index(app).map(|i| i.saturating_sub(1)).unwrap_or(0);
            select(app, (n > 0).then_some(i));
        }
        Key::Char('y') => decide(app, Decision::Approve),
        Key::Char('a') => decide(app, Decision::Always),
        Key::Char('n') => decide(app, Decision::Deny),
        Key::Char('o') => {
            if let Some(r) = sel_index(app).and_then(|i| app.ux.elevate.requests.get(i).cloned())
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
    let sel = sel_index(app);
    if reqs.len() > 1 || sel.is_none() {
        for (i, r) in reqs.iter().enumerate() {
            let st = if Some(i) == sel {
                t.sel(t.accent)
            } else {
                t.text()
            };
            let mark = if Some(i) == sel { "▸" } else { " " };
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
    let Some(r) = sel.and_then(|i| reqs.get(i)) else {
        if reqs.is_empty() {
            a.line("no open requests", t.dim());
        } else {
            if let Some(n) = &v.notice {
                a.line(n, t.bold(t.yellow));
            }
            a.footer("[j/k] select a request   [esc] later", t.dim());
        }
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
