//! The status bar (08 §4, D#1629): one row below the tab bar (`position = "top"`) or at the
//! bottom, with `left`/`center`/`right` segment lists from `[ui.status_bar]`. Segment data comes
//! from `status.segments` (07 §2.1) on the focused machine — requested only after model changes
//! (at most once a second) and every 10 s for the load average — while `clock`, `mode` and
//! `prefix_indicator` are drawn locally each frame. Before the first reply (or from a server
//! without the method) the model-derived segments are computed locally.

use crate::app::{App, Mode, Pending, RpcErr};
use crate::parity::Reply;
use crate::screen::{Grid, Rect as SRect};
use crossterm::event::{MouseButton as CtButton, MouseEvent, MouseEventKind};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;
use vk_proto::model::*;
use vk_proto::render::Style;

const MIN_INTERVAL: Duration = Duration::from_secs(1);
const REFRESH: Duration = Duration::from_secs(10);

#[derive(Default)]
pub struct State {
    /// (machine, `segments` object) from the last reply.
    pub data: Option<(usize, Value)>,
    /// The model changed since the last request.
    pub stale: bool,
    pub inflight: bool,
    pub last_req: Option<Instant>,
    /// Machines whose server has no `status.segments`.
    pub unsupported: HashSet<usize>,
    /// Minute last drawn by the clock (redraw when it changes).
    pub minute: i64,
}

pub fn enabled(app: &App) -> bool {
    app.config.ui.status_bar.enabled
}

fn at_top(app: &App) -> bool {
    matches!(
        app.config.ui.status_bar.position,
        vk_config::BarPosition::Top
    )
}

/// Rows the status bar takes from the pane area: (top side, bottom side). It sits next to the
/// tab bar when both are on the same edge (08 §3, §4).
pub fn reserved(app: &App) -> (u16, u16) {
    if !enabled(app) || app.size.1 < 6 {
        (0, 0)
    } else if at_top(app) {
        (1, 0)
    } else {
        (0, 1)
    }
}

/// The screen row of the bar, when shown.
pub fn row(app: &App) -> Option<u16> {
    let (tt, tb) = crate::chrome::tab_rows(app);
    match reserved(app) {
        (1, _) => Some(tt),
        (_, 1) => Some(app.size.1.saturating_sub(1 + tb)),
        _ => None,
    }
}

pub fn on_model(app: &mut App) {
    app.parity.status.stale = true;
}

/// Only while the bar is enabled: the clock's next minute (when a `clock` segment is shown)
/// and the next `status.segments` request `tick` would make.
pub(crate) fn deadlines(app: &App, now: Instant, d: &mut crate::deadline::Deadlines) {
    if !enabled(app) {
        return;
    }
    let bar = &app.config.ui.status_bar;
    if [&bar.left, &bar.center, &bar.right]
        .iter()
        .any(|side| side.iter().any(|s| s == "clock"))
    {
        let into = now_ms().rem_euclid(60_000) as u64;
        d.redraw(
            "statusbar.clock",
            now + Duration::from_millis(60_000 - into),
        );
    }
    let mi = app.cur;
    let st = &app.parity.status;
    if st.inflight || st.unsupported.contains(&mi) || !app.machines[mi].connected() {
        return;
    }
    let at = match st.last_req {
        None => now,
        Some(_) if st.data.as_ref().is_some_and(|(m, _)| *m != mi) => now,
        Some(t) if st.stale => t + MIN_INTERVAL,
        Some(t) => t + REFRESH,
    };
    d.at("statusbar", at);
}

/// Cheap refresh: one request in flight at most, after model changes (≥ 1 s apart) or every
/// 10 s; a clock redraw when the minute turns.
pub fn tick(app: &mut App) {
    if !enabled(app) {
        return;
    }
    let minute = now_ms() / 60_000;
    if minute != app.parity.status.minute {
        app.parity.status.minute = minute;
        app.dirty = true;
    }
    let mi = app.cur;
    let st = &app.parity.status;
    if st.inflight || st.unsupported.contains(&mi) || !app.machines[mi].connected() {
        return;
    }
    let since = st.last_req.map(|t| t.elapsed());
    let due = match since {
        None => true,
        Some(e) => {
            (st.stale && e >= MIN_INTERVAL)
                || e >= REFRESH
                || st.data.as_ref().is_some_and(|(m, _)| *m != mi)
        }
    };
    if !due {
        return;
    }
    let st = &mut app.parity.status;
    st.inflight = true;
    st.stale = false;
    st.last_req = Some(Instant::now());
    app.command("status.segments", json!({}), Pending::Parity(Reply::Status));
}

pub fn on_reply(app: &mut App, mi: usize, res: Result<Value, RpcErr>) {
    let st = &mut app.parity.status;
    st.inflight = false;
    match res {
        Ok(v) => {
            if let Some(s) = v.get("segments") {
                st.data = Some((mi, s.clone()));
            }
        }
        Err(e) if e.is_method_not_found() => {
            st.unsupported.insert(mi);
        }
        Err(_) => {}
    }
    app.dirty = true;
}

pub fn action(app: &mut App, action: &str) -> bool {
    if action != "status_bar_toggle" {
        return false;
    }
    let sb = &mut app.config.ui.status_bar;
    sb.enabled = !sb.enabled;
    app.parity.status.stale = true;
    app.prev = Grid::new(0, 0);
    app.toast(if enabled(app) {
        "status bar on (ui.status_bar.enabled to keep it)"
    } else {
        "status bar off"
    });
    true
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Local `HH:MM`.
pub fn clock(ms: i64) -> String {
    // SAFETY: localtime_r only writes into the provided struct.
    unsafe {
        let t: libc::time_t = (ms / 1000) as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return String::new();
        }
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    }
}

/// Segment data: the server's reply for the focused machine, else computed from the model.
fn data(app: &App) -> Value {
    if let Some((mi, v)) = &app.parity.status.data
        && *mi == app.cur
    {
        return v.clone();
    }
    local_data(app)
}

/// The model-derived subset of `status.segments` (no cpu, ports without previews).
pub fn local_data(app: &App) -> Value {
    let m = app.m();
    let ws = app.focused_ws();
    let task = ws
        .as_ref()
        .and_then(|w| w.task.as_deref())
        .and_then(|t| m.model.tasks.iter().find(|x| x.id == t));
    let focused = app.focused_pane();
    let open: Vec<&Interaction> = m
        .model
        .interactions
        .iter()
        .filter(|i| i.status == InteractionStatus::Open)
        .collect();
    let unfocused = open
        .iter()
        .filter(|i| focused.as_deref() != Some(&i.pane))
        .count();
    let runs = &m.model.runs;
    let done = runs
        .iter()
        .filter(|r| {
            r.execution.value == Execution::Idle && r.done_rev > *m.seen.get(&r.pane).unwrap_or(&0)
        })
        .count();
    let needs: HashSet<&String> = open.iter().map(|i| &i.run).collect();
    json!({
        "machine": {"label": m.label},
        "session": {"name": m.model.session},
        "workspace": ws.as_ref().map(|w| json!({"name": w.display_name(), "handle": w.handle})),
        "task": task.map(|t| json!({"handle": t.handle, "title": t.title})),
        "branch": ws
            .as_ref()
            .and_then(|w| w.branch.clone())
            .map(|b| json!({"name": b})),
        "attention": {"count": unfocused},
        "agents_summary": {
            "working": runs.iter().filter(|r| r.execution.value == Execution::Working).count(),
            "done": done,
            "needs_input": needs.len(),
            "error": runs.iter().filter(|r| matches!(r.execution.value, Execution::Error | Execution::RateLimited)).count(),
            "total": runs.len(),
        },
        "sync_input": {"enabled": false},
    })
}

fn num(v: &Value, p: &str) -> u64 {
    v.pointer(p).and_then(Value::as_u64).unwrap_or(0)
}

/// Text and style of one segment; empty text = not shown.
pub fn segment(app: &App, id: &str, d: &Value) -> (String, Style) {
    let t = &app.theme;
    let s = |p: &str| {
        d.pointer(p)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    match id {
        "machine" => {
            let up = app.m().connected();
            let dot = if up { "●" } else { "○" };
            (
                format!("{} {dot}", s("/machine/label")),
                t.s(if up { t.green } else { t.red }),
            )
        }
        "session" => (s("/session/name"), t.dim()),
        "workspace" => (s("/workspace/name"), t.text()),
        "task" => match d.get("task").filter(|v| !v.is_null()) {
            Some(tk) => {
                let h = tk.get("handle").and_then(Value::as_str).unwrap_or("");
                let title = tk.get("title").and_then(Value::as_str).unwrap_or("");
                (
                    format!("◆ {h} {}", crate::draw::truncate(title, 24)),
                    t.s(t.accent),
                )
            }
            None => (String::new(), t.text()),
        },
        "branch" => match d.pointer("/branch/name").and_then(Value::as_str) {
            Some(b) => (format!("⎇ {b}"), t.dim()),
            None => (String::new(), t.dim()),
        },
        "ports" => {
            let mut parts = Vec::new();
            match d.pointer("/ports/range") {
                Some(Value::Array(a)) if a.len() == 2 => {
                    parts.push(format!(":{}-{}", a[0], a[1]));
                }
                Some(o @ Value::Object(_)) => {
                    parts.push(format!(":{}-{}", num(o, "/start"), num(o, "/end")));
                }
                _ => {}
            }
            if let Some(Value::Array(pv)) = d.pointer("/ports/previews") {
                for p in pv.iter().take(3) {
                    parts.push(format!("◉:{}", num(p, "/port")));
                }
            }
            (parts.join(" "), t.s(t.blue))
        }
        "attention" => match num(d, "/attention/count") {
            0 => (String::new(), t.text()),
            n => (format!("{n} need you"), t.bold(t.red)),
        },
        "agents_summary" => {
            let mut parts = Vec::new();
            for (k, g) in [
                ("working", "●"),
                ("done", "✓"),
                ("needs_input", "⚠"),
                ("error", "✗"),
            ] {
                let n = num(d, &format!("/agents_summary/{k}"));
                if n > 0 {
                    parts.push(format!("{g}{n}"));
                }
            }
            (parts.join(" "), t.s(t.accent))
        }
        "cpu" => match d.pointer("/cpu/load/0").and_then(Value::as_f64) {
            Some(l) => (format!("load {l:.2}"), t.dim()),
            None => (String::new(), t.dim()),
        },
        "gateway" => match crate::gw_indicator::get(app, app.cur) {
            Some(g) => match g.render() {
                Some(text) => (
                    text,
                    t.s(match g.tone() {
                        crate::gw_indicator::Tone::Ok => t.green,
                        crate::gw_indicator::Tone::Warn => t.yellow,
                        crate::gw_indicator::Tone::Error => t.red,
                        crate::gw_indicator::Tone::Neutral => t.muted,
                    }),
                ),
                None => (String::new(), t.dim()),
            },
            None => (String::new(), t.dim()),
        },
        "clock" => (clock(now_ms()), t.text()),
        "prefix_indicator" => match app.mode {
            Mode::Prefix(_) => ("PREFIX".into(), t.rev()),
            _ => (String::new(), t.text()),
        },
        "mode" => {
            let m = match &app.mode {
                Mode::Normal | Mode::Prefix(_) => "normal",
                Mode::Navigate { .. } => "navigate",
                Mode::Copy(_) => "copy",
                Mode::Resize => "resize",
                Mode::Prompt(_) => "prompt",
                Mode::Popup(_) => "popup",
            };
            (m.to_string(), t.dim())
        }
        "sync_input" => {
            if let Some(b) = crate::sync_input::badge(app) {
                (b.trim().to_string(), t.bold(t.yellow))
            } else if d.pointer("/sync_input/enabled").and_then(Value::as_bool) == Some(true) {
                ("SYNC".into(), t.bold(t.yellow))
            } else {
                (String::new(), t.text())
            }
        }
        // Plugin segments arrive with M5.
        _ => (String::new(), t.text()),
    }
}

/// Rendered segments for one list: (id, text, style), empty ones dropped.
pub fn list(app: &App, ids: &[String], d: &Value) -> Vec<(String, String, Style)> {
    ids.iter()
        .filter_map(|id| {
            let (text, st) = segment(app, id, d);
            (!text.is_empty()).then(|| (id.clone(), text, st))
        })
        .collect()
}

const SEP: &str = " │ ";

fn width(segs: &[(String, String, Style)]) -> u16 {
    let w: usize = segs
        .iter()
        .map(|(_, s, _)| UnicodeWidthStr::width(s.as_str()))
        .sum();
    (w + UnicodeWidthStr::width(SEP) * segs.len().saturating_sub(1)) as u16
}

/// One placed segment: (id, text, style, x start, x end, separator before it).
type Placed = (String, String, Style, u16, u16, bool);

/// Segments with their x ranges on the bar.
fn layout(app: &App) -> Vec<Placed> {
    let cfg = &app.config.ui.status_bar;
    let d = data(app);
    let (x0, span) = crate::chrome::main_x(app);
    let cols = x0 + span;
    let mut left = list(app, &cfg.left, &d);
    let center = list(app, &cfg.center, &d);
    let mut right = list(app, &cfg.right, &d);
    // Native plugin segments (07 §7.4): after the configured ones on the left, before them on
    // the right.
    left.extend(crate::plugin_ui::segments(app, "left"));
    let mut pr = crate::plugin_ui::segments(app, "right");
    pr.append(&mut right);
    let mut right = pr;
    // The gateway indicator is on by default (right side, ahead of the configured segments);
    // listing `gateway` in any `[ui.status_bar]` list places it there instead.
    let listed = [&cfg.left, &cfg.center, &cfg.right]
        .iter()
        .any(|l| l.iter().any(|s| s == "gateway"));
    if !listed {
        let (text, st) = segment(app, "gateway", &d);
        if !text.is_empty() {
            right.insert(0, ("gateway".into(), text, st));
        }
    }
    let right = right;
    let mut out = Vec::new();
    let place = |segs: Vec<(String, String, Style)>, start: u16, out: &mut Vec<Placed>| {
        let mut x = start;
        for (i, (id, text, st)) in segs.into_iter().enumerate() {
            if i > 0 {
                x += UnicodeWidthStr::width(SEP) as u16;
            }
            let w = UnicodeWidthStr::width(text.as_str()) as u16;
            out.push((id, text, st, x, x + w, i > 0));
            x += w;
        }
    };
    let rw = width(&right);
    let cw = width(&center);
    place(left, x0 + 1, &mut out);
    place(center, x0 + span.saturating_sub(cw) / 2, &mut out);
    place(right, cols.saturating_sub(rw + 1), &mut out);
    out
}

pub fn draw(app: &App, g: &mut Grid) {
    let Some(y) = row(app) else {
        return;
    };
    let t = app.theme;
    let (x0, span) = crate::chrome::main_x(app);
    let cols = x0 + span;
    g.fill(
        SRect {
            x: x0,
            y,
            w: span,
            h: 1,
        },
        t.text(),
    );
    // Left, centre, right in that order: on a narrow bar the later lists overwrite.
    let sep_w = UnicodeWidthStr::width(SEP) as u16;
    for (_, text, st, a, _, sep) in layout(app) {
        if sep {
            let sx = a.saturating_sub(sep_w);
            g.put_str(sx, y, SEP, t.dim(), cols.saturating_sub(sx));
        }
        g.put_str(a, y, &text, st, cols.saturating_sub(a));
    }
}

/// A click on the `attention` segment opens the oldest unfocused card (08 §4).
pub fn on_mouse(app: &mut App, me: &MouseEvent) -> bool {
    let Some(y) = row(app) else {
        return false;
    };
    if me.row != y || crate::chrome::in_sidebar(app, me.column) {
        return false;
    }
    if let MouseEventKind::Down(CtButton::Left) = me.kind {
        let hit = layout(app)
            .into_iter()
            .find(|(_, _, _, a, b, _)| me.column >= *a && me.column < *b)
            .map(|(id, ..)| id);
        if hit.as_deref() == Some("attention") {
            crate::inbox::next_attention(app, false);
        } else if let Some(id) = hit.as_deref().filter(|h| h.starts_with("plugin:")) {
            crate::plugin_ui::click_segment(app, id);
        }
    }
    true
}
