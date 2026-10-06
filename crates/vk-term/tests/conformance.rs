//! In-repo VT conformance suite for `vk_term::Engine` (spec 10 §6, "own corpus").
//!
//! This is NOT esctest2 (GPL-2.0, so neither vendored nor derived here); vttest (MIT/X11) cases
//! are replayed in `tests/vttest_derived.rs`. This file is a
//! deterministic table of escape-sequence cases written from ECMA-48, the xterm ctlseqs
//! document, the kitty keyboard protocol and DEC STD 070 behaviour, in the same spirit (input
//! bytes in, expected screen/cursor/modes/replies/effects out). Each case is fed whole and again
//! one byte at a time (the parser must be split-invariant).
//!
//! Known divergences from xterm/the spec are recorded as expected failures (`.xfail("why")`).
//! They must keep failing: when the engine is fixed or upgraded and one starts to pass, the
//! suite fails and tells you to drop the marker, so the baseline never silently rots.
//!
//! Running the real esctest2 later: it drives a terminal over a PTY and needs the terminal
//! under test as a program. Run `vibeke debug ptyshot`-style, i.e. start `python3 esctest.py
//! --expected-terminal xterm --xterm-checksum=... --logfile esctest.log` inside a Vibeke pane
//! (`vibeke pane run -- python3 esctest/esctest.py ...`), and record failures into the same
//! expected-failure style. vttest likewise: run it in a pane and compare `vibeke debug ptyshot`
//! screens against golden captures. Set `VK_CONFORMANCE_REPORT=1` with `--nocapture` to print the
//! per-category baseline of this suite.

use vk_proto::render::{Color, attr};
use vk_term::{Effect, Engine, NotifyKind};

type Check = Box<dyn Fn(&Out) -> Result<(), String>>;

struct Out {
    e: Engine,
    fx: Vec<Effect>,
    replies: Vec<u8>,
}

struct Case {
    cat: &'static str,
    name: &'static str,
    cols: u16,
    rows: u16,
    input: Vec<u8>,
    checks: Vec<Check>,
    xfail: Option<&'static str>,
}

fn case(cat: &'static str, name: &'static str, cols: u16, rows: u16, input: &str) -> Case {
    Case {
        cat,
        name,
        cols,
        rows,
        input: input.as_bytes().to_vec(),
        checks: vec![],
        xfail: None,
    }
}

impl Case {
    fn check(mut self, f: impl Fn(&Out) -> Result<(), String> + 'static) -> Self {
        self.checks.push(Box::new(f));
        self
    }
    fn xfail(mut self, why: &'static str) -> Self {
        self.xfail = Some(why);
        self
    }
    /// Expected rows (trailing blanks trimmed); rows not listed must be empty.
    fn screen(self, rows: &'static [&'static str]) -> Self {
        self.check(move |o| {
            let got: Vec<String> =
                o.e.visible_rows()
                    .iter()
                    .map(|r| r.text().trim_end().to_string())
                    .collect();
            for (i, g) in got.iter().enumerate() {
                let want = rows.get(i).copied().unwrap_or("");
                if g != want {
                    return Err(format!("row {i}: got {g:?} want {want:?} (screen {got:?})"));
                }
            }
            Ok(())
        })
    }
    fn cursor(self, col: u16, row: u16) -> Self {
        self.check(move |o| {
            let c = o.e.cursor();
            (c.col == col && c.row == row)
                .then_some(())
                .ok_or(format!("cursor ({},{}) want ({col},{row})", c.col, c.row))
        })
    }
    fn cursor_visible(self, v: bool) -> Self {
        self.check(move |o| {
            (o.e.cursor().visible == v)
                .then_some(())
                .ok_or(format!("cursor visible {} want {v}", o.e.cursor().visible))
        })
    }
    fn replies(self, want: &'static str) -> Self {
        self.check(move |o| {
            (o.replies == want.as_bytes()).then_some(()).ok_or(format!(
                "replies {:?} want {:?}",
                String::from_utf8_lossy(&o.replies),
                want
            ))
        })
    }
    fn reply_prefix(self, pre: &'static str, suffix: &'static str) -> Self {
        self.check(move |o| {
            let s = String::from_utf8_lossy(&o.replies).into_owned();
            (s.starts_with(pre) && s.ends_with(suffix))
                .then_some(())
                .ok_or(format!("reply {s:?} want {pre:?}..{suffix:?}"))
        })
    }
    fn modes(self, f: impl Fn(&vk_proto::render::PaneModes) -> bool + 'static) -> Self {
        self.check(move |o| {
            let m = o.e.modes();
            f(&m).then_some(()).ok_or(format!("modes {m:?}"))
        })
    }
    fn effect(self, want: Effect) -> Self {
        self.check(move |o| {
            o.fx.contains(&want)
                .then_some(())
                .ok_or(format!("effects {:?} lack {want:?}", o.fx))
        })
    }
    fn title(self, t: &'static str) -> Self {
        self.check(move |o| {
            (o.e.title() == t)
                .then_some(())
                .ok_or(format!("title {:?} want {t:?}", o.e.title()))
        })
    }
    fn wrapped(self, row: u16, v: bool) -> Self {
        self.check(move |o| {
            (o.e.row(row).wrapped == v).then_some(()).ok_or(format!(
                "row {row} wrapped {} want {v}",
                o.e.row(row).wrapped
            ))
        })
    }
    fn history(self, n: usize) -> Self {
        self.check(move |o| {
            (o.e.history_len() == n)
                .then_some(())
                .ok_or(format!("history {} want {n}", o.e.history_len()))
        })
    }
    /// Style of the cell at (col,row): attrs must be exactly `attrs`; colours when given.
    fn cell(
        self,
        col: u16,
        row: u16,
        attrs: u16,
        fg: Option<Color>,
        bg: Option<Color>,
        ul: Option<Color>,
    ) -> Self {
        self.check(move |o| {
            let r = o.e.row(row);
            let mut x = 0u16;
            for s in &r.spans {
                if col < x + s.cols {
                    let st = s.style;
                    let ok = st.attrs == attrs
                        && fg.is_none_or(|c| c == st.fg)
                        && bg.is_none_or(|c| c == st.bg)
                        && ul.is_none_or(|c| c == st.ul);
                    return ok.then_some(()).ok_or(format!(
                        "cell ({col},{row}) style {st:?}; want attrs {attrs:#x} fg {fg:?} bg {bg:?} ul {ul:?}"
                    ));
                }
                x += s.cols;
            }
            Err(format!("cell ({col},{row}) beyond row spans"))
        })
    }
    fn attrs(self, a: u16) -> Self {
        self.cell(0, 0, a, None, None, None)
    }
    fn fg(self, c: Color) -> Self {
        self.check(move |o| {
            let st = o.e.row(0).spans[0].style;
            (st.fg == c)
                .then_some(())
                .ok_or(format!("fg {:?} want {c:?}", st.fg))
        })
    }
    fn bg(self, c: Color) -> Self {
        self.check(move |o| {
            let st = o.e.row(0).spans[0].style;
            (st.bg == c)
                .then_some(())
                .ok_or(format!("bg {:?} want {c:?}", st.bg))
        })
    }
    fn ul(self, c: Color) -> Self {
        self.check(move |o| {
            let st = o.e.row(0).spans[0].style;
            (st.ul == c)
                .then_some(())
                .ok_or(format!("ul {:?} want {c:?}", st.ul))
        })
    }
}

fn run(c: &Case, bytewise: bool) -> Out {
    let mut o = Out {
        e: Engine::new(c.cols, c.rows, 100),
        fx: vec![],
        replies: vec![],
    };
    let mut fx = Vec::new();
    if bytewise {
        for b in &c.input {
            o.e.feed(std::slice::from_ref(b), &mut fx);
        }
    } else {
        o.e.feed(&c.input, &mut fx);
    }
    for f in fx {
        match f {
            Effect::Reply(b) => o.replies.extend(b),
            other => o.fx.push(other),
        }
    }
    o
}

fn check(c: &Case, o: &Out) -> Result<(), String> {
    for ch in &c.checks {
        ch(o)?;
    }
    Ok(())
}

const I: fn(u8) -> Color = Color::Indexed;

fn cases() -> Vec<Case> {
    let mut v: Vec<Case> = Vec::new();
    // A 6x4 grid pre-filled AAAAAA/BBBBBB/CCCCCC/DDDDDD, cursor homed.
    const FILL: &str = "\x1b[2J\x1b[HAAAAAA\x1b[2;1HBBBBBB\x1b[3;1HCCCCCC\x1b[4;1HDDDDDD";
    let fill = |extra: &str| format!("{FILL}{extra}");

    // ---------------------------------------------------------------- cursor movement
    let cur = "cursor";
    v.push(case(cur, "CUP row;col", 20, 10, "\x1b[3;5H").cursor(4, 2));
    v.push(case(cur, "CUP default homes", 20, 10, "\x1b[5;5H\x1b[H").cursor(0, 0));
    v.push(case(cur, "CUP empty row param", 20, 10, "\x1b[;5H").cursor(4, 0));
    v.push(case(cur, "HVP", 20, 10, "\x1b[2;3f").cursor(2, 1));
    v.push(case(cur, "CUP clamps to the grid", 20, 10, "\x1b[99;99H").cursor(19, 9));
    v.push(case(cur, "CUU", 20, 10, "\x1b[5;5H\x1b[2A").cursor(4, 2));
    v.push(case(cur, "CUU clamps at top", 20, 10, "\x1b[2;5H\x1b[100A").cursor(4, 0));
    v.push(case(cur, "CUD clamps at bottom", 20, 10, "\x1b[5;5H\x1b[100B").cursor(4, 9));
    v.push(case(cur, "CUF clamps at right", 20, 10, "\x1b[100C").cursor(19, 0));
    v.push(case(cur, "CUB clamps at left", 20, 10, "\x1b[1;5H\x1b[100D").cursor(0, 0));
    v.push(case(cur, "CUF 0 is 1", 20, 10, "\x1b[0C").cursor(1, 0));
    v.push(case(cur, "CNL", 20, 10, "\x1b[3;5H\x1b[2E").cursor(0, 4));
    v.push(case(cur, "CPL", 20, 10, "\x1b[5;5H\x1b[2F").cursor(0, 2));
    v.push(case(cur, "CHA", 20, 10, "\x1b[2;3H\x1b[7G").cursor(6, 1));
    v.push(case(cur, "HPA", 20, 10, "\x1b[2;3H\x1b[9`").cursor(8, 1));
    v.push(case(cur, "HPR", 20, 10, "\x1b[2;3H\x1b[2a").cursor(4, 1));
    v.push(case(cur, "VPA", 20, 10, "\x1b[2;3H\x1b[7d").cursor(2, 6));
    v.push(case(cur, "VPR", 20, 10, "\x1b[2;3H\x1b[2e").cursor(2, 3));
    v.push(case(cur, "BS", 20, 10, "ab\x08").cursor(1, 0));
    v.push(case(cur, "BS stops at column 0", 20, 10, "\x08\x08").cursor(0, 0));
    v.push(case(cur, "CR", 20, 10, "abc\r").cursor(0, 0));
    v.push(case(cur, "LF keeps column", 20, 10, "abc\n").cursor(3, 1));
    v.push(case(cur, "VT and FF act as LF", 20, 10, "a\x0bb\x0cc").cursor(3, 2));
    v.push(case(cur, "NEL", 20, 10, "abc\x1bE").cursor(0, 1));
    v.push(case(cur, "IND", 20, 10, "abc\x1bD").cursor(3, 1));
    v.push(case(cur, "RI", 20, 10, "\x1b[3;3H\x1bM").cursor(2, 1));
    v.push(
        case(
            cur,
            "RI at top scrolls down",
            6,
            3,
            "AAA\r\nBBB\r\nCCC\x1b[H\x1bM",
        )
        .screen(&["", "AAA", "BBB"]),
    );
    v.push(case(cur, "HT", 40, 4, "\t").cursor(8, 0));
    v.push(case(cur, "HT from mid-stop", 40, 4, "a\t").cursor(8, 0));
    v.push(case(cur, "HT twice", 40, 4, "\t\t").cursor(16, 0));
    v.push(
        case(
            cur,
            "HT stops at last column, no wrap",
            20,
            4,
            "\x1b[1;19H\t",
        )
        .cursor(19, 0),
    );
    v.push(case(cur, "CHT", 40, 4, "\x1b[2I").cursor(16, 0));
    v.push(case(cur, "CBT", 40, 4, "\x1b[1;11H\x1b[Z").cursor(8, 0));
    v.push(case(cur, "TBC 3 clears all stops", 40, 4, "\x1b[3g\t").cursor(39, 0));
    v.push(
        case(
            cur,
            "HTS sets a stop",
            40,
            4,
            "\x1b[3g\x1b[1;6H\x1bH\x1b[1;1H\t",
        )
        .cursor(5, 0),
    );
    v.push(
        case(
            cur,
            "TBC 0 clears the stop at the cursor",
            40,
            4,
            "\x1b[1;9H\x1b[0g\x1b[1;1H\t",
        )
        .cursor(16, 0),
    );
    v.push(
        case(
            cur,
            "DECSC/DECRC restore position and SGR",
            20,
            4,
            "\x1b[2;4H\x1b[1;31m\x1b7\x1b[H\x1b[0m\x1b8X",
        )
        .cursor(4, 1)
        .cell(3, 1, attr::BOLD, Some(I(1)), None, None),
    );
    v.push(case(cur, "CSI s / CSI u", 20, 4, "\x1b[2;4H\x1b[s\x1b[H\x1b[u").cursor(3, 1));
    v.push(case(cur, "DECRC without save homes", 20, 4, "\x1b[2;4H\x1b8").cursor(0, 0));
    v.push(case(cur, "REP repeats the last character", 20, 4, "a\x1b[3b").screen(&["aaaa"]));
    v.push(
        case(
            cur,
            "CUP with origin mode is region relative",
            10,
            6,
            "\x1b[2;4r\x1b[?6h\x1b[1;1H",
        )
        .cursor(0, 1),
    );
    v.push(
        case(
            cur,
            "CUP clamps inside the region with origin mode",
            10,
            6,
            "\x1b[2;4r\x1b[?6h\x1b[99;1H",
        )
        .cursor(0, 3),
    );

    // ---------------------------------------------------------------- erase / edit
    let er = "erase";
    v.push(
        case(er, "ED 0", 6, 4, &fill("\x1b[2;3H\x1b[J"))
            .screen(&["AAAAAA", "BB"])
            .cursor(2, 1),
    );
    v.push(
        case(er, "ED 1", 6, 4, &fill("\x1b[2;3H\x1b[1J"))
            .screen(&["", "   BBB", "CCCCCC", "DDDDDD"]),
    );
    v.push(
        case(er, "ED 2 keeps the cursor", 6, 4, &fill("\x1b[2;3H\x1b[2J"))
            .screen(&[])
            .cursor(2, 1),
    );
    v.push(
        case(
            er,
            "ED 3 clears scrollback only",
            6,
            2,
            "a\r\nb\r\nc\r\nd\x1b[3J",
        )
        .screen(&["c", "d"])
        .history(0),
    );
    v.push(
        case(er, "EL 0", 6, 4, &fill("\x1b[2;3H\x1b[K"))
            .screen(&["AAAAAA", "BB", "CCCCCC", "DDDDDD"]),
    );
    v.push(
        case(er, "EL 1", 6, 4, &fill("\x1b[2;3H\x1b[1K"))
            .screen(&["AAAAAA", "   BBB", "CCCCCC", "DDDDDD"]),
    );
    v.push(
        case(er, "EL 2", 6, 4, &fill("\x1b[2;3H\x1b[2K"))
            .screen(&["AAAAAA", "", "CCCCCC", "DDDDDD"]),
    );
    v.push(
        case(er, "ECH", 6, 4, &fill("\x1b[2;3H\x1b[2X"))
            .screen(&["AAAAAA", "BB  BB", "CCCCCC", "DDDDDD"])
            .cursor(2, 1),
    );
    v.push(
        case(er, "DCH", 6, 4, &fill("\x1b[2;3H\x1b[2P"))
            .screen(&["AAAAAA", "BBBB", "CCCCCC", "DDDDDD"]),
    );
    v.push(
        case(er, "ICH", 6, 4, &fill("\x1b[2;3H\x1b[2@"))
            .screen(&["AAAAAA", "BB  BB", "CCCCCC", "DDDDDD"]),
    );
    v.push(
        case(er, "IL", 6, 4, &fill("\x1b[2;1H\x1b[L")).screen(&["AAAAAA", "", "BBBBBB", "CCCCCC"]),
    );
    v.push(
        case(
            er,
            "IL then DL restores the rows above the lost one",
            6,
            4,
            &fill("\x1b[2;1H\x1b[L\x1b[M"),
        )
        .screen(&["AAAAAA", "BBBBBB", "CCCCCC"]),
    );
    v.push(
        case(er, "DL pulls rows up", 6, 4, &fill("\x1b[2;1H\x1b[M"))
            .screen(&["AAAAAA", "CCCCCC", "DDDDDD"]),
    );
    v.push(
        case(
            er,
            "IL/DL are inert outside the scroll region",
            6,
            4,
            &fill("\x1b[2;3r\x1b[1;1H\x1b[L"),
        )
        .screen(&["AAAAAA", "BBBBBB", "CCCCCC", "DDDDDD"]),
    );
    v.push(
        case(
            er,
            "IL moves the cursor to column 0",
            6,
            4,
            &fill("\x1b[2;4H\x1b[L"),
        )
        .cursor(0, 1),
    );
    v.push(
        case(
            er,
            "EL uses the current background (BCE)",
            6,
            2,
            "\x1b[41m\x1b[2K",
        )
        .cell(0, 0, 0, None, Some(I(1)), None)
        .cell(5, 0, 0, None, Some(I(1)), None),
    );
    v.push(
        case(
            er,
            "ED 2 uses the current background (BCE)",
            6,
            2,
            "\x1b[44m\x1b[2J",
        )
        .cell(3, 1, 0, None, Some(I(4)), None),
    );
    v.push(
        case(
            er,
            "ICH inside a line with margins pending wrap",
            6,
            2,
            "abcdef\x1b[1;1H\x1b[@",
        )
        .screen(&[" abcde"]),
    );
    v.push(case(er, "DECALN fills with E", 4, 2, "\x1b#8").screen(&["EEEE", "EEEE"]));

    // ---------------------------------------------------------------- scroll regions
    let sc = "scroll";
    v.push(
        case(
            sc,
            "DECSTBM + LF at region bottom",
            6,
            4,
            "1\r\n2\r\n3\r\n4\x1b[2;3r\x1b[3;1H\n",
        )
        .screen(&["1", "3", "", "4"])
        .cursor(0, 2),
    );
    v.push(case(sc, "DECSTBM homes the cursor", 6, 4, "\x1b[3;3H\x1b[2;3r").cursor(0, 0));
    v.push(
        case(
            sc,
            "region scroll does not feed scrollback",
            6,
            4,
            "\x1b[2;3r\x1b[3;1H\n\n\n\n",
        )
        .history(0),
    );
    v.push(
        case(
            sc,
            "full-screen scroll feeds scrollback",
            6,
            2,
            "a\r\nb\r\nc\r\nd",
        )
        .history(2)
        .screen(&["c", "d"]),
    );
    v.push(case(sc, "SU", 6, 4, &fill("\x1b[S")).screen(&["BBBBBB", "CCCCCC", "DDDDDD"]));
    v.push(case(sc, "SD", 6, 4, &fill("\x1b[T")).screen(&["", "AAAAAA", "BBBBBB", "CCCCCC"]));
    v.push(
        case(sc, "SU 2 in a region", 6, 4, &fill("\x1b[2;4r\x1b[2S")).screen(&["AAAAAA", "DDDDDD"]),
    );
    v.push(
        case(sc, "SD in a region", 6, 4, &fill("\x1b[2;3r\x1b[T"))
            .screen(&["AAAAAA", "", "BBBBBB", "DDDDDD"]),
    );
    v.push(
        case(
            sc,
            "RI at region top scrolls the region",
            6,
            4,
            &fill("\x1b[2;3r\x1b[2;1H\x1bM"),
        )
        .screen(&["AAAAAA", "", "BBBBBB", "DDDDDD"]),
    );
    v.push(
        case(
            sc,
            "IND at region bottom",
            6,
            4,
            &fill("\x1b[2;3r\x1b[3;1H\x1bD"),
        )
        .screen(&["AAAAAA", "CCCCCC", "", "DDDDDD"]),
    );
    v.push(
        case(
            sc,
            "invalid region (top >= bottom) is ignored",
            6,
            4,
            &fill("\x1b[3;3r\x1b[4;1H\n"),
        )
        .screen(&["BBBBBB", "CCCCCC", "DDDDDD"]),
    );
    v.push(
        case(
            sc,
            "CSI r resets the region",
            6,
            4,
            &fill("\x1b[2;3r\x1b[r\x1b[4;1H\n"),
        )
        .screen(&["BBBBBB", "CCCCCC", "DDDDDD"]),
    );
    v.push(
        case(
            sc,
            "LF below the region does not scroll it",
            6,
            4,
            &fill("\x1b[1;2r\x1b[3;1H\n"),
        )
        .screen(&["AAAAAA", "BBBBBB", "CCCCCC", "DDDDDD"])
        .cursor(0, 3),
    );
    v.push(
        case(
            sc,
            "DECLRMM text wraps inside the margins",
            8,
            3,
            "\x1b[?69h\x1b[3;6s\x1b[1;3Habcdef",
        )
        .screen(&["  abcd", "  ef"]),
    );
    v.push(
        case(
            sc,
            "DECLRMM CR goes to the left margin",
            8,
            3,
            "\x1b[?69h\x1b[3;6s\x1b[1;4Hx\r",
        )
        .cursor(2, 0),
    );
    v.push(
        case(
            sc,
            "DECLRMM ICH stops at the right margin",
            8,
            2,
            "abcdefgh\x1b[?69h\x1b[3;6s\x1b[1;3H\x1b[@",
        )
        .screen(&["ab cdegh"]),
    );

    // ---------------------------------------------------------------- SGR
    let sg = "sgr";
    v.push(case(sg, "bold", 10, 2, "\x1b[1mX").attrs(attr::BOLD));
    v.push(case(sg, "dim", 10, 2, "\x1b[2mX").attrs(attr::DIM));
    v.push(case(sg, "italic", 10, 2, "\x1b[3mX").attrs(attr::ITALIC));
    v.push(case(sg, "underline", 10, 2, "\x1b[4mX").attrs(attr::UNDERLINE));
    v.push(case(sg, "double underline 21", 10, 2, "\x1b[21mX").attrs(attr::DOUBLE_UNDERLINE));
    v.push(case(sg, "undercurl 4:3", 10, 2, "\x1b[4:3mX").attrs(attr::UNDERCURL));
    v.push(case(sg, "dotted underline 4:4", 10, 2, "\x1b[4:4mX").attrs(attr::DOTTED_UNDERLINE));
    v.push(case(sg, "dashed underline 4:5", 10, 2, "\x1b[4:5mX").attrs(attr::DASHED_UNDERLINE));
    v.push(case(sg, "4:0 turns underline off", 10, 2, "\x1b[4m\x1b[4:0mX").attrs(0));
    v.push(case(sg, "blink", 10, 2, "\x1b[5mX").attrs(attr::BLINK));
    v.push(case(sg, "inverse", 10, 2, "\x1b[7mX").attrs(attr::INVERSE));
    v.push(case(sg, "hidden", 10, 2, "\x1b[8mX").attrs(attr::HIDDEN));
    v.push(case(sg, "strikethrough", 10, 2, "\x1b[9mX").attrs(attr::STRIKE));
    v.push(case(sg, "22 clears bold and dim", 10, 2, "\x1b[1;2m\x1b[22mX").attrs(0));
    v.push(
        case(
            sg,
            "23/24/25/27/28/29 clear",
            10,
            2,
            "\x1b[3;4;5;7;8;9m\x1b[23;24;25;27;28;29mX",
        )
        .attrs(0),
    );
    v.push(
        case(sg, "0 resets everything", 10, 2, "\x1b[1;3;31;42m\x1b[0mX")
            .attrs(0)
            .fg(Color::Default)
            .bg(Color::Default),
    );
    v.push(
        case(sg, "empty SGR resets", 10, 2, "\x1b[1;31m\x1b[mX")
            .attrs(0)
            .fg(Color::Default),
    );
    v.push(case(sg, "fg 31", 10, 2, "\x1b[31mX").fg(I(1)));
    v.push(case(sg, "bg 42", 10, 2, "\x1b[42mX").bg(I(2)));
    v.push(case(sg, "bright fg 91", 10, 2, "\x1b[91mX").fg(I(9)));
    v.push(case(sg, "bright bg 107", 10, 2, "\x1b[107mX").bg(I(15)));
    v.push(case(sg, "38;5;n", 10, 2, "\x1b[38;5;200mX").fg(I(200)));
    v.push(case(sg, "48;5;n", 10, 2, "\x1b[48;5;17mX").bg(I(17)));
    v.push(case(sg, "38;2;r;g;b", 10, 2, "\x1b[38;2;1;2;3mX").fg(Color::Rgb(1, 2, 3)));
    v.push(case(sg, "48;2;r;g;b", 10, 2, "\x1b[48;2;9;8;7mX").bg(Color::Rgb(9, 8, 7)));
    v.push(
        case(sg, "38:2::r:g:b colon form", 10, 2, "\x1b[38:2::10:20:30mX")
            .fg(Color::Rgb(10, 20, 30)),
    );
    v.push(case(sg, "38:5:n colon form", 10, 2, "\x1b[38:5:99mX").fg(I(99)));
    v.push(case(sg, "58;5;n underline colour", 10, 2, "\x1b[4m\x1b[58;5;9mX").ul(I(9)));
    v.push(
        case(
            sg,
            "58;2;r;g;b underline colour",
            10,
            2,
            "\x1b[4m\x1b[58;2;4;5;6mX",
        )
        .ul(Color::Rgb(4, 5, 6)),
    );
    v.push(
        case(
            sg,
            "59 resets underline colour",
            10,
            2,
            "\x1b[4m\x1b[58;5;9m\x1b[59mX",
        )
        .ul(Color::Default),
    );
    v.push(
        case(
            sg,
            "39/49 default colours",
            10,
            2,
            "\x1b[31;42m\x1b[39;49mX",
        )
        .fg(Color::Default)
        .bg(Color::Default),
    );
    v.push(
        case(
            sg,
            "combined 1;3;4;38;5;9;48;2;1;1;1",
            10,
            2,
            "\x1b[1;3;4;38;5;9;48;2;1;1;1mX",
        )
        .attrs(attr::BOLD | attr::ITALIC | attr::UNDERLINE)
        .fg(I(9))
        .bg(Color::Rgb(1, 1, 1)),
    );
    v.push(
        case(sg, "SGR persists across lines", 10, 3, "\x1b[1mA\r\nB").cell(
            0,
            1,
            attr::BOLD,
            None,
            None,
            None,
        ),
    );
    v.push(
        case(
            sg,
            "SGR survives erase of cells but BCE colour only",
            10,
            2,
            "\x1b[1;44m\x1b[K",
        )
        .cell(2, 0, 0, None, Some(I(4)), None),
    );
    v.push(case(sg, "unknown SGR parameter ignored", 10, 2, "\x1b[1;999mX").attrs(attr::BOLD));
    v.push(case(sg, "truncated 38;5 does not corrupt", 10, 2, "\x1b[38;5mX").screen(&["X"]));
    v.push(case(sg, "overline 53 does not break text", 10, 2, "\x1b[53mX").screen(&["X"]));

    // ---------------------------------------------------------------- DEC modes
    let dm = "modes";
    v.push(case(dm, "DECTCEM hide/show cursor", 10, 2, "\x1b[?25l").cursor_visible(false));
    v.push(case(dm, "DECTCEM show", 10, 2, "\x1b[?25l\x1b[?25h").cursor_visible(true));
    v.push(case(dm, "DECCKM", 10, 2, "\x1b[?1h").modes(|m| m.app_cursor));
    v.push(case(dm, "DECCKM reset", 10, 2, "\x1b[?1h\x1b[?1l").modes(|m| !m.app_cursor));
    v.push(case(dm, "bracketed paste", 10, 2, "\x1b[?2004h").modes(|m| m.bracketed_paste));
    v.push(case(dm, "focus events", 10, 2, "\x1b[?1004h").modes(|m| m.focus_events));
    v.push(case(dm, "mouse 1000", 10, 2, "\x1b[?1000h").modes(|m| m.mouse));
    v.push(case(dm, "mouse 1002+1006", 10, 2, "\x1b[?1002h\x1b[?1006h").modes(|m| m.mouse));
    v.push(case(dm, "mouse 1003", 10, 2, "\x1b[?1003h").modes(|m| m.mouse));
    v.push(case(dm, "mouse off", 10, 2, "\x1b[?1000h\x1b[?1000l").modes(|m| !m.mouse));
    v.push(
        case(
            dm,
            "alt screen 1049 keeps the cursor column",
            10,
            3,
            "main\x1b[?1049halt",
        )
        .modes(|m| m.alt_screen)
        .screen(&["    alt"])
        .cursor(7, 0),
    );
    v.push(
        case(
            dm,
            "alt screen 1049 restores main and cursor",
            10,
            3,
            "main\x1b[?1049h\x1b[2Jalt\x1b[?1049l",
        )
        .screen(&["main"])
        .cursor(4, 0)
        .modes(|m| !m.alt_screen),
    );
    v.push(
        case(
            dm,
            "alt screen has no scrollback",
            6,
            2,
            "x\r\ny\r\nz\x1b[?1049h1\r\n2\r\n3\r\n4",
        )
        .history(0),
    );
    v.push(case(dm, "alt screen 47", 10, 3, "main\x1b[?47hxx\x1b[?47l").modes(|m| !m.alt_screen));
    v.push(
        case(
            dm,
            "alt screen 1047 clears on exit",
            10,
            3,
            "\x1b[?1047hxx\x1b[?1047l\x1b[?1047h",
        )
        .screen(&[]),
    );
    v.push(
        case(
            dm,
            "DECAWM off overwrites the last column",
            5,
            2,
            "\x1b[?7labcdefg",
        )
        .screen(&["abcdg"])
        .cursor(4, 0),
    );
    v.push(
        case(dm, "DECAWM on wraps", 5, 2, "abcdefg")
            .screen(&["abcde", "fg"])
            .cursor(2, 1),
    );
    v.push(case(dm, "IRM insert mode", 10, 2, "abc\r\x1b[4hXY").screen(&["XYabc"]));
    v.push(case(dm, "IRM reset", 10, 2, "abc\r\x1b[4h\x1b[4lXY").screen(&["XYc"]));
    v.push(case(dm, "LNM makes LF return", 10, 3, "\x1b[20hab\nc").screen(&["ab", "c"]));
    v.push(
        case(dm, "DECSCUSR block blink", 10, 2, "\x1b[1 q").check(|o| {
            let c = o.e.cursor();
            (c.shape == vk_proto::render::CursorShape::Block && c.blink)
                .then_some(())
                .ok_or(format!("{c:?}"))
        }),
    );
    v.push(
        case(dm, "DECSCUSR steady underline", 10, 2, "\x1b[4 q").check(|o| {
            let c = o.e.cursor();
            (c.shape == vk_proto::render::CursorShape::Underline && !c.blink)
                .then_some(())
                .ok_or(format!("{c:?}"))
        }),
    );
    v.push(
        case(dm, "DECSCUSR blinking bar", 10, 2, "\x1b[5 q").check(|o| {
            let c = o.e.cursor();
            (c.shape == vk_proto::render::CursorShape::Bar && c.blink)
                .then_some(())
                .ok_or(format!("{c:?}"))
        }),
    );
    v.push(
        case(dm, "synchronized output 2026", 10, 2, "\x1b[?2026h").check(|o| {
            o.e.in_sync_update()
                .then_some(())
                .ok_or("not in sync update".into())
        }),
    );
    v.push(
        case(
            dm,
            "synchronized output ends",
            10,
            2,
            "\x1b[?2026h\x1b[?2026l",
        )
        .check(|o| {
            (!o.e.in_sync_update())
                .then_some(())
                .ok_or("still in sync update".into())
        }),
    );
    v.push(
        case(
            dm,
            "RIS resets screen, modes and cursor",
            10,
            3,
            "abc\x1b[?2004h\x1b[?1h\x1b[31m\x1bcX",
        )
        .screen(&["X"])
        .cursor(1, 0)
        .modes(|m| !m.bracketed_paste && !m.app_cursor),
    );
    v.push(case(dm, "DECSTR soft reset keeps the screen", 10, 3, "abc\x1b[?1h\x1b[!p").screen(&["abc"]).modes(|m| !m.app_cursor).xfail("libghostty-vt's DECSTR (CSI ! p) leaves DECCKM (application cursor keys) set; xterm resets it"));
    v.push(case(dm, "DEC special graphics", 10, 2, "\x1b(0lqk\x1b(B").screen(&["┌─┐"]));
    v.push(case(dm, "SO/SI with G1 graphics", 10, 2, "\x1b)0\x0eq\x0fq").screen(&["─q"]));
    v.push(
        case(dm, "DECRQM bracketed paste reset", 10, 2, "\x1b[?2004$p").replies("\x1b[?2004;2$y"),
    );
    v.push(
        case(
            dm,
            "DECRQM bracketed paste set",
            10,
            2,
            "\x1b[?2004h\x1b[?2004$p",
        )
        .replies("\x1b[?2004;1$y"),
    );
    v.push(case(dm, "DECRQM unknown mode", 10, 2, "\x1b[?9999$p").replies("\x1b[?9999;0$y"));
    v.push(case(dm, "DECRQM ANSI IRM", 10, 2, "\x1b[4h\x1b[4$p").replies("\x1b[4;1$y"));
    v.push(case(dm, "title OSC 0", 10, 2, "\x1b]0;hello\x07").title("hello"));
    v.push(case(dm, "title OSC 2 with ST", 10, 2, "\x1b]2;a b\x1b\\").title("a b"));
    v.push(
        case(
            dm,
            "title survives control chars in text",
            10,
            2,
            "\x1b]0;x\x07abc",
        )
        .title("x")
        .screen(&["abc"]),
    );

    // ---------------------------------------------------------------- OSC
    let os = "osc";
    v.push(
        case(os, "OSC 7 cwd", 10, 2, "\x1b]7;file://host/tmp/x%20y\x07")
            .effect(Effect::Cwd("/tmp/x y".into())),
    );
    v.push(
        case(
            os,
            "OSC 8 hyperlink text renders, no residue",
            20,
            2,
            "\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\ end",
        )
        .screen(&["link end"])
        .cursor(8, 0),
    );
    v.push(
        case(
            os,
            "OSC 8 with id param",
            20,
            2,
            "\x1b]8;id=a;https://e.x\x07hi\x1b]8;;\x07",
        )
        .screen(&["hi"])
        .check(|o| {
            let r = o.e.row(0);
            (r.links.len() == 1 && r.links[0].uri == "https://e.x" && r.links[0].cols == 2)
                .then_some(())
                .ok_or(format!("links {:?}", r.links))
        }),
    );
    v.push(
        case(
            os,
            "OSC 133 marks: prompt row and exit code",
            20,
            3,
            "\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07a\r\n\x1b]133;D;2\x07",
        )
        .effect(Effect::Mark {
            kind: 'D',
            exit: Some(2),
        })
        .check(|o| {
            let m: Vec<u8> = o.e.visible_rows().iter().map(|r| r.mark).collect();
            (m[0] == vk_proto::render::mark::PROMPT && m[1] == 0)
                .then_some(())
                .ok_or(format!("marks {m:?}"))
        }),
    );
    v.push(
        case(
            os,
            "OSC 1337 SetUserVar",
            20,
            2,
            "\x1b]1337;SetUserVar=who=dmliZWtl\x07",
        )
        .effect(Effect::UserVar {
            name: "who".into(),
            value: "vibeke".into(),
        })
        .screen(&[]),
    );
    v.push(
        case(os, "OSC 9;4 paused progress", 20, 2, "\x1b]9;4;4;70\x07")
            .effect(Effect::Progress {
                state: 4,
                pct: Some(70),
            })
            .screen(&[]),
    );
    v.push(
        case(os, "OSC 52 set", 10, 2, "\x1b]52;c;aGVsbG8=\x07").effect(Effect::Clipboard {
            primary: false,
            data: b"hello".to_vec(),
        }),
    );
    v.push(
        case(os, "OSC 52 set primary", 10, 2, "\x1b]52;p;aGk=\x07").effect(Effect::Clipboard {
            primary: true,
            data: b"hi".to_vec(),
        }),
    );
    v.push(
        case(os, "OSC 52 query", 10, 2, "\x1b]52;c;?\x07")
            .effect(Effect::ClipboardQuery { primary: false }),
    );
    v.push(
        case(
            os,
            "OSC 52 invalid base64 is not delivered",
            10,
            2,
            "\x1b]52;c;!!!\x07",
        )
        .check(|o| {
            (!o.fx.iter().any(|f| matches!(f, Effect::Clipboard { .. })))
                .then_some(())
                .ok_or("delivered".into())
        }),
    );
    v.push(
        case(os, "OSC 133 A", 10, 2, "\x1b]133;A\x07").effect(Effect::Mark {
            kind: 'A',
            exit: None,
        }),
    );
    v.push(
        case(os, "OSC 133 B/C", 10, 2, "\x1b]133;B\x07\x1b]133;C\x07")
            .effect(Effect::Mark {
                kind: 'B',
                exit: None,
            })
            .effect(Effect::Mark {
                kind: 'C',
                exit: None,
            }),
    );
    v.push(
        case(os, "OSC 133 D with exit code", 10, 2, "\x1b]133;D;7\x07").effect(Effect::Mark {
            kind: 'D',
            exit: Some(7),
        }),
    );
    v.push(
        case(os, "OSC 133 with ST terminator", 10, 2, "\x1b]133;A\x1b\\").effect(Effect::Mark {
            kind: 'A',
            exit: None,
        }),
    );
    v.push(
        case(os, "OSC 9 notification", 10, 2, "\x1b]9;build done\x07").effect(Effect::Notify {
            kind: NotifyKind::Osc9,
            title: None,
            body: "build done".into(),
        }),
    );
    v.push(
        case(os, "OSC 777 notify", 10, 2, "\x1b]777;notify;T;B\x07").effect(Effect::Notify {
            kind: NotifyKind::Osc777,
            title: Some("T".into()),
            body: "B".into(),
        }),
    );
    v.push(
        case(os, "OSC 9;4 progress", 10, 2, "\x1b]9;4;1;40\x07").effect(Effect::Progress {
            state: 1,
            pct: Some(40),
        }),
    );
    v.push(case(os, "BEL", 10, 2, "\x07").effect(Effect::Bell));
    v.push(case(os, "OSC 10 query", 10, 2, "\x1b]10;?\x07").reply_prefix("\x1b]10;rgb:", "\x07"));
    v.push(case(os, "OSC 11 query", 10, 2, "\x1b]11;?\x07").reply_prefix("\x1b]11;rgb:", "\x07"));
    v.push(case(os, "OSC 4 query", 10, 2, "\x1b]4;1;?\x07").reply_prefix("\x1b]4;1;rgb:", "\x07"));
    v.push(
        case(
            os,
            "OSC 11 set then query",
            10,
            2,
            "\x1b]11;rgb:11/22/33\x07\x1b]11;?\x07",
        )
        .reply_prefix("\x1b]11;rgb:1111/2222/3333", "\x07"),
    );
    v.push(
        case(
            os,
            "unterminated OSC then text after ST",
            10,
            2,
            "\x1b]0;abc\x1b\\ok",
        )
        .screen(&["ok"]),
    );
    v.push(case(os, "CAN aborts a sequence", 10, 2, "\x1b[3\x18ok").screen(&["ok"]));
    v.push(case(os, "SUB aborts a sequence", 10, 2, "\x1b[3\x1aok").screen(&["ok"]));
    v.push(case(os, "DCS is swallowed", 10, 2, "\x1bPq#0;2;0;0;0\x1b\\ok").screen(&["ok"]));
    v.push(case(os, "APC is swallowed", 10, 2, "\x1b_Gi=1;AAAA\x1b\\ok").screen(&["ok"]));

    // ---------------------------------------------------------------- kitty keyboard flags
    let kk = "kitty-keyboard";
    v.push(case(kk, "query with nothing pushed", 10, 2, "\x1b[?u").replies("\x1b[?0u"));
    v.push(case(kk, "push", 10, 2, "\x1b[>1u").modes(|m| m.kitty_flags == 1));
    v.push(case(kk, "push then query", 10, 2, "\x1b[>5u\x1b[?u").replies("\x1b[?5u"));
    v.push(
        case(kk, "push, push, pop 1", 10, 2, "\x1b[>1u\x1b[>2u\x1b[<u")
            .modes(|m| m.kitty_flags == 1),
    );
    v.push(case(kk, "pop 2", 10, 2, "\x1b[>1u\x1b[>2u\x1b[<2u").modes(|m| m.kitty_flags == 0));
    v.push(
        case(kk, "pop more than the stack", 10, 2, "\x1b[>1u\x1b[<9u")
            .modes(|m| m.kitty_flags == 0),
    );
    v.push(case(kk, "set (mode 1)", 10, 2, "\x1b[=3u").modes(|m| m.kitty_flags == 3));
    v.push(case(kk, "set or (mode 2)", 10, 2, "\x1b[=1u\x1b[=2;2u").modes(|m| m.kitty_flags == 3));
    v.push(
        case(kk, "set clear bits (mode 3)", 10, 2, "\x1b[=7u\x1b[=2;3u")
            .modes(|m| m.kitty_flags == 5),
    );
    v.push(case(kk, "all five flags", 10, 2, "\x1b[>31u").modes(|m| m.kitty_flags == 31));
    v.push(
        case(
            kk,
            "alt screen has its own stack",
            10,
            2,
            "\x1b[>1u\x1b[?1049h",
        )
        .modes(|m| m.kitty_flags == 0),
    );
    v.push(
        case(
            kk,
            "main stack restored after alt",
            10,
            2,
            "\x1b[>1u\x1b[?1049h\x1b[>4u\x1b[?1049l",
        )
        .modes(|m| m.kitty_flags == 1),
    );
    v.push(case(kk, "RIS clears the stack", 10, 2, "\x1b[>1u\x1bc").modes(|m| m.kitty_flags == 0));
    v.push(
        case(kk, "modifyOtherKeys is tracked", 10, 2, "\x1b[>4;2m").check(|o| {
            (o.e.modify_other_keys() == 2)
                .then_some(())
                .ok_or(format!("level {}", o.e.modify_other_keys()))
        }),
    );

    // ---------------------------------------------------------------- DA / DSR / reports
    let rp = "reports";
    v.push(case(rp, "DSR 5 status", 10, 2, "\x1b[5n").replies("\x1b[0n"));
    v.push(case(rp, "DSR 6 cursor position", 20, 10, "\x1b[4;7H\x1b[6n").replies("\x1b[4;7R"));
    v.push(
        case(
            rp,
            "DSR 6 origin mode is region relative",
            20,
            10,
            "\x1b[3;8r\x1b[?6h\x1b[2;2H\x1b[6n",
        )
        .replies("\x1b[2;2R"),
    );
    v.push(case(rp, "DECXCPR ?6n", 20, 10, "\x1b[4;7H\x1b[?6n").replies("\x1b[?4;7;1R").xfail("libghostty-vt sends no reply to DECXCPR (CSI ? 6 n); xterm answers CSI ? row ; col ; page R"));
    v.push(case(rp, "DA1", 10, 2, "\x1b[c").reply_prefix("\x1b[?", "c"));
    v.push(case(rp, "DA1 with 0 param", 10, 2, "\x1b[0c").reply_prefix("\x1b[?", "c"));
    v.push(case(rp, "DA2", 10, 2, "\x1b[>c").reply_prefix("\x1b[>", "c"));
    v.push(case(rp, "DA3", 10, 2, "\x1b[=c").reply_prefix("\x1bP!|", "\x1b\\"));
    v.push(case(rp, "XTVERSION", 10, 2, "\x1b[>q").reply_prefix("\x1bP>|", "\x1b\\"));
    v.push(
        case(rp, "DECRQSS SGR", 10, 2, "\x1b[1m\x1bP$qm\x1b\\").reply_prefix("\x1bP1$r", "\x1b\\"),
    );
    v.push(
        case(rp, "DECRQSS DECSTBM", 20, 10, "\x1b[2;5r\x1bP$qr\x1b\\")
            .replies("\x1bP1$r2;5r\x1b\\"),
    );
    v.push(
        case(rp, "DECRQSS DECSCUSR", 20, 10, "\x1b[4 q\x1bP$q q\x1b\\")
            .replies("\x1bP1$r4 q\x1b\\"),
    );
    v.push(case(rp, "DECRQSS invalid", 20, 10, "\x1bP$qzz\x1b\\").replies("\x1bP0$r\x1b\\"));
    v.push(case(rp, "XTWINOPS 18 text area chars", 80, 24, "\x1b[18t").replies("\x1b[8;24;80t"));
    v.push(case(rp, "XTWINOPS 14 text area pixels", 80, 24, "\x1b[14t").replies("\x1b[4;384;640t"));
    v.push(case(rp, "XTWINOPS 16 cell pixels", 80, 24, "\x1b[16t").replies("\x1b[6;16;8t"));
    v.push(
        case(
            rp,
            "replies arrive in order",
            20,
            10,
            "\x1b[5n\x1b[4;7H\x1b[6n",
        )
        .replies("\x1b[0n\x1b[4;7R"),
    );
    v.push(case(rp, "no reply for plain text", 10, 2, "hello").replies(""));

    // ---------------------------------------------------------------- wide / combining / wrap
    let wc = "unicode";
    v.push(
        case(wc, "CJK wide takes two columns", 8, 2, "日本")
            .screen(&["日本"])
            .cursor(4, 0),
    );
    v.push(case(wc, "wide span width", 8, 2, "a日b").check(|o| {
        let cols: u16 = o.e.row(0).spans.iter().map(|s| s.cols).sum();
        (cols == 4)
            .then_some(())
            .ok_or(format!("row covers {cols} columns"))
    }));
    v.push(
        case(wc, "wide char at last column wraps whole", 5, 2, "abcd日")
            .screen(&["abcd", "日"])
            .cursor(2, 1)
            .wrapped(0, true),
    );
    v.push(
        case(wc, "combining mark shares the cell", 8, 2, "e\u{301}x")
            .screen(&["e\u{301}x"])
            .cursor(2, 0),
    );
    v.push(
        case(
            wc,
            "several combining marks",
            8,
            2,
            "a\u{300}\u{301}\u{302}b",
        )
        .cursor(2, 0),
    );
    v.push(case(wc, "emoji is wide", 8, 2, "🎉x").cursor(3, 0));
    v.push(
        case(
            wc,
            "without mode 2027 a ZWJ family is three wide emoji (legacy per-codepoint widths)",
            8,
            2,
            "👨\u{200d}👩\u{200d}👧x",
        )
        .cursor(7, 0),
    );
    v.push(
        case(
            wc,
            "without mode 2027 VS16 does not widen a text symbol",
            8,
            2,
            "\u{2764}\u{fe0f}x",
        )
        .cursor(2, 0),
    );
    v.push(
        case(
            wc,
            "without mode 2027 a flag is two wide indicators",
            8,
            2,
            "🇳🇴x",
        )
        .cursor(5, 0),
    );
    v.push(
        case(
            wc,
            "without mode 2027 a skin tone modifier is its own wide cell",
            8,
            2,
            "👍🏽x",
        )
        .cursor(5, 0),
    );
    v.push(
        case(
            wc,
            "mode 2027 ZWJ family is one wide grapheme",
            8,
            2,
            "\x1b[?2027h👨\u{200d}👩\u{200d}👧x",
        )
        .cursor(3, 0),
    );
    v.push(
        case(
            wc,
            "mode 2027 VS16 widens a text symbol",
            8,
            2,
            "\x1b[?2027h\u{2764}\u{fe0f}x",
        )
        .cursor(3, 0),
    );
    v.push(
        case(
            wc,
            "mode 2027 flag is one wide grapheme",
            8,
            2,
            "\x1b[?2027h🇳🇴x",
        )
        .cursor(3, 0),
    );
    v.push(case(wc, "mode 2027 skin tone joins", 8, 2, "\x1b[?2027h👍🏽x").cursor(3, 0));
    v.push(
        case(wc, "DECRQM 2027 is recognised", 8, 2, "\x1b[?2027$p")
            .reply_prefix("\x1b[?2027;", "$y"),
    );
    v.push(
        case(
            wc,
            "overwriting the tail of a wide char blanks its head",
            8,
            2,
            "日\x1b[1;2HX",
        )
        .screen(&[" X"]),
    );
    v.push(
        case(
            wc,
            "overwriting the head of a wide char blanks its tail",
            8,
            2,
            "日\x1b[1;1HX",
        )
        .screen(&["X"])
        .cursor(1, 0),
    );
    v.push(
        case(
            wc,
            "EL through a wide char leaves no half cell",
            8,
            2,
            "a日b\x1b[1;3H\x1b[K",
        )
        .screen(&["a"]),
    );
    v.push(
        case(
            wc,
            "ICH pushing a wide char off the edge drops it",
            4,
            2,
            "ab日\x1b[1;1H\x1b[@",
        )
        .screen(&[" ab"]),
    );
    v.push(case(wc, "UTF-8 split across feeds", 8, 2, "é日").screen(&["é日"]));
    let mut bad = case(
        wc,
        "invalid UTF-8 yields replacement chars, parser recovers",
        8,
        2,
        "",
    );
    bad.input = b"a\xff\xfeb".to_vec();
    v.push(bad.check(|o| {
        let s = o.e.screen_text().trim_end().to_string();
        s.ends_with('b').then_some(()).ok_or(format!("{s:?}"))
    }));
    v.push(
        case(
            wc,
            "C1 8-bit controls are printed or ignored, never CSI",
            8,
            2,
            "\u{9b}31mX",
        )
        .check(|o| {
            let st = o.e.row(0).spans[0].style;
            (st.fg == Color::Default)
                .then_some(())
                .ok_or(format!("{st:?}"))
        }),
    );
    let wr = "wrap";
    v.push(
        case(
            wr,
            "last column is a pending wrap, not a wrap",
            5,
            2,
            "abcde",
        )
        .screen(&["abcde"])
        .cursor(4, 0)
        .wrapped(0, false),
    );
    v.push(
        case(wr, "next char wraps and marks the row", 5, 2, "abcdef")
            .screen(&["abcde", "f"])
            .cursor(1, 1)
            .wrapped(0, true),
    );
    v.push(
        case(
            wr,
            "CR after pending wrap overwrites the same row",
            5,
            2,
            "abcde\rx",
        )
        .screen(&["xbcde"])
        .cursor(1, 0),
    );
    v.push(case(wr, "CUP clears the pending wrap", 5, 2, "abcde\x1b[1;5Hz").screen(&["abcdz"]));
    v.push(
        case(
            wr,
            "BS from pending wrap goes to column 3",
            5,
            2,
            "abcde\x08",
        )
        .cursor(3, 0),
    );
    v.push(
        case(
            wr,
            "LF keeps the pending state out of the next line",
            5,
            3,
            "abcde\nx",
        )
        .screen(&["abcde", "    x"]),
    );
    v.push(
        case(wr, "wrapping at the bottom scrolls", 5, 2, "abcdefghijk")
            .screen(&["fghij", "k"])
            .history(1),
    );
    v.push(
        case(
            wr,
            "soft-wrapped rows are marked, hard breaks are not",
            5,
            3,
            "abcdefg\r\nxy",
        )
        .wrapped(0, true)
        .wrapped(1, false),
    );
    v.push(
        case(wr, "wide char wraps and keeps marking", 5, 3, "abcd日日")
            .screen(&["abcd", "日日"])
            .wrapped(0, true),
    );
    v.push(
        case(
            wr,
            "tab at the last column does not wrap",
            10,
            2,
            "\x1b[1;10H\t",
        )
        .cursor(9, 0),
    );
    v.push(
        case(wr, "resize clamps the cursor", 10, 3, "hello\x1b[3;9H").check(|o| {
            let mut e = Engine::new(10, 3, 10);
            let mut fx = Vec::new();
            e.feed(b"hello\x1b[3;9H", &mut fx);
            e.resize(5, 2);
            let c = e.cursor();
            let _ = o;
            (c.col < 5 && c.row < 2)
                .then_some(())
                .ok_or(format!("{c:?}"))
        }),
    );
    v
}

fn report() -> bool {
    std::env::var("VK_CONFORMANCE_REPORT").is_ok()
}

#[test]
fn conformance_cases() {
    let cs = cases();
    let mut failures = Vec::new();
    let mut by_cat: std::collections::BTreeMap<&str, (u32, u32, u32)> = Default::default();
    let mut names = std::collections::HashSet::new();
    for c in &cs {
        assert!(
            names.insert((c.cat, c.name)),
            "duplicate case {}/{}",
            c.cat,
            c.name
        );
        let whole = run(c, false);
        let res = check(c, &whole);
        let e = by_cat.entry(c.cat).or_default();
        match (&res, c.xfail) {
            (Ok(()), None) => e.0 += 1,
            (Err(_), Some(_)) => e.2 += 1,
            (Ok(()), Some(why)) => {
                e.1 += 1;
                failures.push(format!(
                    "[{}] {}: expected failure now PASSES; remove the xfail marker ({why})",
                    c.cat, c.name
                ));
            }
            (Err(m), None) => {
                e.1 += 1;
                failures.push(format!("[{}] {}: {m}", c.cat, c.name));
            }
        }
        if report()
            && let (Err(m), Some(why)) = (&res, c.xfail)
        {
            println!("XFAIL [{}] {}: {m}\n      note: {why}", c.cat, c.name);
        }
    }
    if report() {
        let total: u32 = by_cat.values().map(|v| v.0 + v.1 + v.2).sum();
        let pass: u32 = by_cat.values().map(|v| v.0).sum();
        let xf: u32 = by_cat.values().map(|v| v.2).sum();
        for (k, (p, f, x)) in &by_cat {
            println!("{k:<16} pass {p:>3}  fail {f:>3}  xfail {x:>3}");
        }
        println!("TOTAL {total}: pass {pass}, expected failures {xf}");
    }
    assert!(
        failures.is_empty(),
        "{} conformance failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The parser must not depend on how the byte stream is chunked: byte-at-a-time feeding gives
/// the same screen, cursor and modes as one write.
#[test]
fn conformance_split_invariance() {
    let mut bad = Vec::new();
    for c in cases() {
        let a = run(&c, false);
        let b = run(&c, true);
        let same = a.e.screen_text() == b.e.screen_text()
            && a.e.cursor() == b.e.cursor()
            && a.e.modes() == b.e.modes()
            && a.replies == b.replies
            && a.fx == b.fx;
        if !same {
            bad.push(format!("[{}] {}", c.cat, c.name));
        }
    }
    assert!(
        bad.is_empty(),
        "chunking changed the result:\n{}",
        bad.join("\n")
    );
}
