//! Task workspace badges and actions in the sidebar (05 §4, §PR status; 08 §2.1).
//!
//! - **PR badge.** A task workspace row shows its pull request as ` #123 ✓` (`✗ checks`, `…`,
//!   `draft`, `merged`, `closed`), coloured by checks/state. The TUI never runs `gh` or touches
//!   the network: it reads the server's **cached** `task.pr` result through `task.get` (which
//!   never runs `gh`), once when a task workspace appears and then every 60 s (the server's
//!   cache lifetime) while it has a checkout. Nothing cached means no badge; the cache fills
//!   when something asks for the PR (`vibeke task pr`, the server's own refresh).
//! - **Missing checkouts.** Reconcile (05 §4) marks a task whose checkout was removed outside
//!   Vibeke `missing`; its row shows ` ⊘ missing` and two actions are offered: **recreate**
//!   (`task.recreate {task}`: a new checkout from the task's branch) and **forget**
//!   (`task.forget {task}`, after a confirmation: drop the task record; nothing on disk is
//!   touched). They are in navigate mode on the task's row (`R`, `F`), as palette entries per
//!   missing task (`Recreate missing task #k7 …`), and as `task_recreate` / `task_forget` for the
//!   focused task. A server without these methods says so; nothing is retried on its own.

use crate::app::{Action, App, Mode, Pending, Popup, RpcErr};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use vk_proto::input::{Key, KeyEvent, KeyKind};
use vk_proto::model::Task;
use vk_proto::render::Style;

/// How often a task's cached PR status is re-read (the server caches `gh` for 60 s).
pub const REFRESH: Duration = Duration::from_secs(60);

/// A cached `PrLookup` (05 PR status).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Pr {
    /// `pr` | `no_pr` | `unavailable`.
    pub kind: String,
    pub label: String,
    /// `none` | `pending` | `passing` | `failing`.
    pub checks: String,
    pub state: String,
    pub draft: bool,
    pub url: String,
}

impl Pr {
    pub fn from_value(v: &Value) -> Option<Pr> {
        let kind = v["kind"].as_str()?.to_string();
        let p = &v["pr"];
        Some(Pr {
            kind,
            label: p["label"].as_str().unwrap_or_default().to_string(),
            checks: p["checks"].as_str().unwrap_or_default().to_string(),
            state: p["state"].as_str().unwrap_or_default().to_lowercase(),
            draft: p["is_draft"].as_bool().unwrap_or(false),
            url: p["url"].as_str().unwrap_or_default().to_string(),
        })
    }
}

#[derive(Debug, Default)]
pub struct State {
    /// (machine, task) → cached PR status.
    pub pr: HashMap<(usize, String), Pr>,
    /// When each task was last asked.
    pub asked: HashMap<(usize, String), Instant>,
    pub inflight: HashSet<(usize, String)>,
    /// Machines whose server has no `task.get`.
    pub unsupported: HashSet<usize>,
}

#[derive(Debug, Clone)]
pub enum Reply {
    Get { task: String },
    Recreate { task: String },
    Forget { task: String },
}

fn find<'a>(app: &'a App, mi: usize, task: &str) -> Option<&'a Task> {
    app.machines
        .get(mi)?
        .model
        .tasks
        .iter()
        .find(|t| t.id == task)
}

pub fn is_missing(t: &Task) -> bool {
    t.status == "missing"
}

/// Tasks whose PR status is worth showing: a workspace row and a checkout.
fn watched(app: &App) -> Vec<(usize, String)> {
    let mut v = Vec::new();
    for (mi, m) in app.machines.iter().enumerate() {
        if !m.connected() || app.ux.tasks.unsupported.contains(&mi) {
            continue;
        }
        for t in &m.model.tasks {
            if t.workspace.is_some()
                && t.worktree_path.is_some()
                && !matches!(t.status.as_str(), "archived" | "missing" | "finished")
            {
                v.push((mi, t.id.clone()));
            }
        }
    }
    v
}

pub fn tick(app: &mut App, now: Instant) {
    for key in watched(app) {
        let s = &app.ux.tasks;
        if s.inflight.contains(&key) || s.asked.get(&key).is_some_and(|t| now < *t + REFRESH) {
            continue;
        }
        app.ux.tasks.inflight.insert(key.clone());
        app.ux.tasks.asked.insert(key.clone(), now);
        app.command_on(
            key.0,
            "task.get",
            json!({"task": key.1}),
            Pending::Ux(crate::ux::Reply::Tasks(Reply::Get {
                task: key.1.clone(),
            })),
        );
    }
}

pub fn deadlines(app: &App, d: &mut crate::deadline::Deadlines) {
    let s = &app.ux.tasks;
    if let Some(t) = watched(app)
        .iter()
        .filter(|k| !s.inflight.contains(*k))
        .filter_map(|k| s.asked.get(k).map(|t| *t + REFRESH))
        .min()
    {
        d.at("tasks.pr", t);
    }
}

pub fn on_disconnected(app: &mut App, mi: usize) {
    app.ux.tasks.inflight.retain(|(m, _)| *m != mi);
}

pub fn on_reply(app: &mut App, mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    match r {
        Reply::Get { task } => {
            let key = (mi, task);
            app.ux.tasks.inflight.remove(&key);
            match res {
                Ok(v) => match Pr::from_value(&v["pr"]) {
                    Some(p) => {
                        app.ux.tasks.pr.insert(key, p);
                    }
                    None => {
                        app.ux.tasks.pr.remove(&key);
                    }
                },
                Err(e) if e.is_method_not_found() => {
                    app.ux.tasks.unsupported.insert(mi);
                }
                Err(_) => {}
            }
        }
        Reply::Recreate { task } => {
            let name = handle_of(app, mi, &task);
            match res {
                Ok(_) => app.toast(format!("recreating the checkout of {name}")),
                Err(e) if e.is_method_not_found() => {
                    app.toast("this server can't recreate tasks yet (no task.recreate); upgrade it")
                }
                Err(e) => app.toast(format!("✗ recreate {name}: {}", e.message)),
            }
        }
        Reply::Forget { task } => {
            let name = handle_of(app, mi, &task);
            match res {
                Ok(_) => app.toast(format!("forgot {name} (nothing on disk was touched)")),
                Err(e) if e.is_method_not_found() => {
                    app.toast("this server can't forget tasks yet (no task.forget); upgrade it")
                }
                Err(e) => app.toast(format!("✗ forget {name}: {}", e.message)),
            }
        }
    }
    app.dirty = true;
}

fn handle_of(app: &App, mi: usize, task: &str) -> String {
    find(app, mi, task)
        .map(|t| format!("#{}", t.handle))
        .unwrap_or_else(|| task.to_string())
}

/// Badges after a task workspace's name: missing checkout, cached PR status.
pub fn segs(app: &App, mi: usize, task: &str) -> Vec<(String, Style)> {
    let t = &app.theme;
    let mut out = Vec::new();
    let Some(task_rec) = find(app, mi, task) else {
        return out;
    };
    if is_missing(task_rec) {
        out.push((" ⊘ missing".into(), t.bold(t.red)));
        return out;
    }
    if let Some(p) = app.ux.tasks.pr.get(&(mi, task.to_string()))
        && p.kind == "pr"
        && !p.label.is_empty()
    {
        let color = match (p.state.as_str(), p.checks.as_str()) {
            ("merged", _) => t.accent,
            ("closed", _) => t.muted,
            (_, "failing") => t.red,
            (_, "passing") => t.green,
            (_, "pending") => t.yellow,
            _ => t.muted,
        };
        let label = vk_proto::text::escape_controls(&p.label);
        out.push((format!(" {label}"), t.s(color)));
    }
    out
}

// ---- recreate / forget ---------------------------------------------------------------------------

pub fn recreate(app: &mut App, mi: usize, task: &str) {
    app.command_on(
        mi,
        "task.recreate",
        json!({"task": task}),
        Pending::Ux(crate::ux::Reply::Tasks(Reply::Recreate {
            task: task.to_string(),
        })),
    );
}

pub fn forget(app: &mut App, mi: usize, task: &str) {
    app.command_on(
        mi,
        "task.forget",
        json!({"task": task}),
        Pending::Ux(crate::ux::Reply::Tasks(Reply::Forget {
            task: task.to_string(),
        })),
    );
}

fn ask_forget(app: &mut App, mi: usize, t: &Task) {
    app.mode = Mode::Popup(Popup::Confirm {
        message: format!(
            "Forget task #{} {}? Its record goes; nothing on disk is touched.",
            t.handle, t.title
        ),
        action: Box::new(Action::ForgetTask {
            machine: mi,
            task: t.id.clone(),
        }),
    });
}

/// Every missing task: (machine, task).
pub fn missing(app: &App) -> Vec<(usize, Task)> {
    app.machines
        .iter()
        .enumerate()
        .flat_map(|(mi, m)| {
            m.model
                .tasks
                .iter()
                .filter(|t| is_missing(t))
                .map(move |t| (mi, t.clone()))
        })
        .collect()
}

/// Palette entries per missing task: (id, description).
pub fn palette_entries(app: &App) -> Vec<(String, String)> {
    let multi = app.machines.len() > 1;
    let mut out = Vec::new();
    for (mi, t) in missing(app) {
        let on = if multi {
            format!(" ({})", app.machines[mi].label)
        } else {
            String::new()
        };
        out.push((
            format!("task_recreate:{mi}:{}", t.id),
            format!(
                "Recreate missing task #{} {} — new checkout from its branch{on}",
                t.handle, t.title
            ),
        ));
        out.push((
            format!("task_forget:{mi}:{}", t.id),
            format!(
                "Forget missing task #{} {} — nothing on disk is touched{on}",
                t.handle, t.title
            ),
        ));
    }
    out
}

/// The missing task an unqualified `task_recreate` / `task_forget` means: the focused
/// workspace's, else the only one.
fn target(app: &App) -> Option<(usize, Task)> {
    let all = missing(app);
    if let Some(w) = app.focused_ws()
        && let Some(t) = w.task.as_deref()
        && let Some(found) = all.iter().find(|(mi, x)| *mi == app.cur && x.id == t)
    {
        return Some(found.clone());
    }
    (all.len() == 1).then(|| all[0].clone())
}

pub fn action(app: &mut App, action: &str) -> bool {
    let (verb, rest) = match action.split_once(':') {
        Some((v, r)) => (v, Some(r)),
        None => (action, None),
    };
    if !matches!(verb, "task_recreate" | "task_forget") {
        return false;
    }
    let chosen = match rest {
        Some(r) => r.split_once(':').and_then(|(m, id)| {
            let mi: usize = m.parse().ok()?;
            find(app, mi, id).cloned().map(|t| (mi, t))
        }),
        None => target(app),
    };
    let Some((mi, t)) = chosen else {
        if missing(app).is_empty() {
            app.toast("no task is marked missing");
        } else {
            // Several: pick one from the palette.
            crate::nav::open_palette(app, "missing task".into());
        }
        return true;
    };
    if !is_missing(&t) {
        app.toast(format!("#{} isn't missing (status {})", t.handle, t.status));
        return true;
    }
    if verb == "task_recreate" {
        recreate(app, mi, &t.id);
    } else {
        ask_forget(app, mi, &t);
    }
    true
}

/// Navigate mode: `R` recreates, `F` forgets the selected row's task when it is missing.
pub fn navigate_key(app: &mut App, ev: &KeyEvent, sel: usize) -> bool {
    if ev.kind == KeyKind::Release || !matches!(ev.key, Key::Char('R' | 'F')) {
        return false;
    }
    let row = crate::draw::sidebar_targets(app).get(sel).cloned();
    let task = row.and_then(|(mi, pane)| {
        let m = &app.machines[mi];
        let ws = m
            .model
            .panes
            .iter()
            .find(|p| p.id == pane)?
            .workspace
            .clone();
        let tid = m
            .model
            .workspaces
            .iter()
            .find(|w| w.id == ws)?
            .task
            .clone()?;
        find(app, mi, &tid).cloned().map(|t| (mi, t))
    });
    match task {
        Some((mi, t)) if is_missing(&t) => {
            if ev.key == Key::Char('R') {
                recreate(app, mi, &t.id);
                app.mode = Mode::Navigate { sel };
            } else {
                ask_forget(app, mi, &t);
            }
            true
        }
        _ => false,
    }
}

#[cfg(test)]
#[path = "taskbadge_tests.rs"]
mod tests;
