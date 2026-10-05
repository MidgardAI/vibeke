//! Copy mode (03 §11.1): vi keys over in-memory + archived scrollback, `/` `?` search with
//! smart case, character/line/rectangle selection, yank to the host clipboard (OSC 52).

use crate::screen::Grid;
use crate::theme::Theme;
use std::cell::Cell;
use vk_proto::input::{Key, KeyEvent, NamedKey};
use vk_proto::layout::Rect;
use vk_proto::render::{Cursor, Row, Style, attr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelKind {
    Char,
    Line,
    Block,
}

#[derive(Debug, Clone)]
pub struct CopyMode {
    pub pane: String,
    pub pending_req: Option<u64>,
    lines: Vec<Row>,
    /// Number of history rows at the front of `lines`.
    hist: usize,
    total_hist: u32,
    cols: u16,
    top: usize,
    cy: usize,
    cx: u16,
    sel: Option<(usize, u16, SelKind)>,
    search_input: Option<(String, bool)>,
    last_search: Option<(String, bool)>,
    height: Cell<u16>,
    message: Option<String>,
}

pub enum Outcome {
    Stay,
    Exit,
    Yank(String),
    Fetch { start: u32, count: u32 },
}

fn row_chars(r: &Row) -> Vec<String> {
    use unicode_segmentation::UnicodeSegmentation;
    let mut out = Vec::new();
    for s in &r.spans {
        for g in s.text.graphemes(true) {
            out.push(g.to_string());
            if unicode_width::UnicodeWidthStr::width(g) == 2 {
                out.push(String::new());
            }
        }
    }
    out
}

impl CopyMode {
    pub fn new(pane: &str, screen: Vec<Row>, cols: u16, cursor: Cursor) -> Self {
        let h = screen.len();
        CopyMode {
            pane: pane.into(),
            pending_req: None,
            lines: screen,
            hist: 0,
            total_hist: 0,
            cols,
            top: 0,
            cy: (cursor.row as usize).min(h.saturating_sub(1)),
            cx: cursor.col,
            sel: None,
            search_input: None,
            last_search: None,
            height: Cell::new(h as u16),
            message: None,
        }
    }

    /// Reply to a history fetch. Returns a follow-up fetch when only the size was requested.
    pub fn on_history(
        &mut self,
        req: u64,
        start: u32,
        total: u32,
        rows: Vec<Row>,
    ) -> Option<(u32, u32)> {
        if Some(req) != self.pending_req {
            return None;
        }
        self.pending_req = None;
        self.total_hist = total;
        if rows.is_empty() && self.hist == 0 && total > 0 {
            let count = total.min(20_000);
            return Some((total - count, count));
        }
        let n = rows.len();
        let mut new = rows;
        new.extend(self.lines.drain(self.hist..));
        // Keep any history already loaded that is newer than this batch.
        self.lines = new;
        self.hist = n;
        self.top += n;
        self.cy += n;
        if let Some((l, c, k)) = self.sel {
            self.sel = Some((l + n, c, k));
        }
        let _ = start;
        None
    }

    pub fn scroll_up(&mut self, n: usize) {
        self.top = self.top.saturating_sub(n);
        let h = self.height.get() as usize;
        if self.cy >= self.top + h {
            self.cy = self.top + h.saturating_sub(1);
        }
    }

    pub fn start_search(&mut self, backward: bool) {
        self.search_input = Some((String::new(), backward));
    }

    fn clamp(&mut self) {
        let max = self.lines.len().saturating_sub(1);
        self.cy = self.cy.min(max);
        let h = self.height.get().max(1) as usize;
        if self.cy < self.top {
            self.top = self.cy;
        }
        if self.cy >= self.top + h {
            self.top = self.cy + 1 - h;
        }
        let w = self.line_len(self.cy) as u16;
        self.cx = self
            .cx
            .min(self.cols.saturating_sub(1))
            .min(w.max(1).saturating_sub(0));
    }

    fn line_len(&self, y: usize) -> usize {
        self.lines.get(y).map(|r| row_chars(r).len()).unwrap_or(0)
    }

    fn text_of(&self, y: usize) -> String {
        self.lines.get(y).map(|r| r.text()).unwrap_or_default()
    }

    pub fn key(&mut self, ev: &KeyEvent) -> Outcome {
        self.message = None;
        if let Some((mut q, back)) = self.search_input.take() {
            match ev.key {
                Key::Named(NamedKey::Escape) => {}
                Key::Named(NamedKey::Enter) => {
                    if !q.is_empty() {
                        self.last_search = Some((q.clone(), back));
                        self.find(&q, back);
                    }
                }
                Key::Named(NamedKey::Backspace) => {
                    q.pop();
                    self.search_input = Some((q, back));
                }
                Key::Char(c) if !ev.mods.ctrl() => {
                    q.push(c);
                    self.search_input = Some((q, back));
                }
                _ => self.search_input = Some((q, back)),
            }
            return Outcome::Stay;
        }
        let h = self.height.get().max(1) as usize;
        let ctrl = ev.mods.ctrl();
        match ev.key {
            Key::Named(NamedKey::Escape) | Key::Char('q') if !ctrl => {
                if self.sel.is_some() && ev.key == Key::Named(NamedKey::Escape) {
                    self.sel = None;
                    return Outcome::Stay;
                }
                return Outcome::Exit;
            }
            Key::Char('c') if ctrl => return Outcome::Exit,
            Key::Char('u') if ctrl => self.cy = self.cy.saturating_sub(h / 2),
            Key::Char('d') if ctrl => self.cy += h / 2,
            Key::Char('b') if ctrl => self.cy = self.cy.saturating_sub(h),
            Key::Char('f') if ctrl => self.cy += h,
            Key::Char('v') if ctrl => self.toggle_sel(SelKind::Block),
            Key::Named(NamedKey::PageUp) => self.cy = self.cy.saturating_sub(h),
            Key::Named(NamedKey::PageDown) => self.cy += h,
            Key::Char('h') | Key::Named(NamedKey::Left) => self.cx = self.cx.saturating_sub(1),
            Key::Char('l') | Key::Named(NamedKey::Right) => self.cx += 1,
            Key::Char('k') | Key::Named(NamedKey::Up) => self.cy = self.cy.saturating_sub(1),
            Key::Char('j') | Key::Named(NamedKey::Down) => self.cy += 1,
            Key::Char('0') | Key::Named(NamedKey::Home) => self.cx = 0,
            Key::Char('$') | Key::Named(NamedKey::End) => {
                self.cx = self.line_len(self.cy).saturating_sub(1) as u16
            }
            Key::Char('g') => self.cy = 0,
            Key::Char('G') => self.cy = self.lines.len().saturating_sub(1),
            Key::Char('H') => self.cy = self.top,
            Key::Char('M') => self.cy = self.top + h / 2,
            Key::Char('L') => self.cy = self.top + h - 1,
            Key::Char('w') => self.word(true),
            Key::Char('b') => self.word(false),
            Key::Char('e') => self.word(true),
            Key::Char('v') => self.toggle_sel(SelKind::Char),
            Key::Char('V') => self.toggle_sel(SelKind::Line),
            Key::Char('/') => self.start_search(false),
            Key::Char('?') => self.start_search(true),
            Key::Char('n') | Key::Char('N') => {
                if let Some((q, back)) = self.last_search.clone() {
                    let back = if ev.key == Key::Char('N') {
                        !back
                    } else {
                        back
                    };
                    self.find(&q, back);
                }
            }
            Key::Char('y') | Key::Named(NamedKey::Enter) => {
                let text = self.selection_text();
                return match text {
                    Some(t) if !t.is_empty() => Outcome::Yank(t),
                    _ => Outcome::Exit,
                };
            }
            _ => {}
        }
        self.clamp();
        // Lazy-load more archived history when scrolling to the top.
        if self.cy == 0
            && self.pending_req.is_none()
            && self.hist > 0
            && (self.total_hist as usize) > self.hist
        {
            let loaded_from = self.total_hist as usize - self.hist;
            let count = loaded_from.min(20_000) as u32;
            if count > 0 {
                return Outcome::Fetch {
                    start: loaded_from as u32 - count,
                    count,
                };
            }
        }
        Outcome::Stay
    }

    fn toggle_sel(&mut self, k: SelKind) {
        self.sel = match self.sel {
            Some((_, _, kk)) if kk == k => None,
            _ => Some((self.cy, self.cx, k)),
        };
    }

    fn word(&mut self, fwd: bool) {
        let chars: Vec<char> = self.text_of(self.cy).chars().collect();
        let mut x = self.cx as usize;
        if fwd {
            while x < chars.len() && !chars[x].is_whitespace() {
                x += 1;
            }
            while x < chars.len() && chars[x].is_whitespace() {
                x += 1;
            }
            if x >= chars.len() && self.cy + 1 < self.lines.len() {
                self.cy += 1;
                x = 0;
            }
        } else {
            x = x.saturating_sub(1);
            while x > 0 && chars.get(x).is_some_and(|c| c.is_whitespace()) {
                x -= 1;
            }
            while x > 0 && chars.get(x - 1).is_some_and(|c| !c.is_whitespace()) {
                x -= 1;
            }
        }
        self.cx = x as u16;
    }

    fn find(&mut self, q: &str, back: bool) {
        let smart_lower = !q.chars().any(|c| c.is_uppercase());
        let norm = |s: &str| {
            if smart_lower {
                s.to_lowercase()
            } else {
                s.to_string()
            }
        };
        let needle = norm(q);
        let n = self.lines.len();
        for step in 1..=n {
            let y = if back {
                (self.cy + n - step % n) % n
            } else {
                (self.cy + step) % n
            };
            let hay = norm(&self.text_of(y));
            if let Some(byte) = hay.find(&needle) {
                self.cy = y;
                self.cx = hay[..byte].chars().count() as u16;
                self.clamp();
                return;
            }
        }
        self.message = Some(format!("not found: {q}"));
    }

    fn in_sel(&self, y: usize, x: u16) -> bool {
        let Some((sy, sx, k)) = self.sel else {
            return false;
        };
        let (a, b) = if (sy, sx) <= (self.cy, self.cx) {
            ((sy, sx), (self.cy, self.cx))
        } else {
            ((self.cy, self.cx), (sy, sx))
        };
        match k {
            SelKind::Line => y >= a.0 && y <= b.0,
            SelKind::Block => y >= a.0 && y <= b.0 && x >= sx.min(self.cx) && x <= sx.max(self.cx),
            SelKind::Char => {
                (y > a.0 || (y == a.0 && x >= a.1)) && (y < b.0 || (y == b.0 && x <= b.1))
            }
        }
    }

    /// Selected text; soft-wrapped lines are joined (03 §11.1).
    pub fn selection_text(&self) -> Option<String> {
        let (sy, _, k) = self.sel?;
        let (y0, y1) = (sy.min(self.cy), sy.max(self.cy));
        let mut out = String::new();
        for y in y0..=y1 {
            let chars = row_chars(&self.lines[y]);
            let picked: String = chars
                .iter()
                .enumerate()
                .filter(|(x, _)| self.in_sel(y, *x as u16) || k == SelKind::Line)
                .map(|(_, c)| c.as_str())
                .collect();
            let wrapped = self.lines[y].wrapped && k != SelKind::Block;
            out.push_str(if wrapped { &picked } else { picked.trim_end() });
            if y < y1 && !wrapped {
                out.push('\n');
            }
        }
        Some(out)
    }

    pub fn draw(&self, g: &mut Grid, r: Rect, t: &Theme) {
        let h = if self.search_input.is_some() || self.message.is_some() {
            r.h.saturating_sub(1)
        } else {
            r.h
        };
        self.height.set(h.max(1));
        for y in 0..h {
            let li = self.top + y as usize;
            let Some(row) = self.lines.get(li) else { break };
            g.put_row(r.x, r.y + y, row, r.w);
            let chars = row_chars(row);
            for x in 0..r.w {
                let selected = self.in_sel(li, x);
                let is_cursor = li == self.cy && x == self.cx;
                if selected || is_cursor {
                    let text = chars
                        .get(x as usize)
                        .map(|s| if s.is_empty() { " " } else { s.as_str() })
                        .unwrap_or(" ");
                    let st = if is_cursor {
                        Style {
                            attrs: attr::INVERSE,
                            ..Style::default()
                        }
                    } else {
                        t.sel(t.fg)
                    };
                    if !(chars.get(x as usize).is_some_and(|s| s.is_empty())) {
                        g.put_str(
                            r.x + x,
                            r.y + y,
                            text,
                            st,
                            1.max(unicode_width::UnicodeWidthStr::width(text) as u16),
                        );
                    }
                }
            }
        }
        let pos = format!(
            " [{}/{}] ",
            self.top,
            self.lines.len().saturating_sub(h as usize)
        );
        let w = pos.len() as u16;
        g.put_str(r.x + r.w.saturating_sub(w), r.y, &pos, t.rev(), w);
        if let Some((q, back)) = &self.search_input {
            let line = format!("{}{q}", if *back { "?" } else { "/" });
            g.fill(
                crate::screen::Rect {
                    x: r.x,
                    y: r.y + r.h - 1,
                    w: r.w,
                    h: 1,
                },
                t.text(),
            );
            g.put_str(r.x, r.y + r.h - 1, &line, t.text(), r.w);
        } else if let Some(m) = &self.message {
            g.put_str(r.x, r.y + r.h - 1, m, t.s(t.yellow), r.w);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vk_proto::input::Mods;
    use vk_proto::render::Span;

    fn row(s: &str, wrapped: bool) -> Row {
        Row {
            spans: vec![Span {
                style: Style::default(),
                text: s.into(),
                cols: s.chars().count() as u16,
            }],
            wrapped,
        }
    }

    #[test]
    fn select_yank_unwraps_and_searches() {
        let lines = vec![
            row("hello wor", true),
            row("ld again", false),
            row("third line", false),
        ];
        let mut cm = CopyMode::new("p", lines, 9, Cursor::default());
        cm.height.set(3);
        assert!(matches!(cm.key(&KeyEvent::ch('V')), Outcome::Stay));
        cm.key(&KeyEvent::ch('j'));
        let Outcome::Yank(t) = cm.key(&KeyEvent::ch('y')) else {
            panic!()
        };
        assert_eq!(t, "hello world again");
        let mut cm = CopyMode::new(
            "p",
            vec![row("abc", false), row("Needle here", false)],
            20,
            Cursor::default(),
        );
        cm.height.set(2);
        cm.key(&KeyEvent::ch('/'));
        for c in "needle".chars() {
            cm.key(&KeyEvent::ch(c));
        }
        cm.key(&KeyEvent::named(NamedKey::Enter));
        assert_eq!(cm.cy, 1);
        assert!(matches!(
            cm.key(&KeyEvent::new(Key::Char('c'), Mods::CTRL)),
            Outcome::Exit
        ));
    }
}
