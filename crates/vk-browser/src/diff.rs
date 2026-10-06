//! Visual diff of two screenshots (spec 06 B6): per-pixel comparison with a per-channel
//! threshold, changed-pixel statistics, changed regions (bounding boxes of connected changed
//! cells) and a diff image (the "after" image faded to grey with changed pixels in red).
//!
//! Images of different sizes are compared on the union canvas; pixels present in only one
//! image count as changed and the result says `size_mismatch`.

use anyhow::{Context, Result};
use image::{ImageFormat, Rgba, RgbaImage};

/// Changed pixels are grouped on a grid of this many pixels per cell.
const CELL: u32 = 16;
/// At most this many regions are reported (largest first); the rest are summarized.
const MAX_REGIONS: usize = 50;

/// Per grid cell: changed pixel count and the bounding box (x0, y0, x1, y1) of its changes.
type Cell = (u64, u32, u32, u32, u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// Changed pixels inside the region.
    pub pixels: u64,
}

#[derive(Debug, Clone)]
pub struct DiffResult {
    pub width: u32,
    pub height: u32,
    pub a_size: (u32, u32),
    pub b_size: (u32, u32),
    pub size_mismatch: bool,
    pub changed_pixels: u64,
    pub total_pixels: u64,
    /// `changed_pixels / total_pixels` (0 for an empty canvas).
    pub changed_ratio: f64,
    /// Largest regions first, at most [`MAX_REGIONS`].
    pub regions: Vec<Region>,
    pub regions_total: usize,
    /// Per-channel threshold actually used (0–255).
    pub channel_threshold: u8,
    /// PNG-encoded diff image.
    pub diff_png: Vec<u8>,
}

/// A threshold in `0.0..=1.0` (fraction of the channel range) → a per-channel delta.
/// `0.0` means any difference counts. Default 0.1 (≈ 25 levels), like pixelmatch's default.
pub fn channel_threshold(threshold: f64) -> u8 {
    (threshold.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn decode(png: &[u8], which: &str) -> Result<RgbaImage> {
    Ok(image::load_from_memory(png)
        .with_context(|| format!("decode screenshot {which}"))?
        .to_rgba8())
}

/// Compare two encoded images (PNG/JPEG).
pub fn diff_png(a: &[u8], b: &[u8], threshold: f64) -> Result<DiffResult> {
    let a = decode(a, "a")?;
    let b = decode(b, "b")?;
    Ok(diff_images(&a, &b, threshold))
}

/// A pixel differs when any channel (R, G, B, A) differs by more than the threshold.
fn differs(p: &Rgba<u8>, q: &Rgba<u8>, t: u8) -> bool {
    p.0.iter().zip(q.0.iter()).any(|(x, y)| x.abs_diff(*y) > t)
}

pub fn diff_images(a: &RgbaImage, b: &RgbaImage, threshold: f64) -> DiffResult {
    let t = channel_threshold(threshold);
    let (w, h) = (a.width().max(b.width()), a.height().max(b.height()));
    let size_mismatch = a.dimensions() != b.dimensions();
    let (cw, ch) = (w.div_ceil(CELL).max(1), h.div_ceil(CELL).max(1));
    // Per cell: changed count and bounding box of changed pixels.
    let mut cells: Vec<Option<Cell>> = vec![None; (cw * ch) as usize];
    let mut out = RgbaImage::new(w, h);
    let mut changed = 0u64;
    for y in 0..h {
        for x in 0..w {
            let pa = (x < a.width() && y < a.height()).then(|| a.get_pixel(x, y));
            let pb = (x < b.width() && y < b.height()).then(|| b.get_pixel(x, y));
            let is_changed = match (pa, pb) {
                (Some(p), Some(q)) => differs(p, q, t),
                _ => true,
            };
            if is_changed {
                changed += 1;
                out.put_pixel(x, y, Rgba([255, 0, 0, 255]));
                let i = ((y / CELL) * cw + x / CELL) as usize;
                let c = cells[i].get_or_insert((0, x, y, x, y));
                c.0 += 1;
                c.1 = c.1.min(x);
                c.2 = c.2.min(y);
                c.3 = c.3.max(x);
                c.4 = c.4.max(y);
            } else {
                // Faded greyscale of the "after" image (or "before" where only it exists).
                let p = pb.or(pa).copied().unwrap_or(Rgba([255, 255, 255, 255]));
                let l = (0.299 * p[0] as f64 + 0.587 * p[1] as f64 + 0.114 * p[2] as f64) as u32;
                let faded = (255 - (255 - l.min(255)) / 4) as u8;
                out.put_pixel(x, y, Rgba([faded, faded, faded, 255]));
            }
        }
    }
    // Connected changed cells (8-neighbourhood) → regions.
    let mut seen = vec![false; cells.len()];
    let mut regions = Vec::new();
    for start in 0..cells.len() {
        if seen[start] || cells[start].is_none() {
            continue;
        }
        let mut stack = vec![start];
        seen[start] = true;
        let mut r = (0u64, u32::MAX, u32::MAX, 0u32, 0u32);
        while let Some(i) = stack.pop() {
            let (n, x0, y0, x1, y1) = cells[i].expect("changed cell");
            r.0 += n;
            r.1 = r.1.min(x0);
            r.2 = r.2.min(y0);
            r.3 = r.3.max(x1);
            r.4 = r.4.max(y1);
            let (cx, cy) = ((i as u32) % cw, (i as u32) / cw);
            for dy in -1i64..=1 {
                for dx in -1i64..=1 {
                    let (nx, ny) = (cx as i64 + dx, cy as i64 + dy);
                    if nx < 0 || ny < 0 || nx >= cw as i64 || ny >= ch as i64 {
                        continue;
                    }
                    let j = (ny as u32 * cw + nx as u32) as usize;
                    if !seen[j] && cells[j].is_some() {
                        seen[j] = true;
                        stack.push(j);
                    }
                }
            }
        }
        regions.push(Region {
            x: r.1,
            y: r.2,
            width: r.3 - r.1 + 1,
            height: r.4 - r.2 + 1,
            pixels: r.0,
        });
    }
    regions.sort_by(|p, q| q.pixels.cmp(&p.pixels).then((p.y, p.x).cmp(&(q.y, q.x))));
    let regions_total = regions.len();
    regions.truncate(MAX_REGIONS);
    let total = w as u64 * h as u64;
    let mut diff_png = Vec::new();
    // Encoding an in-memory RGBA image can't fail short of OOM.
    let _ = out.write_to(&mut std::io::Cursor::new(&mut diff_png), ImageFormat::Png);
    DiffResult {
        width: w,
        height: h,
        a_size: a.dimensions(),
        b_size: b.dimensions(),
        size_mismatch,
        changed_pixels: changed,
        total_pixels: total,
        changed_ratio: if total == 0 {
            0.0
        } else {
            changed as f64 / total as f64
        },
        regions,
        regions_total,
        channel_threshold: t,
        diff_png,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(img: &RgbaImage) -> Vec<u8> {
        let mut v = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut v), ImageFormat::Png)
            .unwrap();
        v
    }

    fn canvas(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_pixel(w, h, Rgba([240, 240, 240, 255]))
    }

    fn fill(img: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, c: [u8; 4]) {
        for yy in y..y + h {
            for xx in x..x + w {
                img.put_pixel(xx, yy, Rgba(c));
            }
        }
    }

    #[test]
    fn identical_images_have_no_changes() {
        let a = canvas(64, 48);
        let r = diff_png(&png(&a), &png(&a), 0.1).unwrap();
        assert_eq!(r.changed_pixels, 0);
        assert_eq!(r.total_pixels, 64 * 48);
        assert_eq!(r.changed_ratio, 0.0);
        assert!(r.regions.is_empty());
        assert!(!r.size_mismatch);
        let d = image::load_from_memory(&r.diff_png).unwrap();
        assert_eq!((d.width(), d.height()), (64, 48));
    }

    #[test]
    fn two_separate_changes_give_two_regions() {
        let a = canvas(200, 100);
        let mut b = a.clone();
        fill(&mut b, 10, 10, 20, 10, [0, 0, 0, 255]); // 200 px
        fill(&mut b, 150, 60, 5, 5, [0, 0, 255, 255]); // 25 px
        let r = diff_png(&png(&a), &png(&b), 0.1).unwrap();
        assert_eq!(r.changed_pixels, 225);
        assert!((r.changed_ratio - 225.0 / 20000.0).abs() < 1e-12);
        assert_eq!(r.regions.len(), 2);
        assert_eq!(
            r.regions[0],
            Region {
                x: 10,
                y: 10,
                width: 20,
                height: 10,
                pixels: 200
            }
        );
        assert_eq!(
            r.regions[1],
            Region {
                x: 150,
                y: 60,
                width: 5,
                height: 5,
                pixels: 25
            }
        );
        // Changed pixels are red in the diff image, unchanged ones grey.
        let d = image::load_from_memory(&r.diff_png).unwrap().to_rgba8();
        assert_eq!(d.get_pixel(15, 15).0, [255, 0, 0, 255]);
        let g = d.get_pixel(100, 50).0;
        // Background 240 grey, faded towards white: 255 - (255 - 240) / 4.
        assert_eq!(g, [252, 252, 252, 255]);
    }

    #[test]
    fn threshold_ignores_small_channel_deltas() {
        let a = canvas(32, 32);
        let mut b = a.clone();
        fill(&mut b, 0, 0, 32, 32, [250, 240, 240, 255]); // +10 on red
        assert_eq!(diff_png(&png(&a), &png(&b), 0.1).unwrap().changed_pixels, 0);
        assert_eq!(
            diff_png(&png(&a), &png(&b), 0.0).unwrap().changed_pixels,
            32 * 32
        );
        assert_eq!(channel_threshold(0.1), 26);
        assert_eq!(channel_threshold(7.0), 255);
    }

    #[test]
    fn size_mismatch_counts_the_extra_area() {
        let a = canvas(10, 10);
        let b = canvas(10, 12);
        let r = diff_png(&png(&a), &png(&b), 0.1).unwrap();
        assert!(r.size_mismatch);
        assert_eq!((r.width, r.height), (10, 12));
        assert_eq!(r.changed_pixels, 20);
        assert_eq!(r.regions.len(), 1);
        assert_eq!((r.regions[0].y, r.regions[0].height), (10, 2));
        assert!(diff_png(b"nope", &png(&a), 0.1).is_err());
    }
}
