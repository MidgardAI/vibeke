//! The zstd dictionary for compressed mux frames (06 A4).
//!
//! A *raw-content* dictionary (zstd uses it as shared history, no training format): the
//! postcard encoding of representative render frames (rows of shell, compiler and agent
//! output with the usual styles, spinner diffs, cursor and mode records) followed by common
//! terminal text. It is built deterministically from the render protocol types at first use,
//! so both ends of a link derive the same bytes from the same code; [`id`] is a hash of the
//! bytes, advertised in the mux `Hello`, and compression is only used when both ends report
//! the same id. A render protocol change changes the id, which simply turns compression off
//! between mismatched builds.

use sha2::{Digest, Sha256};
use std::sync::OnceLock;
use vk_proto::render::{
    Color, Cursor, CursorShape, DiffOp, PaneModes, Row, ServerFrame, Span, Style, attr,
};

const LINES: &[&str] = &[
    "demo@devbox ~/code/vibeke (main) $ cargo test -p vk-remote",
    "   Compiling vk-proto v0.1.0 (/home/demo/code/vibeke/crates/vk-proto)",
    "    Finished `test` profile [unoptimized + debuginfo] target(s) in 4.21s",
    "     Running unittests src/lib.rs (target/debug/deps/vk_remote-3f9a1c0b2e7d)",
    "test result: ok. 42 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out",
    "error[E0308]: mismatched types",
    "  --> src/main.rs:12:5",
    "warning: unused variable: `x`",
    "drwxr-xr-x  12 demo  staff   384 Oct  6 20:49 .",
    "-rw-r--r--   1 demo  staff  4096 Oct  6 20:49 Cargo.toml",
    "⏺ I'll read the file first and then make the change.",
    "✻ Thinking… (esc to interrupt)",
    "⎿  Read 120 lines",
    "╭──────────────────────────────────────────────────────────────╮",
    "│ > Try \"fix the failing test\"                                 │",
    "╰──────────────────────────────────────────────────────────────╯",
    "  ? for shortcuts                                  ⧉ In main.rs",
    "• Running npm run dev",
    "  VITE v5.4.0  ready in 312 ms",
    "  ➜  Local:   http://localhost:5173/",
    "diff --git a/src/lib.rs b/src/lib.rs",
    "@@ -1,7 +1,9 @@",
    "+    let x = 1;",
    "-    let y = 2;",
];

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn styles() -> Vec<Style> {
    vec![
        Style::default(),
        Style {
            fg: Color::Indexed(2),
            attrs: attr::BOLD,
            ..Style::default()
        },
        Style {
            fg: Color::Indexed(1),
            attrs: attr::BOLD,
            ..Style::default()
        },
        Style {
            fg: Color::Indexed(8),
            ..Style::default()
        },
        Style {
            fg: Color::Rgb(215, 119, 87),
            ..Style::default()
        },
        Style {
            fg: Color::Rgb(153, 153, 153),
            attrs: attr::DIM,
            ..Style::default()
        },
        Style {
            bg: Color::Rgb(55, 55, 55),
            ..Style::default()
        },
    ]
}

fn row(text: &str, style: Style, pad_to: u16) -> Row {
    let cols = text.chars().count() as u16;
    let mut spans = vec![Span {
        style,
        text: text.to_string(),
        cols,
    }];
    if pad_to > cols {
        spans.push(Span {
            style: Style::default(),
            text: " ".repeat((pad_to - cols) as usize),
            cols: pad_to - cols,
        });
    }
    Row {
        spans,
        wrapped: false,
    }
}

fn build() -> Vec<u8> {
    let styles = styles();
    let mut out = Vec::new();
    let cursor = Cursor {
        col: 2,
        row: 23,
        visible: true,
        shape: CursorShape::Block,
        blink: false,
    };
    let modes = PaneModes {
        bracketed_paste: true,
        ..PaneModes::default()
    };
    // Spinner-only diffs (the most frequent unfocused-pane frame).
    for (i, s) in SPINNER.iter().enumerate() {
        let f = ServerFrame::PaneDiff {
            pane: "P01J9Z3".into(),
            epoch: 1,
            base_rev: 100 + i as u64,
            rev: 101 + i as u64,
            ops: vec![DiffOp::Rows(vec![(
                22,
                row(
                    &format!("{s} Working… (12s · esc to interrupt)"),
                    styles[4],
                    0,
                ),
            )])],
            cursor,
            modes,
            title: "claude".into(),
        };
        out.extend(vk_proto::frame::encode(&f).unwrap_or_default());
    }
    // Full rows of typical output, each with several styles.
    let rows: Vec<(u16, Row)> = LINES
        .iter()
        .enumerate()
        .map(|(i, l)| (i as u16, row(l, styles[i % styles.len()], 120)))
        .collect();
    let f = ServerFrame::PaneDiff {
        pane: "P01J9Z3".into(),
        epoch: 1,
        base_rev: 200,
        rev: 201,
        ops: vec![DiffOp::ScrollUp { n: 1 }, DiffOp::Rows(rows)],
        cursor,
        modes,
        title: "zsh".into(),
    };
    out.extend(vk_proto::frame::encode(&f).unwrap_or_default());
    // Plain text last: zstd favours the end of a raw dictionary.
    for l in LINES {
        out.extend_from_slice(l.as_bytes());
        out.push(b'\n');
    }
    out
}

/// The dictionary bytes.
pub fn bytes() -> &'static [u8] {
    static D: OnceLock<Vec<u8>> = OnceLock::new();
    D.get_or_init(build)
}

/// Short id of [`bytes`] (first 8 hex digits of its sha256), advertised as `zstd:<id>`.
pub fn id() -> &'static str {
    static I: OnceLock<String> = OnceLock::new();
    I.get_or_init(|| {
        Sha256::digest(bytes())
            .iter()
            .take(4)
            .map(|b| format!("{b:02x}"))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_bounded() {
        assert_eq!(build(), bytes());
        assert_eq!(id().len(), 8);
        assert!(bytes().len() > 2_000 && bytes().len() < 64 * 1024);
    }

    #[test]
    fn dictionary_helps_small_render_frames() {
        let f = ServerFrame::PaneDiff {
            pane: "P01J9Z3".into(),
            epoch: 1,
            base_rev: 7,
            rev: 8,
            ops: vec![DiffOp::Rows(vec![(
                22,
                row("⠙ Working… (13s · esc to interrupt)", styles()[4], 0),
            )])],
            cursor: Cursor::default(),
            modes: PaneModes::default(),
            title: "claude".into(),
        };
        let raw = vk_proto::frame::encode(&f).unwrap();
        let plain = zstd::bulk::compress(&raw, 3).unwrap();
        let with = zstd::bulk::Compressor::with_dictionary(3, bytes())
            .unwrap()
            .compress(&raw)
            .unwrap();
        assert!(
            with.len() < plain.len() && with.len() < raw.len() / 2,
            "raw {} plain {} dict {}",
            raw.len(),
            plain.len(),
            with.len()
        );
        let back = zstd::bulk::Decompressor::with_dictionary(bytes())
            .unwrap()
            .decompress(&with, raw.len())
            .unwrap();
        assert_eq!(back, raw);
    }
}
