//! Cloud sandboxes in the TUI (spec 17 §8): sending the focused pane to a hosted sandbox and
//! bringing one back. One popup, [`Popup::CloudSend`], with these stages:
//!
//! - **Send** (`cloud_send`, "Send to cloud…"): pick a provider (`cloud.providers`: the account
//!   each one is signed in with), sign in when needed, pick the task's existing sandbox or a new
//!   one (`cloud.box.list`), confirm, then follow the job (`cloud.move`, `cloud.job` events).
//! - **Bring back** (`cloud_bring_back`, "Bring back from cloud…"): pick this host or a paired host
//!   (`handoff.peers`), then follow the job (`cloud.move {to: {kind: "local" | "peer"}}`).
//! - **Auth**: renders the `methods` the server sends. A pasted token goes in a masked field (one
//!   `•` per character; a bracketed paste works), `import` rows call `cloud.auth.import`, an `env`
//!   row is a note. `o` (or `ctrl+o` in the token field) opens the token page. The token leaves
//!   this module only in the `cloud.auth.set` request and is cleared once that is sent.
//!
//! Any reply error with `details.reason == "needs_auth"` jumps to the Auth stage for that
//! provider and, after signing in, repeats the original call once.
//!
//! The tab bar shows `☁ <provider> <state>` while a job runs ([`status`]). The sandboxes overview
//! ([`crate::sandboxes`]) shares the Auth stage ([`AuthState`]).

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use unicode_width::UnicodeWidthStr;
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::devices::clean;
use crate::drafts::Area;
use crate::handoff::Peer;
use crate::inbox::fmt_age;
use crate::sandboxes::BoxRow;
use crate::screen::Grid;

/// How often `cloud.jobs` is asked while a job runs on a server that does not push events.
const POLL: Duration = Duration::from_secs(3);

static NEXT_OP: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_op() -> u64 {
    NEXT_OP.fetch_add(1, Ordering::Relaxed)
}

pub(crate) fn s_of(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(clean)
        .filter(|s| !s.is_empty())
}

/// Unix seconds or epoch ms from a number (strings are not understood: 0).
pub(crate) fn time_of(v: &Value, k: &str) -> i64 {
    let t = v
        .get(k)
        .and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64)))
        .unwrap_or(0);
    crate::handoff::epoch_ms(t)
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// What an error shows: its message and the structured reason.
pub(crate) fn err_text(e: &RpcErr) -> String {
    if e.is_method_not_found() {
        return "this server does not support cloud sandboxes: update Vibeke there".into();
    }
    let msg = clean(&e.message);
    match e.reason() {
        Some(r) if !msg.contains(r) => format!("{msg} ({})", clean(r)),
        _ => msg,
    }
}

// ---- auth ------------------------------------------------------------------------------------

/// One way to sign in to a provider (`AuthMethod` of `cloud.providers`).
#[derive(Debug, Clone, PartialEq)]
pub enum AuthMethod {
    PasteToken {
        label: String,
        help_url: Option<String>,
        hint: Option<String>,
    },
    Import {
        source: String,
        label: String,
    },
    Env {
        var: String,
    },
}

impl AuthMethod {
    pub fn from_value(v: &Value) -> Option<AuthMethod> {
        match v.get("kind")?.as_str()? {
            "paste_token" => Some(AuthMethod::PasteToken {
                label: s_of(v, "label").unwrap_or_else(|| "Paste a token".into()),
                help_url: s_of(v, "help_url"),
                hint: s_of(v, "hint"),
            }),
            "import" => {
                let source = s_of(v, "source")?;
                let label = s_of(v, "label").unwrap_or_else(|| format!("Import from {source}"));
                Some(AuthMethod::Import { source, label })
            }
            "env" => Some(AuthMethod::Env {
                var: s_of(v, "var")?,
            }),
            _ => None,
        }
    }

    fn selectable(&self) -> bool {
        !matches!(self, AuthMethod::Env { .. })
    }
}

/// The methods in a JSON array; unknown kinds are skipped.
pub fn methods_of(v: &Value) -> Vec<AuthMethod> {
    v.as_array()
        .map(|a| a.iter().filter_map(AuthMethod::from_value).collect())
        .unwrap_or_default()
}

/// `n` bullets for `token`, at most `room`: the field never shows the token.
pub fn masked(token: &str, room: usize) -> String {
    "•".repeat(token.chars().count().min(room))
}

/// What a key in the Auth stage asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthOut {
    None,
    Cancel,
    SetToken(String),
    Import(String),
    Open(String),
}

/// The Auth stage: the provider's methods, the masked token field and what is in flight.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthState {
    pub provider: String,
    pub label: String,
    pub methods: Vec<AuthMethod>,
    pub sel: usize,
    pub token: String,
    pub busy: Option<String>,
    pub error: Option<String>,
}

impl AuthState {
    pub fn new(provider: &str, label: &str, methods: Vec<AuthMethod>) -> AuthState {
        let sel = methods.iter().position(AuthMethod::selectable).unwrap_or(0);
        AuthState {
            provider: provider.to_string(),
            label: label.to_string(),
            methods,
            sel,
            token: String::new(),
            busy: None,
            error: None,
        }
    }

    pub fn on_token_row(&self) -> bool {
        matches!(
            self.methods.get(self.sel),
            Some(AuthMethod::PasteToken { .. })
        )
    }

    /// The first token page the methods name.
    pub fn help_url(&self) -> Option<String> {
        self.methods.iter().find_map(|m| match m {
            AuthMethod::PasteToken { help_url, .. } => help_url.clone(),
            _ => None,
        })
    }

    fn step(&mut self, down: bool) {
        let n = self.methods.len();
        for k in 1..=n {
            let i = if down {
                (self.sel + k) % n
            } else {
                (self.sel + n - k % n) % n
            };
            if self.methods[i].selectable() {
                self.sel = i;
                return;
            }
        }
    }

    pub fn key(&mut self, ev: &KeyEvent) -> AuthOut {
        if ev.kind == KeyKind::Release {
            return AuthOut::None;
        }
        let esc = matches!(ev.key, Key::Named(NamedKey::Escape));
        if self.busy.is_some() {
            return if esc { AuthOut::Cancel } else { AuthOut::None };
        }
        let plain = !ev.mods.ctrl() && !ev.mods.alt();
        let on_token = self.on_token_row();
        match ev.key {
            _ if esc => AuthOut::Cancel,
            Key::Named(NamedKey::Tab) if ev.mods.shift() => {
                self.step(false);
                AuthOut::None
            }
            Key::Named(NamedKey::Down | NamedKey::Tab) => {
                self.step(true);
                AuthOut::None
            }
            Key::Named(NamedKey::Up) => {
                self.step(false);
                AuthOut::None
            }
            Key::Named(NamedKey::Enter) => match self.methods.get(self.sel) {
                Some(AuthMethod::PasteToken { .. }) => {
                    let t = self.token.trim().to_string();
                    if t.is_empty() {
                        self.error = Some("paste a token first".into());
                        AuthOut::None
                    } else {
                        self.error = None;
                        AuthOut::SetToken(t)
                    }
                }
                Some(AuthMethod::Import { source, .. }) => {
                    self.error = None;
                    AuthOut::Import(source.clone())
                }
                _ => AuthOut::None,
            },
            Key::Named(NamedKey::Backspace) if on_token => {
                self.token.pop();
                self.error = None;
                AuthOut::None
            }
            Key::Char('u') if ev.mods.ctrl() && on_token => {
                self.token.clear();
                self.error = None;
                AuthOut::None
            }
            Key::Char('o') if ev.mods.ctrl() || (plain && !on_token) => match self.help_url() {
                Some(u) => AuthOut::Open(u),
                None => AuthOut::None,
            },
            Key::Char('j') if plain && !on_token => {
                self.step(true);
                AuthOut::None
            }
            Key::Char('k') if plain && !on_token => {
                self.step(false);
                AuthOut::None
            }
            Key::Char(c) if plain && on_token && !c.is_control() && !c.is_whitespace() => {
                self.token.push(c);
                self.error = None;
                AuthOut::None
            }
            _ => AuthOut::None,
        }
    }

    /// A bracketed paste lands in the token field (selecting it first).
    pub fn paste(&mut self, text: &str) {
        if self.busy.is_some() {
            return;
        }
        if !self.on_token_row() {
            match self
                .methods
                .iter()
                .position(|m| matches!(m, AuthMethod::PasteToken { .. }))
            {
                Some(i) => self.sel = i,
                None => return,
            }
        }
        self.token.extend(
            text.chars()
                .filter(|c| !c.is_control() && !c.is_whitespace()),
        );
        self.error = None;
    }

    /// The Auth stage; the cursor when the token field has the keys.
    pub(crate) fn draw(&self, app: &App, a: &mut Area<'_>) -> Option<(u16, u16)> {
        let t = app.theme;
        a.line(&format!("Sign in to {}", self.label), t.bold(t.fg));
        a.line("", t.text());
        let mut cursor = None;
        for (i, m) in self.methods.iter().enumerate() {
            let on = i == self.sel;
            let mark = if on { "›" } else { " " };
            let st = if on { t.sel(t.fg) } else { t.text() };
            match m {
                AuthMethod::PasteToken { label, hint, .. } => {
                    let prefix = format!("{mark} {label}: ");
                    let room = (a.rest().w as usize)
                        .saturating_sub(UnicodeWidthStr::width(prefix.as_str()) + 1);
                    let dots = masked(&self.token, room);
                    if on && self.busy.is_none() {
                        cursor = Some((
                            a.r.x
                                + 1
                                + (UnicodeWidthStr::width(prefix.as_str())
                                    + UnicodeWidthStr::width(dots.as_str()))
                                    as u16,
                            a.y,
                        ));
                    }
                    a.line(&format!("{prefix}{dots}"), st);
                    if on && let Some(h) = hint {
                        a.line(&format!("    {h}"), t.dim());
                    }
                }
                AuthMethod::Import { label, source } => {
                    a.line(&format!("{mark} {label}  (from {source})"), st);
                }
                AuthMethod::Env { var } => {
                    a.line(
                        &format!("  Or set {var} in the environment of the Vibeke server."),
                        t.dim(),
                    );
                }
            }
        }
        a.line("", t.text());
        if let Some(b) = &self.busy {
            a.line(&format!("⏳ {b}"), t.s(t.yellow));
        }
        if let Some(e) = &self.error {
            crate::devices::wrap_lines(a, &format!("✗ {e}"), t.s(t.red));
        }
        let open = if self.help_url().is_some() {
            if self.on_token_row() {
                " · ctrl+o open the token page"
            } else {
                " · o open the token page"
            }
        } else {
            ""
        };
        a.footer(
            &format!("↑/↓ choose · enter sign in{open} · esc back"),
            t.dim(),
        );
        cursor
    }
}

/// The request for an Auth-stage action: (method, params). The token is only in the params.
pub(crate) fn auth_request(provider: &str, out: &AuthOut) -> Option<(&'static str, Value)> {
    match out {
        AuthOut::SetToken(t) => Some(("cloud.auth.set", json!({"provider": provider, "token": t}))),
        AuthOut::Import(s) => Some((
            "cloud.auth.import",
            json!({"provider": provider, "source": s}),
        )),
        _ => None,
    }
}

// ---- providers -------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderRow {
    pub id: String,
    pub label: String,
    pub default: bool,
    /// `missing`, `ok` or `invalid`.
    pub state: String,
    pub source: Option<String>,
    pub account: Option<String>,
    pub methods: Vec<AuthMethod>,
    pub caps: Value,
}

impl ProviderRow {
    pub fn from_value(v: &Value) -> Option<ProviderRow> {
        let id = s_of(v, "id")?;
        let auth = v.get("auth").cloned().unwrap_or(Value::Null);
        Some(ProviderRow {
            label: s_of(v, "label").unwrap_or_else(|| id.clone()),
            id,
            default: v["default"].as_bool().unwrap_or(false),
            state: s_of(&auth, "state").unwrap_or_else(|| "missing".into()),
            source: s_of(&auth, "source"),
            account: s_of(&auth, "account"),
            methods: methods_of(&v["methods"]),
            caps: v.get("caps").cloned().unwrap_or(Value::Null),
        })
    }

    pub fn signed_in(&self) -> bool {
        self.state == "ok"
    }

    /// "signed in as acme", "not signed in", "token rejected".
    pub fn auth_line(&self) -> String {
        match self.state.as_str() {
            "ok" => {
                let mut s = match &self.account {
                    Some(a) => format!("signed in as {a}"),
                    None => "signed in".to_string(),
                };
                if let Some(src) = self.source.as_deref().filter(|s| s.starts_with("env")) {
                    s.push_str(&format!(" ({src})"));
                }
                s
            }
            "invalid" => "token rejected: sign in again".into(),
            _ => "not signed in".into(),
        }
    }

    /// Applies `cloud.auth.changed`'s `{state, account}`.
    pub fn apply_auth(&mut self, data: &Value) {
        if let Some(s) = s_of(data, "state") {
            self.state = s;
        }
        self.account = s_of(data, "account");
    }
}

/// Providers from a `cloud.providers` result.
pub fn providers_of(v: &Value) -> Vec<ProviderRow> {
    v["providers"]
        .as_array()
        .map(|a| a.iter().filter_map(ProviderRow::from_value).collect())
        .unwrap_or_default()
}

// ---- jobs ------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default)]
pub struct JobError {
    pub kind: String,
    pub message: String,
    pub details: Value,
}

impl JobError {
    fn from_value(v: &Value) -> JobError {
        JobError {
            kind: s_of(v, "kind").unwrap_or_default(),
            message: s_of(v, "message").unwrap_or_else(|| "the move failed".into()),
            details: v.get("details").cloned().unwrap_or(Value::Null),
        }
    }

    pub fn reason(&self) -> Option<&str> {
        self.details.get("reason").and_then(Value::as_str)
    }

    /// The error with what its details add: the reason and an unsynced summary.
    pub fn text(&self) -> String {
        let mut s = self.message.clone();
        if let Some(r) = self.reason()
            && !s.contains(r)
        {
            s.push_str(&format!(" ({})", clean(r)));
        }
        if let Some(sum) = self
            .details
            .pointer("/unsynced/summary")
            .or_else(|| self.details.get("summary"))
            .and_then(Value::as_str)
        {
            s.push_str(&format!(": {}", clean(sum)));
        }
        s
    }
}

/// One move of a pane between this host, a sandbox and a paired host (`Job` of `cloud.move`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Job {
    pub id: String,
    /// `send` or `bring_back`.
    pub direction: String,
    pub pane: Option<String>,
    pub box_id: Option<String>,
    pub from: Value,
    pub to: Value,
    pub state: String,
    pub done: u64,
    pub total: u64,
    pub error: Option<JobError>,
    pub result: Value,
    pub updated_at: i64,
}

impl Job {
    pub fn from_value(v: &Value) -> Option<Job> {
        Some(Job {
            id: s_of(v, "id")?,
            direction: s_of(v, "direction").unwrap_or_default(),
            pane: s_of(v, "pane"),
            box_id: s_of(v, "box"),
            from: v.get("from").cloned().unwrap_or(Value::Null),
            to: v.get("to").cloned().unwrap_or(Value::Null),
            state: s_of(v, "state").unwrap_or_else(|| "queued".into()),
            done: v
                .pointer("/progress/done")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            total: v
                .pointer("/progress/total")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            error: v
                .get("error")
                .filter(|e| !e.is_null())
                .map(JobError::from_value),
            result: v.get("result").cloned().unwrap_or(Value::Null),
            updated_at: time_of(v, "updated_at"),
        })
    }

    pub fn active(&self) -> bool {
        !matches!(self.state.as_str(), "done" | "failed" | "cancelled")
    }

    /// The cloud end's provider: where it goes (send) or where it comes from (bring back).
    pub fn provider(&self) -> String {
        for side in [&self.to, &self.from] {
            if side["kind"] == "cloud"
                && let Some(p) = s_of(side, "provider")
            {
                return p;
            }
        }
        "cloud".into()
    }

    pub fn percent(&self) -> Option<u64> {
        (self.done.saturating_mul(100))
            .checked_div(self.total)
            .map(|p| p.min(100))
    }

    /// The state in a sentence.
    pub fn state_text(&self) -> String {
        match self.state.as_str() {
            "queued" => "queued".into(),
            "waiting_turn" => "waiting for the agent's turn to end".into(),
            "creating" => "creating the sandbox".into(),
            "bootstrapping" => "setting up Vibeke in the sandbox".into(),
            "exporting" => "exporting the work".into(),
            "uploading" => "uploading".into(),
            "importing" => "importing".into(),
            "resuming" => "resuming the agent".into(),
            "done" => "done".into(),
            "failed" => "failed".into(),
            "cancelled" => "cancelled".into(),
            other => other.replace('_', " "),
        }
    }

    /// The tab bar's text: "☁ sprites creating 42%".
    pub fn status(&self) -> String {
        let mut s = format!("☁ {} {}", self.provider(), self.state.replace('_', " "));
        if let Some(p) = self.percent().filter(|_| self.total > 0) {
            s.push_str(&format!(" {p}%"));
        }
        s
    }
}

/// A progress bar of `w` cells.
fn bar(pct: u64, w: usize) -> String {
    let full = (pct.min(100) as usize * w) / 100;
    format!("[{}{}] {pct}%", "█".repeat(full), "░".repeat(w - full))
}

// ---- state -----------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Send,
    BringBack,
}

/// What a flow moves.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Pane(String),
    Box(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    PickProvider { sel: usize },
    Auth(AuthState),
    PickBox { sel: usize },
    Confirm,
    PickTarget { sel: usize },
    Sending,
}

/// The call to repeat once after signing in.
#[derive(Debug, Clone)]
pub struct Retry {
    pub method: String,
    pub params: Value,
    pub call: Call,
}

#[derive(Debug, Clone)]
pub struct Flow {
    pub op: u64,
    pub mi: usize,
    pub kind: Kind,
    pub src: Source,
    /// What the title names: "claude in api (w1:p1)".
    pub label: String,
    /// Ids and handles of the task of the pane: the boxes that belong to it.
    pub task_keys: Vec<String>,
    pub providers: Vec<ProviderRow>,
    pub provider: Option<String>,
    pub boxes: Vec<BoxRow>,
    /// The chosen sandbox (`provider/id`); none is a new one.
    pub box_sel: Option<String>,
    pub peers: Vec<Peer>,
    pub interrupt: bool,
    pub stage: Stage,
    /// A list is being read.
    pub loading: bool,
    /// `cloud.move` is on its way.
    pub busy: Option<String>,
    pub error: Option<String>,
    pub retry: Option<Retry>,
    /// The call in flight is a retry after signing in: a second `needs_auth` is an error.
    pub retried: bool,
    pub job: Option<Job>,
    polled_at: Option<Instant>,
}

#[derive(Default)]
pub struct State {
    /// Moves per machine, newest first.
    pub jobs: BTreeMap<usize, Vec<Job>>,
    pub flow: Option<Flow>,
    /// Machines whose server has no `cloud.jobs`.
    pub unsupported: HashSet<usize>,
}

#[derive(Debug, Clone)]
pub enum Call {
    Providers,
    Boxes,
    Peers,
    Move,
    AuthSet,
    AuthImport,
    /// `cloud.jobs`, polled for the flow's job.
    Jobs,
    Cancel,
    /// `cloud.jobs` on connecting (no flow).
    AllJobs,
}

/// Replies routed back here (through `ux::Reply::Cloud`).
#[derive(Debug, Clone)]
pub struct Reply {
    /// The flow it was made for (a later flow ignores it).
    pub op: u64,
    pub call: Call,
}

fn send_call(app: &mut App, mi: usize, method: &str, params: Value, op: u64, call: Call) {
    app.command_on(
        mi,
        method,
        params,
        Pending::Ux(crate::ux::Reply::Cloud(Reply { op, call })),
    );
}

/// A call of the flow; it is kept to repeat after signing in.
fn flow_call(app: &mut App, method: &str, params: Value, call: Call) {
    let Some(f) = app.ux.cloud.flow.as_mut() else {
        return;
    };
    f.retry = Some(Retry {
        method: method.into(),
        params: params.clone(),
        call: call.clone(),
    });
    let (mi, op) = (f.mi, f.op);
    send_call(app, mi, method, params, op, call);
}

// ---- opening ---------------------------------------------------------------------------------

/// The ids and handles of the task the pane's workspace belongs to.
fn task_keys(app: &App, mi: usize, pane: &str) -> Vec<String> {
    let m = &app.machines[mi];
    let Some(ws) = m
        .model
        .panes
        .iter()
        .find(|p| p.id == pane)
        .and_then(|p| m.model.workspaces.iter().find(|w| w.id == p.workspace))
    else {
        return Vec::new();
    };
    let Some(task) = ws.task.as_ref() else {
        return Vec::new();
    };
    let mut v = vec![task.clone()];
    if let Some(t) = m.model.tasks.iter().find(|t| &t.id == task) {
        v.push(t.handle.clone());
        v.push(t.slug.clone());
    }
    v
}

fn open(app: &mut App, mi: usize, kind: Kind, src: Source, label: String) {
    if !app.machines[mi].connected() {
        app.toast(format!("{} offline", app.machines[mi].label));
        return;
    }
    let task_keys = match &src {
        Source::Pane(p) => task_keys(app, mi, p),
        Source::Box(_) => Vec::new(),
    };
    let op = next_op();
    app.ux.cloud.flow = Some(Flow {
        op,
        mi,
        kind,
        src,
        label,
        task_keys,
        providers: Vec::new(),
        provider: None,
        boxes: Vec::new(),
        box_sel: None,
        peers: Vec::new(),
        interrupt: false,
        stage: match kind {
            Kind::Send => Stage::PickProvider { sel: 0 },
            Kind::BringBack => Stage::PickTarget { sel: 0 },
        },
        loading: true,
        busy: None,
        error: None,
        retry: None,
        retried: false,
        job: None,
        polled_at: None,
    });
    app.mode = Mode::Popup(Popup::CloudSend);
    match kind {
        Kind::Send => flow_call(app, "cloud.providers", json!({}), Call::Providers),
        Kind::BringBack => send_call(app, mi, "handoff.peers", json!({}), op, Call::Peers),
    }
}

/// `cloud_send`: the focused (or right-clicked) pane goes to a sandbox.
pub fn open_send(app: &mut App) {
    let Some((mi, pane)) = crate::handoff::take_target(app) else {
        app.toast("no focused pane to send");
        return;
    };
    let label = crate::handoff::pane_label(app, mi, &pane);
    open(app, mi, Kind::Send, Source::Pane(pane), label);
}

/// `cloud_bring_back`: the focused (or right-clicked) cloud pane comes back.
pub fn open_bring_back(app: &mut App) {
    let Some((mi, pane)) = crate::handoff::take_target(app) else {
        app.toast("no focused pane to bring back");
        return;
    };
    let label = crate::handoff::pane_label(app, mi, &pane);
    open(app, mi, Kind::BringBack, Source::Pane(pane), label);
}

/// Bring back everything in sandbox `box_id` (`provider/id`), from the sandboxes overview.
pub(crate) fn open_bring_back_box(app: &mut App, mi: usize, box_id: &str, name: &str) {
    open(
        app,
        mi,
        Kind::BringBack,
        Source::Box(box_id.to_string()),
        format!("sandbox {name}"),
    );
}

pub fn action(app: &mut App, action: &str) -> bool {
    match action {
        "cloud_send" => open_send(app),
        "cloud_bring_back" => open_bring_back(app),
        _ => return false,
    }
    true
}

fn close(app: &mut App) {
    app.ux.cloud.flow = None;
    app.mode = Mode::Normal;
}

// ---- connecting and events -------------------------------------------------------------------

/// After (re)connecting: the moves in progress, for the tab bar.
pub fn on_connected(app: &mut App, mi: usize) {
    app.ux.cloud.unsupported.remove(&mi);
    if crate::push::supported(app, mi) {
        send_call(app, mi, "cloud.jobs", json!({}), 0, Call::AllJobs);
    }
}

fn upsert_job(app: &mut App, mi: usize, j: Job) {
    let jobs = app.ux.cloud.jobs.entry(mi).or_default();
    let before = match jobs.iter().position(|x| x.id == j.id) {
        Some(i) => Some(std::mem::replace(&mut jobs[i], j.clone())),
        None => {
            jobs.insert(0, j.clone());
            None
        }
    };
    jobs.truncate(50);
    if before.as_ref().is_none_or(|b| b.state != j.state) && !j.active() {
        let msg = match &j.error {
            Some(e) => format!("☁ {} failed: {}", j.provider(), e.text()),
            None => format!("☁ {} {}", j.provider(), j.state_text()),
        };
        app.toast(msg);
    }
    let own = app
        .ux
        .cloud
        .flow
        .as_ref()
        .is_some_and(|f| f.mi == mi && f.job.as_ref().is_some_and(|x| x.id == j.id));
    if own {
        flow_job(app, j);
    }
    app.dirty = true;
}

/// The flow's job changed. A job that failed for lack of a sign-in goes to the Auth stage, once
/// per move the user started; any other end of the job closes that move's retry.
fn flow_job(app: &mut App, j: Job) {
    let needs = j
        .error
        .as_ref()
        .filter(|e| e.reason() == Some("needs_auth"))
        .map(|e| e.details.clone());
    let ended = !j.active();
    let Some(f) = app.ux.cloud.flow.as_mut() else {
        return;
    };
    f.job = Some(j);
    if let Some(d) = needs {
        enter_auth(f, &d);
    } else if ended {
        f.retried = false;
    }
}

/// Pushed `cloud.*` events.
pub fn on_event(app: &mut App, mi: usize, kind: &str, v: &Value) {
    let data = &v["data"];
    match kind {
        "cloud.job" => {
            let job = data.get("job").filter(|j| j.is_object()).unwrap_or(data);
            if let Some(j) = Job::from_value(job) {
                upsert_job(app, mi, j);
            }
        }
        "cloud.box.changed" => {
            // `data` is the BoxView, whose own `box` field is the id string.
            let b = data.get("box").filter(|b| b.is_object()).unwrap_or(data);
            if let Some(row) = BoxRow::from_value(b) {
                crate::sandboxes::on_box_changed(app, mi, row);
            }
        }
        "cloud.auth.changed" => {
            let provider = v["subject"]["provider"]
                .as_str()
                .or_else(|| data["provider"].as_str())
                .map(str::to_string);
            if let Some(p) = provider {
                if let Some(f) = app.ux.cloud.flow.as_mut() {
                    for r in f.providers.iter_mut().filter(|r| r.id == p) {
                        r.apply_auth(data);
                    }
                }
                crate::sandboxes::on_auth_changed(app, mi, &p, data);
            }
        }
        _ => {}
    }
    app.dirty = true;
}

/// Moves in progress, for the right side of the tab bar.
pub fn status(app: &App) -> Option<String> {
    let active: Vec<String> = app
        .ux
        .cloud
        .jobs
        .values()
        .flatten()
        .filter(|j| j.active())
        .map(Job::status)
        .collect();
    match active.len() {
        0 => None,
        1 | 2 => Some(active.join(" · ")),
        n => Some(format!("{} · +{} more", active[..2].join(" · "), n - 2)),
    }
}

// ---- polling ---------------------------------------------------------------------------------

fn polling(app: &App) -> Option<&Flow> {
    if !matches!(app.mode, Mode::Popup(Popup::CloudSend)) {
        return None;
    }
    let f = app.ux.cloud.flow.as_ref()?;
    (matches!(f.stage, Stage::Sending)
        && f.job.as_ref().is_some_and(Job::active)
        && !crate::push::supported(app, f.mi))
    .then_some(f)
}

/// A server that does not push `cloud.job` is asked for the flow's job.
pub fn tick(app: &mut App) {
    let now = Instant::now();
    let Some(f) = polling(app) else {
        return;
    };
    if f.polled_at.is_some_and(|t| now.duration_since(t) < POLL) {
        return;
    }
    let (mi, op) = (f.mi, f.op);
    if let Some(f) = app.ux.cloud.flow.as_mut() {
        f.polled_at = Some(now);
    }
    send_call(app, mi, "cloud.jobs", json!({}), op, Call::Jobs);
}

pub fn deadlines(app: &App, now: Instant, d: &mut crate::deadline::Deadlines) {
    if let Some(f) = polling(app) {
        d.at("cloud.poll", f.polled_at.map_or(now, |t| t + POLL));
    }
}

// ---- keys ------------------------------------------------------------------------------------

/// What a key asks the app to do (after the flow's own state changed).
#[derive(Debug, Clone, PartialEq)]
pub enum Act {
    None,
    Close,
    ChooseProvider(usize),
    SetToken(String),
    Import(String),
    OpenUrl(String),
    ChooseBox(usize),
    StartMove,
    CancelJob(String),
    Focus(String),
    /// Leave the Auth stage for the provider list.
    LeaveAuth,
}

fn moved(sel: &mut usize, n: usize, up: bool, down: bool) {
    if down {
        *sel = (*sel + 1).min(n.saturating_sub(1));
    } else if up {
        *sel = sel.saturating_sub(1);
    }
}

/// The flow's key handling without the app: changes the flow and says what else to do.
pub fn on_key(f: &mut Flow, ev: &KeyEvent) -> Act {
    let esc = matches!(ev.key, Key::Named(NamedKey::Escape));
    let plain = !ev.mods.ctrl() && !ev.mods.alt();
    let up = matches!(ev.key, Key::Char('k') | Key::Named(NamedKey::Up));
    let down = matches!(
        ev.key,
        Key::Char('j') | Key::Named(NamedKey::Down | NamedKey::Tab)
    );
    let enter = matches!(ev.key, Key::Named(NamedKey::Enter));
    let quit = esc || (plain && matches!(ev.key, Key::Char('q')));
    let idle = f.busy.is_none() && !f.loading;
    match &mut f.stage {
        Stage::PickProvider { sel } => {
            if quit {
                return Act::Close;
            }
            moved(sel, f.providers.len(), up, down);
            if enter && idle && *sel < f.providers.len() {
                return Act::ChooseProvider(*sel);
            }
        }
        Stage::Auth(a) => match a.key(ev) {
            AuthOut::None => {}
            AuthOut::Cancel => return Act::LeaveAuth,
            AuthOut::SetToken(t) => {
                a.busy = Some("checking the token…".into());
                return Act::SetToken(t);
            }
            AuthOut::Import(s) => {
                a.busy = Some(format!("importing from {s}…"));
                return Act::Import(s);
            }
            AuthOut::Open(u) => return Act::OpenUrl(u),
        },
        Stage::PickBox { sel } => {
            if esc {
                f.stage = Stage::PickProvider { sel: 0 };
                f.error = None;
                return Act::None;
            }
            moved(sel, f.boxes.len() + 1, up, down);
            if enter && idle {
                return Act::ChooseBox(*sel);
            }
        }
        Stage::Confirm => {
            if esc {
                f.error = None;
                f.stage = if f.boxes.is_empty() {
                    Stage::PickProvider { sel: 0 }
                } else {
                    Stage::PickBox { sel: 0 }
                };
                return Act::None;
            }
            if f.busy.is_some() {
                return Act::None;
            }
            let toggle = matches!(ev.key, Key::Named(NamedKey::Space))
                || (plain && matches!(ev.key, Key::Char('i' | ' ')));
            if toggle {
                f.interrupt = !f.interrupt;
            } else if enter {
                return Act::StartMove;
            }
        }
        Stage::PickTarget { sel } => {
            if quit {
                return Act::Close;
            }
            moved(sel, f.peers.len() + 1, up, down);
            if enter && f.busy.is_none() {
                return Act::StartMove;
            }
        }
        Stage::Sending => {
            if quit {
                return Act::Close;
            }
            match f.job.clone() {
                Some(j) if j.active() => {
                    if plain && matches!(ev.key, Key::Char('x')) {
                        return Act::CancelJob(j.id);
                    }
                }
                Some(j) if enter => {
                    if j.state == "done"
                        && let Some(p) = s_of(&j.result, "pane")
                    {
                        return Act::Focus(p);
                    }
                    if j.state == "done" {
                        return Act::Close;
                    }
                    // Failed or cancelled: back to the step before.
                    f.job = None;
                    f.error = None;
                    f.stage = match f.kind {
                        Kind::Send => Stage::Confirm,
                        Kind::BringBack => Stage::PickTarget { sel: 0 },
                    };
                }
                _ => {}
            }
        }
    }
    Act::None
}

pub fn key(app: &mut App, ev: KeyEvent) {
    app.mode = Mode::Popup(Popup::CloudSend);
    if ev.kind == KeyKind::Release {
        return;
    }
    app.dirty = true;
    let Some(f) = app.ux.cloud.flow.as_mut() else {
        app.mode = Mode::Normal;
        return;
    };
    let act = on_key(f, &ev);
    run(app, act);
}

fn run(app: &mut App, act: Act) {
    match act {
        Act::None => {}
        Act::Close => close(app),
        Act::ChooseProvider(i) => choose_provider(app, i),
        Act::SetToken(t) => auth_send(app, AuthOut::SetToken(t)),
        Act::Import(s) => auth_send(app, AuthOut::Import(s)),
        Act::OpenUrl(u) => {
            let mi = app.cur;
            crate::nav::open_url(app, mi, "", &u);
        }
        Act::ChooseBox(i) => {
            if let Some(f) = app.ux.cloud.flow.as_mut() {
                f.box_sel = f.boxes.get(i).map(|b| b.box_id.clone());
                f.stage = Stage::Confirm;
                f.error = None;
            }
        }
        Act::StartMove => start_move(app),
        Act::CancelJob(id) => {
            let Some(f) = app.ux.cloud.flow.as_ref() else {
                return;
            };
            let (mi, op) = (f.mi, f.op);
            send_call(app, mi, "cloud.cancel", json!({"id": id}), op, Call::Cancel);
        }
        Act::Focus(p) => {
            let mi = app.ux.cloud.flow.as_ref().map_or(app.cur, |f| f.mi);
            close(app);
            if app.machines[mi].model.panes.iter().any(|x| x.id == p) {
                app.focus_pane(mi, &p);
            }
        }
        Act::LeaveAuth => {
            if let Some(f) = app.ux.cloud.flow.as_mut() {
                f.retry = None;
                f.retried = false;
                f.stage = match f.kind {
                    Kind::Send => Stage::PickProvider { sel: 0 },
                    Kind::BringBack => Stage::PickTarget { sel: 0 },
                };
            }
        }
    }
}

/// A bracketed paste: into the masked token field in the Auth stage.
pub fn on_paste(app: &mut App, text: &str) {
    if let Some(f) = app.ux.cloud.flow.as_mut()
        && let Stage::Auth(a) = &mut f.stage
    {
        a.paste(text);
        app.dirty = true;
    }
}

fn auth_send(app: &mut App, out: AuthOut) {
    let Some(f) = app.ux.cloud.flow.as_mut() else {
        return;
    };
    let Stage::Auth(a) = &mut f.stage else {
        return;
    };
    let Some((method, params)) = auth_request(&a.provider, &out) else {
        return;
    };
    // The token is in the request only: the field is empty from here on.
    a.token.clear();
    let call = if method == "cloud.auth.set" {
        Call::AuthSet
    } else {
        Call::AuthImport
    };
    let (mi, op) = (f.mi, f.op);
    send_call(app, mi, method, params, op, call);
}

fn choose_provider(app: &mut App, i: usize) {
    let Some(f) = app.ux.cloud.flow.as_mut() else {
        return;
    };
    let Some(p) = f.providers.get(i).cloned() else {
        return;
    };
    f.provider = Some(p.id.clone());
    f.error = None;
    f.retry = None;
    f.retried = false;
    if p.signed_in() {
        load_boxes(app);
    } else {
        f.stage = Stage::Auth(AuthState::new(&p.id, &p.label, p.methods));
    }
}

fn load_boxes(app: &mut App) {
    let Some(f) = app.ux.cloud.flow.as_mut() else {
        return;
    };
    let Some(p) = f.provider.clone() else {
        return;
    };
    f.loading = true;
    f.boxes.clear();
    f.box_sel = None;
    f.stage = Stage::PickBox { sel: 0 };
    flow_call(
        app,
        "cloud.box.list",
        json!({"provider": p, "refresh": false}),
        Call::Boxes,
    );
}

/// The `to` of `cloud.move` for the flow's choice.
pub fn move_target(f: &Flow) -> Option<Value> {
    match (&f.kind, &f.stage) {
        (Kind::Send, _) => {
            let p = f.provider.clone()?;
            let mut to = json!({"kind": "cloud", "provider": p});
            if let Some(b) = &f.box_sel {
                to["box"] = json!(b);
            }
            Some(to)
        }
        (Kind::BringBack, Stage::PickTarget { sel }) => Some(match sel.checked_sub(1) {
            None => json!({"kind": "local"}),
            Some(i) => json!({"kind": "peer", "peer": f.peers.get(i)?.id}),
        }),
        _ => None,
    }
}

/// The `cloud.move` params for the flow.
pub fn move_params(f: &Flow) -> Option<Value> {
    let mut p = match &f.src {
        Source::Pane(x) => json!({"pane": x}),
        Source::Box(x) => json!({"box": x}),
    };
    p["to"] = move_target(f)?;
    if f.kind == Kind::Send {
        p["interrupt"] = json!(f.interrupt);
    }
    Some(p)
}

fn start_move(app: &mut App) {
    let Some(f) = app.ux.cloud.flow.as_mut() else {
        return;
    };
    let Some(params) = move_params(f) else {
        f.error = Some("nothing chosen".into());
        return;
    };
    f.busy = Some("starting…".into());
    f.error = None;
    f.retried = false;
    f.job = None;
    // Staying on this stage until the job exists: a failure shows here.
    flow_call(app, "cloud.move", params, Call::Move);
}

/// The Auth stage for a `needs_auth` error's details: its provider (else `fallback`) and the
/// methods the error lists (else the provider's own).
pub(crate) fn auth_stage(
    providers: &[ProviderRow],
    fallback: Option<&str>,
    details: &Value,
) -> Option<AuthState> {
    let provider = s_of(details, "provider").or_else(|| fallback.map(str::to_string))?;
    let row = providers.iter().find(|p| p.id == provider);
    let mut methods = methods_of(&details["methods"]);
    if methods.is_empty() {
        methods = row.map(|r| r.methods.clone()).unwrap_or_default();
    }
    let label = row.map_or_else(|| provider.clone(), |r| r.label.clone());
    Some(AuthState::new(&provider, &label, methods))
}

/// Enter the Auth stage from a `needs_auth` error's details. A second one after a retry is an
/// error shown on the stage.
fn enter_auth(f: &mut Flow, details: &Value) {
    f.busy = None;
    f.loading = false;
    if f.retried {
        f.retried = false;
        f.retry = None;
        f.error = Some("still not signed in: the sign-in did not work".into());
        return;
    }
    match auth_stage(&f.providers, f.provider.as_deref(), details) {
        Some(a) => {
            f.provider = Some(a.provider.clone());
            f.stage = Stage::Auth(a);
        }
        None => f.error = Some("sign in to the provider first".into()),
    }
}

// ---- replies ---------------------------------------------------------------------------------

pub fn on_reply(app: &mut App, mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    app.dirty = true;
    if let Call::AllJobs = r.call {
        match res {
            Ok(v) => {
                let mut jobs: Vec<Job> = v["jobs"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Job::from_value).collect())
                    .unwrap_or_default();
                jobs.sort_by_key(|j| std::cmp::Reverse(j.updated_at));
                jobs.truncate(50);
                app.ux.cloud.jobs.insert(mi, jobs);
            }
            Err(e) if e.is_method_not_found() => {
                app.ux.cloud.unsupported.insert(mi);
            }
            Err(_) => {}
        }
        return;
    }
    let Some(f) = app.ux.cloud.flow.as_mut() else {
        return;
    };
    if f.op != r.op || f.mi != mi {
        return;
    }
    if let Err(e) = &res
        && e.reason() == Some("needs_auth")
        && matches!(r.call, Call::Providers | Call::Boxes | Call::Move)
    {
        enter_auth(f, &e.details);
        return;
    }
    match (r.call, res) {
        (Call::Providers, Ok(v)) => {
            f.loading = false;
            f.retried = false;
            f.providers = providers_of(&v);
            if let Stage::PickProvider { sel } = &mut f.stage {
                *sel = f
                    .providers
                    .iter()
                    .position(|p| p.default && p.signed_in())
                    .or_else(|| f.providers.iter().position(ProviderRow::signed_in))
                    .or_else(|| f.providers.iter().position(|p| p.default))
                    .unwrap_or(0);
            }
            if f.providers.is_empty() {
                f.error = Some("no cloud provider is available on this machine".into());
            }
        }
        (Call::Boxes, Ok(v)) => {
            f.loading = false;
            f.retried = false;
            let provider = f.provider.clone().unwrap_or_default();
            let keys = f.task_keys.clone();
            f.boxes = v["boxes"]
                .as_array()
                .map(|a| a.iter().filter_map(BoxRow::from_value).collect::<Vec<_>>())
                .unwrap_or_default()
                .into_iter()
                .filter(|b| {
                    b.provider == provider
                        && matches!(b.ownership.as_str(), "attached" | "idle")
                        && b.state != "destroyed"
                        && b.task.as_ref().is_some_and(|t| keys.contains(t))
                })
                .collect();
            if f.boxes.is_empty() {
                // Nothing of this task there: a new sandbox, no question.
                f.box_sel = None;
                f.stage = Stage::Confirm;
            }
        }
        (Call::Peers, Ok(v)) => {
            f.loading = false;
            f.peers = v["peers"]
                .as_array()
                .map(|a| a.iter().filter_map(Peer::from_value).collect::<Vec<_>>())
                .unwrap_or_default()
                .into_iter()
                .filter(|p| !p.is_expired(now_ms()))
                .collect();
        }
        (Call::Peers, Err(_)) => {
            // No gateway or no peers: this host is still a target.
            f.loading = false;
        }
        (Call::Move, Ok(v)) => {
            f.busy = None;
            // `retried` stays until the job ends: a job that fails with `needs_auth` after the
            // retry is an error, not another sign-in (one retry per move the user started).
            let job = v.get("job").filter(|j| j.is_object()).unwrap_or(&v);
            match Job::from_value(job) {
                Some(j) => {
                    let id = j.id.clone();
                    f.job = Some(j.clone());
                    f.stage = Stage::Sending;
                    // The event may have come first.
                    let known = app
                        .ux
                        .cloud
                        .jobs
                        .get(&mi)
                        .and_then(|l| l.iter().find(|x| x.id == id))
                        .cloned();
                    upsert_job(
                        app,
                        mi,
                        known.filter(|k| k.updated_at > j.updated_at).unwrap_or(j),
                    );
                }
                None => f.error = Some("the server sent no job".into()),
            }
        }
        (Call::Jobs, Ok(v)) => {
            let id = f.job.as_ref().map(|j| j.id.clone());
            let found = v["jobs"]
                .as_array()
                .and_then(|a| a.iter().find(|j| j["id"].as_str() == id.as_deref()))
                .and_then(Job::from_value);
            if let Some(j) = found {
                upsert_job(app, mi, j);
            }
        }
        (Call::AuthSet | Call::AuthImport, Ok(v)) => auth_done(app, &v),
        (Call::AuthSet | Call::AuthImport, Err(e)) => {
            if let Stage::Auth(a) = &mut f.stage {
                a.busy = None;
                a.error = Some(err_text(&e));
            }
        }
        (Call::Cancel, Err(e)) => f.error = Some(err_text(&e)),
        (Call::Cancel, Ok(v)) => {
            if let Some(j) = Job::from_value(&v) {
                upsert_job(app, mi, j);
            }
        }
        (Call::Move, Err(e)) => {
            f.busy = None;
            f.retried = false;
            f.error = Some(err_text(&e));
        }
        (_, Err(e)) => {
            f.loading = false;
            f.busy = None;
            f.retried = false;
            f.error = Some(err_text(&e));
        }
        _ => {}
    }
}

/// Signed in (`cloud.auth.set` / `cloud.auth.import` answered `{provider, account}`): the
/// original call is repeated once; without one the flow goes on to the sandboxes.
fn auth_done(app: &mut App, v: &Value) {
    let Some(f) = app.ux.cloud.flow.as_mut() else {
        return;
    };
    let provider = s_of(v, "provider").or_else(|| f.provider.clone());
    if let Some(p) = &provider {
        for r in f.providers.iter_mut().filter(|r| &r.id == p) {
            r.state = "ok".into();
            r.account = s_of(v, "account");
        }
    }
    f.error = None;
    match f.retry.take() {
        Some(r) => {
            f.retried = true;
            f.stage = match r.call {
                Call::Move => {
                    f.busy = Some("starting…".into());
                    match f.kind {
                        Kind::Send => Stage::Confirm,
                        Kind::BringBack => Stage::PickTarget { sel: 0 },
                    }
                }
                Call::Boxes => {
                    f.loading = true;
                    Stage::PickBox { sel: 0 }
                }
                _ => {
                    f.loading = true;
                    Stage::PickProvider { sel: 0 }
                }
            };
            f.retry = Some(Retry {
                method: r.method.clone(),
                params: r.params.clone(),
                call: r.call.clone(),
            });
            let (mi, op) = (f.mi, f.op);
            send_call(app, mi, &r.method, r.params, op, r.call);
        }
        None => load_boxes(app),
    }
}

// ---- drawing ---------------------------------------------------------------------------------

fn box_line(b: &BoxRow, now: i64) -> String {
    let age = if b.last_activity_at > 0 {
        format!("active {} ago", fmt_age(now - b.last_activity_at))
    } else {
        String::new()
    };
    let unsynced = if b.unsynced.as_ref().is_some_and(|u| u.any()) {
        "  ⚠ unsynced"
    } else {
        ""
    };
    format!(
        "{}  {}  {} pane{}  {age}{unsynced}",
        crate::draw::truncate(&b.name, 24),
        b.state,
        b.panes.len(),
        if b.panes.len() == 1 { "" } else { "s" }
    )
}

/// The popup; the cursor while the token field has the keys.
pub fn draw(app: &App, g: &mut Grid) -> Option<(u16, u16)> {
    let f = app.ux.cloud.flow.as_ref()?;
    let t = app.theme;
    let title = match f.kind {
        Kind::Send => "Send to cloud",
        Kind::BringBack => "Bring back from cloud",
    };
    let mut a = Area::open(app, g, &format!("Vibeke · {title}"));
    let now = now_ms();
    let row = |on: bool| if on { t.sel(t.fg) } else { t.text() };
    let mark = |on: bool| if on { "›" } else { " " };
    match &f.stage {
        Stage::PickProvider { sel } => {
            a.line(&format!("Where should {} run?", f.label), t.bold(t.fg));
            a.line("", t.text());
            if f.loading {
                a.line("  loading providers…", t.dim());
            }
            for (i, p) in f.providers.iter().enumerate() {
                let on = i == *sel;
                let def = if p.default { " (default)" } else { "" };
                a.line(
                    &format!("{} {}{def}  {}", mark(on), p.label, p.auth_line()),
                    row(on),
                );
            }
            a.line("", t.text());
            error_lines(&mut a, app, f);
            a.footer("j/k move · enter choose · esc", t.dim());
        }
        Stage::Auth(auth) => {
            return auth.draw(app, &mut a);
        }
        Stage::PickBox { sel } => {
            a.line(
                &format!(
                    "Which sandbox should {} go to ({})?",
                    f.label,
                    f.provider.as_deref().unwrap_or("")
                ),
                t.bold(t.fg),
            );
            a.line("", t.text());
            if f.loading {
                a.line("  loading sandboxes…", t.dim());
            }
            for (i, b) in f.boxes.iter().enumerate() {
                let on = i == *sel;
                a.line(&format!("{} {}", mark(on), box_line(b, now)), row(on));
            }
            if !f.loading {
                let on = *sel == f.boxes.len();
                a.line(&format!("{} New sandbox", mark(on)), row(on));
            }
            a.line("", t.text());
            error_lines(&mut a, app, f);
            a.footer("j/k move · enter choose · esc back", t.dim());
        }
        Stage::Confirm => {
            let provider = f
                .providers
                .iter()
                .find(|p| Some(&p.id) == f.provider.as_ref())
                .map_or_else(
                    || f.provider.clone().unwrap_or_default(),
                    |p| p.label.clone(),
                );
            let target = match &f.box_sel {
                Some(b) => f
                    .boxes
                    .iter()
                    .find(|x| &x.box_id == b)
                    .map_or(b.clone(), |x| format!("sandbox {}", x.name)),
                None => "a new sandbox".to_string(),
            };
            a.line(
                &format!("Send {} to {target} on {provider}?", f.label),
                t.bold(t.fg),
            );
            a.line("", t.text());
            a.line(
                "After the agent's turn, Vibeke copies the work to the sandbox and resumes the",
                t.dim(),
            );
            a.line(
                "agent there. If a step fails, the agent keeps running here.",
                t.dim(),
            );
            a.line("", t.text());
            a.line(
                &format!(
                    "{} Interrupt agent if busy  (i toggles)",
                    if f.interrupt { "[x]" } else { "[ ]" }
                ),
                t.text(),
            );
            a.line("", t.text());
            if let Some(b) = &f.busy {
                a.line(&format!("⏳ {b}"), t.s(t.yellow));
            }
            error_lines(&mut a, app, f);
            a.footer("enter send · i interrupt · esc back", t.dim());
        }
        Stage::PickTarget { sel } => {
            a.line(&format!("Bring {} back to:", f.label), t.bold(t.fg));
            a.line("", t.text());
            let on = *sel == 0;
            a.line(&format!("{} This host", mark(on)), row(on));
            for (i, p) in f.peers.iter().enumerate() {
                let on = *sel == i + 1;
                a.line(
                    &format!("{} {}  {}", mark(on), p.name, p.note(now)),
                    row(on),
                );
            }
            if f.loading {
                a.line("  loading paired hosts…", t.dim());
            }
            a.line("", t.text());
            if let Some(b) = &f.busy {
                a.line(&format!("⏳ {b}"), t.s(t.yellow));
            }
            error_lines(&mut a, app, f);
            a.footer("j/k move · enter bring back · esc", t.dim());
        }
        Stage::Sending => {
            a.line(&f.label, t.bold(t.fg));
            a.line("", t.text());
            match &f.job {
                None => a.line("⏳ starting…", t.s(t.yellow)),
                Some(j) => {
                    let verb = if j.direction == "bring_back" {
                        "Bringing back from"
                    } else {
                        "Sending to"
                    };
                    a.line(&format!("{verb} {}", j.provider()), t.text());
                    let (icon, st) = match j.state.as_str() {
                        "done" => ("✓", t.s(t.green)),
                        "failed" => ("✗", t.s(t.red)),
                        "cancelled" => ("•", t.dim()),
                        _ => ("⏳", t.s(t.yellow)),
                    };
                    a.line(&format!("{icon} {}", j.state_text()), st);
                    if let Some(p) = j.percent().filter(|_| j.total > 0 && j.active()) {
                        a.line(&bar(p, 30), t.s(t.accent));
                    }
                    if let Some(e) = &j.error {
                        crate::devices::wrap_lines(&mut a, &e.text(), t.s(t.red));
                    }
                    if j.state == "done"
                        && let Some(p) = s_of(&j.result, "pane")
                    {
                        // The pane's handle (`w1:p3`) once the model has it.
                        let handle = app
                            .machines
                            .get(f.mi)
                            .and_then(|m| m.model.panes.iter().find(|x| x.id == p))
                            .map_or(p.as_str(), |x| x.handle.as_str());
                        a.line(&format!("New pane: {handle} (enter opens it)"), t.text());
                    }
                }
            }
            error_lines(&mut a, app, f);
            let footer = match f.job.as_ref().map(|j| (j.active(), j.state.as_str())) {
                Some((true, _)) => "x cancel · esc close (the move continues)",
                Some((false, "done")) => "enter open the pane · esc close",
                Some(_) => "enter try again · esc close",
                None => "esc close",
            };
            a.footer(footer, t.dim());
        }
    }
    None
}

fn error_lines(a: &mut Area<'_>, app: &App, f: &Flow) {
    if let Some(e) = &f.error {
        crate::devices::wrap_lines(a, &format!("✗ {e}"), app.theme.s(app.theme.red));
    }
}

#[cfg(test)]
#[path = "cloud_tests.rs"]
mod tests;
