//! Grid comparison (18 §10.3, §10.5). The diff is computed from the grids, never from images.

use crate::grid::{Color, Grid, Rect};
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

/// The colours a host draws for "default" when it renders an inverse cell (18 §10.3). `None`
/// means unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefaultColors {
    pub fg: Option<(u8, u8, u8)>,
    pub bg: Option<(u8, u8, u8)>,
}

/// Inverse video as it is drawn: each cell with the inverse attribute becomes a plain cell
/// with its fg and bg swapped. A default colour is first replaced by the matching colour of
/// `defaults`. When a side of the cell is default and `defaults` does not know that colour,
/// the cell keeps its inverse attribute, because it cannot be resolved.
///
/// Herdr 0.9.3 draws a pane cell with SGR 7 this way: it swaps fg and bg and resolves
/// "default" to the pane terminal's colours (`src/pane/terminal.rs`, `ghostty_style`), so its
/// capture holds fixed RGB colours and no inverse flag. Vibeke's export keeps the flag.
pub fn resolve_inverse(g: &Grid, defaults: DefaultColors) -> Grid {
    let mut out = g.clone();
    for line in &mut out.cells {
        for cell in line {
            if !cell.attrs.inverse {
                continue;
            }
            let resolve = |c: Color, d: Option<(u8, u8, u8)>| match (c, d) {
                (Color::Default, Some((r, g, b))) => Some(Color::Rgb(r, g, b)),
                (Color::Default, None) => None,
                (c, _) => Some(c),
            };
            if let (Some(fg), Some(bg)) =
                (resolve(cell.fg, defaults.fg), resolve(cell.bg, defaults.bg))
            {
                cell.fg = bg;
                cell.bg = fg;
                cell.attrs.inverse = false;
            }
        }
    }
    out
}

/// An RGB colour.
type Rgb = (u8, u8, u8);

/// The default colours Herdr used for inverse cells, read from the pair of grids: every cell
/// that Vibeke shows inverse with default colours, and Herdr shows as a plain cell with the
/// same text and RGB colours, gives Herdr's default fg (its bg) and default bg (its fg). The
/// colours count only when all such cells agree; otherwise they stay unknown and the cells
/// differ. Herdr resolves these colours from its own terminal state, not from the plugin, so
/// one pair per grid is expected.
pub fn infer_inverse_defaults(herdr: &Grid, vibeke: &Grid) -> DefaultColors {
    let mut found: Option<(Rgb, Rgb)> = None;
    for (hl, vl) in herdr.cells.iter().zip(&vibeke.cells) {
        for (h, v) in hl.iter().zip(vl) {
            if !v.attrs.inverse
                || h.attrs.inverse
                || v.fg != Color::Default
                || v.bg != Color::Default
                || h.text != v.text
                || h.cont != v.cont
            {
                continue;
            }
            let (Color::Rgb(fr, fg, fb), Color::Rgb(br, bg, bb)) = (h.fg, h.bg) else {
                continue;
            };
            // Herdr's fg is the default bg, and its bg the default fg.
            let pair = ((br, bg, bb), (fr, fg, fb));
            match found {
                None => found = Some(pair),
                Some(p) if p == pair => {}
                Some(_) => return DefaultColors::default(),
            }
        }
    }
    match found {
        Some((fg, bg)) if fg != bg => DefaultColors {
            fg: Some(fg),
            bg: Some(bg),
        },
        _ => DefaultColors::default(),
    }
}

/// Compare two already masked grids. Cursor is not compared. When the sizes differ, the
/// overlapping area is still compared, so the report can show where the grids agree.
///
/// Inverse video is compared as it is drawn: both grids go through [`resolve_inverse`] with
/// the defaults from [`infer_inverse_defaults`], so Herdr's swapped RGB colours and Vibeke's
/// inverse flag compare equal when they look the same.
pub fn compare(herdr: &Grid, vibeke: &Grid) -> GridDiff {
    let defaults = infer_inverse_defaults(herdr, vibeke);
    compare_cells(
        &resolve_inverse(herdr, defaults),
        &resolve_inverse(vibeke, defaults),
    )
}

/// Compare two grids cell by cell, attributes included, without any normalization.
pub fn compare_cells(herdr: &Grid, vibeke: &Grid) -> GridDiff {
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
    /// `split`, `split-right`, `split-down`, `tab`, `overlay`, `popup`, `zoomed`.
    pub kind: String,
    pub rect: Rect,
}

/// Placement matches when the kind and the relative position (side of the split, popup
/// centred, tab focused) agree; exact sizes differ between the two hosts' frames.
///
/// * Splits: the plugin pane lies on the declared side of the pane it split, so something is to
///   its left (`split-right`: `col > 0`) or above it (`split-down`: `row > 0`) on both sides.
///   For `split` (the direction is the host's choice) the pane lies on the same side on both:
///   something is to its left on both or on neither, and above it on both or on neither.
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
        "split" => {
            (herdr.rect.col > 0) == (vibeke.rect.col > 0)
                && (herdr.rect.row > 0) == (vibeke.rect.row > 0)
        }
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
    use crate::grid::Attrs;

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

    /// One row as the two hosts captured it in the annotate `manage` step: Vibeke keeps
    /// SGR 7 with default colours, Herdr draws (30,30,46) on (205,214,244).
    fn highlighted_row(text: &str, cols: u16) -> (Grid, Grid) {
        let mut vibeke = Grid::blank(cols, 2);
        let mut herdr = Grid::blank(cols, 2);
        for (x, ch) in text.chars().enumerate().take(cols as usize) {
            for g in [&mut vibeke, &mut herdr] {
                g.cells[0][x].text = ch.to_string();
                g.cells[1][x].text = ch.to_string();
            }
        }
        for x in 0..cols as usize {
            vibeke.cells[0][x].attrs.inverse = true;
            herdr.cells[0][x].fg = Color::Rgb(30, 30, 46);
            herdr.cells[0][x].bg = Color::Rgb(205, 214, 244);
        }
        (herdr, vibeke)
    }

    #[test]
    fn inverse_matches_herdr_resolved_colours() {
        let (herdr, vibeke) = highlighted_row("› sample selection", 24);
        // The literal comparison sees the whole row.
        assert_eq!(compare_cells(&herdr, &vibeke).cells.len(), 24);
        assert_eq!(
            infer_inverse_defaults(&herdr, &vibeke),
            DefaultColors {
                fg: Some((205, 214, 244)),
                bg: Some((30, 30, 46)),
            }
        );
        assert!(compare(&herdr, &vibeke).is_empty());
        // The same row in another colour on one cell still differs there.
        let mut other = herdr.clone();
        other.cells[0][3].bg = Color::Rgb(1, 2, 3);
        let d = compare(&other, &vibeke);
        assert!(!d.is_empty());
        // The other cells give inconsistent defaults now, so nothing is resolved.
        assert_eq!(
            infer_inverse_defaults(&other, &vibeke),
            DefaultColors::default()
        );
        // Different text is still a difference.
        let mut text = herdr.clone();
        text.cells[0][2].text = "x".into();
        assert!(
            compare(&text, &vibeke)
                .cells
                .iter()
                .any(|(y, x, w)| (*y, *x) == (0, 2) && w.contains(&CellDiff::Text))
        );
    }

    #[test]
    fn inverse_with_explicit_colours_needs_no_defaults() {
        let mut vibeke = Grid::blank(2, 1);
        let mut herdr = Grid::blank(2, 1);
        vibeke.cells[0][0].attrs.inverse = true;
        vibeke.cells[0][0].fg = Color::Indexed(2);
        vibeke.cells[0][0].bg = Color::Rgb(9, 9, 9);
        herdr.cells[0][0].fg = Color::Rgb(9, 9, 9);
        herdr.cells[0][0].bg = Color::Indexed(2);
        assert!(compare(&herdr, &vibeke).is_empty());
        // An inverse cell with a default colour and no known default stays inverse.
        let mut lone = Grid::blank(1, 1);
        lone.cells[0][0].attrs.inverse = true;
        let r = resolve_inverse(&lone, DefaultColors::default());
        assert!(r.cells[0][0].attrs.inverse);
        // A plain default cell never equals an unresolved inverse one.
        assert!(!compare(&Grid::blank(1, 1), &lone).is_empty());
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
            &p("split", rect(0, 40, 24, 40)),
            &p("split", rect(0, 46, 39, 45))
        ));
        assert!(placement_matches(
            &p("split", rect(20, 0, 20, 80)),
            &p("split", rect(19, 0, 20, 90))
        ));
        assert!(!placement_matches(
            &p("split", rect(0, 40, 24, 40)),
            &p("split", rect(20, 0, 20, 80))
        ));
        assert!(placement_matches(
            &p("tab", rect(0, 0, 1, 1)),
            &p("tab", rect(3, 3, 9, 9))
        ));
    }
}
