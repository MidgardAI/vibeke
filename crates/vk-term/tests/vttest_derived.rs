//! vttest-derived conformance cases for `vk_term::Engine` (spec 03 §2.3, 10 §6).
//!
//! These replay the escape-sequence programs of vttest's first menu ("Test of cursor
//! movements": the box of `*`/`+` with the frame of `E`s, the autowrap demo, cursor controls
//! inside CSI sequences, leading zeros) without a terminal in the loop: the sequences are
//! produced exactly as vttest's `main.c` (`tst_movements`) and `esc.c` helpers emit them, fed
//! to the engine, and the resulting screen is compared with what vttest tells the user to
//! verify by eye. vttest is MIT/X11-licensed, so deriving cases from it is fine; its notice:
//!
//! > Copyright 1996-2024,2025 by Thomas E. Dickey
//! >
//! > Permission is hereby granted, free of charge, to any person obtaining a copy of this
//! > software and associated documentation files (the "Software"), to deal in the Software
//! > without restriction, including without limitation the rights to use, copy, modify,
//! > merge, publish, distribute, distribute with modifications, sublicense, and/or sell copies
//! > of the Software, and to permit persons to whom the Software is furnished to do so,
//! > subject to the following conditions: The above copyright notice and this permission
//! > notice shall be included in all copies or substantial portions of the Software.
//! > THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND.
//!
//! esctest2 is GPL-2.0 and is not vendored or derived from here (the workspace is
//! Apache-2.0); `tests/conformance.rs` holds independently written cases in its spirit.

use vk_term::Engine;

const ROWS: usize = 24;
const COLS: usize = 80;

/// vttest's `esc.c` helpers, writing into a byte buffer (7-bit controls, as vttest's default).
#[derive(Default)]
struct Vt(Vec<u8>);

impl Vt {
    fn s(&mut self, s: &str) {
        self.0.extend_from_slice(s.as_bytes());
    }
    fn csi(&mut self, s: &str) {
        self.s(&format!("\x1b[{s}"));
    }
    fn cup(&mut self, row: usize, col: usize) {
        self.csi(&format!("{row};{col}H"));
    }
    fn hvp(&mut self, row: usize, col: usize) {
        self.csi(&format!("{row};{col}f"));
    }
    fn cub(&mut self, n: usize) {
        self.csi(&format!("{n}D"));
    }
    fn cuf(&mut self, n: usize) {
        self.csi(&format!("{n}C"));
    }
    fn cuu(&mut self, n: usize) {
        self.csi(&format!("{n}A"));
    }
    fn cud(&mut self, n: usize) {
        self.csi(&format!("{n}B"));
    }
    fn ed(&mut self, n: usize) {
        self.csi(&format!("{n}J"));
    }
    fn el(&mut self, n: usize) {
        self.csi(&format!("{n}K"));
    }
    fn ind(&mut self) {
        self.s("\x1bD");
    }
    fn ri(&mut self) {
        self.s("\x1bM");
    }
    fn nel(&mut self) {
        self.s("\x1bE");
    }
    fn decaln(&mut self) {
        self.s("\x1b#8");
    }
    fn decstbm(&mut self, top: usize, bottom: usize) {
        if top == 0 && bottom == 0 {
            self.csi("r");
        } else {
            self.csi(&format!("{top};{bottom}r"));
        }
    }
    fn decom(&mut self, on: bool) {
        self.csi(if on { "?6h" } else { "?6l" });
    }
    /// `tprintf("\n")` on a tty in `crmod` (ONLCR): CR LF.
    fn nl(&mut self) {
        self.s("\r\n");
    }
}

fn run(bytes: &[u8]) -> Vec<Vec<char>> {
    let mut e = Engine::new(COLS as u16, ROWS as u16, 0);
    e.feed(bytes, &mut Vec::new());
    // Byte-at-a-time must give the same screen (the parser is split-invariant).
    let mut e2 = Engine::new(COLS as u16, ROWS as u16, 0);
    for b in bytes {
        e2.feed(std::slice::from_ref(b), &mut Vec::new());
    }
    assert_eq!(e.screen_text(), e2.screen_text(), "split invariance");
    e.visible_rows()
        .iter()
        .map(|r| {
            let mut v: Vec<char> = r.text().chars().collect();
            v.resize(COLS, ' ');
            v
        })
        .collect()
}

fn show(s: &[Vec<char>]) -> String {
    s.iter()
        .map(|r| r.iter().collect::<String>().trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `tst_movements`, first screen at 80 columns: "The screen should be cleared, and have an
/// unbroken border of *'s and +'s around the edge, and exactly in the middle there should be
/// a frame of E's around this text with one (1) free position around it."
#[test]
fn vttest_movements_box() {
    let (width, max_lines) = (COLS, ROWS);
    let inner_l = (width - 60) / 2;
    let inner_r = 61 + inner_l;
    let hlfxtra = (width - 80) / 2;
    let mut v = Vt::default();
    v.decaln();
    v.cup(9, inner_l);
    v.ed(1);
    v.cup(18, 60 + hlfxtra);
    v.ed(0);
    v.el(1);
    v.cup(9, inner_r);
    v.el(0);
    for row in 10..=16 {
        v.cup(row, inner_l);
        v.el(1);
        v.cup(row, inner_r);
        v.el(0);
    }
    v.cup(17, 30);
    v.el(2);
    for col in 1..=width {
        v.hvp(max_lines, col);
        v.s("*");
        v.hvp(1, col);
        v.s("*");
    }
    v.cup(2, 2);
    for _ in 2..max_lines {
        v.s("+");
        v.cub(1);
        v.ind();
    }
    v.cup(max_lines - 1, width - 1);
    for _ in (2..max_lines).rev() {
        v.s("+");
        v.cub(1);
        v.ri();
    }
    v.cup(2, 1);
    for row in 2..max_lines {
        v.s("*");
        v.cup(row, width);
        v.s("*");
        v.cub(10);
        if row < 10 {
            v.nel();
        } else {
            v.nl();
        }
    }
    v.cup(2, 10);
    v.cub(42 + hlfxtra);
    v.cuf(2);
    for _ in 3..=width - 2 {
        v.s("+");
        v.cuf(0);
        v.cub(2);
        v.cuf(1);
    }
    v.cup(max_lines - 1, inner_r - 1);
    v.cuf(42 + hlfxtra);
    v.cub(2);
    for _ in (3..=width - 2).rev() {
        v.s("+");
        v.cub(1);
        v.cuf(1);
        v.cub(0);
        v.s("\x08");
    }
    v.cup(1, 1);
    v.cuu(10);
    v.cuu(1);
    v.cuu(0);
    v.cup(max_lines, width);
    v.cud(10);
    v.cud(1);
    v.cud(0);
    v.cup(10, 2 + inner_l);
    for _ in 10..=15 {
        for _ in 2 + inner_l..=inner_r - 2 {
            v.s(" ");
        }
        v.cud(1);
        v.cub(58);
    }
    v.cuu(5);
    v.cuf(1);
    let text = [
        "The screen should be cleared,  and have an unbroken bor-",
        "der of *'s and +'s around the edge,   and exactly in the",
        "middle  there should be a frame of E's around this  text",
        "with  one (1) free position around it.    ",
    ];
    v.s(text[0]);
    for (i, t) in text.iter().enumerate().skip(1) {
        v.cup(11 + i, inner_l + 3);
        v.s(t);
    }
    let got = run(&v.0);

    // What vttest asks the user to see, cell by cell (1-based rows/columns).
    let mut want = vec![vec![' '; COLS]; ROWS];
    for r in 1..=ROWS {
        for c in 1..=COLS {
            let ch = if r == 1 || r == ROWS || c == 1 || c == COLS {
                '*'
            } else if c == 2 || c == COLS - 1 || r == 2 || r == ROWS - 1 {
                '+'
            } else if ((r == 9 || r == 16) && (11..=70).contains(&c))
                || ((10..=15).contains(&r) && (c == 11 || c == 70))
            {
                'E'
            } else {
                ' '
            };
            want[r - 1][c - 1] = ch;
        }
    }
    for (i, t) in text.iter().enumerate() {
        for (k, ch) in t.chars().enumerate() {
            want[10 + i][12 + k] = ch;
        }
    }
    assert_eq!(show(&got), show(&want));
}

/// `tst_movements`, autowrap demo at 80 columns: "The left/right margins should have letters
/// in order", mixing wraps, backspace at the margin, tabs to the margin and newlines.
#[test]
fn vttest_autowrap_mixing_controls() {
    let width = COLS;
    let max_lines = ROWS;
    let on_left: Vec<char> = ('A'..='Z').collect();
    let on_right: Vec<char> = ('a'..='z').collect();
    let region = max_lines - 6;
    let mut v = Vt::default();
    v.ed(2);
    v.cup(1, 1);
    v.s("Test of autowrap, mixing control and print characters.\r\n");
    v.s("The left/right margins should have letters in order:\r\n");
    v.decstbm(3, region + 3);
    v.decom(true);
    for i in 0..on_left.len() {
        match i % 4 {
            0 => {
                v.cup(region + 1, 1);
                v.s(&on_left[i].to_string());
                v.cup(region + 1, width);
                v.s(&on_right[i].to_string());
                v.nl();
            }
            1 => {
                v.cup(region, width);
                v.s(&format!("{}{}", on_right[i - 1], on_left[i]));
                v.cup(region + 1, width);
                v.s(&format!("{}\x08 {}", on_left[i], on_right[i]));
                v.nl();
            }
            2 => {
                v.cup(region + 1, width);
                v.s(&format!("{}\x08\x08\t\t{}", on_left[i], on_right[i]));
                v.cup(region + 1, 2);
                v.s(&format!("\x08{}", on_left[i]));
                v.nl();
            }
            _ => {
                v.cup(region + 1, width);
                v.nl();
                v.cup(region, 1);
                v.s(&on_left[i].to_string());
                v.cup(region, width);
                v.s(&on_right[i].to_string());
            }
        }
    }
    v.decom(false);
    v.decstbm(0, 0);
    let got = run(&v.0);
    let s = show(&got);
    // Inside the scroll region only the margins are written, and each row pairs the same
    // letter on both sides, in alphabetical order downwards, ending with Z/z.
    let mut lefts = String::new();
    for r in 3..region + 3 {
        let row = &got[r - 1];
        let (l, rt) = (row[0], row[COLS - 1]);
        assert!(
            row[1..COLS - 1].iter().all(|c| *c == ' '),
            "row {r} interior\n{s}"
        );
        if l == ' ' && rt == ' ' {
            continue;
        }
        assert_eq!(l.to_ascii_lowercase(), rt, "row {r} pairs\n{s}");
        lefts.push(l);
    }
    let all: String = on_left.iter().collect();
    assert!(
        all.ends_with(&lefts) && lefts.ends_with('Z') && lefts.len() >= region - 1,
        "margins in order: {lefts:?}\n{s}"
    );
    // The 19-row region keeps the last 18 pairs plus the blank row the final newline opened.
    assert_eq!(lefts, "IJKLMNOPQRSTUVWXYZ");
}

/// `tst_movements`: "Below should be four identical lines" (controls inside CSI sequences)
/// and "you should see the sentence" (leading zeros in parameters).
#[test]
fn vttest_controls_inside_csi_and_leading_zeros() {
    let mut v = Vt::default();
    v.ed(2);
    v.cup(1, 1);
    v.s("Test of cursor-control characters inside ESC sequences.\r\n");
    v.s("Below should be four identical lines:\r\n");
    v.s("\r\n");
    v.s("A B C D E F G H I\r\n");
    for i in 1..10u8 {
        v.s(&((b'@' + i) as char).to_string());
        v.s("\x1b[2\x08C"); // two forward, one backspace
    }
    v.s("\r\n");
    v.s("A ");
    for i in 2..10u8 {
        v.s(&format!("\x1b[\r{}C", 2 * i as usize - 2));
        v.s(&((b'@' + i) as char).to_string());
    }
    v.s("\r\n");
    v.csi("20l"); // LNM off: VT is a plain line feed
    for i in 1..10u8 {
        v.s(&format!("{} ", (b'@' + i) as char));
        v.s("\x1b[1\x0bA");
    }
    v.s("\r\n\r\n");
    let got = run(&v.0);
    let s = show(&got);
    for r in 3..=6 {
        assert_eq!(
            got[r].iter().collect::<String>().trim_end(),
            "A B C D E F G H I",
            "line {r}\n{s}"
        );
    }

    let ctext = "This is a correct sentence";
    let mut v = Vt::default();
    v.ed(2);
    v.cup(1, 1);
    v.s("Test of leading zeros in ESC sequences.\r\n");
    for (col, ch) in ctext.chars().enumerate() {
        v.s(&format!("\x1b[00000000004;00000000{}H", col + 1));
        v.s(&ch.to_string());
    }
    let got = run(&v.0);
    assert_eq!(got[3].iter().collect::<String>().trim_end(), ctext);
}
