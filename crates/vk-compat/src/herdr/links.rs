//! Herdr 0.9.3's link targets for `pane.link.resolve` and `pane.link.activate`
//! (`src/app/actions.rs`, `crates/ghostty-vt` `viewport_link_*`): an OSC 8 hyperlink under the
//! cell is the target; otherwise the whitespace-bounded token under the cell (across soft
//! wraps) is searched for an `http://` or `https://` URL, trimmed like Herdr trims it.
//!
//! The caller lays the logical line out as [`Cell`]s (one grapheme each, with its viewport row
//! and columns); this module finds the token, the URL and the regions to underline.

use std::ops::Range;

/// One display cell of a logical line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// Viewport row (may lie outside the viewport for a token that continues there).
    pub row: i64,
    pub col: u16,
    /// Last column the cell covers (`col + 1` for a wide character).
    pub end_col: u16,
    /// The cell's grapheme (empty for a blank cell).
    pub text: String,
}

impl Cell {
    fn blank(&self) -> bool {
        self.text.chars().next().is_none_or(char::is_whitespace)
    }
}

/// Herdr's `LinkRegion`: inclusive columns on one viewport row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub row: u16,
    pub start_col: u16,
    pub end_col: u16,
}

/// Herdr bounds the word selection it formats (`ghostty_terminal_select_word_bounded`).
const MAX_TOKEN_CELLS: usize = 8192;

/// The whitespace-bounded token around cell `clicked`: `(first, last)` cell indices, or `None`
/// when the clicked cell is blank.
pub fn token(cells: &[Cell], clicked: usize) -> Option<(usize, usize)> {
    if cells.get(clicked)?.blank() {
        return None;
    }
    let (mut a, mut b) = (clicked, clicked);
    while a > 0 && !cells[a - 1].blank() && clicked - a < MAX_TOKEN_CELLS {
        a -= 1;
    }
    while b + 1 < cells.len() && !cells[b + 1].blank() && b - clicked < MAX_TOKEN_CELLS {
        b += 1;
    }
    Some((a, b))
}

/// The token's text and the byte offset of the clicked cell in it (Herdr's
/// `LinkTarget::Text`).
pub fn token_text(cells: &[Cell], tok: (usize, usize), clicked: usize) -> (String, usize) {
    let mut text = String::new();
    let mut clicked_byte = 0;
    for (i, c) in cells[tok.0..=tok.1].iter().enumerate() {
        if tok.0 + i == clicked {
            clicked_byte = text.len();
        }
        text.push_str(&c.text);
    }
    (text, clicked_byte)
}

fn safe_web_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// Herdr's `url_byte_range`: the byte range of the `http(s)://` URL in `text` that contains
/// `clicked_byte`, trimmed of trailing punctuation and unbalanced closers.
pub fn url_byte_range(text: &str, clicked_byte: usize) -> Option<Range<usize>> {
    let clicked = text.get(..clicked_byte)?.chars().count();
    let chars: Vec<char> = text.chars().collect();
    let starts = |i: usize, p: &str| {
        let p: Vec<char> = p.chars().collect();
        chars.len() >= i + p.len() && chars[i..i + p.len()] == p[..]
    };
    let mut start = 0;
    let mut span = None;
    while start < chars.len() {
        if starts(start, "http://") || starts(start, "https://") {
            let mut end = start;
            while end + 1 < chars.len() && !chars[end + 1].is_whitespace() {
                end += 1;
            }
            if clicked >= start && clicked <= end {
                let mut e = end;
                loop {
                    let trim = match chars[e] {
                        '"' | '\'' | '`' | '.' | ',' | ';' | ':' | '!' | '?' => true,
                        ')' => !balanced(&chars[start..e], '(', ')'),
                        ']' => !balanced(&chars[start..e], '[', ']'),
                        '}' => !balanced(&chars[start..e], '{', '}'),
                        _ => false,
                    };
                    if !trim {
                        break;
                    }
                    if e == start {
                        return None;
                    }
                    e -= 1;
                }
                span = (clicked <= e).then_some((start, e));
                break;
            }
            start = end + 1;
        } else {
            start += 1;
        }
    }
    let (s, e) = span?;
    let byte = |idx: usize| text.char_indices().nth(idx).map_or(text.len(), |(b, _)| b);
    let range = byte(s)..byte(e + 1);
    safe_web_url(text.get(range.clone())?).then_some(range)
}

fn balanced(chars: &[char], open: char, close: char) -> bool {
    let mut n = 0i32;
    for &c in chars {
        if c == open {
            n += 1;
        } else if c == close {
            n -= 1;
        }
    }
    n > 0
}

/// Herdr's `url_at_byte`.
pub fn url_at_byte(text: &str, clicked_byte: usize) -> Option<&str> {
    text.get(url_byte_range(text, clicked_byte)?)
}

/// The viewport regions of the URL `range` (bytes of the token text) in the token's cells,
/// merged per row like Herdr's `viewport_link_regions`; cells outside `0..rows` are clipped.
pub fn regions(
    cells: &[Cell],
    tok: (usize, usize),
    range: &Range<usize>,
    rows: u16,
) -> Vec<Region> {
    let mut out: Vec<Region> = Vec::new();
    let mut byte = 0usize;
    for c in &cells[tok.0..=tok.1] {
        let len = c.text.len();
        let hit = byte < range.end && byte + len > range.start;
        byte += len;
        if !hit || c.row < 0 || c.row >= i64::from(rows) {
            continue;
        }
        let row = c.row as u16;
        match out
            .last_mut()
            .filter(|l| l.row == row && l.end_col + 1 == c.col)
        {
            Some(l) => l.end_col = c.end_col,
            None => out.push(Region {
                row,
                start_col: c.col,
                end_col: c.end_col,
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(row: i64, s: &str) -> Vec<Cell> {
        s.chars()
            .enumerate()
            .map(|(i, ch)| Cell {
                row,
                col: i as u16,
                end_col: i as u16,
                text: if ch == ' ' {
                    String::new()
                } else {
                    ch.to_string()
                },
            })
            .collect()
    }

    #[test]
    fn urls() {
        let t = "see:https://example.com/a_(b).";
        let r = url_byte_range(t, 10).unwrap();
        assert_eq!(&t[r], "https://example.com/a_(b)");
        assert_eq!(url_at_byte("(https://x.io/y)", 3), Some("https://x.io/y"));
        assert_eq!(url_at_byte("ftp://x.io", 2), None);
        assert_eq!(url_at_byte("nothing", 2), None);
        // The click must land inside the trimmed URL.
        assert_eq!(url_at_byte("https://x.io.", 12), None);
    }

    #[test]
    fn tokens_and_regions() {
        let mut cells = line(0, "go https://exam");
        cells.extend(line(1, "ple.com/x now"));
        let tok = token(&cells, 5).unwrap();
        let (text, b) = token_text(&cells, tok, 5);
        assert_eq!(text, "https://example.com/x");
        assert_eq!(b, 2);
        let r = url_byte_range(&text, b).unwrap();
        assert_eq!(
            regions(&cells, tok, &r, 24),
            vec![
                Region {
                    row: 0,
                    start_col: 3,
                    end_col: 14
                },
                Region {
                    row: 1,
                    start_col: 0,
                    end_col: 8
                },
            ]
        );
        // A row below the viewport is clipped.
        assert_eq!(regions(&cells, tok, &r, 1).len(), 1);
        assert!(token(&cells, 2).is_none(), "blank cell");
    }
}
