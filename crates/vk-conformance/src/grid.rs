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
//! is the width of a wide lead cell, `c` marks a continuation cell. `palette.overrides` lists
//! only entries the program changed; `fg`/`bg` are null unless OSC 10/11 changed them.
//! [`Grid::from_vibeke_json`] parses this shape.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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

impl Grid {
    pub fn blank(cols: u16, rows: u16) -> Grid {
        todo!("lane GRID")
    }

    /// From `vk-term` rows (the Herdr side decodes Herdr's output with `vk-term`). Rows are
    /// padded or cut to `cols`; missing rows are blank.
    pub fn from_rows(rows: &[vk_proto::render::Row], cols: u16, nrows: u16) -> Grid {
        todo!("lane GRID")
    }

    /// From Vibeke's `pane.screenshot` `format = "grid"` data; also returns the palette changes
    /// the export reported.
    pub fn from_vibeke_json(v: &serde_json::Value) -> anyhow::Result<(Grid, PaletteChanges)> {
        todo!("lane GRID")
    }

    /// The sub-grid inside `r`. Cutting through a wide character leaves a blank cell.
    pub fn crop(&self, r: Rect) -> Grid {
        todo!("lane GRID")
    }

    /// See the module docs.
    pub fn resolve_palette(&mut self, changes: &PaletteChanges) {
        todo!("lane GRID")
    }

    /// Row-major text, one line per row, continuation cells skipped.
    pub fn text(&self) -> String {
        todo!("lane GRID")
    }
}
