//! Composed host-terminal cell grid and the minimal-diff escape writer (03 §6.2, §10).

use std::fmt::Write as _;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use vk_proto::render::{Color, CursorShape, Row, Style, attr};

const INLINE: usize = 14;

/// Compact storage for one grapheme cluster: inline up to 14 bytes, heap beyond (long ZWJ
/// sequences). Zero-padded so derived equality is byte equality.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CellText {
    Inline { len: u8, buf: [u8; INLINE] },
    Heap(Box<str>),
}

impl CellText {
    pub fn new(s: &str) -> Self {
        if s.len() <= INLINE {
            let mut buf = [0u8; INLINE];
            buf[..s.len()].copy_from_slice(s.as_bytes());
            CellText::Inline {
                len: s.len() as u8,
                buf,
            }
        } else {
            CellText::Heap(s.into())
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            CellText::Inline { len, buf } => {
                std::str::from_utf8(&buf[..*len as usize]).unwrap_or("")
            }
            CellText::Heap(s) => s,
        }
    }
}

impl From<&str> for CellText {
    fn from(s: &str) -> Self {
        CellText::new(s)
    }
}

/// One host cell. `width` is 1 or 2 for a visible head cell and 0 for the right half of a wide
/// character (whose `text` is empty).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Cell {
    pub text: CellText,
    pub width: u8,
    pub style: Style,
}

impl Cell {
    pub fn blank(style: Style) -> Self {
        Cell {
            text: CellText::new(" "),
            width: 1,
            style,
        }
    }

    pub fn is_tail(&self) -> bool {
        self.width == 0
    }

    fn tail(style: Style) -> Self {
        Cell {
            text: CellText::new(""),
            width: 0,
            style,
        }
    }

    fn is_default_blank(&self) -> bool {
        self.width == 1 && self.text.as_str() == " " && self.style == Style::default()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grid {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<Cell>,
}

/// Display width of one grapheme cluster (0 = not drawable: control or lone combining mark).
fn grapheme_width(g: &str) -> usize {
    let Some(first) = g.chars().next() else {
        return 0;
    };
    if first.is_control() {
        return 0;
    }
    if g.contains('\u{FE0F}') {
        return 2;
    }
    if g.contains('\u{FE0E}') {
        return 1;
    }
    UnicodeWidthStr::width(g).min(2)
}

impl Grid {
    pub fn new(cols: u16, rows: u16) -> Self {
        Grid {
            cols,
            rows,
            cells: vec![Cell::blank(Style::default()); cols as usize * rows as usize],
        }
    }

    fn idx(&self, x: u16, y: u16) -> usize {
        y as usize * self.cols as usize + x as usize
    }

    pub fn get(&self, x: u16, y: u16) -> Option<&Cell> {
        (x < self.cols && y < self.rows).then(|| &self.cells[self.idx(x, y)])
    }

    pub fn row(&self, y: u16) -> &[Cell] {
        let s = y as usize * self.cols as usize;
        &self.cells[s..s + self.cols as usize]
    }

    pub fn clear(&mut self) {
        let blank = Cell::blank(Style::default());
        self.cells.iter_mut().for_each(|c| *c = blank.clone());
    }

    pub fn fill(&mut self, rect: Rect, style: Style) {
        let x1 = rect.x.saturating_add(rect.w).min(self.cols);
        let y1 = rect.y.saturating_add(rect.h).min(self.rows);
        for y in rect.y..y1 {
            for x in rect.x..x1 {
                self.put_cell(x, y, Cell::blank(style));
            }
        }
    }

    /// If `(x, y)` is half of a wide character, blank the other half.
    fn break_wide_at(&mut self, x: u16, y: u16) {
        let i = self.idx(x, y);
        let (w, style) = (self.cells[i].width, self.cells[i].style);
        if w == 0 && x > 0 {
            self.cells[i - 1] = Cell::blank(style);
        } else if w == 2 && x + 1 < self.cols {
            self.cells[i + 1] = Cell::blank(style);
        }
    }

    /// Write a width-1 or width-2 cell (the latter also writes its tail at `x + 1`, which must
    /// be in range), repairing any wide character it partially overwrites.
    fn put_cell(&mut self, x: u16, y: u16, cell: Cell) {
        self.break_wide_at(x, y);
        let i = self.idx(x, y);
        if cell.width == 2 {
            self.break_wide_at(x + 1, y);
            self.cells[i + 1] = Cell::tail(cell.style);
        }
        self.cells[i] = cell;
    }

    /// Write `s` starting at `(x, y)`, at most `max_cols` columns, clipped to the grid. Returns
    /// the columns used. A wide character that does not fit is replaced by a space.
    pub fn put_str(&mut self, x: u16, y: u16, s: &str, style: Style, max_cols: u16) -> u16 {
        if y >= self.rows || x >= self.cols {
            return 0;
        }
        let limit = max_cols.min(self.cols - x) as usize;
        let mut used = 0usize;
        for g in s.graphemes(true) {
            let w = grapheme_width(g);
            if w == 0 {
                continue;
            }
            if used + w > limit {
                if used < limit {
                    self.put_cell(x + used as u16, y, Cell::blank(style));
                    used += 1;
                }
                break;
            }
            self.put_cell(
                x + used as u16,
                y,
                Cell {
                    text: CellText::new(g),
                    width: w as u8,
                    style,
                },
            );
            used += w;
        }
        used as u16
    }

    /// Write one cell holding `text` (a single grapheme, e.g. a kitty unicode placeholder with
    /// its diacritics) without re-measuring it.
    pub fn put_grapheme(&mut self, x: u16, y: u16, text: &str, style: Style) {
        if x < self.cols && y < self.rows {
            self.put_cell(
                x,
                y,
                Cell {
                    text: CellText::new(text),
                    width: 1,
                    style,
                },
            );
        }
    }

    /// Blit a pane row. Returns the columns written; cells right of the row's content are left
    /// untouched (pane rows normally cover the full pane width).
    pub fn put_row(&mut self, x: u16, y: u16, row: &Row, max_cols: u16) -> u16 {
        let mut used = 0u16;
        for span in &row.spans {
            if used >= max_cols {
                break;
            }
            let Some(nx) = x.checked_add(used) else { break };
            used += self.put_str(nx, y, &span.text, span.style, max_cols - used);
        }
        used
    }
}

/// What the compositor needs to know about the host terminal to emit output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostCaps {
    pub truecolor: bool,
    pub sync_update: bool,
    pub undercurl: bool,
    /// Kitty graphics (`a=q` answered OK): browser panes draw with unicode placeholders.
    pub kitty_graphics: bool,
    /// Kitty shared-memory transmission (`t=s`) works (local host only).
    pub kitty_shm: bool,
    /// iTerm2 inline images (OSC 1337): whole-frame browser panes (iTerm2, and the fallback
    /// when kitty graphics are missing).
    pub iterm2_images: bool,
    /// The host terminal is on another machine (the TUI runs inside SSH): no shm, lower rates.
    pub host_remote: bool,
    /// Host cell size in device pixels (`CSI 16 t` or TIOCGWINSZ), 0 = unknown.
    pub cell_w: u16,
    pub cell_h: u16,
    /// Device pixel ratio × 100 (browser viewport sizing, 06 B3.2).
    pub dpr_x100: u16,
    /// SGR-pixels mouse (DECSET 1016) is supported.
    pub sgr_pixels: bool,
}

/// Nearest xterm-256 palette index for an RGB colour.
pub fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [i32; 6] = [0, 95, 135, 175, 215, 255];
    let nearest = |v: u8| {
        let v = v as i32;
        (0..6).min_by_key(|&i| (LEVELS[i] - v).abs()).unwrap_or(0)
    };
    let (ri, gi, bi) = (nearest(r), nearest(g), nearest(b));
    let cube = (LEVELS[ri], LEVELS[gi], LEVELS[bi]);
    let avg = (r as i32 + g as i32 + b as i32) / 3;
    let gray_i = ((avg - 8 + 5) / 10).clamp(0, 23);
    let gv = 8 + 10 * gray_i;
    let dist = |c: (i32, i32, i32)| {
        let (dr, dg, db) = (c.0 - r as i32, c.1 - g as i32, c.2 - b as i32);
        dr * dr + dg * dg + db * db
    };
    if dist((gv, gv, gv)) < dist(cube) {
        (232 + gray_i) as u8
    } else {
        (16 + 36 * ri + 6 * gi + bi) as u8
    }
}

/// The style as the host will actually be asked to render it (degraded per capabilities).
fn effective(style: Style, caps: &HostCaps) -> Style {
    let mut s = style;
    let q = |c: Color| match c {
        Color::Rgb(r, g, b) if !caps.truecolor => Color::Indexed(rgb_to_256(r, g, b)),
        c => c,
    };
    s.fg = q(s.fg);
    s.bg = q(s.bg);
    s.ul = q(s.ul);
    if s.attrs & attr::ANY_UNDERLINE == 0 || !caps.undercurl {
        s.ul = Color::Default;
    }
    if !caps.undercurl && s.attrs & attr::ANY_UNDERLINE != 0 {
        s.attrs = (s.attrs & !attr::ANY_UNDERLINE) | attr::UNDERLINE;
    }
    s
}

#[derive(Clone, Copy)]
enum Slot {
    Fg,
    Bg,
    Ul,
}

fn push_color(p: &mut Vec<String>, c: Color, slot: Slot) {
    let (base, ext, reset) = match slot {
        Slot::Fg => (30u16, 38, "39"),
        Slot::Bg => (40, 48, "49"),
        Slot::Ul => (0, 58, "59"),
    };
    match c {
        Color::Default => p.push(reset.to_string()),
        Color::Indexed(n) if base != 0 && n < 8 => p.push((base + n as u16).to_string()),
        Color::Indexed(n) if base != 0 && n < 16 => p.push((base + 60 + n as u16 - 8).to_string()),
        Color::Indexed(n) => p.push(format!("{ext};5;{n}")),
        Color::Rgb(r, g, b) => p.push(format!("{ext};2;{r};{g};{b}")),
    }
}

fn attr_params(attrs: u16, p: &mut Vec<String>) {
    let table = [
        (attr::BOLD, "1"),
        (attr::DIM, "2"),
        (attr::ITALIC, "3"),
        (attr::UNDERLINE, "4"),
        (attr::DOUBLE_UNDERLINE, "4:2"),
        (attr::UNDERCURL, "4:3"),
        (attr::DOTTED_UNDERLINE, "4:4"),
        (attr::DASHED_UNDERLINE, "4:5"),
        (attr::BLINK, "5"),
        (attr::INVERSE, "7"),
        (attr::HIDDEN, "8"),
        (attr::STRIKE, "9"),
    ];
    for (bit, code) in table {
        if attrs & bit != 0 {
            p.push(code.to_string());
        }
    }
}

struct StrSink<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for StrSink<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

struct Writer<'a> {
    out: &'a mut Vec<u8>,
    caps: &'a HostCaps,
    cur: Style,
    pos: Option<(u16, u16)>,
    cols: u16,
}

impl Writer<'_> {
    fn move_to(&mut self, x: u16, y: u16) {
        let mut best = format!("\x1b[{};{}H", y + 1, x + 1);
        if let Some((cx, cy)) = self.pos {
            let mut cands: Vec<String> = Vec::new();
            if cy == y {
                if x > cx {
                    cands.push(if x - cx == 1 {
                        "\x1b[C".into()
                    } else {
                        format!("\x1b[{}C", x - cx)
                    });
                } else if x < cx {
                    cands.push(if cx - x == 1 {
                        "\x1b[D".into()
                    } else {
                        format!("\x1b[{}D", cx - x)
                    });
                }
                cands.push(format!("\x1b[{}G", x + 1));
                if x == 0 {
                    cands.push("\r".into());
                }
            } else {
                let vert = if y > cy {
                    format!("\x1b[{}B", y - cy)
                } else {
                    format!("\x1b[{}A", cy - y)
                };
                if x == cx {
                    cands.push(vert.clone());
                }
                if x == 0 {
                    cands.push(format!("\r{vert}"));
                }
                cands.push(format!("{vert}\x1b[{}G", x + 1));
            }
            for c in cands {
                if c.len() < best.len() {
                    best = c;
                }
            }
        }
        self.out.extend_from_slice(best.as_bytes());
        self.pos = Some((x, y));
    }

    fn set_style(&mut self, style: Style) {
        let eff = effective(style, self.caps);
        if eff == self.cur {
            return;
        }
        let mut p: Vec<String> = Vec::new();
        if eff.attrs == self.cur.attrs {
            if eff.fg != self.cur.fg {
                push_color(&mut p, eff.fg, Slot::Fg);
            }
            if eff.bg != self.cur.bg {
                push_color(&mut p, eff.bg, Slot::Bg);
            }
            if eff.ul != self.cur.ul {
                push_color(&mut p, eff.ul, Slot::Ul);
            }
        } else {
            if self.cur != Style::default() {
                p.push("0".into());
            }
            attr_params(eff.attrs, &mut p);
            if eff.fg != Color::Default {
                push_color(&mut p, eff.fg, Slot::Fg);
            }
            if eff.bg != Color::Default {
                push_color(&mut p, eff.bg, Slot::Bg);
            }
            if eff.ul != Color::Default {
                push_color(&mut p, eff.ul, Slot::Ul);
            }
            if p.is_empty() {
                p.push("0".into());
            }
        }
        let _ = write!(StrSink(self.out), "\x1b[{}m", p.join(";"));
        self.cur = eff;
    }

    fn text(&mut self, cell: &Cell, width: u16, x: u16, y: u16) {
        let t = cell.text.as_str();
        if t.is_empty() || u16::from(cell.width) != width {
            self.out.push(b' ');
        } else {
            self.out.extend_from_slice(t.as_bytes());
        }
        let nx = x + width;
        self.pos = if nx >= self.cols { None } else { Some((nx, y)) };
    }
}

/// Emit the minimal escape stream turning a host screen showing `prev` into one showing `next`.
/// Leaves SGR reset and the cursor at an unspecified position (follow with [`cursor`]); callers
/// without synchronized-update support should hide the cursor around the call.
pub fn diff(prev: &Grid, next: &Grid, caps: &HostCaps, out: &mut Vec<u8>) {
    let start = out.len();
    if caps.sync_update {
        out.extend_from_slice(b"\x1b[?2026h");
    }
    let body = out.len();
    let full = prev.cols != next.cols || prev.rows != next.rows;
    let cols = next.cols;
    let mut w = Writer {
        out,
        caps,
        cur: Style::default(),
        pos: None,
        cols,
    };
    if full {
        w.out.extend_from_slice(b"\x1b[0m\x1b[H\x1b[2J");
    }
    for y in 0..next.rows {
        let nrow = next.row(y);
        let prow = if full { None } else { Some(prev.row(y)) };
        let mut x = 0u16;
        while x < cols {
            let ncell = &nrow[x as usize];
            let unit: u16 = if ncell.width == 2 && x + 1 < cols {
                2
            } else {
                1
            };
            let changed = match prow {
                Some(prow) => (0..unit).any(|k| nrow[(x + k) as usize] != prow[(x + k) as usize]),
                None => !(ncell.is_default_blank() && unit == 1),
            };
            if !changed {
                x += unit;
                continue;
            }
            // Bridge a short unchanged gap by reprinting it rather than moving the cursor.
            if let Some((px, py)) = w.pos
                && py == y
                && px < x
                && x - px <= 3
            {
                let cur = w.cur;
                let ok = (px..x).all(|gx| {
                    let c = &nrow[gx as usize];
                    c.width == 1 && effective(c.style, caps) == cur
                });
                if ok {
                    for gx in px..x {
                        w.text(&nrow[gx as usize], 1, gx, y);
                    }
                }
            }
            if w.pos != Some((x, y)) {
                w.move_to(x, y);
            }
            w.set_style(ncell.style);
            w.text(ncell, unit, x, y);
            x += unit;
        }
    }
    if w.cur != Style::default() {
        w.out.extend_from_slice(b"\x1b[0m");
    }
    if out.len() == body {
        out.truncate(start);
    } else if caps.sync_update {
        out.extend_from_slice(b"\x1b[?2026l");
    }
}

/// Position and show/hide the host cursor with the requested shape (steady DECSCUSR).
pub fn cursor(out: &mut Vec<u8>, x: u16, y: u16, visible: bool, shape: CursorShape) {
    cursor_blink(out, x, y, visible, shape, false);
}

/// Like [`cursor`] with an explicit blink flag.
pub fn cursor_blink(
    out: &mut Vec<u8>,
    x: u16,
    y: u16,
    visible: bool,
    shape: CursorShape,
    blink: bool,
) {
    if !visible {
        out.extend_from_slice(b"\x1b[?25l");
        return;
    }
    let base = match shape {
        CursorShape::Block => 1,
        CursorShape::Underline => 3,
        CursorShape::Bar => 5,
    };
    let n = if blink { base } else { base + 1 };
    let _ = write!(StrSink(out), "\x1b[{n} q\x1b[{};{}H\x1b[?25h", y + 1, x + 1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use vk_proto::render::Span;

    const ALL: HostCaps = HostCaps {
        truecolor: true,
        sync_update: false,
        undercurl: true,
        kitty_graphics: false,
        kitty_shm: false,
        iterm2_images: false,
        host_remote: false,
        cell_w: 0,
        cell_h: 0,
        dpr_x100: 0,
        sgr_pixels: false,
    };

    fn st(fg: Color, attrs: u16) -> Style {
        Style {
            fg,
            attrs,
            ..Style::default()
        }
    }

    // ---- mini terminal emulator used to verify diff output ----
    struct Emu {
        g: Grid,
        x: usize,
        y: usize,
        cur: Style,
    }

    impl Emu {
        fn new(cols: u16, rows: u16) -> Self {
            Emu {
                g: Grid::new(cols, rows),
                x: 0,
                y: 0,
                cur: Style::default(),
            }
        }

        fn print(&mut self, bytes: &[u8]) {
            for g in std::str::from_utf8(bytes).unwrap().graphemes(true) {
                if g == "\r" {
                    self.x = 0;
                    continue;
                }
                let w = grapheme_width(g);
                assert!(w > 0, "unexpected control {g:?}");
                assert!(self.x + w <= self.g.cols as usize, "write past edge");
                self.g.put_cell(
                    self.x as u16,
                    self.y as u16,
                    Cell {
                        text: CellText::new(g),
                        width: w as u8,
                        style: self.cur,
                    },
                );
                self.x += w;
            }
        }

        fn run(&mut self, bytes: &[u8]) {
            let mut i = 0;
            let mut text_start = 0;
            while i < bytes.len() {
                if bytes[i] == 0x1b {
                    self.print(&bytes[text_start..i]);
                    assert_eq!(bytes[i + 1], b'[');
                    let mut j = i + 2;
                    while !(0x40..=0x7e).contains(&bytes[j]) {
                        j += 1;
                    }
                    let params = std::str::from_utf8(&bytes[i + 2..j]).unwrap();
                    self.csi(params, bytes[j] as char);
                    i = j + 1;
                    text_start = i;
                } else {
                    i += 1;
                }
            }
            self.print(&bytes[text_start..]);
        }

        fn csi(&mut self, params: &str, fin: char) {
            let nums: Vec<usize> = params.split(';').map(|p| p.parse().unwrap_or(0)).collect();
            let n1 = nums[0].max(1);
            match fin {
                'H' => {
                    self.y = nums[0].max(1) - 1;
                    self.x = nums.get(1).copied().unwrap_or(1).max(1) - 1;
                }
                'C' => self.x += n1,
                'D' => self.x -= n1,
                'B' => self.y += n1,
                'A' => self.y -= n1,
                'G' => self.x = n1 - 1,
                'J' => {
                    assert_eq!(nums[0], 2);
                    assert_eq!(self.cur, Style::default(), "2J with non-default style");
                    self.g.clear();
                }
                'm' => self.sgr(params),
                'h' | 'l' => {}
                f => panic!("unhandled CSI {params}{f}"),
            }
        }

        fn sgr(&mut self, params: &str) {
            let items: Vec<&str> = params.split(';').collect();
            let mut i = 0;
            while i < items.len() {
                let it = items[i];
                let n: u32 = it.split(':').next().unwrap().parse().unwrap_or(0);
                let s = &mut self.cur;
                match n {
                    0 => *s = Style::default(),
                    1 => s.attrs |= attr::BOLD,
                    2 => s.attrs |= attr::DIM,
                    3 => s.attrs |= attr::ITALIC,
                    4 => match it.strip_prefix("4:") {
                        None | Some("1") => s.attrs |= attr::UNDERLINE,
                        Some("2") => s.attrs |= attr::DOUBLE_UNDERLINE,
                        Some("3") => s.attrs |= attr::UNDERCURL,
                        Some("4") => s.attrs |= attr::DOTTED_UNDERLINE,
                        Some("5") => s.attrs |= attr::DASHED_UNDERLINE,
                        other => panic!("ul {other:?}"),
                    },
                    5 => s.attrs |= attr::BLINK,
                    7 => s.attrs |= attr::INVERSE,
                    8 => s.attrs |= attr::HIDDEN,
                    9 => s.attrs |= attr::STRIKE,
                    30..=37 => s.fg = Color::Indexed((n - 30) as u8),
                    40..=47 => s.bg = Color::Indexed((n - 40) as u8),
                    90..=97 => s.fg = Color::Indexed((n - 90 + 8) as u8),
                    100..=107 => s.bg = Color::Indexed((n - 100 + 8) as u8),
                    39 => s.fg = Color::Default,
                    49 => s.bg = Color::Default,
                    59 => s.ul = Color::Default,
                    38 | 48 | 58 => {
                        let c = if items[i + 1] == "5" {
                            let c = Color::Indexed(items[i + 2].parse().unwrap());
                            i += 2;
                            c
                        } else {
                            assert_eq!(items[i + 1], "2");
                            let v: Vec<u8> = items[i + 2..i + 5]
                                .iter()
                                .map(|x| x.parse().unwrap())
                                .collect();
                            i += 4;
                            Color::Rgb(v[0], v[1], v[2])
                        };
                        match n {
                            38 => s.fg = c,
                            48 => s.bg = c,
                            _ => s.ul = c,
                        }
                    }
                    x => panic!("sgr {x}"),
                }
                i += 1;
            }
        }
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    fn random_grid(rng: &mut Rng, cols: u16, rows: u16, base: Option<&Grid>) -> Grid {
        const ALPHA: [&str; 9] = ["a", "b", " ", "宽", "é", "e\u{301}", "😀", "x", "ø"];
        let styles = [
            Style::default(),
            st(Color::Indexed(1), attr::BOLD),
            st(Color::Rgb(10, 200, 30), 0),
            Style {
                fg: Color::Indexed(12),
                bg: Color::Rgb(1, 2, 3),
                ul: Color::Rgb(255, 0, 0),
                attrs: attr::UNDERCURL | attr::ITALIC,
            },
            Style {
                bg: Color::Indexed(200),
                attrs: attr::UNDERLINE | attr::INVERSE,
                ..Style::default()
            },
        ];
        let mut g = base.cloned().unwrap_or_else(|| Grid::new(cols, rows));
        let edits = if base.is_some() { 1 + rng.below(6) } else { 40 };
        for _ in 0..edits {
            let y = rng.below(rows as usize) as u16;
            let x = rng.below(cols as usize) as u16;
            let n = 1 + rng.below(8);
            let s: String = (0..n).map(|_| ALPHA[rng.below(ALPHA.len())]).collect();
            let mc = 1 + rng.below(cols as usize) as u16;
            g.put_str(x, y, &s, styles[rng.below(styles.len())], mc);
        }
        g
    }

    #[test]
    fn randomized_diff_round_trips_through_emulator() {
        let mut rng = Rng(0x9E3779B97F4A7C15);
        for round in 0..300 {
            let (cols, rows) = (6 + rng.below(20) as u16, 2 + rng.below(5) as u16);
            let caps = HostCaps {
                sync_update: round % 2 == 0,
                ..ALL
            };
            let empty = Grid::new(1, 1);
            let a = random_grid(&mut rng, cols, rows, None);
            let mut emu = Emu::new(cols, rows);
            let mut out = Vec::new();
            diff(&empty, &a, &caps, &mut out);
            emu.run(&out);
            assert_eq!(emu.g, a, "full draw mismatch round {round}");
            for _ in 0..5 {
                let before = emu.g.clone();
                let b = random_grid(&mut rng, cols, rows, Some(&before));
                let mut out = Vec::new();
                diff(&before, &b, &caps, &mut out);
                emu.run(&out);
                assert_eq!(emu.g, b, "incremental mismatch round {round}");
                assert_eq!(emu.cur, Style::default());
            }
        }
    }

    #[test]
    fn put_str_wide_clip_and_replace() {
        let mut g = Grid::new(10, 2);
        assert_eq!(g.put_str(0, 0, "a宽b", Style::default(), 10), 4);
        assert_eq!(g.get(1, 0).unwrap().width, 2);
        assert!(g.get(2, 0).unwrap().is_tail());
        // wide char that doesn't fit in max_cols becomes a space
        let mut g = Grid::new(10, 1);
        assert_eq!(g.put_str(0, 0, "ab宽", Style::default(), 3), 3);
        assert_eq!(g.get(2, 0).unwrap().text.as_str(), " ");
        assert_eq!(g.get(2, 0).unwrap().width, 1);
        // clipped at grid edge
        let mut g = Grid::new(5, 1);
        assert_eq!(g.put_str(3, 0, "abcdef", Style::default(), 99), 2);
        assert_eq!(g.put_str(9, 0, "z", Style::default(), 9), 0);
        // wide in last column
        let mut g = Grid::new(5, 1);
        assert_eq!(g.put_str(4, 0, "宽", Style::default(), 9), 1);
        assert_eq!(g.get(4, 0).unwrap().text.as_str(), " ");
    }

    #[test]
    fn graphemes_and_emoji() {
        let mut g = Grid::new(10, 1);
        let n = g.put_str(0, 0, "e\u{301}👨‍👩‍👧❤\u{fe0f}\u{7}x", Style::default(), 10);
        assert_eq!(n, 1 + 2 + 2 + 1);
        assert_eq!(g.get(0, 0).unwrap().text.as_str(), "e\u{301}");
        assert!(matches!(g.get(1, 0).unwrap().text, CellText::Heap(_)));
        assert_eq!(g.get(5, 0).unwrap().text.as_str(), "x");
    }

    #[test]
    fn overwriting_half_a_wide_char_blanks_other_half() {
        let mut g = Grid::new(6, 1);
        g.put_str(0, 0, "宽宽", Style::default(), 6);
        g.put_str(1, 0, "x", Style::default(), 1);
        assert_eq!(g.get(0, 0).unwrap().text.as_str(), " ");
        assert_eq!(g.get(1, 0).unwrap().text.as_str(), "x");
        assert_eq!(g.get(2, 0).unwrap().width, 2);
        g.put_str(2, 0, "y", Style::default(), 1);
        assert_eq!(g.get(3, 0).unwrap().text.as_str(), " ");
    }

    #[test]
    fn put_row_blits_spans() {
        let row = Row {
            spans: vec![
                Span {
                    style: st(Color::Indexed(2), 0),
                    text: "ab".into(),
                    cols: 2,
                },
                Span {
                    style: Style::default(),
                    text: "宽c".into(),
                    cols: 3,
                },
            ],
            wrapped: false,
        };
        let mut g = Grid::new(10, 2);
        assert_eq!(g.put_row(2, 1, &row, 10), 5);
        assert_eq!(g.get(2, 1).unwrap().style.fg, Color::Indexed(2));
        assert_eq!(g.get(4, 1).unwrap().text.as_str(), "宽");
        assert_eq!(g.get(6, 1).unwrap().text.as_str(), "c");
        // clipped
        let mut g = Grid::new(10, 2);
        assert_eq!(g.put_row(0, 0, &row, 3), 3);
        assert_eq!(g.get(2, 0).unwrap().text.as_str(), " ");
    }

    #[test]
    fn fill_and_clear() {
        let mut g = Grid::new(6, 3);
        g.put_str(0, 0, "宽宽宽", Style::default(), 6);
        let s = Style {
            bg: Color::Indexed(4),
            ..Style::default()
        };
        g.fill(
            Rect {
                x: 1,
                y: 0,
                w: 2,
                h: 5,
            },
            s,
        );
        assert_eq!(g.get(0, 0).unwrap().text.as_str(), " ");
        assert_eq!(g.get(1, 2).unwrap().style, s);
        assert_eq!(g.get(3, 0).unwrap().text.as_str(), " "); // broken wide at 2-3
        g.clear();
        assert_eq!(g, Grid::new(6, 3));
    }

    #[test]
    fn identical_grids_emit_nothing_even_with_sync() {
        let g = Grid::new(10, 3);
        let mut out = Vec::new();
        let caps = HostCaps {
            sync_update: true,
            ..ALL
        };
        diff(&g, &g, &caps, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn sync_wrapping() {
        let a = Grid::new(10, 3);
        let mut b = a.clone();
        b.put_str(0, 0, "hi", Style::default(), 5);
        let mut out = Vec::new();
        let caps = HostCaps {
            sync_update: true,
            ..ALL
        };
        diff(&a, &b, &caps, &mut out);
        assert_eq!(out, b"\x1b[?2026h\x1b[1;1Hhi\x1b[?2026l");
    }

    #[test]
    fn sgr_forms() {
        let a = Grid::new(10, 1);
        let mut b = a.clone();
        let s1 = Style {
            fg: Color::Rgb(1, 2, 3),
            bg: Color::Indexed(9),
            ul: Color::Rgb(9, 8, 7),
            attrs: attr::BOLD | attr::UNDERCURL,
        };
        b.put_str(0, 0, "a", s1, 1);
        let s2 = Style {
            fg: Color::Indexed(1),
            ..s1
        };
        b.put_str(1, 0, "b", s2, 1);
        let mut out = Vec::new();
        diff(&a, &b, &ALL, &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b[1;1H\x1b[1;4:3;38;2;1;2;3;101;58;2;9;8;7ma\x1b[31mb\x1b[0m"
        );
        // degraded host: no truecolor, no undercurl
        let mut out = Vec::new();
        diff(&a, &b, &HostCaps::default(), &mut out);
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, "\x1b[1;1H\x1b[1;4;38;5;16;101ma\x1b[31mb\x1b[0m");
        assert!(!s.contains("58;"));
    }

    #[test]
    fn quantize() {
        assert_eq!(rgb_to_256(0, 0, 0), 16);
        assert_eq!(rgb_to_256(255, 255, 255), 231);
        assert_eq!(rgb_to_256(255, 0, 0), 196);
        assert_eq!(rgb_to_256(128, 128, 128), 244);
        assert_eq!(rgb_to_256(0, 0, 255), 21);
    }

    #[test]
    fn cursor_helper() {
        let mut out = Vec::new();
        cursor(&mut out, 4, 2, true, CursorShape::Bar);
        assert_eq!(out, b"\x1b[6 q\x1b[3;5H\x1b[?25h");
        out.clear();
        cursor(&mut out, 0, 0, false, CursorShape::Block);
        assert_eq!(out, b"\x1b[?25l");
        out.clear();
        cursor_blink(&mut out, 0, 0, true, CursorShape::Underline, true);
        assert!(out.starts_with(b"\x1b[3 q"));
    }

    #[test]
    fn wide_char_never_in_last_column() {
        let a = Grid::new(4, 1);
        let mut b = a.clone();
        // force an invalid grid with a wide head in the last column
        let i = b.idx(3, 0);
        b.cells[i] = Cell {
            text: "宽".into(),
            width: 2,
            style: Style::default(),
        };
        let mut out = Vec::new();
        diff(&a, &b, &ALL, &mut out);
        assert!(!String::from_utf8(out).unwrap().contains('宽'));
    }

    #[test]
    fn half_of_wide_changed() {
        let mut a = Grid::new(6, 1);
        a.put_str(0, 0, "宽宽", Style::default(), 6);
        let mut b = a.clone();
        b.put_str(1, 0, "x", Style::default(), 1);
        let mut out = Vec::new();
        diff(&a, &b, &ALL, &mut out);
        let mut emu = Emu::new(6, 1);
        emu.g = a.clone();
        emu.run(&out);
        assert_eq!(emu.g, b);
    }

    #[test]
    fn one_changed_row_300x80_is_small() {
        let mut a = Grid::new(300, 80);
        for y in 0..80 {
            a.put_str(0, y, &"lorem ipsum ".repeat(30), Style::default(), 300);
        }
        let mut b = a.clone();
        b.put_str(0, 40, &"X".repeat(300), st(Color::Indexed(2), 0), 300);
        let mut out = Vec::new();
        let caps = HostCaps {
            sync_update: true,
            ..ALL
        };
        diff(&a, &b, &caps, &mut out);
        assert!(out.len() < 400, "{} bytes", out.len());
        // single cell change is tiny
        let mut c = a.clone();
        c.put_str(100, 3, "z", Style::default(), 1);
        let mut out = Vec::new();
        diff(&a, &c, &ALL, &mut out);
        assert!(out.len() < 20, "{}", out.len());
    }

    #[test]
    #[ignore]
    fn bench_full_redraw() {
        use std::time::Instant;
        let mut a = Grid::new(300, 80);
        let styles = [
            Style::default(),
            st(Color::Indexed(2), 0),
            st(Color::Rgb(1, 2, 3), attr::BOLD),
        ];
        let rows: Vec<Row> = (0..80)
            .map(|_| Row {
                spans: (0..30)
                    .map(|i| Span {
                        style: styles[i % 3],
                        text: "abcdefghij".into(),
                        cols: 10,
                    })
                    .collect(),
                wrapped: false,
            })
            .collect();
        let t = Instant::now();
        for _ in 0..100 {
            for (y, r) in rows.iter().enumerate() {
                a.put_row(0, y as u16, r, 300);
            }
        }
        println!("compose 300x80: {:?}/frame", t.elapsed() / 100);
        let blank = Grid::new(300, 80);
        let t = Instant::now();
        let mut out = Vec::new();
        for _ in 0..100 {
            out.clear();
            diff(&blank, &a, &ALL, &mut out);
        }
        println!(
            "diff 300x80 (all cells differ): {:?}/frame, {} bytes",
            t.elapsed() / 100,
            out.len()
        );
        let t = Instant::now();
        for _ in 0..100 {
            out.clear();
            diff(&Grid::new(1, 1), &a, &ALL, &mut out);
        }
        println!("full redraw 300x80: {:?}/frame", t.elapsed() / 100);
    }
}
