//! Copy mode (03 §11.1): vi (or emacs) keys over in-memory + archived scrollback, `/` `?`
//! search with smart case, character/line/rectangle selection, yank to the host clipboard
//! (OSC 52). Keys come from [`crate::copykeys`] (`[keys.copy_mode]`); a mouse drag selects too
//! (`crate::selection`).

use crate::copykeys::{CopyAction, CopyKeys};
use crate::screen::Grid;
use crate::theme::Theme;
use std::cell::Cell;
use std::sync::Arc;
use vk_proto::input::{Key, KeyEvent, NamedKey};
use vk_proto::layout::Rect;
use vk_proto::render::{Cursor, Row, Style, attr, mark};

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
    /// `g` was pressed: stay at the top while older pages load.
    want_top: bool,
    /// Archive paging and search fallback (`crate::search`, M4).
    pub archive: crate::search::ArchiveCursor,
    /// Resolved `[keys.copy_mode]` table.
    keys: Arc<CopyKeys>,
    /// Entered by a mouse drag: releasing copies (copy-on-select) or keeps the selection.
    pub mouse: bool,
    /// A `pane.scroll_requested` offset to apply once enough history is loaded.
    want_offset: Option<u32>,
}

pub enum Outcome {
    Stay,
    Exit,
    Yank(String),
    Fetch {
        start: u32,
        count: u32,
    },
    /// `/`/`?`/`n` found nothing in the loaded rows: search the pane's whole history
    /// (`search.query`, archive included).
    Search {
        q: String,
        back: bool,
    },
    /// At the top with every in-memory row loaded: page older rows from the archive
    /// (`pane.read {source: archive}`).
    Archive,
    /// `edit_scrollback` from copy mode: open the viewer at absolute line `line`.
    EditScrollback {
        line: u64,
    },
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
            want_top: false,
            archive: Default::default(),
            keys: CopyKeys::default_arc(),
            mouse: false,
            want_offset: None,
        }
    }

    pub fn clear_selection(&mut self) {
        self.sel = None;
    }

    pub fn set_keys(&mut self, keys: Arc<CopyKeys>) {
        self.keys = keys;
    }

    /// Mouse selection (03 §11.1): put the cursor on view cell (`row`, `col`); `start` anchors a
    /// new character selection there. Returns false outside the loaded rows.
    pub fn mouse_select(&mut self, row: u16, col: u16, start: bool) -> bool {
        let y = self.top + row as usize;
        if y >= self.lines.len() {
            return false;
        }
        self.cy = y;
        self.cx = col.min(self.cols.saturating_sub(1));
        if start || self.sel.is_none() {
            self.sel = Some((self.cy, self.cx, SelKind::Char));
        }
        true
    }

    /// Double click (03 §11.1): select the word (or run of blanks, or single punctuation
    /// character) under view cell (`row`, `col`), following soft wraps. Word characters:
    /// [`crate::selection::word_class`]. False outside the loaded rows.
    pub fn select_word_at(&mut self, row: u16, col: u16) -> bool {
        let y = self.top + row as usize;
        if y >= self.lines.len() {
            return false;
        }
        use crate::selection::word_class as class;
        let cells = |y: usize| row_chars(&self.lines[y]);
        let mut cur = cells(y);
        let mut x = (col as usize).min(cur.len().saturating_sub(1));
        // A wide character's second cell belongs to the first.
        while x > 0 && cur.get(x).is_some_and(|g| g.is_empty()) {
            x -= 1;
        }
        let cls = cur.get(x).map_or(0, |g| class(g));
        let joins = |g: &str| g.is_empty() || (cls != 2 && class(g) == cls);
        // Back to the start, crossing soft wraps from the row above.
        let (mut sy, mut sx) = (y, x);
        loop {
            if sx > 0 && joins(&cur[sx - 1]) {
                sx -= 1;
            } else if sx == 0 && sy > 0 && self.lines[sy - 1].wrapped && cls != 2 {
                let prev = cells(sy - 1);
                match prev.last() {
                    Some(g) if joins(g) => {
                        sy -= 1;
                        sx = prev.len() - 1;
                        cur = prev;
                    }
                    _ => break,
                }
            } else {
                break;
            }
        }
        // Forward to the end.
        let mut cur = cells(y);
        let (mut ey, mut ex) = (y, x);
        loop {
            if ex + 1 < cur.len() && joins(&cur[ex + 1]) {
                ex += 1;
            } else if ex + 1 >= cur.len()
                && self.lines[ey].wrapped
                && ey + 1 < self.lines.len()
                && cls != 2
            {
                let next = cells(ey + 1);
                match next.first() {
                    Some(g) if joins(g) => {
                        ey += 1;
                        ex = 0;
                        cur = next;
                    }
                    _ => break,
                }
            } else {
                break;
            }
        }
        self.sel = Some((sy, sx as u16, SelKind::Char));
        self.cy = ey;
        self.cx = ex as u16;
        true
    }

    /// Triple click: select the whole logical line under view row `row` (soft-wrapped rows
    /// joined). False outside the loaded rows.
    pub fn select_line_at(&mut self, row: u16) -> bool {
        let y = self.top + row as usize;
        if y >= self.lines.len() {
            return false;
        }
        let mut start = y;
        while start > 0 && self.lines[start - 1].wrapped {
            start -= 1;
        }
        let mut end = y;
        while self.lines[end].wrapped && end + 1 < self.lines.len() {
            end += 1;
        }
        self.sel = Some((start, 0, SelKind::Line));
        self.cy = end;
        self.cx = 0;
        true
    }

    /// Rows shown at the last draw.
    pub fn view_height(&self) -> u16 {
        self.height.get()
    }

    /// Mouse wheel in copy mode: move the view and cursor by `n` rows (older rows load at the
    /// top as with the keys).
    pub fn wheel(&mut self, up: bool, n: usize) -> Outcome {
        self.message = None;
        if up {
            self.top = self.top.saturating_sub(n);
            self.cy = self.cy.saturating_sub(n);
        } else {
            self.top = (self.top + n).min(self.lines.len().saturating_sub(1));
            self.cy += n;
        }
        self.after_move()
    }

    /// Absolute history line of the top of the view.
    pub fn view_top_abs(&self) -> u64 {
        self.top_abs() + self.top as u64
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
        self.archive.history_seen = true;
        if rows.is_empty() && self.hist == 0 && total > 0 && !self.archive.first_page_asked {
            // The first page stays in memory when we know where memory starts; older rows
            // come from the archive (`Outcome::Archive`).
            let floor = self.archive.mem_first.unwrap_or(0).min(total as u64) as u32;
            let count = (total - floor).min(20_000);
            if count > 0 {
                self.archive.first_page_asked = true;
                return Some((total - count, count));
            }
        }
        // Each page is older than everything loaded so far: prepend it.
        let n = rows.len();
        let mut new = rows;
        new.append(&mut self.lines);
        self.lines = new;
        self.hist += n;
        if self.want_top {
            self.top = 0;
            self.cy = 0;
        } else {
            self.top += n;
            self.cy += n;
        }
        if let Some((l, c, k)) = self.sel {
            self.sel = Some((l + n, c, k));
        }
        let _ = start;
        if let Some(o) = self.want_offset.take() {
            self.apply_offset(o);
        }
        None
    }

    /// Scroll so the top of the view is `offset` rows above the live screen's first row
    /// (`pane.scroll_requested`, 07 §2.6). Older in-memory rows are fetched first when the
    /// offset reaches past what is loaded; the offset is clamped to the history there is.
    pub fn scroll_to_offset(&mut self, offset: u32) -> Outcome {
        self.message = None;
        let start = self.hist + self.archive.rows;
        if offset as usize <= start {
            self.apply_offset(offset);
            return Outcome::Stay;
        }
        if self.pending_req.is_some() {
            // The first history page is on its way: apply once it is in.
            self.want_offset = Some(offset);
            return Outcome::Stay;
        }
        if !self.archive.active && (self.total_hist as usize) > self.hist {
            let loaded_from = self.total_hist as usize - self.hist;
            let floor = self
                .archive
                .mem_first
                .map_or(0, |m| m as usize)
                .min(loaded_from);
            let count = (loaded_from - floor).min(20_000) as u32;
            if count > 0 {
                self.want_offset = Some(offset);
                return Outcome::Fetch {
                    start: loaded_from as u32 - count,
                    count,
                };
            }
        }
        self.apply_offset(offset);
        Outcome::Stay
    }

    /// [`CopyMode::scroll_to_offset`] for a copy mode whose first history page is still loading.
    pub fn want_offset(&mut self, offset: u32) {
        if self.pending_req.is_some() {
            self.want_offset = Some(offset);
        } else {
            self.apply_offset(offset);
        }
    }

    fn apply_offset(&mut self, offset: u32) {
        let start = self.hist + self.archive.rows;
        self.want_top = false;
        self.top = start.saturating_sub(offset as usize);
        self.cy = self.top;
        self.clamp();
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
                        if !self.find(&q, back) {
                            return Outcome::Search { q, back };
                        }
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
        use CopyAction as A;
        match self.keys.resolve(ev) {
            None => {}
            Some(A::Cancel) => {
                if self.sel.is_some() {
                    self.sel = None;
                    return Outcome::Stay;
                }
                return Outcome::Exit;
            }
            Some(A::Exit) => return Outcome::Exit,
            Some(A::HalfPageUp) => self.cy = self.cy.saturating_sub(h / 2),
            Some(A::HalfPageDown) => self.cy += h / 2,
            Some(A::PageUp) => self.cy = self.cy.saturating_sub(h),
            Some(A::PageDown) => self.cy += h,
            Some(A::SelectBlock) => self.toggle_sel(SelKind::Block),
            Some(A::Left) => self.cx = self.cx.saturating_sub(1),
            Some(A::Right) => self.cx += 1,
            Some(A::Up) => self.cy = self.cy.saturating_sub(1),
            Some(A::Down) => self.cy += 1,
            Some(A::LineStart) => self.cx = 0,
            Some(A::LineEnd) => self.cx = self.line_len(self.cy).saturating_sub(1) as u16,
            Some(A::Top) => {
                self.cy = 0;
                self.want_top = true;
            }
            Some(A::Bottom) => {
                self.cy = self.lines.len().saturating_sub(1);
                self.want_top = false;
            }
            Some(A::ViewTop) => self.cy = self.top,
            Some(A::ViewMiddle) => self.cy = self.top + h / 2,
            Some(A::ViewBottom) => self.cy = self.top + h - 1,
            Some(A::WordNext | A::WordEnd) => self.word(true),
            Some(A::WordPrev) => self.word(false),
            Some(A::SelectChar) => self.toggle_sel(SelKind::Char),
            Some(A::SelectLine) => self.toggle_sel(SelKind::Line),
            Some(A::SearchForward) => self.start_search(false),
            Some(A::SearchBackward) => self.start_search(true),
            Some(a @ (A::SearchNext | A::SearchPrev)) => {
                if let Some((q, back)) = self.last_search.clone() {
                    let back = if a == A::SearchPrev { !back } else { back };
                    if !self.find(&q, back) {
                        return Outcome::Search { q, back };
                    }
                }
            }
            Some(A::Copy) => {
                let text = self.selection_text();
                return match text {
                    Some(t) if !t.is_empty() => Outcome::Yank(t),
                    _ => Outcome::Exit,
                };
            }
            Some(A::EditScrollback) => {
                return Outcome::EditScrollback {
                    line: self.view_top_abs(),
                };
            }
            Some(A::PromptPrev) => self.prompt_jump(true),
            Some(A::PromptNext) => self.prompt_jump(false),
            Some(A::SelectOutput) => self.select_output(),
        }
        self.after_move()
    }

    /// `[` / `]` (03 §8): the previous / next OSC 133 prompt row. At the top of the loaded rows
    /// without an earlier prompt, older rows load (press again).
    fn prompt_jump(&mut self, back: bool) {
        let is_prompt = |r: &Row| r.mark == mark::PROMPT;
        let found = if back {
            (0..self.cy).rev().find(|&y| is_prompt(&self.lines[y]))
        } else {
            (self.cy + 1..self.lines.len()).find(|&y| is_prompt(&self.lines[y]))
        };
        match found {
            Some(y) => {
                self.cy = y;
                self.cx = 0;
                self.want_top = false;
                // Show the prompt near the top of the view, with its output below.
                self.top = y.saturating_sub(1);
            }
            None if !self.lines.iter().any(is_prompt) => {
                self.message = Some("no prompt marks (shell integration, OSC 133)".into());
            }
            None if back => {
                self.message = Some("no earlier prompt loaded".into());
                self.cy = 0;
            }
            None => self.message = Some("no later prompt".into()),
        }
    }

    /// `o` (03 §8): select the output of the command whose prompt is at or above the cursor,
    /// up to the next prompt (command-line rows and trailing blank rows excluded).
    fn select_output(&mut self) {
        let Some(p) = (0..=self.cy.min(self.lines.len().saturating_sub(1)))
            .rev()
            .find(|&y| self.lines[y].mark == mark::PROMPT)
        else {
            self.message = Some("no prompt marks (shell integration, OSC 133)".into());
            return;
        };
        let end = (p + 1..self.lines.len())
            .find(|&y| self.lines[y].mark == mark::PROMPT)
            .unwrap_or(self.lines.len());
        let mut start = p + 1;
        while start < end && self.lines[start].mark != mark::NONE {
            start += 1;
        }
        let mut last = end;
        while last > start && self.lines[last - 1].text().trim().is_empty() {
            last -= 1;
        }
        if last <= start {
            self.message = Some("the command printed nothing".into());
            return;
        }
        self.sel = Some((start, 0, SelKind::Line));
        self.cy = last - 1;
        self.cx = 0;
    }

    /// Clamp the cursor, then ask for older rows when it reached the top.
    fn after_move(&mut self) -> Outcome {
        self.clamp();
        // Lazy-load older rows when scrolling to the top. `FetchHistory` indexes by absolute
        // line (archive, then memory); it pages the in-memory rows, and everything when the
        // server doesn't report where memory starts (`pane.read` → `mem_first`).
        let mem_first = self.archive.mem_first;
        if self.cy == 0
            && self.pending_req.is_none()
            && !self.archive.active
            && self.hist > 0
            && (self.total_hist as usize) > self.hist
        {
            let loaded_from = self.total_hist as usize - self.hist;
            let floor = mem_first.map_or(0, |m| m as usize).min(loaded_from);
            let count = (loaded_from - floor).min(20_000) as u32;
            if count > 0 {
                return Outcome::Fetch {
                    start: loaded_from as u32 - count,
                    count,
                };
            }
        }
        // Past memory: older rows come from the archive (`pane.read {source: archive}`), which
        // knows where it ends.
        let top = self.top_abs();
        if self.cy == 0
            && self.pending_req.is_none()
            && self.archive.history_seen
            && self.archive.inflight.is_none()
            && !self.archive.exhausted
            && top > self.archive.first.unwrap_or(0)
            && (self.archive.active || mem_first.is_some_and(|m| top <= m))
        {
            return Outcome::Archive;
        }
        Outcome::Stay
    }

    /// Absolute history line of the first loaded row (shared by `FetchHistory`, `pane.read`
    /// and `search.query`).
    pub fn top_abs(&self) -> u64 {
        if self.archive.active {
            self.archive.top_abs
        } else {
            (self.total_hist as u64).saturating_sub(self.hist as u64)
        }
    }

    /// Viewport position for scroll reports (`pane.scroll_changed`): rows between the top of
    /// the view and the live screen (0 = the live screen's top row is visible at the top), and
    /// the history rows known.
    pub fn scroll_offset(&self) -> (u32, u32) {
        let off = self.hist.saturating_sub(self.top) as u32;
        let total = (self.total_hist as usize).max(self.hist) + self.archive.rows;
        (off, total as u32)
    }

    /// (in-memory history rows loaded, in-memory history rows on the server, archive rows
    /// prepended).
    pub fn loaded(&self) -> (usize, u32, usize) {
        (self.hist, self.total_hist, self.archive.rows)
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Screen rows after the history, trailing blank rows trimmed (as the server counts them).
    pub fn screen_rows_trimmed(&self) -> usize {
        let start = self.hist + self.archive.rows;
        let screen = &self.lines[start.min(self.lines.len())..];
        let mut n = screen.len();
        while n > 0 && screen[n - 1].text().trim_end().is_empty() {
            n -= 1;
        }
        n
    }

    /// Older rows (from `pane.read`, starting at absolute line `from`) in front of everything
    /// loaded; the view stays put.
    pub fn prepend_archive(&mut self, from: u64, rows: Vec<Row>) {
        let n = rows.len();
        if n == 0 {
            return;
        }
        let mut new = rows;
        new.append(&mut self.lines);
        self.lines = new;
        self.archive.rows += n;
        self.archive.active = true;
        self.archive.top_abs = from;
        if self.want_top {
            self.top = 0;
            self.cy = 0;
        } else {
            self.top += n;
            self.cy += n;
        }
        if let Some((l, c, k)) = self.sel {
            self.sel = Some((l + n, c, k));
        }
    }

    /// Put the cursor on row `y` (index into the loaded rows), column `x`.
    pub fn jump(&mut self, y: usize, x: u16) {
        self.want_top = false;
        self.cy = y.min(self.lines.len().saturating_sub(1));
        self.cx = x;
        self.clamp();
    }

    /// Search the loaded rows from the cursor (smart case); false when nothing matches.
    pub fn find_text(&mut self, q: &str, back: bool) -> bool {
        self.find(q, back)
    }

    /// Text of loaded row `y`.
    pub fn row_text(&self, y: usize) -> String {
        self.text_of(y)
    }

    pub fn set_message(&mut self, m: impl Into<String>) {
        self.message = Some(m.into());
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    pub fn cursor_row(&self) -> usize {
        self.cy
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

    fn find(&mut self, q: &str, back: bool) -> bool {
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
                return true;
            }
        }
        self.message = Some(format!("not found: {q}"));
        false
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
            // Trailing blanks are trimmed at every line end and at the end of the selection.
            out.push_str(if wrapped && y < y1 {
                &picked
            } else {
                picked.trim_end()
            });
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
            ..Default::default()
        }
    }

    #[test]
    fn older_pages_are_prepended_and_g_stays_on_top() {
        let screen = vec![row("prompt", false)];
        let mut cm = CopyMode::new("p", screen, 20, Cursor::default());
        cm.height.set(5);
        cm.pending_req = Some(1);
        assert_eq!(cm.on_history(1, 0, 30_000, vec![]), Some((10_000, 20_000)));
        cm.pending_req = Some(2);
        let page = |a: usize, b: usize| {
            (a..b)
                .map(|i| row(&format!("l{i}"), false))
                .collect::<Vec<_>>()
        };
        assert_eq!(cm.on_history(2, 10_000, 30_000, page(10_000, 30_000)), None);
        assert_eq!(cm.lines.len(), 20_001);
        let Outcome::Fetch { start, count } = cm.key(&KeyEvent::ch('g')) else {
            panic!("expected a fetch for older rows")
        };
        assert_eq!((start, count), (0, 10_000));
        cm.pending_req = Some(3);
        cm.on_history(3, 0, 30_000, page(0, 10_000));
        assert_eq!(cm.lines.len(), 30_001);
        assert_eq!(cm.lines[0].text(), "l0");
        assert_eq!(cm.lines[29_999].text(), "l29999");
        assert_eq!((cm.top, cm.cy), (0, 0));
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
