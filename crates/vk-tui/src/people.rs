//! People in the TUI: the People tab of the Connections view ([`crate::connections`]; `people`
//! in the palette, `share_pane` on `prefix+alt+v` and in a pane's right-click menu): the
//! colleagues you shared a pane or a workspace with, on one machine's gateway (`gateway.call`,
//! like [`crate::sharing`]). Three stages:
//!
//! 1. **List** (`share.list`, only `kind == "share"`): the devices shares made (`devices`) and
//!    the links nobody has opened yet (`invitations`), each with who (the name given when
//!    sharing, else the device's own), what (the pane or workspace, named from the machine's
//!    model while it has them, else the id), the access (View, View + approve or Control) and
//!    when it
//!    ends. `x` revokes one, or cancels an unused link, after a confirm (`share.revoke {id}`),
//!    `r` reads the list again, `n` shares.
//! 2. **Share** (`n`: the machine's focused pane; `share_pane`: the focused or right-clicked
//!    pane): What (**This pane**, default, or **This workspace**, the pane's), Access (**View**,
//!    default; **View + approve**: may answer the agent's questions and approvals in that scope,
//!    not administer the host; **Control** (`scope: "full"`): may type into the pane(s) and
//!    prompt the agent, still refused every host-wide method), each with what it means (a
//!    warning for approve, a red one for Control, cautious unless the model says the pane runs
//!    contained), Expires (1h, 2h default, 8h, 24h, 7d) and an optional Name (the share's
//!    `label`). Enter creates the link: `share.create {kind: "share", scope, pane | workspace,
//!    ttl_s, label?}`; Control asks first ("Give … control of … until …? [y]"), and only `y`
//!    creates it.
//! 3. **Link**: what it grants and until when, that whoever opens it first gets the access, the
//!    link with its QR code; `c` copies it. Esc goes
//!    back to the list, where the unused link waits (and can be cancelled) until it is opened or
//!    expires.
//!
//! As in the Devices tab, `gateway.status` is asked on opening and every few seconds while a link
//! shows: while the gateway isn't online the link and its QR code are hidden and the view says
//! why. An older gateway (or server bridge) that refuses `kind: "share"` shows its own message.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde_json::{Value, json};
use unicode_width::UnicodeWidthStr;
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::devices::{GwHealth, POLL, clean, gateway_problem, wrap_lines};
use crate::handoff::epoch_ms;
use crate::inbox::fmt_age;
use crate::screen::Grid;
use crate::sharing::{bridge_error, draw_link_qr, gw_call_pending, unreachable};

/// Expiry choices (seconds, label); [`DEFAULT_TTL`] is preselected.
pub const TTLS: [(u64, &str); 5] = [
    (3600, "1h"),
    (2 * 3600, "2h"),
    (8 * 3600, "8h"),
    (24 * 3600, "24h"),
    (7 * 24 * 3600, "7d"),
];
const DEFAULT_TTL: usize = 1;
/// The most kept of a typed name.
const NAME_MAX: usize = 64;

/// Access choices: (scope, name), View first (the default).
pub const ACCESS: [(&str, &str); 3] = [
    ("view", "View"),
    ("approve", "View + approve"),
    ("full", "Control"),
];
/// [`ACCESS`] index of Control.
const CONTROL: usize = 2;

const VIEW_TEXT: &str = "watch the agent and its screen, read-only";
const FIRST_OPENS: &str = "Whoever opens this link first gets the access. Send it privately; revoke it in People when done.";

/// Tags each `share.create`, so a late answer can't land in a newer attempt or a reopened view.
static NEXT_ATTEMPT: AtomicU64 = AtomicU64::new(1);

fn now_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn s_of(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

// ---- state ---------------------------------------------------------------------------------------

/// A colleague's access: a device a share made, or a share link nobody has opened yet.
#[derive(Debug, Clone, PartialEq)]
pub struct ShareRow {
    /// The device id, or the unused link's pairing id (what `share.revoke` takes either way).
    pub id: String,
    /// An unused link (`invitations`), not a device.
    pub pending: bool,
    /// The name given when sharing, else the device's own; may be empty.
    pub who: String,
    /// "view", "approve" or "full".
    pub scope: String,
    pub workspace: Option<String>,
    pub pane: Option<String>,
    /// Unix seconds: when the access ends.
    pub expires_at: Option<i64>,
    /// Unix seconds: an unused link must be opened before this.
    pub link_expires_at: Option<i64>,
}

impl ShareRow {
    /// A `share.list` entry (`pending` for `invitations`); None for any other kind.
    fn from_value(v: &Value, pending: bool) -> Option<ShareRow> {
        if v.get("kind").and_then(Value::as_str) != Some("share") {
            return None;
        }
        let limit = &v["limit"];
        Some(ShareRow {
            id: s_of(v, "id")?,
            pending,
            who: s_of(v, "label")
                .or_else(|| s_of(v, "name"))
                .map(|s| clean(&s))
                .unwrap_or_default(),
            scope: s_of(v, "scope").unwrap_or_default().to_lowercase(),
            workspace: s_of(limit, "workspace"),
            pane: s_of(limit, "pane"),
            expires_at: if pending {
                v["device_expires_at"].as_i64()
            } else {
                v["expires_at"].as_i64()
            },
            link_expires_at: if pending {
                v["link_expires_at"].as_i64()
            } else {
                None
            },
        })
    }

    fn who(&self) -> String {
        if self.who.is_empty() {
            "someone".into()
        } else {
            crate::plugins::sanitize(&self.who, 40)
        }
    }
}

/// `x` asks first.
#[derive(Debug, Clone, PartialEq)]
pub struct Confirm {
    pub id: String,
    pub who: String,
    pub pending: bool,
}

/// What a share covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum What {
    Pane,
    Workspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    What,
    Access,
    Expires,
    Name,
    Create,
}

const FIELDS: [Field; 5] = [
    Field::What,
    Field::Access,
    Field::Expires,
    Field::Name,
    Field::Create,
];

/// The new-share form.
#[derive(Debug, Clone, PartialEq)]
pub struct Form {
    /// The pane **This pane** shares.
    pub pane: Option<String>,
    /// That pane's workspace (**This workspace**).
    pub workspace: Option<String>,
    pub what: What,
    /// Index into [`ACCESS`].
    pub access: usize,
    /// Index into [`TTLS`].
    pub ttl: usize,
    pub name: String,
    pub focus: Field,
    pub error: Option<String>,
    /// Enter with Control picked: asking "Give … control …? [y]" before the link is made.
    pub confirming: bool,
}

impl Form {
    /// For `pane` on machine `mi` (else that machine's focused pane).
    fn new(app: &App, mi: usize, pane: Option<String>) -> Form {
        let m = app.machines.get(mi);
        let pane = pane.or_else(|| m.and_then(|m| m.focus.pane.clone()));
        let workspace = m.and_then(|m| {
            pane.as_ref()
                .and_then(|p| m.model.panes.iter().find(|x| &x.id == p))
                .map(|p| p.workspace.clone())
                .or_else(|| m.focus.workspace.clone())
        });
        Form {
            what: if pane.is_some() {
                What::Pane
            } else {
                What::Workspace
            },
            pane,
            workspace,
            access: 0,
            ttl: DEFAULT_TTL,
            name: String::new(),
            focus: Field::What,
            error: None,
            confirming: false,
        }
    }

    /// `share.create`'s params.
    pub fn params(&self) -> Result<Value, String> {
        let scope = ACCESS[self.access].0;
        let mut p = json!({"kind": "share", "scope": scope});
        match (self.what, &self.pane, &self.workspace) {
            (What::Pane, Some(id), _) => p["pane"] = json!(id),
            (What::Workspace, _, Some(id)) => p["workspace"] = json!(id),
            (What::Pane, None, _) => return Err("no pane to share: focus one first".into()),
            (What::Workspace, _, None) => return Err("no workspace to share".into()),
        }
        p["ttl_s"] = json!(TTLS[self.ttl].0);
        let name = self.name.trim();
        if !name.is_empty() {
            p["label"] = json!(name);
        }
        Ok(p)
    }

    /// ←/→ (and space) on a choice.
    fn change(&mut self, by: i32) {
        match self.focus {
            Field::What => {
                self.what = match self.what {
                    What::Pane => What::Workspace,
                    What::Workspace => What::Pane,
                }
            }
            Field::Access => {
                self.access = (self.access as i32 + by).rem_euclid(ACCESS.len() as i32) as usize
            }
            Field::Expires => {
                self.ttl = (self.ttl as i32 + by).clamp(0, TTLS.len() as i32 - 1) as usize
            }
            Field::Name | Field::Create => {}
        }
    }
}

/// A link just created.
#[derive(Debug, Clone, PartialEq)]
pub struct Created {
    pub link: String,
    pub pid: String,
    /// Unix seconds: the link must be opened before this.
    pub open_by: Option<i64>,
    /// Unix seconds: when the access ends.
    pub until: Option<i64>,
    /// What it covers ("claude in api (w1:p1)", "workspace api").
    pub what: String,
    /// "view", "approve" or "full".
    pub scope: String,
    pub who: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    List,
    Form(Form),
    Created(Created),
}

#[derive(Debug, Clone)]
pub struct View {
    pub mi: usize,
    pub rows: Vec<ShareRow>,
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
    /// `gateway.status`, when the server answered it (older servers don't).
    pub gw: Option<GwHealth>,
    /// When `gateway.status` was last asked while a link shows.
    gw_asked: Option<Instant>,
    /// The `share.create` this view waits for; any other answer is an abandoned attempt.
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
            gw: None,
            gw_asked: None,
            creating: None,
        }
    }

    fn clamp(&mut self) {
        self.sel = self.sel.min(self.rows.len().saturating_sub(1));
    }
}

/// Replies routed back here (through `ux::Reply::People`).
#[derive(Debug, Clone)]
pub enum Reply {
    List,
    Revoke {
        who: String,
        pending: bool,
    },
    Create {
        attempt: u64,
        what: String,
        scope: String,
        who: Option<String>,
    },
    /// `share.revoke` of a link made for an abandoned attempt: nothing to show.
    Cancel,
    /// `gateway.status`: is the gateway online?
    GatewayInfo,
}

fn gw(app: &mut App, mi: usize, method: &str, params: Value, r: Reply) {
    gw_call_pending(
        app,
        mi,
        method,
        params,
        Pending::Ux(crate::ux::Reply::People(r)),
    );
}

fn view_mut(app: &mut App) -> Option<&mut View> {
    app.ux.people.as_mut()
}

// ---- names ---------------------------------------------------------------------------------------

/// "claude in api (w1:p1)", or the id when the machine doesn't have the pane (any more).
fn pane_name(app: &App, mi: usize, pane: &str) -> String {
    let known = app
        .machines
        .get(mi)
        .is_some_and(|m| m.model.panes.iter().any(|p| p.id == pane));
    if known {
        crate::handoff::pane_label(app, mi, pane)
    } else {
        format!("pane {}", crate::plugins::sanitize(pane, 40))
    }
}

/// "workspace api", or the id.
fn workspace_name(app: &App, mi: usize, ws: &str) -> String {
    let name = app
        .machines
        .get(mi)
        .and_then(|m| m.model.workspaces.iter().find(|w| w.id == ws))
        .map(|w| w.display_name().to_string());
    format!(
        "workspace {}",
        crate::plugins::sanitize(name.as_deref().unwrap_or(ws), 40)
    )
}

fn what_of(app: &App, mi: usize, r: &ShareRow) -> String {
    match (&r.pane, &r.workspace) {
        (Some(p), _) => pane_name(app, mi, p),
        (None, Some(w)) => workspace_name(app, mi, w),
        (None, None) => "this host".into(),
    }
}

fn access(scope: &str) -> &'static str {
    match scope {
        "approve" => "View + approve",
        "full" => "Control",
        _ => "View",
    }
}

/// The model says what the form shares runs contained (a sandbox, container or VM): the pane,
/// or every pane of the workspace. Unknown counts as not contained.
fn contained(app: &App, mi: usize, f: &Form) -> bool {
    let Some(m) = app.machines.get(mi) else {
        return false;
    };
    match (f.what, &f.pane, &f.workspace) {
        (What::Pane, Some(id), _) => m
            .model
            .panes
            .iter()
            .any(|p| &p.id == id && p.isolation.is_contained()),
        (What::Workspace, _, Some(ws)) => {
            let mut panes = m
                .model
                .panes
                .iter()
                .filter(|p| &p.workspace == ws)
                .peekable();
            panes.peek().is_some() && panes.all(|p| p.isolation.is_contained())
        }
        _ => false,
    }
}

/// What the picked access means, and its style: dim for View, a warning for approve, red for
/// Control.
fn access_text(app: &App, mi: usize, f: &Form) -> (String, vk_proto::render::Style) {
    let t = app.theme;
    let contained = contained(app, mi, f);
    match f.access {
        0 => (VIEW_TEXT.into(), t.dim()),
        1 => (
            format!(
                "They can say yes to what the agent asks — commands it runs as you{}",
                if contained {
                    ""
                } else if f.what == What::Pane {
                    ", with access to your whole machine (this pane's agent isn't sandboxed)"
                } else {
                    ", with access to your whole machine (this workspace's agents aren't sandboxed)"
                }
            ),
            t.s(t.yellow),
        ),
        _ => (
            format!(
                "Control lets them type into {} and prompt its agent — like a shell as you on this machine: they can read and change any file you can, unless the pane runs in a sandbox. Only share with someone you trust; keep the expiry short.",
                if f.what == What::Pane {
                    "this pane"
                } else {
                    "the panes of this workspace"
                }
            ),
            t.bold(t.red),
        ),
    }
}

/// The form's expiry as the confirmation says it: "14:30", or "7d from now".
fn until_of(f: &Form, now: i64) -> String {
    let ttl = TTLS[f.ttl].0 as i64;
    if ttl < 24 * 3600 {
        crate::statusbar::clock((now + ttl) * 1000)
    } else {
        format!("{} from now", TTLS[f.ttl].1)
    }
}

// ---- open / refresh ------------------------------------------------------------------------------

/// Open the People tab on machine `mi`: on the list, or with `pane` straight on the form.
pub(crate) fn open_on(app: &mut App, mi: usize, pane: Option<String>) {
    if !app.machines.get(mi).is_some_and(|m| m.connected()) {
        app.toast("that machine is offline");
        return;
    }
    let mut v = View::new(mi);
    if pane.is_some() {
        v.stage = Stage::Form(Form::new(app, mi, pane));
    }
    app.ux.people = Some(v);
    app.mode = Mode::Popup(Popup::People);
    refresh(app);
    ask_gateway_status(app, mi);
}

fn refresh(app: &mut App) {
    let Some(v) = view_mut(app) else {
        return;
    };
    v.loading = true;
    v.error = None;
    let mi = v.mi;
    gw(app, mi, "share.list", json!({}), Reply::List);
}

/// Ask the server for `gateway.status` (a server method, not bridged to the gateway).
fn ask_gateway_status(app: &mut App, mi: usize) {
    app.command_on(
        mi,
        "gateway.status",
        json!({}),
        Pending::Ux(crate::ux::Reply::People(Reply::GatewayInfo)),
    );
}

fn close(app: &mut App) {
    app.ux.people = None;
    app.mode = Mode::Normal;
}

/// Leaving the tab for another: a link already made stays valid (and listed).
pub(crate) fn leave(app: &mut App) {
    app.ux.people = None;
}

/// The form's name field has the keys.
pub(crate) fn typing(app: &App) -> bool {
    app.ux
        .people
        .as_ref()
        .is_some_and(|v| matches!(&v.stage, Stage::Form(f) if f.focus == Field::Name))
}

/// Why the link can't be used right now: the gateway is unreachable, or not online.
fn link_blocked(v: &View) -> Option<String> {
    gateway_problem(v.error.is_some(), v.gw.as_ref(), "your colleague")
}

// ---- polling -------------------------------------------------------------------------------------

/// How long until `gateway.status` is asked again while a link shows.
fn gw_every(v: &View) -> std::time::Duration {
    if link_blocked(v).is_some() {
        2 * POLL
    } else {
        5 * POLL
    }
}

/// While a link shows, keep checking the gateway: a link that can't be used yet waits for it to
/// come back, a usable one is hidden as soon as the relay drops.
pub fn tick(app: &mut App) {
    if !matches!(app.mode, Mode::Popup(Popup::People)) {
        return;
    }
    let now = Instant::now();
    let Some(v) = view_mut(app) else {
        return;
    };
    if !matches!(v.stage, Stage::Created(_)) {
        return;
    }
    let every = gw_every(v);
    if v.gw_asked.is_none_or(|t| now.duration_since(t) >= every) {
        v.gw_asked = Some(now);
        let mi = v.mi;
        ask_gateway_status(app, mi);
    }
}

pub fn deadlines(app: &App, now: Instant, d: &mut crate::deadline::Deadlines) {
    if let (Mode::Popup(Popup::People), Some(v)) = (&app.mode, &app.ux.people)
        && matches!(v.stage, Stage::Created(_))
    {
        d.at(
            "people.gateway",
            v.gw_asked.map_or(now, |t| t + gw_every(v)),
        );
    }
}

// ---- actions -------------------------------------------------------------------------------------

/// `x` on the selected row: ask first.
fn ask_revoke(v: &mut View) {
    match v.rows.get(v.sel) {
        Some(r) => {
            v.confirm = Some(Confirm {
                id: r.id.clone(),
                who: r.who(),
                pending: r.pending,
            })
        }
        None => v.notice = Some("nothing selected".into()),
    }
}

fn revoke(app: &mut App, c: Confirm) {
    let Some(v) = view_mut(app) else {
        return;
    };
    v.busy = Some(if c.pending {
        format!("cancelling the link for {}…", c.who)
    } else {
        format!("revoking {}'s access…", c.who)
    });
    let mi = v.mi;
    gw(
        app,
        mi,
        "share.revoke",
        json!({"id": c.id}),
        Reply::Revoke {
            who: c.who,
            pending: c.pending,
        },
    );
}

/// `n`: the form for the machine's focused pane.
fn new_share(app: &mut App) {
    let Some(mi) = app.ux.people.as_ref().map(|v| v.mi) else {
        return;
    };
    let f = Form::new(app, mi, None);
    if let Some(v) = view_mut(app) {
        v.notice = None;
        v.stage = Stage::Form(f);
    }
}

/// Enter in the form: checked here first; Control asks before anything is sent.
fn submit(app: &mut App) {
    let Some(v) = view_mut(app) else {
        return;
    };
    let Stage::Form(f) = &mut v.stage else {
        return;
    };
    if let Err(e) = f.params() {
        f.error = Some(e);
        return;
    }
    if f.access == CONTROL {
        f.confirming = true;
        return;
    }
    create(app);
}

/// `share.create` for the form as it stands.
fn create(app: &mut App) {
    let Some((mi, f)) = app.ux.people.as_ref().and_then(|v| match &v.stage {
        Stage::Form(f) => Some((v.mi, f.clone())),
        _ => None,
    }) else {
        return;
    };
    let params = match f.params() {
        Ok(p) => p,
        Err(e) => {
            if let Some(Stage::Form(f)) = view_mut(app).map(|v| &mut v.stage) {
                f.error = Some(e);
            }
            return;
        }
    };
    let what = match f.what {
        What::Pane => f.pane.as_deref().map(|p| pane_name(app, mi, p)),
        What::Workspace => f.workspace.as_deref().map(|w| workspace_name(app, mi, w)),
    }
    .unwrap_or_default();
    let who = Some(f.name.trim().to_string()).filter(|n| !n.is_empty());
    let attempt = NEXT_ATTEMPT.fetch_add(1, Ordering::Relaxed);
    let Some(v) = view_mut(app) else {
        return;
    };
    v.busy = Some("creating a share link…".into());
    v.notice = None;
    v.creating = Some(attempt);
    gw(
        app,
        mi,
        "share.create",
        params,
        Reply::Create {
            attempt,
            what,
            scope: ACCESS[f.access].0.to_string(),
            who,
        },
    );
}

// ---- keys ----------------------------------------------------------------------------------------

pub fn key(app: &mut App, ev: KeyEvent) {
    app.mode = Mode::Popup(Popup::People);
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
                Key::Char('n') if plain => new_share(app),
                Key::Char('r' | 'g') if plain => refresh(app),
                _ => {}
            }
        }
        Stage::Form(f) if f.confirming => {
            // Only y gives control; anything else goes back to the form.
            f.confirming = false;
            if plain && matches!(ev.key, Key::Char('y')) {
                create(app);
            }
        }
        Stage::Form(f) => {
            if esc {
                // A link still being made is cancelled when its answer comes.
                v.busy = None;
                v.creating = None;
                v.stage = Stage::List;
            } else if v.busy.is_none() && form_key(f, &ev) {
                submit(app);
            }
        }
        Stage::Created(c) => {
            if quit {
                // The link stays valid until it is opened or expires; the list shows it.
                v.stage = Stage::List;
            } else if plain && matches!(ev.key, Key::Char('c')) && !blocked {
                let link = c.link.clone();
                app.copy_text(&link);
            }
        }
    }
}

/// A key in the form; true to create the link.
fn form_key(f: &mut Form, ev: &KeyEvent) -> bool {
    let plain = !ev.mods.ctrl() && !ev.mods.alt();
    let naming = f.focus == Field::Name;
    let n = FIELDS.len();
    let at = FIELDS.iter().position(|x| *x == f.focus).unwrap_or(0);
    let next = FIELDS[(at + 1) % n];
    let prev = FIELDS[(at + n - 1) % n];
    match &ev.key {
        Key::Named(NamedKey::Enter) => return true,
        // Tab reaches here only from the name field (elsewhere it switches tabs).
        Key::Named(NamedKey::Tab) if ev.mods.shift() => f.focus = prev,
        Key::Named(NamedKey::Tab) | Key::Named(NamedKey::Down) => f.focus = next,
        Key::Named(NamedKey::Up) => f.focus = prev,
        Key::Char('j') if plain && !naming => f.focus = next,
        Key::Char('k') if plain && !naming => f.focus = prev,
        Key::Named(NamedKey::Left) => f.change(-1),
        Key::Char('h') if plain && !naming => f.change(-1),
        Key::Named(NamedKey::Right) | Key::Char('l') if !naming => f.change(1),
        Key::Named(NamedKey::Space) | Key::Char(' ') if !naming => {
            if f.focus == Field::Create {
                return true;
            }
            f.change(1);
        }
        Key::Named(NamedKey::Backspace) if naming => {
            f.name.pop();
        }
        Key::Char('u') if ev.mods.ctrl() && naming => f.name.clear(),
        Key::Named(NamedKey::Space) if naming => push_name(f, " "),
        Key::Char(c) if plain && naming => push_name(f, &c.to_string()),
        _ => {}
    }
    f.error = None;
    false
}

/// Typed or pasted into the name, cleaned and capped at [`NAME_MAX`].
fn push_name(f: &mut Form, s: &str) {
    for c in s.chars().filter(|c| !c.is_control()) {
        if f.name.chars().count() >= NAME_MAX {
            break;
        }
        f.name.push(c);
    }
}

/// A bracketed paste: into the name field when it has the keys.
pub fn on_paste(app: &mut App, text: &str) {
    let Some(v) = view_mut(app) else {
        return;
    };
    if let Stage::Form(f) = &mut v.stage
        && f.focus == Field::Name
    {
        push_name(f, &text.replace(['\n', '\r'], " "));
        app.dirty = true;
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

/// `share.create` failed: in the form when it is still open (an older gateway refusing a share
/// says so there), else the notice.
fn create_failed(v: &mut View, msg: String) {
    match &mut v.stage {
        Stage::Form(f) => f.error = Some(msg),
        _ => v.notice = Some(format!("✗ {msg}")),
    }
}

pub fn on_reply(app: &mut App, mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    app.dirty = true;
    // An answer for an attempt nobody waits for any more (backed out, closed, a newer attempt) is
    // dropped, and a link made for it must not stay usable.
    if let Reply::Create { attempt, .. } = &r
        && !app
            .ux
            .people
            .as_ref()
            .is_some_and(|v| v.mi == mi && v.creating == Some(*attempt))
    {
        if let Ok(x) = &res
            && let Some(pid) = s_of(x, "pid")
        {
            gw(app, mi, "share.revoke", json!({"id": pid}), Reply::Cancel);
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
                    let mut rows: Vec<ShareRow> = x["devices"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|d| ShareRow::from_value(d, false))
                                .collect()
                        })
                        .unwrap_or_default();
                    if let Some(a) = x["invitations"].as_array() {
                        rows.extend(a.iter().filter_map(|i| ShareRow::from_value(i, true)));
                    }
                    v.rows = rows;
                    v.error = None;
                }
                Err(e) => failed(v, &e),
            }
            v.clamp();
        }
        Reply::Revoke { who, pending } => {
            v.busy = None;
            match res {
                Ok(_) => {
                    v.notice = Some(if pending {
                        format!("cancelled the link for {who}")
                    } else {
                        format!("revoked {who}'s access: their link no longer works")
                    });
                    refresh(app);
                }
                Err(e) => failed(v, &e),
            }
        }
        Reply::Create {
            what, scope, who, ..
        } => {
            v.busy = None;
            v.creating = None;
            match res {
                Ok(x) => match (s_of(&x, "link"), s_of(&x, "pid")) {
                    (Some(link), Some(pid)) => {
                        v.notice = None;
                        v.gw_asked = Some(Instant::now());
                        v.stage = Stage::Created(Created {
                            link,
                            pid,
                            open_by: x["open_by"].as_i64(),
                            until: x["expires_at"].as_i64(),
                            what,
                            scope,
                            who,
                        });
                        ask_gateway_status(app, mi);
                        refresh(app);
                    }
                    _ => create_failed(v, "the gateway returned no link".into()),
                },
                Err(e) => {
                    let msg = bridge_error(&e);
                    if unreachable(&e) {
                        v.error = Some(msg.clone());
                    }
                    create_failed(v, msg);
                }
            }
        }
        Reply::Cancel => {}
        Reply::GatewayInfo => {
            // An older server doesn't know the method: behave as before.
            if let Ok(x) = res {
                v.gw = Some(GwHealth::from_value(&x));
            }
        }
    }
}

// ---- drawing -------------------------------------------------------------------------------------

/// Draw the view; the cursor position in the name field.
pub fn draw(app: &App, g: &mut Grid) -> Option<(u16, u16)> {
    let v = app.ux.people.as_ref()?;
    let t = app.theme;
    let mut a = crate::connections::area(app, g, crate::connections::Tab::People, v.mi);
    if let Some(e) = &v.error {
        a.line(&format!("⚠ {e}"), t.bold(t.yellow));
        a.line("", t.text());
    }
    match &v.stage {
        Stage::List => {
            draw_list(app, &mut a, v);
            None
        }
        Stage::Form(f) => draw_form(app, &mut a, v, f),
        Stage::Created(c) => {
            draw_created(app, &mut a, v, c);
            None
        }
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

fn row_line(app: &App, mi: usize, r: &ShareRow, now_ms: i64) -> String {
    let mut parts = vec![r.who(), what_of(app, mi, r), access(&r.scope).to_string()];
    match r.expires_at.map(epoch_ms) {
        Some(t) if t > now_ms => parts.push(format!("{} left", fmt_age(t - now_ms))),
        Some(_) => parts.push("expired".into()),
        None => {}
    }
    if r.pending {
        parts.push(match r.link_expires_at.map(epoch_ms) {
            Some(t) if t > now_ms => {
                format!("link not opened yet (open for {})", fmt_age(t - now_ms))
            }
            _ => "link not opened yet".into(),
        });
    }
    parts.join(" · ")
}

fn draw_list(app: &App, a: &mut crate::drafts::Area<'_>, v: &View) {
    let t = app.theme;
    let now_ms = now_s() * 1000;
    a.line("People you shared a pane or workspace with", t.bold(t.fg));
    a.line("", t.text());
    if v.rows.is_empty() {
        a.line(
            if v.loading {
                "  loading…"
            } else {
                "  Nobody yet — press n to share this pane with a colleague"
            },
            t.dim(),
        );
    }
    for (i, r) in v.rows.iter().enumerate() {
        let on = i == v.sel;
        a.line(
            &format!(
                "{} {}",
                if on { "›" } else { " " },
                row_line(app, v.mi, r, now_ms)
            ),
            if on { t.sel(t.fg) } else { t.text() },
        );
    }
    a.line("", t.text());
    if let Some(c) = &v.confirm {
        let q = if c.pending {
            format!(
                "Cancel the unused link for {}? It stops working. [y] cancel it  [other] keep",
                c.who
            )
        } else {
            let what = v
                .rows
                .iter()
                .find(|r| r.id == c.id)
                .map(|r| what_of(app, v.mi, r))
                .unwrap_or_else(|| "this host".into());
            format!(
                "Revoke {}'s access to {what}? Their link stops working. [y] revoke  [other] keep",
                c.who
            )
        };
        wrap_lines(a, &q, t.bold(t.red));
    } else {
        status_line(app, a, v);
    }
    a.footer(
        "j/k move · n share this pane · x revoke · r refresh · esc",
        t.dim(),
    );
}

fn draw_form(app: &App, a: &mut crate::drafts::Area<'_>, v: &View, f: &Form) -> Option<(u16, u16)> {
    let t = app.theme;
    a.line("Share with someone", t.bold(t.fg));
    a.line("", t.text());
    let mark = |x: Field| if f.focus == x { "›" } else { " " };
    let st = |x: Field| if f.focus == x { t.sel(t.fg) } else { t.text() };
    let pick = |on: bool, s: &str| {
        if on {
            format!("[{s}]")
        } else {
            format!(" {s} ")
        }
    };
    let what = match f.what {
        What::Pane => f.pane.as_deref().map_or_else(
            || "no focused pane".to_string(),
            |p| pane_name(app, v.mi, p),
        ),
        What::Workspace => f.workspace.as_deref().map_or_else(
            || "no workspace".to_string(),
            |w| workspace_name(app, v.mi, w),
        ),
    };
    a.line(
        &format!(
            "{} What     {} {}",
            mark(Field::What),
            pick(f.what == What::Pane, "This pane"),
            pick(f.what == What::Workspace, "This workspace")
        ),
        st(Field::What),
    );
    a.line(&format!("           {what}"), t.dim());
    let levels: Vec<String> = ACCESS
        .iter()
        .enumerate()
        .map(|(i, (_, name))| pick(i == f.access, name))
        .collect();
    a.line(
        &format!("{} Access   {}", mark(Field::Access), levels.join(" ")),
        st(Field::Access),
    );
    let (warning, warning_st) = access_text(app, v.mi, f);
    wrap_lines(a, &format!("           {warning}"), warning_st);
    let ttls: Vec<String> = TTLS
        .iter()
        .enumerate()
        .map(|(i, (_, l))| pick(i == f.ttl, l))
        .collect();
    a.line(
        &format!("{} Expires  {}", mark(Field::Expires), ttls.join(" ")),
        st(Field::Expires),
    );
    let prefix = format!("{} Name     ", mark(Field::Name));
    let r = a.rest();
    let cursor = (
        r.x + (UnicodeWidthStr::width(prefix.as_str()) + UnicodeWidthStr::width(f.name.as_str()))
            as u16,
        r.y,
    );
    if f.name.is_empty() && f.focus != Field::Name {
        a.line(
            &format!("{prefix}(optional: who it's for)"),
            st(Field::Name),
        );
    } else {
        a.line(&format!("{prefix}{}", f.name), st(Field::Name));
    }
    a.line("", t.text());
    a.line(
        if f.focus == Field::Create {
            "  [> Create link <]"
        } else {
            "  [ Create link ]"
        },
        if f.focus == Field::Create {
            t.bold(t.accent)
        } else {
            t.text()
        },
    );
    if f.confirming {
        let who = match f.name.trim() {
            "" => "whoever opens the link".to_string(),
            n => crate::plugins::sanitize(n, 40),
        };
        a.line("", t.text());
        wrap_lines(
            a,
            &format!(
                "Give {who} control of {what} until {}? [y] yes, create the link  [n] back",
                until_of(f, now_s())
            ),
            t.bold(t.red),
        );
    } else if let Some(b) = &v.busy {
        a.line(&format!("⏳ {b}"), t.s(t.yellow));
    } else if let Some(e) = &f.error {
        wrap_lines(a, &format!("✗ {e}"), t.s(t.red));
    }
    a.footer(
        if f.confirming {
            "y create the link · n back"
        } else {
            "↑/↓ move · ←/→ change · enter create the link · esc back"
        },
        t.dim(),
    );
    (f.focus == Field::Name).then_some(cursor)
}

/// " until 14:30" within a day, else " for 7d".
fn until_text(until: i64, now: i64) -> String {
    if until - now < 24 * 3600 {
        format!(" until {}", crate::statusbar::clock(until * 1000))
    } else {
        format!(" for {}", fmt_age((until - now) * 1000))
    }
}

fn draw_created(app: &App, a: &mut crate::drafts::Area<'_>, v: &View, c: &Created) {
    let t = app.theme;
    let now = now_s();
    a.line(
        &match &c.who {
            Some(w) => format!("Share link for {}", crate::plugins::sanitize(w, 40)),
            None => "Share link".into(),
        },
        t.bold(t.fg),
    );
    let can = match c.scope.as_str() {
        "approve" => "view and answer the agent's questions and approvals in",
        "full" => "control (type into and prompt the agent of)",
        _ => "view",
    };
    let until = c.until.map(|u| until_text(u, now)).unwrap_or_default();
    wrap_lines(
        a,
        &format!(
            "Anyone with this link can {can} {}{until} — send it to your colleague; they open it in a browser.",
            c.what
        ),
        t.text(),
    );
    if let Some(open) = c.open_by.filter(|o| *o > now) {
        a.line(
            &format!(
                "The link works once and must be opened within {}.",
                fmt_age((open - now) * 1000)
            ),
            t.dim(),
        );
    }
    wrap_lines(a, FIRST_OPENS, t.dim());
    status_line(app, a, v);
    a.line("", t.text());
    let blocked = link_blocked(v);
    match &blocked {
        Some(why) => wrap_lines(a, why, t.bold(t.yellow)),
        None => draw_link_qr(app, a, &c.link),
    }
    a.footer(
        if blocked.is_some() {
            "esc back to the list (the link stays valid)"
        } else {
            "c copy link · esc back to the list (the link stays valid)"
        },
        t.dim(),
    );
}

#[cfg(test)]
#[path = "people_tests.rs"]
mod tests;
