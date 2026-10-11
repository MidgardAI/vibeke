//! The canonical cell grid (18 §10.3). Both sides are converted to this before comparison.
//!
//! * Exactly the inner PTY size; blank cells are kept.
//! * One grapheme per cell; a wide character is its cell plus a continuation cell
//!   (`cont = true`, empty text). Combining marks stay with their base.
//! * Colors are default, indexed or RGB. [`Grid::resolve_palette`] turns indexes that point at
//!   a palette entry the program changed (OSC 4) into RGB, and default colors the program
//!   changed (OSC 10/11) into RGB, so Herdr's capture (which bakes changed entries into RGB)
//!   and `vk-term` (which keeps the index) compare equal.
//! * Attributes: bold, italic, inverse and any underline.
//!
//! ## Vibeke's structured export
//!
//! `pane.screenshot` with `format = "grid"` returns (inline as `data`, or as a JSON blob):
//!
//! ```json
//! {"cols":80,"rows":24,
//!  "cursor":{"row":0,"col":0,"visible":true},
//!  "palette":{"overrides":{"1":"#ff0000"},"fg":"#d0d0d0","bg":null},
//!  "lines":[[{"t":"a","fg":1,"bg":"#101010","a":1},{"t":"世","w":2},{"c":true}, …], …]}
//! ```
//!
//! A cell omits default fields: `t` defaults to `" "`, `fg`/`bg` default to the default color,
//! a number is an index, `"#rrggbb"` is RGB, `a` is the `vk_proto::render::attr` bitmask, `w`
//! is the width of a wide lead cell (`2`; it must be followed by a continuation cell), `c`
//! marks a continuation cell. A continuation cell takes its colors and attributes from its
//! lead cell. Each line has exactly `cols` cells and there are exactly `rows` lines; `cursor`
//! may be null. Unknown keys are ignored (`vibeke/1` is additive). `palette.overrides` lists
//! only entries the program changed; `fg`/`bg` are null unless OSC 10/11 changed them.
//! [`Grid::from_vibeke_json`] parses this shape.

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use vk_proto::render::attr;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Attrs {
    pub bold: bool,
    pub italic: bool,
    pub inverse: bool,
    pub underline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Cell {
    pub text: String,
    /// The continuation cell of a wide character.
    pub cont: bool,
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            text: " ".into(),
            cont: false,
            fg: Color::Default,
            bg: Color::Default,
            attrs: Attrs::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub row: u16,
    pub col: u16,
    pub visible: bool,
}

/// Palette changes a program made (OSC 4, 10, 11), tracked per side.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaletteChanges {
    pub overrides: BTreeMap<u8, (u8, u8, u8)>,
    pub fg: Option<(u8, u8, u8)>,
    pub bg: Option<(u8, u8, u8)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grid {
    pub cols: u16,
    pub rows: u16,
    /// `rows` lines of exactly `cols` cells.
    pub cells: Vec<Vec<Cell>>,
    pub cursor: Option<Cursor>,
}

/// A rectangle in cells, used for crops and masks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub row: u16,
    pub col: u16,
    pub rows: u16,
    pub cols: u16,
}

/// Display width of one grapheme cluster, 0 for a control character or a lone combining mark.
/// The same rule as `grapheme_width` in `crates/vk-tui/src/screen.rs`, which advances the TUI
/// through render spans (that function is private to `vk-tui`, so it is mirrored here).
pub fn grapheme_width(g: &str) -> usize {
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

impl Color {
    fn from_render(c: vk_proto::render::Color) -> Color {
        match c {
            vk_proto::render::Color::Default => Color::Default,
            vk_proto::render::Color::Indexed(i) => Color::Indexed(i),
            vk_proto::render::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
        }
    }

    fn from_json(v: Option<&Value>) -> anyhow::Result<Color> {
        match v {
            None | Some(Value::Null) => Ok(Color::Default),
            Some(Value::Number(n)) => {
                let i = n
                    .as_u64()
                    .filter(|i| *i <= 255)
                    .with_context(|| format!("color index {n} is not 0-255"))?;
                Ok(Color::Indexed(i as u8))
            }
            Some(Value::String(s)) => {
                let (r, g, b) = parse_hex_rgb(s)?;
                Ok(Color::Rgb(r, g, b))
            }
            Some(other) => bail!("bad color {other}"),
        }
    }
}

/// `#rrggbb` to a triple.
pub(crate) fn parse_hex_rgb(s: &str) -> anyhow::Result<(u8, u8, u8)> {
    let h = s
        .strip_prefix('#')
        .filter(|h| h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .with_context(|| format!("bad RGB color {s:?}, want #rrggbb"))?;
    let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(0);
    Ok((byte(0), byte(2), byte(4)))
}

impl Attrs {
    fn from_bits(a: u16) -> Attrs {
        Attrs {
            bold: a & attr::BOLD != 0,
            italic: a & attr::ITALIC != 0,
            inverse: a & attr::INVERSE != 0,
            underline: a & attr::ANY_UNDERLINE != 0,
        }
    }
}

impl Cell {
    fn styled(text: &str, fg: Color, bg: Color, attrs: Attrs) -> Cell {
        Cell {
            text: text.into(),
            cont: false,
            fg,
            bg,
            attrs,
        }
    }

    /// The continuation cell of `lead`.
    fn cont_of(lead: &Cell) -> Cell {
        Cell {
            text: String::new(),
            cont: true,
            fg: lead.fg,
            bg: lead.bg,
            attrs: lead.attrs,
        }
    }
}

impl Grid {
    pub fn blank(cols: u16, rows: u16) -> Grid {
        Grid {
            cols,
            rows,
            cells: vec![vec![Cell::default(); cols as usize]; rows as usize],
            cursor: None,
        }
    }

    /// From `vk-term` rows (the Herdr side decodes Herdr's output with `vk-term`). Rows are
    /// padded or cut to `cols`; missing rows are blank.
    ///
    /// A span's `text` holds one grapheme per cell without the spacer of a wide character, and
    /// `cols` is authoritative: graphemes advance by [`grapheme_width`], and the span ends at
    /// its own `cols` even if the widths disagree.
    pub fn from_rows(rows: &[vk_proto::render::Row], cols: u16, nrows: u16) -> Grid {
        let mut grid = Grid::blank(cols, nrows);
        for (y, row) in rows.iter().take(nrows as usize).enumerate() {
            let line = &mut grid.cells[y];
            let mut x = 0usize;
            for span in &row.spans {
                let fg = Color::from_render(span.style.fg);
                let bg = Color::from_render(span.style.bg);
                let attrs = Attrs::from_bits(span.style.attrs);
                let end = x + span.cols as usize;
                let mut at = x;
                for g in span.text.graphemes(true) {
                    if at >= end {
                        break;
                    }
                    let mut w = grapheme_width(g).max(1);
                    if at + w > end {
                        w = 1;
                    }
                    if w == 2 && at + 1 >= cols as usize {
                        // A wide character cut by the right edge: a blank cell, as in crop.
                        if at < cols as usize {
                            line[at] = Cell::styled(" ", fg, bg, attrs);
                        }
                        at += 2;
                        continue;
                    }
                    if at < cols as usize {
                        let lead = Cell::styled(g, fg, bg, attrs);
                        if w == 2 {
                            line[at + 1] = Cell::cont_of(&lead);
                        }
                        line[at] = lead;
                    }
                    at += w;
                }
                // Columns the span covers but its text did not fill keep the span's style.
                while at < end {
                    if at < cols as usize {
                        line[at] = Cell::styled(" ", fg, bg, attrs);
                    }
                    at += 1;
                }
                x = end;
            }
        }
        grid
    }

    /// From Vibeke's `pane.screenshot` `format = "grid"` data; also returns the palette changes
    /// the export reported.
    pub fn from_vibeke_json(v: &Value) -> anyhow::Result<(Grid, PaletteChanges)> {
        let obj = v.as_object().context("grid export must be a JSON object")?;
        let dim = |k: &str| -> anyhow::Result<u16> {
            let n = obj
                .get(k)
                .and_then(Value::as_u64)
                .with_context(|| format!("grid export: `{k}` must be a number"))?;
            u16::try_from(n).with_context(|| format!("grid export: `{k}` = {n} is too large"))
        };
        let cols = dim("cols")?;
        let rows = dim("rows")?;
        let lines = obj
            .get("lines")
            .and_then(Value::as_array)
            .context("grid export: `lines` must be an array")?;
        if lines.len() != rows as usize {
            bail!("grid export: {} lines, expected rows = {rows}", lines.len());
        }
        let mut grid = Grid::blank(cols, rows);
        for (y, line) in lines.iter().enumerate() {
            let cells = line
                .as_array()
                .with_context(|| format!("grid export: line {y} must be an array"))?;
            if cells.len() != cols as usize {
                bail!(
                    "grid export: line {y} has {} cells, expected cols = {cols}",
                    cells.len()
                );
            }
            let mut wide_open = false;
            for (x, c) in cells.iter().enumerate() {
                let c = c
                    .as_object()
                    .with_context(|| format!("grid export: cell ({y},{x}) must be an object"))?;
                let is_cont = match c.get("c") {
                    None | Some(Value::Null) => false,
                    Some(Value::Bool(b)) => *b,
                    Some(other) => bail!("grid export: cell ({y},{x}): bad `c` {other}"),
                };
                if is_cont {
                    if !wide_open {
                        bail!("grid export: cell ({y},{x}) is a continuation without a wide lead");
                    }
                    wide_open = false;
                    grid.cells[y][x] = Cell::cont_of(&grid.cells[y][x - 1]);
                    continue;
                }
                if wide_open {
                    bail!("grid export: cell ({y},{x}) must continue the wide cell before it");
                }
                let text = match c.get("t") {
                    None | Some(Value::Null) => " ".to_string(),
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => bail!("grid export: cell ({y},{x}): bad `t` {other}"),
                };
                let width = match c.get("w") {
                    None | Some(Value::Null) => 1,
                    Some(w) => w
                        .as_u64()
                        .filter(|w| (1..=2).contains(w))
                        .with_context(|| format!("grid export: cell ({y},{x}): bad `w` {w}"))?,
                };
                if width == 2 && x + 1 >= cols as usize {
                    bail!("grid export: wide cell ({y},{x}) has no room for its continuation");
                }
                wide_open = width == 2;
                let a = match c.get("a") {
                    None | Some(Value::Null) => 0,
                    Some(a) => a
                        .as_u64()
                        .and_then(|a| u16::try_from(a).ok())
                        .with_context(|| format!("grid export: cell ({y},{x}): bad `a` {a}"))?,
                };
                grid.cells[y][x] = Cell::styled(
                    &text,
                    Color::from_json(c.get("fg"))
                        .with_context(|| format!("grid export: cell ({y},{x}) fg"))?,
                    Color::from_json(c.get("bg"))
                        .with_context(|| format!("grid export: cell ({y},{x}) bg"))?,
                    Attrs::from_bits(a),
                );
            }
            if wide_open {
                bail!("grid export: line {y} ends inside a wide cell");
            }
        }
        grid.cursor = match obj.get("cursor") {
            None | Some(Value::Null) => None,
            Some(c) => {
                let n = |k: &str| -> anyhow::Result<u16> {
                    c.get(k)
                        .and_then(Value::as_u64)
                        .and_then(|n| u16::try_from(n).ok())
                        .with_context(|| format!("grid export: cursor `{k}` must be a number"))
                };
                Some(Cursor {
                    row: n("row")?,
                    col: n("col")?,
                    visible: c
                        .get("visible")
                        .and_then(Value::as_bool)
                        .context("grid export: cursor `visible` must be a bool")?,
                })
            }
        };
        let mut changes = PaletteChanges::default();
        if let Some(p) = obj.get("palette").filter(|p| !p.is_null()) {
            let p = p
                .as_object()
                .context("grid export: `palette` must be an object")?;
            if let Some(o) = p.get("overrides").filter(|o| !o.is_null()) {
                let o = o
                    .as_object()
                    .context("grid export: `palette.overrides` must be an object")?;
                for (k, v) in o {
                    let i: u8 = k
                        .parse()
                        .with_context(|| format!("grid export: palette index {k:?}"))?;
                    let s = v.as_str().with_context(|| {
                        format!("grid export: palette entry {k} must be a string")
                    })?;
                    changes.overrides.insert(i, parse_hex_rgb(s)?);
                }
            }
            let opt = |k: &str| -> anyhow::Result<Option<(u8, u8, u8)>> {
                match p.get(k) {
                    None | Some(Value::Null) => Ok(None),
                    Some(Value::String(s)) => parse_hex_rgb(s).map(Some),
                    Some(other) => bail!("grid export: palette `{k}` bad value {other}"),
                }
            };
            changes.fg = opt("fg")?;
            changes.bg = opt("bg")?;
        }
        Ok((grid, changes))
    }

    /// The sub-grid inside `r`. Cutting through a wide character leaves a blank cell.
    pub fn crop(&self, r: Rect) -> Grid {
        let row0 = r.row.min(self.rows);
        let col0 = r.col.min(self.cols);
        let rows = r.rows.min(self.rows - row0);
        let cols = r.cols.min(self.cols - col0);
        let mut out = Grid::blank(cols, rows);
        for y in 0..rows as usize {
            let src = &self.cells[row0 as usize + y];
            for x in 0..cols as usize {
                let cell = &src[col0 as usize + x];
                let cut_left = cell.cont && x == 0;
                let cut_right = x + 1 == cols as usize
                    && !cell.cont
                    && src.get(col0 as usize + x + 1).is_some_and(|c| c.cont);
                out.cells[y][x] = if cut_left || cut_right {
                    Cell::default()
                } else {
                    cell.clone()
                };
            }
        }
        out.cursor = self.cursor.and_then(|c| {
            (c.row >= row0 && c.row < row0 + rows && c.col >= col0 && c.col < col0 + cols).then(
                || Cursor {
                    row: c.row - row0,
                    col: c.col - col0,
                    visible: c.visible,
                },
            )
        });
        out
    }

    /// See the module docs.
    pub fn resolve_palette(&mut self, changes: &PaletteChanges) {
        let fix = |c: &mut Color, default: Option<(u8, u8, u8)>| match *c {
            Color::Indexed(i) => {
                if let Some(&(r, g, b)) = changes.overrides.get(&i) {
                    *c = Color::Rgb(r, g, b);
                }
            }
            Color::Default => {
                if let Some((r, g, b)) = default {
                    *c = Color::Rgb(r, g, b);
                }
            }
            Color::Rgb(..) => {}
        };
        for line in &mut self.cells {
            for cell in line {
                fix(&mut cell.fg, changes.fg);
                fix(&mut cell.bg, changes.bg);
            }
        }
    }

    /// Row-major text, one line per row, continuation cells skipped.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for (y, line) in self.cells.iter().enumerate() {
            if y > 0 {
                out.push('\n');
            }
            for cell in line.iter().filter(|c| !c.cont) {
                out.push_str(&cell.text);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use vk_proto::render::{Row, Span, Style};

    fn span(text: &str, cols: u16, style: Style) -> Span {
        Span {
            style,
            text: text.into(),
            cols,
        }
    }

    #[test]
    fn from_rows_handles_wide_combining_and_padding() {
        let bold_red = Style {
            fg: vk_proto::render::Color::Indexed(1),
            attrs: attr::BOLD | attr::UNDERCURL,
            ..Style::default()
        };
        // "a", wide "世", "e" + combining acute, then a styled span; trailing blanks trimmed.
        let row = Row::new(
            vec![
                span("a世e\u{301}", 4, Style::default()),
                span("x", 1, bold_red),
            ],
            false,
        );
        let g = Grid::from_rows(&[row], 8, 2);
        assert_eq!((g.cols, g.rows), (8, 2));
        assert_eq!(g.cells[0].len(), 8);
        assert_eq!(g.cells[0][0].text, "a");
        assert_eq!(g.cells[0][1].text, "世");
        assert!(g.cells[0][2].cont && g.cells[0][2].text.is_empty());
        assert_eq!(g.cells[0][3].text, "e\u{301}");
        assert_eq!(g.cells[0][4].text, "x");
        assert_eq!(g.cells[0][4].fg, Color::Indexed(1));
        assert!(g.cells[0][4].attrs.bold && g.cells[0][4].attrs.underline);
        assert!(!g.cells[0][4].attrs.italic);
        assert_eq!(g.cells[0][5], Cell::default());
        assert!(g.cells[1].iter().all(|c| *c == Cell::default()));
        assert_eq!(g.text(), "a世e\u{301}x   \n        ");
    }

    #[test]
    fn from_rows_cuts_long_rows_and_a_wide_char_at_the_edge() {
        let row = Row::new(vec![span("ab世", 4, Style::default())], false);
        let g = Grid::from_rows(&[row], 3, 1);
        assert_eq!(g.text(), "ab ");
        assert!(!g.cells[0][2].cont);
    }

    fn sample_json() -> serde_json::Value {
        json!({
            "cols": 4, "rows": 2,
            "cursor": {"row": 1, "col": 2, "visible": true},
            "palette": {"overrides": {"1": "#ff0000"}, "fg": "#d0d0d0", "bg": null},
            "lines": [
                [{"t": "a", "fg": 1, "bg": "#101010", "a": 1}, {"t": "世", "w": 2}, {"c": true}, {}],
                [{}, {}, {}, {"t": "z", "a": 8}]
            ]
        })
    }

    #[test]
    fn json_valid() {
        let (g, p) = Grid::from_vibeke_json(&sample_json()).unwrap();
        assert_eq!(g.text(), "a世 \n   z");
        assert_eq!(g.cells[0][0].fg, Color::Indexed(1));
        assert_eq!(g.cells[0][0].bg, Color::Rgb(0x10, 0x10, 0x10));
        assert!(g.cells[0][0].attrs.bold);
        assert!(g.cells[0][2].cont);
        assert!(g.cells[1][3].attrs.underline);
        assert_eq!(
            g.cursor,
            Some(Cursor {
                row: 1,
                col: 2,
                visible: true
            })
        );
        assert_eq!(p.overrides.get(&1), Some(&(255, 0, 0)));
        assert_eq!(p.fg, Some((0xd0, 0xd0, 0xd0)));
        assert_eq!(p.bg, None);
    }

    #[test]
    fn json_invalid() {
        let bad = |f: &dyn Fn(&mut serde_json::Value)| {
            let mut v = sample_json();
            f(&mut v);
            Grid::from_vibeke_json(&v).unwrap_err().to_string()
        };
        assert!(bad(&|v| v["rows"] = json!(3)).contains("lines"));
        assert!(bad(&|v| v["lines"][1] = json!([{}, {}, {}])).contains("cells"));
        assert!(bad(&|v| v["lines"][0][2] = json!({})).contains("continue"));
        assert!(bad(&|v| v["lines"][1][0] = json!({"c": true})).contains("continuation"));
        assert!(bad(&|v| v["lines"][0][0]["fg"] = json!(300)).contains("fg"));
        assert!(bad(&|v| v["lines"][0][0]["bg"] = json!("red")).contains("bg"));
        assert!(bad(&|v| v["palette"]["overrides"] = json!({"1": "#ff00"})).contains("rrggbb"));
        assert!(bad(&|v| v["cols"] = json!("4")).contains("cols"));
        assert!(Grid::from_vibeke_json(&json!([])).is_err());
    }

    #[test]
    fn crop_through_wide_char_leaves_blank() {
        let (g, _) = Grid::from_vibeke_json(&sample_json()).unwrap();
        // Cut between "世" and its continuation on the right.
        let r = g.crop(Rect {
            row: 0,
            col: 0,
            rows: 1,
            cols: 2,
        });
        assert_eq!(r.text(), "a ");
        assert!(!r.cells[0][1].cont);
        // Start on the continuation cell.
        let r = g.crop(Rect {
            row: 0,
            col: 2,
            rows: 2,
            cols: 2,
        });
        assert_eq!(r.text(), "  \n z");
        assert_eq!(r.cells[0][0], Cell::default());
        assert_eq!(
            r.cursor,
            Some(Cursor {
                row: 1,
                col: 0,
                visible: true
            })
        );
        // A rectangle past the edge is clamped.
        let r = g.crop(Rect {
            row: 1,
            col: 3,
            rows: 5,
            cols: 5,
        });
        assert_eq!((r.cols, r.rows), (1, 1));
    }

    #[test]
    fn palette_resolution() {
        let (mut g, p) = Grid::from_vibeke_json(&sample_json()).unwrap();
        g.cells[1][0].fg = Color::Indexed(2);
        g.resolve_palette(&p);
        // Changed entry 1 becomes RGB, unchanged entry 2 stays an index.
        assert_eq!(g.cells[0][0].fg, Color::Rgb(255, 0, 0));
        assert_eq!(g.cells[1][0].fg, Color::Indexed(2));
        // Default fg was changed (OSC 10), default bg was not.
        assert_eq!(g.cells[0][1].fg, Color::Rgb(0xd0, 0xd0, 0xd0));
        assert_eq!(g.cells[0][1].bg, Color::Default);
        assert_eq!(g.cells[0][0].bg, Color::Rgb(0x10, 0x10, 0x10));
    }
}
