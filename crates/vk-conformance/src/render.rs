//! One renderer for both sides (18 §10.5), so fonts cannot create false differences: Herdr,
//! Vibeke and a diff overlay side by side, as one SVG, plus a small HTML index per run. Text
//! is escaped; grids are redacted before they reach this module.
//!
//! Fixed choices: a 9×18 pixel cell, the [`FONT`] family, the standard xterm 256-colour
//! palette (the same table as `palette` in `crates/vk-server/src/pane_api.rs`) and a fixed
//! default foreground and background.
//!
//! The same panels also render as PNG ([`png`], [`side_by_side_png`]), with the built-in
//! `font8x8` bitmap font on an 8×16 pixel cell, for CI run pages that show images but not SVG.

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

/// One panel: the grid it shows (`None`: "not run"), its title and the diff it overlays.
type PanelSpec<'a> = (Option<&'a Grid>, String, Option<&'a GridDiff>);

/// The Herdr, Vibeke and diff panels, for both the SVG and the PNG renderer.
fn panel_specs<'a>(
    herdr: Option<&'a Grid>,
    vibeke: Option<&'a Grid>,
    diff: Option<&'a GridDiff>,
) -> [PanelSpec<'a>; 3] {
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
    [
        (herdr, "Herdr 0.9.3".to_string(), None),
        (vibeke, "Vibeke".to_string(), None),
        (diff.and(base), overlay_title, diff),
    ]
}

/// Herdr | Vibeke | diff overlay. Either side may be missing (shown as "not run").
pub fn side_by_side(
    herdr: Option<&Grid>,
    vibeke: Option<&Grid>,
    diff: Option<&GridDiff>,
    title: &str,
) -> String {
    let panels = panel_specs(herdr, vibeke, diff);
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

// PNG rendering. The same panels as the SVG, rasterized here with the `font8x8` bitmap font
// and encoded with `image`'s PNG encoder, as `crates/vk-server/src/pane_render.rs::png` does.
// No system font is read, so CI renders the same pixels on every runner.

/// PNG cell width in pixels.
pub const PX_CELL_W: u32 = 8;
/// PNG cell height in pixels: each 8x8 glyph is drawn 8 px wide and 16 px tall.
pub const PX_CELL_H: u32 = 16;
/// Height of a title line in a PNG.
pub const PX_LINE_H: u32 = 20;
const PX_GAP: u32 = 16;
const PX_PAD: u32 = 8;
/// The widest image [`side_by_side_png`] lays its panels out in a row for; wider panels are
/// stacked instead.
pub const PX_MAX_W: u32 = 4096;

type Rgb = (u8, u8, u8);

const PAGE_RGB: Rgb = (255, 255, 255);
const TEXT_RGB: Rgb = (0, 0, 0);
const DIFF_RGB: Rgb = (0xff, 0x2d, 0x55);
/// The diff fill's opacity, 0.45 as in the SVG.
const DIFF_ALPHA: u8 = 115;
const MUTED_FILL: Rgb = (0xee, 0xee, 0xee);
const MUTED_STROKE: Rgb = (0x99, 0x99, 0x99);
const MUTED_TEXT: Rgb = (0x55, 0x55, 0x55);

/// The 8x8 bitmap of `ch` (row bytes, least significant bit = leftmost pixel), if the font has
/// one.
fn glyph(ch: char) -> Option<[u8; 8]> {
    use font8x8::UnicodeFonts as _;
    font8x8::BASIC_FONTS
        .get(ch)
        .or_else(|| font8x8::LATIN_FONTS.get(ch))
        .or_else(|| font8x8::BOX_FONTS.get(ch))
        .or_else(|| font8x8::BLOCK_FONTS.get(ch))
        .or_else(|| font8x8::GREEK_FONTS.get(ch))
        .or_else(|| font8x8::HIRAGANA_FONTS.get(ch))
        .or_else(|| font8x8::MISC_FONTS.get(ch))
}

/// A hollow box for characters the font lacks.
const MISSING: [u8; 8] = [0x00, 0x7e, 0x42, 0x42, 0x42, 0x42, 0x7e, 0x00];

/// Display width of a character in cells: 0 for control and combining characters.
fn char_cells(ch: char) -> u32 {
    if ch.is_control() {
        return 0;
    }
    unicode_width::UnicodeWidthChar::width(ch)
        .unwrap_or(0)
        .min(2) as u32
}

/// Width of a line of text in pixels.
pub fn text_px_width(s: &str) -> u32 {
    s.chars().map(char_cells).sum::<u32>() * PX_CELL_W
}

/// `s` without control characters, cut to at most `max_px` pixels wide (ending in `...` when
/// cut).
pub fn fit(s: &str, max_px: u32) -> String {
    let clean: String = s
        .chars()
        .map(|c| if c.is_control() { '\u{fffd}' } else { c })
        .collect();
    if text_px_width(&clean) <= max_px {
        return clean;
    }
    let budget = max_px.saturating_sub(3 * PX_CELL_W);
    let mut out = String::new();
    let mut w = 0;
    for c in clean.chars() {
        let cw = char_cells(c) * PX_CELL_W;
        if w + cw > budget {
            break;
        }
        w += cw;
        out.push(c);
    }
    out.push_str("...");
    out
}

/// An RGB canvas with a white page background. Drawing outside it is clipped.
pub struct Canvas {
    w: u32,
    h: u32,
    px: Vec<u8>,
}

impl Canvas {
    /// A white canvas, at least 1x1.
    pub fn new(w: u32, h: u32) -> Canvas {
        let (w, h) = (w.max(1), h.max(1));
        Canvas {
            w,
            h,
            px: [PAGE_RGB.0, PAGE_RGB.1, PAGE_RGB.2].repeat(w as usize * h as usize),
        }
    }

    pub fn width(&self) -> u32 {
        self.w
    }

    pub fn height(&self) -> u32 {
        self.h
    }

    fn index(&self, x: u32, y: u32) -> Option<usize> {
        (x < self.w && y < self.h).then(|| (y as usize * self.w as usize + x as usize) * 3)
    }

    fn set(&mut self, x: u32, y: u32, c: Rgb) {
        if let Some(i) = self.index(x, y) {
            self.px[i..i + 3].copy_from_slice(&[c.0, c.1, c.2]);
        }
    }

    fn fill(&mut self, x: u32, y: u32, w: u32, h: u32, c: Rgb) {
        for yy in y..y.saturating_add(h).min(self.h) {
            for xx in x..x.saturating_add(w).min(self.w) {
                self.set(xx, yy, c);
            }
        }
    }

    /// Mixes `c` into a rectangle with opacity `alpha` (0-255).
    fn tint(&mut self, x: u32, y: u32, w: u32, h: u32, c: Rgb, alpha: u8) {
        let a = alpha as u16;
        let mix = |new: u8, old: u8| ((new as u16 * a + old as u16 * (255 - a)) / 255) as u8;
        for yy in y..y.saturating_add(h).min(self.h) {
            for xx in x..x.saturating_add(w).min(self.w) {
                if let Some(i) = self.index(xx, yy) {
                    self.px[i] = mix(c.0, self.px[i]);
                    self.px[i + 1] = mix(c.1, self.px[i + 1]);
                    self.px[i + 2] = mix(c.2, self.px[i + 2]);
                }
            }
        }
    }

    /// A rectangle outline `t` pixels thick, inside the rectangle.
    fn stroke(&mut self, x: u32, y: u32, w: u32, h: u32, c: Rgb, t: u32) {
        let t = t.min(w).min(h);
        self.fill(x, y, w, t, c);
        self.fill(x, y + h - t, w, t, c);
        self.fill(x, y, t, h, c);
        self.fill(x + w - t, y, t, h, c);
    }

    /// A dashed rectangle outline: 6 pixels on, 4 off, as the SVG's `stroke-dasharray`.
    fn dashed(&mut self, x: u32, y: u32, w: u32, h: u32, c: Rgb, t: u32) {
        let t = t.min(w).min(h);
        for i in (0..w).step_by(10) {
            let len = 6.min(w - i);
            self.fill(x + i, y, len, t, c);
            self.fill(x + i, y + h - t, len, t, c);
        }
        for i in (0..h).step_by(10) {
            let len = 6.min(h - i);
            self.fill(x, y + i, t, len, c);
            self.fill(x + w - t, y + i, t, len, c);
        }
    }

    /// One glyph at cell origin (x, y), `cells` wide (wide characters are centred).
    fn glyph(&mut self, x: u32, y: u32, cells: u32, bits: [u8; 8], fg: Rgb, bold: bool) {
        let x0 = x + (cells.saturating_sub(1) * PX_CELL_W) / 2;
        for (r, row) in bits.iter().enumerate() {
            for b in 0..8u32 {
                if row & (1 << b) != 0 {
                    for dy in 0..2 {
                        let yy = y + r as u32 * 2 + dy;
                        self.set(x0 + b, yy, fg);
                        if bold {
                            self.set(x0 + b + 1, yy, fg);
                        }
                    }
                }
            }
        }
    }

    /// One line of text with its top left at (x, y); returns its width in pixels.
    pub fn text(&mut self, x: u32, y: u32, s: &str, c: Rgb, bold: bool) -> u32 {
        let mut adv = 0;
        for ch in s.chars() {
            let cells = char_cells(ch);
            if cells == 0 {
                continue;
            }
            if !ch.is_whitespace() {
                let bits = glyph(ch).unwrap_or(MISSING);
                self.glyph(x + adv, y, cells, bits, c, bold);
            }
            adv += cells * PX_CELL_W;
        }
        adv
    }

    /// The canvas as a PNG file.
    pub fn encode(&self) -> Vec<u8> {
        use image::ImageEncoder as _;
        let mut out = Vec::new();
        image::codecs::png::PngEncoder::new(&mut out)
            .write_image(&self.px, self.w, self.h, image::ExtendedColorType::Rgb8)
            .expect("an RGB buffer of the canvas size encodes");
        out
    }
}

/// Draws the cells of `g` with their top left at (x0, y0).
fn draw_grid(cv: &mut Canvas, x0: u32, y0: u32, g: &Grid) {
    let (cols, rows) = (g.cols as usize, g.rows as usize);
    cv.fill(
        x0,
        y0,
        g.cols as u32 * PX_CELL_W,
        g.rows as u32 * PX_CELL_H,
        DEFAULT_BG,
    );
    for (row, line) in g.cells.iter().take(rows).enumerate() {
        let cy = y0 + row as u32 * PX_CELL_H;
        for (col, cell) in line.iter().take(cols).enumerate() {
            if cell.cont {
                continue;
            }
            let wide = col + 1 < cols && line.get(col + 1).is_some_and(|c| c.cont);
            let cells = if wide { 2 } else { 1 };
            let cx = x0 + col as u32 * PX_CELL_W;
            let cw = cells * PX_CELL_W;
            let (fg, bg) = cell_colors(cell);
            if bg != DEFAULT_BG {
                cv.fill(cx, cy, cw, PX_CELL_H, bg);
            }
            if let Some(ch) = cell.text.chars().next()
                && !cell.text.trim().is_empty()
            {
                let bits = glyph(ch).unwrap_or(MISSING);
                cv.glyph(cx, cy, cells, bits, fg, cell.attrs.bold);
            }
            if cell.attrs.underline {
                cv.fill(cx, cy + PX_CELL_H - 2, cw, 1, fg);
            }
        }
    }
}

fn panel_px_size(grid: Option<&Grid>) -> (u32, u32) {
    match grid {
        Some(g) => (
            (g.cols as u32 * PX_CELL_W).max(160),
            PX_LINE_H + g.rows as u32 * PX_CELL_H,
        ),
        None => (240, PX_LINE_H + 4 * PX_CELL_H),
    }
}

/// One PNG panel at (`x`, `y`): a title, then the grid or "not run", then an optional diff
/// overlay of highlighted cells.
fn draw_panel(cv: &mut Canvas, x: u32, y: u32, spec: &PanelSpec<'_>) {
    let (grid, title, diff) = spec;
    let (w, h) = panel_px_size(*grid);
    cv.text(x, y + 2, &fit(title, w), TEXT_RGB, false);
    let top = y + PX_LINE_H;
    let Some(g) = grid else {
        cv.fill(x, top, w, h - PX_LINE_H, MUTED_FILL);
        cv.stroke(x, top, w, h - PX_LINE_H, MUTED_STROKE, 1);
        let label = "not run";
        let tx = x + (w - text_px_width(label)) / 2;
        let ty = top + (h - PX_LINE_H - PX_CELL_H) / 2;
        cv.text(tx, ty, label, MUTED_TEXT, false);
        return;
    };
    draw_grid(cv, x, top, g);
    if let Some(d) = diff {
        for &(row, col, _) in &d.cells {
            if row >= g.rows || col >= g.cols {
                continue;
            }
            let (cx, cy) = (x + col as u32 * PX_CELL_W, top + row as u32 * PX_CELL_H);
            cv.tint(cx, cy, PX_CELL_W, PX_CELL_H, DIFF_RGB, DIFF_ALPHA);
            cv.stroke(cx, cy, PX_CELL_W, PX_CELL_H, DIFF_RGB, 1);
        }
        if !d.same_size {
            let (gw, gh) = (g.cols as u32 * PX_CELL_W, g.rows as u32 * PX_CELL_H);
            cv.dashed(x, top, gw, gh, DIFF_RGB, 3);
        }
    }
}

/// Where the three panels go, relative to the block's top left, and the block's size. The
/// panels sit in a row, or one above the other when a row would be wider than `max_w`.
fn layout(specs: &[PanelSpec<'_>; 3], max_w: u32) -> ([(u32, u32); 3], u32, u32) {
    let sizes = specs.each_ref().map(|(g, _, _)| panel_px_size(*g));
    let row_w = sizes.iter().map(|s| s.0).sum::<u32>() + 2 * PX_GAP;
    let mut at = [(0, 0); 3];
    if row_w <= max_w {
        let mut x = 0;
        for (i, (w, _)) in sizes.iter().enumerate() {
            at[i] = (x, 0);
            x += w + PX_GAP;
        }
        let h = sizes.iter().map(|s| s.1).max().unwrap_or(0);
        (at, row_w, h)
    } else {
        let mut y = 0;
        for (i, (_, h)) in sizes.iter().enumerate() {
            at[i] = (0, y);
            y += h + PX_GAP;
        }
        let w = sizes.iter().map(|s| s.0).max().unwrap_or(0);
        (at, w, y - PX_GAP)
    }
}

/// The size in pixels of a Herdr | Vibeke | diff block no wider than `max_w` where possible
/// (see [`draw_side_by_side`]).
pub fn side_by_side_px_size(
    herdr: Option<&Grid>,
    vibeke: Option<&Grid>,
    diff: Option<&GridDiff>,
    max_w: u32,
) -> (u32, u32) {
    let (_, w, h) = layout(&panel_specs(herdr, vibeke, diff), max_w);
    (w, h)
}

/// Draws Herdr | Vibeke | diff overlay with its top left at (x, y). The panels are stacked
/// when a row of them would be wider than `max_w`.
pub fn draw_side_by_side(
    cv: &mut Canvas,
    x: u32,
    y: u32,
    herdr: Option<&Grid>,
    vibeke: Option<&Grid>,
    diff: Option<&GridDiff>,
    max_w: u32,
) {
    let specs = panel_specs(herdr, vibeke, diff);
    let (at, _, _) = layout(&specs, max_w);
    for (spec, (dx, dy)) in specs.iter().zip(at) {
        draw_panel(cv, x + dx, y + dy, spec);
    }
}

/// One grid as PNG: [`PX_CELL_W`] x [`PX_CELL_H`] pixel cells, the same fixed palette as
/// [`svg`], no title.
pub fn png(grid: &Grid) -> Vec<u8> {
    let mut cv = Canvas::new(grid.cols as u32 * PX_CELL_W, grid.rows as u32 * PX_CELL_H);
    draw_grid(&mut cv, 0, 0, grid);
    cv.encode()
}

/// Herdr | Vibeke | diff overlay as PNG, under a title. Either side may be missing (shown as
/// "not run").
pub fn side_by_side_png(
    herdr: Option<&Grid>,
    vibeke: Option<&Grid>,
    diff: Option<&GridDiff>,
    title: &str,
) -> Vec<u8> {
    let inner = PX_MAX_W - 2 * PX_PAD;
    let title = fit(title, inner);
    let (bw, bh) = side_by_side_px_size(herdr, vibeke, diff, inner);
    let mut cv = Canvas::new(
        bw.max(text_px_width(&title)) + 2 * PX_PAD,
        PX_PAD + PX_LINE_H + 8 + bh + PX_PAD,
    );
    cv.text(PX_PAD, PX_PAD + 2, &title, TEXT_RGB, true);
    draw_side_by_side(
        &mut cv,
        PX_PAD,
        PX_PAD + PX_LINE_H + 8,
        herdr,
        vibeke,
        diff,
        inner,
    );
    cv.encode()
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

    fn decode(png: &[u8]) -> image::RgbImage {
        assert_eq!(&png[1..4], b"PNG");
        image::load_from_memory(png).unwrap().to_rgb8()
    }

    #[test]
    fn png_draws_cells_in_the_fixed_palette() {
        let mut g = grid("A b");
        g.cells[0][2].bg = Color::Indexed(4);
        let img = decode(&png(&g));
        assert_eq!(img.dimensions(), (3 * PX_CELL_W, PX_CELL_H));
        let has = |x0: u32, c: (u8, u8, u8)| {
            (x0..x0 + PX_CELL_W)
                .flat_map(|x| (0..PX_CELL_H).map(move |y| (x, y)))
                .any(|(x, y)| img.get_pixel(x, y).0 == [c.0, c.1, c.2])
        };
        assert!(has(0, DEFAULT_FG), "the A is drawn");
        assert!(!has(PX_CELL_W, DEFAULT_FG), "a blank cell has no glyph");
        assert_eq!(img.get_pixel(PX_CELL_W, 0).0, [0, 0, 0]);
        // The bottom right pixel of the "b" cell is outside its glyph.
        let blue = palette(4);
        let p = img.get_pixel(3 * PX_CELL_W - 1, PX_CELL_H - 1).0;
        assert_eq!(p, [blue.0, blue.1, blue.2]);
    }

    #[test]
    fn side_by_side_png_marks_diffs_and_stacks_wide_panels() {
        let h = grid("abc");
        let v = grid("abd");
        let d = compare(&h, &v);
        let img = decode(&side_by_side_png(Some(&h), Some(&v), Some(&d), "step 1"));
        // Three 160 px panels (the minimum width) in a row.
        assert_eq!(img.width(), 3 * 160 + 2 * PX_GAP + 2 * PX_PAD);
        assert_eq!(
            img.height(),
            PX_PAD + PX_LINE_H + 8 + PX_LINE_H + PX_CELL_H + PX_PAD
        );
        // The differing cell (column 2) of the diff panel is tinted.
        // A pixel of that cell outside the "d" glyph and its 1 px outline: black, tinted.
        let x = PX_PAD + 2 * (160 + PX_GAP) + 2 * PX_CELL_W + 3;
        let y = PX_PAD + PX_LINE_H + 8 + PX_LINE_H + PX_CELL_H - 2;
        let p = img.get_pixel(x, y).0;
        assert!(p[0] > 100 && p[1] < 100, "tinted pixel {p:?}");

        // A missing side is a "not run" panel.
        let img = decode(&side_by_side_png(None, Some(&v), None, "step 2"));
        assert_eq!(img.width(), 240 + 160 + 240 + 2 * PX_GAP + 2 * PX_PAD);

        // Panels wider than the limit are stacked.
        let wide = Grid::blank(300, 2);
        let (w, h) = side_by_side_px_size(Some(&wide), Some(&wide), None, PX_MAX_W);
        assert_eq!(w, 300 * PX_CELL_W);
        assert_eq!(
            h,
            2 * (PX_LINE_H + 2 * PX_CELL_H) + (PX_LINE_H + 4 * PX_CELL_H) + 2 * PX_GAP
        );
    }

    #[test]
    fn fit_cuts_and_cleans_labels() {
        assert_eq!(fit("abc", 100), "abc");
        assert_eq!(fit("a\u{1b}b", 100), "a\u{fffd}b");
        let cut = fit(&"x".repeat(100), 10 * PX_CELL_W);
        assert_eq!(cut, format!("{}...", "x".repeat(7)));
        assert_eq!(text_px_width(&cut), 10 * PX_CELL_W);
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
