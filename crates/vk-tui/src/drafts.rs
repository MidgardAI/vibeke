//! Drafts composer and workspace notes (08 §6.7, research R3; server: 07 §2.14a `draft.*`,
//! `notes.*`).
//!
//! - **Drafts** (`:drafts`, agent peek `d`, task details `D`): a full pane-area view listing the
//!   scope's drafts in user order with attachment counts and the last send state. Keys: `n` new,
//!   `e`/`enter` edit (multi-line editor: `ctrl+x` save, `ctrl+f` attach a file, `ctrl+s` attach
//!   the latest screenshot, `esc` discard), `J/K` reorder, `space` select, `m` combine selected,
//!   `x` delete (confirm), `s` send, `R` reconcile an uncertain send, `tab` notes.
//! - **Send** shows `draft.check` for the chosen target run (`send_path`, the reason when only
//!   "Open pane to send" is possible, "steer: not supported") and an "include notes" toggle. The
//!   send is dispatched through [`App::mutate`], so its idempotency key is persisted in the
//!   pending-operations file before anything leaves the client and reconciled after a restart;
//!   a refused send keeps the draft, wrote zero bytes and offers `o` **Open pane to send**.
//! - **Notes** (`:notes`, the second tab): one plain-text document per workspace, "Never sent
//!   unless you include it", saved with `expected_rev` (a conflict asks before overwriting).
//! - The agent peek's reply box gains **Save as draft** (`ctrl+d`).
//!
//! [`TextEditor`] and [`Area`] are shared with the desk and assist views.

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::draw::truncate;
use crate::screen::{Grid, Rect as SRect};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};
use vk_proto::render::Style;

pub(crate) fn st<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

pub(crate) fn arr<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get(k)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub(crate) fn ctrl(ev: &KeyEvent, c: char) -> bool {
    ev.mods.ctrl() && ev.key == Key::Char(c)
}

// ---- text editor ---------------------------------------------------------------------------------

/// A small multi-line (or single-line) text editor for drafts, notes, desk context packages and
/// assistant outputs. The cursor is a (line, char) position.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TextEditor {
    pub lines: Vec<String>,
    pub row: usize,
    pub col: usize,
    pub multiline: bool,
    pub dirty: bool,
}

impl TextEditor {
    pub fn new(text: &str, multiline: bool) -> Self {
        let lines: Vec<String> = if multiline {
            text.split('\n').map(str::to_string).collect()
        } else {
            vec![text.replace(['\n', '\r'], " ")]
        };
        let row = lines.len().saturating_sub(1);
        let col = lines[row].chars().count();
        TextEditor {
            lines,
            row,
            col,
            multiline,
            dirty: false,
        }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    fn byte(&self, row: usize, col: usize) -> usize {
        self.lines[row]
            .char_indices()
            .nth(col)
            .map(|(i, _)| i)
            .unwrap_or(self.lines[row].len())
    }

    fn len(&self, row: usize) -> usize {
        self.lines[row].chars().count()
    }

    pub fn insert_char(&mut self, c: char) {
        let b = self.byte(self.row, self.col);
        self.lines[self.row].insert(b, c);
        self.col += 1;
        self.dirty = true;
    }

    pub fn newline(&mut self) {
        if !self.multiline {
            return;
        }
        let b = self.byte(self.row, self.col);
        let rest = self.lines[self.row].split_off(b);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
        self.dirty = true;
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            match c {
                '\r' => {}
                '\n' if self.multiline => self.newline(),
                '\n' => self.insert_char(' '),
                c if !c.is_control() || c == '\t' => self.insert_char(c),
                _ => {}
            }
        }
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            let b = self.byte(self.row, self.col - 1);
            self.lines[self.row].remove(b);
            self.col -= 1;
            self.dirty = true;
        } else if self.row > 0 {
            let cur = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.len(self.row);
            self.lines[self.row].push_str(&cur);
            self.dirty = true;
        }
    }

    fn delete(&mut self) {
        if self.col < self.len(self.row) {
            let b = self.byte(self.row, self.col);
            self.lines[self.row].remove(b);
            self.dirty = true;
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
            self.dirty = true;
        }
    }

    /// Editing keys. Returns false for keys the editor doesn't use (the caller's shortcuts).
    pub fn key(&mut self, ev: &KeyEvent) -> bool {
        match ev.key {
            Key::Char('u') if ev.mods.ctrl() => {
                self.lines[self.row].clear();
                self.col = 0;
                self.dirty = true;
            }
            Key::Char(c) if !ev.mods.ctrl() && !ev.mods.alt() => self.insert_char(c),
            Key::Named(NamedKey::Enter) if self.multiline => self.newline(),
            Key::Named(NamedKey::Backspace) => self.backspace(),
            Key::Named(NamedKey::Delete) => self.delete(),
            Key::Named(NamedKey::Left) => {
                if self.col > 0 {
                    self.col -= 1;
                } else if self.row > 0 {
                    self.row -= 1;
                    self.col = self.len(self.row);
                }
            }
            Key::Named(NamedKey::Right) => {
                if self.col < self.len(self.row) {
                    self.col += 1;
                } else if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.col = 0;
                }
            }
            Key::Named(NamedKey::Up) if self.row > 0 => {
                self.row -= 1;
                self.col = self.col.min(self.len(self.row));
            }
            Key::Named(NamedKey::Down) if self.row + 1 < self.lines.len() => {
                self.row += 1;
                self.col = self.col.min(self.len(self.row));
            }
            Key::Named(NamedKey::Home) => self.col = 0,
            Key::Named(NamedKey::End) => self.col = self.len(self.row),
            Key::Named(NamedKey::Up | NamedKey::Down) => {}
            _ => return false,
        }
        true
    }

    /// Draw the editor into `r`, scrolled so the cursor line is visible; the cursor cell is
    /// shown reversed.
    pub fn draw(&self, g: &mut Grid, r: SRect, text: Style, cursor: Style) {
        if r.h == 0 || r.w == 0 {
            return;
        }
        let top = self.row.saturating_sub(r.h as usize - 1);
        for (i, l) in self.lines.iter().enumerate().skip(top).take(r.h as usize) {
            let y = r.y + (i - top) as u16;
            let skip = if i == self.row {
                self.col.saturating_sub(r.w as usize - 1)
            } else {
                0
            };
            let shown: String = l.chars().skip(skip).collect();
            g.put_str(r.x, y, &shown, text, r.w);
            if i == self.row {
                let cx = (self.col - skip) as u16;
                let ch = l.chars().nth(self.col).unwrap_or(' ');
                if cx < r.w {
                    g.put_str(r.x + cx, y, &ch.to_string(), cursor, 1);
                }
            }
        }
    }
}

// ---- pane-area drawing helper ----------------------------------------------------------------------

/// A full pane-area view: title row, lines, footer row (like the inbox and task details).
pub(crate) struct Area<'a> {
    pub g: &'a mut Grid,
    pub r: SRect,
    pub y: u16,
}

impl<'a> Area<'a> {
    pub fn open(app: &App, g: &'a mut Grid, title: &str) -> Area<'a> {
        let a = app.pane_area();
        let r = SRect {
            x: a.x,
            y: a.y,
            w: a.w,
            h: a.h,
        };
        let t = app.theme;
        g.fill(r, t.text());
        g.put_str(r.x + 1, r.y, title, t.bold(t.accent), r.w.saturating_sub(2));
        Area { g, r, y: r.y + 1 }
    }
    /// Last row usable for body lines (the footer row is below it).
    pub fn bottom(&self) -> u16 {
        self.r.y + self.r.h.saturating_sub(1)
    }
    pub fn left(&self) -> u16 {
        self.bottom().saturating_sub(self.y)
    }
    pub fn line(&mut self, s: &str, st: Style) {
        if self.y >= self.bottom() {
            return;
        }
        self.g
            .put_str(self.r.x + 1, self.y, s, st, self.r.w.saturating_sub(2));
        self.y += 1;
    }
    pub fn footer(&mut self, s: &str, st: Style) {
        let y = self.bottom();
        self.g
            .put_str(self.r.x + 1, y, s, st, self.r.w.saturating_sub(2));
    }
    /// The remaining body area as a rect (for editors).
    pub fn rest(&self) -> SRect {
        SRect {
            x: self.r.x + 1,
            y: self.y,
            w: self.r.w.saturating_sub(2),
            h: self.left(),
        }
    }
}

// ---- state -----------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Reply {
    List {
        view: u64,
    },
    Saved {
        view: u64,
    },
    /// `draft.create` from the peek reply box (no view).
    ReplySaved,
    Reordered {
        view: u64,
    },
    Deleted {
        view: u64,
    },
    Combined {
        view: u64,
    },
    Check {
        view: u64,
        run: String,
    },
    Sent {
        view: u64,
        draft: String,
    },
    Reconciled {
        view: u64,
    },
    Notes {
        view: u64,
    },
    NotesSaved {
        view: u64,
    },
    LatestShot {
        view: u64,
    },
    Attached {
        view: u64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SendInfo {
    pub state: String,
    pub detail: Option<String>,
    pub reconciled: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DraftItem {
    pub id: String,
    pub title: Option<String>,
    pub text: String,
    pub attachments: usize,
    pub rev: u64,
    pub send: Option<SendInfo>,
}

impl DraftItem {
    pub fn parse(v: &Value) -> Option<DraftItem> {
        let id = st(v, "id");
        if id.is_empty() {
            return None;
        }
        let send = arr(v, "sends").last().map(|s| SendInfo {
            state: st(s, "state").to_string(),
            detail: s.get("detail").and_then(Value::as_str).map(str::to_string),
            reconciled: s.get("reconciled").and_then(Value::as_bool) == Some(true),
        });
        Some(DraftItem {
            id: id.into(),
            title: v
                .get("title")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .map(str::to_string),
            text: st(v, "text").to_string(),
            attachments: arr(v, "attachments").len(),
            rev: v.get("rev").and_then(Value::as_u64).unwrap_or(0),
            send,
        })
    }
    pub fn label(&self) -> String {
        self.title
            .clone()
            .unwrap_or_else(|| crate::tasks::first_line(&self.text))
    }
    pub fn unknown(&self) -> bool {
        self.send
            .as_ref()
            .is_some_and(|s| s.state == "delivery_unknown")
    }
}

/// Send-state marker shown on a draft row.
pub fn send_marker(s: Option<&SendInfo>) -> &'static str {
    match s.map(|s| s.state.as_str()) {
        Some("sending") => "sending",
        Some("delivered") => "✓ delivered",
        Some("delivery_unknown") => "? unknown",
        Some("failed") | Some("cancelled") => "✗ failed",
        _ => "",
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tab {
    Drafts,
    Notes,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Origin {
    None,
    Peek(String),
    Task,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub run: String,
    pub pane: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SendPhase {
    Checking,
    Ready,
    Sending,
    /// Accepted by the server; delivery is confirmed by the agent's next turn (events).
    Sent(String),
    /// `send_unsafe`: zero bytes written, the draft is kept.
    Refused {
        reason: String,
        pane: Option<String>,
    },
    /// Needs `R` reconcile before anything else.
    ReconcileFirst,
    /// The earlier attempt may have arrived; a retry needs an explicit, warned action.
    MayHaveArrived(String),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SendFlow {
    pub draft: String,
    pub targets: Vec<Target>,
    pub sel: usize,
    pub check: Option<Value>,
    pub check_err: Option<String>,
    pub include_notes: bool,
    pub phase: SendPhase,
    /// Generated when the flow opens; a warned retry gets a fresh key.
    pub idem: String,
    pub retry: bool,
}

impl SendFlow {
    pub fn target(&self) -> Option<&Target> {
        self.targets.get(self.sel)
    }
    /// The check says the guarded prompt-input path is available.
    pub fn safe(&self) -> bool {
        self.check
            .as_ref()
            .is_some_and(|c| st(c, "send_path") == "prompt_input")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Editing {
    /// `None`: a new draft.
    pub draft: Option<String>,
    pub rev: Option<u64>,
    pub ed: TextEditor,
    /// Attachments for a new draft (sent with `draft.create`).
    pub pending: Vec<Value>,
    /// `ctrl+f` path prompt.
    pub attach_prompt: Option<String>,
    pub confirm_discard: bool,
    pub saving: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Sub {
    None,
    Edit(Editing),
    ConfirmDelete(String),
    Send(SendFlow),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct NotesState {
    pub loaded: bool,
    pub text: String,
    pub rev: Option<u64>,
    pub editing: Option<TextEditor>,
    pub saving: bool,
    /// `expected_rev` conflict: ask before overwriting.
    pub conflict: bool,
    pub changed_elsewhere: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DraftsView {
    pub id: u64,
    pub machine: usize,
    /// (`workspace` | `task`, id).
    pub scope: (String, String),
    /// Workspace for notes (and send targets).
    pub workspace: Option<String>,
    pub label: String,
    pub origin: Origin,
    pub default_run: Option<String>,
    pub tab: Tab,
    pub items: Vec<DraftItem>,
    pub sel: usize,
    pub selected: BTreeSet<String>,
    pub sub: Sub,
    pub notes: NotesState,
    pub loading: bool,
    pub notice: Option<String>,
    pub error: Option<String>,
}

impl DraftsView {
    pub fn cur(&self) -> Option<&DraftItem> {
        self.items.get(self.sel)
    }
}

// ---- opening ---------------------------------------------------------------------------------------

fn ws_of_pane(app: &App, mi: usize, pane: &str) -> Option<String> {
    app.machines[mi]
        .model
        .panes
        .iter()
        .find(|p| p.id == pane)
        .map(|p| p.workspace.clone())
}

fn ws_name(app: &App, mi: usize, ws: &str) -> String {
    app.machines[mi]
        .model
        .workspaces
        .iter()
        .find(|w| w.id == ws)
        .map(|w| w.display_name().to_string())
        .unwrap_or_else(|| ws.to_string())
}

/// Open the drafts view for a workspace (`:drafts`, `:notes`, peek `d`).
pub fn open_workspace(
    app: &mut App,
    mi: usize,
    ws: &str,
    default_run: Option<String>,
    origin: Origin,
    tab: Tab,
) {
    let label = format!("drafts · {}", ws_name(app, mi, ws));
    open(
        app,
        mi,
        ("workspace".into(), ws.into()),
        Some(ws.into()),
        label,
        default_run,
        origin,
        tab,
    );
}

/// Open the drafts view for a tracked task (task details `D`).
pub fn open_task(app: &mut App, mi: usize, task: &str, title: &str) {
    let ws = app.machines[mi]
        .model
        .tasks
        .iter()
        .find(|t| t.id == task)
        .and_then(|t| t.workspace.clone())
        .or_else(|| app.focused_ws().map(|w| w.id));
    open(
        app,
        mi,
        ("task".into(), task.into()),
        ws,
        format!("drafts · task {}", truncate(title, 40)),
        None,
        Origin::Task,
        Tab::Drafts,
    );
}

#[allow(clippy::too_many_arguments)]
fn open(
    app: &mut App,
    mi: usize,
    scope: (String, String),
    workspace: Option<String>,
    label: String,
    default_run: Option<String>,
    origin: Origin,
    tab: Tab,
) {
    let id = app.next_ui_id();
    app.drafts = Some(DraftsView {
        id,
        machine: mi,
        scope,
        workspace,
        label,
        origin,
        default_run,
        tab,
        items: Vec::new(),
        sel: 0,
        selected: BTreeSet::new(),
        sub: Sub::None,
        notes: NotesState::default(),
        loading: true,
        notice: None,
        error: None,
    });
    app.mode = Mode::Popup(Popup::Drafts);
    refresh(app);
    if tab == Tab::Notes {
        load_notes(app);
    }
}

/// Palette actions owned by this module.
pub fn action(app: &mut App, action: &str) -> bool {
    match action {
        "drafts" | "notes" => {
            let Some(ws) = app.focused_ws() else {
                app.toast("no workspace");
                return true;
            };
            let mi = app.cur;
            let run = app.focused_pane().and_then(|p| {
                app.m()
                    .model
                    .runs
                    .iter()
                    .find(|r| r.pane == p)
                    .map(|r| r.id.clone())
            });
            let tab = if action == "notes" {
                Tab::Notes
            } else {
                Tab::Drafts
            };
            open_workspace(app, mi, &ws.id, run, Origin::None, tab);
            true
        }
        _ => false,
    }
}

/// Peek `d`: the drafts of the agent's workspace, sending to that agent by default.
pub fn open_from_peek(app: &mut App, pane: &str) {
    let mi = app.cur;
    let Some(ws) = ws_of_pane(app, mi, pane) else {
        app.toast("pane is gone");
        return;
    };
    let run = app.machines[mi]
        .model
        .runs
        .iter()
        .find(|r| r.pane == pane)
        .map(|r| r.id.clone());
    open_workspace(app, mi, &ws, run, Origin::Peek(pane.into()), Tab::Drafts);
}

/// Peek reply box `ctrl+d`: keep a half-written follow-up as a workspace draft.
pub fn save_reply_as_draft(app: &mut App, pane: &str, text: &str) {
    let mi = app.cur;
    if text.trim().is_empty() {
        app.toast("nothing to save");
        return;
    }
    let Some(ws) = ws_of_pane(app, mi, pane) else {
        app.toast("pane is gone");
        return;
    };
    let key = app.new_idempotency_key("draft");
    app.command_on(
        mi,
        "draft.create",
        json!({"scope": "workspace", "id": ws, "text": text, "idempotency_key": key}),
        Pending::Drafts(Reply::ReplySaved),
    );
}

fn close(app: &mut App) {
    let origin = app.drafts.take().map(|v| v.origin);
    app.mode = match origin {
        Some(Origin::Peek(p)) => Mode::Popup(Popup::Peek { pane: p }),
        Some(Origin::Task) if app.task_view.is_some() => Mode::Popup(Popup::Task),
        _ => Mode::Normal,
    };
}

pub fn refresh(app: &mut App) {
    let Some(v) = &mut app.drafts else {
        return;
    };
    v.loading = true;
    let (mi, id) = (v.machine, v.id);
    let params = json!({"scope": v.scope.0, "id": v.scope.1});
    app.command_on(
        mi,
        "draft.list",
        params,
        Pending::Drafts(Reply::List { view: id }),
    );
}

fn load_notes(app: &mut App) {
    let Some(v) = &mut app.drafts else {
        return;
    };
    let (mi, id) = (v.machine, v.id);
    let Some(ws) = v.workspace.clone() else {
        v.notes.error = Some("no workspace for notes".into());
        return;
    };
    app.command_on(
        mi,
        "notes.get",
        json!({"workspace": ws}),
        Pending::Drafts(Reply::Notes { view: id }),
    );
}

/// Runs this view can send to: the workspace's agents (all agents for a task scope), the
/// default (focused pane / peeked agent) first.
fn targets(app: &App, v: &DraftsView) -> Vec<Target> {
    let m = &app.machines[v.machine];
    let mut out: Vec<Target> = m
        .model
        .runs
        .iter()
        .filter(|r| r.ended_at_ms.is_none())
        .filter(|r| {
            v.scope.0 == "task"
                || m.model
                    .panes
                    .iter()
                    .any(|p| p.id == r.pane && Some(&p.workspace) == v.workspace.as_ref())
        })
        .map(|r| {
            let handle = m
                .model
                .panes
                .iter()
                .find(|p| p.id == r.pane)
                .map(|p| p.handle.clone())
                .unwrap_or_default();
            Target {
                run: r.id.clone(),
                pane: r.pane.clone(),
                label: format!(
                    "{} {} · pane {handle}",
                    crate::draw::harness_icon(&r.harness),
                    r.name.clone().unwrap_or_else(|| r.harness.clone())
                ),
            }
        })
        .collect();
    if let Some(d) = &v.default_run
        && let Some(i) = out.iter().position(|t| &t.run == d)
    {
        let t = out.remove(i);
        out.insert(0, t);
    }
    out
}

fn check(app: &mut App, v: &DraftsView, f: &SendFlow) {
    if let Some(t) = f.target() {
        app.command_on(
            v.machine,
            "draft.check",
            json!({"target_run": t.run, "draft": f.draft}),
            Pending::Drafts(Reply::Check {
                view: v.id,
                run: t.run.clone(),
            }),
        );
    }
}

/// Dispatch `draft.send` durably (pending-operations store first).
fn send(app: &mut App, v: &mut DraftsView, f: &mut SendFlow) {
    let Some(t) = f.target().cloned() else {
        return;
    };
    let mut params = json!({
        "draft": f.draft,
        "target_run": t.run,
        "idempotency_key": f.idem,
        "include_notes": f.include_notes,
    });
    if f.retry {
        params["retry_despite_unknown"] = json!(true);
    }
    f.phase = SendPhase::Sending;
    let (mi, id, draft) = (v.machine, v.id, f.draft.clone());
    if !app.mutate(
        mi,
        "draft.send",
        params,
        Pending::Drafts(Reply::Sent { view: id, draft }),
    ) {
        f.phase = SendPhase::Failed(
            "couldn't record the pending send locally (or the machine is offline); not sent".into(),
        );
    }
}

// ---- keys ------------------------------------------------------------------------------------------

pub fn key(app: &mut App, ev: KeyEvent) {
    let Some(mut v) = app.drafts.take() else {
        app.mode = Mode::Normal;
        return;
    };
    app.mode = Mode::Popup(Popup::Drafts);
    if ev.kind == KeyKind::Release {
        app.drafts = Some(v);
        return;
    }
    v.notice = None;
    match std::mem::replace(&mut v.sub, Sub::None) {
        Sub::Edit(e) => edit_key(app, &mut v, e, ev),
        Sub::ConfirmDelete(id) => match ev.key {
            Key::Char('y' | 'Y') => {
                let (mi, vid) = (v.machine, v.id);
                let key = app.new_idempotency_key("draft-del");
                app.command_on(
                    mi,
                    "draft.delete",
                    json!({"draft": id, "idempotency_key": key}),
                    Pending::Drafts(Reply::Deleted { view: vid }),
                );
            }
            _ => v.notice = Some("Not deleted".into()),
        },
        Sub::Send(f) => send_key(app, &mut v, f, ev),
        Sub::None => {
            if v.tab == Tab::Notes {
                notes_key(app, &mut v, ev);
            } else if list_key(app, &mut v, ev) {
                // closed
                return;
            }
        }
    }
    if matches!(app.mode, Mode::Popup(Popup::Drafts)) {
        app.drafts = Some(v);
    }
}

/// Returns true when the view closed.
fn list_key(app: &mut App, v: &mut DraftsView, ev: KeyEvent) -> bool {
    let n = v.items.len();
    match ev.key {
        Key::Named(NamedKey::Escape) | Key::Char('q') => {
            app.drafts = Some(v.clone());
            close(app);
            return true;
        }
        Key::Named(NamedKey::Tab) => {
            v.tab = Tab::Notes;
            if !v.notes.loaded {
                app.drafts = Some(v.clone());
                load_notes(app);
                if let Some(x) = app.drafts.take() {
                    *v = x;
                }
            }
        }
        Key::Char('j') | Key::Named(NamedKey::Down) => v.sel = (v.sel + 1).min(n.saturating_sub(1)),
        Key::Char('k') | Key::Named(NamedKey::Up) => v.sel = v.sel.saturating_sub(1),
        Key::Char('r') => {
            app.drafts = Some(v.clone());
            refresh(app);
            if let Some(x) = app.drafts.take() {
                *v = x;
            }
        }
        Key::Char('n') => {
            v.sub = Sub::Edit(Editing {
                draft: None,
                rev: None,
                ed: TextEditor::new("", true),
                pending: Vec::new(),
                attach_prompt: None,
                confirm_discard: false,
                saving: false,
                error: None,
            })
        }
        Key::Char('e') | Key::Named(NamedKey::Enter) => {
            if let Some(d) = v.cur() {
                v.sub = Sub::Edit(Editing {
                    draft: Some(d.id.clone()),
                    rev: Some(d.rev),
                    ed: TextEditor::new(&d.text, true),
                    pending: Vec::new(),
                    attach_prompt: None,
                    confirm_discard: false,
                    saving: false,
                    error: None,
                });
            }
        }
        Key::Char(' ') => {
            if let Some(d) = v.cur().map(|d| d.id.clone())
                && !v.selected.remove(&d)
            {
                v.selected.insert(d);
            }
        }
        Key::Char(c @ ('J' | 'K')) if n > 1 => {
            let j = if c == 'J' {
                (v.sel + 1).min(n - 1)
            } else {
                v.sel.saturating_sub(1)
            };
            if j != v.sel {
                v.items.swap(v.sel, j);
                v.sel = j;
                let order: Vec<&str> = v.items.iter().map(|d| d.id.as_str()).collect();
                let key = app.new_idempotency_key("draft-order");
                app.command_on(
                    v.machine,
                    "draft.reorder",
                    json!({"order": order, "idempotency_key": key}),
                    Pending::Drafts(Reply::Reordered { view: v.id }),
                );
            }
        }
        Key::Char('m') => {
            let ids: Vec<String> = v
                .items
                .iter()
                .filter(|d| v.selected.contains(&d.id))
                .map(|d| d.id.clone())
                .collect();
            if ids.len() < 2 {
                v.notice = Some("Select two or more drafts with space, then m".into());
            } else {
                let key = app.new_idempotency_key("draft-combine");
                app.command_on(
                    v.machine,
                    "draft.combine",
                    json!({"ids": ids, "idempotency_key": key}),
                    Pending::Drafts(Reply::Combined { view: v.id }),
                );
            }
        }
        Key::Char('x') => {
            if let Some(d) = v.cur() {
                v.sub = Sub::ConfirmDelete(d.id.clone());
            }
        }
        Key::Char('R') => match v.cur() {
            Some(d) if d.unknown() => {
                app.command_on(
                    v.machine,
                    "draft.reconcile",
                    json!({"draft": d.id}),
                    Pending::Drafts(Reply::Reconciled { view: v.id }),
                );
                v.notice = Some("Checking whether the earlier attempt arrived…".into());
            }
            Some(_) => v.notice = Some("Only a ? unknown send needs reconciling".into()),
            None => {}
        },
        Key::Char('s') => {
            let Some(d) = v.cur().cloned() else {
                return false;
            };
            let ts = targets(app, v);
            if ts.is_empty() {
                v.notice = Some("No running agent to send to in this workspace".into());
                return false;
            }
            let f = SendFlow {
                draft: d.id.clone(),
                targets: ts,
                sel: 0,
                check: None,
                check_err: None,
                include_notes: false,
                phase: if d.unknown() && !d.send.as_ref().is_some_and(|s| s.reconciled) {
                    SendPhase::ReconcileFirst
                } else {
                    SendPhase::Checking
                },
                idem: app.new_idempotency_key("draft-send"),
                retry: false,
            };
            check(app, v, &f);
            v.sub = Sub::Send(f);
        }
        _ => {}
    }
    false
}

fn edit_key(app: &mut App, v: &mut DraftsView, mut e: Editing, ev: KeyEvent) {
    if e.saving {
        v.sub = Sub::Edit(e);
        return;
    }
    if e.confirm_discard {
        match ev.key {
            Key::Char('y' | 'Y') => v.notice = Some("Changes discarded".into()),
            _ => {
                e.confirm_discard = false;
                v.sub = Sub::Edit(e);
            }
        }
        return;
    }
    if let Some(mut p) = e.attach_prompt.take() {
        match ev.key {
            Key::Named(NamedKey::Escape) => {}
            Key::Named(NamedKey::Enter) => {
                let path = p.trim().to_string();
                if !path.is_empty() {
                    attach(app, v, &mut e, json!({"kind": "file", "path": path}));
                }
            }
            Key::Named(NamedKey::Backspace) => {
                p.pop();
                e.attach_prompt = Some(p);
            }
            Key::Char(c) if !ev.mods.ctrl() && !ev.mods.alt() => {
                p.push(c);
                e.attach_prompt = Some(p);
            }
            _ => e.attach_prompt = Some(p),
        }
        v.sub = Sub::Edit(e);
        return;
    }
    if ev.key == Key::Named(NamedKey::Escape) {
        if e.ed.dirty {
            e.confirm_discard = true;
            v.sub = Sub::Edit(e);
        }
        return;
    }
    if ctrl(&ev, 'x') {
        save(app, v, &mut e);
        v.sub = Sub::Edit(e);
        return;
    }
    if ctrl(&ev, 'f') {
        e.attach_prompt = Some(String::new());
        v.sub = Sub::Edit(e);
        return;
    }
    if ctrl(&ev, 's') {
        // Attach the newest screenshot of this workspace (a blob already on that machine).
        let mut p = json!({"limit": 1});
        if v.scope.0 == "task" {
            p["task"] = json!(v.scope.1);
        }
        app.command_on(
            v.machine,
            "screenshot.list",
            p,
            Pending::Drafts(Reply::LatestShot { view: v.id }),
        );
        v.notice = Some("Attaching the latest screenshot…".into());
        v.sub = Sub::Edit(e);
        return;
    }
    e.ed.key(&ev);
    v.sub = Sub::Edit(e);
}

fn attach(app: &mut App, v: &mut DraftsView, e: &mut Editing, a: Value) {
    match &e.draft {
        None => {
            e.pending.push(a);
            v.notice = Some(format!(
                "{} attachment(s) will be added when you save",
                e.pending.len()
            ));
        }
        Some(d) => {
            let key = app.new_idempotency_key("draft-attach");
            app.command_on(
                v.machine,
                "draft.update",
                json!({"draft": d, "add_attachment": a, "idempotency_key": key}),
                Pending::Drafts(Reply::Attached { view: v.id }),
            );
        }
    }
}

fn save(app: &mut App, v: &mut DraftsView, e: &mut Editing) {
    let text = e.ed.text();
    let key = app.new_idempotency_key("draft-save");
    let (method, params) = match &e.draft {
        None => {
            let mut p =
                json!({"scope": v.scope.0, "id": v.scope.1, "text": text, "idempotency_key": key});
            if !e.pending.is_empty() {
                p["attachments"] = json!(e.pending);
            }
            ("draft.create", p)
        }
        Some(d) => {
            let mut p = json!({"draft": d, "text": text, "idempotency_key": key});
            if let Some(r) = e.rev {
                p["expected_rev"] = json!(r);
            }
            ("draft.update", p)
        }
    };
    e.saving = true;
    e.error = None;
    app.command_on(
        v.machine,
        method,
        params,
        Pending::Drafts(Reply::Saved { view: v.id }),
    );
}

fn send_key(app: &mut App, v: &mut DraftsView, mut f: SendFlow, ev: KeyEvent) {
    match (&f.phase, ev.key) {
        (_, Key::Named(NamedKey::Escape)) => return,
        (SendPhase::Sending, _) => {}
        (_, Key::Char('o')) => {
            // Open pane to send: the only focus change, on explicit request.
            let pane = match &f.phase {
                SendPhase::Refused { pane: Some(p), .. } => Some(p.clone()),
                _ => f.target().map(|t| t.pane.clone()),
            };
            if let Some(p) = pane {
                let mi = v.machine;
                app.drafts = None;
                app.focus_pane(mi, &p);
                app.mode = Mode::Normal;
                app.toast("Draft kept — paste or type it in the pane");
                return;
            }
        }
        (SendPhase::ReconcileFirst, Key::Char('R')) => {
            app.command_on(
                v.machine,
                "draft.reconcile",
                json!({"draft": f.draft}),
                Pending::Drafts(Reply::Reconciled { view: v.id }),
            );
            v.notice = Some("Checking whether the earlier attempt arrived…".into());
        }
        (SendPhase::MayHaveArrived(_), Key::Char('!')) => {
            f.retry = true;
            f.idem = app.new_idempotency_key("draft-send");
            send(app, v, &mut f);
        }
        (SendPhase::Sent(_) | SendPhase::Failed(_), Key::Named(NamedKey::Enter)) => return,
        (SendPhase::Ready | SendPhase::Checking, Key::Char('j') | Key::Named(NamedKey::Down))
            if f.sel + 1 < f.targets.len() =>
        {
            f.sel += 1;
            f.check = None;
            f.phase = SendPhase::Checking;
            check(app, v, &f);
        }
        (SendPhase::Ready | SendPhase::Checking, Key::Char('k') | Key::Named(NamedKey::Up))
            if f.sel > 0 =>
        {
            f.sel -= 1;
            f.check = None;
            f.phase = SendPhase::Checking;
            check(app, v, &f);
        }
        (SendPhase::Ready | SendPhase::Checking, Key::Char('i')) => {
            f.include_notes = !f.include_notes
        }
        (SendPhase::Ready, Key::Named(NamedKey::Enter) | Key::Char('y')) => {
            if f.safe() {
                send(app, v, &mut f);
            } else {
                v.notice = Some("Not safe to send from here — o opens the pane".into());
            }
        }
        _ => {}
    }
    v.sub = Sub::Send(f);
}

fn notes_key(app: &mut App, v: &mut DraftsView, ev: KeyEvent) {
    let n = &mut v.notes;
    if n.conflict {
        match ev.key {
            Key::Char('o') => {
                n.conflict = false;
                let text = n.editing.as_ref().map(TextEditor::text).unwrap_or_default();
                n.saving = true;
                let ws = v.workspace.clone();
                app.command_on(
                    v.machine,
                    "notes.set",
                    json!({"workspace": ws, "text": text}),
                    Pending::Drafts(Reply::NotesSaved { view: v.id }),
                );
            }
            Key::Char('r') => {
                n.conflict = false;
                n.editing = None;
                app.drafts = Some(v.clone());
                load_notes(app);
                if let Some(x) = app.drafts.take() {
                    *v = x;
                }
            }
            _ => {}
        }
        return;
    }
    if let Some(ed) = &mut n.editing {
        if n.saving {
            return;
        }
        if ev.key == Key::Named(NamedKey::Escape) {
            n.editing = None;
            v.notice = Some("Notes edits discarded".into());
            return;
        }
        if ctrl(&ev, 'x') {
            n.saving = true;
            let mut p = json!({"workspace": v.workspace, "text": ed.text()});
            if let Some(r) = n.rev {
                p["expected_rev"] = json!(r);
            }
            app.command_on(
                v.machine,
                "notes.set",
                p,
                Pending::Drafts(Reply::NotesSaved { view: v.id }),
            );
            return;
        }
        ed.key(&ev);
        return;
    }
    match ev.key {
        Key::Named(NamedKey::Escape) | Key::Char('q') => {
            app.drafts = Some(v.clone());
            close(app);
        }
        Key::Named(NamedKey::Tab) => v.tab = Tab::Drafts,
        Key::Char('e') | Key::Named(NamedKey::Enter) if n.loaded => {
            n.editing = Some(TextEditor::new(&n.text, true));
            n.changed_elsewhere = false;
        }
        Key::Char('r') => {
            app.drafts = Some(v.clone());
            load_notes(app);
            if let Some(x) = app.drafts.take() {
                *v = x;
            }
        }
        _ => {}
    }
}

/// Paste into whichever editor is active in a drafts/desk/assist view. Returns true if used.
pub fn on_paste(app: &mut App, text: &str) -> bool {
    if let Some(v) = &mut app.drafts
        && matches!(app.mode, Mode::Popup(Popup::Drafts))
    {
        match &mut v.sub {
            Sub::Edit(e) => {
                match &mut e.attach_prompt {
                    Some(p) => p.push_str(text.trim()),
                    None => e.ed.insert_str(text),
                }
                return true;
            }
            Sub::None if v.tab == Tab::Notes => {
                if let Some(ed) = &mut v.notes.editing {
                    ed.insert_str(text);
                    return true;
                }
            }
            _ => {}
        }
    }
    if crate::desk::on_paste(app, text) {
        return true;
    }
    crate::assist::on_paste(app, text)
}

// ---- replies & events ------------------------------------------------------------------------------

fn view_mut(app: &mut App, id: u64) -> Option<&mut DraftsView> {
    app.drafts.as_mut().filter(|v| v.id == id)
}

pub fn on_reply(app: &mut App, _mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    match r {
        Reply::ReplySaved => match res {
            Ok(_) => app.toast("Saved as draft — :drafts to edit or send (nothing was sent)"),
            Err(e) => app.toast(format!("✗ draft not saved: {}", e.message)),
        },
        Reply::List { view } => {
            let Some(v) = view_mut(app, view) else { return };
            v.loading = false;
            match res {
                Ok(x) => {
                    v.error = None;
                    let keep = v.cur().map(|d| d.id.clone());
                    v.items = arr(&x, "drafts")
                        .iter()
                        .filter_map(DraftItem::parse)
                        .collect();
                    v.selected.retain(|id| v.items.iter().any(|d| &d.id == id));
                    v.sel = keep
                        .and_then(|k| v.items.iter().position(|d| d.id == k))
                        .unwrap_or(v.sel)
                        .min(v.items.len().saturating_sub(1));
                }
                Err(e) if e.is_method_not_found() => {
                    v.error =
                        Some("This machine's server has no drafts (needs a newer server)".into())
                }
                Err(e) => v.error = Some(e.message),
            }
        }
        Reply::Saved { view } => {
            let Some(v) = view_mut(app, view) else { return };
            match res {
                Ok(_) => {
                    if matches!(v.sub, Sub::Edit(_)) {
                        v.sub = Sub::None;
                    }
                    v.notice = Some("Draft saved — nothing was sent".into());
                    refresh(app);
                }
                Err(e) => {
                    if let Sub::Edit(ed) = &mut v.sub {
                        ed.saving = false;
                        ed.error = Some(if e.reason() == Some("draft_changed") {
                            "This draft changed elsewhere — esc, then reopen it to merge".into()
                        } else {
                            e.message
                        });
                    }
                }
            }
        }
        Reply::Attached { view } => {
            let Some(v) = view_mut(app, view) else { return };
            match res {
                Ok(x) => {
                    v.notice = Some("Attachment added".into());
                    // The attachment bumped the revision; keep saving against the new one.
                    let rev = x
                        .get("draft")
                        .and_then(|d| d.get("rev"))
                        .and_then(Value::as_u64);
                    if let (Some(rev), Sub::Edit(e)) = (rev, &mut v.sub) {
                        e.rev = Some(rev);
                    }
                }
                Err(e) => v.notice = Some(format!("✗ not attached: {}", e.message)),
            }
            refresh(app);
        }
        Reply::LatestShot { view } => {
            let Some(v) = view_mut(app, view) else { return };
            let shot = res
                .as_ref()
                .ok()
                .and_then(|x| arr(x, "screenshots").first().cloned());
            match shot {
                Some(s) => {
                    let a = json!({"kind": "screenshot", "blob": st(&s, "blob"), "label": st(&s, "handle")});
                    let mut v2 = v.clone();
                    if let Sub::Edit(mut e) = std::mem::replace(&mut v2.sub, Sub::None) {
                        attach(app, &mut v2, &mut e, a);
                        v2.sub = Sub::Edit(e);
                    }
                    app.drafts = Some(v2);
                }
                None => {
                    v.notice = Some(match res {
                        Err(e) => format!("✗ {}", e.message),
                        Ok(_) => "No screenshot to attach".into(),
                    })
                }
            }
        }
        Reply::Reordered { view } | Reply::Deleted { view } | Reply::Combined { view } => {
            let Some(v) = view_mut(app, view) else { return };
            match res {
                Ok(_) => {
                    if matches!(r, Reply::Combined { .. }) {
                        v.selected.clear();
                        v.notice = Some("Combined into a new draft (sources kept)".into());
                    }
                    if matches!(r, Reply::Deleted { .. }) {
                        v.notice = Some("Draft deleted".into());
                    }
                }
                Err(e) => v.notice = Some(format!("✗ {}", e.message)),
            }
            refresh(app);
        }
        Reply::Check { view, run } => {
            let Some(v) = view_mut(app, view) else { return };
            if let Sub::Send(f) = &mut v.sub
                && f.target().is_some_and(|t| t.run == run)
            {
                match res {
                    Ok(x) => {
                        f.check = Some(x);
                        f.check_err = None;
                        if f.phase == SendPhase::Checking {
                            f.phase = SendPhase::Ready;
                        }
                    }
                    Err(e) => {
                        f.check = None;
                        f.check_err = Some(e.message);
                        if f.phase == SendPhase::Checking {
                            f.phase = SendPhase::Ready;
                        }
                    }
                }
            }
        }
        Reply::Sent { view, draft } => {
            let label = app
                .drafts
                .as_ref()
                .and_then(|v| v.items.iter().find(|d| d.id == draft))
                .map(DraftItem::label)
                .unwrap_or_else(|| "draft".into());
            let Some(v) = view_mut(app, view) else {
                match res {
                    Ok(_) => app.toast(format!("sending “{label}”…")),
                    Err(e) => app.toast(format!("✗ “{label}” not sent: {}", e.message)),
                }
                return;
            };
            if let Sub::Send(f) = &mut v.sub
                && f.draft == draft
            {
                f.phase = match res {
                    Ok(_) => SendPhase::Sent(
                        "Sending — delivered once the agent starts a turn with this text".into(),
                    ),
                    Err(e) => match e.reason() {
                        Some("send_unsafe") => SendPhase::Refused {
                            reason: e
                                .details
                                .get("detail")
                                .and_then(Value::as_str)
                                .unwrap_or(&e.message)
                                .to_string(),
                            pane: e
                                .details
                                .get("pane")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                        },
                        Some("reconcile_first") => SendPhase::ReconcileFirst,
                        Some("delivery_unknown") | Some("already_delivered") => {
                            SendPhase::MayHaveArrived(e.message)
                        }
                        _ if e.outcome_unknown() => SendPhase::MayHaveArrived(format!(
                            "{} — outcome unknown; checked with the server on reconnect",
                            e.message
                        )),
                        _ => SendPhase::Failed(e.message),
                    },
                };
            }
            refresh(app);
        }
        Reply::Reconciled { view } => {
            let Some(v) = view_mut(app, view) else { return };
            match res {
                Ok(x) => {
                    let note = st(&x, "note").to_string();
                    let may = x.get("may_retry").and_then(Value::as_bool) == Some(true);
                    let delivered = x
                        .get("send")
                        .map(|s| st(s, "state") == "delivered")
                        .unwrap_or(false);
                    let msg = if delivered {
                        "✓ The earlier attempt was delivered".to_string()
                    } else if may {
                        format!("Still uncertain ({note}) — a retry is allowed but warns")
                    } else {
                        note
                    };
                    if let Sub::Send(f) = &mut v.sub {
                        f.phase = if delivered {
                            SendPhase::Sent(msg.clone())
                        } else if may {
                            SendPhase::MayHaveArrived(msg.clone())
                        } else {
                            SendPhase::Failed(msg.clone())
                        };
                    }
                    v.notice = Some(msg);
                }
                Err(e) => v.notice = Some(format!("✗ {}", e.message)),
            }
            refresh(app);
        }
        Reply::Notes { view } => {
            let Some(v) = view_mut(app, view) else { return };
            match res {
                Ok(x) => {
                    let n = x.get("notes").cloned().unwrap_or(Value::Null);
                    v.notes.text = st(&n, "text").to_string();
                    v.notes.rev = n.get("rev").and_then(Value::as_u64);
                    v.notes.loaded = true;
                    v.notes.error = None;
                    v.notes.changed_elsewhere = false;
                }
                Err(e) => v.notes.error = Some(e.message),
            }
        }
        Reply::NotesSaved { view } => {
            let Some(v) = view_mut(app, view) else { return };
            v.notes.saving = false;
            match res {
                Ok(x) => {
                    let n = x.get("notes").cloned().unwrap_or(Value::Null);
                    v.notes.text = st(&n, "text").to_string();
                    v.notes.rev = n.get("rev").and_then(Value::as_u64);
                    v.notes.editing = None;
                    v.notes.conflict = false;
                    v.notice = Some("Notes saved — never sent unless you include them".into());
                }
                Err(e) if e.kind == "conflict" => v.notes.conflict = true,
                Err(e) => v.notes.error = Some(e.message),
            }
        }
    }
}

/// Pushed `draft.*` / `notes.updated` events.
pub fn on_event(app: &mut App, mi: usize, kind: &str, v: &Value) {
    let Some(view) = &mut app.drafts else {
        return;
    };
    if view.machine != mi {
        return;
    }
    let draft = v["subject"]["draft"].as_str().unwrap_or("").to_string();
    if kind == "notes.updated" {
        if view.notes.editing.is_some() || view.notes.saving {
            if !view.notes.saving {
                view.notes.changed_elsewhere = true;
            }
        } else if view.notes.loaded {
            load_notes(app);
        }
        return;
    }
    if let Sub::Send(f) = &mut view.sub
        && f.draft == draft
    {
        match kind {
            "draft.delivered" => f.phase = SendPhase::Sent("✓ delivered".into()),
            "draft.delivery_unknown" => {
                f.phase = SendPhase::MayHaveArrived(
                    "? delivery unknown — the message may have arrived; R reconciles".into(),
                )
            }
            "draft.send_failed" => f.phase = SendPhase::Failed("✗ send failed (draft kept)".into()),
            _ => {}
        }
    }
    if kind == "draft.delivered" {
        app.toast("✓ draft delivered");
    }
    refresh(app);
}

// ---- drawing ---------------------------------------------------------------------------------------

pub fn draw(app: &App, g: &mut Grid) {
    let Some(v) = &app.drafts else {
        return;
    };
    let t = app.theme;
    let tabs = match v.tab {
        Tab::Drafts => "[Drafts]  Notes",
        Tab::Notes => " Drafts  [Notes]",
    };
    let mut a = Area::open(app, g, &format!("{} · {tabs}", v.label));
    if let Some(n) = &v.notice {
        a.line(n, t.bold(t.yellow));
    }
    if v.tab == Tab::Notes {
        draw_notes(app, v, &mut a);
        return;
    }
    match &v.sub {
        Sub::Edit(e) => {
            a.line(
                if e.draft.is_some() {
                    "Edit draft — saving never sends"
                } else {
                    "New draft — saving never sends"
                },
                t.bold(t.fg),
            );
            if let Some(err) = &e.error {
                a.line(err, t.s(t.red));
            }
            if !e.pending.is_empty() {
                a.line(
                    &format!("{} attachment(s) pending", e.pending.len()),
                    t.dim(),
                );
            }
            if let Some(p) = &e.attach_prompt {
                a.line(
                    &format!(
                        "Attach file (absolute path on {}): {p}",
                        app.machines[v.machine].label
                    ),
                    t.s(t.accent),
                );
            }
            if e.confirm_discard {
                a.line(
                    "Discard your changes? [y] discard  [any key] keep editing",
                    t.bold(t.yellow),
                );
            }
            let r = a.rest();
            e.ed.draw(a.g, r, t.text(), t.rev());
            a.footer(
                if e.saving {
                    "saving…"
                } else {
                    "type to edit · ctrl+x save · ctrl+f attach file · ctrl+s attach latest screenshot · esc discard"
                },
                t.dim(),
            );
            return;
        }
        Sub::Send(f) => {
            draw_send(app, v, f, &mut a);
            return;
        }
        _ => {}
    }
    if let Some(e) = &v.error {
        a.line(e, t.s(t.red));
    }
    if v.items.is_empty() {
        a.line(
            if v.loading {
                "loading drafts…"
            } else {
                "No drafts yet — n writes one (it stays here until you send it)"
            },
            t.dim(),
        );
    }
    for (i, d) in v.items.iter().enumerate() {
        let mark = if v.selected.contains(&d.id) {
            "◉"
        } else {
            "○"
        };
        let att = if d.attachments > 0 {
            format!(" 📎{}", d.attachments)
        } else {
            String::new()
        };
        let state = send_marker(d.send.as_ref());
        let row = format!(
            "{mark} {:>2}. {}{att}  {state}",
            i + 1,
            truncate(&d.label(), 70)
        );
        let style = if i == v.sel {
            t.sel(t.fg)
        } else if state.starts_with('?') {
            t.s(t.yellow)
        } else {
            t.text()
        };
        a.line(&row, style);
    }
    if let Some(d) = v.cur() {
        a.line("", t.text());
        a.line("─── preview ───", t.dim());
        for l in d.text.lines().take(a.left().saturating_sub(2) as usize) {
            a.line(l, t.text());
        }
        if d.unknown() {
            a.line(
                "? The last send's delivery is unknown — R reconciles before any retry",
                t.s(t.yellow),
            );
        }
    }
    if let Sub::ConfirmDelete(_) = &v.sub {
        a.footer(
            "Delete this draft? [y] delete  [any key] keep",
            t.bold(t.yellow),
        );
    } else {
        a.footer(
            "j/k move · n new · e edit · J/K reorder · space select · m combine · x delete · s send · R reconcile · tab notes · esc close",
            t.dim(),
        );
    }
}

fn draw_send(app: &App, v: &DraftsView, f: &SendFlow, a: &mut Area) {
    let t = app.theme;
    let d = v.items.iter().find(|d| d.id == f.draft);
    a.line(
        &format!(
            "Send draft “{}”",
            d.map(DraftItem::label).unwrap_or_default()
        ),
        t.bold(t.fg),
    );
    a.line("To:", t.dim());
    for (i, tg) in f.targets.iter().enumerate() {
        let st_ = if i == f.sel {
            t.sel(t.accent)
        } else {
            t.text()
        };
        a.line(&format!("  {}", tg.label), st_);
    }
    match (&f.check, &f.check_err) {
        (Some(c), _) => {
            let path = st(c, "send_path");
            if path == "prompt_input" {
                a.line(
                    &format!(
                        "send path: prompt_input ({}) · steer: not supported",
                        st(c, "harness")
                    ),
                    t.s(t.green),
                );
            } else {
                a.line("Open pane to send", t.bold(t.yellow));
                let why = st(c, "unsafe");
                if !why.is_empty() {
                    a.line(&format!("  reason: {why}"), t.s(t.yellow));
                }
                a.line("  steer: not supported", t.dim());
            }
            let hidden = arr(c, "hidden_attachments");
            if !hidden.is_empty() {
                a.line(
                    &format!("  {} attachment(s) not visible to that pane", hidden.len()),
                    t.s(t.yellow),
                );
            }
        }
        (None, Some(e)) => a.line(&format!("check failed: {e}"), t.s(t.red)),
        (None, None) => a.line("checking the send path…", t.dim()),
    }
    a.line(
        &format!(
            "[{}] include notes (i)",
            if f.include_notes { "x" } else { " " }
        ),
        t.text(),
    );
    let (msg, style, keys): (String, _, &str) = match &f.phase {
        SendPhase::Checking => (
            "checking…".into(),
            t.dim(),
            "j/k target · i notes · esc back",
        ),
        SendPhase::Ready if f.safe() => (
            String::new(),
            t.dim(),
            "[enter] send · j/k target · i notes · esc back",
        ),
        SendPhase::Ready => (
            "Not safe to send from here.".into(),
            t.bold(t.yellow),
            "[o] Open pane to send · j/k target · esc back",
        ),
        SendPhase::Sending => ("sending…".into(), t.s(t.yellow), "esc back"),
        SendPhase::Sent(m) => (m.clone(), t.s(t.green), "esc back"),
        SendPhase::Refused { reason, .. } => (
            format!("Not sent (zero bytes written; draft kept): {reason}"),
            t.bold(t.yellow),
            "[o] Open pane to send · esc back",
        ),
        SendPhase::ReconcileFirst => (
            "The earlier attempt may have arrived.".into(),
            t.bold(t.yellow),
            "[R] reconcile first · esc back",
        ),
        SendPhase::MayHaveArrived(m) => (
            m.clone(),
            t.bold(t.yellow),
            "[!] send again anyway (the earlier message may have arrived) · esc back",
        ),
        SendPhase::Failed(m) => (format!("✗ {m}"), t.s(t.red), "esc back"),
    };
    if !msg.is_empty() {
        a.line(&msg, style);
    }
    a.line("", t.text());
    if let Some(d) = d {
        a.line("─── exact text ───", t.dim());
        let max = a.left().saturating_sub(1) as usize;
        for l in d.text.lines().take(max) {
            a.line(l, t.text());
        }
    }
    a.footer(keys, t.dim());
}

fn draw_notes(app: &App, v: &DraftsView, a: &mut Area) {
    let t = app.theme;
    let n = &v.notes;
    a.line("Never sent unless you include it", t.bold(t.accent));
    if let Some(e) = &n.error {
        a.line(e, t.s(t.red));
    }
    if n.changed_elsewhere {
        a.line(
            "These notes changed elsewhere since you started editing",
            t.s(t.yellow),
        );
    }
    if n.conflict {
        a.line(
            "Notes changed elsewhere — [r] reload theirs (discard yours) · [o] overwrite with yours",
            t.bold(t.yellow),
        );
    }
    match &n.editing {
        Some(ed) => {
            let r = a.rest();
            ed.draw(a.g, r, t.text(), t.rev());
            a.footer(
                if n.saving {
                    "saving…"
                } else {
                    "type to edit · ctrl+x save · esc discard edits"
                },
                t.dim(),
            );
        }
        None => {
            if !n.loaded {
                a.line("loading…", t.dim());
            } else if n.text.is_empty() {
                a.line("(empty) — e to write notes for this workspace", t.dim());
            }
            for l in n.text.lines() {
                a.line(l, t.text());
            }
            a.footer("e edit · r reload · tab drafts · esc close", t.dim());
        }
    }
}

#[cfg(test)]
#[path = "drafts_tests.rs"]
pub(crate) mod tests;
