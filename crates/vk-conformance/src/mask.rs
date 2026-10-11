//! Masks and stability (18 §10.4). A mask is a rectangle or a regular expression, never "the
//! whole pane". Masks are applied before the stability check, so a spinner cannot keep a
//! screen from becoming stable.

use crate::grid::{Grid, Rect};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Mask {
    Rect {
        rect: Rect,
    },
    /// Matched against each row's text; matching cells are masked.
    Regex {
        regex: String,
    },
}

/// Masked cells become a fixed placeholder cell on both sides.
pub fn apply(grid: &Grid, masks: &[Mask]) -> anyhow::Result<Grid> {
    todo!("lane GRID")
}

/// Rejects a mask that covers the whole grid, a rect outside it, or a regex that does not
/// compile or matches the empty string.
pub fn validate(masks: &[Mask], cols: u16, rows: u16) -> anyhow::Result<()> {
    todo!("lane GRID")
}

/// Poll `capture` until the masked grid has not changed for `stable`, or `timeout` passes.
/// Returns the stable masked grid, or the last one with `false`.
pub fn wait_stable(
    mut capture: impl FnMut() -> anyhow::Result<Grid>,
    masks: &[Mask],
    stable: Duration,
    timeout: Duration,
) -> anyhow::Result<(Grid, bool)> {
    todo!("lane GRID")
}
