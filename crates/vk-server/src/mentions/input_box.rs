//! Whether an agent's input box is empty, read from the styled screen (spec 19 §4.2).
//!
//! Plain screen text cannot tell a placeholder from a draft: Codex shows
//! `› Ask Codex to do anything` in an empty box, Claude `❯ Try "…"`. Both draw the placeholder
//! faint (SGR 2) or in a gray foreground, while typed text has the normal colour. So the reader
//! finds the harness's input line, takes the text after the prompt marker up to the box edge
//! (and the continuation lines inside the box), and calls the box empty only when every visible
//! character there is placeholder-styled.
//!
//! - Claude: a `❯` (or `>`) line right below a horizontal rule (current UI) or inside a rounded
//!   box (`╭─╮ │ > │ ╰─╯`, older UI). The box ends at the next rule or bottom border.
//! - Codex: the lowest `›` (or `>`) line; continuation lines follow until the first blank line
//!   (the footer comes after one).
//! - Other harnesses, and Claude or Codex screens where that layout is not found: the plain-text
//!   reader [`crate::tracking::input_text`]. It calls a box empty only when nothing follows the
//!   prompt marker, and any text (a placeholder too) a draft.

use crate::Server;
use crate::agents::harness::{Family, Harness};
use vk_proto::render::{Color, Row, Style, attr};

/// What the input box holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InputBox {
    /// Nothing, or only the harness's placeholder.
    Empty,
    /// Text the person typed (or pasted).
    Draft(String),
    /// No input box was found where the harness draws it.
    Unknown,
}

/// Rows of the screen bottom that may hold the input box.
const SCAN_ROWS: usize = 30;

#[derive(Debug, Clone, Copy)]
struct Cell {
    ch: char,
    /// Drawn like a placeholder: faint, or a gray foreground.
    faint: bool,
    inverse: bool,
}

/// The input box of `pane` as its agent (`harness`) draws it. `None` when the pane is gone.
pub(crate) fn of_pane(server: &Server, pane: &str, harness: Option<Harness>) -> Option<InputBox> {
    let rt = server.pane_rt(pane)?;
    let rows = rt.screen.lock().unwrap().engine.visible_rows();
    Some(read(&rows, harness.map(|h| h.family())))
}

/// The input box on `rows` (the visible screen, top to bottom) for a harness of `family`.
pub(crate) fn read(rows: &[Row], family: Option<Family>) -> InputBox {
    let grid: Vec<Vec<Cell>> = rows.iter().map(cells).collect();
    let styled = match family {
        Some(Family::Claude) => claude(&grid),
        Some(Family::Codex) => codex(&grid),
        _ => InputBox::Unknown,
    };
    match styled {
        InputBox::Unknown => plain(rows),
        known => known,
    }
}

fn plain(rows: &[Row]) -> InputBox {
    let text = rows
        .iter()
        .map(|r| r.text().trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    match crate::tracking::input_text(&text) {
        Some(t) if t.is_empty() => InputBox::Empty,
        Some(t) => InputBox::Draft(t),
        None => InputBox::Unknown,
    }
}

/// Placeholder styling: faint, or a gray foreground (bright black, the grayscale ramp, or an
/// RGB gray of medium brightness).
pub(crate) fn is_faint(s: &Style) -> bool {
    if s.attrs & attr::DIM != 0 {
        return true;
    }
    match s.fg {
        Color::Indexed(8) => true,
        Color::Indexed(n) => (232..=250).contains(&n),
        Color::Rgb(r, g, b) => {
            let hi = r.max(g).max(b);
            let lo = r.min(g).min(b);
            hi - lo <= 16 && (60..=180).contains(&hi)
        }
        Color::Default => false,
    }
}

fn cells(row: &Row) -> Vec<Cell> {
    let mut out = Vec::new();
    for span in &row.spans {
        let faint = is_faint(&span.style);
        let inverse = span.style.attrs & attr::INVERSE != 0;
        out.extend(span.text.chars().map(|ch| Cell { ch, faint, inverse }));
    }
    out
}

fn text_of(cells: &[Cell]) -> String {
    cells.iter().map(|c| c.ch).collect()
}

fn is_edge(c: char) -> bool {
    matches!(c, '│' | '┃' | '|' | '▌')
}

/// The cells without surrounding blanks and box edges.
fn inner(cells: &[Cell]) -> &[Cell] {
    let mut s = cells;
    loop {
        let before = s.len();
        while s.first().is_some_and(|c| c.ch.is_whitespace()) {
            s = &s[1..];
        }
        while s.last().is_some_and(|c| c.ch.is_whitespace()) {
            s = &s[..s.len() - 1];
        }
        if s.first().is_some_and(|c| is_edge(c.ch)) {
            s = &s[1..];
        }
        if s.last().is_some_and(|c| is_edge(c.ch)) {
            s = &s[..s.len() - 1];
        }
        if s.len() == before {
            return s;
        }
    }
}

fn is_rule(t: &str) -> bool {
    let t = t.trim();
    t.chars().count() >= 10 && t.chars().all(|c| c == '─' || c == '━')
}

fn is_box_top(t: &str) -> bool {
    t.trim_start().starts_with(['╭', '┌'])
}

fn is_box_bottom(t: &str) -> bool {
    t.trim_start().starts_with(['╰', '└'])
}

/// The cells after a prompt marker (`markers`) at the start of `line`, if it starts with one.
fn after_marker<'a>(line: &'a [Cell], markers: &[char]) -> Option<&'a [Cell]> {
    let first = line.first()?;
    markers.contains(&first.ch).then(|| &line[1..])
}

fn claude(grid: &[Vec<Cell>]) -> InputBox {
    let n = grid.len();
    for i in (n.saturating_sub(SCAN_ROWS)..n).rev() {
        let Some(first) = after_marker(inner(&grid[i]), &['❯', '>']) else {
            continue;
        };
        let above = i
            .checked_sub(1)
            .map(|j| text_of(&grid[j]))
            .unwrap_or_default();
        let boxed = text_of(&grid[i]).trim_start().starts_with(['│', '┃']);
        if !(is_rule(&above) || (boxed && is_box_top(&above))) {
            continue;
        }
        let mut body: Vec<&[Cell]> = vec![first];
        for row in &grid[i + 1..] {
            let t = text_of(row);
            if is_rule(&t) || is_box_bottom(&t) {
                return classify(&body);
            }
            body.push(inner(row));
        }
        // The box runs off the screen: its end (and so its content) is not known.
        return InputBox::Unknown;
    }
    InputBox::Unknown
}

fn codex(grid: &[Vec<Cell>]) -> InputBox {
    let n = grid.len();
    for i in (n.saturating_sub(SCAN_ROWS)..n).rev() {
        let Some(first) = after_marker(inner(&grid[i]), &['›', '>']) else {
            continue;
        };
        let mut body: Vec<&[Cell]> = vec![first];
        for row in &grid[i + 1..] {
            let line = inner(row);
            if line.is_empty() {
                break;
            }
            body.push(line);
        }
        return classify(&body);
    }
    InputBox::Unknown
}

/// Empty when no visible character of the box is typed text: blanks, the cursor, and a
/// placeholder (faint text that starts right after the prompt, on the first line only).
fn classify(body: &[&[Cell]]) -> InputBox {
    let lines: Vec<&[Cell]> = body.iter().map(|l| inner(l)).collect();
    let text = lines
        .iter()
        .map(|l| text_of(l))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    if text.is_empty() {
        return InputBox::Empty;
    }
    // A placeholder is one line; any content on a later line is the person's.
    if lines[1..].iter().any(|l| !l.is_empty()) {
        return InputBox::Draft(text);
    }
    let first = lines[0];
    // Pasted text and images show as chips (`[Pasted text #1 +20 lines]`), maybe styled faint.
    if text.starts_with('[') {
        return InputBox::Draft(text);
    }
    let visible: Vec<&Cell> = first.iter().filter(|c| !c.ch.is_whitespace()).collect();
    let placeholder = visible.iter().enumerate().all(|(k, c)| {
        if c.faint {
            return true;
        }
        // The cursor drawn on the placeholder's first character.
        k == 0 && c.inverse && visible.len() > 1 && visible[1..].iter().all(|d| d.faint)
    });
    if placeholder {
        InputBox::Empty
    } else {
        InputBox::Draft(text)
    }
}

#[cfg(test)]
#[path = "input_box_tests.rs"]
mod tests;
