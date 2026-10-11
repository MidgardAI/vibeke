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

/// Compare two already masked grids. Cursor is not compared. When the sizes differ, the
/// overlapping area is still compared, so the report can show where the grids agree.
pub fn compare(herdr: &Grid, vibeke: &Grid) -> GridDiff {
    let same_size = herdr.cols == vibeke.cols && herdr.rows == vibeke.rows;
    let mut cells = Vec::new();
    for (y, (hl, vl)) in herdr.cells.iter().zip(&vibeke.cells).enumerate() {
        for (x, (h, v)) in hl.iter().zip(vl).enumerate() {
            let mut what = Vec::new();
            if h.text != v.text || h.cont != v.cont {
                what.push(CellDiff::Text);
            }
            if h.fg != v.fg {
                what.push(CellDiff::Fg);
            }
            if h.bg != v.bg {
                what.push(CellDiff::Bg);
            }
            if h.attrs != v.attrs {
                what.push(CellDiff::Attrs);
            }
            if !what.is_empty() {
                cells.push((y as u16, x as u16, what));
            }
        }
    }
    GridDiff { same_size, cells }
}

/// Placement comparison: the plugin pane's outer rectangle on each side, relative to the
/// host's pane area (the area panes share, without sidebars or tab bars). On Herdr this is the
/// `pane.layout` rectangle as returned ([`crate::herdr::HerdrSide::pane_rect`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    /// `split-right`, `split-down`, `tab`, `overlay`, `popup`, `zoomed`.
    pub kind: String,
    pub rect: Rect,
}

/// Placement matches when the kind and the relative position (side of the split, popup
/// centred, tab focused) agree; exact sizes differ between the two hosts' frames.
///
/// * Splits: the plugin pane lies on the declared side of the pane it split, so something is to
///   its left (`split-right`: `col > 0`) or above it (`split-down`: `row > 0`) on both sides.
/// * Popups: the centres of the two rectangles are at most 2 cells apart on each axis (both
///   hosts centre popups in the pane area; contents are only compared at equal inner sizes).
/// * Tabs, overlays and zoomed panes: the kind is enough.
pub fn placement_matches(herdr: &Placement, vibeke: &Placement) -> bool {
    if herdr.kind != vibeke.kind {
        return false;
    }
    match herdr.kind.as_str() {
        "split-right" => herdr.rect.col > 0 && vibeke.rect.col > 0,
        "split-down" => herdr.rect.row > 0 && vibeke.rect.row > 0,
        "popup" => {
            // Twice the centre, to stay in integers.
            let c2 = |start: u16, len: u16| 2 * start as i32 + len as i32;
            let dx =
                (c2(herdr.rect.col, herdr.rect.cols) - c2(vibeke.rect.col, vibeke.rect.cols)).abs();
            let dy =
                (c2(herdr.rect.row, herdr.rect.rows) - c2(vibeke.rect.row, vibeke.rect.rows)).abs();
            dx <= 4 && dy <= 4
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{Attrs, Color};

    fn rect(row: u16, col: u16, rows: u16, cols: u16) -> Rect {
        Rect {
            row,
            col,
            rows,
            cols,
        }
    }

    #[test]
    fn compare_reports_each_kind() {
        let a = Grid::blank(3, 2);
        let mut b = a.clone();
        assert!(compare(&a, &b).is_empty());
        b.cells[0][1].text = "x".into();
        b.cells[1][2].fg = Color::Indexed(3);
        b.cells[1][2].bg = Color::Rgb(1, 2, 3);
        b.cells[1][0].attrs = Attrs {
            bold: true,
            ..Attrs::default()
        };
        b.cursor = None;
        let d = compare(&a, &b);
        assert!(d.same_size);
        assert_eq!(
            d.cells,
            vec![
                (0, 1, vec![CellDiff::Text]),
                (1, 0, vec![CellDiff::Attrs]),
                (1, 2, vec![CellDiff::Fg, CellDiff::Bg]),
            ]
        );
        assert!(!d.is_empty());
    }

    #[test]
    fn compare_different_sizes() {
        let d = compare(&Grid::blank(3, 2), &Grid::blank(4, 2));
        assert!(!d.same_size);
        assert!(d.cells.is_empty());
        assert!(!d.is_empty());
    }

    #[test]
    fn placements() {
        let p = |kind: &str, r: Rect| Placement {
            kind: kind.into(),
            rect: r,
        };
        assert!(placement_matches(
            &p("split-right", rect(0, 40, 24, 40)),
            &p("split-right", rect(0, 50, 30, 50))
        ));
        assert!(!placement_matches(
            &p("split-right", rect(0, 40, 24, 40)),
            &p("split-right", rect(0, 0, 24, 40))
        ));
        assert!(!placement_matches(
            &p("split-right", rect(0, 40, 24, 40)),
            &p("split-down", rect(12, 0, 12, 80))
        ));
        assert!(placement_matches(
            &p("popup", rect(6, 20, 12, 40)),
            &p("popup", rect(7, 21, 11, 39))
        ));
        assert!(!placement_matches(
            &p("popup", rect(6, 20, 12, 40)),
            &p("popup", rect(0, 0, 12, 40))
        ));
        assert!(placement_matches(
            &p("tab", rect(0, 0, 1, 1)),
            &p("tab", rect(3, 3, 9, 9))
        ));
    }
}
