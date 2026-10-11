//! One renderer for both sides (18 §10.5), so fonts cannot create false differences: Herdr,
//! Vibeke and a diff overlay side by side, as one SVG, plus a small HTML index per run. Text
//! is escaped; grids are redacted before they reach this module.

use crate::diff::GridDiff;
use crate::grid::Grid;

/// One grid as SVG (fixed cell size, a fixed default palette and theme).
pub fn svg(grid: &Grid, title: &str) -> String {
    todo!("lane GRID")
}

/// Herdr | Vibeke | diff overlay. Either side may be missing (shown as "not run").
pub fn side_by_side(
    herdr: Option<&Grid>,
    vibeke: Option<&Grid>,
    diff: Option<&GridDiff>,
    title: &str,
) -> String {
    todo!("lane GRID")
}

/// Redact every cell's text with `vk-redact`, row by row, keeping the grid shape.
pub fn redact(grid: &Grid) -> Grid {
    todo!("lane GRID")
}
