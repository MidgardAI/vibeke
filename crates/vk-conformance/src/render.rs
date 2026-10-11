//! One renderer for both sides (18 §10.5), so fonts cannot create false differences: Herdr,
//! Vibeke and a diff overlay side by side, as one SVG, plus a small HTML index per run. Text
//! is escaped; grids are redacted before they reach this module.
//!
//! Fixed choices: a 9×18 pixel cell, the [`FONT`] family, the standard xterm 256-colour
//! palette (the same table as `palette` in `crates/vk-server/src/pane_api.rs`) and a fixed
//! default foreground and background.

use crate::diff::GridDiff;
use crate::grid::{Cell, Color, Grid, grapheme_width};
use std::fmt::Write as _;
use unicode_segmentation::UnicodeSegmentation;

const CELL_W: u32 = 9;
const CELL_H: u32 = 18;
const TITLE_H: u32 = 24;
const GAP: u32 = 16;
pub const FONT: &str = "DejaVu Sans Mono, Menlo, Consolas, monospace";
const DEFAULT_FG: (u8, u8, u8) = (229, 229, 229);
const DEFAULT_BG: (u8, u8, u8) = (0, 0, 0);
const PAGE_BG: &str = "#ffffff";
const DIFF_FILL: &str = "#ff2d55";

/// The xterm 256-colour table.
pub fn palette(n: u8) -> (u8, u8, u8) {
    const BASE: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    match n {
        0..=15 => BASE[n as usize],
        16..=231 => {
            let i = n - 16;
            let step = |x: u8| if x == 0 { 0 } else { 55 + 40 * x };
            (step(i / 36), step((i / 6) % 6), step(i % 6))
        }
        _ => {
            let g = 8 + 10 * (n - 232);
            (g, g, g)
        }
    }
}

fn hex((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn rgb(c: Color, default: (u8, u8, u8)) -> (u8, u8, u8) {
    match c {
        Color::Default => default,
        Color::Indexed(i) => palette(i),
        Color::Rgb(r, g, b) => (r, g, b),
    }
}

/// Escape text for SVG and HTML content and attribute values.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            // Control characters are not allowed in XML.
            c if c.is_control() => out.push('\u{fffd}'),
            c => out.push(c),
        }
    }
    out
}

fn panel_size(grid: Option<&Grid>) -> (u32, u32) {
    match grid {
        Some(g) => (
            (g.cols as u32 * CELL_W).max(160),
            TITLE_H + g.rows as u32 * CELL_H,
        ),
        None => (240, TITLE_H + 4 * CELL_H),
    }
}

/// The colours a cell is drawn with, after inverse.
fn cell_colors(c: &Cell) -> ((u8, u8, u8), (u8, u8, u8)) {
    let fg = rgb(c.fg, DEFAULT_FG);
    let bg = rgb(c.bg, DEFAULT_BG);
    if c.attrs.inverse { (bg, fg) } else { (fg, bg) }
}

/// One panel at (`x`, `y`): a title, then the grid or "not run", then an optional diff
/// overlay of highlighted cells.
fn panel(
    out: &mut String,
    x: u32,
    y: u32,
    grid: Option<&Grid>,
    title: &str,
    diff: Option<&GridDiff>,
) {
    let (w, h) = panel_size(grid);
    let _ = write!(
        out,
        r##"<g transform="translate({x},{y})"><text x="0" y="16" font-family="{font}" font-size="14" fill="#000000">{title}</text>"##,
        font = escape(FONT),
        title = escape(title),
    );
    let Some(g) = grid else {
        let _ = write!(
            out,
            r##"<rect x="0" y="{TITLE_H}" width="{w}" height="{bh}" fill="#eeeeee" stroke="#999999"/><text x="{tx}" y="{ty}" font-family="{font}" font-size="14" fill="#555555" text-anchor="middle">not run</text></g>"##,
            bh = h - TITLE_H,
            tx = w / 2,
            ty = TITLE_H + (h - TITLE_H) / 2 + 5,
            font = escape(FONT),
        );
        return;
    };
    let gw = g.cols as u32 * CELL_W;
    let gh = g.rows as u32 * CELL_H;
    let _ = write!(
        out,
        r#"<rect x="0" y="{TITLE_H}" width="{gw}" height="{gh}" fill="{}"/>"#,
        hex(DEFAULT_BG)
    );
    for (row, line) in g.cells.iter().enumerate() {
        let cy = TITLE_H + row as u32 * CELL_H;
        for (col, cell) in line.iter().enumerate() {
            if cell.cont {
                continue;
            }
            let wide = line.get(col + 1).is_some_and(|c| c.cont);
            let cw = if wide { 2 * CELL_W } else { CELL_W };
            let cx = col as u32 * CELL_W;
            let (fg, bg) = cell_colors(cell);
            if bg != DEFAULT_BG {
                let _ = write!(
                    out,
                    r#"<rect x="{cx}" y="{cy}" width="{cw}" height="{CELL_H}" fill="{}"/>"#,
                    hex(bg)
                );
            }
            if cell.text.trim().is_empty() && !cell.attrs.underline {
                continue;
            }
            let mut style = String::new();
            if cell.attrs.bold {
                style.push_str(r#" font-weight="bold""#);
            }
            if cell.attrs.italic {
                style.push_str(r#" font-style="italic""#);
            }
            if cell.attrs.underline {
                style.push_str(r#" text-decoration="underline""#);
            }
            // Keep a cell's text inside its cell, whatever the font's advance is.
            let _ = write!(
                out,
                r#"<text x="{cx}" y="{ty}" textLength="{cw}" lengthAdjust="spacingAndGlyphs" fill="{fill}"{style}>{text}</text>"#,
                ty = cy + CELL_H - 5,
                fill = hex(fg),
                text = escape(if cell.text.is_empty() {
                    " "
                } else {
                    &cell.text
                }),
            );
        }
    }
    if let Some(d) = diff {
        for &(row, col, _) in &d.cells {
            if row >= g.rows || col >= g.cols {
                continue;
            }
            let _ = write!(
                out,
                r#"<rect x="{}" y="{}" width="{CELL_W}" height="{CELL_H}" fill="{DIFF_FILL}" fill-opacity="0.45" stroke="{DIFF_FILL}" stroke-width="1"/>"#,
                col as u32 * CELL_W,
                TITLE_H + row as u32 * CELL_H,
            );
        }
        if !d.same_size {
            let _ = write!(
                out,
                r#"<rect x="0" y="{TITLE_H}" width="{gw}" height="{gh}" fill="none" stroke="{DIFF_FILL}" stroke-width="3" stroke-dasharray="6 4"/>"#
            );
        }
    }
    out.push_str("</g>");
}

fn document(w: u32, h: u32, title: &str, body: &str) -> String {
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" font-family="{font}" font-size="15" xml:space="preserve"><title>{title}</title><rect width="{w}" height="{h}" fill="{PAGE_BG}"/>{body}</svg>"#,
        font = escape(FONT),
        title = escape(title),
    )
}

/// One grid as SVG (fixed cell size, a fixed default palette and theme).
pub fn svg(grid: &Grid, title: &str) -> String {
    let (w, h) = panel_size(Some(grid));
    let mut body = String::new();
    panel(&mut body, 0, 0, Some(grid), title, None);
    document(w, h, title, &body)
}

/// Herdr | Vibeke | diff overlay. Either side may be missing (shown as "not run").
pub fn side_by_side(
    herdr: Option<&Grid>,
    vibeke: Option<&Grid>,
    diff: Option<&GridDiff>,
    title: &str,
) -> String {
    // The overlay draws the differing cells over the Vibeke grid (Herdr's when Vibeke did not
    // run).
    let base = vibeke.or(herdr);
    let overlay_title = match diff {
        None => "diff: not compared".to_string(),
        Some(d) if d.is_empty() => "diff: identical".to_string(),
        Some(d) => format!(
            "diff: {} cells{}",
            d.cells.len(),
            if d.same_size { "" } else { ", size differs" }
        ),
    };
    let panels: [(Option<&Grid>, String, Option<&GridDiff>); 3] = [
        (herdr, "Herdr 0.9.3".to_string(), None),
        (vibeke, "Vibeke".to_string(), None),
        (diff.and(base), overlay_title, diff),
    ];
    let mut body = String::new();
    let title_h = TITLE_H + 8;
    let _ = write!(
        body,
        r##"<text x="0" y="18" font-size="16" font-weight="bold" fill="#000000">{}</text>"##,
        escape(title)
    );
    let mut x = 0;
    let mut h = 0;
    for (grid, name, d) in &panels {
        let (pw, ph) = panel_size(*grid);
        panel(&mut body, x, title_h, *grid, name, *d);
        x += pw + GAP;
        h = h.max(ph);
    }
    document(x - GAP, title_h + h, title, &body)
}

/// Redact every cell's text with `vk-redact`, row by row, keeping the grid shape.
///
/// A row whose text changes is laid out again from the left with the redacted text; each cell
/// keeps the colors and attributes of the cell that was at its column, and the rest of the row
/// is blank.
pub fn redact(grid: &Grid) -> Grid {
    let mut out = grid.clone();
    for (y, line) in grid.cells.iter().enumerate() {
        let text: String = line
            .iter()
            .filter(|c| !c.cont)
            .map(|c| c.text.as_str())
            .collect();
        let red = vk_redact::redact(&text);
        if red.as_ref() == text.as_str() {
            continue;
        }
        let cols = line.len();
        let styled = |x: usize, t: &str| Cell {
            text: t.into(),
            cont: false,
            ..line[x].clone()
        };
        let new = &mut out.cells[y];
        let mut x = 0usize;
        for g in red.graphemes(true) {
            let w = grapheme_width(g);
            if w == 0 {
                continue;
            }
            if x + w > cols {
                break;
            }
            new[x] = styled(x, g);
            if w == 2 {
                new[x + 1] = Cell {
                    text: String::new(),
                    cont: true,
                    ..new[x].clone()
                };
            }
            x += w;
        }
        while x < cols {
            new[x] = styled(x, " ");
            x += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::compare;

    fn grid(text: &str) -> Grid {
        let mut g = Grid::blank(text.chars().count() as u16, 1);
        for (x, ch) in text.chars().enumerate() {
            g.cells[0][x].text = ch.to_string();
        }
        g
    }

    #[test]
    fn escapes_text_and_title() {
        let s = svg(&grid("<a&b>\"'"), "t<&>");
        assert!(s.contains("&lt;"));
        assert!(s.contains("&amp;"));
        assert!(s.contains("&gt;"));
        assert!(s.contains("&quot;"));
        assert!(s.contains("&#39;"));
        assert!(s.contains("<title>t&lt;&amp;&gt;</title>"));
        assert!(!s.contains("<a&b>"));
        assert_eq!(escape("a\u{1b}b"), "a\u{fffd}b");
    }

    #[test]
    fn palette_matches_xterm() {
        assert_eq!(palette(1), (205, 0, 0));
        assert_eq!(palette(16), (0, 0, 0));
        assert_eq!(palette(196), (255, 0, 0));
        assert_eq!(palette(231), (255, 255, 255));
        assert_eq!(palette(232), (8, 8, 8));
        assert_eq!(palette(255), (238, 238, 238));
    }

    #[test]
    fn side_by_side_marks_diffs_and_missing_sides() {
        let h = grid("abc");
        let v = grid("abd");
        let d = compare(&h, &v);
        let s = side_by_side(Some(&h), Some(&v), Some(&d), "step 1");
        assert!(s.contains("Herdr 0.9.3"));
        assert!(s.contains("diff: 1 cells"));
        assert_eq!(s.matches(DIFF_FILL).count(), 2);
        let s = side_by_side(None, Some(&v), None, "step 2");
        assert!(s.contains("not run"));
        assert!(s.contains("diff: not compared"));
    }

    #[test]
    fn redact_keeps_shape() {
        let secret = "token=ghp_0123456789abcdefghijklmnopqrstuvwxyzAB";
        let g = grid(&format!("{secret}   "));
        let r = redact(&g);
        assert_eq!((r.cols, r.rows), (g.cols, g.rows));
        assert_eq!(r.cells[0].len(), g.cells[0].len());
        assert!(!r.text().contains("ghp_0123456789"));
        let plain = grid("hello");
        assert_eq!(redact(&plain), plain);
    }
}
