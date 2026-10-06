//! Feed the kitty output into Vibeke's own VT engine (libghostty-vt): the APC commands must be
//! consumed without leaking text, every image must be acknowledged (`q=0`), and the placeholder
//! cells must land as graphemes carrying the image id in their foreground colour with the
//! expected row/column diacritics.
//!
//! This also pins what the vendored engine accepts *inbound* (spec 03 §9): raw RGB/RGBA direct
//! transmission yes; PNG only with a host-supplied decoder (not configured → "unsupported
//! format"); shm / temp-file media not at all ("unsupported medium"); zlib only for some
//! streams (see `zlib_acceptance_of_vendored_engine`).

use vk_browser::frame::{Rgba, TileDiffer};
use vk_browser::kitty::{self, Header, PLACEHOLDER, PixelFormat, TileEncoder, Transfer};
use vk_proto::render::Color;
use vk_term::{Effect, Engine};

const CELL: (u32, u32) = (16, 32);

fn noisy(w: u32, h: u32) -> Rgba {
    let mut img = Rgba::new(w, h);
    for y in 0..h {
        for x in 0..w {
            img.fill_rect(
                x,
                y,
                1,
                1,
                [(x * 7) as u8, (y * 13) as u8, (x ^ y) as u8, 255],
            );
        }
    }
    img
}

fn replies(effects: &[Effect]) -> Vec<String> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Reply(b) => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        })
        .collect()
}

fn reply_for(replies: &[String], id: u32) -> Option<String> {
    let p = format!("\x1b_Gi={id};");
    replies
        .iter()
        .find(|r| r.starts_with(&p))
        .map(|r| r[p.len()..r.len() - 2].to_owned())
}

#[test]
fn kitty_output_parses_in_vk_term_engine() {
    // 12×3 cells; tiles of 4×2 cells → 3×2 tiles.
    let (w, h) = (12 * CELL.0, 3 * CELL.1);
    let img = noisy(w, h);
    let mut diff = TileDiffer::cell_aligned(CELL.0, CELL.1, 4, 2);
    let tiles = diff.diff(&img);
    assert_eq!(tiles.len(), 6);

    // (base id, transfer, expected reply)
    let variants = [
        (
            300,
            Transfer::Direct {
                format: PixelFormat::Rgba,
                zlib: false,
            },
            "OK",
        ),
        (
            400,
            Transfer::Direct {
                format: PixelFormat::Rgb,
                zlib: false,
            },
            "OK",
        ),
        (
            500,
            Transfer::Direct {
                format: PixelFormat::Png,
                zlib: false,
            },
            "EINVAL: unsupported format",
        ),
        (600, Transfer::Shm, "EINVAL: unsupported medium"),
        (
            700,
            Transfer::TempFile {
                dir: std::env::temp_dir(),
            },
            "EINVAL: unsupported medium",
        ),
    ];
    let mut out = Vec::new();
    let mut encoders = Vec::new();
    for (base, t, expect) in variants {
        let mut enc = TileEncoder::new(t, base, CELL.0, CELL.1);
        enc.quiet = 0;
        enc.encode(&img, &tiles, &mut out).unwrap();
        encoders.push((enc, expect));
    }
    let lines = encoders[0]
        .0
        .placeholder_lines(w, h, 4 * CELL.0, 2 * CELL.1)
        .unwrap();
    for (i, l) in lines.iter().enumerate() {
        out.extend(format!("\x1b[{};1H", i + 1).as_bytes());
        out.extend(l.as_bytes());
    }
    out.extend(vk_browser::probe::kitty_query_direct(31));

    let mut engine = Engine::new(20, 6, 100);
    let mut effects = Vec::new();
    engine.feed(&out, &mut effects);
    assert!(
        engine.tracker_pending_is_empty(),
        "parser back at ground after the stream"
    );
    let text = engine.screen_text();
    assert!(!text.contains("a=T"), "APC leaked into the grid: {text:?}");
    assert!(!text.contains(';'), "payload leaked: {text:?}");

    let replies = replies(&effects);
    for (enc, expect) in &mut encoders {
        for i in 0..6 {
            let id = enc.base_id + i;
            assert_eq!(
                reply_for(&replies, id).as_deref(),
                Some(*expect),
                "{:?} image {id}",
                enc.transfer
            );
        }
        enc.cleanup();
    }
    assert_eq!(
        reply_for(&replies, 31).as_deref(),
        Some("OK"),
        "a=q answered"
    );

    // Placeholder cells: 12 per row, ids by tile, diacritics only on each tile's first cell.
    for (y, row_in_tile) in [(0u16, 0u16), (1, 1), (2, 0)] {
        let row = engine.row(y);
        let cells: Vec<(String, Color)> = row
            .spans
            .iter()
            .flat_map(|s| {
                let fg = s.style.fg;
                split_graphemes(&s.text).into_iter().map(move |g| (g, fg))
            })
            .filter(|(g, _)| g.starts_with(PLACEHOLDER))
            .collect();
        assert_eq!(cells.len(), 12, "row {y}: {cells:?}");
        let tile_row = (y / 2) as u32;
        for (x, (g, fg)) in cells.iter().enumerate() {
            let id = 300 + tile_row * 3 + (x as u32 / 4);
            let expect = if id < 256 {
                Color::Indexed(id as u8)
            } else {
                Color::Rgb((id >> 16) as u8, (id >> 8) as u8, id as u8)
            };
            assert_eq!(*fg, expect, "row {y} col {x}");
            let (r, c, _) = kitty::decode_placeholder(g).unwrap();
            if x % 4 == 0 {
                assert_eq!((r, c), (Some(row_in_tile), Some(0)));
            } else {
                assert_eq!((r, c), (None, None), "compact cells inherit from the left");
            }
        }
    }
}

/// The vendored libghostty-vt (35a81a9, 1.3.2-dev) accepts `o=z` payloads made of stored or
/// simple blocks but rejects typical dynamic-Huffman streams with "decompression failed", even
/// though the same bytes decode with flate2 and with Wuffs' C decoder directly. Pinned here so
/// an engine update that fixes it is noticed (then `o=z` can be asserted OK like raw pixels).
/// Real Ghostty releases must be checked on the host before relying on `o=z` (06 B3.2).
#[test]
fn zlib_acceptance_of_vendored_engine() {
    let img = noisy(100, 7);
    let px = img.data.clone();
    let send = |z: &[u8], id: u32| -> Option<String> {
        let mut h = Header::new(id, PixelFormat::Rgba, 100, 7);
        h.zlib = true;
        h.quiet = 0;
        let mut out = Vec::new();
        kitty::write_chunked(&mut out, &h.control('d', None), z, 0);
        let mut e = Engine::new(20, 5, 10);
        let mut fx = Vec::new();
        e.feed(&out, &mut fx);
        reply_for(&replies(&fx), id)
    };
    // Flat colour (compressed) and stored blocks: accepted.
    assert_eq!(
        send(&kitty::zlib(&[7u8; 2800], 6), 1).as_deref(),
        Some("OK")
    );
    assert_eq!(send(&kitty::zlib(&px, 0), 2).as_deref(), Some("OK"));
    // Noisy pixels, dynamic Huffman: rejected by this engine build.
    let r = send(&kitty::zlib(&px, 6), 3);
    eprintln!("noisy o=z reply: {r:?}");
    assert!(
        r.as_deref() == Some("EINVAL: decompression failed") || r.as_deref() == Some("OK"),
        "unexpected reply {r:?}"
    );
    if r.as_deref() == Some("OK") {
        eprintln!("vendored engine now accepts dynamic-Huffman o=z: update spec 06 B3.2 notes");
    }
}

/// Placeholder + its combining diacritics form one grapheme; split on the placeholder.
fn split_graphemes(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for ch in s.chars() {
        if ch == PLACEHOLDER || out.is_empty() || !kitty::DIACRITICS.contains(&ch) {
            out.push(ch.to_string());
        } else {
            out.last_mut().unwrap().push(ch);
        }
    }
    out
}
