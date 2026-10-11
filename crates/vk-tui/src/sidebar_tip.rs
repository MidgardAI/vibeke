//! The full text of a cut-off sidebar row (08 §2). A row wider than the sidebar ends in a dim
//! `…`. Resting the pointer on it for [`DELAY`], or selecting it in navigate mode, shows the
//! whole text in a tooltip beside the sidebar, level with the row. A collision line's tooltip
//! also says how to open it. The tooltip never takes focus: a key, a click, a scroll or leaving
//! the row hides it, and it never shows over a popup, a prompt or another mode.

use crate::app::{App, Mode};
use crate::draw::SideRow;
use crate::event::{MouseEvent, MouseEventKind};
use crate::screen::{Grid, Rect as SRect};
use crate::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// How long the pointer rests on a row before its tooltip shows.
pub const DELAY: Duration = Duration::from_millis(400);
/// Widest tooltip text, in columns.
const MAX_W: usize = 64;
/// Most tooltip text lines; a longer text ends in `…`.
const MAX_LINES: usize = 8;

#[derive(Debug, Default, Clone)]
pub struct State {
    /// The cut-off sidebar row under the pointer (index into `sidebar_rows`) and since when.
    pub hover: Option<(usize, Instant)>,
    /// The hover tooltip's delay passed and a repaint was asked for.
    pub shown: bool,
}

fn cols(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// The row is wider than a sidebar `w` columns wide. Preview chip lines are laid out to fit.
pub fn is_cut(r: &SideRow, w: u16) -> bool {
    r.chips.is_empty() && r.segs.iter().map(|(s, _)| cols(s)).sum::<usize>() > w as usize
}

fn eligible(app: &App) -> bool {
    app.sidebar
        && matches!(app.mode, Mode::Normal | Mode::Navigate { .. })
        && crate::plugins::popup(app).is_none()
}

/// Track the pointer: resting on a cut-off row arms its tooltip; anything else hides it.
pub fn on_mouse(app: &mut App, me: &MouseEvent) {
    let row = match me.kind {
        MouseEventKind::Moved if eligible(app) && crate::chrome::in_sidebar(app, me.column) => {
            (me.row as usize).checked_sub(1).filter(|i| {
                crate::draw::sidebar_rows(app)
                    .get(*i)
                    .is_some_and(|r| is_cut(r, app.sidebar_w))
            })
        }
        _ => None,
    };
    match row {
        Some(i) if app.ux.tip.hover.is_some_and(|(h, _)| h == i) => {}
        Some(i) => {
            app.ux.tip.hover = Some((i, Instant::now()));
            app.ux.tip.shown = false;
        }
        None => app.ux.tip.hover = None,
    }
}

/// A key hides the hover tooltip.
pub fn hide(app: &mut App) {
    app.ux.tip.hover = None;
}

/// Repaint once when the hover tooltip is due.
pub fn tick(app: &mut App, now: Instant) {
    if let Some((_, since)) = app.ux.tip.hover
        && !app.ux.tip.shown
        && now.duration_since(since) >= DELAY
    {
        app.ux.tip.shown = true;
        app.dirty = true;
    }
}

/// Wake for the hover tooltip's delay (`tick` then repaints).
pub fn deadlines(app: &App, d: &mut crate::deadline::Deadlines) {
    if let Some((_, since)) = app.ux.tip.hover
        && !app.ux.tip.shown
    {
        d.at("sidebar_tip", since + DELAY);
    }
}

/// The row whose tooltip shows, and whether navigate mode selected it: the selected row when
/// it is cut off, else the row the pointer rested on.
fn current(app: &App, rows: &[SideRow]) -> Option<(usize, bool)> {
    if !eligible(app) {
        return None;
    }
    let w = app.sidebar_w;
    if let Mode::Navigate { sel } = app.mode
        && let Some((i, _)) = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.selectable())
            .nth(sel)
        && is_cut(&rows[i], w)
    {
        return Some((i, true));
    }
    let (i, since) = app.ux.tip.hover?;
    (Instant::now().duration_since(since) >= DELAY && rows.get(i).is_some_and(|r| is_cut(r, w)))
        .then_some((i, false))
}

/// Word-wrap `s` to `width` columns. A word wider than a line breaks inside it.
pub fn wrap_words(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        let need = if cur.is_empty() {
            cols(word)
        } else {
            cols(&cur) + 1 + cols(word)
        };
        if need <= width {
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
            continue;
        }
        if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        for g in word.graphemes(true) {
            if !cur.is_empty() && cols(&cur) + cols(g) > width {
                out.push(std::mem::take(&mut cur));
            }
            cur.push_str(g);
        }
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

/// Draw the tooltip over the main area, beside the sidebar. Returns its rectangle, so the
/// pane cursor under it can be hidden.
pub fn draw(app: &App, g: &mut Grid) -> Option<SRect> {
    crate::chrome::sidebar_x(app)?;
    let rows = crate::draw::sidebar_rows(app);
    let (i, nav) = current(app, &rows)?;
    let r = &rows[i];
    let t = app.theme;
    let (mx, mw) = crate::chrome::main_x(app);
    let text_w = MAX_W.min((mw as usize).saturating_sub(4));
    if text_w < 8 {
        return None;
    }
    let full = r.text();
    let mut lines = wrap_words(full.trim_start(), text_w);
    if lines.len() > MAX_LINES {
        lines.truncate(MAX_LINES);
        if let Some(last) = lines.last_mut() {
            while !last.is_empty() && cols(last) + 1 > text_w {
                last.pop();
            }
            last.push('…');
        }
    }
    let hint = r.collision.as_ref().map(|_| {
        if nav {
            "enter: details · d: dismiss"
        } else {
            "click for details"
        }
    });
    let inner = lines
        .iter()
        .map(|l| cols(l))
        .chain(hint.map(cols))
        .max()
        .unwrap_or(0)
        .min(text_w) as u16;
    let (bw, bh) = (
        inner + 4,
        lines.len() as u16 + 2 + u16::from(hint.is_some()),
    );
    let x = if crate::chrome::sidebar_right(app) {
        (mx + mw).saturating_sub(bw)
    } else {
        mx
    };
    // The first text line sits level with the row.
    let y = (i as u16).min(app.size.1.saturating_sub(bh));
    let o = SRect {
        x,
        y,
        w: bw,
        h: bh.min(app.size.1),
    };
    g.fill(o, t.text());
    let b = t.border(true);
    let (x1, y1) = (o.x + o.w - 1, o.y + o.h - 1);
    for bx in o.x..=x1 {
        g.put_str(bx, o.y, "─", b, 1);
        g.put_str(bx, y1, "─", b, 1);
    }
    for by in o.y..=y1 {
        g.put_str(o.x, by, "│", b, 1);
        g.put_str(x1, by, "│", b, 1);
    }
    g.put_str(o.x, o.y, "╭", b, 1);
    g.put_str(x1, o.y, "╮", b, 1);
    g.put_str(o.x, y1, "╰", b, 1);
    g.put_str(x1, y1, "╯", b, 1);
    // A collision line keeps its warning colour.
    let st = match (&r.collision, r.segs.first()) {
        (Some(_), Some((_, st))) => *st,
        _ => t.s(t.fg),
    };
    let mut ly = o.y + 1;
    for l in &lines {
        if ly >= y1 {
            break;
        }
        g.put_str(o.x + 2, ly, l, st, inner);
        ly += 1;
    }
    if let Some(h) = hint
        && ly < y1
    {
        g.put_str(o.x + 2, ly, h, t.dim(), inner);
    }
    Some(o)
}

#[cfg(test)]
#[path = "sidebar_tip_tests.rs"]
mod tests;
