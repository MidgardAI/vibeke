//! Row projection parity and invalidation tests. The legacy reader is deliberately frozen:
//! it is an independent oracle for the allocation rewrite, including style and link runs.

use super::*;

fn feed(engine: &mut Engine, bytes: &[u8]) {
    engine.feed(bytes, &mut Vec::new());
}

fn assert_projection_matches_legacy(engine: &Engine) {
    let expected: Vec<Row> = (0..engine.rows())
        .map(|y| legacy_read_row(engine, sys::GHOSTTY_POINT_TAG_ACTIVE, y as u32))
        .collect();
    assert_eq!(&*engine.visible_rows_shared(), expected.as_slice());
    assert_eq!(engine.visible_rows(), expected);
    for y in 0..engine.history_len() {
        let raw_y = engine.engine_history() - engine.history_len() + y;
        assert_eq!(
            engine.history_row(y).unwrap(),
            legacy_read_row(engine, sys::GHOSTTY_POINT_TAG_HISTORY, raw_y as u32),
        );
    }
}

#[test]
fn row_projection_matches_legacy_across_vt_corpus() {
    let long_grapheme = format!("x{}", "\u{301}".repeat(24));
    let corpus = [
        "plain text and blanks  ",
        "\r\n\x1b[1;31mred bold\x1b[0m ",
        "\x1b[38;2;12;34;56;48;5;123;4:3mstyled\x1b[0m",
        "\r\n日e\u{301}👩‍💻界",
        long_grapheme.as_str(),
        "\r\n\x1b]8;id=docs;https://example.com/docs\x1b\\日linked text\x1b]8;;\x07  ",
        "\x1b[44m\x1b[K\x1b[0m",
        "\x1b[48;2;40;50;60m\x1b[2K\x1b[0m",
        "\r\n\x1b]133;A\x07$ \x1b]133;B\x07echo test\r\n\x1b]133;C\x07test\r\n\x1b]133;D;0\x07",
        "\x1b[2;3Hoverwrite\x1b[3@ins\x1b[2P\x1b[2X",
        "\x1b[2;4r\x1b[4;1Hscroll\n\n\x1b[r",
        "\x1b[?1049h\x1b[2J\x1b[Halt screen 日\x1b[?1049l",
        "\x1b_Ga=T,f=32,s=1,v=1,i=7,c=1,r=1;/////w==\x1b\\",
        "\x1b_Ga=d,d=A;\x1b\\",
        "\x1b]4;1;#123456\x07\x1b]10;#eeeeee\x07\x1b]11;#222222\x07",
        "\x1b[?2026h\x1b[Hsync update\x1b[?2026l",
        "\r\n\r\nline one\r\nline two\r\nline three\r\nline four\r\nline five",
        "\x1bc",
    ];
    for cols in [2, 9, 37] {
        for chunk_len in [1, 7, 4096] {
            let mut engine = Engine::new(cols, 5, 16);
            assert_projection_matches_legacy(&engine);
            for (i, input) in corpus.iter().enumerate() {
                // Include reads with unfinished UTF-8, CSI, OSC and image sequences.
                for chunk in input.as_bytes().chunks(chunk_len) {
                    feed(&mut engine, chunk);
                    assert_projection_matches_legacy(&engine);
                }
                // Damage consumers must not consume row-cache invalidations.
                engine.take_damage();
                assert_projection_matches_legacy(&engine);
                if i % 4 == 0 {
                    engine.resize(cols + 3, 4);
                    assert_projection_matches_legacy(&engine);
                    engine.resize(cols, 5);
                    assert_projection_matches_legacy(&engine);
                    engine = Engine::restore(&engine.snapshot(), 16).unwrap();
                    assert_projection_matches_legacy(&engine);
                }
            }
        }
    }
}

#[test]
fn shared_projection_retains_client_revisions_and_invalidates_at_engine_mutations() {
    let mut engine = Engine::new(12, 3, 10);
    assert_eq!(engine.row_projection_builds(), 0);
    feed(&mut engine, b"first");
    let first = engine.visible_rows_shared();
    assert_eq!(engine.row_projection_builds(), 1);
    assert!(Arc::ptr_eq(&first, &engine.visible_rows_shared()));

    feed(&mut engine, b"\rsecond");
    engine.take_damage();
    let second = engine.visible_rows_shared();
    assert_eq!(engine.row_projection_builds(), 2);
    assert!(!Arc::ptr_eq(&first, &second));
    assert_eq!(first[0].text(), "first");
    assert_eq!(second[0].text(), "second");
    // Slow clients can retain either revision without changing what later clients read.
    let mut owned = engine.visible_rows();
    owned[0] = Row::default();
    assert!(Arc::ptr_eq(&second, &engine.visible_rows_shared()));
    assert_eq!(engine.visible_rows_shared()[0].text(), "second");

    engine.resize(8, 4);
    let resized = engine.visible_rows_shared();
    assert_eq!(engine.row_projection_builds(), 3);
    assert_eq!(resized.len(), 4);
    assert_eq!(second.len(), 3);
    assert!(!Arc::ptr_eq(&resized, &second));
    assert_projection_matches_legacy(&engine);

    let mut restored = Engine::restore(&engine.snapshot(), 10).unwrap();
    assert_eq!(restored.row_projection_builds(), 0);
    let restored_rows = restored.visible_rows_shared();
    assert_eq!(&*restored_rows, &*resized);
    assert!(!Arc::ptr_eq(&restored_rows, &resized));
    restored.set_replaying(true);
    feed(&mut restored, b"\rreplay");
    let replayed = restored.visible_rows_shared();
    assert_eq!(replayed[0].text(), "replay");
    assert_eq!(restored_rows[0].text(), "second");

    // A journal-gap replacement may share the server revision with its predecessor.
    // The cache belongs to the engine, so it cannot show the predecessor's rows.
    engine = Engine::new(8, 4, 10);
    let replacement = engine.visible_rows_shared();
    assert!(replacement.iter().all(|row| row.text().is_empty()));
    assert!(!Arc::ptr_eq(&replacement, &resized));
}

#[test]
fn palette_defaults_keep_the_symbolic_row_projection() {
    let mut engine = Engine::new(20, 2, 10);
    feed(&mut engine, b"default \x1b[31mred\x1b[0m");
    let before = engine.visible_rows_shared();
    let palette = Palette {
        fg: (1, 2, 3),
        bg: (4, 5, 6),
        ansi: [(7, 8, 9); 16],
        ..Palette::default()
    };
    engine.set_palette(palette);
    assert!(Arc::ptr_eq(&before, &engine.visible_rows_shared()));
    assert_eq!(engine.row_projection_builds(), 1);
    assert_projection_matches_legacy(&engine);
}

#[test]
#[ignore = "manual row conversion benchmark; run with --ignored --nocapture"]
fn row_projection_benchmark() {
    use std::hint::black_box;
    use std::time::Instant;
    let mut engine = Engine::new(160, 48, 100);
    let line = "build src/module.rs ... \x1b[32mok\x1b[0m 日e\u{301} ";
    for y in 1..=48 {
        feed(
            &mut engine,
            format!("\x1b[{y};1H{}", line.repeat(3)).as_bytes(),
        );
    }
    let iterations = 200;
    let legacy = Instant::now();
    for _ in 0..iterations {
        for y in 0..engine.rows() {
            black_box(legacy_read_row(
                &engine,
                sys::GHOSTTY_POINT_TAG_ACTIVE,
                y as u32,
            ));
        }
    }
    let legacy = legacy.elapsed();
    let direct = Instant::now();
    for _ in 0..iterations {
        for y in 0..engine.rows() {
            black_box(engine.row(y));
        }
    }
    let direct = direct.elapsed();
    engine.visible_rows_shared();
    let shared = Instant::now();
    for _ in 0..iterations {
        black_box(engine.visible_rows_shared());
    }
    let shared = shared.elapsed();
    eprintln!(
        "{iterations} screens (160x48): legacy={legacy:?}, direct={direct:?}, cached={shared:?}"
    );
    assert_eq!(engine.row_projection_builds(), 1);
}

// The reader before the per-cell allocation rewrite. Keep it independent so the corpus
// checks span boundaries, column widths, links and trimming as well as rendered text.
fn legacy_read_row(engine: &Engine, tag: i32, y: u32) -> Row {
    let mut r = sys::GhosttyGridRef {
        size: size_of::<sys::GhosttyGridRef>(),
        node: ptr::null_mut(),
        x: 0,
        y: 0,
    };
    if unsafe { sys::ghostty_terminal_grid_ref(engine.term, point(tag, 0, y), &mut r) }
        != sys::GHOSTTY_SUCCESS
    {
        return Row::default();
    }
    let mut wrapped = false;
    let mut has_links = false;
    let mut sem = 0i32;
    unsafe {
        let mut raw_row: sys::GhosttyRow = 0;
        sys::ghostty_grid_ref_row(&r, &mut raw_row);
        sys::ghostty_row_get(
            raw_row,
            sys::GHOSTTY_ROW_DATA_WRAP,
            (&raw mut wrapped).cast(),
        );
        sys::ghostty_row_get(
            raw_row,
            sys::GHOSTTY_ROW_DATA_HYPERLINK,
            (&raw mut has_links).cast(),
        );
        sys::ghostty_row_get(
            raw_row,
            sys::GHOSTTY_ROW_DATA_SEMANTIC_PROMPT,
            (&raw mut sem).cast(),
        );
    }
    let mut links: Vec<Link> = Vec::new();
    let mut ubuf: Vec<u8> = Vec::new();
    let cols = engine.cols();
    let mut spans: Vec<Span> = Vec::new();
    let mut last_style: Option<(u16, Style)> = None;
    let mut gbuf = [0u32; 16];
    for x in 0..cols {
        // Same page node and row: only the column changes (a grid ref is node + x + y).
        r.x = x;
        let mut cell: sys::GhosttyCell = 0;
        unsafe { sys::ghostty_grid_ref_cell(&r, &mut cell) };
        let (mut tag_v, mut wide, mut styled, mut cp, mut sid, mut linked) =
            (0i32, 0i32, false, 0u32, 0u16, false);
        unsafe {
            let keys = [
                sys::GHOSTTY_CELL_DATA_CONTENT_TAG,
                sys::GHOSTTY_CELL_DATA_WIDE,
                sys::GHOSTTY_CELL_DATA_HAS_STYLING,
                sys::GHOSTTY_CELL_DATA_CODEPOINT,
                sys::GHOSTTY_CELL_DATA_STYLE_ID,
                sys::GHOSTTY_CELL_DATA_HAS_HYPERLINK,
            ];
            let mut vals: [*mut c_void; 6] = [
                (&raw mut tag_v).cast(),
                (&raw mut wide).cast(),
                (&raw mut styled).cast(),
                (&raw mut cp).cast(),
                (&raw mut sid).cast(),
                (&raw mut linked).cast(),
            ];
            sys::ghostty_cell_get_multi(
                cell,
                if has_links { 6 } else { 5 },
                keys.as_ptr(),
                vals.as_mut_ptr(),
                ptr::null_mut(),
            );
        }
        if wide == sys::GHOSTTY_CELL_WIDE_SPACER_TAIL {
            continue;
        }
        if linked {
            let w = if wide == sys::GHOSTTY_CELL_WIDE_WIDE {
                2
            } else {
                1
            };
            if let Some(uri) = cell_uri(&r, &mut ubuf) {
                match links.last_mut() {
                    Some(l) if l.col + l.cols == x && l.uri == uri => l.cols += w,
                    _ => links.push(Link {
                        col: x,
                        cols: w,
                        uri: uri.to_string(),
                    }),
                }
            }
        }
        let mut style = if !styled {
            Style::default()
        } else if let Some((id, s)) = last_style
            && id == sid
        {
            s
        } else {
            let s = unsafe {
                let mut gs: sys::GhosttyStyle = std::mem::zeroed();
                gs.size = size_of::<sys::GhosttyStyle>();
                sys::ghostty_grid_ref_style(&r, &mut gs);
                style_of(&gs)
            };
            last_style = Some((sid, s));
            s
        };
        match tag_v {
            sys::GHOSTTY_CELL_CONTENT_BG_COLOR_PALETTE => {
                let mut i = 0u8;
                unsafe {
                    sys::ghostty_cell_get(
                        cell,
                        sys::GHOSTTY_CELL_DATA_COLOR_PALETTE,
                        (&raw mut i).cast(),
                    )
                };
                style.bg = Color::Indexed(i);
            }
            sys::GHOSTTY_CELL_CONTENT_BG_COLOR_RGB => {
                let mut c = sys::GhosttyColorRgb::default();
                unsafe {
                    sys::ghostty_cell_get(
                        cell,
                        sys::GHOSTTY_CELL_DATA_COLOR_RGB,
                        (&raw mut c).cast(),
                    )
                };
                style.bg = Color::Rgb(c.r, c.g, c.b);
            }
            _ => {}
        }
        let (text, w): (String, u16) = if wide == sys::GHOSTTY_CELL_WIDE_SPACER_HEAD
            || cp == 0
            || tag_v >= sys::GHOSTTY_CELL_CONTENT_BG_COLOR_PALETTE
        {
            (" ".into(), 1)
        } else {
            let mut s = String::new();
            if tag_v == sys::GHOSTTY_CELL_CONTENT_CODEPOINT_GRAPHEME {
                let mut n = 0usize;
                let res = unsafe {
                    sys::ghostty_grid_ref_graphemes(&r, gbuf.as_mut_ptr(), gbuf.len(), &mut n)
                };
                if res == sys::GHOSTTY_SUCCESS {
                    s.extend(gbuf[..n].iter().filter_map(|&c| char::from_u32(c)));
                } else {
                    let mut big = vec![0u32; n];
                    unsafe { sys::ghostty_grid_ref_graphemes(&r, big.as_mut_ptr(), n, &mut n) };
                    s.extend(
                        big[..n.min(big.len())]
                            .iter()
                            .filter_map(|&c| char::from_u32(c)),
                    );
                }
            }
            if s.is_empty() {
                s.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
            }
            (
                s,
                if wide == sys::GHOSTTY_CELL_WIDE_WIDE {
                    2
                } else {
                    1
                },
            )
        };
        match spans.last_mut() {
            Some(last) if last.style == style => {
                last.text.push_str(&text);
                last.cols += w;
            }
            _ => spans.push(Span {
                style,
                text,
                cols: w,
            }),
        }
    }
    // Trim trailing default-styled blanks to keep frames small.
    if let Some(last) = spans.last_mut()
        && last.style == Style::default()
    {
        let trimmed = last.text.trim_end_matches(' ');
        let removed = last.text.len() - trimmed.len();
        if removed > 0 {
            last.cols -= removed as u16;
            last.text.truncate(trimmed.len());
        }
        if last.text.is_empty() {
            spans.pop();
        }
    }
    Row {
        spans,
        wrapped,
        mark: mark_of(sem),
        links,
    }
}
