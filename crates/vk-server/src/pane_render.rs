//! PNG and SVG renderings of a pane grid for `pane.screenshot {format: png|svg}` (07 §2.6).
//!
//! Both draw the rows the VT engine returns (`vk_proto::render::Row`, the same styled spans the
//! TUI composes) on a fixed cell grid of [`CELL_W`] x [`CELL_H`] units with [`PAD`] around it,
//! in the pane's palette (the theme's default colours: catppuccin mocha, or latte when the
//! clients report a light terminal; colours 16–255 are the xterm cube and grays).
//!
//! - **SVG** is text: a background rectangle per non-default background run and one `<text>`
//!   per span, stretched to its column count (`textLength`) so columns line up whatever
//!   monospace font the viewer picks. Bold, italic, dim, underline, strikethrough, inverse and
//!   hidden map to SVG attributes.
//! - **PNG** is rasterized here with the `font8x8` bitmap font (ASCII, Latin-1, box drawing,
//!   block elements, Greek, Hiragana; each 8x8 glyph is drawn 8 px wide and 16 px tall, wide
//!   characters are centred in their two cells, anything else is a hollow box) and encoded with
//!   `image`'s PNG encoder. No system font is read, so the output is the same on every machine.

use font8x8::UnicodeFonts;
use std::fmt::Write as _;
use unicode_width::UnicodeWidthChar;
use vk_proto::render::{Color, Row, Style, attr};

/// Cell width in pixels (PNG) or user units (SVG).
pub const CELL_W: u32 = 8;
/// Cell height in pixels (PNG) or user units (SVG).
pub const CELL_H: u32 = 16;
/// Margin around the grid.
pub const PAD: u32 = 8;
/// Largest PNG this module renders (pixels); larger captures are refused.
pub const MAX_PIXELS: u64 = 24_000_000;

/// Colours used to resolve a style: default foreground/background and the 16 ANSI colours.
#[derive(Debug, Clone, Copy)]
pub struct Colors {
    pub fg: (u8, u8, u8),
    pub bg: (u8, u8, u8),
    pub cursor: (u8, u8, u8),
    pub ansi: [(u8, u8, u8); 16],
}

impl Colors {
    pub fn from_palette(p: &vk_term::engine::Palette) -> Self {
        Colors {
            fg: p.fg,
            bg: p.bg,
            cursor: p.cursor,
            ansi: p.ansi,
        }
    }
}

impl Default for Colors {
    fn default() -> Self {
        Self::from_palette(&vk_term::engine::Palette::default())
    }
}

/// What to draw: the rows, the grid width, and the cursor cell (row index into `rows`, column).
pub struct Grid<'a> {
    pub rows: &'a [Row],
    pub cols: u16,
    pub cursor: Option<(usize, u16)>,
}

type Rgb = (u8, u8, u8);

/// xterm-256 colour `n` (16–255; 0–15 come from [`Colors::ansi`]).
fn xterm(n: u8) -> Rgb {
    match n {
        16..=231 => {
            let i = n - 16;
            let step = |x: u8| if x == 0 { 0 } else { 55 + 40 * x };
            (step(i / 36), step((i / 6) % 6), step(i % 6))
        }
        232..=255 => {
            let g = 8 + 10 * (n - 232);
            (g, g, g)
        }
        _ => (0, 0, 0),
    }
}

fn blend(a: Rgb, b: Rgb) -> Rgb {
    let m = |x: u8, y: u8| ((x as u16 + y as u16) / 2) as u8;
    (m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

/// The resolved colours of one style: (foreground, background, background is the default).
fn resolve(st: &Style, c: &Colors) -> (Rgb, Rgb, bool) {
    let bold = st.attrs & attr::BOLD != 0;
    let pick = |col: Color, default: Rgb, fg: bool| match col {
        Color::Default => default,
        // Bold brightens the eight basic colours, as most terminals do.
        Color::Indexed(n) if n < 8 && fg && bold => c.ansi[n as usize + 8],
        Color::Indexed(n) if n < 16 => c.ansi[n as usize],
        Color::Indexed(n) => xterm(n),
        Color::Rgb(r, g, b) => (r, g, b),
    };
    let mut fg = pick(st.fg, c.fg, true);
    let mut bg = pick(st.bg, c.bg, false);
    let mut default_bg = st.bg == Color::Default;
    if st.attrs & attr::INVERSE != 0 {
        std::mem::swap(&mut fg, &mut bg);
        default_bg = false;
    }
    if st.attrs & attr::DIM != 0 {
        fg = blend(fg, bg);
    }
    if st.attrs & attr::HIDDEN != 0 {
        fg = bg;
    }
    (fg, bg, default_bg)
}

fn hex(c: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2)
}

/// Each character of a span with its starting column and width (zero-width characters are
/// dropped; the span's own column count is authoritative for where the next span starts).
fn cells(text: &str) -> Vec<(char, u16)> {
    text.chars()
        .filter_map(|ch| {
            let w = ch.width().unwrap_or(0) as u16;
            (w > 0).then_some((ch, w))
        })
        .collect()
}

fn esc_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            // Not allowed in XML 1.0.
            c if (c as u32) < 0x20 && c != '\t' => out.push(' '),
            '\u{fffe}' | '\u{ffff}' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Canvas size for a grid: (width, height).
pub fn size(g: &Grid) -> (u32, u32) {
    (
        g.cols.max(1) as u32 * CELL_W + 2 * PAD,
        g.rows.len().max(1) as u32 * CELL_H + 2 * PAD,
    )
}

/// The grid as a standalone SVG document.
pub fn svg(g: &Grid, colors: &Colors, title: &str) -> String {
    let (w, h) = size(g);
    let mut bgs = String::new();
    let mut texts = String::new();
    for (ri, row) in g.rows.iter().enumerate() {
        let y = PAD + ri as u32 * CELL_H;
        let mut col: u32 = 0;
        for sp in &row.spans {
            let (fg, bg, default_bg) = resolve(&sp.style, colors);
            let x = PAD + col * CELL_W;
            let width = sp.cols as u32 * CELL_W;
            if !default_bg && sp.cols > 0 {
                let _ = write!(
                    bgs,
                    "<rect x=\"{x}\" y=\"{y}\" width=\"{width}\" height=\"{CELL_H}\" fill=\"{}\"/>",
                    hex(bg)
                );
            }
            let st = sp.style.attrs;
            let visible = st & attr::HIDDEN == 0 && !sp.text.trim().is_empty();
            if visible && sp.cols > 0 {
                let mut a = format!("fill=\"{}\"", hex(fg));
                if st & attr::BOLD != 0 {
                    a.push_str(" font-weight=\"bold\"");
                }
                if st & attr::ITALIC != 0 {
                    a.push_str(" font-style=\"italic\"");
                }
                let mut deco = vec![];
                if st & attr::ANY_UNDERLINE != 0 {
                    deco.push("underline");
                }
                if st & attr::STRIKE != 0 {
                    deco.push("line-through");
                }
                if !deco.is_empty() {
                    let _ = write!(a, " text-decoration=\"{}\"", deco.join(" "));
                }
                let _ = write!(
                    texts,
                    "<text x=\"{x}\" y=\"{}\" textLength=\"{width}\" lengthAdjust=\"spacingAndGlyphs\" {a}>{}</text>",
                    y + CELL_H - 4,
                    esc_xml(&sp.text)
                );
            }
            col += sp.cols as u32;
        }
    }
    let mut cursor = String::new();
    if let Some((r, c)) = g.cursor
        && r < g.rows.len()
    {
        let _ = write!(
            cursor,
            "<rect x=\"{}\" y=\"{}\" width=\"{CELL_W}\" height=\"{CELL_H}\" fill=\"none\" stroke=\"{}\" stroke-width=\"1\"/>",
            PAD + c as u32 * CELL_W,
            PAD + r as u32 * CELL_H,
            hex(colors.cursor)
        );
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\">\n<title>{}</title>\n<rect width=\"100%\" height=\"100%\" fill=\"{}\"/>\n<g>{bgs}</g>\n<g font-family=\"ui-monospace,Menlo,Consolas,'DejaVu Sans Mono',monospace\" font-size=\"13\" xml:space=\"preserve\">{texts}</g>\n{cursor}</svg>\n",
        esc_xml(title),
        hex(colors.bg)
    )
}

/// The 8x8 bitmap of `ch` (row bytes, least significant bit = leftmost pixel), if the font
/// has one.
pub fn glyph(ch: char) -> Option<[u8; 8]> {
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

struct Canvas {
    w: u32,
    h: u32,
    px: Vec<u8>,
}

impl Canvas {
    fn new(w: u32, h: u32, bg: Rgb) -> Self {
        let mut px = Vec::with_capacity((w * h * 3) as usize);
        for _ in 0..w * h {
            px.extend_from_slice(&[bg.0, bg.1, bg.2]);
        }
        Canvas { w, h, px }
    }
    fn set(&mut self, x: u32, y: u32, c: Rgb) {
        if x < self.w && y < self.h {
            let i = ((y * self.w + x) * 3) as usize;
            self.px[i..i + 3].copy_from_slice(&[c.0, c.1, c.2]);
        }
    }
    fn fill(&mut self, x: u32, y: u32, w: u32, h: u32, c: Rgb) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.set(xx, yy, c);
            }
        }
    }
    /// One glyph at cell origin (x, y), `cells` wide (wide characters are centred).
    fn glyph(&mut self, x: u32, y: u32, cells: u16, bits: [u8; 8], fg: Rgb, bold: bool) {
        let x0 = x + (cells.saturating_sub(1) as u32 * CELL_W) / 2;
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
}

/// The grid rasterized and encoded as PNG. Refuses canvases over [`MAX_PIXELS`].
pub fn png(g: &Grid, colors: &Colors) -> Result<Vec<u8>, String> {
    let (w, h) = size(g);
    if w as u64 * h as u64 > MAX_PIXELS {
        return Err(format!(
            "the capture would be {w}x{h} pixels (limit {MAX_PIXELS}); ask for fewer lines"
        ));
    }
    let mut cv = Canvas::new(w, h, colors.bg);
    for (ri, row) in g.rows.iter().enumerate() {
        let y = PAD + ri as u32 * CELL_H;
        let mut col: u32 = 0;
        for sp in &row.spans {
            let (mut fg, mut bg, default_bg) = resolve(&sp.style, colors);
            let st = sp.style.attrs;
            let span_x = PAD + col * CELL_W;
            if !default_bg {
                cv.fill(span_x, y, sp.cols as u32 * CELL_W, CELL_H, bg);
            }
            let mut c = col;
            for (ch, cw) in cells(&sp.text) {
                if c >= col + sp.cols as u32 {
                    break;
                }
                let x = PAD + c * CELL_W;
                let at_cursor = g.cursor.is_some_and(|(cr, cc)| cr == ri && cc as u32 == c);
                if at_cursor {
                    std::mem::swap(&mut fg, &mut bg);
                    cv.fill(x, y, cw as u32 * CELL_W, CELL_H, bg);
                }
                if st & attr::HIDDEN == 0 && ch != ' ' {
                    let bits = glyph(ch).unwrap_or(MISSING);
                    cv.glyph(x, y, cw, bits, fg, st & attr::BOLD != 0);
                }
                if st & attr::ANY_UNDERLINE != 0 {
                    cv.fill(x, y + CELL_H - 2, cw as u32 * CELL_W, 1, fg);
                    if st & attr::DOUBLE_UNDERLINE != 0 {
                        cv.fill(x, y + CELL_H - 4, cw as u32 * CELL_W, 1, fg);
                    }
                }
                if st & attr::STRIKE != 0 {
                    cv.fill(x, y + CELL_H / 2, cw as u32 * CELL_W, 1, fg);
                }
                if at_cursor {
                    std::mem::swap(&mut fg, &mut bg);
                }
                c += cw as u32;
            }
            col += sp.cols as u32;
        }
    }
    // A cursor on a blank cell past the text.
    if let Some((r, c)) = g.cursor
        && r < g.rows.len()
    {
        let used: u32 = g.rows[r].spans.iter().map(|s| s.cols as u32).sum();
        if c as u32 >= used {
            cv.fill(
                PAD + c as u32 * CELL_W,
                PAD + r as u32 * CELL_H,
                CELL_W,
                CELL_H,
                colors.cursor,
            );
        }
    }
    let mut out = Vec::new();
    {
        use image::ImageEncoder as _;
        image::codecs::png::PngEncoder::new(&mut out)
            .write_image(&cv.px, w, h, image::ExtendedColorType::Rgb8)
            .map_err(|e| e.to_string())?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vk_proto::render::Span;

    fn span(text: &str, style: Style) -> Span {
        Span {
            style,
            text: text.into(),
            cols: text.chars().map(|c| c.width().unwrap_or(0) as u16).sum(),
        }
    }

    fn rows() -> Vec<Row> {
        let red_bold = Style {
            fg: Color::Indexed(1),
            attrs: attr::BOLD,
            ..Default::default()
        };
        let blue_bg = Style {
            bg: Color::Rgb(0, 0, 200),
            attrs: attr::UNDERLINE,
            ..Default::default()
        };
        vec![
            Row::new(
                vec![span("ok ", Style::default()), span("ERR <&>", red_bold)],
                false,
            ),
            Row::new(vec![span("└─ 日本", blue_bg)], false),
        ]
    }

    #[test]
    fn svg_has_spans_backgrounds_and_escapes() {
        let rows = rows();
        let g = Grid {
            rows: &rows,
            cols: 12,
            cursor: Some((0, 3)),
        };
        let c = Colors::default();
        let s = svg(&g, &c, "w1:p1 <x>");
        assert!(s.starts_with("<?xml"));
        assert!(s.contains("<title>w1:p1 &lt;x&gt;</title>"));
        // 12 cols * 8 + 16, 2 rows * 16 + 16.
        assert!(s.contains("width=\"112\" height=\"48\""), "{s}");
        // Bold red brightens to ANSI 9.
        let bright_red = hex(c.ansi[9]);
        assert!(
            s.contains(&format!(
                "fill=\"{bright_red}\" font-weight=\"bold\">ERR &lt;&amp;&gt;</text>"
            )),
            "{s}"
        );
        assert!(s.contains("fill=\"#0000c8\""), "background run");
        assert!(s.contains("text-decoration=\"underline\""));
        // The span stretched to its 7 columns (two of them the wide 日 and 本... counted 2 each).
        assert!(s.contains("textLength=\"56\""), "{s}");
        assert!(s.contains("stroke="), "cursor outline");
    }

    #[test]
    fn png_renders_glyphs_in_the_palette() {
        let rows = rows();
        let g = Grid {
            rows: &rows,
            cols: 12,
            cursor: None,
        };
        let c = Colors::default();
        let bytes = png(&g, &c).unwrap();
        assert_eq!(&bytes[1..4], b"PNG");
        let img = image::load_from_memory(&bytes).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (112, 48));
        // The margin is the default background.
        assert_eq!(img.get_pixel(0, 0).0, [c.bg.0, c.bg.1, c.bg.2]);
        // Some pixel of the "o" in the first cell is the default foreground.
        let cell0 = (PAD..PAD + CELL_W)
            .flat_map(|x| (PAD..PAD + CELL_H).map(move |y| (x, y)))
            .any(|(x, y)| img.get_pixel(x, y).0 == [c.fg.0, c.fg.1, c.fg.2]);
        assert!(cell0, "glyph pixels drawn");
        // Row 2 has a blue background under its text.
        assert_eq!(img.get_pixel(PAD, PAD + CELL_H).0, [0, 0, 200]);
        assert!(glyph('A').is_some() && glyph('─').is_some() && glyph('█').is_some());
        assert!(glyph('日').is_none(), "drawn as a box");
    }

    #[test]
    fn png_refuses_huge_canvases() {
        let rows = vec![Row::default(); 20_000];
        let g = Grid {
            rows: &rows,
            cols: 400,
            cursor: None,
        };
        assert!(png(&g, &Colors::default()).is_err());
    }
}
