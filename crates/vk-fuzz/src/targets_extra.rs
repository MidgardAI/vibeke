//! The four targets added for 10 §6 after the first eight: `vt_resize_interleave`,
//! `compat_socket`, `transcript_parse`, `osc_image` (re-exported from `targets`).

use crate::rng::Rng;
use std::time::Duration;

fn lossy(data: &[u8]) -> String {
    String::from_utf8_lossy(data).into_owned()
}

/// Bytes with resize operations between them (10 §6 `vt_resize_interleave`). Oracle, checked
/// after every feed and resize: the dimensions are what was asked for, the cursor is inside the
/// grid, every visible row is present; at the end `snapshot -> restore` reproduces the screen.
pub fn vt_resize_interleave(data: &[u8]) {
    use vk_term::Engine;
    let started = std::time::Instant::now();
    let mut rng = Rng::from_bytes(data);
    let (mut cols, mut rows) = (2 + rng.below(120) as u16, 1 + rng.below(50) as u16);
    let mut e = Engine::new(cols, rows, 500);
    let mut out = Vec::new();
    let check = |e: &Engine, cols: u16, rows: u16, what: &str| {
        assert_eq!(
            (e.cols(), e.rows()),
            (cols, rows),
            "dimensions after {what}"
        );
        let c = e.cursor();
        assert!(
            c.col < cols && c.row < rows,
            "cursor {c:?} outside {cols}x{rows} after {what}"
        );
        assert_eq!(
            e.visible_rows().len(),
            rows as usize,
            "visible rows after {what}"
        );
    };
    let mut rest = data;
    while !rest.is_empty() {
        let n = 1 + rng.below(rest.len().min(128));
        e.feed(&rest[..n], &mut out);
        rest = &rest[n..];
        check(&e, cols, rows, "feed");
        if rng.chance(3) {
            // Mostly small steps (reflow of the same content), sometimes extremes; the engine's
            // minimum width is 2.
            cols = match rng.below(4) {
                0 => 2 + rng.below(3) as u16,
                1 => cols.saturating_add(rng.below(20) as u16).min(300),
                2 => cols.saturating_sub(rng.below(20) as u16).max(2),
                _ => 2 + rng.below(299) as u16,
            };
            rows = match rng.below(3) {
                0 => 1 + rng.below(3) as u16,
                1 => 1 + rng.below(100) as u16,
                _ => rows,
            };
            e.resize(cols, rows);
            check(&e, cols, rows, "resize");
        }
        if out.len() > 10_000 {
            out.clear();
        }
    }
    let snap = e.snapshot();
    if !snap.is_empty() {
        let back = Engine::restore(&snap, 500).expect("a snapshot we just took must restore");
        assert_eq!(
            (back.cols(), back.rows()),
            (e.cols(), e.rows()),
            "restored dimensions"
        );
        assert_eq!(back.screen_text(), e.screen_text(), "restored screen text");
    }
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "engine too slow on {} bytes",
        data.len()
    );
}

/// Herdr-compat socket requests: one JSON line per connection (10 §6 `compat_socket`). Oracle:
/// no panic; whatever the first line parses to, the reply is exactly one valid JSON line with
/// the request's id, so the connection can be closed after it (one-shot semantics); the
/// subscription and sandbox checks never panic on the parsed method and params.
pub fn compat_socket(data: &[u8]) {
    use vk_compat::herdr::{events, wire};
    // `serve_wire` reads up to the first newline only.
    let first = data
        .iter()
        .position(|&b| b == b'\n')
        .map_or(data, |i| &data[..=i]);
    match wire::parse_request(first) {
        Ok(req) => {
            assert!(
                first.len() <= wire::MAX_LINE,
                "an over-long line was accepted"
            );
            let _ = events::parse_subscriptions(&req.params);
            let _ = vk_server::compat::sandbox_allows(&req.method);
            let line = wire::ok_line(&req.id, serde_json::json!({"echo": req.params}));
            one_json_line(&line, &req.id);
        }
        Err((id, e)) => {
            let line = wire::err_line(&id, &e);
            one_json_line(&line, &id);
        }
    }
    let _ = wire::parse_request(data);
}

fn one_json_line(line: &str, id: &str) {
    assert!(
        !line.contains('\n') && !line.contains('\r'),
        "reply spans lines: {line:?}"
    );
    let v: serde_json::Value = serde_json::from_str(line).expect("reply is valid JSON");
    assert_eq!(v["id"], id, "reply id");
}

/// Harness transcript JSONL (10 §6 `transcript_parse`): bounded memory with huge lines. Oracle:
/// no panic; `consumed` never exceeds the input and ends on a line boundary; every indexed row
/// is bounded (8 KiB plus the ellipsis) whatever the line size; parsing in two chunks at an
/// arbitrary cut loses no complete line (`consumed` of the first chunk resumes the second).
pub fn transcript_parse(data: &[u8]) {
    use vk_server::desk::parse_chunk;
    let whole = parse_chunk(data, 0, 0, 1);
    assert!(whole.consumed as usize <= data.len());
    assert!(
        whole.consumed == 0 || data[whole.consumed as usize - 1] == b'\n',
        "consumed does not end on a newline"
    );
    for r in &whole.rows {
        assert!(
            r.text.len() <= 8192 + 4,
            "unbounded row: {} bytes",
            r.text.len()
        );
    }
    let mut rng = Rng::from_bytes(data);
    let cut = if data.is_empty() {
        0
    } else {
        rng.below(data.len())
    };
    let a = parse_chunk(&data[..cut], 0, 0, 1);
    let rest = &data[a.consumed as usize..];
    let b = parse_chunk(rest, a.consumed, a.turns, 1);
    assert_eq!(
        a.consumed + b.consumed,
        whole.consumed,
        "resumed parse consumed a different amount"
    );
    assert_eq!(
        a.rows.len() + b.rows.len(),
        whole.rows.len(),
        "resumed parse produced a different number of rows"
    );
}

/// Inline-image payloads (10 §6 `osc_image`): kitty graphics (APC `G`), sixel (DCS `q`) and
/// iTerm2 (`OSC 1337 File=`) sequences, with hostile dimensions and payloads, into the engine,
/// plus the tile inflater and placeholder codec. Oracle: no panic or hang; the inflater never
/// produces more than its limit; placeholder rows stay inside their documented bounds.
pub fn osc_image(data: &[u8]) {
    use vk_browser::kitty;
    use vk_term::Engine;
    let mut rng = Rng::from_bytes(data);
    let started = std::time::Instant::now();
    // Wrap the payload in one of the three image sequences (or feed it bare).
    let body = &data[..data.len().min(4096)];
    let mut seq = Vec::new();
    match rng.below(4) {
        0 => {
            seq.extend_from_slice(b"\x1b_G");
            seq.extend_from_slice(body);
            seq.extend_from_slice(b"\x1b\\");
        }
        1 => {
            seq.extend_from_slice(b"\x1bPq");
            seq.extend_from_slice(body);
            seq.extend_from_slice(b"\x1b\\");
        }
        2 => {
            seq.extend_from_slice(b"\x1b]1337;File=");
            seq.extend_from_slice(body);
            seq.push(7);
        }
        _ => seq.extend_from_slice(body),
    }
    // Declared sizes far beyond what the payload carries.
    if rng.chance(3) {
        let (w, h) = (rng.next_u64() as u32, rng.next_u64() as u32);
        let id = rng.below(1 << 24);
        seq.extend(format!("\x1b_Gf=32,s={w},v={h},a=T,i={id};AAAA\x1b\\").bytes());
    }
    let mut e = Engine::new(2 + rng.below(100) as u16, 1 + rng.below(40) as u16, 100);
    let mut fx = Vec::new();
    let step = 1 + rng.below(512);
    for chunk in seq.chunks(step) {
        e.feed(chunk, &mut fx);
        fx.clear();
    }
    let _ = e.screen_text();

    let max = rng.below(1 << 16);
    if let Ok(px) = kitty::unzlib_limited(data, max) {
        assert!(px.len() <= max, "inflater exceeded its limit");
    }
    let _ = kitty::unzlib_limited(&kitty::zlib(data, 1), max.max(data.len()));
    let _ = kitty::decode_placeholder(&lossy(data));
    let (id, row, col0, cols) = (
        rng.next_u64() as u32,
        rng.below(400) as u16,
        rng.below(400) as u16,
        rng.below(400) as u16,
    );
    if let Ok(s) = kitty::placeholder_row(id, row, col0, cols, rng.chance(2)) {
        assert!(
            row < 297 && (col0 + cols) <= 297,
            "oversized placement accepted"
        );
        assert!(
            cols == 0 || s.contains(kitty::PLACEHOLDER),
            "no placeholder cells"
        );
    }
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "image sequences too slow on {} bytes",
        data.len()
    );
}
