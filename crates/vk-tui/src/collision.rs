//! Shared-checkout collisions in the TUI (05 §10 UX; lane 3A). Collision detection is advisory:
//! these surfaces warn and offer actions; nothing here blocks, reverts or reassigns anything.
//!
//! - **Pane frame badge.** A pane whose run is in an open `high` or `medium` collision shows a
//!   `⚠` in its top-left corner.
//! - **Sidebar.** The workspace shows one line under its agents, the server's headline of its
//!   most severe collision ("claude and codex both edited `src/auth.ts`"), with a count of any
//!   others, and the agent row carries `⚠`. `low` (same directory, or a guessed edit of a file
//!   another run read) is listed in the popup only. A click on the line (or `enter` on it in
//!   navigate mode) opens the popup on that collision; `d` there dismisses it.
//! - **Dismissed** collisions (`collision.dismiss`, "I have seen this") leave the sidebar, the
//!   agent rows and the pane frames until the server shows them again (a new run joined or the
//!   severity rose). The popup still lists them.
//! - **Popup** (`collisions` in the palette or bound): the full headline, paths, runs and a
//!   timeline, with **d** dismiss (or show again),
//!   **p** pause the selected run (the adapter's interrupt), **t** tell the agents (native
//!   steer only: each run shows how it can be reached, and a run with no such channel is told
//!   so, never typed into), **f** start a fresh task from the shared checkout's `HEAD` (asks
//!   for confirmation after showing the hand-off; the original runs keep working), **i** ignore
//!   the selected path, **o** jump to the run's pane, `[` `]` other collisions.
//!
//! The server owns everything; this module reads `collision.list` (when a machine connects,
//! every 30 s and right after a `task.collision_*` event) and `collision.get` for the popup. A
//! server without the methods is left alone.

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::screen::Grid;
use crate::time::{Duration, Instant};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};

/// How often the list is re-read when no event arrived.
pub const REFRESH: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RunInfo {
    pub run: String,
    pub handle: String,
    pub name: String,
    pub harness: String,
    pub pane: String,
    pub state: String,
    pub alive: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PathInfo {
    pub path: String,
    pub severity: String,
    pub reason: String,
    pub ambiguous: bool,
}

/// One open collision as the list reports it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Rec {
    pub id: String,
    pub root: String,
    pub severity: String,
    pub headline: String,
    pub ambiguous: bool,
    pub runs: Vec<RunInfo>,
    pub paths: Vec<PathInfo>,
    pub last_ms: i64,
    /// The user dismissed the warning (`dismissed_ms` is set).
    pub dismissed: bool,
}

fn text(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or_default().to_string()
}

/// `low` 1, `medium` 2, `high` 3.
pub fn rank(sev: &str) -> u8 {
    match sev {
        "high" => 3,
        "medium" => 2,
        "low" => 1,
        _ => 0,
    }
}

impl Rec {
    pub fn from_value(v: &Value) -> Option<Rec> {
        let id = v["id"].as_str()?.to_string();
        let runs = v["run_info"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|r| RunInfo {
                        run: text(r, "run"),
                        handle: text(r, "handle"),
                        name: text(r, "name"),
                        harness: text(r, "harness"),
                        pane: text(r, "pane"),
                        state: text(r, "state"),
                        alive: r["alive"].as_bool().unwrap_or(false),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let paths = v["paths"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|p| PathInfo {
                        path: text(p, "path"),
                        severity: text(p, "severity"),
                        reason: text(&p["reason"], "kind"),
                        ambiguous: p["ambiguous"].as_bool().unwrap_or(false),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(Rec {
            id,
            root: text(v, "root"),
            severity: text(v, "severity"),
            headline: text(v, "headline"),
            ambiguous: v["ambiguous"].as_bool().unwrap_or(false),
            runs,
            paths,
            last_ms: v["last_ms"].as_i64().unwrap_or(0),
            dismissed: v["dismissed_ms"].is_i64(),
        })
    }

    fn has_pane(&self, pane: &str) -> bool {
        self.runs.iter().any(|r| r.alive && r.pane == pane)
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TLine {
    pub at_ms: i64,
    pub who: String,
    pub what: String,
    pub path: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Steer {
    pub run: String,
    pub channel: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Detail {
    pub rec: Rec,
    pub timeline: Vec<TLine>,
    pub steer: Vec<Steer>,
}

/// "Start a fresh task from here" waiting for the user's yes.
#[derive(Debug, Clone, PartialEq)]
pub struct Confirm {
    pub title: String,
    pub base: String,
    pub harness: String,
    pub prompt: String,
    pub run: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct View {
    pub mi: usize,
    pub id: String,
    pub detail: Option<Detail>,
    pub sel: usize,
    pub notice: Option<String>,
    pub confirm: Option<Confirm>,
}

#[derive(Debug, Default)]
pub struct State {
    /// Open collisions per machine.
    pub recs: HashMap<usize, Vec<Rec>>,
    pub asked: HashMap<usize, Instant>,
    pub inflight: HashSet<usize>,
    /// Machines to re-read as soon as possible (an event arrived).
    pub stale: HashSet<usize>,
    /// Machines whose server has no `collision.list`.
    pub unsupported: HashSet<usize>,
    pub view: Option<View>,
}

#[derive(Debug, Clone)]
pub enum Reply {
    List,
    Get { id: String },
    Pause { id: String, run: String },
    Tell { id: String },
    Fresh { id: String, dry: bool },
    Ignore { id: String, path: String },
    Dismiss { on: bool },
}

fn send(app: &mut App, mi: usize, method: &str, params: Value, r: Reply) {
    app.command_on(
        mi,
        method,
        params,
        Pending::Ux(crate::ux::Reply::Collision(r)),
    );
}

// ---- reading the list -----------------------------------------------------------------------

pub fn tick(app: &mut App, now: Instant) {
    for mi in 0..app.machines.len() {
        if !app.machines[mi].connected()
            || !app.machines[mi].capabilities().host
            || app.ux.collision.unsupported.contains(&mi)
            || app.ux.collision.inflight.contains(&mi)
        {
            continue;
        }
        let s = &app.ux.collision;
        let due = s.stale.contains(&mi) || s.asked.get(&mi).is_none_or(|t| now >= *t + REFRESH);
        if !due {
            continue;
        }
        app.ux.collision.inflight.insert(mi);
        app.ux.collision.stale.remove(&mi);
        app.ux.collision.asked.insert(mi, now);
        send(
            app,
            mi,
            "collision.list",
            json!({"status": "open"}),
            Reply::List,
        );
    }
}

pub fn deadlines(app: &App, d: &mut crate::deadline::Deadlines) {
    let s = &app.ux.collision;
    if let Some(t) = (0..app.machines.len())
        .filter(|mi| {
            app.machines[*mi].connected()
                && app.machines[*mi].capabilities().host
                && !s.unsupported.contains(mi)
        })
        .filter(|mi| !s.inflight.contains(mi))
        .filter_map(|mi| s.asked.get(&mi).map(|t| *t + REFRESH))
        .min()
    {
        d.at("collision.list", t);
    }
}

pub fn on_disconnected(app: &mut App, mi: usize) {
    app.ux.collision.inflight.remove(&mi);
    app.ux.collision.recs.remove(&mi);
    app.ux.collision.asked.remove(&mi);
}

/// A pushed `task.collision_*` event: read the list again at once.
pub fn on_event(app: &mut App, mi: usize, kind: &str) {
    if kind.starts_with("task.collision_") {
        app.ux.collision.stale.insert(mi);
        app.dirty = true;
    }
}

// ---- what the rest of the UI shows ----------------------------------------------------------

fn open_recs(app: &App, mi: usize) -> &[Rec] {
    app.ux
        .collision
        .recs
        .get(&mi)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Open collisions the user has not dismissed: what the sidebar and the frames warn about.
fn shown_recs(app: &App, mi: usize) -> impl Iterator<Item = &Rec> {
    open_recs(app, mi).iter().filter(|r| !r.dismissed)
}

/// The highest severity rank of the open collisions the pane's run is in.
pub fn pane_rank(app: &App, mi: usize, pane: &str) -> u8 {
    shown_recs(app, mi)
        .filter(|r| r.has_pane(pane))
        .map(|r| rank(&r.severity))
        .max()
        .unwrap_or(0)
}

/// The pane-frame badge: `⚠` for high and medium (low is a sidebar hint only).
pub fn pane_badge(app: &App, mi: usize, pane: &str) -> Option<&'static str> {
    (pane_rank(app, mi, pane) >= 2).then_some("⚠")
}

/// The marker on an agent row: `⚠` for high and medium (low is listed in the popup only).
pub fn agent_marker(app: &App, mi: usize, pane: &str) -> Option<(&'static str, u8)> {
    match pane_rank(app, mi, pane) {
        0 | 1 => None,
        n => Some(("⚠", n)),
    }
}

/// One sidebar line of a workspace.
pub struct SideLine {
    pub text: String,
    pub rank: u8,
    /// The collision it names (what a click opens).
    pub id: String,
}

/// The workspace's sidebar line, if any: the headline of its most severe (then most recent)
/// `high` or `medium` collision, "· +N more" when it has others, with the severity rank.
pub fn sidebar_lines(app: &App, mi: usize, ws_panes: &[&str]) -> Vec<SideLine> {
    let mut v: Vec<&Rec> = shown_recs(app, mi)
        .filter(|r| rank(&r.severity) >= 2)
        .filter(|r| ws_panes.iter().any(|p| r.has_pane(p)))
        .collect();
    v.sort_by(|a, b| {
        rank(&b.severity)
            .cmp(&rank(&a.severity))
            .then(b.last_ms.cmp(&a.last_ms))
    });
    let Some(top) = v.first() else {
        return vec![];
    };
    let text = match v.len() - 1 {
        0 => top.headline.clone(),
        n => format!("{} · +{n} more", top.headline),
    };
    vec![SideLine {
        text,
        rank: rank(&top.severity),
        id: top.id.clone(),
    }]
}

/// A left click on a sidebar collision line opens the popup on it. True when consumed.
pub fn sidebar_click(app: &mut App, y: u16) -> bool {
    let hit = y
        .checked_sub(1)
        .and_then(|i| crate::draw::sidebar_rows(app).into_iter().nth(i as usize))
        .and_then(|r| r.collision);
    match hit {
        Some(t) => {
            open(app, Some(t));
            true
        }
        None => false,
    }
}

/// Navigate-mode keys on a collision line: `enter` (or space) opens the popup, `d` dismisses.
/// Pane actions never reach it; moving, `/` and leaving still work.
pub fn navigate_key(app: &mut App, ev: &KeyEvent, sel: usize) -> bool {
    if ev.kind == KeyKind::Release || app.ux.nav.typing {
        return false;
    }
    let line = |app: &App, i: usize| {
        crate::draw::sidebar_rows(app)
            .into_iter()
            .filter(|r| r.selectable())
            .nth(i)
            .and_then(|r| r.collision)
    };
    // A digit that jumps to a collision line opens it.
    if let Key::Char(c) = ev.key
        && let Some(d) = c.to_digit(10)
    {
        let Some(t) = line(app, (d as usize).saturating_sub(1)) else {
            return false;
        };
        open(app, Some(t));
        return true;
    }
    let Some((mi, id)) = line(app, sel) else {
        return false;
    };
    match ev.key {
        Key::Named(NamedKey::Escape | NamedKey::Down | NamedKey::Up)
        | Key::Char('j' | 'k' | 'q' | '/') => return false,
        Key::Named(NamedKey::Enter) | Key::Char(' ') => open(app, Some((mi, id))),
        Key::Char('d') => {
            dismiss(app, mi, &id, true);
            app.mode = Mode::Navigate { sel };
        }
        _ => app.mode = Mode::Navigate { sel },
    }
    true
}

/// Dismiss (`on`) or show again a collision. The sidebar follows at once; the server's reply
/// (or the next list) settles it.
fn dismiss(app: &mut App, mi: usize, id: &str, on: bool) {
    if let Some(r) = app
        .ux
        .collision
        .recs
        .get_mut(&mi)
        .and_then(|v| v.iter_mut().find(|r| r.id == id))
    {
        r.dismissed = on;
    }
    if let Some(d) = app
        .ux
        .collision
        .view
        .as_mut()
        .filter(|v| v.mi == mi && v.id == id)
        .and_then(|v| v.detail.as_mut())
    {
        d.rec.dismissed = on;
    }
    send(
        app,
        mi,
        "collision.dismiss",
        json!({"collision": id, "dismissed": on}),
        Reply::Dismiss { on },
    );
    app.dirty = true;
}

// ---- popup ----------------------------------------------------------------------------------

pub fn action(app: &mut App, action: &str) -> bool {
    if action == "collisions" {
        open(app, None);
        return true;
    }
    false
}

/// All open collisions as (machine, id), the most severe and recent first.
fn all(app: &App) -> Vec<(usize, String)> {
    let mut v: Vec<(u8, i64, usize, String)> = Vec::new();
    for (mi, recs) in &app.ux.collision.recs {
        for r in recs {
            v.push((rank(&r.severity), r.last_ms, *mi, r.id.clone()));
        }
    }
    v.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    v.into_iter().map(|x| (x.2, x.3)).collect()
}

pub fn open(app: &mut App, seed: Option<(usize, String)>) {
    let target = seed.or_else(|| {
        // The focused pane's own collision first.
        let focus = app.focused_pane();
        focus
            .and_then(|p| {
                open_recs(app, app.cur)
                    .iter()
                    .filter(|r| r.has_pane(&p))
                    .max_by_key(|r| rank(&r.severity))
                    .map(|r| (app.cur, r.id.clone()))
            })
            .or_else(|| all(app).into_iter().next())
    });
    let Some((mi, id)) = target else {
        app.toast("no collisions: no two agents are editing the same files");
        return;
    };
    app.ux.collision.view = Some(View {
        mi,
        id: id.clone(),
        ..Default::default()
    });
    send(
        app,
        mi,
        "collision.get",
        json!({"collision": id}),
        Reply::Get { id },
    );
    app.mode = Mode::Popup(Popup::Collision);
}

fn parse_detail(v: &Value) -> Option<Detail> {
    let rec = Rec::from_value(&v["collision"])?;
    let timeline = v["collision"]["timeline"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|t| TLine {
                    at_ms: t["at_ms"].as_i64().unwrap_or(0),
                    who: match t["run"].as_str() {
                        Some(r) => r.to_string(),
                        None => format!(
                            "one of {}",
                            t["candidates"]
                                .as_array()
                                .map(|c| {
                                    c.iter()
                                        .filter_map(Value::as_str)
                                        .collect::<Vec<_>>()
                                        .join("/")
                                })
                                .unwrap_or_default()
                        ),
                    },
                    what: text(t, "what"),
                    path: text(t, "path"),
                    source: text(t, "source"),
                })
                .collect()
        })
        .unwrap_or_default();
    let steer = v["steer"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|s| Steer {
                    run: text(s, "run"),
                    channel: s["channel"].as_str().map(str::to_string),
                    reason: s["reason"].as_str().map(str::to_string),
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Detail {
        rec,
        timeline,
        steer,
    })
}

/// Selectable rows: the runs, then the paths.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Row {
    Run(usize),
    Path(usize),
}

fn rows(d: &Detail) -> Vec<Row> {
    (0..d.rec.runs.len())
        .map(Row::Run)
        .chain((0..d.rec.paths.len()).map(Row::Path))
        .collect()
}

fn run_label(r: &RunInfo) -> String {
    let who = if r.name.is_empty() {
        r.harness.clone()
    } else {
        r.name.clone()
    };
    if r.handle.is_empty() {
        who
    } else {
        format!("{who} ({})", r.handle)
    }
}

fn now_ms() -> i64 {
    crate::time::SystemTime::now()
        .duration_since(crate::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn ago(ms: i64) -> String {
    let s = (now_ms() - ms).max(0) / 1000;
    match s {
        0..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        _ => format!("{}h ago", s / 3600),
    }
}

pub fn on_reply(app: &mut App, mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    app.dirty = true;
    match r {
        Reply::List => {
            app.ux.collision.inflight.remove(&mi);
            match res {
                Ok(v) => {
                    let recs: Vec<Rec> = v["collisions"]
                        .as_array()
                        .map(|a| a.iter().filter_map(Rec::from_value).collect())
                        .unwrap_or_default();
                    app.ux.collision.recs.insert(mi, recs);
                }
                Err(e) if e.kind == "method_not_found" => {
                    app.ux.collision.unsupported.insert(mi);
                }
                Err(_) => {}
            }
        }
        Reply::Get { id } => {
            let Some(v) = app
                .ux
                .collision
                .view
                .as_mut()
                .filter(|v| v.id == id && v.mi == mi)
            else {
                return;
            };
            match res {
                Ok(j) => match parse_detail(&j) {
                    Some(d) => {
                        v.sel = v.sel.min(rows(&d).len().saturating_sub(1));
                        v.detail = Some(d);
                    }
                    None => v.notice = Some("unreadable collision".into()),
                },
                Err(e) => {
                    v.notice = Some(format!("✗ {}", e.message));
                    v.detail = None;
                }
            }
        }
        Reply::Pause { id, run } => {
            let msg = match res {
                Ok(_) => format!("✓ interrupted {run}"),
                Err(e) => format!("✗ pause: {}", e.message),
            };
            note(app, &id, msg);
            app.ux.collision.stale.insert(mi);
        }
        Reply::Tell { id } => {
            let msg = match res {
                Ok(v) => {
                    let parts: Vec<String> = v["results"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .map(|x| {
                                    format!(
                                        "{} {}{}",
                                        x["run"].as_str().unwrap_or(""),
                                        x["status"].as_str().unwrap_or(""),
                                        x["reason"]
                                            .as_str()
                                            .map(|r| format!(" ({r})"))
                                            .unwrap_or_default()
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    format!("told: {}", parts.join(" · "))
                }
                Err(e) => format!("✗ tell: {}", e.message),
            };
            note(app, &id, msg);
        }
        Reply::Fresh { id, dry } => match res {
            Ok(v) if dry => {
                if let Some(view) = app.ux.collision.view.as_mut().filter(|x| x.id == id) {
                    view.confirm = Some(Confirm {
                        title: text(&v, "title"),
                        base: text(&v, "base"),
                        harness: text(&v, "harness"),
                        prompt: text(&v, "prompt"),
                        run: v["source_run"].as_str().map(str::to_string),
                    });
                }
            }
            Ok(v) => {
                let handle = v["result"]["task"]["handle"].as_str().unwrap_or("?");
                let msg = format!(
                    "✓ task #{handle} created from the shared HEAD; the other agents keep working"
                );
                note(app, &id, msg.clone());
                app.toast(msg);
                app.ux.collision.stale.insert(mi);
            }
            Err(e) => note(app, &id, format!("✗ new task: {}", e.message)),
        },
        Reply::Dismiss { on } => {
            match res {
                Ok(_) if on => app.toast(
                    "dismissed: it comes back if another agent joins or it gets more serious",
                ),
                Ok(_) => app.toast("the collision warning shows again"),
                Err(e) => app.toast(format!("✗ dismiss: {}", e.message)),
            }
            app.ux.collision.stale.insert(mi);
        }
        Reply::Ignore { id, path } => {
            let msg = match res {
                Ok(_) => format!("✓ ignoring {path}"),
                Err(e) => format!("✗ ignore: {}", e.message),
            };
            note(app, &id, msg);
            app.ux.collision.stale.insert(mi);
            // The record may have closed; reload it.
            send(
                app,
                mi,
                "collision.get",
                json!({"collision": id}),
                Reply::Get { id: id.clone() },
            );
        }
    }
}

fn note(app: &mut App, id: &str, msg: String) {
    if let Some(v) = app.ux.collision.view.as_mut().filter(|v| v.id == id) {
        v.notice = Some(msg);
    }
}

pub fn key(app: &mut App, ev: KeyEvent) {
    let keep = |app: &mut App| app.mode = Mode::Popup(Popup::Collision);
    if ev.kind == KeyKind::Release {
        return keep(app);
    }
    let Some(mut v) = app.ux.collision.view.take() else {
        return;
    };
    // The confirmation of "start a fresh task".
    if let Some(c) = v.confirm.take() {
        if matches!(ev.key, Key::Char('y') | Key::Named(NamedKey::Enter)) {
            let mut p = json!({"collision": v.id, "title": c.title, "harness": c.harness, "prompt": c.prompt});
            if let Some(r) = &c.run {
                p["run"] = json!(r);
            }
            v.notice = Some("creating the task…".into());
            send(
                app,
                v.mi,
                "collision.start_task",
                p,
                Reply::Fresh {
                    id: v.id.clone(),
                    dry: false,
                },
            );
        } else {
            v.notice = Some("not created".into());
        }
        app.ux.collision.view = Some(v);
        return keep(app);
    }
    let rs = v.detail.as_ref().map(rows).unwrap_or_default();
    let n = rs.len();
    let list = all(app);
    match ev.key {
        Key::Named(NamedKey::Escape) | Key::Char('q') => return,
        Key::Char('j') | Key::Named(NamedKey::Down) => v.sel = (v.sel + 1).min(n.saturating_sub(1)),
        Key::Char('k') | Key::Named(NamedKey::Up) => v.sel = v.sel.saturating_sub(1),
        Key::Char(c @ ('[' | ']')) => {
            if let Some(i) = list.iter().position(|x| x.0 == v.mi && x.1 == v.id) {
                let next = if c == ']' {
                    (i + 1) % list.len()
                } else {
                    (i + list.len() - 1) % list.len()
                };
                let (mi, id) = list[next].clone();
                v = View {
                    mi,
                    id: id.clone(),
                    ..Default::default()
                };
                send(
                    app,
                    mi,
                    "collision.get",
                    json!({"collision": id}),
                    Reply::Get { id },
                );
            }
        }
        Key::Char('r') => {
            send(
                app,
                v.mi,
                "collision.get",
                json!({"collision": v.id}),
                Reply::Get { id: v.id.clone() },
            );
        }
        Key::Char('o') | Key::Named(NamedKey::Enter) => {
            if let (Some(d), Some(Row::Run(i))) = (&v.detail, rs.get(v.sel))
                && let Some(r) = d.rec.runs.get(*i)
                && r.alive
                && !r.pane.is_empty()
            {
                let (mi, pane) = (v.mi, r.pane.clone());
                app.focus_pane(mi, &pane);
                return;
            }
        }
        Key::Char('p') => match (&v.detail, rs.get(v.sel)) {
            (Some(d), Some(Row::Run(i))) => {
                let run = d.rec.runs[*i].run.clone();
                v.notice = Some(format!("interrupting {run}…"));
                send(
                    app,
                    v.mi,
                    "collision.pause",
                    json!({"collision": v.id, "run": run}),
                    Reply::Pause {
                        id: v.id.clone(),
                        run,
                    },
                );
            }
            _ => v.notice = Some("select an agent to pause".into()),
        },
        Key::Char('t') => {
            v.notice = Some("telling the agents…".into());
            send(
                app,
                v.mi,
                "collision.tell",
                json!({"collision": v.id}),
                Reply::Tell { id: v.id.clone() },
            );
        }
        Key::Char('f') => {
            v.notice = Some("preparing the hand-off…".into());
            send(
                app,
                v.mi,
                "collision.start_task",
                json!({"collision": v.id, "dry_run": true}),
                Reply::Fresh {
                    id: v.id.clone(),
                    dry: true,
                },
            );
        }
        // Dismissing closes the popup: the warning is dealt with. Showing it again stays.
        Key::Char('d') => {
            let on = !v.detail.as_ref().is_some_and(|d| d.rec.dismissed);
            let (mi, id) = (v.mi, v.id.clone());
            app.ux.collision.view = Some(v);
            dismiss(app, mi, &id, on);
            if on {
                return;
            }
            return keep(app);
        }
        Key::Char('i') => match (&v.detail, rs.get(v.sel)) {
            (Some(d), Some(Row::Path(i))) => {
                let path = d.rec.paths[*i].path.clone();
                send(
                    app,
                    v.mi,
                    "collision.ignore",
                    json!({"collision": v.id, "path": path}),
                    Reply::Ignore {
                        id: v.id.clone(),
                        path,
                    },
                );
            }
            _ => v.notice = Some("select a path to ignore".into()),
        },
        _ => {}
    }
    app.ux.collision.view = Some(v);
    keep(app);
}

fn sev_style(app: &App, sev: &str) -> vk_proto::render::Style {
    let t = app.theme;
    match sev {
        "high" => t.bold(t.red),
        "medium" => t.bold(t.yellow),
        _ => t.dim(),
    }
}

pub fn draw(app: &App, g: &mut Grid) {
    let Some(v) = &app.ux.collision.view else {
        return;
    };
    let t = app.theme;
    let mut a = crate::drafts::Area::open(app, g, "collision");
    let Some(d) = &v.detail else {
        a.line(v.notice.as_deref().unwrap_or("loading…"), t.dim());
        a.footer("esc back", t.dim());
        return;
    };
    let rs = rows(d);
    // The whole headline, wrapped (the sidebar line is often cut off), in at most three lines
    // so the agents and paths below stay on screen.
    let width = a.r.w.saturating_sub(2) as usize;
    for l in crate::sidebar_tip::wrap_capped(&d.rec.headline, width, 3) {
        a.line(&l, sev_style(app, &d.rec.severity));
    }
    if d.rec.dismissed {
        a.line(
            "dismissed: hidden until another agent joins or it gets more serious · d shows it again",
            t.dim(),
        );
    }
    a.line(
        &format!(
            "{} · {}{}",
            d.rec.severity.to_uppercase(),
            d.rec.root,
            if d.rec.ambiguous {
                " · attribution is ambiguous: these agents possibly did this"
            } else {
                ""
            }
        ),
        sev_style(app, &d.rec.severity),
    );
    a.line(
        "advisory: Vibeke warns; it never blocks, reverts or reassigns changes",
        t.dim(),
    );
    a.line("", t.text());
    a.line("agents", t.bold(t.fg));
    for (i, r) in d.rec.runs.iter().enumerate() {
        let sel = rs.get(v.sel) == Some(&Row::Run(i));
        let steer = d
            .steer
            .iter()
            .find(|s| s.run == r.run)
            .map(|s| match (&s.channel, &s.reason) {
                (Some(c), _) => format!("tell: {c}"),
                (None, Some(why)) => format!("tell: no ({why})"),
                _ => "tell: no".into(),
            })
            .unwrap_or_default();
        let state = if r.alive { r.state.as_str() } else { "ended" };
        let line = format!(
            " {} {} · {state} · {steer}",
            if sel { "▸" } else { " " },
            run_label(r)
        );
        a.line(&line, if sel { t.sel(t.accent) } else { t.text() });
    }
    a.line("", t.text());
    a.line("paths", t.bold(t.fg));
    for (i, p) in d.rec.paths.iter().enumerate() {
        let sel = rs.get(v.sel) == Some(&Row::Path(i));
        let line = format!(
            " {} {:<6} {}  ({}{})",
            if sel { "▸" } else { " " },
            p.severity,
            p.path,
            p.reason.replace('_', " "),
            if p.ambiguous { ", ambiguous" } else { "" }
        );
        a.line(
            &line,
            if sel {
                t.sel(t.accent)
            } else {
                sev_style(app, &p.severity)
            },
        );
    }
    a.line("", t.text());
    a.line("timeline", t.bold(t.fg));
    let skip = d.timeline.len().saturating_sub(10);
    for e in d.timeline.iter().skip(skip) {
        a.line(
            &format!(
                "  {:>8}  {} {} {}  [{}]",
                ago(e.at_ms),
                e.who,
                e.what,
                e.path,
                e.source
            ),
            t.dim(),
        );
    }
    if let Some(c) = &v.confirm {
        a.line("", t.text());
        a.line(
            &format!(
                "start a fresh task \"{}\" from the shared HEAD ({}) with a new {} run?",
                c.title,
                &c.base[..c.base.len().min(12)],
                c.harness
            ),
            t.bold(t.yellow),
        );
        for l in c.prompt.lines().take(6) {
            a.line(&format!("  │ {l}"), t.dim());
        }
        a.line(
            "  the agents here keep working; nothing is moved.  [y] create · any other key cancels",
            t.text(),
        );
    }
    if let Some(n) = &v.notice {
        a.line("", t.text());
        a.line(n, t.bold(t.yellow));
    }
    a.footer(
        "j/k move · d dismiss · p pause · t tell · f fresh task · i ignore path · o open pane · [ ] other · esc back",
        t.dim(),
    );
}

#[cfg(test)]
#[path = "collision_tests.rs"]
mod tests;
