//! Scrollback search beyond the loaded rows (08 §13 copy mode, D#563; 07 §2.14):
//!
//! * **Copy-mode fallback.** When `/`, `?` or `n` finds nothing in the rows copy mode has
//!   loaded, it asks `search.query {q, pane}` (live scrollback + FTS archive), picks the nearest
//!   older hit and loads rows up to it with `pane.read {source: archive}` (pages of 5000, at
//!   most 200 000 rows), then puts the cursor on the match.
//! * **Archive paging.** Copy mode pages in-memory rows with `FetchHistory`; past the start of
//!   memory (`pane.read … mem_first`) it pages the archive with `pane.read {to, lines}` and stops
//!   at the archive's first row instead of paging blank rows.
//! * **Global search** (`search_global`, `prefix+alt+/`): a popup over `search.query` on every
//!   connected machine; `enter` searches, `enter` again on a hit focuses the pane and opens copy
//!   mode on the hit's absolute line.
//!
//! Absolute line numbers are shared by `FetchHistory` (its index space), `pane.read` and
//! `search.query`, so a hit maps to a copy-mode row as `line - top_abs`.

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::copy::CopyMode;
use crate::parity::Reply as PReply;
use crate::screen::{Grid, Rect as SRect};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};
use vk_proto::render::{CursorShape, Row, Span, Style};

/// Rows per archive page while scrolling.
pub const PAGE: u64 = 2000;
/// Rows per request while loading toward a search hit (`pane.read` caps at 5000).
pub const GOAL_PAGE: u64 = 5000;
/// Most rows copy mode loads to reach a hit.
pub const MAX_LOAD: u64 = 200_000;

/// Archive state of one copy-mode session (lives in `CopyMode::archive`).
#[derive(Debug, Clone, Default)]
pub struct ArchiveCursor {
    /// The first `FetchHistory` reply arrived (`total_hist` is known).
    pub history_seen: bool,
    /// The first history page was requested (never twice, even if it came back empty).
    pub first_page_asked: bool,
    /// Rows were prepended from `pane.read`; `top_abs` is authoritative and `FetchHistory`
    /// paging is off.
    pub active: bool,
    /// Absolute line of the first loaded row while `active`.
    pub top_abs: u64,
    /// Rows prepended from `pane.read`.
    pub rows: usize,
    /// First in-memory line (`pane.read` → `mem_first`), when the server reports it.
    pub mem_first: Option<u64>,
    /// Oldest line still on disk (`pane.read` → `first`).
    pub first: Option<u64>,
    /// No older rows exist.
    pub exhausted: bool,
    /// Request generation of an outstanding `pane.read` page.
    pub inflight: Option<u64>,
    /// Request generation of an outstanding `search.query`.
    pub query: Option<u64>,
    /// Where to put the cursor once loaded (a search hit).
    pub goal: Option<Goal>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Goal {
    /// Absolute line.
    pub line: u64,
    /// Text to place the cursor on within the line.
    pub q: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Reply {
    /// `pane.read {lines: 0}`: archive bounds for copy mode on `pane`.
    Info { pane: String },
    /// `pane.read` page for copy mode.
    Page { pane: String, seq: u64 },
    /// `search.query` for copy mode's `/` fallback.
    Query {
        pane: String,
        q: String,
        back: bool,
        seq: u64,
    },
    /// `search.query` for the global popup.
    Global { seq: u64 },
}

#[derive(Default)]
pub struct State {
    pub seq: u64,
    /// After a global-search jump: open copy mode on this hit once the pane is focused and
    /// drawn.
    pub jump: Option<Jump>,
}

#[derive(Debug, Clone)]
pub struct Jump {
    pub mi: usize,
    pub pane: String,
    pub goal: Goal,
    pub at: Instant,
}

/// One global-search hit.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub mi: usize,
    pub pane: String,
    pub handle: String,
    pub title: String,
    pub line: u64,
    pub text: String,
    pub source: String,
    pub live: bool,
}

#[derive(Debug, Clone, Default)]
pub struct GlobalSearch {
    pub input: String,
    /// The query the hits are for.
    pub searched: Option<String>,
    pub hits: Vec<Hit>,
    pub sel: usize,
    /// Machines still to answer.
    pub waiting: usize,
    pub seq: u64,
    pub errors: Vec<String>,
}

fn next_seq(app: &mut App) -> u64 {
    app.parity.search.seq += 1;
    app.parity.search.seq
}

fn plain_row(text: &str, wrapped: bool) -> Row {
    Row {
        spans: vec![Span {
            style: Style::default(),
            text: text.to_string(),
            cols: UnicodeWidthStr::width(text) as u16,
        }],
        wrapped,
    }
}

/// `pane.read` rows (`{n, text, wrapped}`) → rows, with the first row's absolute line.
pub fn rows_of(v: &Value) -> (Option<u64>, Vec<Row>) {
    let arr = v.get("rows").and_then(Value::as_array);
    let first = arr
        .and_then(|a| a.first())
        .and_then(|r| r.get("n"))
        .and_then(Value::as_u64);
    let rows = arr
        .map(|a| {
            a.iter()
                .map(|r| {
                    plain_row(
                        r.get("text").and_then(Value::as_str).unwrap_or(""),
                        r.get("wrapped").and_then(Value::as_bool).unwrap_or(false),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    (first, rows)
}

/// `pane.read {lines: 0}`: the archive's first line and where memory starts, for copy mode on
/// `pane` (sent before its first `FetchHistory`).
pub fn request_bounds(app: &mut App, pane: &str) {
    let pane = pane.to_string();
    app.command(
        "pane.read",
        json!({"pane": pane, "source": "archive", "lines": 0}),
        Pending::Parity(PReply::Search(Reply::Info { pane })),
    );
}

/// What copy mode needs next after a reply or step.
enum Next {
    None,
    Page { to: u64, lines: u64 },
}

/// Move toward the goal: jump when it's loaded, else ask for the next page.
fn goal_step(cm: &mut CopyMode) -> Next {
    let Some(goal) = cm.archive.goal.clone() else {
        return Next::None;
    };
    if cm.pending_req.is_some() || cm.archive.inflight.is_some() || !cm.archive.history_seen {
        return Next::None;
    }
    let top = cm.top_abs();
    if goal.line >= top {
        let idx = (goal.line - top) as usize;
        cm.archive.goal = None;
        if idx < cm.len() {
            let col = goal
                .q
                .as_deref()
                .and_then(|q| {
                    let hay = cm.row_text(idx).to_lowercase();
                    hay.find(&q.to_lowercase())
                        .map(|b| hay[..b].chars().count() as u16)
                })
                .unwrap_or(0);
            cm.jump(idx, col);
            cm.set_message(format!("line {} (history)", goal.line));
        } else {
            cm.set_message("that line is no longer on screen");
        }
        return Next::None;
    }
    if cm.archive.exhausted || goal.line < cm.archive.first.unwrap_or(0) {
        cm.archive.goal = None;
        cm.set_message(format!("line {} is no longer in the archive", goal.line));
        return Next::None;
    }
    if top - goal.line > MAX_LOAD || cm.archive.rows as u64 >= MAX_LOAD {
        cm.archive.goal = None;
        cm.set_message(format!(
            "match is {} lines back — too far to load here (prefix+alt+/ or `vibeke search`)",
            top - goal.line
        ));
        return Next::None;
    }
    cm.set_message(format!(
        "loading history… ({} lines to go)",
        top - goal.line
    ));
    Next::Page {
        to: top,
        lines: (top - goal.line + 3).min(GOAL_PAGE),
    }
}

fn send_page(app: &mut App, pane: String, to: u64, lines: u64) {
    let seq = next_seq(app);
    if let Mode::Copy(cm) = &mut app.mode {
        cm.archive.inflight = Some(seq);
    }
    app.command(
        "pane.read",
        json!({"pane": pane, "source": "archive", "to": to, "lines": lines}),
        Pending::Parity(PReply::Search(Reply::Page { pane, seq })),
    );
}

/// Run `goal_step` on the current copy mode and send what it asks for.
fn advance(app: &mut App) {
    let next = match &mut app.mode {
        Mode::Copy(cm) => (goal_step(cm), cm.pane.clone()),
        _ => return,
    };
    if let (Next::Page { to, lines }, pane) = next {
        send_page(app, pane, to, lines);
    }
}

/// `Outcome::Archive`: the next older page.
pub fn copy_page(app: &mut App, mut cm: Box<CopyMode>) {
    let top = cm.top_abs();
    let pane = cm.pane.clone();
    cm.set_message("loading older history…");
    app.mode = Mode::Copy(cm);
    send_page(app, pane, top, PAGE);
}

/// `Outcome::Search`: nothing in the loaded rows; ask the server about the whole history.
pub fn copy_search(app: &mut App, mut cm: Box<CopyMode>, q: String, back: bool) {
    let seq = next_seq(app);
    let pane = cm.pane.clone();
    cm.archive.query = Some(seq);
    cm.set_message(format!("searching all history for {q}…"));
    app.mode = Mode::Copy(cm);
    app.command(
        "search.query",
        json!({"q": q, "pane": pane, "limit": 100, "context": 0}),
        Pending::Parity(PReply::Search(Reply::Query { pane, q, back, seq })),
    );
}

/// Pick the hit to go to: the newest one older than what's loaded (the nearest step back);
/// failing that, the newest (or oldest for `?`) of all.
pub fn pick_hit(lines: &[u64], top_abs: u64, back: bool) -> Option<u64> {
    lines
        .iter()
        .copied()
        .filter(|l| *l < top_abs)
        .max()
        .or_else(|| {
            if back {
                lines.iter().copied().min()
            } else {
                lines.iter().copied().max()
            }
        })
}

fn copy_of<'a>(app: &'a mut App, pane: &str) -> Option<&'a mut Box<CopyMode>> {
    match &mut app.mode {
        Mode::Copy(cm) if cm.pane == pane => Some(cm),
        _ => None,
    }
}

pub fn on_reply(app: &mut App, mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    match r {
        Reply::Info { pane } => {
            let Some(cm) = copy_of(app, &pane) else {
                return;
            };
            if let Ok(v) = res {
                cm.archive.mem_first = v.get("mem_first").and_then(Value::as_u64);
                cm.archive.first = v.get("first").and_then(Value::as_u64);
            }
        }
        Reply::Page { pane, seq } => {
            let Some(cm) = copy_of(app, &pane) else {
                return;
            };
            if cm.archive.inflight != Some(seq) {
                return;
            }
            cm.archive.inflight = None;
            match res {
                Ok(v) => {
                    if let Some(f) = v.get("first").and_then(Value::as_u64) {
                        cm.archive.first = Some(f);
                    }
                    let (from, rows) = rows_of(&v);
                    let more = v.get("more_before").and_then(Value::as_bool) == Some(true);
                    match from {
                        Some(from) if !rows.is_empty() && from < cm.top_abs() => {
                            // Only rows older than what's loaded (the screen may have scrolled).
                            let keep = (cm.top_abs() - from) as usize;
                            let rows: Vec<Row> = rows.into_iter().take(keep).collect();
                            cm.prepend_archive(from, rows);
                            cm.archive.exhausted = !more;
                        }
                        _ => cm.archive.exhausted = true,
                    }
                    if cm.archive.goal.is_none() {
                        cm.set_message(if cm.archive.exhausted {
                            "start of history"
                        } else {
                            ""
                        });
                    }
                }
                Err(e) => {
                    cm.archive.exhausted = true;
                    cm.archive.goal = None;
                    cm.set_message(format!("archive: {}", e.message));
                }
            }
            advance(app);
        }
        Reply::Query { pane, q, back, seq } => {
            let Some(cm) = copy_of(app, &pane) else {
                return;
            };
            if cm.archive.query != Some(seq) {
                return;
            }
            cm.archive.query = None;
            let lines: Vec<u64> = match &res {
                Ok(v) => v
                    .get("hits")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter(|h| {
                                h.get("pane")
                                    .and_then(Value::as_str)
                                    .is_none_or(|p| p == pane)
                            })
                            .filter_map(|h| h.get("line").and_then(Value::as_u64))
                            .collect()
                    })
                    .unwrap_or_default(),
                Err(_) => vec![],
            };
            match pick_hit(&lines, cm.top_abs(), back) {
                Some(line) => {
                    cm.archive.goal = Some(Goal { line, q: Some(q) });
                    advance(app);
                }
                None => {
                    let why = match res {
                        Err(e) if !e.is_method_not_found() => format!(" ({})", e.message),
                        _ => String::new(),
                    };
                    cm.set_message(format!("not found: {q} (whole history){why}"));
                }
            }
        }
        Reply::Global { seq } => {
            let label = app.machines[mi].label.clone();
            let Mode::Popup(Popup::Search(s)) = &mut app.mode else {
                return;
            };
            if s.seq != seq {
                return;
            }
            s.waiting = s.waiting.saturating_sub(1);
            match res {
                Ok(v) => s.hits.extend(parse_hits(mi, &v)),
                Err(e) => s.errors.push(format!("{label}: {}", e.message)),
            }
        }
    }
}

pub fn parse_hits(mi: usize, v: &Value) -> Vec<Hit> {
    v.get("hits")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|h| {
                    let s = |k: &str| h.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                    Hit {
                        mi,
                        pane: s("pane"),
                        handle: s("pane_handle"),
                        title: s("title"),
                        line: h.get("line").and_then(Value::as_u64).unwrap_or(0),
                        text: s("text"),
                        source: s("source"),
                        live: h.get("live").and_then(Value::as_bool).unwrap_or(false),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// History arrived for copy mode: a pending goal (global-search jump) can proceed.
pub fn after_history(app: &mut App) {
    advance(app);
}

pub fn action(app: &mut App, action: &str) -> bool {
    if action != "search_global" {
        return false;
    }
    app.mode = Mode::Popup(Popup::Search(Box::default()));
    true
}

fn run_global(app: &mut App, mut s: Box<GlobalSearch>) {
    let q = s.input.trim().to_string();
    if q.is_empty() {
        app.mode = Mode::Popup(Popup::Search(s));
        return;
    }
    let seq = next_seq(app);
    s.seq = seq;
    s.hits.clear();
    s.errors.clear();
    s.sel = 0;
    s.searched = Some(q.clone());
    let machines: Vec<usize> = (0..app.machines.len())
        .filter(|i| app.machines[*i].connected())
        .collect();
    s.waiting = machines.len();
    app.mode = Mode::Popup(Popup::Search(s));
    for mi in machines {
        app.command_on(
            mi,
            "search.query",
            json!({"q": q, "limit": 50, "context": 0}),
            Pending::Parity(PReply::Search(Reply::Global { seq })),
        );
    }
}

/// Focus the hit's pane, then (once it's drawn) copy mode on the hit's line.
pub fn jump_to(app: &mut App, h: &Hit, q: Option<String>) {
    if !h.live {
        app.toast(format!(
            "{} is closed — `vibeke pane read {} --source archive`",
            h.handle, h.pane
        ));
        return;
    }
    app.focus_pane(h.mi, &h.pane);
    app.parity.search.jump = Some(Jump {
        mi: h.mi,
        pane: h.pane.clone(),
        goal: Goal { line: h.line, q },
        at: Instant::now(),
    });
    poll_jump(app);
}

/// Open copy mode for a pending jump once the pane's cells are here.
pub fn poll_jump(app: &mut App) {
    let Some(j) = app.parity.search.jump.clone() else {
        return;
    };
    if j.at.elapsed() > Duration::from_secs(5) {
        app.parity.search.jump = None;
        return;
    }
    if app.cur != j.mi
        || app.focused_pane().as_deref() != Some(j.pane.as_str())
        || !matches!(app.mode, Mode::Normal)
        || !app.m().panes.contains_key(&j.pane)
    {
        return;
    }
    app.parity.search.jump = None;
    app.enter_copy(None);
    if let Mode::Copy(cm) = &mut app.mode {
        cm.archive.goal = Some(j.goal);
    }
    advance(app);
}

pub fn popup_key(app: &mut App, ev: KeyEvent, mut s: Box<GlobalSearch>) {
    if ev.kind == KeyKind::Release {
        app.mode = Mode::Popup(Popup::Search(s));
        return;
    }
    let n = s.hits.len();
    match ev.key {
        Key::Named(NamedKey::Escape) => return,
        Key::Named(NamedKey::Enter) => {
            let fresh = s.searched.as_deref() == Some(s.input.trim());
            if fresh && let Some(h) = s.hits.get(s.sel).cloned() {
                let q = s.searched.clone();
                app.mode = Mode::Normal;
                jump_to(app, &h, q);
                return;
            }
            return run_global(app, s);
        }
        Key::Named(NamedKey::Down | NamedKey::Tab) => s.sel = (s.sel + 1).min(n.saturating_sub(1)),
        Key::Char('n' | 'j') if ev.mods.ctrl() => s.sel = (s.sel + 1).min(n.saturating_sub(1)),
        Key::Named(NamedKey::Up) => s.sel = s.sel.saturating_sub(1),
        Key::Char('p' | 'k') if ev.mods.ctrl() => s.sel = s.sel.saturating_sub(1),
        Key::Named(NamedKey::Backspace) => {
            s.input.pop();
        }
        Key::Char('u') if ev.mods.ctrl() => s.input.clear(),
        Key::Char(c) if !ev.mods.ctrl() && !ev.mods.alt() => s.input.push(c),
        _ => {}
    }
    app.mode = Mode::Popup(Popup::Search(s));
}

pub fn draw_popup(app: &App, g: &mut Grid, s: &GlobalSearch) -> Option<(u16, u16, CursorShape)> {
    let t = app.theme;
    let area = app.pane_area();
    let w = 100u16.min(area.w.saturating_sub(2)).max(24);
    let h = 24u16.min(area.h.saturating_sub(1)).max(7);
    let mut b = crate::popups::frame(app, g, w, h, "search all panes · enter search/jump · esc");
    b.line(&format!("> {}", s.input), t.bold(t.fg));
    let x = area.x + (area.w.saturating_sub(w)) / 2 + 2;
    let y0 = area.y + (area.h.saturating_sub(h)) / 3 + 2;
    let status = if s.waiting > 0 {
        "searching…".to_string()
    } else if s.searched.is_some() {
        format!(
            "{} hit(s){}",
            s.hits.len(),
            if s.errors.is_empty() {
                String::new()
            } else {
                format!(" · {}", s.errors.join("; "))
            }
        )
    } else {
        "type a query, enter to search (live scrollback + archive)".into()
    };
    b.line(&status, t.dim());
    let rows = h.saturating_sub(5) as usize;
    let skip = s.sel.saturating_sub(rows.saturating_sub(1));
    let multi = app.machines.len() > 1;
    let inner_w = w.saturating_sub(4);
    for (row, (i, hit)) in s.hits.iter().enumerate().skip(skip).take(rows).enumerate() {
        let y = y0 + 1 + row as u16;
        let st = if i == s.sel { t.sel(t.fg) } else { t.text() };
        g.fill(
            SRect {
                x,
                y,
                w: inner_w,
                h: 1,
            },
            st,
        );
        let mut label = String::new();
        if multi {
            label.push_str(&format!("[{}] ", app.machines[hit.mi].label));
        }
        label.push_str(&format!(
            "{} {}{}",
            hit.handle,
            crate::draw::truncate(&hit.title, 16),
            if hit.live { "" } else { " (closed)" }
        ));
        let lw = (UnicodeWidthStr::width(label.as_str()) as u16).min(inner_w / 2);
        let dim = if i == s.sel {
            Style {
                bg: t.selection,
                ..t.dim()
            }
        } else {
            t.dim()
        };
        g.put_str(x, y, &label, dim, lw);
        let tag = match hit.source.as_str() {
            "archive" => "▤ ",
            "scrollback" => "↑ ",
            _ => "▣ ",
        };
        let text = format!("{tag}{}", hit.text.trim());
        g.put_str(x + lw + 1, y, &text, st, inner_w.saturating_sub(lw + 1));
    }
    Some((
        x + 2 + UnicodeWidthStr::width(s.input.as_str()) as u16,
        y0 - 1,
        CursorShape::Bar,
    ))
}
