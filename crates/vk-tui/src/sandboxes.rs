//! The sandboxes overview (spec 17 §8; `sandboxes` in the palette): every cloud sandbox on one
//! machine's server (`cloud.box.list {refresh: true}` on opening), grouped by provider. A
//! provider's header says whether you are signed in; `s` signs in with the shared Auth stage of
//! [`crate::cloud`]. A row shows the name, state, ownership, task or workspace, panes, age, last
//! activity and a marker for work that exists only in the sandbox.
//!
//! Row keys: `enter` opens the task's pane, `b` brings the sandbox's work back
//! ([`crate::cloud`]), `p` suspends or resumes (only where the provider can), `c` checkpoints,
//! `a` adopts an orphaned or foreign sandbox, `f` forgets a missing one, `d` destroys it after a
//! confirm (a refusal for unsynced work offers *Bring back first* and *Destroy anyway*), and
//! `C` cleans up: a dry run lists what would go, then one confirm. `cloud.box.changed` events
//! keep the list current. The footer counts what runs and what is idle.

use serde_json::{Value, json};
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::cloud::{AuthOut, AuthState, ProviderRow, err_text, now_ms, s_of, time_of};
use crate::drafts::Area;
use crate::inbox::fmt_age;
use crate::screen::Grid;

// ---- data ------------------------------------------------------------------------------------

/// Work that exists only in the sandbox (`unsynced` of a `BoxView`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Unsynced {
    pub commits: u64,
    pub dirty: u64,
    pub untracked: u64,
    pub summary: String,
}

impl Unsynced {
    pub fn from_value(v: &Value) -> Option<Unsynced> {
        if !v.is_object() {
            return None;
        }
        let n = |k: &str| {
            v.get(k)
                .and_then(|x| x.as_u64().or_else(|| x.as_bool().map(u64::from)))
                .unwrap_or(0)
        };
        Some(Unsynced {
            commits: n("commits"),
            dirty: n("dirty"),
            untracked: n("untracked"),
            summary: s_of(v, "summary").unwrap_or_default(),
        })
    }

    pub fn any(&self) -> bool {
        self.commits + self.dirty + self.untracked > 0
    }
}

/// One sandbox (`BoxView`).
#[derive(Debug, Clone, PartialEq)]
pub struct BoxRow {
    /// `<provider>/<id>`.
    pub box_id: String,
    pub provider: String,
    pub id: String,
    pub name: String,
    pub state: String,
    /// `attached`, `idle`, `orphaned`, `foreign` or `missing`.
    pub ownership: String,
    pub task: Option<String>,
    pub workspace: Option<String>,
    pub panes: Vec<String>,
    pub sessions: u64,
    /// Epoch ms; 0 when the server sent none.
    pub created_at: i64,
    pub last_activity_at: i64,
    pub unsynced: Option<Unsynced>,
    pub caps: Value,
}

impl BoxRow {
    pub fn from_value(v: &Value) -> Option<BoxRow> {
        let provider = s_of(v, "provider")?;
        let id = s_of(v, "id")?;
        let panes = v
            .get("panes")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|p| match p {
                        Value::String(s) => Some(s.clone()),
                        o => s_of(o, "id").or_else(|| s_of(o, "pane")),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(BoxRow {
            box_id: s_of(v, "box").unwrap_or_else(|| format!("{provider}/{id}")),
            name: s_of(v, "name").unwrap_or_else(|| id.clone()),
            state: s_of(v, "state").unwrap_or_default(),
            ownership: s_of(v, "ownership").unwrap_or_default(),
            task: s_of(v, "task"),
            workspace: s_of(v, "workspace"),
            panes,
            sessions: v.get("sessions").and_then(Value::as_u64).unwrap_or(0),
            created_at: time_of(v, "created_at"),
            last_activity_at: time_of(v, "last_activity_at"),
            unsynced: v.get("unsynced").and_then(Unsynced::from_value),
            caps: v.get("caps").cloned().unwrap_or(Value::Null),
            provider,
            id,
        })
    }

    pub fn is_running(&self) -> bool {
        matches!(
            self.state.as_str(),
            "running" | "warm" | "started" | "ready"
        )
    }

    pub fn is_suspended(&self) -> bool {
        matches!(
            self.state.as_str(),
            "suspended" | "paused" | "cold" | "stopped"
        )
    }

    /// A capability the provider reports; unknown (no caps sent) counts as yes, the server
    /// refuses what it cannot do.
    fn cap(&self, name: &str) -> bool {
        match &self.caps {
            Value::Object(o) => o.get(name).and_then(Value::as_bool).unwrap_or(false),
            _ => true,
        }
    }

    pub fn can_suspend(&self) -> bool {
        self.cap("explicit_suspend")
    }

    pub fn can_checkpoint(&self) -> bool {
        self.cap("checkpoints")
    }

    fn has_unsynced(&self) -> bool {
        self.unsynced.as_ref().is_some_and(Unsynced::any)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListError {
    pub provider: String,
    pub kind: String,
    pub message: String,
}

// ---- state -----------------------------------------------------------------------------------

/// A question the view is asking.
#[derive(Debug, Clone, PartialEq)]
pub enum Confirm {
    Destroy {
        box_id: String,
        name: String,
    },
    /// `cloud.box.destroy` refused for unsynced work.
    Unsynced {
        box_id: String,
        name: String,
        summary: String,
    },
    /// What `cloud.prune {dry_run: true}` would destroy, one line each.
    Prune {
        lines: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    List,
    Auth(AuthState),
}

#[derive(Debug, Clone)]
pub enum Call {
    List,
    Providers,
    Suspend,
    Resume,
    Checkpoint {
        name: String,
    },
    Destroy {
        box_id: String,
        name: String,
        force: bool,
    },
    Adopt {
        name: String,
    },
    Forget {
        box_id: String,
    },
    PruneDry,
    Prune,
    AuthSet,
    AuthImport,
}

/// Replies routed back here (through `ux::Reply::Sandboxes`).
#[derive(Debug, Clone)]
pub struct Reply {
    /// The view it was made for (a reopened view ignores it).
    pub op: u64,
    pub call: Call,
}

#[derive(Debug, Clone)]
pub struct Retry {
    pub method: String,
    pub params: Value,
    pub call: Call,
}

#[derive(Debug, Clone)]
pub struct View {
    pub op: u64,
    pub mi: usize,
    /// By provider (the order of `providers`), then name.
    pub rows: Vec<BoxRow>,
    pub providers: Vec<ProviderRow>,
    pub errors: Vec<ListError>,
    pub sel: usize,
    pub loading: bool,
    pub notice: Option<String>,
    pub busy: Option<String>,
    pub confirm: Option<Confirm>,
    pub stage: Stage,
    pub retry: Option<Retry>,
    pub retried: bool,
}

impl View {
    fn new(mi: usize) -> View {
        View {
            op: crate::cloud::next_op(),
            mi,
            rows: Vec::new(),
            providers: Vec::new(),
            errors: Vec::new(),
            sel: 0,
            loading: true,
            notice: None,
            busy: None,
            confirm: None,
            stage: Stage::List,
            retry: None,
            retried: false,
        }
    }

    fn provider_rank(&self, id: &str) -> usize {
        self.providers
            .iter()
            .position(|p| p.id == id)
            .unwrap_or(usize::MAX)
    }

    fn sort(&mut self) {
        let sel = self.rows.get(self.sel).map(|b| b.box_id.clone());
        let mut rows = std::mem::take(&mut self.rows);
        rows.sort_by(|a, b| {
            self.provider_rank(&a.provider)
                .cmp(&self.provider_rank(&b.provider))
                .then_with(|| a.provider.cmp(&b.provider))
                .then_with(|| a.name.cmp(&b.name))
        });
        self.rows = rows;
        if let Some(id) = sel
            && let Some(i) = self.rows.iter().position(|b| b.box_id == id)
        {
            self.sel = i;
        }
        self.sel = self.sel.min(self.rows.len().saturating_sub(1));
    }

    fn upsert(&mut self, b: BoxRow) {
        if b.state == "destroyed" {
            self.rows.retain(|x| x.box_id != b.box_id);
        } else {
            match self.rows.iter_mut().find(|x| x.box_id == b.box_id) {
                Some(x) => *x = b,
                None => self.rows.push(b),
            }
        }
        self.sort();
    }

    /// (running, idle) for the footer.
    pub fn counts(&self) -> (usize, usize) {
        (
            self.rows.iter().filter(|b| b.is_running()).count(),
            self.rows.iter().filter(|b| b.ownership == "idle").count(),
        )
    }

    pub fn selected(&self) -> Option<&BoxRow> {
        self.rows.get(self.sel)
    }

    /// The provider `s` signs in to: the selected row's, else the first one without a sign-in.
    fn sign_in_provider(&self) -> Option<&ProviderRow> {
        self.selected()
            .and_then(|b| self.providers.iter().find(|p| p.id == b.provider))
            .or_else(|| self.providers.iter().find(|p| !p.signed_in()))
            .or_else(|| self.providers.first())
    }
}

// ---- opening ---------------------------------------------------------------------------------

pub fn action(app: &mut App, action: &str) -> bool {
    match action {
        "sandboxes" => open(app, app.cur),
        _ => return false,
    }
    true
}

pub fn open(app: &mut App, mi: usize) {
    if !app.machines.get(mi).is_some_and(|m| m.connected()) {
        app.toast("that machine is offline");
        return;
    }
    let v = View::new(mi);
    let op = v.op;
    app.ux.sandboxes = Some(v);
    app.mode = Mode::Popup(Popup::Sandboxes);
    send(app, mi, op, "cloud.providers", json!({}), Call::Providers);
    list(app);
}

fn send(app: &mut App, mi: usize, op: u64, method: &str, params: Value, call: Call) {
    app.command_on(
        mi,
        method,
        params,
        Pending::Ux(crate::ux::Reply::Sandboxes(Reply { op, call })),
    );
}

/// `cloud.box.list {refresh: true}`.
fn list(app: &mut App) {
    let Some(v) = app.ux.sandboxes.as_mut() else {
        return;
    };
    v.loading = true;
    v.retry = None;
    let (mi, op) = (v.mi, v.op);
    send(
        app,
        mi,
        op,
        "cloud.box.list",
        json!({"refresh": true}),
        Call::List,
    );
}

fn close(app: &mut App) {
    app.ux.sandboxes = None;
    app.mode = Mode::Normal;
}

// ---- events ----------------------------------------------------------------------------------

/// `cloud.box.changed`: the row is replaced, added or (destroyed) removed.
pub fn on_box_changed(app: &mut App, mi: usize, b: BoxRow) {
    if let Some(v) = app.ux.sandboxes.as_mut()
        && v.mi == mi
    {
        v.upsert(b);
        app.dirty = true;
    }
}

/// `cloud.auth.changed {provider} => {state, account}`.
pub fn on_auth_changed(app: &mut App, mi: usize, provider: &str, data: &Value) {
    if let Some(v) = app.ux.sandboxes.as_mut()
        && v.mi == mi
    {
        for p in v.providers.iter_mut().filter(|p| p.id == provider) {
            p.apply_auth(data);
        }
        app.dirty = true;
    }
}

// ---- keys ------------------------------------------------------------------------------------

/// What a key asks the app to do (after the view's own state changed).
#[derive(Debug, Clone)]
pub enum Act {
    None,
    Close,
    Refresh,
    Open(BoxRow),
    /// Bring a sandbox's work back (`box`, name).
    BringBack(String, String),
    Call {
        method: &'static str,
        params: Value,
        call: Call,
        busy: String,
    },
    SignIn,
    Auth(AuthOut),
    LeaveAuth,
}

fn act_call(method: &'static str, params: Value, call: Call, busy: String) -> Act {
    Act::Call {
        method,
        params,
        call,
        busy,
    }
}

/// The view's key handling without the app.
pub fn on_key(v: &mut View, ev: &KeyEvent) -> Act {
    if ev.kind == KeyKind::Release {
        return Act::None;
    }
    if let Stage::Auth(a) = &mut v.stage {
        return match a.key(ev) {
            AuthOut::None => Act::None,
            AuthOut::Cancel => Act::LeaveAuth,
            out @ (AuthOut::SetToken(_) | AuthOut::Import(_)) => {
                a.busy = Some("signing in…".into());
                Act::Auth(out)
            }
            out => Act::Auth(out),
        };
    }
    let esc = matches!(ev.key, Key::Named(NamedKey::Escape));
    let plain = !ev.mods.ctrl() && !ev.mods.alt();
    let yes = plain && matches!(ev.key, Key::Char('y' | 'Y') | Key::Named(NamedKey::Enter));
    if let Some(c) = v.confirm.take() {
        return match c {
            Confirm::Destroy { box_id, name } if yes => act_call(
                "cloud.box.destroy",
                json!({"box": box_id}),
                Call::Destroy {
                    box_id,
                    name: name.clone(),
                    force: false,
                },
                format!("destroying {name}…"),
            ),
            Confirm::Unsynced { box_id, name, .. } if plain => match ev.key {
                Key::Char('b' | 'B') => Act::BringBack(box_id, name),
                Key::Char('d' | 'D') => act_call(
                    "cloud.box.destroy",
                    json!({"box": box_id, "force": true}),
                    Call::Destroy {
                        box_id,
                        name: name.clone(),
                        force: true,
                    },
                    format!("destroying {name}…"),
                ),
                _ => Act::None,
            },
            Confirm::Prune { .. } if yes => {
                act_call("cloud.prune", json!({}), Call::Prune, "cleaning up…".into())
            }
            // Anything else keeps the sandboxes.
            _ => Act::None,
        };
    }
    v.notice = None;
    let n = v.rows.len();
    let cur = v.rows.get(v.sel).cloned();
    let up = matches!(ev.key, Key::Char('k') | Key::Named(NamedKey::Up));
    let down = matches!(ev.key, Key::Char('j') | Key::Named(NamedKey::Down));
    if esc || (plain && matches!(ev.key, Key::Char('q'))) {
        return Act::Close;
    }
    if down {
        v.sel = (v.sel + 1).min(n.saturating_sub(1));
        return Act::None;
    }
    if up {
        v.sel = v.sel.saturating_sub(1);
        return Act::None;
    }
    if !plain {
        return Act::None;
    }
    // Keys that need no row.
    match ev.key {
        Key::Char('r' | 'g') => return Act::Refresh,
        Key::Char('s') => return Act::SignIn,
        Key::Char('C') => {
            return act_call(
                "cloud.prune",
                json!({"dry_run": true}),
                Call::PruneDry,
                "looking for sandboxes to clean up…".into(),
            );
        }
        _ => {}
    }
    let Some(b) = cur else {
        if matches!(
            ev.key,
            Key::Named(NamedKey::Enter) | Key::Char('b' | 'p' | 'c' | 'a' | 'f' | 'd')
        ) {
            v.notice = Some("no sandbox selected".into());
        }
        return Act::None;
    };
    match ev.key {
        Key::Named(NamedKey::Enter) => Act::Open(b),
        Key::Char('b') => Act::BringBack(b.box_id, b.name),
        Key::Char('p') => {
            if !b.can_suspend() {
                v.notice = Some(format!(
                    "{} suspends sandboxes by itself: there is nothing to do",
                    b.provider
                ));
                return Act::None;
            }
            if b.is_suspended() {
                act_call(
                    "cloud.box.resume",
                    json!({"box": b.box_id}),
                    Call::Resume,
                    format!("resuming {}…", b.name),
                )
            } else {
                act_call(
                    "cloud.box.suspend",
                    json!({"box": b.box_id}),
                    Call::Suspend,
                    format!("suspending {}…", b.name),
                )
            }
        }
        Key::Char('c') => {
            if !b.can_checkpoint() {
                v.notice = Some(format!("{} has no checkpoints", b.provider));
                return Act::None;
            }
            act_call(
                "cloud.box.checkpoint",
                json!({"box": b.box_id}),
                Call::Checkpoint {
                    name: b.name.clone(),
                },
                format!("checkpointing {}…", b.name),
            )
        }
        Key::Char('a') => {
            if matches!(b.ownership.as_str(), "orphaned" | "foreign") {
                act_call(
                    "cloud.box.adopt",
                    json!({"box": b.box_id}),
                    Call::Adopt {
                        name: b.name.clone(),
                    },
                    format!("adopting {}…", b.name),
                )
            } else {
                v.notice = Some("only an orphaned or foreign sandbox can be adopted".into());
                Act::None
            }
        }
        Key::Char('f') => {
            if b.ownership == "missing" {
                act_call(
                    "cloud.box.forget",
                    json!({"box": b.box_id}),
                    Call::Forget { box_id: b.box_id },
                    "forgetting…".into(),
                )
            } else {
                v.notice = Some("only a missing sandbox can be forgotten".into());
                Act::None
            }
        }
        Key::Char('d') => {
            v.confirm = Some(Confirm::Destroy {
                box_id: b.box_id,
                name: b.name,
            });
            Act::None
        }
        _ => Act::None,
    }
}

pub fn key(app: &mut App, ev: KeyEvent) {
    app.mode = Mode::Popup(Popup::Sandboxes);
    if ev.kind == KeyKind::Release {
        return;
    }
    app.dirty = true;
    let Some(v) = app.ux.sandboxes.as_mut() else {
        app.mode = Mode::Normal;
        return;
    };
    let act = on_key(v, &ev);
    run(app, act);
}

pub fn on_paste(app: &mut App, text: &str) {
    if let Some(v) = app.ux.sandboxes.as_mut()
        && let Stage::Auth(a) = &mut v.stage
    {
        a.paste(text);
        app.dirty = true;
    }
}

fn run(app: &mut App, act: Act) {
    match act {
        Act::None => {}
        Act::Close => close(app),
        Act::Refresh => list(app),
        Act::Open(b) => open_box(app, &b),
        Act::BringBack(box_id, name) => {
            let Some(mi) = app.ux.sandboxes.as_ref().map(|v| v.mi) else {
                return;
            };
            // The flow replaces this view; closing it brings the overview back to nothing.
            app.ux.sandboxes = None;
            crate::cloud::open_bring_back_box(app, mi, &box_id, &name);
        }
        Act::Call {
            method,
            params,
            call,
            busy,
        } => {
            let Some(v) = app.ux.sandboxes.as_mut() else {
                return;
            };
            v.busy = Some(busy);
            v.retried = false;
            v.retry = Some(Retry {
                method: method.into(),
                params: params.clone(),
                call: call.clone(),
            });
            let (mi, op) = (v.mi, v.op);
            send(app, mi, op, method, params, call);
        }
        Act::SignIn => sign_in(app),
        Act::Auth(out) => auth(app, out),
        Act::LeaveAuth => {
            if let Some(v) = app.ux.sandboxes.as_mut() {
                v.stage = Stage::List;
                v.retry = None;
                v.retried = false;
            }
        }
    }
}

fn sign_in(app: &mut App) {
    let Some(v) = app.ux.sandboxes.as_mut() else {
        return;
    };
    let st = v
        .sign_in_provider()
        .map(|p| AuthState::new(&p.id, &p.label, p.methods.clone()));
    match st {
        Some(a) => {
            v.retry = None;
            v.stage = Stage::Auth(a);
        }
        None => v.notice = Some("no cloud provider is available on this machine".into()),
    }
}

fn auth(app: &mut App, out: AuthOut) {
    if let AuthOut::Open(u) = &out {
        let mi = app.cur;
        crate::nav::open_url(app, mi, "", u);
        return;
    }
    let Some(v) = app.ux.sandboxes.as_mut() else {
        return;
    };
    let Stage::Auth(a) = &mut v.stage else {
        return;
    };
    let Some((method, params)) = crate::cloud::auth_request(&a.provider, &out) else {
        return;
    };
    // The token is in the request only: the field is empty from here on.
    a.token.clear();
    let call = if method == "cloud.auth.set" {
        Call::AuthSet
    } else {
        Call::AuthImport
    };
    let (mi, op) = (v.mi, v.op);
    send(app, mi, op, method, params, call);
}

/// Enter: focus the sandbox's pane, else its task's workspace.
fn open_box(app: &mut App, b: &BoxRow) {
    let Some(mi) = app.ux.sandboxes.as_ref().map(|v| v.mi) else {
        return;
    };
    let m = &app.machines[mi];
    let pane = b.panes.iter().find_map(|p| {
        m.model
            .panes
            .iter()
            .find(|x| &x.id == p || &x.handle == p)
            .map(|x| x.id.clone())
    });
    let ws = b.workspace.clone().or_else(|| {
        let t = b.task.as_ref()?;
        let task = m
            .model
            .tasks
            .iter()
            .find(|x| &x.id == t || &x.handle == t || &x.slug == t)?;
        task.workspace.clone()
    });
    let ws = ws.filter(|w| {
        m.model
            .workspaces
            .iter()
            .any(|x| &x.id == w || &x.handle == w)
    });
    if let Some(p) = pane {
        close(app);
        app.focus_pane(mi, &p);
        return;
    }
    if let Some(w) = ws {
        let id = app.machines[mi]
            .model
            .workspaces
            .iter()
            .find(|x| x.id == w || x.handle == w)
            .map(|x| x.id.clone())
            .unwrap_or(w);
        close(app);
        if crate::nav::focus_workspace(app, mi, &id) {
            return;
        }
    }
    app.toast(
        "this sandbox has no pane on this machine: press a to adopt it, or b to bring it back",
    );
}

// ---- replies ---------------------------------------------------------------------------------

pub fn on_reply(app: &mut App, mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    app.dirty = true;
    let Some(v) = app.ux.sandboxes.as_mut() else {
        return;
    };
    if v.op != r.op || v.mi != mi {
        return;
    }
    // A missing sign-in: the Auth stage, then the same call once more.
    if let Err(e) = &res
        && e.reason() == Some("needs_auth")
        && !matches!(r.call, Call::AuthSet | Call::AuthImport | Call::Providers)
    {
        v.busy = None;
        v.loading = false;
        if v.retried {
            v.retried = false;
            v.retry = None;
            v.notice = Some("✗ still not signed in: the sign-in did not work".into());
        } else if let Some(a) = crate::cloud::auth_stage(&v.providers, None, &e.details) {
            v.stage = Stage::Auth(a);
        } else {
            v.notice = Some("✗ sign in to the provider first (s)".into());
        }
        return;
    }
    let done = |v: &mut View| {
        v.busy = None;
        v.retried = false;
    };
    match (r.call, res) {
        (Call::Providers, Ok(x)) => {
            v.providers = crate::cloud::providers_of(&x);
            v.sort();
        }
        (Call::Providers, Err(_)) => {}
        (Call::List, Ok(x)) => {
            v.loading = false;
            v.retried = false;
            v.rows = x["boxes"]
                .as_array()
                .map(|a| a.iter().filter_map(BoxRow::from_value).collect())
                .unwrap_or_default();
            v.errors = x["errors"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|e| {
                            Some(ListError {
                                provider: s_of(e, "provider")?,
                                kind: s_of(e, "kind").unwrap_or_default(),
                                message: s_of(e, "message").unwrap_or_default(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            v.sort();
        }
        (Call::Suspend | Call::Resume, Ok(x)) => {
            done(v);
            if let Some(b) = BoxRow::from_value(&x) {
                v.upsert(b);
            }
        }
        (Call::Checkpoint { name }, Ok(x)) => {
            done(v);
            let id = x
                .get("checkpoint")
                .and_then(|c| s_of(c, "id").or_else(|| c.as_str().map(str::to_string)))
                .unwrap_or_default();
            v.notice = Some(
                format!("checkpoint of {name} saved {id}")
                    .trim()
                    .to_string(),
            );
        }
        (Call::Destroy { box_id, name, .. }, Ok(_)) => {
            done(v);
            v.rows.retain(|b| b.box_id != box_id);
            v.sort();
            v.notice = Some(format!("destroyed {name}"));
        }
        (
            Call::Destroy {
                box_id,
                name,
                force,
            },
            Err(e),
        ) => {
            done(v);
            if !force && e.kind == "conflict" && e.reason() == Some("unsynced_changes") {
                let summary = e
                    .details
                    .pointer("/unsynced/summary")
                    .and_then(Value::as_str)
                    .map(crate::devices::clean)
                    .unwrap_or_else(|| "work that exists only in the sandbox".into());
                v.confirm = Some(Confirm::Unsynced {
                    box_id,
                    name,
                    summary,
                });
            } else {
                v.notice = Some(format!("✗ {}", err_text(&e)));
            }
        }
        (Call::Adopt { name }, Ok(x)) => {
            done(v);
            let task = s_of(&x, "task")
                .map(|t| format!(" as task {t}"))
                .unwrap_or_default();
            v.notice = Some(format!("adopted {name}{task}"));
            list(app);
        }
        (Call::Forget { box_id }, Ok(_)) => {
            done(v);
            v.rows.retain(|b| b.box_id != box_id);
            v.sort();
        }
        (Call::PruneDry, Ok(x)) => {
            done(v);
            let lines: Vec<String> = x["candidates"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(BoxRow::from_value)
                        .map(|b| {
                            let un = if b.has_unsynced() {
                                "  ⚠ unsynced"
                            } else {
                                ""
                            };
                            format!("{} ({}, {}){un}", b.name, b.provider, b.ownership)
                        })
                        .collect()
                })
                .unwrap_or_default();
            if lines.is_empty() {
                v.notice = Some("nothing to clean up".into());
            } else {
                v.confirm = Some(Confirm::Prune { lines });
            }
        }
        (Call::Prune, Ok(x)) => {
            done(v);
            let n = x["destroyed"].as_array().map_or(0, Vec::len);
            let skipped = x["skipped"].as_array().map_or(0, Vec::len);
            v.notice = Some(if skipped > 0 {
                format!(
                    "cleaned up {n}; skipped {skipped} that still hold unsynced work or are in use"
                )
            } else {
                format!("cleaned up {n}")
            });
            list(app);
        }
        (Call::AuthSet | Call::AuthImport, Ok(x)) => auth_done(app, &x),
        (Call::AuthSet | Call::AuthImport, Err(e)) => {
            if let Stage::Auth(a) = &mut v.stage {
                a.busy = None;
                a.error = Some(err_text(&e));
            }
        }
        (Call::List, Err(e)) => {
            v.loading = false;
            v.retried = false;
            v.notice = Some(format!("✗ {}", err_text(&e)));
        }
        (_, Err(e)) => {
            done(v);
            v.notice = Some(format!("✗ {}", err_text(&e)));
        }
    }
}

/// Signed in: back to the list, read again, and the call that needed it repeated once.
fn auth_done(app: &mut App, x: &Value) {
    let Some(v) = app.ux.sandboxes.as_mut() else {
        return;
    };
    let provider = s_of(x, "provider");
    if let Stage::Auth(a) = &v.stage {
        let id = provider.clone().unwrap_or_else(|| a.provider.clone());
        for p in v.providers.iter_mut().filter(|p| p.id == id) {
            p.state = "ok".into();
            p.account = s_of(x, "account");
        }
    }
    v.stage = Stage::List;
    v.notice = Some(match s_of(x, "account") {
        Some(a) => format!("signed in as {a}"),
        None => "signed in".into(),
    });
    let retry = v.retry.take();
    let (mi, op) = (v.mi, v.op);
    match retry {
        Some(r) if !matches!(r.call, Call::List) => {
            v.retried = true;
            v.retry = Some(r.clone());
            v.busy = Some("trying again…".into());
            send(app, mi, op, &r.method, r.params, r.call);
        }
        _ => {}
    }
    list(app);
}

// ---- drawing ---------------------------------------------------------------------------------

/// "● running", "◌ suspended".
fn state_badge(b: &BoxRow) -> String {
    if b.is_running() {
        "● running".into()
    } else if b.is_suspended() {
        format!("◌ {}", b.state)
    } else {
        format!("○ {}", b.state)
    }
}

/// The task's title (else its handle), else the workspace.
fn where_label(app: &App, mi: usize, b: &BoxRow) -> String {
    let m = &app.machines[mi];
    if let Some(t) = &b.task {
        return m
            .model
            .tasks
            .iter()
            .find(|x| &x.id == t || &x.handle == t)
            .map_or_else(|| t.clone(), |x| x.title.clone());
    }
    if let Some(w) = &b.workspace {
        return m
            .model
            .workspaces
            .iter()
            .find(|x| &x.id == w || &x.handle == w)
            .map_or_else(|| w.clone(), |x| x.display_name().to_string());
    }
    "—".into()
}

/// One row: `name state ownership unsynced task/workspace panes age activity`.
fn row_line(app: &App, mi: usize, b: &BoxRow, now: i64) -> String {
    use crate::draw::truncate;
    let age = if b.created_at > 0 {
        fmt_age(now - b.created_at)
    } else {
        "—".into()
    };
    let act = if b.last_activity_at > 0 {
        format!("{} ago", fmt_age(now - b.last_activity_at))
    } else {
        "—".into()
    };
    // The unsynced marker comes early so a narrow view never cuts it off.
    format!(
        "{:<20} {:<12} {:<9} {:<11} {:<20} {:>2} pane{} {:>5} {:>8}",
        truncate(&b.name, 20),
        truncate(&state_badge(b), 12),
        b.ownership,
        if b.has_unsynced() { "⚠ unsynced" } else { "" },
        truncate(&where_label(app, mi, b), 20),
        b.panes.len(),
        if b.panes.len() == 1 { " " } else { "s" },
        age,
        act,
    )
}

pub fn draw(app: &App, g: &mut Grid) -> Option<(u16, u16)> {
    let v = app.ux.sandboxes.as_ref()?;
    let t = app.theme;
    let label = app.machines.get(v.mi).map_or("", |m| m.label.as_str());
    let mut a = Area::open(app, g, &format!("Vibeke · Sandboxes · {label}"));
    if let Stage::Auth(auth) = &v.stage {
        return auth.draw(app, &mut a);
    }
    let now = now_ms();
    let mut order: Vec<String> = v.providers.iter().map(|p| p.id.clone()).collect();
    for b in &v.rows {
        if !order.contains(&b.provider) {
            order.push(b.provider.clone());
        }
    }
    if order.is_empty() && v.loading {
        a.line("  loading…", t.dim());
    }
    for id in &order {
        let prow = v.providers.iter().find(|p| &p.id == id);
        let name = prow.map_or(id.as_str(), |p| p.label.as_str());
        match prow {
            Some(p) if p.signed_in() => {
                a.line(&format!("{name} — {}", p.auth_line()), t.bold(t.fg));
            }
            Some(p) => {
                a.line(
                    &format!("{name} — {} (s signs in)", p.auth_line()),
                    t.bold(t.yellow),
                );
            }
            None => {
                a.line(name, t.bold(t.fg));
            }
        }
        for e in v.errors.iter().filter(|e| &e.provider == id) {
            if e.kind != "needs_auth" {
                a.line(&format!("  ✗ {}", e.message), t.s(t.red));
            }
        }
        let mut any = false;
        for (i, b) in v.rows.iter().enumerate().filter(|(_, b)| &b.provider == id) {
            any = true;
            let on = i == v.sel;
            a.line(
                &format!(
                    "{} {}",
                    if on { "›" } else { " " },
                    row_line(app, v.mi, b, now)
                ),
                if on { t.sel(t.fg) } else { t.text() },
            );
        }
        if !any && !v.loading {
            a.line("    (no sandboxes)", t.dim());
        }
        a.line("", t.text());
    }
    let (running, idle) = v.counts();
    a.line(&format!("{running} running · {idle} idle"), t.dim());
    match &v.confirm {
        Some(Confirm::Destroy { name, .. }) => {
            a.line(
                &format!("Destroy {name}? Its files are deleted. [y] destroy  [n] keep"),
                t.bold(t.red),
            );
        }
        Some(Confirm::Unsynced { name, summary, .. }) => {
            a.line(
                &format!("{name} holds work that is not on this host: {summary}"),
                t.bold(t.red),
            );
            a.line(
                "[b] bring back first  [d] destroy anyway (the work is lost)  [n] keep",
                t.bold(t.red),
            );
        }
        Some(Confirm::Prune { lines }) => {
            a.line(
                &format!(
                    "Clean up {} sandbox(es)? [y] destroy  [n] keep",
                    lines.len()
                ),
                t.bold(t.red),
            );
            for l in lines {
                a.line(&format!("  {l}"), t.text());
            }
        }
        None => {}
    }
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
    a.footer(
        "j/k · enter open · b bring back · p suspend/resume · c checkpoint · a adopt · d destroy · C clean up · s sign in · r · esc",
        t.dim(),
    );
    None
}

#[cfg(test)]
#[path = "sandboxes_tests.rs"]
mod tests;
