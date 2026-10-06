//! Frame decoding and tile diffing.
//!
//! A browser frame is split into a fixed grid of tiles; each tile is hashed and only tiles whose
//! hash changed since the previous frame are re-sent. For the browser pane tiles are
//! **cell-aligned** (`tile_cols × tile_rows` cells), so each tile can be its own kitty image with
//! its own unicode-placeholder placement and clip correctly at pane edges.

use anyhow::{Context, Result};

/// An 8-bit RGBA image, rows top to bottom, no padding.
#[derive(Clone, PartialEq, Eq)]
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl std::fmt::Debug for Rgba {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Rgba({}x{})", self.width, self.height)
    }
}

impl Rgba {
    pub fn new(width: u32, height: u32) -> Rgba {
        Rgba {
            width,
            height,
            data: vec![0; width as usize * height as usize * 4],
        }
    }

    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = (y as usize * self.width as usize + x as usize) * 4;
        [
            self.data[i],
            self.data[i + 1],
            self.data[i + 2],
            self.data[i + 3],
        ]
    }

    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, px: [u8; 4]) {
        for yy in y..(y + h).min(self.height) {
            for xx in x..(x + w).min(self.width) {
                let i = (yy as usize * self.width as usize + xx as usize) * 4;
                self.data[i..i + 4].copy_from_slice(&px);
            }
        }
    }

    /// Mean RGB over a rectangle (clamped to the image).
    pub fn mean_rgb(&self, x: u32, y: u32, w: u32, h: u32) -> [f32; 3] {
        let mut sum = [0f64; 3];
        let mut n = 0f64;
        for yy in y..(y + h).min(self.height) {
            for xx in x..(x + w).min(self.width) {
                let p = self.pixel(xx, yy);
                for c in 0..3 {
                    sum[c] += p[c] as f64;
                }
                n += 1.0;
            }
        }
        if n == 0.0 {
            return [0.0; 3];
        }
        [
            (sum[0] / n) as f32,
            (sum[1] / n) as f32,
            (sum[2] / n) as f32,
        ]
    }

    /// Copy a rectangle out as tightly packed RGBA.
    pub fn extract(&self, r: &TileRect) -> Vec<u8> {
        let mut out = Vec::with_capacity(r.w as usize * r.h as usize * 4);
        let stride = self.width as usize * 4;
        for y in r.y..r.y + r.h {
            let start = y as usize * stride + r.x as usize * 4;
            out.extend_from_slice(&self.data[start..start + r.w as usize * 4]);
        }
        out
    }

    /// Drop alpha: tightly packed RGB (kitty `f=24`, 25% fewer bytes for opaque pages).
    pub fn extract_rgb(&self, r: &TileRect) -> Vec<u8> {
        let mut out = Vec::with_capacity(r.w as usize * r.h as usize * 3);
        let stride = self.width as usize * 4;
        for y in r.y..r.y + r.h {
            let start = y as usize * stride + r.x as usize * 4;
            for px in self.data[start..start + r.w as usize * 4]
                .as_chunks::<4>()
                .0
            {
                out.extend_from_slice(&px[..3]);
            }
        }
        out
    }
}

/// Decode a JPEG or PNG frame (format sniffed from the bytes) to RGBA.
pub fn decode(bytes: &[u8]) -> Result<Rgba> {
    let img = image::load_from_memory(bytes).context("decode frame")?;
    let rgba = img.into_rgba8();
    Ok(Rgba {
        width: rgba.width(),
        height: rgba.height(),
        data: rgba.into_raw(),
    })
}

/// Scale `img` to fit inside `max_w × max_h` keeping its aspect ratio (up or down; the watch
/// view fits an agent's viewport into the pane). Returns the image unchanged when it already
/// has that size or a bound is zero.
pub fn scale_to_fit(img: &Rgba, max_w: u32, max_h: u32) -> Rgba {
    if max_w == 0 || max_h == 0 || img.width == 0 || img.height == 0 {
        return img.clone();
    }
    let k = (max_w as f64 / img.width as f64).min(max_h as f64 / img.height as f64);
    let w = ((img.width as f64 * k).round() as u32).clamp(1, max_w);
    let h = ((img.height as f64 * k).round() as u32).clamp(1, max_h);
    if w == img.width && h == img.height {
        return img.clone();
    }
    let Some(src) = image::RgbaImage::from_raw(img.width, img.height, img.data.clone()) else {
        return img.clone();
    };
    let out = image::imageops::resize(&src, w, h, image::imageops::FilterType::Triangle);
    Rgba {
        width: w,
        height: h,
        data: out.into_raw(),
    }
}

/// Encode RGBA as PNG (for kitty `f=100`).
pub fn encode_png(width: u32, height: u32, rgba: &[u8], fast: bool) -> Result<Vec<u8>> {
    use image::ImageEncoder;
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    let mut out = Vec::new();
    let (c, f) = if fast {
        (CompressionType::Fast, FilterType::Sub)
    } else {
        (CompressionType::Default, FilterType::Adaptive)
    };
    PngEncoder::new_with_quality(&mut out, c, f).write_image(
        rgba,
        width,
        height,
        image::ExtendedColorType::Rgba8,
    )?;
    Ok(out)
}

/// One tile of the grid, in image pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileRect {
    pub index: usize,
    pub col: u32,
    pub row: u32,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Remembers the previous frame and reports which tiles changed.
///
/// Change detection compares tile rows against the previous frame (exact, and at memory
/// bandwidth: hashing 8 MB per frame row by row with blake3 cost ~11 ms on an M1 Pro, the
/// comparison ~1 ms). [`tile_hash`] gives a content hash when a tile must be content-addressed
/// (render-stream image dedupe, 03 §9).
#[derive(Debug, Clone)]
pub struct TileDiffer {
    pub tile_w: u32,
    pub tile_h: u32,
    width: u32,
    height: u32,
    prev: Vec<u8>,
}

/// blake3 of a tile's pixels (first 128 bits).
pub fn tile_hash(img: &Rgba, r: &TileRect) -> u128 {
    let mut h = blake3::Hasher::new();
    let stride = img.width as usize * 4;
    for y in r.y..r.y + r.h {
        let start = y as usize * stride + r.x as usize * 4;
        h.update(&img.data[start..start + r.w as usize * 4]);
    }
    let b = h.finalize();
    u128::from_le_bytes(b.as_bytes()[..16].try_into().expect("16 bytes"))
}

impl TileDiffer {
    pub fn new(tile_w: u32, tile_h: u32) -> TileDiffer {
        assert!(tile_w > 0 && tile_h > 0);
        TileDiffer {
            tile_w,
            tile_h,
            width: 0,
            height: 0,
            prev: Vec::new(),
        }
    }

    /// Tiles of `tile_cols × tile_rows` terminal cells.
    pub fn cell_aligned(cell_w: u32, cell_h: u32, tile_cols: u32, tile_rows: u32) -> TileDiffer {
        TileDiffer::new(cell_w * tile_cols, cell_h * tile_rows)
    }

    pub fn grid(&self) -> (u32, u32) {
        (
            self.width.div_ceil(self.tile_w),
            self.height.div_ceil(self.tile_h),
        )
    }

    /// Forget everything (the next diff reports every tile).
    pub fn reset(&mut self) {
        self.width = 0;
        self.height = 0;
        self.prev.clear();
    }

    /// Every tile rect for a `width × height` image.
    pub fn tiles(&self, width: u32, height: u32) -> Vec<TileRect> {
        let cols = width.div_ceil(self.tile_w);
        let rows = height.div_ceil(self.tile_h);
        let mut out = Vec::with_capacity((cols * rows) as usize);
        for row in 0..rows {
            for col in 0..cols {
                let (x, y) = (col * self.tile_w, row * self.tile_h);
                out.push(TileRect {
                    index: (row * cols + col) as usize,
                    col,
                    row,
                    x,
                    y,
                    w: self.tile_w.min(width - x),
                    h: self.tile_h.min(height - y),
                });
            }
        }
        out
    }

    fn tile_differs(&self, img: &Rgba, r: &TileRect) -> bool {
        let stride = img.width as usize * 4;
        (r.y..r.y + r.h).any(|y| {
            let start = y as usize * stride + r.x as usize * 4;
            let end = start + r.w as usize * 4;
            img.data[start..end] != self.prev[start..end]
        })
    }

    /// Tiles that changed since the last call (all of them after a size change or reset).
    pub fn diff(&mut self, img: &Rgba) -> Vec<TileRect> {
        let tiles = self.tiles(img.width, img.height);
        let resized = img.width != self.width || img.height != self.height;
        let changed: Vec<TileRect> = if resized {
            tiles
        } else {
            tiles
                .into_iter()
                .filter(|t| self.tile_differs(img, t))
                .collect()
        };
        self.width = img.width;
        self.height = img.height;
        if !changed.is_empty() {
            self.prev.clear();
            self.prev.extend_from_slice(&img.data);
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(w: u32, h: u32) -> Rgba {
        let mut i = Rgba::new(w, h);
        i.fill_rect(0, 0, w, h, [255, 255, 255, 255]);
        i
    }

    #[test]
    fn scale_to_fit_keeps_aspect() {
        let mut a = img(1280, 720);
        a.fill_rect(0, 0, 640, 720, [255, 0, 0, 255]);
        let s = scale_to_fit(&a, 800, 800);
        assert_eq!((s.width, s.height), (800, 450));
        assert_eq!(s.pixel(10, 10)[0], 255);
        assert!(s.pixel(10, 10)[1] < 10, "left half stays red");
        assert_eq!(s.pixel(790, 10), [255, 255, 255, 255]);
        let up = scale_to_fit(&img(100, 50), 400, 400);
        assert_eq!((up.width, up.height), (400, 200));
        let same = scale_to_fit(&img(100, 50), 100, 50);
        assert_eq!((same.width, same.height), (100, 50));
        assert_eq!(scale_to_fit(&img(10, 10), 0, 5).width, 10);
    }

    #[test]
    fn grid_and_edge_tiles() {
        let d = TileDiffer::new(64, 64);
        let t = d.tiles(130, 70);
        assert_eq!(t.len(), 3 * 2);
        assert_eq!(t[2].w, 2);
        assert_eq!(t[2].x, 128);
        assert_eq!(t[5].h, 6);
        assert_eq!(t[5].index, 5);
        assert_eq!((t[4].col, t[4].row), (1, 1));
    }

    #[test]
    fn diff_reports_only_changed_tiles() {
        let mut d = TileDiffer::new(64, 64);
        let mut a = img(256, 128);
        assert_eq!(d.diff(&a).len(), 8, "first frame: everything");
        assert!(d.diff(&a).is_empty(), "same frame: nothing");
        a.fill_rect(70, 10, 4, 4, [0, 0, 0, 255]);
        let c = d.diff(&a);
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].col, c[0].row), (1, 0));
        // A change straddling four tiles.
        a.fill_rect(60, 60, 10, 10, [1, 2, 3, 255]);
        let c: Vec<_> = d.diff(&a).iter().map(|t| t.index).collect();
        assert_eq!(c, vec![0, 1, 4, 5]);
        // Resize: everything again.
        assert_eq!(d.diff(&img(128, 128)).len(), 4);
        d.reset();
        assert_eq!(d.diff(&img(128, 128)).len(), 4);
    }

    #[test]
    fn cell_aligned_tiles() {
        let d = TileDiffer::cell_aligned(16, 32, 4, 2);
        assert_eq!((d.tile_w, d.tile_h), (64, 64));
    }

    #[test]
    fn extract_and_png_roundtrip() {
        let mut a = img(100, 50);
        a.fill_rect(64, 0, 36, 50, [10, 20, 30, 255]);
        let d = TileDiffer::new(64, 64);
        let t = d.tiles(100, 50);
        let px = a.extract(&t[1]);
        assert_eq!(px.len(), 36 * 50 * 4);
        assert!(px.chunks(4).all(|p| p == [10, 20, 30, 255]));
        assert_eq!(a.extract_rgb(&t[1]).len(), 36 * 50 * 3);
        let png = encode_png(36, 50, &px, true).unwrap();
        let back = decode(&png).unwrap();
        assert_eq!((back.width, back.height), (36, 50));
        assert_eq!(back.data, px);
        assert_eq!(a.mean_rgb(64, 0, 36, 50), [10.0, 20.0, 30.0]);
        assert_eq!(tile_hash(&a, &t[1]), tile_hash(&a, &t[1]));
        assert_ne!(tile_hash(&a, &t[0]), tile_hash(&a, &t[1]));
    }
}
