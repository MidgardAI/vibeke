//! Masks and stability (18 §10.4). A mask is a rectangle or a regular expression, never "the
//! whole pane". Masks are applied before the stability check, so a spinner cannot keep a
//! screen from becoming stable.

use crate::grid::{Cell, Grid, Rect};
use anyhow::{Context, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

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

/// The text every masked cell gets; the same on both sides, so masked cells always compare equal.
pub const PLACEHOLDER: &str = "\u{2592}";

fn placeholder() -> Cell {
    Cell {
        text: PLACEHOLDER.into(),
        ..Cell::default()
    }
}

/// Masked cells become a fixed placeholder cell on both sides.
pub fn apply(grid: &Grid, masks: &[Mask]) -> anyhow::Result<Grid> {
    let mut out = grid.clone();
    for m in masks {
        match m {
            Mask::Rect { rect } => {
                let rows = (rect.row as usize)..(rect.row as usize + rect.rows as usize);
                let cols = (rect.col as usize)..(rect.col as usize + rect.cols as usize);
                for (y, line) in out.cells.iter_mut().enumerate() {
                    if !rows.contains(&y) {
                        continue;
                    }
                    for (x, cell) in line.iter_mut().enumerate() {
                        if cols.contains(&x) {
                            *cell = placeholder();
                        }
                    }
                }
            }
            Mask::Regex { regex } => {
                let re = Regex::new(regex).with_context(|| format!("mask regex {regex:?}"))?;
                for (y, line) in grid.cells.iter().enumerate() {
                    // The row's text and, for each byte range, the first column of its cell.
                    let mut text = String::new();
                    let mut spans: Vec<(usize, usize, usize)> = Vec::new();
                    for (x, cell) in line.iter().enumerate() {
                        if cell.cont {
                            continue;
                        }
                        let start = text.len();
                        text.push_str(&cell.text);
                        spans.push((start, text.len(), x));
                    }
                    for mat in re.find_iter(&text) {
                        if mat.start() == mat.end() {
                            continue;
                        }
                        for &(s, e, x) in &spans {
                            if s < mat.end() && e > mat.start() {
                                out.cells[y][x] = placeholder();
                                // A wide character's continuation goes with it.
                                if line.get(x + 1).is_some_and(|c| c.cont) {
                                    out.cells[y][x + 1] = placeholder();
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Rejects a mask that covers the whole grid, a rect outside it, or a regex that does not
/// compile or matches the empty string.
pub fn validate(masks: &[Mask], cols: u16, rows: u16) -> anyhow::Result<()> {
    for m in masks {
        match m {
            Mask::Rect { rect } => {
                if rect.rows == 0 || rect.cols == 0 {
                    bail!("mask {rect:?} is empty");
                }
                if rect.row as u32 + rect.rows as u32 > rows as u32
                    || rect.col as u32 + rect.cols as u32 > cols as u32
                {
                    bail!("mask {rect:?} is outside the {cols}x{rows} grid");
                }
                if rect.row == 0 && rect.col == 0 && rect.rows == rows && rect.cols == cols {
                    bail!("mask {rect:?} covers the whole pane; mask the changing part only");
                }
            }
            Mask::Regex { regex } => {
                let re = Regex::new(regex).with_context(|| format!("mask regex {regex:?}"))?;
                if re.is_match("") {
                    bail!("mask regex {regex:?} matches the empty string");
                }
            }
        }
    }
    Ok(())
}

/// Poll `capture` until the masked grid has not changed for `stable`, or `timeout` passes.
/// Returns the stable masked grid, or the last one with `false`.
pub fn wait_stable(
    mut capture: impl FnMut() -> anyhow::Result<Grid>,
    masks: &[Mask],
    stable: Duration,
    timeout: Duration,
) -> anyhow::Result<(Grid, bool)> {
    let deadline = Instant::now() + timeout;
    let poll = (stable / 4).clamp(Duration::from_millis(1), Duration::from_millis(50));
    let mut last = apply(&capture()?, masks)?;
    let mut since = Instant::now();
    loop {
        if since.elapsed() >= stable {
            return Ok((last, true));
        }
        if Instant::now() >= deadline {
            return Ok((last, false));
        }
        std::thread::sleep(poll);
        let g = apply(&capture()?, masks)?;
        if g != last {
            last = g;
            since = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid_of(lines: &[&str]) -> Grid {
        let cols = lines[0].chars().count() as u16;
        let mut g = Grid::blank(cols, lines.len() as u16);
        for (y, l) in lines.iter().enumerate() {
            for (x, ch) in l.chars().enumerate() {
                g.cells[y][x].text = ch.to_string();
            }
        }
        g
    }

    #[test]
    fn rect_mask() {
        let g = grid_of(&["abcd", "efgh"]);
        let m = [Mask::Rect {
            rect: Rect {
                row: 1,
                col: 1,
                rows: 1,
                cols: 2,
            },
        }];
        validate(&m, 4, 2).unwrap();
        let out = apply(&g, &m).unwrap();
        assert_eq!(out.text(), "abcd\ne\u{2592}\u{2592}h");
    }

    #[test]
    fn regex_mask() {
        let g = grid_of(&["at 12:34 ok", "no clock   "]);
        let m = [Mask::Regex {
            regex: r"\d\d:\d\d".into(),
        }];
        validate(&m, 11, 2).unwrap();
        let out = apply(&g, &m).unwrap();
        assert_eq!(
            out.text(),
            "at \u{2592}\u{2592}\u{2592}\u{2592}\u{2592} ok\nno clock   "
        );
    }

    #[test]
    fn regex_mask_covers_wide_continuation() {
        let mut g = Grid::blank(3, 1);
        g.cells[0][0].text = "世".into();
        g.cells[0][1] = Cell {
            text: String::new(),
            cont: true,
            ..Cell::default()
        };
        let out = apply(
            &g,
            &[Mask::Regex {
                regex: "世".into()
            }],
        )
        .unwrap();
        assert_eq!(out.cells[0][0].text, PLACEHOLDER);
        assert_eq!(out.cells[0][1].text, PLACEHOLDER);
        assert!(!out.cells[0][1].cont);
        assert_eq!(out.cells[0][2].text, " ");
    }

    #[test]
    fn whole_pane_and_bad_masks_rejected() {
        let whole = Mask::Rect {
            rect: Rect {
                row: 0,
                col: 0,
                rows: 2,
                cols: 4,
            },
        };
        assert!(
            validate(&[whole], 4, 2)
                .unwrap_err()
                .to_string()
                .contains("whole")
        );
        let outside = Mask::Rect {
            rect: Rect {
                row: 1,
                col: 3,
                rows: 1,
                cols: 2,
            },
        };
        assert!(validate(&[outside], 4, 2).is_err());
        assert!(validate(&[Mask::Regex { regex: ".*".into() }], 4, 2).is_err());
        assert!(validate(&[Mask::Regex { regex: "(".into() }], 4, 2).is_err());
    }

    #[test]
    fn masked_spinner_becomes_stable() {
        let frames = ["|", "/", "-", "\\"];
        let mut n = 0usize;
        let capture = || {
            n += 1;
            Ok(grid_of(&[format!("loading {}", frames[n % 4]).as_str()]))
        };
        let (_, stable) = wait_stable(
            capture,
            &[Mask::Regex {
                regex: r"loading .".into(),
            }],
            Duration::from_millis(60),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(stable);

        // Without the mask the spinner never settles.
        let mut n = 0usize;
        let capture = || {
            n += 1;
            Ok(grid_of(&[format!("loading {}", frames[n % 4]).as_str()]))
        };
        let (_, stable) = wait_stable(
            capture,
            &[],
            Duration::from_millis(60),
            Duration::from_millis(300),
        )
        .unwrap();
        assert!(!stable);
    }
}
