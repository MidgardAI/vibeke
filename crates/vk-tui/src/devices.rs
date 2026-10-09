//! Devices in the TUI: a full pane-area view (`devices` in the palette, `prefix+alt+d`) of the
//! phones and apps paired with one machine, reached through that machine's server with
//! `gateway.call` (like [`crate::sharing`]). Three stages:
//!
//! 1. **List** (`devices.list`, only `kind == "device"`): name, platform, scope, how long ago it
//!    paired, 🔔 when it takes push. `n` pairs a new one, `x` revokes the selected one after a
//!    confirm (`devices.revoke`), `r` reads the list again. When the relay needs an account
//!    (`account.status`, read once on opening), a line shows who is signed in, and `s` signs in.
//! 2. **Pick scope**: full (default), approve or view. Enter asks `account.status` first: a relay
//!    that needs an account nobody has signed in to goes through **Sign in**, anything else (an
//!    open relay, an older gateway without the method) creates the link (`pair.create`).
//!    `pair_phone` / `phone_pairing` open the view here.
//! 3. **Pairing**: the link with a QR code, the scope and a countdown to when the link stops
//!    working. `pair.status` is polled about once a second (one request in flight): the phone
//!    claims the link (its fingerprint shows here and in the confirm prompt), then the pairing
//!    is done ("Paired ✓") and the list is read again. Esc cancels a pending pairing
//!    (`share.revoke {id: pid}`, best effort).
//!
//! **Sign in** (`account.login.start`): a device code with its URL and a QR code of the URL with
//! the code filled in, polled like a pairing (`account.login.status`). Done continues with the
//! picked scope's link; an expired code goes back to the scope picker; Esc cancels
//! (`account.login.cancel`). The gateway's text is cleaned of control and bidi characters
//! before it is shown or copied.
//!
//! Without a gateway (or without a relay/app URL) the server answers with a message that the view
//! shows as it is.

use std::sync::atomic::{AtomicU64, Ordering};
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
/// Past the bridge's own timeout: a `pair.status` with no answer by then never gets one.
const STALE: Duration = Duration::from_secs(35);
/// The most kept of any text the account sign-in shows.
const CLEAN_MAX: usize = 512;
/// When `account.login.start` leaves out `expires_in`.
/// Longest countdown we show, whatever the gateway says.
const MAX_EXPIRES_IN: u64 = 3600;
const DEFAULT_EXPIRES_IN: u64 = 900;

/// Tags each `pair.create`, so a late answer can't land in a newer attempt or a reopened view.
static NEXT_ATTEMPT: AtomicU64 = AtomicU64::new(1);

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

/// `s` without control characters or bidi/format controls that could disguise a URL or code,
/// trimmed and capped at [`CLEAN_MAX`] characters.
pub(crate) fn clean(s: &str) -> String {
    let kept: String = s
        .chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(
                    *c,
                    '\u{200E}'
                        | '\u{200F}'
                        | '\u{202A}'..='\u{202E}'
                        | '\u{2066}'..='\u{2069}'
                        | '\u{061C}'
                        | '\u{2028}'
                        | '\u{2029}'
                )
        })
        .collect();
    kept.trim().chars().take(CLEAN_MAX).collect()
}

fn clean_of(v: &Value, k: &str) -> Option<String> {
    s_of(v, k).map(|s| clean(&s)).filter(|s| !s.is_empty())
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
    /// A phone opened it and is asking to pair: y/n in the Confirm prompt.
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
    /// When the outstanding `pair.status` was sent. A reply lost to a reconnect never comes, so
    /// after [`STALE`] the next poll goes out anyway.
    inflight: Option<Instant>,
}

/// What `gateway.status` says: whether the gateway can serve a phone right now.
#[derive(Debug, Clone, PartialEq)]
pub struct GwHealth {
    pub connected: bool,
    /// The supervisor's state when this server manages the gateway.
    pub state: Option<String>,
    pub last_error: Option<String>,
}

impl GwHealth {
    fn from_value(x: &Value) -> GwHealth {
        GwHealth {
            connected: x["connected"].as_bool().unwrap_or(true),
            state: s_of(x, "state"),
            last_error: clean_of(x, "last_error"),
        }
    }

    /// Why a link made now would be useless, if it would be. States this view can't judge
    /// (`external`, `local_only`, none) pass as before.
    fn problem(&self) -> Option<String> {
        let bad = matches!(
            self.state.as_deref(),
            Some("off" | "starting" | "connecting" | "offline" | "login_required" | "crashed")
        );
        if !self.connected || bad {
            let state = match &self.state {
                Some(st) if bad => format!(" (state {st})"),
                _ => String::new(),
            };
            let why = self
                .last_error
                .as_deref()
                .map(|e| format!(" — {e}"))
                .unwrap_or_default();
            Some(format!(
                "The gateway is offline{state}{why}: a phone can't use a link until it's online — run `vibeke gateway on`"
            ))
        } else {
            None
        }
    }
}

/// Where a relay-account sign-in stands (`account.login.status`).
#[derive(Debug, Clone, PartialEq)]
pub enum SignInStatus {
    Pending,
    Done { login: String },
    Expired,
    Denied,
    Error { message: String },
}

impl SignInStatus {
    fn from_value(x: &Value) -> Option<SignInStatus> {
        Some(match x["status"].as_str()? {
            "pending" => SignInStatus::Pending,
            "done" => SignInStatus::Done {
                login: clean_of(x, "login").unwrap_or_default(),
            },
            "expired" => SignInStatus::Expired,
            "denied" => SignInStatus::Denied,
            "error" => SignInStatus::Error {
                message: clean_of(x, "message").unwrap_or_default(),
            },
            _ => return None,
        })
    }
}

/// A device-code sign-in to the relay's account (`account.login.start`).
#[derive(Debug, Clone, PartialEq)]
pub struct SignIn {
    pub id: String,
    pub uri: String,
    /// `uri` with the code filled in: the QR code and `c` carry it.
    pub uri_complete: String,
    pub code: String,
    pub expires_at: Instant,
    pub status: SignInStatus,
    /// The scope picked before signing in: its pairing link follows. None from the list (`s`).
    pub scope_after: Option<String>,
    polled_at: Option<Instant>,
    inflight: Option<Instant>,
}

impl SignIn {
    fn from_value(x: &Value, scope_after: Option<String>) -> Option<SignIn> {
        let uri = clean_of(x, "verification_uri")?;
        Some(SignIn {
            id: s_of(x, "id")?,
            uri_complete: clean_of(x, "verification_uri_complete").unwrap_or_else(|| uri.clone()),
            uri,
            code: clean_of(x, "user_code")?,
            expires_at: Instant::now()
                + Duration::from_secs(
                    x["expires_in"]
                        .as_u64()
                        .unwrap_or(DEFAULT_EXPIRES_IN)
                        .min(MAX_EXPIRES_IN),
                ),
            status: SignInStatus::Pending,
            scope_after,
            polled_at: None,
            inflight: None,
        })
    }
}

/// What `account.status` says about the relay's account.
#[derive(Debug, Clone, PartialEq)]
pub struct Account {
    /// False for open and self-hosted relays: no account line, no `s`.
    pub needs_account: bool,
    pub logged_in: bool,
    pub login: Option<String>,
}

impl Account {
    fn from_value(x: &Value) -> Account {
        Account {
            needs_account: x["needs_account"].as_bool().unwrap_or(false),
            logged_in: x["logged_in"].as_bool().unwrap_or(false),
            login: clean_of(x, "login"),
        }
    }

    fn sign_in_needed(&self) -> bool {
        self.needs_account && !self.logged_in
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    List,
    PickScope { sel: usize },
    Pairing(Pairing),
    SignIn(SignIn),
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
    /// `account.status`, when the gateway answered it.
    pub account: Option<Account>,
    /// `gateway.status`, when the server answered it (older servers don't).
    pub gw: Option<GwHealth>,
    /// When `gateway.status` was last asked while the pairing link can't be used.
    gw_asked: Option<Instant>,
    /// The `pair.create` (or the `account.status` / `account.login.start` before it) this view
    /// waits for; any other answer is an abandoned attempt.
    creating: Option<u64>,
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
            account: None,
            gw: None,
            gw_asked: None,
            creating: None,
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
    Create {
        attempt: u64,
    },
    Status {
        pid: String,
    },
    /// `share.revoke` of an abandoned link or `account.login.cancel`: nothing to show.
    Cancel,
    /// `account.status` on opening: the account line.
    AccountInfo,
    /// `account.status` before creating a link for `scope`.
    AccountCheck {
        attempt: u64,
        scope: String,
    },
    LoginStart {
        attempt: u64,
        scope_after: Option<String>,
    },
    LoginStatus {
        id: String,
    },
    /// `gateway.status`: is the gateway online?
    GatewayInfo,
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
    gw(app, mi, "account.status", json!({}), Reply::AccountInfo);
    ask_gateway_status(app, mi);
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
    let Some(v) = view_mut(app) else {
        return;
    };
    let mi = v.mi;
    // While a link is up, keep checking the gateway: a link that can't be used yet waits for it
    // to come back, a usable one is hidden as soon as the relay drops.
    let every = if link_blocked(v).is_some() {
        2 * POLL
    } else {
        5 * POLL
    };
    if matches!(v.stage, Stage::Pairing(_))
        && v.gw_asked.is_none_or(|t| now.duration_since(t) >= every)
    {
        v.gw_asked = Some(now);
        ask_gateway_status(app, mi);
        return;
    }
    // Expiry is the gateway's call (`gone`, `expired`): its clock, and a pairing or sign-in may
    // finish at the last second.
    let (polled_at, inflight, method, params, reply) = match &mut v.stage {
        Stage::Pairing(p) if p.status != PairStatus::Expired => (
            &mut p.polled_at,
            &mut p.inflight,
            "pair.status",
            json!({"pid": p.pid}),
            Reply::Status { pid: p.pid.clone() },
        ),
        Stage::SignIn(s) if s.status == SignInStatus::Pending => (
            &mut s.polled_at,
            &mut s.inflight,
            "account.login.status",
            json!({"id": s.id}),
            Reply::LoginStatus { id: s.id.clone() },
        ),
        _ => return,
    };
    if inflight.is_some_and(|t| now.duration_since(t) < STALE)
        || polled_at.is_some_and(|t| now.duration_since(t) < POLL)
    {
        return;
    }
    *polled_at = Some(now);
    *inflight = Some(now);
    gw(app, mi, method, params, reply);
}

/// (polled_at, inflight) of the stage that polls, if any.
fn polling(stage: &Stage) -> Option<(Option<Instant>, Option<Instant>)> {
    match stage {
        Stage::Pairing(p) if p.status != PairStatus::Expired => Some((p.polled_at, p.inflight)),
        Stage::SignIn(s) if s.status == SignInStatus::Pending => Some((s.polled_at, s.inflight)),
        _ => None,
    }
}

pub fn deadlines(app: &App, now: Instant, d: &mut crate::deadline::Deadlines) {
    if let (Mode::Popup(Popup::Devices), Some(v)) = (&app.mode, &app.ux.devices)
        && let Some((polled_at, inflight)) = polling(&v.stage)
    {
        let next = polled_at.map_or(now, |t| t + POLL);
        d.at(
            "devices.poll",
            inflight.map_or(next, |t| next.max(t + STALE)),
        );
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

/// Why the pairing link can't be used right now: the gateway is unreachable, or the server says
/// it is not online.
fn link_blocked(v: &View) -> Option<String> {
    if v.error.is_some() {
        return Some(
            "The gateway is unreachable: a phone can't use a link until it's back — run `vibeke gateway on`"
                .into(),
        );
    }
    v.gw.as_ref().and_then(GwHealth::problem)
}

/// Ask the server for `gateway.status` (a server method, not bridged to the gateway).
fn ask_gateway_status(app: &mut App, mi: usize) {
    app.command_on(
        mi,
        "gateway.status",
        json!({}),
        Pending::Ux(crate::ux::Reply::Devices(Reply::GatewayInfo)),
    );
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
    let attempt = NEXT_ATTEMPT.fetch_add(1, Ordering::Relaxed);
    v.busy = Some("creating a pairing link…".into());
    v.notice = None;
    v.creating = Some(attempt);
    let mi = v.mi;
    gw(
        app,
        mi,
        "pair.create",
        json!({"scope": scope}),
        Reply::Create { attempt },
    );
}

/// Enter on a scope: a relay that needs an account nobody signed in to signs in first.
fn confirm_scope(app: &mut App, scope: &str) {
    let Some(v) = view_mut(app) else {
        return;
    };
    let attempt = NEXT_ATTEMPT.fetch_add(1, Ordering::Relaxed);
    v.busy = Some("checking the relay account…".into());
    v.notice = None;
    v.creating = Some(attempt);
    let mi = v.mi;
    gw(
        app,
        mi,
        "account.status",
        json!({}),
        Reply::AccountCheck {
            attempt,
            scope: scope.into(),
        },
    );
}

/// `account.login.start`; the gateway hands back the same login while one is pending.
fn start_login(app: &mut App, scope_after: Option<String>) {
    let Some(v) = view_mut(app) else {
        return;
    };
    let attempt = NEXT_ATTEMPT.fetch_add(1, Ordering::Relaxed);
    v.busy = Some("starting the sign-in…".into());
    v.notice = None;
    v.creating = Some(attempt);
    let mi = v.mi;
    gw(
        app,
        mi,
        "account.login.start",
        json!({}),
        Reply::LoginStart {
            attempt,
            scope_after,
        },
    );
}

fn scope_index(scope: &str) -> usize {
    SCOPES.iter().position(|s| s.0 == scope).unwrap_or(0)
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
    let blocked = link_blocked(v).is_some();
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
                Key::Char('s')
                    if plain && v.account.as_ref().is_some_and(Account::sign_in_needed) =>
                {
                    start_login(app, None)
                }
                Key::Char('r' | 'g') if plain => refresh(app),
                _ => {}
            }
        }
        Stage::PickScope { sel } => {
            if quit {
                v.busy = None;
                v.creating = None;
                v.stage = Stage::List;
            } else if v.busy.is_some() {
            } else if down {
                *sel = (*sel + 1).min(SCOPES.len() - 1);
            } else if up {
                *sel = sel.saturating_sub(1);
            } else if enter {
                let scope = SCOPES[*sel].0;
                confirm_scope(app, scope);
            }
        }
        Stage::Pairing(p) => {
            if quit {
                // Even when it looks expired: a no-op then, and the gateway's clock decides.
                let pid = p.pid.clone();
                let mi = v.mi;
                v.stage = Stage::List;
                cancel_link(app, mi, &pid);
            } else if plain && matches!(ev.key, Key::Char('c')) {
                if !blocked {
                    let link = p.link.clone();
                    app.copy_text(&link);
                }
            } else if plain && matches!(ev.key, Key::Char('n')) && p.status == PairStatus::Expired {
                v.stage = Stage::PickScope { sel: 0 };
            }
        }
        Stage::SignIn(s) => {
            if quit {
                let id = s.id.clone();
                let mi = v.mi;
                v.stage = Stage::List;
                gw(
                    app,
                    mi,
                    "account.login.cancel",
                    json!({"id": id}),
                    Reply::Cancel,
                );
            } else if plain && matches!(ev.key, Key::Char('c')) {
                let link = s.uri_complete.clone();
                app.copy_text(&link);
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

/// A failed sign-in call: always the notice (`unavailable` is the account server, not the
/// gateway).
fn sign_in_failed(v: &mut View, e: &RpcErr) {
    v.notice = Some(format!("✗ {}", clean(&bridge_error(e))));
}

/// The gateway (or the server's bridge) predates `account.*`: pair as before.
fn predates_accounts(e: &RpcErr) -> bool {
    // An older gateway bridge refuses unknown methods as `forbidden` (`permission_denied` here).
    e.is_method_not_found()
        || e.kind == "permission_denied"
        || (e.kind == "invalid_params" && e.message.contains("does not carry"))
}

/// `account.login.start` refused with reason `not_needed` (the relay is open after all).
fn not_needed(e: &RpcErr) -> bool {
    ["/reason", "/details/reason"]
        .iter()
        .any(|p| e.details.pointer(p).and_then(Value::as_str) == Some("not_needed"))
}

pub fn on_reply(app: &mut App, mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    app.dirty = true;
    // An answer for an attempt nobody waits for any more (backed out, closed, a newer attempt) is
    // dropped, and a link made for it must not stay usable. A late `account.login.start` is left
    // alone: the gateway hands the same pending login to the next attempt.
    let attempt = match &r {
        Reply::Create { attempt }
        | Reply::AccountCheck { attempt, .. }
        | Reply::LoginStart { attempt, .. } => Some(*attempt),
        _ => None,
    };
    if let Some(attempt) = attempt
        && !app
            .ux
            .devices
            .as_ref()
            .is_some_and(|v| v.mi == mi && v.creating == Some(attempt))
    {
        if let Reply::Create { .. } = r
            && let Ok(x) = &res
            && let Some(pid) = s_of(x, "pid")
        {
            cancel_link(app, mi, &pid);
        }
        return;
    }
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
        Reply::Create { .. } => {
            v.busy = None;
            v.creating = None;
            match res {
                Ok(x) => {
                    let pid = s_of(&x, "pid").unwrap_or_default();
                    match s_of(&x, "link") {
                        Some(link) if !pid.is_empty() => {
                            // "Signed in as …" stays up while the phone pairs.
                            v.notice = v.notice.take().filter(|n| n.starts_with("Signed in"));
                            v.gw_asked = Some(Instant::now());
                            v.stage = Stage::Pairing(Pairing {
                                pid,
                                link,
                                open_by: x["open_by"].as_i64().unwrap_or(0),
                                status: PairStatus::Pending,
                                scope: s_of(&x, "scope").unwrap_or_default().to_lowercase(),
                                polled_at: None,
                                inflight: None,
                            });
                            ask_gateway_status(app, mi);
                        }
                        _ => v.notice = Some("✗ the gateway returned no link".into()),
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
            p.inflight = None;
            match res {
                Ok(x) => {
                    // The gateway answered: the unreachable banner no longer holds.
                    v.error = None;
                    match x["status"].as_str() {
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
                    }
                }
                // Keep asking: the next poll may get through.
                Err(e) => failed(v, &e),
            }
        }
        Reply::Cancel => {}
        Reply::GatewayInfo => {
            // An older server doesn't know the method: behave as before.
            if let Ok(x) = res {
                v.gw = Some(GwHealth::from_value(&x));
            }
        }
        Reply::AccountInfo => {
            // An older gateway doesn't know the method: no account line.
            if let Ok(x) = res {
                v.account = Some(Account::from_value(&x));
            }
        }
        Reply::AccountCheck { scope, .. } => match res {
            Ok(x) => {
                let acc = Account::from_value(&x);
                let sign_in = acc.sign_in_needed();
                v.account = Some(acc);
                if sign_in {
                    start_login(app, Some(scope));
                } else {
                    create(app, &scope);
                }
            }
            Err(e) if predates_accounts(&e) => create(app, &scope),
            Err(e) => {
                v.busy = None;
                v.creating = None;
                failed(v, &e);
            }
        },
        Reply::LoginStart { scope_after, .. } => {
            v.busy = None;
            v.creating = None;
            match res {
                Ok(x) => match SignIn::from_value(&x, scope_after) {
                    Some(s) => {
                        v.notice = None;
                        v.stage = Stage::SignIn(s);
                    }
                    None => v.notice = Some("✗ the gateway returned no sign-in code".into()),
                },
                Err(e) if not_needed(&e) => {
                    if let Some(acc) = v.account.as_mut() {
                        acc.needs_account = false;
                    }
                    if let Some(scope) = scope_after {
                        create(app, &scope);
                    }
                }
                Err(e) => sign_in_failed(v, &e),
            }
        }
        Reply::LoginStatus { id } => {
            let Stage::SignIn(s) = &mut v.stage else {
                return;
            };
            if s.id != id {
                return;
            }
            s.inflight = None;
            let status = match res {
                Ok(x) => match SignInStatus::from_value(&x) {
                    Some(st) => st,
                    None => return,
                },
                // The gateway no longer knows this login (restarted): as good as expired.
                Err(e) if e.kind == "not_found" => SignInStatus::Expired,
                // Keep asking: the next poll may get through.
                Err(e) => {
                    sign_in_failed(v, &e);
                    return;
                }
            };
            s.status = status.clone();
            let scope_after = s.scope_after.clone();
            match status {
                SignInStatus::Pending => {}
                SignInStatus::Done { login } => {
                    v.account = Some(Account {
                        needs_account: true,
                        logged_in: true,
                        login: Some(login.clone()).filter(|l| !l.is_empty()),
                    });
                    let notice = if login.is_empty() {
                        "Signed in".to_string()
                    } else {
                        format!("Signed in as {login}")
                    };
                    match scope_after {
                        Some(scope) => {
                            v.stage = Stage::PickScope {
                                sel: scope_index(&scope),
                            };
                            create(app, &scope);
                            if let Some(v) = view_mut(app) {
                                v.notice = Some(notice);
                            }
                        }
                        None => {
                            v.stage = Stage::List;
                            v.notice = Some(notice);
                        }
                    }
                }
                SignInStatus::Expired => match scope_after {
                    Some(scope) => {
                        v.stage = Stage::PickScope {
                            sel: scope_index(&scope),
                        };
                        v.notice =
                            Some("✗ the sign-in code expired: press enter for a new one".into());
                    }
                    None => {
                        v.stage = Stage::List;
                        v.notice = Some("✗ the sign-in code expired: press s for a new one".into());
                    }
                },
                SignInStatus::Denied => {
                    v.stage = Stage::List;
                    v.notice = Some("✗ the sign-in was denied".into());
                }
                SignInStatus::Error { message } => {
                    v.stage = Stage::List;
                    v.notice = Some(if message.is_empty() {
                        "✗ the sign-in failed".into()
                    } else {
                        format!("✗ the sign-in failed: {message}")
                    });
                }
            }
        }
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
        Stage::SignIn(s) => draw_sign_in(app, &mut a, v, s),
    }
}

/// The notice (red when it is a failure), then the busy line.
fn status_line(app: &App, a: &mut crate::drafts::Area<'_>, v: &View) {
    let t = app.theme;
    if let Some(n) = &v.notice {
        a.line(
            n,
            t.s(if n.starts_with('✗') {
                t.red
            } else {
                t.yellow
            }),
        );
    }
    if let Some(b) = &v.busy {
        a.line(&format!("⏳ {b}"), t.s(t.yellow));
    }
}

fn draw_list(app: &App, a: &mut crate::drafts::Area<'_>, v: &View) {
    let t = app.theme;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    a.line("Your paired phones and apps", t.bold(t.fg));
    let sign_in = v.account.as_ref().is_some_and(Account::sign_in_needed);
    if let Some(acc) = v.account.as_ref().filter(|acc| acc.needs_account) {
        if acc.logged_in {
            a.line(
                &format!("Account: {}", acc.login.as_deref().unwrap_or("signed in")),
                t.dim(),
            );
        } else {
            a.line("Account: sign in required (s)", t.s(t.yellow));
        }
    }
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
        if sign_in {
            "j/k move · n pair a phone · s sign in · x revoke · r refresh · esc"
        } else {
            "j/k move · n pair a phone · x revoke · r refresh · esc"
        },
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
                    "'{who}'{plat} is asking to pair — press y to pair or n to reject in the Confirm prompt, fingerprint {}",
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
        a.line(n, t.s(if n.starts_with('✗') { t.red } else { t.green }));
    }
    a.line("", t.text());
    let blocked = link_blocked(v);
    if p.status != PairStatus::Expired {
        match &blocked {
            Some(why) => wrap_lines(a, why, t.bold(t.yellow)),
            None => draw_link_qr(app, a, &p.link),
        }
    }
    a.footer(
        if p.status == PairStatus::Expired {
            "n new link · esc back"
        } else if blocked.is_some() {
            "esc cancel"
        } else {
            "c copy link · esc cancel"
        },
        t.dim(),
    );
}

fn draw_sign_in(app: &App, a: &mut crate::drafts::Area<'_>, v: &View, s: &SignIn) {
    let t = app.theme;
    let left = s
        .expires_at
        .saturating_duration_since(Instant::now())
        .as_secs() as i64;
    a.line("Sign in to the relay account", t.bold(t.fg));
    wrap_lines(
        a,
        &format!("Open {} on any device and enter the code", s.uri),
        t.text(),
    );
    a.line("", t.text());
    a.line(&format!("    {}", s.code), t.bold(t.accent));
    a.line("", t.text());
    a.line(
        &format!(
            "Waiting for the sign-in… the code works for {}",
            countdown(left)
        ),
        t.s(t.yellow),
    );
    if s.scope_after.is_some() {
        a.line("The pairing link follows once you're signed in", t.dim());
    }
    status_line(app, a, v);
    a.line("", t.text());
    draw_link_qr(app, a, &s.uri_complete);
    a.footer("c copy link · esc cancel", t.dim());
}

#[cfg(test)]
#[path = "devices_tests.rs"]
mod tests;
