//! Grid comparison (18 §10.3, §10.5). The diff is computed from the grids, never from images.

use crate::grid::{Grid, Rect};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellDiff {
    Text,
    Fg,
    Bg,
    Attrs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridDiff {
    pub same_size: bool,
    /// (row, col, what differs) for every differing cell.
    pub cells: Vec<(u16, u16, Vec<CellDiff>)>,
}

impl GridDiff {
    pub fn is_empty(&self) -> bool {
        self.same_size && self.cells.is_empty()
    }
}

/// Compare two already masked grids. Cursor is not compared.
pub fn compare(herdr: &Grid, vibeke: &Grid) -> GridDiff {
    todo!("lane GRID")
}

/// Placement comparison: the plugin pane's outer rectangle on each side, in the host screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    /// `split-right`, `split-down`, `tab`, `overlay`, `popup`, `zoomed`.
    pub kind: String,
    pub rect: Rect,
}

/// Placement matches when the kind and the relative position (side of the split, popup
/// centred, tab focused) agree; exact sizes differ between the two hosts' frames.
pub fn placement_matches(herdr: &Placement, vibeke: &Placement) -> bool {
    todo!("lane GRID")
}
