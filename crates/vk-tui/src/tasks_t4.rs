//! Task details, T4 surfaces (15 §5, §6.1, §8.1, §8.2; 07 `task.review.*`, `task.dependency.*`):
//!
//! - **Snapshot** (`s`): `task.review.snapshot` captures the checkout's uncommitted work as an
//!   immutable, accept-capable candidate. The package shows the snapshot candidate's label; a
//!   capture the server can't make consistent shows **Workspace changing — verification subject
//!   unavailable** (nothing recorded) with `s` to try again.
//! - **Request reviewer** (`R`): `task.review.request_reviewer` returns the exact prompt; it is
//!   shown editable, and `ctrl+s` starts the reviewer with `task.review.start_reviewer
//!   {request, prompt_digest}`. An edited prompt is recorded first (a new request carrying the
//!   user's text) and started only when the server's recorded text is exactly what was shown.
//! - **Review notes** (`N`): reviewer findings (agent opinion, never evidence) classified
//!   blocking / not blocking / dismissed with `task.review.note.classify`; a dismissal needs a
//!   reason.
//! - **Dependencies** (`d`): confirmed links listed with `task.dependency.list`; `a`/`A` add one
//!   through a task picker (`task.dependency.add`, a refused cycle shows its path), `x` removes
//!   the selected link after `y`.
//! - **Effort** (`f`): the user's value, the deterministic heuristic and a model estimate, each
//!   labelled with its source; applying any of them is an explicit `task.set {effort,
//!   effort_source}`. `E` runs **Estimate effort** through the assist flow (preview → confirm →
//!   result, `enter` applies).
//!
//! Every mutation goes through [`App::mutate`] (idempotency key persisted first, 15 §10.3).

use crate::app::{App, Pending, RpcErr};
use crate::drafts::TextEditor;
use crate::screen::{Grid, Rect as SRect};
use crate::tasks::{
    Api, Lines, Reply, TaskSub, TaskView, arr, edit, fetch, package, st, subject_label, view_of,
    wrap_push,
};
use serde_json::{Value, json};
use vk_proto::input::{Key, KeyEvent, NamedKey};

/// The server's label for a capture that could not be made consistent (15 §5).
pub const WORKSPACE_CHANGING: &str = "Workspace changing — verification subject unavailable";
/// Shown for the current dirty-snapshot candidate.
pub const SNAPSHOT_CANDIDATE: &str =
    "Snapshot of uncommitted work · accept-capable while the checkout still matches it";
pub const SNAPSHOT_EARLIER: &str = "Earlier snapshot · the checkout changed since — inspect only; snapshot again to review the current work";
pub const NOTE_LABEL: &str = "Reviewer finding · agent opinion, not evidence";
pub const DISMISS_NEEDS_REASON: &str = "A dismissal needs a reason";
pub const NEWER_SERVER: &str = "This needs a newer server on this machine (15 T4)";

/// `task.set` effort values with labels (same as the inbox).
pub const EFFORTS: [(&str, &str); 4] = crate::inbox::EFFORTS;

// ---- state ----------------------------------------------------------------------------------------

/// Per task view: fresher copies of what the package also carries, plus a model estimate the
/// assist flow produced (shown with its source, never applied by itself).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct T4State {
    /// Last `task.review.notes` reply.
    pub notes: Option<Value>,
    /// Last `task.dependency.list` → `dependencies`.
    pub deps: Option<Value>,
    /// `{effort, rationale, request}` from **Estimate effort** (14 `effort_estimate`).
    pub model_estimate: Option<Value>,
    /// A snapshot request is in flight.
    pub snapshotting: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum T4Sub {
    /// The capture couldn't be made consistent: nothing was recorded.
    WorkspaceChanging {
        message: String,
    },
    Reviewer(ReviewerFlow),
    Notes(NotesView),
    Deps(DepsView),
    Effort {
        sel: usize,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum RvPhase {
    /// `request_reviewer` in flight; with `auto_start` the user already confirmed this exact
    /// (edited) text, so a reply carrying exactly it starts the reviewer.
    Requesting {
        auto_start: Option<String>,
    },
    Prompt {
        request: String,
        /// The prompt the server recorded (what `digest` names).
        prompt: String,
        digest: String,
        harness: String,
        subject: String,
        label: String,
        uses_provider: String,
        ed: TextEditor,
    },
    Starting {
        request: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReviewerFlow {
    pub phase: RvPhase,
    pub error: Option<String>,
    pub notice: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NotesView {
    pub sel: usize,
    /// Reason entry for a classification: (classification, reason).
    pub reason: Option<(String, String)>,
    pub error: Option<String>,
    pub busy: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Picker {
    pub filter: String,
    pub sel: usize,
    /// `blocks` (ranked) or `related`.
    pub blocks: bool,
    /// true: this task waits for the chosen one; false: the chosen one waits for this task.
    pub waits_for: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DepsView {
    pub sel: usize,
    pub picker: Option<Picker>,
    /// Edge id awaiting `y`.
    pub confirm_remove: Option<String>,
    pub error: Option<String>,
    pub busy: bool,
}

#[derive(Debug, Clone)]
pub enum T4Reply {
    Snapshot { view: u64 },
    ReviewerPrepared { view: u64 },
    ReviewerStarted { view: u64 },
    Notes { view: u64 },
    Classified { view: u64 },
    Deps { view: u64 },
    DepChanged { view: u64, added: bool },
    EffortSet { view: u64, label: String },
}

// ---- helpers --------------------------------------------------------------------------------------

/// The package carries the T4 additions (`snapshot`, `review_notes`, `dependencies`, `effort`).
pub fn t4_supported(p: &Value) -> bool {
    ["snapshot", "review_notes", "dependencies", "effort"]
        .iter()
        .any(|k| p.get(*k).is_some_and(|x| !x.is_null()))
}

/// Map an estimate's effort to `task.set`'s values (`few_minutes` → `minutes`, …).
pub fn effort_param(e: &str) -> &str {
    match e {
        "few_minutes" | "a_few_minutes" | "minutes" => "minutes",
        "deep_review" | "deep" => "deep",
        "quick" => "quick",
        _ => "unknown",
    }
}

pub fn effort_label(e: &str) -> &'static str {
    let p = effort_param(e);
    EFFORTS
        .iter()
        .find(|(k, _)| *k == p)
        .map(|(_, l)| *l)
        .unwrap_or("unknown")
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// "blocks N linked task(s)" (15 §8.1).
pub fn blocks_text(n: u64) -> String {
    format!("blocks {}", plural(n, "linked task", "linked tasks"))
}

fn notes_of(v: &TaskView) -> Vec<Value> {
    if let Some(n) = &v.t4.notes {
        return arr(n, "notes").to_vec();
    }
    package(v)
        .map(|p| arr(p, "review_notes").to_vec())
        .unwrap_or_default()
}

fn reviewer_runs_of(v: &TaskView) -> Vec<Value> {
    if let Some(n) = &v.t4.notes {
        return arr(n, "reviewer_runs").to_vec();
    }
    package(v)
        .map(|p| arr(p, "reviewer_runs").to_vec())
        .unwrap_or_default()
}

fn deps_of(v: &TaskView) -> Value {
    v.t4.deps
        .clone()
        .or_else(|| package(v).and_then(|p| p.get("dependencies").cloned()))
        .unwrap_or(Value::Null)
}

/// Dependency rows: (edge, label, waits_for) — the task's own links first, then dependents.
fn dep_rows(app: &App, v: &TaskView) -> Vec<(Value, String)> {
    let d = deps_of(v);
    let mut out = Vec::new();
    for e in arr(&d, "depends_on") {
        let edge = e.get("edge").cloned().unwrap_or(Value::Null);
        let other = task_name(app, v.machine, st(&edge, "depends_on"), e.get("title"));
        out.push((
            edge.clone(),
            format!("waits for {other}  ({})", st(&edge, "kind")),
        ));
    }
    for e in arr(&d, "dependents") {
        let edge = e.get("edge").cloned().unwrap_or(Value::Null);
        let other = task_name(app, v.machine, st(&edge, "task"), e.get("title"));
        out.push((
            edge.clone(),
            format!("{other} waits for this  ({})", st(&edge, "kind")),
        ));
    }
    out
}

/// `#handle title` for a task id (the model, else the server-supplied title, else the id).
fn task_name(app: &App, mi: usize, id: &str, title: Option<&Value>) -> String {
    if let Some(t) = app.machines[mi].model.tasks.iter().find(|t| t.id == id) {
        return format!("#{} {}", t.handle, t.title);
    }
    match title.and_then(Value::as_str) {
        Some(t) => format!("“{t}”"),
        None => format!("task {}", crate::draw::truncate(id, 10)),
    }
}

/// Picker candidates: open tasks on the view's machine other than this one, filtered.
fn candidates(app: &App, v: &TaskView, filter: &str) -> Vec<(String, String)> {
    let f = filter.to_lowercase();
    app.machines[v.machine]
        .model
        .tasks
        .iter()
        .filter(|t| t.id != v.task && t.status != "archived")
        .map(|t| (t.id.clone(), format!("#{} {}", t.handle, t.title)))
        .filter(|(_, l)| f.is_empty() || l.to_lowercase().contains(&f))
        .collect()
}

fn set_sub(app: &mut App, s: T4Sub) {
    if let Some(v) = &mut app.task_view {
        v.sub = TaskSub::T4(s);
    }
}

fn notice(app: &mut App, n: impl Into<String>) {
    if let Some(v) = &mut app.task_view {
        v.notice = Some(n.into());
    }
}

// ---- keys -----------------------------------------------------------------------------------------

/// Task-details keys owned here (when no sub-screen is open).
pub fn is_main_key(ev: &KeyEvent) -> bool {
    !ev.mods.ctrl()
        && !ev.mods.alt()
        && matches!(ev.key, Key::Char('s' | 'R' | 'N' | 'd' | 'f' | 'E'))
}

pub fn keys_hint(v: &TaskView) -> Vec<&'static str> {
    match package(v) {
        Some(p) if t4_supported(p) => vec![
            "s snapshot",
            "R request reviewer",
            "N review notes",
            "d dependencies",
            "f effort",
            "E estimate effort",
        ],
        _ => vec![],
    }
}

pub fn main_key(app: &mut App, ev: KeyEvent) {
    let Some(v) = &app.task_view else {
        return;
    };
    let (mi, task, id) = (v.machine, v.task.clone(), v.id);
    let supported = package(v).is_some_and(t4_supported);
    if !supported && ev.key != Key::Char('E') {
        let msg = match &v.review {
            Api::Loading => "Review package still loading".to_string(),
            Api::Ok(_) | Api::Unsupported => NEWER_SERVER.to_string(),
            Api::Err(e) => format!("Review unavailable: {e}"),
        };
        notice(app, msg);
        return;
    }
    match ev.key {
        Key::Char('s') => snapshot(app),
        Key::Char('R') => request_reviewer(app, None),
        Key::Char('N') => {
            set_sub(
                app,
                T4Sub::Notes(NotesView {
                    sel: 0,
                    reason: None,
                    error: None,
                    busy: false,
                }),
            );
            app.command_on(
                mi,
                "task.review.notes",
                json!({"task": task}),
                Pending::Task(Reply::T4(T4Reply::Notes { view: id })),
            );
        }
        Key::Char('d') => {
            set_sub(
                app,
                T4Sub::Deps(DepsView {
                    sel: 0,
                    picker: None,
                    confirm_remove: None,
                    error: None,
                    busy: false,
                }),
            );
            refresh_deps(app);
        }
        Key::Char('f') => {
            let cur = package(app.task_view.as_ref().unwrap())
                .and_then(|p| p.pointer("/effort/set"))
                .and_then(Value::as_str)
                .map(effort_param)
                .and_then(|e| EFFORTS.iter().position(|(k, _)| *k == e))
                .unwrap_or(0);
            set_sub(app, T4Sub::Effort { sel: cur });
        }
        Key::Char('E') => crate::assist::estimate_effort(app, mi, &task),
        _ => {}
    }
}

fn refresh_deps(app: &mut App) {
    let Some(v) = &app.task_view else {
        return;
    };
    let (mi, task, id) = (v.machine, v.task.clone(), v.id);
    app.command_on(
        mi,
        "task.dependency.list",
        json!({"task": task}),
        Pending::Task(Reply::T4(T4Reply::Deps { view: id })),
    );
}

/// `task.review.snapshot`: capture the uncommitted work as an immutable candidate.
fn snapshot(app: &mut App) {
    let Some(v) = &app.task_view else {
        return;
    };
    if v.t4.snapshotting {
        notice(app, "A snapshot is already being captured…");
        return;
    }
    if package(v).and_then(|p| p.pointer("/snapshot/available")) == Some(&json!(false)) {
        notice(
            app,
            "Nothing to snapshot: no uncommitted changes in a live checkout",
        );
        return;
    }
    let (mi, task, id) = (v.machine, v.task.clone(), v.id);
    let key = app.new_idempotency_key("snapshot");
    if app.mutate(
        mi,
        "task.review.snapshot",
        json!({"task": task, "idempotency_key": key}),
        Pending::Task(Reply::T4(T4Reply::Snapshot { view: id })),
    ) && let Some(v) = &mut app.task_view
    {
        v.t4.snapshotting = true;
        v.sub = TaskSub::None;
        v.notice = Some(
            "Capturing a snapshot of uncommitted work… (your files, index and branches stay as they are)"
                .into(),
        );
    }
}

/// `task.review.request_reviewer`, with the user's edited text when `prompt` is given (that
/// text is then started as soon as the server records exactly it).
fn request_reviewer(app: &mut App, prompt: Option<String>) {
    let Some(v) = &app.task_view else {
        return;
    };
    let (mi, task, id) = (v.machine, v.task.clone(), v.id);
    let mut params = json!({"task": task, "idempotency_key": app.new_idempotency_key("reviewer")});
    if let Some(p) = &prompt {
        params["prompt"] = json!(p);
    }
    let flow = ReviewerFlow {
        phase: RvPhase::Requesting {
            auto_start: prompt.clone(),
        },
        error: None,
        notice: None,
    };
    if app.mutate(
        mi,
        "task.review.request_reviewer",
        params,
        Pending::Task(Reply::T4(T4Reply::ReviewerPrepared { view: id })),
    ) {
        set_sub(app, T4Sub::Reviewer(flow));
    }
}

fn start_reviewer(app: &mut App, request: &str, digest: &str) {
    let Some(v) = &app.task_view else {
        return;
    };
    let (mi, id) = (v.machine, v.id);
    let key = app.new_idempotency_key("reviewer-start");
    if app.mutate(
        mi,
        "task.review.start_reviewer",
        json!({"request": request, "prompt_digest": digest, "idempotency_key": key}),
        Pending::Task(Reply::T4(T4Reply::ReviewerStarted { view: id })),
    ) {
        set_sub(
            app,
            T4Sub::Reviewer(ReviewerFlow {
                phase: RvPhase::Starting {
                    request: request.to_string(),
                },
                error: None,
                notice: None,
            }),
        );
    }
}

fn set_effort(app: &mut App, effort: &str, source: &str) {
    let Some(v) = &app.task_view else {
        return;
    };
    let (mi, task, id) = (v.machine, v.task.clone(), v.id);
    let effort = effort_param(effort).to_string();
    let key = app.new_idempotency_key("effort");
    let label = format!(
        "Effort set to {} (from {})",
        effort_label(&effort),
        source_label(source)
    );
    app.mutate(
        mi,
        "task.set",
        json!({"task": task, "effort": effort, "effort_source": source, "idempotency_key": key}),
        Pending::Task(Reply::T4(T4Reply::EffortSet { view: id, label })),
    );
}

fn source_label(s: &str) -> &str {
    if s.starts_with("assistant") {
        "the assistant's estimate"
    } else if s == "heuristic" {
        "the heuristic estimate"
    } else {
        "your choice"
    }
}

pub fn sub_key(app: &mut App, s: T4Sub, ev: KeyEvent) {
    let esc = ev.key == Key::Named(NamedKey::Escape);
    match s {
        T4Sub::WorkspaceChanging { message } => match ev.key {
            Key::Char('s') => snapshot(app),
            _ if esc || ev.key == Key::Char('q') => notice(app, "Nothing was recorded"),
            _ => set_sub(app, T4Sub::WorkspaceChanging { message }),
        },
        T4Sub::Reviewer(f) => reviewer_key(app, f, ev),
        T4Sub::Notes(n) => notes_key(app, n, ev),
        T4Sub::Deps(d) => deps_key(app, d, ev),
        T4Sub::Effort { sel } => effort_key(app, sel, ev),
    }
}

fn reviewer_key(app: &mut App, mut f: ReviewerFlow, ev: KeyEvent) {
    let esc = ev.key == Key::Named(NamedKey::Escape);
    f.notice = None;
    match f.phase {
        RvPhase::Prompt {
            request,
            prompt,
            digest,
            harness,
            subject,
            label,
            uses_provider,
            mut ed,
        } => {
            if esc {
                notice(app, "Reviewer not started — nothing was launched or sent");
                return;
            }
            if ev.mods.ctrl() && ev.key == Key::Char('s') {
                let text = ed.text();
                if text.trim().is_empty() {
                    f.error = Some("The prompt is empty".into());
                } else if text == prompt {
                    start_reviewer(app, &request, &digest);
                    return;
                } else {
                    // Record the edited text first; it starts once the server holds exactly it.
                    request_reviewer(app, Some(text));
                    return;
                }
            } else if ev.mods.ctrl() && ev.key == Key::Char('r') {
                ed = TextEditor::new(&prompt, true);
            } else {
                ed.key(&ev);
            }
            f.phase = RvPhase::Prompt {
                request,
                prompt,
                digest,
                harness,
                subject,
                label,
                uses_provider,
                ed,
            };
            set_sub(app, T4Sub::Reviewer(f));
        }
        phase => {
            if esc {
                // The request is recorded server-side but nothing launches without a start.
                notice(app, "Reviewer not started — nothing was launched or sent");
                return;
            }
            f.phase = phase;
            set_sub(app, T4Sub::Reviewer(f));
        }
    }
}

fn notes_key(app: &mut App, mut n: NotesView, ev: KeyEvent) {
    let Some(v) = &app.task_view else {
        return;
    };
    let notes = notes_of(v);
    let (mi, id) = (v.machine, v.id);
    if let Some((class, mut reason)) = n.reason.take() {
        match ev.key {
            Key::Named(NamedKey::Escape) => n.error = None,
            Key::Named(NamedKey::Enter) => {
                let Some(note) = notes.get(n.sel) else {
                    set_sub(app, T4Sub::Notes(n));
                    return;
                };
                if class == "dismissed" && reason.trim().is_empty() {
                    n.error = Some(DISMISS_NEEDS_REASON.into());
                    n.reason = Some((class, reason));
                } else {
                    let mut p = json!({
                        "note": st(note, "id"),
                        "classification": class,
                        "idempotency_key": app.new_idempotency_key("note"),
                    });
                    if !reason.trim().is_empty() {
                        p["reason"] = json!(reason.trim());
                    }
                    n.error = None;
                    if app.mutate(
                        mi,
                        "task.review.note.classify",
                        p,
                        Pending::Task(Reply::T4(T4Reply::Classified { view: id })),
                    ) {
                        n.busy = true;
                    }
                }
            }
            _ => {
                edit(&mut reason, &ev);
                n.reason = Some((class, reason));
            }
        }
        set_sub(app, T4Sub::Notes(n));
        return;
    }
    match ev.key {
        Key::Named(NamedKey::Escape) | Key::Char('q') => return,
        Key::Char('j') | Key::Named(NamedKey::Down) => {
            n.sel = (n.sel + 1).min(notes.len().saturating_sub(1))
        }
        Key::Char('k') | Key::Named(NamedKey::Up) => n.sel = n.sel.saturating_sub(1),
        Key::Char(c @ ('b' | 'n' | 'x')) => {
            if notes.get(n.sel).is_none() {
                n.error = Some("No note selected".into());
            } else {
                let class = match c {
                    'b' => "blocking",
                    'n' => "not_blocking",
                    _ => "dismissed",
                };
                n.error = None;
                n.reason = Some((class.into(), String::new()));
            }
        }
        _ => {}
    }
    set_sub(app, T4Sub::Notes(n));
}

fn deps_key(app: &mut App, mut d: DepsView, ev: KeyEvent) {
    let Some(v) = app.task_view.clone() else {
        return;
    };
    let (mi, id) = (v.machine, v.id);
    let esc = ev.key == Key::Named(NamedKey::Escape);
    if let Some(edge) = d.confirm_remove.take() {
        if matches!(ev.key, Key::Char('y' | 'Y')) {
            let key = app.new_idempotency_key("dep");
            if app.mutate(
                mi,
                "task.dependency.remove",
                json!({"edge": edge, "idempotency_key": key}),
                Pending::Task(Reply::T4(T4Reply::DepChanged {
                    view: id,
                    added: false,
                })),
            ) {
                d.busy = true;
            }
        }
        set_sub(app, T4Sub::Deps(d));
        return;
    }
    if let Some(mut p) = d.picker.take() {
        let cands = candidates(app, &v, &p.filter);
        match ev.key {
            Key::Named(NamedKey::Escape) => {}
            Key::Named(NamedKey::Tab) => {
                p.blocks = !p.blocks;
                d.picker = Some(p);
            }
            Key::Named(NamedKey::Down) => {
                p.sel = (p.sel + 1).min(cands.len().saturating_sub(1));
                d.picker = Some(p);
            }
            Key::Named(NamedKey::Up) => {
                p.sel = p.sel.saturating_sub(1);
                d.picker = Some(p);
            }
            Key::Named(NamedKey::Enter) => match cands.get(p.sel) {
                Some((other, _)) => {
                    let (task, on) = if p.waits_for {
                        (v.task.clone(), other.clone())
                    } else {
                        (other.clone(), v.task.clone())
                    };
                    let key = app.new_idempotency_key("dep");
                    d.error = None;
                    if app.mutate(
                        mi,
                        "task.dependency.add",
                        json!({"task": task, "depends_on": on,
                               "kind": if p.blocks { "blocks" } else { "related" },
                               "idempotency_key": key}),
                        Pending::Task(Reply::T4(T4Reply::DepChanged {
                            view: id,
                            added: true,
                        })),
                    ) {
                        d.busy = true;
                    }
                }
                None => d.picker = Some(p),
            },
            _ => {
                if edit(&mut p.filter, &ev) {
                    p.sel = 0;
                }
                d.picker = Some(p);
            }
        }
        set_sub(app, T4Sub::Deps(d));
        return;
    }
    let rows = dep_rows(app, &v);
    match ev.key {
        _ if esc || ev.key == Key::Char('q') => return,
        Key::Char('j') | Key::Named(NamedKey::Down) => {
            d.sel = (d.sel + 1).min(rows.len().saturating_sub(1))
        }
        Key::Char('k') | Key::Named(NamedKey::Up) => d.sel = d.sel.saturating_sub(1),
        Key::Char(c @ ('a' | 'A')) => {
            d.error = None;
            d.picker = Some(Picker {
                filter: String::new(),
                sel: 0,
                blocks: true,
                waits_for: c == 'a',
            });
        }
        Key::Char('x') => match rows.get(d.sel) {
            Some((edge, _)) => d.confirm_remove = Some(st(edge, "id").to_string()),
            None => d.error = Some("No link selected".into()),
        },
        _ => {}
    }
    set_sub(app, T4Sub::Deps(d));
}

fn effort_key(app: &mut App, sel: usize, ev: KeyEvent) {
    let Some(v) = &app.task_view else {
        return;
    };
    let heuristic = package(v)
        .and_then(|p| p.pointer("/effort/heuristic/effort"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let model = v.t4.model_estimate.clone();
    let (mi, task) = (v.machine, v.task.clone());
    match ev.key {
        Key::Named(NamedKey::Escape) | Key::Char('q') => {}
        Key::Char('j') | Key::Named(NamedKey::Down) => set_sub(
            app,
            T4Sub::Effort {
                sel: (sel + 1).min(EFFORTS.len() - 1),
            },
        ),
        Key::Char('k') | Key::Named(NamedKey::Up) => set_sub(
            app,
            T4Sub::Effort {
                sel: sel.saturating_sub(1),
            },
        ),
        Key::Named(NamedKey::Enter) => set_effort(app, EFFORTS[sel].0, "user"),
        Key::Char('h') => match heuristic {
            Some(e) => set_effort(app, &e, "heuristic"),
            None => {
                notice(app, "No heuristic estimate for this task yet");
                set_sub(app, T4Sub::Effort { sel });
            }
        },
        Key::Char('m') => match model {
            Some(m) => {
                let src = format!("assistant:{}", st(&m, "request"));
                set_effort(app, st(&m, "effort"), &src);
            }
            None => {
                notice(app, "No model estimate yet — E runs Estimate effort");
                set_sub(app, T4Sub::Effort { sel });
            }
        },
        Key::Char('E') => crate::assist::estimate_effort(app, mi, &task),
        _ => set_sub(app, T4Sub::Effort { sel }),
    }
}

// ---- replies --------------------------------------------------------------------------------------

/// A dependency refusal in words; a cycle names its path (15 §8.1).
pub fn dep_error_text(app: &App, mi: usize, e: &RpcErr) -> String {
    match e.reason() {
        Some("dependency_cycle") => {
            let path: Vec<String> = e
                .details
                .get("path")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(|id| task_name(app, mi, id, None))
                        .collect()
                })
                .unwrap_or_default();
            if path.is_empty() {
                "Refused: this link would create a dependency cycle".into()
            } else {
                format!(
                    "Refused: this link would create a dependency cycle: {}",
                    path.join(" → ")
                )
            }
        }
        Some("self_dependency") => "Refused: a task can't depend on itself".into(),
        Some("duplicate") => "That link already exists".into(),
        _ => e.message.clone(),
    }
}

pub fn on_reply(app: &mut App, mi: usize, r: T4Reply, res: Result<Value, RpcErr>) {
    app.dirty = true;
    match r {
        T4Reply::Snapshot { view } => {
            let Some(v) = view_of(app, view) else {
                return;
            };
            v.t4.snapshotting = false;
            match res {
                Ok(x) => {
                    let label = match st(&x, "label") {
                        "" => SNAPSHOT_CANDIDATE,
                        l => l,
                    };
                    v.notice = Some(format!("✓ {label}"));
                    v.sub = TaskSub::None;
                    fetch(app, false);
                }
                Err(e) if e.reason() == Some("workspace_changing") => {
                    v.notice = None;
                    v.sub = TaskSub::T4(T4Sub::WorkspaceChanging { message: e.message });
                }
                Err(e) if e.reason() == Some("nothing_to_snapshot") => {
                    v.notice =
                        Some("Nothing to snapshot: the checkout has no uncommitted changes".into())
                }
                Err(e) if e.is_method_not_found() => v.notice = Some(NEWER_SERVER.into()),
                Err(e) => v.notice = Some(format!("Snapshot not taken: {}", e.message)),
            }
        }
        T4Reply::ReviewerPrepared { view } => {
            let Some(v) = view_of(app, view) else {
                return;
            };
            let TaskSub::T4(T4Sub::Reviewer(f)) = &mut v.sub else {
                return;
            };
            let auto = match &f.phase {
                RvPhase::Requesting { auto_start } => auto_start.clone(),
                _ => return,
            };
            match res {
                Ok(x) => {
                    let request = x
                        .pointer("/request/id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let prompt = st(&x, "prompt").to_string();
                    let digest = st(&x, "prompt_digest").to_string();
                    if auto.as_deref() == Some(prompt.as_str()) && !digest.is_empty() {
                        start_reviewer(app, &request, &digest);
                        return;
                    }
                    if auto.is_some() {
                        f.notice = Some(
                            "The server recorded a different text (clipped?) — review it and press ctrl+s again"
                                .into(),
                        );
                    }
                    f.phase = RvPhase::Prompt {
                        ed: TextEditor::new(&prompt, true),
                        request,
                        prompt,
                        digest,
                        harness: st(&x, "harness").to_string(),
                        subject: st(&x, "subject").to_string(),
                        label: st(&x, "label").to_string(),
                        uses_provider: st(&x, "uses_provider").to_string(),
                    };
                }
                Err(e) => {
                    let msg = if e.is_method_not_found() {
                        NEWER_SERVER.to_string()
                    } else {
                        format!("Reviewer not prepared: {}", e.message)
                    };
                    v.sub = TaskSub::None;
                    v.notice = Some(msg);
                }
            }
        }
        T4Reply::ReviewerStarted { view } => {
            let Some(v) = view_of(app, view) else {
                return;
            };
            match res {
                Ok(x) => {
                    v.sub = TaskSub::None;
                    v.notice = Some(format!(
                        "Reviewer started (run {}) — its findings arrive as review notes (agent opinion, not evidence)",
                        crate::draw::truncate(st(&x, "run"), 12)
                    ));
                    fetch(app, false);
                }
                Err(e) if e.reason() == Some("prompt_mismatch") => {
                    v.sub = TaskSub::None;
                    v.notice = Some(
                        "Not started: the prompt changed since it was shown — nothing was launched; R to review it again"
                            .into(),
                    );
                }
                Err(e) => {
                    v.sub = TaskSub::None;
                    v.notice = Some(format!("Reviewer not started: {}", e.message));
                }
            }
        }
        T4Reply::Notes { view } => {
            let Some(v) = view_of(app, view) else {
                return;
            };
            match res {
                Ok(x) => v.t4.notes = Some(x),
                Err(e) => {
                    if let TaskSub::T4(T4Sub::Notes(n)) = &mut v.sub {
                        n.error = Some(if e.is_method_not_found() {
                            NEWER_SERVER.into()
                        } else {
                            e.message
                        });
                    }
                }
            }
        }
        T4Reply::Classified { view } => {
            let Some(v) = view_of(app, view) else {
                return;
            };
            let TaskSub::T4(T4Sub::Notes(n)) = &mut v.sub else {
                if let Err(e) = res {
                    v.notice = Some(format!("Not classified: {}", e.message));
                }
                return;
            };
            n.busy = false;
            match res {
                Ok(x) => {
                    let class = x
                        .pointer("/note/classification")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    v.notice = Some(format!("Note marked {}", classification_label(class)));
                    let (task, id) = (v.task.clone(), v.id);
                    app.command_on(
                        mi,
                        "task.review.notes",
                        json!({"task": task}),
                        Pending::Task(Reply::T4(T4Reply::Notes { view: id })),
                    );
                    fetch(app, false);
                }
                Err(e) => n.error = Some(e.message),
            }
        }
        T4Reply::Deps { view } => {
            let Some(v) = view_of(app, view) else {
                return;
            };
            match res {
                Ok(x) => v.t4.deps = x.get("dependencies").cloned(),
                Err(e) => {
                    if let TaskSub::T4(T4Sub::Deps(d)) = &mut v.sub {
                        d.error = Some(if e.is_method_not_found() {
                            NEWER_SERVER.into()
                        } else {
                            e.message
                        });
                    }
                }
            }
        }
        T4Reply::DepChanged { view, added } => {
            let err = res.as_ref().err().map(|e| dep_error_text(app, mi, e));
            let Some(v) = view_of(app, view) else {
                return;
            };
            if let TaskSub::T4(T4Sub::Deps(d)) = &mut v.sub {
                d.busy = false;
                d.error = err.clone();
            } else if let Some(e) = &err {
                v.notice = Some(e.clone());
            }
            if err.is_none() {
                v.notice = Some(if added {
                    "Link confirmed — it only informs ranking and never changes either task".into()
                } else {
                    "Link removed (kept as history)".into()
                });
                refresh_deps(app);
                fetch(app, false);
            }
        }
        T4Reply::EffortSet { view, label } => {
            let Some(v) = view_of(app, view) else {
                return;
            };
            match res {
                Ok(_) => {
                    if matches!(v.sub, TaskSub::T4(T4Sub::Effort { .. })) {
                        v.sub = TaskSub::None;
                    }
                    v.notice = Some(label);
                    fetch(app, false);
                }
                Err(e) => v.notice = Some(format!("Effort not set: {}", e.message)),
            }
        }
    }
}

/// The assist flow produced a model estimate for `task`: keep it (with its source) for the
/// open task view; it is applied only by an explicit action.
pub fn on_model_estimate(app: &mut App, mi: usize, task: &str, output: &Value, request: &str) {
    if let Some(v) = &mut app.task_view
        && v.machine == mi
        && v.task == task
    {
        v.t4.model_estimate = Some(json!({
            "effort": effort_param(st(output, "effort")),
            "rationale": st(output, "rationale"),
            "request": request,
        }));
    }
}

fn classification_label(c: &str) -> &'static str {
    match c {
        "blocking" => "blocking",
        "not_blocking" => "not blocking",
        "dismissed" => "dismissed",
        _ => "unassessed",
    }
}

fn severity_tag(s: &str) -> &'static str {
    match s {
        "blocking" => "BLOCKING",
        "concern" => "CONCERN",
        "nit" => "NIT",
        "no_findings" => "NO FINDINGS",
        _ => "UNSTRUCTURED",
    }
}

// ---- drawing --------------------------------------------------------------------------------------

/// T4 additions to the review block of the detail view.
pub fn review_lines(app: &App, p: &Value, v: &TaskView, w: usize, out: &mut Lines) {
    if !t4_supported(p) {
        return;
    }
    let t = app.theme;
    // Snapshot candidate.
    let subject = p.get("subject").cloned().unwrap_or(Value::Null);
    if st(&subject, "kind") == "dirty_snapshot" {
        let current = p.get("subject_current").and_then(Value::as_bool) != Some(false);
        let (text, style) = if current {
            (SNAPSHOT_CANDIDATE, t.s(t.accent))
        } else {
            (SNAPSHOT_EARLIER, t.s(t.yellow))
        };
        out.push(("Snapshot".into(), t.bold(t.fg)));
        wrap_push(out, text, "  ", w.saturating_sub(2), style);
        let stored = subject
            .pointer("/snapshot/commit")
            .and_then(Value::as_str)
            .map(|c| {
                format!(
                    " · stored as commit {} — your files, index and branches are unchanged",
                    &c[..c.len().min(10)]
                )
            })
            .unwrap_or_default();
        out.push((format!("  {}{stored}", subject_label(&subject)), t.dim()));
    }
    if v.t4.snapshotting {
        out.push(("  capturing snapshot…".into(), t.s(t.yellow)));
    }
    // Effort.
    if let Some(e) = p.get("effort").filter(|e| !e.is_null()) {
        out.push(("Effort".into(), t.bold(t.fg)));
        let set = e.get("set").and_then(Value::as_str);
        out.push((
            match set {
                Some(s) => format!(
                    "  set: {} (yours — ranking uses only this)",
                    effort_label(s)
                ),
                None => "  set: not set (ranking treats it as unknown)".into(),
            },
            t.text(),
        ));
        if let Some(h) = e.get("heuristic").filter(|h| !h.is_null()) {
            out.push((
                format!(
                    "  estimate: {} · source: heuristic — not applied",
                    effort_label(st(h, "effort"))
                ),
                t.dim(),
            ));
            for r in arr(h, "reasons").iter().filter_map(Value::as_str).take(3) {
                wrap_push(out, r, "    · ", w.saturating_sub(6), t.dim());
            }
        }
        if let Some(m) = &v.t4.model_estimate {
            out.push((
                format!(
                    "  estimate: {} · source: assistant — not applied",
                    effort_label(st(m, "effort"))
                ),
                t.dim(),
            ));
            if !st(m, "rationale").is_empty() {
                wrap_push(
                    out,
                    st(m, "rationale"),
                    "    ",
                    w.saturating_sub(4),
                    t.dim(),
                );
            }
        }
    }
    // Reviewer notes.
    let notes = notes_of(v);
    let runs = reviewer_runs_of(v);
    if !notes.is_empty() || !runs.is_empty() {
        let open = notes
            .iter()
            .filter(|n| n.get("open_concern").and_then(Value::as_bool) == Some(true))
            .count() as u64;
        out.push((
            format!(
                "Review notes · {} · {} need your decision — [N] classify",
                plural(notes.len() as u64, "note", "notes"),
                open
            ),
            t.bold(t.fg),
        ));
        for r in runs.iter().rev().take(2) {
            out.push((
                format!(
                    "  reviewer {} · {}{}",
                    st(r, "harness"),
                    st(r, "state"),
                    r.get("error")
                        .and_then(Value::as_str)
                        .map(|e| format!(" — {e}"))
                        .unwrap_or_default()
                ),
                t.dim(),
            ));
        }
        for n in notes.iter().take(4) {
            let open = n.get("open_concern").and_then(Value::as_bool) == Some(true);
            out.push((
                format!(
                    "  {:<13}{}",
                    severity_tag(st(n, "severity")),
                    crate::draw::truncate(&st(n, "text").replace('\n', " "), 70)
                ),
                if open { t.s(t.yellow) } else { t.dim() },
            ));
        }
    }
    // Dependencies.
    let d = p.get("dependencies").cloned().unwrap_or(Value::Null);
    let rows = dep_rows(app, v);
    let blocks = d
        .get("blocks_open_tasks")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if !rows.is_empty() || blocks > 0 {
        out.push((
            format!("Dependencies · {} — [d] edit", blocks_text(blocks)),
            t.bold(t.fg),
        ));
        for (_, l) in rows.iter().take(6) {
            out.push((format!("  {l}"), t.text()));
        }
    }
}

/// Draw an open T4 screen into the task area `r`, from row `y`.
pub fn draw_sub(app: &App, g: &mut Grid, v: &TaskView, s: &T4Sub, r: SRect, mut y: u16) {
    let t = app.theme;
    let w = r.w.saturating_sub(4) as usize;
    let bottom = r.y + r.h.saturating_sub(1);
    let sel = |on: bool| if on { t.sel(t.accent) } else { t.text() };
    let mut lines: Lines = Vec::new();
    let (title, footer): (&str, String) = match s {
        T4Sub::WorkspaceChanging { message } => {
            lines.push((WORKSPACE_CHANGING.into(), t.bold(t.red)));
            lines.push((
                "Nothing was recorded: the checkout kept changing while it was being captured."
                    .into(),
                t.text(),
            ));
            if !message.is_empty() {
                wrap_push(&mut lines, message, "  ", w, t.dim());
            }
            lines.push((
                "Wait until the agent stops writing and snapshot again, or commit and select the committed revision."
                    .into(),
                t.dim(),
            ));
            ("Snapshot", "s snapshot again · esc close".into())
        }
        T4Sub::Reviewer(f) => {
            if let Some(n) = &f.notice {
                lines.push((n.clone(), t.bold(t.yellow)));
            }
            if let Some(e) = &f.error {
                lines.push((e.clone(), t.s(t.red)));
            }
            match &f.phase {
                RvPhase::Requesting { auto_start } => {
                    lines.push((
                        if auto_start.is_some() {
                            "Recording your edited prompt… (nothing is launched yet)".to_string()
                        } else {
                            "Preparing the reviewer prompt… (nothing is launched or sent)"
                                .to_string()
                        },
                        t.dim(),
                    ));
                    ("Request reviewer", "esc cancel".into())
                }
                RvPhase::Starting { .. } => {
                    lines.push(("Starting the reviewer…".into(), t.s(t.yellow)));
                    ("Request reviewer", "esc close".into())
                }
                RvPhase::Prompt {
                    prompt,
                    harness,
                    subject,
                    label,
                    uses_provider,
                    ed,
                    ..
                } => {
                    lines.push((
                        format!(
                            "Reviewer: {harness} · subject {} · review-only run bound with role `review`",
                            crate::draw::truncate(subject, 12)
                        ),
                        t.bold(t.fg),
                    ));
                    if !label.is_empty() {
                        wrap_push(&mut lines, label, "", w, t.dim());
                    }
                    if !uses_provider.is_empty() {
                        lines.push((format!("⚠ {uses_provider}"), t.s(t.yellow)));
                    }
                    let edited = ed.text() != *prompt;
                    lines.push((
                        if edited {
                            "Exact prompt (edited — recorded as your text before starting):"
                                .to_string()
                        } else {
                            "Exact prompt — sent once as the reviewer's initial prompt:".to_string()
                        },
                        t.dim(),
                    ));
                    // Lines above, then the editor in the rest of the area.
                    let title_y = y;
                    g.put_str(
                        r.x + 1,
                        title_y,
                        "Request reviewer",
                        t.bold(t.accent),
                        r.w.saturating_sub(2),
                    );
                    y += 1;
                    for (s, stl) in &lines {
                        if y >= bottom {
                            break;
                        }
                        g.put_str(r.x + 2, y, s, *stl, r.w.saturating_sub(3));
                        y += 1;
                    }
                    let er = SRect {
                        x: r.x + 2,
                        y,
                        w: r.w.saturating_sub(3),
                        h: bottom.saturating_sub(y),
                    };
                    ed.draw(g, er, t.text(), t.rev());
                    g.put_str(
                        r.x + 1,
                        bottom,
                        "edit · ctrl+s start the reviewer with this exact prompt · ctrl+r reset · esc cancel (nothing launched)",
                        t.dim(),
                        r.w.saturating_sub(2),
                    );
                    return;
                }
            }
        }
        T4Sub::Notes(n) => {
            lines.push((NOTE_LABEL.into(), t.dim()));
            let notes = notes_of(v);
            if notes.is_empty() {
                lines.push(("No review notes yet".into(), t.dim()));
            }
            for (i, note) in notes.iter().enumerate() {
                let class = st(note, "classification");
                let open = note.get("open_concern").and_then(Value::as_bool) == Some(true);
                let state = match class {
                    "" | "unassessed" if open => "unassessed · needs your decision".to_string(),
                    "" | "unassessed" => "unassessed".to_string(),
                    c => {
                        let r = note
                            .get("classification_reason")
                            .and_then(Value::as_str)
                            .map(|r| format!(" — {r}"))
                            .unwrap_or_default();
                        format!("{} by you{r}", classification_label(c))
                    }
                };
                lines.push((
                    format!(
                        "{:<13}{}",
                        severity_tag(st(note, "severity")),
                        crate::draw::truncate(
                            &st(note, "text").replace('\n', " "),
                            w.saturating_sub(14)
                        )
                    ),
                    if i == n.sel {
                        sel(true)
                    } else if open {
                        t.s(t.yellow)
                    } else {
                        t.text()
                    },
                ));
                let turn = note
                    .get("turn")
                    .and_then(Value::as_u64)
                    .map(|t| format!(" turn {t}"))
                    .unwrap_or_default();
                lines.push((
                    format!(
                        "             {state} · reviewer run {}{turn}",
                        crate::draw::truncate(st(note, "run"), 10)
                    ),
                    t.dim(),
                ));
            }
            if let Some((class, reason)) = &n.reason {
                lines.push((String::new(), t.text()));
                lines.push((
                    format!(
                        "Mark {} — reason{}: {reason}▏",
                        classification_label(class),
                        if class == "dismissed" {
                            " (required)"
                        } else {
                            " (optional)"
                        }
                    ),
                    t.bold(t.fg),
                ));
            }
            if n.busy {
                lines.push(("recording…".into(), t.s(t.yellow)));
            }
            if let Some(e) = &n.error {
                lines.push((e.clone(), t.s(t.red)));
            }
            let footer = if n.reason.is_some() {
                "type a reason · enter confirm · esc back".to_string()
            } else {
                "j/k move · b blocking · n not blocking · x dismiss (reason required) · esc back"
                    .to_string()
            };
            ("Review notes", footer)
        }
        T4Sub::Deps(d) => {
            let deps = deps_of(v);
            let blocks = deps
                .get("blocks_open_tasks")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            lines.push((
                format!(
                    "Confirmed links only · this task {} · links inform ranking, never change a task",
                    blocks_text(blocks)
                ),
                t.dim(),
            ));
            let rows = dep_rows(app, v);
            if rows.is_empty() {
                lines.push(("No links".into(), t.dim()));
            }
            for (i, (_, l)) in rows.iter().enumerate() {
                lines.push((l.clone(), sel(i == d.sel && d.picker.is_none())));
            }
            if let Some(p) = &d.picker {
                lines.push((String::new(), t.text()));
                lines.push((
                    format!(
                        "{} · kind ‹ {} › · filter: {}▏",
                        if p.waits_for {
                            "This task waits for…"
                        } else {
                            "…waits for this task"
                        },
                        if p.blocks { "blocks" } else { "related" },
                        p.filter
                    ),
                    t.bold(t.fg),
                ));
                let cands = candidates(app, v, &p.filter);
                if cands.is_empty() {
                    lines.push(("  no matching tasks".into(), t.dim()));
                }
                for (i, (_, l)) in cands.iter().enumerate().take(12) {
                    lines.push((format!("  {l}"), sel(i == p.sel)));
                }
            }
            if let Some(e) = &d.confirm_remove {
                let what = rows
                    .iter()
                    .find(|(edge, _)| st(edge, "id") == e)
                    .map(|(_, l)| l.clone())
                    .unwrap_or_default();
                lines.push((
                    format!("Remove “{what}”? [y] remove  [n] keep"),
                    t.bold(t.yellow),
                ));
            }
            if d.busy {
                lines.push(("recording…".into(), t.s(t.yellow)));
            }
            if let Some(e) = &d.error {
                wrap_push(&mut lines, e, "", w, t.bold(t.red));
            }
            let footer = if d.picker.is_some() {
                "type to filter · ↑↓ choose · tab blocks/related · enter confirm link · esc back"
            } else {
                "j/k move · a this task waits for… · A …waits for this task · x remove · esc back"
            };
            ("Dependencies", footer.to_string())
        }
        T4Sub::Effort { sel: s } => {
            let p = package(v).cloned().unwrap_or(Value::Null);
            let e = p.get("effort").cloned().unwrap_or(Value::Null);
            lines.push((
                match e.get("set").and_then(Value::as_str) {
                    Some(x) => format!("Set: {} (yours)", effort_label(x)),
                    None => "Set: not set".into(),
                },
                t.bold(t.fg),
            ));
            for (i, (_, l)) in EFFORTS.iter().enumerate() {
                lines.push((format!("  {l}"), sel(i == *s)));
            }
            lines.push((String::new(), t.text()));
            match e.get("heuristic").filter(|h| !h.is_null()) {
                Some(h) => {
                    lines.push((
                        format!(
                            "Heuristic estimate: {} · source: heuristic · [h] apply",
                            effort_label(st(h, "effort"))
                        ),
                        t.text(),
                    ));
                    let lab = st(h, "label");
                    if !lab.is_empty() {
                        lines.push((format!("  {lab}"), t.dim()));
                    }
                    for r in arr(h, "reasons").iter().filter_map(Value::as_str) {
                        wrap_push(&mut lines, r, "  · ", w.saturating_sub(4), t.dim());
                    }
                }
                None => lines.push(("Heuristic estimate: none".into(), t.dim())),
            }
            match &v.t4.model_estimate {
                Some(m) => {
                    lines.push((
                        format!(
                            "Model estimate: {} · source: assistant · [m] apply",
                            effort_label(st(m, "effort"))
                        ),
                        t.text(),
                    ));
                    if !st(m, "rationale").is_empty() {
                        wrap_push(
                            &mut lines,
                            st(m, "rationale"),
                            "  ",
                            w.saturating_sub(2),
                            t.dim(),
                        );
                    }
                }
                None => lines.push((
                    "Model estimate: none — [E] Estimate effort (shows the exact payload first)"
                        .into(),
                    t.dim(),
                )),
            }
            lines.push((
                "Estimates are labelled with their source and never applied by themselves; the five-minute view uses only your value."
                    .into(),
                t.dim(),
            ));
            (
                "Effort",
                "j/k choose · enter set · h apply heuristic · m apply model estimate · E estimate · esc back"
                    .into(),
            )
        }
    };
    g.put_str(r.x + 1, y, title, t.bold(t.accent), r.w.saturating_sub(2));
    y += 1;
    for (s, stl) in lines {
        if y >= bottom {
            break;
        }
        g.put_str(r.x + 2, y, &s, stl, r.w.saturating_sub(3));
        y += 1;
    }
    g.put_str(r.x + 1, bottom, &footer, t.dim(), r.w.saturating_sub(2));
}

#[cfg(test)]
#[path = "tasks_t4_tests.rs"]
mod tests;
