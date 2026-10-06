//! Session desk (08 §6.7, research R2 "Find and reopen previous work"; server: 07 §2.14a
//! `desk.*`).
//!
//! `:desk` opens a full pane-area view: a query line (`desk.search`), filter chips for
//! repository (default: the focused workspace's root), harness and date, and result rows
//! `harness · repo · session · turn n · age · snippet` with a status marker (`● live`,
//! `↻ resumable`, `· none`). `tab` switches to the sessions list (`desk.sessions`). `enter` opens
//! the result's turn (`desk.open`). Actions, labelled exactly:
//! - `o` **Focus live pane** (live only; `desk.open focus=true`),
//! - `r` **Resume native session** (shows the exact command and asks for a target: a new tab in
//!   the session's workspace, or the focused pane when it has no agent),
//! - `c` **Start new agent with context** (the `desk.context` package in an editor with
//!   selectable turns; saving creates a draft through `desk.resume mode=new_agent`, optionally
//!   starting the harness without a prompt; never sends).
//!
//! The footer shows coverage from `desk.status`; `F` forgets a session (confirm, `desk.forget`).

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::drafts::{Area, TextEditor, arr, ctrl, now_ms, st};
use crate::draw::truncate;
use crate::screen::Grid;
use serde_json::{Value, json};
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};

pub const HARNESSES: [&str; 4] = ["any", "claude", "codex", "pi"];
pub const SINCE: [&str; 3] = ["any", "7d", "30d"];

pub const FOCUS: &str = "Focus live pane";
pub const RESUME: &str = "Resume native session";
pub const NEW_AGENT: &str = "Start new agent with context";

#[derive(Debug, Clone)]
pub enum Reply {
    Search { view: u64, query: String },
    Sessions { view: u64 },
    Status { view: u64 },
    Open { view: u64 },
    Focus { view: u64 },
    Context { view: u64 },
    Resume { view: u64 },
    NewAgent { view: u64, text: Option<String> },
    DraftEdited { view: u64 },
    Forget { view: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tab {
    Results,
    Sessions,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResumeTarget {
    NewTab,
    Pane(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Context {
    pub hit: Value,
    pub turns: String,
    pub editing_turns: bool,
    pub ed: Option<TextEditor>,
    /// The package text as generated (to tell whether the user edited it).
    pub original: String,
    pub start: bool,
    pub saving: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Sub {
    None,
    Detail {
        hit: Value,
        open: Option<Value>,
        scroll: u16,
    },
    Resume {
        hit: Value,
        target: ResumeTarget,
        free_pane: Option<String>,
    },
    Context(Box<Context>),
    ConfirmForget(Value),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Desk {
    pub id: u64,
    pub machine: usize,
    pub tab: Tab,
    pub query: String,
    pub editing: bool,
    pub repo: Option<String>,
    pub repo_on: bool,
    pub harness: usize,
    pub since: usize,
    pub hits: Vec<Value>,
    pub sessions: Vec<Value>,
    pub sel: usize,
    pub status: Option<Value>,
    pub sub: Sub,
    pub loading: bool,
    pub notice: Option<String>,
    pub error: Option<String>,
    /// The query the shown hits belong to.
    pub searched: Option<String>,
}

impl Desk {
    pub fn rows(&self) -> &[Value] {
        match self.tab {
            Tab::Results => &self.hits,
            Tab::Sessions => &self.sessions,
        }
    }
    pub fn cur(&self) -> Option<&Value> {
        self.rows().get(self.sel)
    }
    fn filters(&self) -> Value {
        let mut p = json!({});
        if self.repo_on
            && let Some(r) = &self.repo
        {
            p["repo"] = json!(r);
        }
        if self.harness > 0 {
            p["harness"] = json!(HARNESSES[self.harness]);
        }
        if self.since > 0 {
            p["since"] = json!(SINCE[self.since]);
        }
        p
    }
    /// `desk.search` params for the current query and filters.
    pub fn search_params(&self) -> Value {
        let mut p = self.filters();
        p["text"] = json!(self.query.trim());
        p
    }
    pub fn sessions_params(&self) -> Value {
        let mut p = self.filters();
        if let Some(o) = p.as_object_mut() {
            o.remove("since");
        }
        p
    }
    pub fn chips(&self) -> String {
        let repo = match (&self.repo, self.repo_on) {
            (Some(r), true) => format!("repo: {}", short_path(r)),
            _ => "repo: any".into(),
        };
        format!(
            "[{repo}] (R)  [harness: {}] (h)  [date: {}] (D)",
            HARNESSES[self.harness], SINCE[self.since]
        )
    }
}

fn short_path(p: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    match p.strip_prefix(&home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => p.to_string(),
    }
}

pub fn status_marker(s: &str) -> &'static str {
    match s {
        "live" => "● live",
        "resumable" => "↻ resumable",
        _ => "· none",
    }
}

fn age(ts: i64) -> String {
    if ts <= 0 {
        return "—".into();
    }
    crate::inbox::fmt_age(now_ms() - ts)
}

/// One result row: `harness · repo · session · turn n · age · snippet`.
pub fn hit_row(h: &Value) -> String {
    let session = st(h, "session");
    let repo = h
        .get("repo")
        .and_then(Value::as_str)
        .or_else(|| h.get("cwd").and_then(Value::as_str))
        .map(|r| r.rsplit('/').next().unwrap_or(r).to_string())
        .unwrap_or_else(|| "—".into());
    let turn = h
        .get("turn")
        .and_then(Value::as_u64)
        .map(|t| format!("turn {t}"))
        .unwrap_or_else(|| {
            h.get("turns")
                .and_then(Value::as_u64)
                .map(|t| format!("{t} turns"))
                .unwrap_or_default()
        });
    let ts = h
        .get("ts")
        .or_else(|| h.get("last_ts"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let snippet = st(h, "snippet").replace('\n', " ");
    format!(
        "{:<13} {} · {repo} · {} · {turn} · {} · {snippet}",
        status_marker(st(h, "status")),
        st(h, "harness"),
        truncate(session, 12),
        age(ts)
    )
}

// ---- opening & requests --------------------------------------------------------------------------

pub fn action(app: &mut App, action: &str) -> bool {
    if action != "desk" {
        return false;
    }
    let mi = app.cur;
    open(app, mi);
    true
}

pub fn open(app: &mut App, mi: usize) {
    let id = app.next_ui_id();
    let repo = app
        .focused_ws()
        .map(|w| w.root_path)
        .filter(|r| !r.is_empty());
    app.desk = Some(Desk {
        id,
        machine: mi,
        tab: Tab::Results,
        query: String::new(),
        editing: true,
        repo_on: repo.is_some(),
        repo,
        harness: 0,
        since: 0,
        hits: Vec::new(),
        sessions: Vec::new(),
        sel: 0,
        status: None,
        sub: Sub::None,
        loading: false,
        notice: None,
        error: None,
        searched: None,
    });
    app.mode = Mode::Popup(Popup::Desk);
    app.command_on(
        mi,
        "desk.status",
        json!({}),
        Pending::Desk(Reply::Status { view: id }),
    );
}

fn search(app: &mut App, d: &mut Desk) {
    if d.query.trim().is_empty() {
        d.notice = Some("Type what you're looking for, then enter".into());
        return;
    }
    d.loading = true;
    app.command_on(
        d.machine,
        "desk.search",
        d.search_params(),
        Pending::Desk(Reply::Search {
            view: d.id,
            query: d.query.trim().to_string(),
        }),
    );
}

fn load_sessions(app: &mut App, d: &mut Desk) {
    d.loading = true;
    app.command_on(
        d.machine,
        "desk.sessions",
        d.sessions_params(),
        Pending::Desk(Reply::Sessions { view: d.id }),
    );
}

fn reload(app: &mut App, d: &mut Desk) {
    match d.tab {
        Tab::Results if !d.query.trim().is_empty() => search(app, d),
        Tab::Results => {}
        Tab::Sessions => load_sessions(app, d),
    }
}

fn open_params(h: &Value) -> Value {
    let mut p = json!({"session": st(h, "session")});
    if let Some(t) = h.get("turn").and_then(Value::as_u64) {
        p["turn"] = json!(t);
    }
    if let Some(path) = h.get("path").and_then(Value::as_str) {
        p["path"] = json!(path);
    }
    p
}

/// The focused pane, when it has no agent (a free pane to type a resume command into).
fn free_pane(app: &App, mi: usize) -> Option<String> {
    if mi != app.cur {
        return None;
    }
    let p = app.focused_pane()?;
    let busy = app.machines[mi].model.runs.iter().any(|r| r.pane == p);
    (!busy).then_some(p)
}

fn focus_live(app: &mut App, d: &mut Desk, h: &Value) {
    if st(h, "status") != "live" {
        d.notice = Some(format!(
            "Not live — {} is offered for resumable sessions",
            RESUME
        ));
        return;
    }
    let mut p = open_params(h);
    p["focus"] = json!(true);
    app.command_on(
        d.machine,
        "desk.open",
        p,
        Pending::Desk(Reply::Focus { view: d.id }),
    );
}

fn start_resume(app: &App, d: &mut Desk, h: &Value) {
    match st(h, "status") {
        "live" => d.notice = Some(format!("This session is live — o: {FOCUS}")),
        "resumable" => {
            d.sub = Sub::Resume {
                hit: h.clone(),
                target: ResumeTarget::NewTab,
                free_pane: free_pane(app, d.machine),
            }
        }
        _ => d.notice = Some("No resume handle for this session".into()),
    }
}

fn start_context(app: &mut App, d: &mut Desk, h: &Value) {
    let c = Context {
        hit: h.clone(),
        turns: String::new(),
        editing_turns: false,
        ed: None,
        original: String::new(),
        start: false,
        saving: false,
        error: None,
    };
    request_context(app, d, &c);
    d.sub = Sub::Context(Box::new(c));
}

fn request_context(app: &mut App, d: &Desk, c: &Context) {
    let mut p = json!({"session": st(&c.hit, "session")});
    if !c.turns.trim().is_empty() {
        p["turns"] = json!(c.turns.trim());
    }
    if let Some(path) = c.hit.get("path").and_then(Value::as_str) {
        p["path"] = json!(path);
    }
    app.command_on(
        d.machine,
        "desk.context",
        p,
        Pending::Desk(Reply::Context { view: d.id }),
    );
}

// ---- keys --------------------------------------------------------------------------------------------

pub fn key(app: &mut App, ev: KeyEvent) {
    let Some(mut d) = app.desk.take() else {
        app.mode = Mode::Normal;
        return;
    };
    app.mode = Mode::Popup(Popup::Desk);
    if ev.kind == KeyKind::Release {
        app.desk = Some(d);
        return;
    }
    d.notice = None;
    let sub = std::mem::replace(&mut d.sub, Sub::None);
    match sub {
        Sub::None => {
            if !list_key(app, &mut d, ev) {
                return; // closed
            }
        }
        Sub::Detail { hit, open, scroll } => match ev.key {
            Key::Named(NamedKey::Escape) => {}
            Key::Char('j') | Key::Named(NamedKey::Down) => {
                d.sub = Sub::Detail {
                    hit,
                    open,
                    scroll: scroll.saturating_add(1),
                }
            }
            Key::Char('k') | Key::Named(NamedKey::Up) => {
                d.sub = Sub::Detail {
                    hit,
                    open,
                    scroll: scroll.saturating_sub(1),
                }
            }
            Key::Char('o') => {
                focus_live(app, &mut d, &hit);
                if d.notice.is_some() {
                    d.sub = Sub::Detail { hit, open, scroll };
                }
            }
            Key::Char('r') => {
                start_resume(app, &mut d, &hit);
                if matches!(d.sub, Sub::None) {
                    d.sub = Sub::Detail { hit, open, scroll };
                }
            }
            Key::Char('c') => start_context(app, &mut d, &hit),
            _ => d.sub = Sub::Detail { hit, open, scroll },
        },
        Sub::Resume {
            hit,
            target,
            free_pane,
        } => match ev.key {
            Key::Named(NamedKey::Escape) => {}
            Key::Char('t') => {
                d.sub = Sub::Resume {
                    hit,
                    target: ResumeTarget::NewTab,
                    free_pane,
                }
            }
            Key::Char('p') if free_pane.is_some() => {
                let p = free_pane.clone().unwrap_or_default();
                d.sub = Sub::Resume {
                    hit,
                    target: ResumeTarget::Pane(p),
                    free_pane,
                }
            }
            Key::Named(NamedKey::Enter) | Key::Char('y') => {
                let mut p = json!({"session": st(&hit, "session"), "mode": "native"});
                if let ResumeTarget::Pane(pn) = &target {
                    p["pane"] = json!(pn);
                }
                app.command_on(
                    d.machine,
                    "desk.resume",
                    p,
                    Pending::Desk(Reply::Resume { view: d.id }),
                );
                d.notice = Some(format!("{RESUME}…"));
            }
            _ => {
                d.sub = Sub::Resume {
                    hit,
                    target,
                    free_pane,
                }
            }
        },
        Sub::Context(mut c) => {
            let leaving = closed_context(&ev, &c);
            context_key(app, &mut d, &mut c, ev);
            if !leaving {
                d.sub = Sub::Context(c);
            }
        }
        Sub::ConfirmForget(h) => match ev.key {
            Key::Char('y' | 'Y') => {
                app.command_on(
                    d.machine,
                    "desk.forget",
                    json!({"session": st(&h, "session")}),
                    Pending::Desk(Reply::Forget { view: d.id }),
                );
            }
            _ => d.notice = Some("Nothing forgotten".into()),
        },
    }
    if matches!(app.mode, Mode::Popup(Popup::Desk)) {
        app.desk = Some(d);
    }
}

fn closed_context(ev: &KeyEvent, c: &Context) -> bool {
    ev.key == Key::Named(NamedKey::Escape) && !c.editing_turns && !c.saving
}

/// Returns false when the view closed.
fn list_key(app: &mut App, d: &mut Desk, ev: KeyEvent) -> bool {
    if d.editing {
        match ev.key {
            Key::Named(NamedKey::Escape) => d.editing = false,
            Key::Named(NamedKey::Enter) => {
                d.editing = false;
                d.tab = Tab::Results;
                d.sel = 0;
                search(app, d);
            }
            Key::Named(NamedKey::Backspace) => {
                d.query.pop();
            }
            Key::Named(NamedKey::Tab) => {
                d.editing = false;
                d.tab = Tab::Sessions;
                d.sel = 0;
                load_sessions(app, d);
            }
            Key::Char('u') if ev.mods.ctrl() => d.query.clear(),
            Key::Char(c) if !ev.mods.ctrl() && !ev.mods.alt() => d.query.push(c),
            _ => {}
        }
        return true;
    }
    let n = d.rows().len();
    match ev.key {
        Key::Named(NamedKey::Escape) | Key::Char('q') => {
            app.mode = Mode::Normal;
            return false;
        }
        Key::Char('/') | Key::Char('i') => d.editing = true,
        Key::Named(NamedKey::Tab) => {
            d.tab = match d.tab {
                Tab::Results => Tab::Sessions,
                Tab::Sessions => Tab::Results,
            };
            d.sel = 0;
            if d.tab == Tab::Sessions {
                load_sessions(app, d);
            }
        }
        Key::Char('j') | Key::Named(NamedKey::Down) => d.sel = (d.sel + 1).min(n.saturating_sub(1)),
        Key::Char('k') | Key::Named(NamedKey::Up) => d.sel = d.sel.saturating_sub(1),
        Key::Char('R') => {
            if d.repo.is_some() {
                d.repo_on = !d.repo_on;
                reload(app, d);
            } else {
                d.notice = Some("The focused workspace has no repository root".into());
            }
        }
        Key::Char('h') => {
            d.harness = (d.harness + 1) % HARNESSES.len();
            reload(app, d);
        }
        Key::Char('D') => {
            d.since = (d.since + 1) % SINCE.len();
            reload(app, d);
        }
        Key::Named(NamedKey::Enter) => {
            if let Some(h) = d.cur().cloned() {
                app.command_on(
                    d.machine,
                    "desk.open",
                    open_params(&h),
                    Pending::Desk(Reply::Open { view: d.id }),
                );
                d.sub = Sub::Detail {
                    hit: h,
                    open: None,
                    scroll: 0,
                };
            }
        }
        Key::Char('o') => {
            if let Some(h) = d.cur().cloned() {
                focus_live(app, d, &h);
            }
        }
        Key::Char('r') => {
            if let Some(h) = d.cur().cloned() {
                start_resume(app, d, &h);
            }
        }
        Key::Char('c') => {
            if let Some(h) = d.cur().cloned() {
                start_context(app, d, &h);
            }
        }
        Key::Char('F') => {
            if let Some(h) = d.cur().cloned() {
                d.sub = Sub::ConfirmForget(h);
            }
        }
        _ => {}
    }
    true
}

fn context_key(app: &mut App, d: &mut Desk, c: &mut Context, ev: KeyEvent) {
    if c.saving {
        return;
    }
    if c.editing_turns {
        match ev.key {
            Key::Named(NamedKey::Escape) => c.editing_turns = false,
            Key::Named(NamedKey::Enter) => {
                c.editing_turns = false;
                c.ed = None;
                request_context(app, d, c);
            }
            Key::Named(NamedKey::Backspace) => {
                c.turns.pop();
            }
            Key::Char(ch) if ch.is_ascii_digit() || matches!(ch, ',' | '-' | ' ') => {
                c.turns.push(ch)
            }
            _ => {}
        }
        return;
    }
    if ev.key == Key::Named(NamedKey::Escape) {
        return; // back to the list; nothing was created
    }
    if ctrl(&ev, 't') {
        c.editing_turns = true;
        return;
    }
    if ctrl(&ev, 'a') {
        c.start = !c.start;
        return;
    }
    if ctrl(&ev, 'x') {
        let Some(ed) = &c.ed else {
            return;
        };
        let text = ed.text();
        let mut p = json!({
            "session": st(&c.hit, "session"),
            "mode": "new_agent",
            "start": c.start,
        });
        if !c.turns.trim().is_empty() {
            p["turns"] = json!(c.turns.trim());
        }
        if let Some(path) = c.hit.get("path").and_then(Value::as_str) {
            p["path"] = json!(path);
        }
        c.saving = true;
        app.command_on(
            d.machine,
            "desk.resume",
            p,
            Pending::Desk(Reply::NewAgent {
                view: d.id,
                text: (text != c.original).then_some(text),
            }),
        );
        return;
    }
    if let Some(ed) = &mut c.ed {
        ed.key(&ev);
    }
}

pub fn on_paste(app: &mut App, text: &str) -> bool {
    if !matches!(app.mode, Mode::Popup(Popup::Desk)) {
        return false;
    }
    let Some(d) = &mut app.desk else {
        return false;
    };
    if let Sub::Context(c) = &mut d.sub
        && let Some(ed) = &mut c.ed
    {
        ed.insert_str(text);
        return true;
    }
    if d.editing {
        d.query.push_str(&text.replace(['\n', '\r'], " "));
        return true;
    }
    false
}

// ---- replies -----------------------------------------------------------------------------------------

pub fn on_reply(app: &mut App, _mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    let id = match &r {
        Reply::Search { view, .. }
        | Reply::Sessions { view }
        | Reply::Status { view }
        | Reply::Open { view }
        | Reply::Focus { view }
        | Reply::Context { view }
        | Reply::Resume { view }
        | Reply::NewAgent { view, .. }
        | Reply::DraftEdited { view }
        | Reply::Forget { view } => *view,
    };
    let Some(mut d) = app.desk.take() else {
        return;
    };
    if d.id != id {
        app.desk = Some(d);
        return;
    }
    let unsupported = |e: &RpcErr| {
        if e.is_method_not_found() {
            "This machine's server has no session desk (needs a newer server)".to_string()
        } else {
            e.message.clone()
        }
    };
    let mut close_to: Option<Mode> = None;
    match r {
        Reply::Search { query, .. } => {
            d.loading = false;
            match res {
                Ok(x) => {
                    d.hits = arr(&x, "hits").to_vec();
                    d.searched = Some(query);
                    d.error = None;
                    d.sel = d.sel.min(d.hits.len().saturating_sub(1));
                }
                Err(e) => d.error = Some(unsupported(&e)),
            }
        }
        Reply::Sessions { .. } => {
            d.loading = false;
            match res {
                Ok(x) => {
                    d.sessions = arr(&x, "sessions").to_vec();
                    d.error = None;
                    d.sel = d.sel.min(d.sessions.len().saturating_sub(1));
                }
                Err(e) => d.error = Some(unsupported(&e)),
            }
        }
        Reply::Status { .. } => match res {
            Ok(x) => d.status = Some(x),
            Err(e) => d.error = Some(unsupported(&e)),
        },
        Reply::Open { .. } => {
            if let Sub::Detail { open, .. } = &mut d.sub {
                match res {
                    Ok(x) => *open = Some(x),
                    Err(e) => d.notice = Some(format!("✗ {}", e.message)),
                }
            }
        }
        Reply::Focus { .. } => match res {
            Ok(x) if x.get("focused").and_then(Value::as_bool) == Some(true) => {
                let pane = x["live"]["pane"].as_str().map(str::to_string);
                if let Some(p) = pane {
                    let mi = d.machine;
                    app.focus_pane(mi, &p);
                }
                close_to = Some(Mode::Normal);
            }
            Ok(_) => d.notice = Some("That session is no longer live".into()),
            Err(e) => d.notice = Some(format!("✗ {}", e.message)),
        },
        Reply::Context { .. } => {
            if let Sub::Context(c) = &mut d.sub {
                match res {
                    Ok(x) => {
                        let text = st(&x, "text").to_string();
                        c.original = text.clone();
                        let mut ed = TextEditor::new(&text, true);
                        ed.row = 0;
                        ed.col = 0;
                        c.ed = Some(ed);
                        c.error = None;
                    }
                    Err(e) => c.error = Some(e.message),
                }
            }
        }
        Reply::Resume { .. } => match res {
            Ok(x) => {
                d.sub = Sub::None;
                d.notice = Some(format!(
                    "{}: {}",
                    x.get("label").and_then(Value::as_str).unwrap_or(RESUME),
                    st(&x, "command")
                ));
            }
            Err(e) => {
                d.notice = Some(match e.reason() {
                    Some("session_live") => format!("The session is live — o: {FOCUS}"),
                    Some("pane_busy") => "That pane has an agent — choose a new tab".into(),
                    _ => format!("✗ {}", e.message),
                })
            }
        },
        Reply::NewAgent { text, .. } => match res {
            Ok(x) => {
                let draft = x
                    .get("draft")
                    .and_then(|v| v.get("id").and_then(Value::as_str).or(v.as_str()))
                    .unwrap_or("")
                    .to_string();
                let started = x.get("run").is_some_and(|r| !r.is_null());
                match (text, draft.is_empty()) {
                    (Some(t), false) => {
                        let key = app.new_idempotency_key("draft-ctx");
                        app.command_on(
                            d.machine,
                            "draft.update",
                            json!({"draft": draft, "text": t, "idempotency_key": key}),
                            Pending::Desk(Reply::DraftEdited { view: d.id }),
                        );
                    }
                    _ => {
                        d.sub = Sub::None;
                    }
                }
                d.notice = Some(format!(
                    "Saved as a draft — nothing was sent{}. :drafts to review and send",
                    if started {
                        "; the harness started without a prompt"
                    } else {
                        ""
                    }
                ));
            }
            Err(e) => {
                if let Sub::Context(c) = &mut d.sub {
                    c.saving = false;
                    c.error = Some(e.message);
                }
            }
        },
        Reply::DraftEdited { .. } => {
            d.sub = Sub::None;
            if let Err(e) = res {
                d.notice = Some(format!(
                    "Draft created, but your edits weren't saved: {}",
                    e.message
                ));
            }
        }
        Reply::Forget { .. } => match res {
            Ok(x) => {
                d.notice = Some(format!(
                    "Forgot {} row(s); the session won't be indexed again",
                    x.get("rows_deleted").and_then(Value::as_u64).unwrap_or(0)
                ));
                reload(app, &mut d);
            }
            Err(e) => d.notice = Some(format!("✗ {}", e.message)),
        },
    }
    match close_to {
        Some(m) => {
            if matches!(app.mode, Mode::Popup(Popup::Desk)) {
                app.mode = m;
            }
        }
        None => app.desk = Some(d),
    }
}

// ---- drawing -----------------------------------------------------------------------------------------

fn coverage(s: &Value) -> String {
    let counts = s.get("counts").cloned().unwrap_or(Value::Null);
    let roots = s
        .pointer("/selection/roots")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(h, v)| format!("{h}({})", v.as_array().map_or(0, Vec::len)))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "none".into());
    let pending: u64 = arr(s, "sources")
        .iter()
        .filter_map(|x| x.get("pending_bytes").and_then(Value::as_u64))
        .sum();
    format!(
        "coverage: {} sources · {} rows · roots: {roots} · {} excluded · {}d retention · {} pending",
        counts.get("sources").and_then(Value::as_u64).unwrap_or(0),
        counts.get("rows").and_then(Value::as_u64).unwrap_or(0),
        arr(s, "exclude").len(),
        s.get("retention_days").and_then(Value::as_u64).unwrap_or(0),
        crate::upload::human(pending)
    )
}

pub fn draw(app: &App, g: &mut Grid) {
    let Some(d) = &app.desk else {
        return;
    };
    let t = app.theme;
    let tabs = match d.tab {
        Tab::Results => "[Results]  Sessions",
        Tab::Sessions => " Results  [Sessions]",
    };
    let mut a = Area::open(app, g, &format!("session desk · {tabs}"));
    if let Some(n) = &d.notice {
        a.line(n, t.bold(t.yellow));
    }
    match &d.sub {
        Sub::Detail { hit, open, scroll } => {
            a.line(&hit_row(hit), t.bold(t.fg));
            match open {
                None => a.line("loading…", t.dim()),
                Some(o) => {
                    let items = arr(o, "turn_items");
                    if items.is_empty() {
                        a.line("(no indexed items for this turn)", t.dim());
                    }
                    let mut lines: Vec<(String, bool)> = Vec::new();
                    for it in items {
                        lines.push((format!("{} · {}", st(it, "role"), st(it, "kind")), true));
                        for l in st(it, "text").lines() {
                            lines.push((format!("  {l}"), false));
                        }
                    }
                    for (l, head) in lines.into_iter().skip(*scroll as usize) {
                        a.line(&l, if head { t.s(t.accent) } else { t.text() });
                    }
                }
            }
            a.footer(
                &format!("{} · j/k scroll · esc back", actions_line(hit)),
                t.dim(),
            );
            return;
        }
        Sub::Resume {
            hit,
            target,
            free_pane,
        } => {
            a.line(RESUME, t.bold(t.fg));
            a.line(&hit_row(hit), t.text());
            let cmd = hit
                .pointer("/resume/command")
                .and_then(Value::as_str)
                .unwrap_or("(the harness's own resume command)");
            a.line("Exact command:", t.dim());
            a.line(&format!("  {cmd}"), t.bold(t.accent));
            a.line("Where:", t.dim());
            let sel = |on: bool| if on { t.sel(t.accent) } else { t.text() };
            a.line(
                "  [t] a new tab in the session's workspace",
                sel(*target == ResumeTarget::NewTab),
            );
            match free_pane {
                Some(p) => a.line(
                    &format!("  [p] the focused pane ({}) — it has no agent", short_id(p)),
                    sel(matches!(target, ResumeTarget::Pane(_))),
                ),
                None => a.line("  (the focused pane has an agent — not offered)", t.dim()),
            }
            a.footer("enter resume · t/p target · esc back", t.dim());
            return;
        }
        Sub::Context(c) => {
            a.line(NEW_AGENT, t.bold(t.fg));
            a.line(
                &format!(
                    "Turns: {}{}   (ctrl+t edit, e.g. 3-5,7; default: the last three)",
                    if c.turns.is_empty() {
                        "last three"
                    } else {
                        &c.turns
                    },
                    if c.editing_turns { "▏" } else { "" }
                ),
                if c.editing_turns {
                    t.sel(t.accent)
                } else {
                    t.text()
                },
            );
            a.line(
                &format!(
                    "[{}] also start {} in a free pane, without a prompt (ctrl+a)",
                    if c.start { "x" } else { " " },
                    st(&c.hit, "harness")
                ),
                t.text(),
            );
            if let Some(e) = &c.error {
                a.line(e, t.s(t.red));
            }
            match &c.ed {
                Some(ed) => {
                    let r = a.rest();
                    ed.draw(a.g, r, t.text(), t.rev());
                }
                None => a.line("building the context package…", t.dim()),
            }
            a.footer(
                if c.saving {
                    "saving the draft…"
                } else {
                    "edit the package · ctrl+x save as draft (never sends) · esc back"
                },
                t.dim(),
            );
            return;
        }
        _ => {}
    }
    let q = if d.editing {
        format!("search: {}▏", d.query)
    } else {
        format!("search: {}   (/ to edit)", d.query)
    };
    a.line(&q, if d.editing { t.sel(t.fg) } else { t.text() });
    a.line(&d.chips(), t.s(t.accent));
    if let Some(e) = &d.error {
        a.line(e, t.s(t.red));
    }
    if d.loading {
        a.line("searching…", t.dim());
    }
    let rows = d.rows();
    if rows.is_empty() && !d.loading {
        a.line(
            match (d.tab, &d.searched) {
                (Tab::Results, None) => {
                    "Type what you remember from a conversation, then enter · tab lists sessions"
                }
                (Tab::Results, Some(_)) => "No matches",
                (Tab::Sessions, _) => "No indexed sessions",
            },
            t.dim(),
        );
    }
    let reserve = 2;
    let max = a.left().saturating_sub(reserve) as usize;
    let skip = d.sel.saturating_sub(max.saturating_sub(1));
    for (i, h) in rows.iter().enumerate().skip(skip).take(max) {
        let style = if i == d.sel && !d.editing {
            t.sel(t.fg)
        } else {
            match st(h, "status") {
                "live" => t.s(t.green),
                "resumable" => t.text(),
                _ => t.dim(),
            }
        };
        a.line(&hit_row(h), style);
    }
    if let Some(h) = d.cur().filter(|_| !d.editing) {
        a.line(&actions_line(h), t.s(t.accent));
    }
    if let Some(s) = &d.status {
        let y = a.bottom().saturating_sub(1);
        if a.y <= y {
            a.y = y;
            a.line(&coverage(s), t.dim());
        }
    }
    if let Sub::ConfirmForget(h) = &d.sub {
        a.footer(
            &format!(
                "Forget session {} from the index? [y] forget  [any key] keep",
                truncate(st(h, "session"), 16)
            ),
            t.bold(t.yellow),
        );
    } else if d.editing {
        a.footer(
            "type · enter search · tab sessions · esc stop typing",
            t.dim(),
        );
    } else {
        a.footer(
            "j/k · enter open · o/r/c actions · F forget · R/h/D filters · / search · tab · esc",
            t.dim(),
        );
    }
}

fn short_id(p: &str) -> String {
    p.chars()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// The actions offered for a result, labelled exactly (only the available ones).
pub fn actions_line(h: &Value) -> String {
    let mut v = Vec::new();
    match st(h, "status") {
        "live" => v.push(format!("[o] {FOCUS}")),
        "resumable" => v.push(format!("[r] {RESUME}")),
        _ => {}
    }
    v.push(format!("[c] {NEW_AGENT}"));
    v.join("  ")
}

#[cfg(test)]
#[path = "desk_tests.rs"]
mod tests;
