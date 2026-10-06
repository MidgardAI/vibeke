//! Task surfaces of lane 2C (spec 15 §3, §5, §6.4, §11):
//!
//! - **Human review** (`H`): pick one of the intent's *human* criteria, then `y` supported,
//!   `n` does not meet it (a note is required), `w` withdraw; `enter` records it with
//!   `task.review.human_review {task, criterion, verdict, note?, expected_subject}` for the
//!   revision on screen. The server refuses check criteria and a moved subject; the review is
//!   never a check result and never accepts the task.
//! - **Select changes** (`P`): the checkout's changed files (`git.status`), `space` ticks files,
//!   `enter` captures only those with `task.review.snapshot {task, paths}` — a selected-patch
//!   candidate whose checks verify the selection alone; accepting it never reviews the rest.
//! - **Link run**: when **Track this work** is refused because the run's identity is not
//!   verified, the form asks `task.link.status` and shows why, the remedies (install the
//!   integration through setup, or start the agent through Vibeke) and the verified runs nearby;
//!   `enter` on one opens **Track this work** for that run instead. Nothing is bound or
//!   installed implicitly.
//!
//! Mutations go through [`App::mutate`] (idempotency key persisted first, 15 §10.3).

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::screen::{Grid, Rect as SRect};
use crate::tasks::{
    Lines, Reply, TaskSub, TaskView, TrackForm, TrackPhase, arr, edit, fetch, package, st, view_of,
    wrap_push,
};
use serde_json::{Value, json};
use vk_proto::input::{Key, KeyEvent, NamedKey};
use vk_proto::render::Style;

pub const HUMAN_NOTE: &str = "Your judgment decides this human criterion on this revision only — it is not a check result and does not accept the task.";
pub const SELECT_NOTE: &str = "Only the ticked files are captured (on top of HEAD); checks on that subject verify the selection alone, and accepting it does not review the rest of the checkout.";

#[derive(Debug, Clone, PartialEq)]
pub enum HumanPhase {
    Pick,
    /// Typing the note for `verdict`.
    Note {
        verdict: String,
    },
    Saving,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HumanForm {
    /// (criterion id, text) of the human criteria.
    pub criteria: Vec<(String, String)>,
    pub sel: usize,
    pub subject: String,
    pub note: String,
    pub phase: HumanPhase,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PatchPicker {
    /// (path, ticked).
    pub files: Vec<(String, bool)>,
    pub sel: usize,
    pub loading: bool,
    pub saving: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Sub {
    Human(HumanForm),
    Patch(PatchPicker),
}

#[derive(Debug, Clone)]
pub enum R {
    HumanSaved { view: u64 },
    Files { view: u64 },
    PatchSaved { view: u64 },
    LinkStatus { form: u64 },
}

fn set_sub(app: &mut App, s: Sub) {
    if let Some(v) = &mut app.task_view {
        v.sub = TaskSub::Lane2c(s);
    }
}

fn notice(app: &mut App, n: impl Into<String>) {
    if let Some(v) = &mut app.task_view {
        v.notice = Some(n.into());
        if matches!(v.sub, TaskSub::Lane2c(_)) {
            v.sub = TaskSub::None;
        }
    }
}

/// The intent's human criteria: (id, text).
pub fn human_criteria(p: &Value) -> Vec<(String, String)> {
    arr(&p["intent"], "criteria")
        .iter()
        .filter(|c| st(c, "evaluation") == "human")
        .map(|c| (st(c, "id").to_string(), st(c, "text").to_string()))
        .collect()
}

/// The subject on screen can carry a human review (immutable: not the live checkout).
pub fn reviewable_subject(p: &Value) -> Option<String> {
    let s = p.get("subject").filter(|s| !s.is_null())?;
    (st(s, "kind") != "checkout_live").then(|| st(s, "id").to_string())
}

// ---- keys -----------------------------------------------------------------------------------------

pub fn is_main_key(ev: &KeyEvent) -> bool {
    !ev.mods.ctrl() && !ev.mods.alt() && matches!(ev.key, Key::Char('H' | 'P'))
}

pub fn keys_hint(v: &TaskView) -> Vec<&'static str> {
    match package(v) {
        Some(p) if p.get("human_reviews").is_some() => vec!["H human review", "P select changes"],
        _ => vec![],
    }
}

pub fn main_key(app: &mut App, ev: KeyEvent) {
    let Some(v) = &app.task_view else {
        return;
    };
    let Some(p) = package(v).cloned() else {
        notice(app, "Review package still loading");
        return;
    };
    if p.get("human_reviews").is_none() {
        notice(app, crate::tasks_t4::NEWER_SERVER);
        return;
    }
    let (mi, id) = (v.machine, v.id);
    match ev.key {
        Key::Char('H') => {
            let criteria = human_criteria(&p);
            if criteria.is_empty() {
                notice(app, "This task has no human criteria to review");
                return;
            }
            let Some(subject) = reviewable_subject(&p) else {
                notice(
                    app,
                    "Select a committed revision or snapshot to record a review",
                );
                return;
            };
            set_sub(
                app,
                Sub::Human(HumanForm {
                    criteria,
                    sel: 0,
                    subject,
                    note: String::new(),
                    phase: HumanPhase::Pick,
                    error: None,
                }),
            );
        }
        Key::Char('P') => {
            let path = v
                .detail
                .as_ref()
                .and_then(|d| d.get("task"))
                .map(|t| {
                    t.get("worktree_path")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .unwrap_or(st(t, "repo_root"))
                        .to_string()
                })
                .unwrap_or_default();
            if path.is_empty() {
                notice(app, "No checkout for this task");
                return;
            }
            set_sub(
                app,
                Sub::Patch(PatchPicker {
                    files: vec![],
                    sel: 0,
                    loading: true,
                    saving: false,
                    error: None,
                }),
            );
            app.command_on(
                mi,
                "git.status",
                json!({"path": path}),
                Pending::Task(Reply::Lane2c(R::Files { view: id })),
            );
        }
        _ => {}
    }
}

pub fn sub_key(app: &mut App, s: Sub, ev: KeyEvent) {
    let esc = ev.key == Key::Named(NamedKey::Escape);
    match s {
        Sub::Human(mut f) => match f.phase.clone() {
            HumanPhase::Saving => {
                if esc {
                    notice(app, "Recording continues in the background");
                } else {
                    set_sub(app, Sub::Human(f));
                }
            }
            HumanPhase::Pick => match ev.key {
                _ if esc || ev.key == Key::Char('q') => notice(app, "Nothing was recorded"),
                Key::Char('j') | Key::Named(NamedKey::Down) => {
                    f.sel = (f.sel + 1).min(f.criteria.len().saturating_sub(1));
                    set_sub(app, Sub::Human(f));
                }
                Key::Char('k') | Key::Named(NamedKey::Up) => {
                    f.sel = f.sel.saturating_sub(1);
                    set_sub(app, Sub::Human(f));
                }
                Key::Char(c @ ('y' | 'n' | 'w')) => {
                    let verdict = match c {
                        'y' => "supported",
                        'n' => "failed",
                        _ => "withdrawn",
                    };
                    f.phase = HumanPhase::Note {
                        verdict: verdict.into(),
                    };
                    f.error = None;
                    set_sub(app, Sub::Human(f));
                }
                _ => set_sub(app, Sub::Human(f)),
            },
            HumanPhase::Note { verdict } => match ev.key {
                _ if esc => {
                    f.phase = HumanPhase::Pick;
                    set_sub(app, Sub::Human(f));
                }
                Key::Named(NamedKey::Enter) => {
                    if verdict == "failed" && f.note.trim().is_empty() {
                        f.error = Some("Say what is wrong: a failed review needs a note".into());
                        set_sub(app, Sub::Human(f));
                        return;
                    }
                    submit_human(app, f, &verdict);
                }
                _ => {
                    edit(&mut f.note, &ev);
                    set_sub(app, Sub::Human(f));
                }
            },
        },
        Sub::Patch(mut pk) => match ev.key {
            _ if esc || ev.key == Key::Char('q') => notice(app, "Nothing was captured"),
            _ if pk.saving || pk.loading => set_sub(app, Sub::Patch(pk)),
            Key::Char('j') | Key::Named(NamedKey::Down) => {
                pk.sel = (pk.sel + 1).min(pk.files.len().saturating_sub(1));
                set_sub(app, Sub::Patch(pk));
            }
            Key::Char('k') | Key::Named(NamedKey::Up) => {
                pk.sel = pk.sel.saturating_sub(1);
                set_sub(app, Sub::Patch(pk));
            }
            Key::Char(' ') | Key::Char('x') => {
                if let Some(f) = pk.files.get_mut(pk.sel) {
                    f.1 = !f.1;
                }
                set_sub(app, Sub::Patch(pk));
            }
            Key::Named(NamedKey::Enter) => {
                let paths: Vec<String> = pk
                    .files
                    .iter()
                    .filter(|f| f.1)
                    .map(|f| f.0.clone())
                    .collect();
                if paths.is_empty() {
                    pk.error = Some("Tick at least one file (space)".into());
                    set_sub(app, Sub::Patch(pk));
                    return;
                }
                let Some(v) = &app.task_view else { return };
                let (mi, task, id) = (v.machine, v.task.clone(), v.id);
                let key = app.new_idempotency_key("select");
                pk.saving = true;
                pk.error = None;
                if app.mutate(
                    mi,
                    "task.review.snapshot",
                    json!({"task": task, "paths": paths, "idempotency_key": key}),
                    Pending::Task(Reply::Lane2c(R::PatchSaved { view: id })),
                ) {
                    set_sub(app, Sub::Patch(pk));
                }
            }
            _ => set_sub(app, Sub::Patch(pk)),
        },
    }
}

fn submit_human(app: &mut App, mut f: HumanForm, verdict: &str) {
    let Some(v) = &app.task_view else { return };
    let (mi, task, id) = (v.machine, v.task.clone(), v.id);
    let Some((crit, _)) = f.criteria.get(f.sel).cloned() else {
        return;
    };
    let key = app.new_idempotency_key("human-review");
    let mut params = json!({
        "task": task,
        "criterion": crit,
        "verdict": verdict,
        "expected_subject": f.subject,
        "idempotency_key": key,
    });
    if !f.note.trim().is_empty() {
        params["note"] = json!(f.note.trim());
    }
    f.phase = HumanPhase::Saving;
    if app.mutate(
        mi,
        "task.review.human_review",
        params,
        Pending::Task(Reply::Lane2c(R::HumanSaved { view: id })),
    ) {
        set_sub(app, Sub::Human(f));
    }
}

// ---- replies --------------------------------------------------------------------------------------

pub fn on_reply(app: &mut App, mi: usize, r: R, res: Result<Value, RpcErr>) {
    app.dirty = true;
    match r {
        R::HumanSaved { view } => {
            let Some(v) = view_of(app, view) else { return };
            match res {
                Ok(_) => {
                    v.sub = TaskSub::None;
                    v.notice = Some(format!("Review recorded · {HUMAN_NOTE}"));
                    fetch(app, false);
                }
                Err(e) => {
                    let msg = match e.reason() {
                        Some("review_changed") => {
                            "The revision changed since you looked — review it again".to_string()
                        }
                        Some("not_a_human_criterion") => {
                            "Only human criteria are decided by your review".to_string()
                        }
                        _ => e.message.clone(),
                    };
                    if let TaskSub::Lane2c(Sub::Human(f)) = &mut v.sub {
                        f.phase = HumanPhase::Pick;
                        f.error = Some(msg);
                    } else {
                        v.notice = Some(msg);
                    }
                }
            }
        }
        R::Files { view } => {
            let Some(v) = view_of(app, view) else { return };
            let TaskSub::Lane2c(Sub::Patch(pk)) = &mut v.sub else {
                return;
            };
            pk.loading = false;
            match res {
                Ok(s) => {
                    pk.files = arr(&s, "files")
                        .iter()
                        .filter(|f| !f.get("secret").and_then(Value::as_bool).unwrap_or(false))
                        .map(|f| (st(f, "path").to_string(), false))
                        .collect();
                    if pk.files.is_empty() {
                        pk.error = Some("No uncommitted changes in this checkout".into());
                    }
                }
                Err(e) => pk.error = Some(e.message),
            }
        }
        R::PatchSaved { view } => {
            let Some(v) = view_of(app, view) else { return };
            match res {
                Ok(r) => {
                    v.sub = TaskSub::None;
                    v.notice = Some(
                        r.get("label")
                            .and_then(Value::as_str)
                            .unwrap_or("Selected changes captured")
                            .to_string(),
                    );
                    fetch(app, false);
                }
                Err(e) => {
                    let msg = match e.reason() {
                        Some("workspace_changing") => {
                            crate::tasks_t4::WORKSPACE_CHANGING.to_string()
                        }
                        Some("nothing_selected") => {
                            "The ticked files hold no uncommitted change".to_string()
                        }
                        _ => e.message.clone(),
                    };
                    if let TaskSub::Lane2c(Sub::Patch(pk)) = &mut v.sub {
                        pk.saving = false;
                        pk.error = Some(msg);
                    } else {
                        v.notice = Some(msg);
                    }
                }
            }
        }
        R::LinkStatus { form } => {
            let Some(f) = app.track.as_mut().filter(|f| f.id == form) else {
                return;
            };
            match res {
                Ok(v) => {
                    f.link = Some(v);
                    f.link_sel = 0;
                }
                Err(e) if e.is_method_not_found() => {}
                Err(e) => f.error = Some(e.message),
            }
            let _ = mi;
        }
    }
}

// ---- Link run (Track form, unverified identity) ---------------------------------------------------

/// Ask why the form's run is not verified (once per form).
pub fn request_link(app: &mut App, mi: usize) {
    let Some(f) = app.track.as_ref() else { return };
    if f.link.is_some() {
        return;
    }
    let (id, run) = (f.id, f.run.clone());
    app.command_on(
        mi,
        "task.link.status",
        json!({"run": run}),
        Pending::Task(Reply::Lane2c(R::LinkStatus { form: id })),
    );
}

/// Keys of the **Link run** step; returns whether the key was handled (the form stays open
/// unless a candidate was chosen or it was closed).
pub fn link_key(app: &mut App, mut f: TrackForm, ev: &KeyEvent) -> bool {
    let n = f
        .link
        .as_ref()
        .map(|l| arr(l, "candidates").len())
        .unwrap_or(0);
    match ev.key {
        Key::Char('j') | Key::Named(NamedKey::Down) => {
            f.link_sel = (f.link_sel + 1).min(n.saturating_sub(1));
        }
        Key::Char('k') | Key::Named(NamedKey::Up) => f.link_sel = f.link_sel.saturating_sub(1),
        Key::Named(NamedKey::Enter) if n > 0 => {
            let cand = f
                .link
                .as_ref()
                .and_then(|l| arr(l, "candidates").get(f.link_sel).cloned())
                .unwrap_or(Value::Null);
            let (mi, run, pane) = (
                f.machine,
                st(&cand, "run").to_string(),
                st(&cand, "pane").to_string(),
            );
            crate::tasks::open_track_run(app, mi, &run, &pane);
            return true;
        }
        _ => return false,
    }
    app.track = Some(f);
    app.mode = Mode::Popup(Popup::Track);
    true
}

/// The **Link run** step's lines.
pub fn link_lines(app: &App, f: &TrackForm, out: &mut Vec<(String, Style)>) {
    let t = app.theme;
    let Some(l) = &f.link else {
        out.push(("checking why…".into(), t.dim()));
        return;
    };
    for r in arr(l, "reasons") {
        out.push((format!("• {}", r.as_str().unwrap_or("")), t.text()));
    }
    let rem = arr(l, "remedies");
    if !rem.is_empty() {
        out.push(("To verify it:".into(), t.bold(t.fg)));
        for r in rem {
            let cmd = st(r, "command");
            out.push((
                if cmd.is_empty() {
                    format!("  – {}", st(r, "label"))
                } else {
                    format!("  – {} ({cmd})", st(r, "label"))
                },
                t.dim(),
            ));
        }
    }
    let cands = arr(l, "candidates");
    if cands.is_empty() {
        out.push((
            "No verified agent runs nearby to link instead.".into(),
            t.dim(),
        ));
    } else {
        out.push(("Verified runs you can track instead:".into(), t.bold(t.fg)));
        for (i, c) in cands.iter().enumerate() {
            let name = c
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(st(c, "harness"));
            let line = format!(
                "{} {name} · {} · {} turn(s){}",
                if i == f.link_sel { ">" } else { " " },
                st(c, "integration"),
                c.get("turns").and_then(Value::as_u64).unwrap_or(0),
                if c.get("same_pane").and_then(Value::as_bool) == Some(true) {
                    " · same pane"
                } else {
                    ""
                }
            );
            out.push((
                line,
                if i == f.link_sel {
                    t.sel(t.accent)
                } else {
                    t.text()
                },
            ));
        }
        out.push((
            "[enter] track the selected run · [esc] close".into(),
            t.dim(),
        ));
    }
}

/// Whether a reply moved the form into the unverified (Link run) step.
pub fn unverified(f: &TrackForm) -> bool {
    f.phase == TrackPhase::Unverified
}

// ---- drawing --------------------------------------------------------------------------------------

/// Review lines added to the task detail: human reviews, a selected-patch subject, purged data.
pub fn review_lines(app: &App, p: &Value, w: usize, out: &mut Lines) {
    let t = app.theme;
    if let Some(sel) = p.pointer("/subject/selection").filter(|s| !s.is_null()) {
        let paths: Vec<&str> = arr(sel, "paths").iter().filter_map(Value::as_str).collect();
        out.push((String::new(), t.text()));
        out.push(("Selected changes".into(), t.bold(t.fg)));
        wrap_push(out, &paths.join(", "), "  ", w, t.text());
        wrap_push(out, SELECT_NOTE, "  ", w, t.dim());
    }
    let hr = arr(p, "human_reviews");
    if !hr.is_empty() {
        out.push((String::new(), t.text()));
        out.push(("Your reviews".into(), t.bold(t.fg)));
        let texts: std::collections::HashMap<&str, &str> = arr(&p["intent"], "criteria")
            .iter()
            .map(|c| (st(c, "id"), st(c, "text")))
            .collect();
        for r in hr {
            let crit = texts
                .get(st(r, "criterion_id"))
                .copied()
                .unwrap_or("criterion");
            let mark = match st(r, "verdict") {
                "supported" => "✓",
                "failed" => "✗",
                _ => "↺",
            };
            let current =
                p.pointer("/subject/id").and_then(Value::as_str) == Some(st(r, "subject_id"));
            let line = format!(
                "  {mark} {crit} · {}{}",
                st(r, "label"),
                if current { "" } else { " (earlier revision)" }
            );
            wrap_push(
                out,
                &line,
                "    ",
                w,
                if current { t.text() } else { t.dim() },
            );
            if let Some(n) = r.get("note").and_then(Value::as_str) {
                wrap_push(out, &format!("“{n}”"), "    ", w, t.dim());
            }
        }
    }
    if let Some(pg) = p.get("purged").filter(|v| !v.is_null()) {
        out.push((String::new(), t.text()));
        wrap_push(
            out,
            &format!("⚠ {}", st(pg, "note")),
            "  ",
            w,
            t.s(t.yellow),
        );
    }
}

pub fn draw_sub(app: &App, g: &mut Grid, s: &Sub, r: SRect, mut y: u16) {
    let t = app.theme;
    let w = r.w.saturating_sub(4) as usize;
    let bottom = r.y + r.h.saturating_sub(1);
    let sel = |on: bool| if on { t.sel(t.accent) } else { t.text() };
    let mut lines: Lines = Vec::new();
    let (title, footer): (&str, &str) = match s {
        Sub::Human(f) => {
            wrap_push(&mut lines, HUMAN_NOTE, "", w, t.dim());
            lines.push((
                format!("Revision {}", crate::draw::truncate(&f.subject, 12)),
                t.dim(),
            ));
            if let Some(e) = &f.error {
                lines.push((e.clone(), t.s(t.red)));
            }
            for (i, (_, text)) in f.criteria.iter().enumerate() {
                lines.push((
                    format!("{} {text}", if i == f.sel { ">" } else { " " }),
                    sel(i == f.sel),
                ));
            }
            match &f.phase {
                HumanPhase::Pick => (
                    "Human review",
                    "j/k criterion · y supports it · n does not meet it · w withdraw · esc close",
                ),
                HumanPhase::Note { verdict } => {
                    lines.push((String::new(), t.text()));
                    lines.push((
                        format!(
                            "{} — note{}: {}",
                            match verdict.as_str() {
                                "supported" => "Supports the criterion",
                                "failed" => "Does not meet the criterion",
                                _ => "Withdraw your review",
                            },
                            if verdict == "failed" {
                                " (required)"
                            } else {
                                " (optional)"
                            },
                            f.note
                        ),
                        t.bold(t.fg),
                    ));
                    ("Human review", "type a note · enter record · esc back")
                }
                HumanPhase::Saving => {
                    lines.push(("Recording…".into(), t.s(t.yellow)));
                    ("Human review", "esc close")
                }
            }
        }
        Sub::Patch(pk) => {
            wrap_push(&mut lines, SELECT_NOTE, "", w, t.dim());
            if let Some(e) = &pk.error {
                lines.push((e.clone(), t.s(t.red)));
            }
            if pk.loading {
                lines.push(("loading changed files…".into(), t.dim()));
            }
            for (i, (path, on)) in pk.files.iter().enumerate() {
                lines.push((
                    format!(
                        "{} [{}] {path}",
                        if i == pk.sel { ">" } else { " " },
                        if *on { "x" } else { " " }
                    ),
                    sel(i == pk.sel),
                ));
            }
            if pk.saving {
                lines.push((
                    "Capturing the selection… (your files, index and branches stay as they are)"
                        .into(),
                    t.s(t.yellow),
                ));
            }
            (
                "Select changes to review",
                "j/k move · space tick · enter capture · esc close",
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
    g.put_str(r.x + 1, bottom, footer, t.dim(), r.w.saturating_sub(2));
}

#[cfg(test)]
#[path = "tasks_2c_tests.rs"]
mod tests;
