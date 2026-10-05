//! The one VT engine (03 §1): `alacritty_terminal` (vendored with a snapshot patch) plus the
//! [`Tracker`] for parser-state capture and the OSCs alacritty ignores.

use crate::tracker::{Tracked, Tracker, params};
use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{self, Term, TermDamage, TermMode, TermSnapshot};
use alacritty_terminal::vte::ansi::{
    self, Color as AColor, CursorShape as ACursorShape, NamedColor, Processor, Rgb,
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use vk_proto::render::{Color, Cursor, CursorShape, PaneModes, Row, Span, Style, attr};

pub const ENGINE: &str = "alacritty_terminal";
pub const ENGINE_VERSION: &str = "0.26.0+vibeke.1";

#[derive(Clone, Default)]
struct Listener(Arc<Mutex<Vec<Event>>>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        if let Ok(mut v) = self.0.lock() {
            v.push(event);
        }
    }
}

/// Disables the processor's synchronized-update buffering: every byte is applied immediately
/// so the grid always reflects the journal offset. Vibeke paces frames itself (03 §12).
#[derive(Default)]
pub struct NoSync;

impl ansi::Timeout for NoSync {
    fn set_timeout(&mut self, _: Duration) {}
    fn clear_timeout(&mut self) {}
    fn pending_timeout(&self) -> bool {
        false
    }
}

struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyKind {
    Osc9,
    Osc99,
    Osc777,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Bytes to write back to the PTY (query replies).
    Reply(Vec<u8>),
    Bell,
    TitleChanged,
    Notify {
        kind: NotifyKind,
        title: Option<String>,
        body: String,
    },
    Clipboard {
        primary: bool,
        data: Vec<u8>,
    },
    ClipboardQuery {
        primary: bool,
    },
    Cwd(String),
    /// OSC 133 shell-integration mark on the cursor's row.
    Mark {
        kind: char,
        exit: Option<i32>,
    },
    Progress {
        state: u8,
        pct: Option<u8>,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Extra {
    cwd: Option<String>,
    modify_other_keys: u8,
    sync: bool,
}

#[derive(Serialize, Deserialize)]
struct Snapshot {
    engine_version: String,
    term: TermSnapshot,
    pending: Vec<u8>,
    extra: Extra,
}

pub struct Engine {
    term: Term<Listener>,
    parser: Processor<NoSync>,
    tracker: Tracker,
    events: Listener,
    extra: Extra,
    replaying: bool,
    tracked: Vec<Tracked>,
    palette: Palette,
    cell_px: (u16, u16),
}

/// Default colours used to answer OSC 10/11/12/4 queries.
#[derive(Clone, Debug)]
pub struct Palette {
    pub fg: (u8, u8, u8),
    pub bg: (u8, u8, u8),
    pub cursor: (u8, u8, u8),
    pub ansi: [(u8, u8, u8); 16],
}

impl Default for Palette {
    fn default() -> Self {
        Palette {
            fg: (0xcd, 0xd6, 0xf4),
            bg: (0x1e, 0x1e, 0x2e),
            cursor: (0xf5, 0xe0, 0xdc),
            ansi: [
                (0x45, 0x47, 0x5a),
                (0xf3, 0x8b, 0xa8),
                (0xa6, 0xe3, 0xa1),
                (0xf9, 0xe2, 0xaf),
                (0x89, 0xb4, 0xfa),
                (0xf5, 0xc2, 0xe7),
                (0x94, 0xe2, 0xd5),
                (0xba, 0xc2, 0xde),
                (0x58, 0x5b, 0x70),
                (0xf3, 0x8b, 0xa8),
                (0xa6, 0xe3, 0xa1),
                (0xf9, 0xe2, 0xaf),
                (0x89, 0xb4, 0xfa),
                (0xf5, 0xc2, 0xe7),
                (0x94, 0xe2, 0xd5),
                (0xa6, 0xad, 0xc8),
            ],
        }
    }
}

fn config(scrollback: usize) -> term::Config {
    term::Config {
        scrolling_history: scrollback,
        kitty_keyboard: true,
        osc52: term::Osc52::CopyPaste,
        ..Default::default()
    }
}

impl Engine {
    pub fn new(cols: u16, rows: u16, scrollback: usize) -> Self {
        let events = Listener::default();
        let size = Size {
            cols: cols.max(2) as usize,
            rows: rows.max(1) as usize,
        };
        Engine {
            term: Term::new(config(scrollback), &size, events.clone()),
            parser: Processor::new(),
            tracker: Tracker::default(),
            events,
            extra: Extra::default(),
            replaying: false,
            tracked: Vec::new(),
            palette: Palette::default(),
            cell_px: (8, 16),
        }
    }

    pub fn set_palette(&mut self, p: Palette) {
        self.palette = p;
    }

    pub fn cols(&self) -> u16 {
        self.term.grid().columns() as u16
    }
    pub fn rows(&self) -> u16 {
        self.term.grid().screen_lines() as u16
    }

    /// While replaying the journal after a restart, side effects are suppressed (01 §1.2).
    pub fn set_replaying(&mut self, r: bool) {
        self.replaying = r;
    }
    pub fn replaying(&self) -> bool {
        self.replaying
    }

    /// True between `CSI ? 2026 h` and `l` (synchronized output).
    pub fn in_sync_update(&self) -> bool {
        self.extra.sync
    }

    pub fn cwd(&self) -> Option<&str> {
        self.extra.cwd.as_deref()
    }

    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<Effect>) {
        self.tracked.clear();
        self.tracker.feed(bytes, &mut self.tracked);
        self.parser.advance(&mut self.term, bytes);
        let tracked = std::mem::take(&mut self.tracked);
        for t in &tracked {
            self.on_tracked(t, out);
        }
        self.tracked = tracked;
        let events: Vec<Event> = std::mem::take(&mut *self.events.0.lock().unwrap());
        for ev in events {
            self.on_event(ev, out);
        }
        if self.replaying {
            out.retain(|e| matches!(e, Effect::TitleChanged | Effect::Cwd(_)));
        }
    }

    fn on_tracked(&mut self, t: &Tracked, out: &mut Vec<Effect>) {
        match t {
            Tracked::Csi {
                private,
                params: p,
                inter,
                fin,
            } if inter.is_empty() => match (private, fin) {
                (Some(b'>'), b'q') => {
                    out.push(Effect::Reply(vk_proto::ident::xtversion().into_bytes()))
                }
                (Some(b'='), b'c') => {
                    out.push(Effect::Reply(vk_proto::ident::DA3.as_bytes().to_vec()))
                }
                (Some(b'>'), b'm') => {
                    let ps = params(p);
                    if ps.first() == Some(&4) {
                        self.extra.modify_other_keys = ps.get(1).copied().unwrap_or(0).min(2) as u8;
                    }
                }
                (Some(b'?'), b'h' | b'l') => {
                    if params(p).contains(&2026) {
                        self.extra.sync = *fin == b'h';
                    }
                }
                _ => {}
            },
            Tracked::Csi { .. } => {}
            Tracked::Osc(body) => self.on_osc(body, out),
        }
    }

    fn on_osc(&mut self, body: &[u8], out: &mut Vec<Effect>) {
        let s = String::from_utf8_lossy(body);
        let (num, rest) = s.split_once(';').unwrap_or((&s, ""));
        match num {
            "7" => {
                // file://host/path
                let path = rest
                    .strip_prefix("file://")
                    .map(|r| r.find('/').map(|i| &r[i..]).unwrap_or(""))
                    .unwrap_or(rest);
                let path = percent_decode(path);
                if !path.is_empty() && self.extra.cwd.as_deref() != Some(&path) {
                    self.extra.cwd = Some(path.clone());
                    out.push(Effect::Cwd(path));
                }
            }
            "9" => {
                if let Some(r) = rest.strip_prefix("4;") {
                    let mut it = r.split(';');
                    let state = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
                    let pct = it.next().and_then(|x| x.parse().ok());
                    out.push(Effect::Progress { state, pct });
                } else {
                    out.push(Effect::Notify {
                        kind: NotifyKind::Osc9,
                        title: None,
                        body: rest.to_string(),
                    });
                }
            }
            "777" => {
                let mut it = rest.splitn(3, ';');
                if it.next() == Some("notify") {
                    let title = it.next().map(str::to_string);
                    let body = it.next().unwrap_or("").to_string();
                    out.push(Effect::Notify {
                        kind: NotifyKind::Osc777,
                        title,
                        body,
                    });
                }
            }
            "99" => {
                // OSC 99 ; metadata ; payload  (kitty). We support single-chunk title/body.
                let (meta, payload) = rest.split_once(';').unwrap_or(("", rest));
                let is_body = meta.split(':').any(|kv| kv == "p=body");
                if is_body {
                    out.push(Effect::Notify {
                        kind: NotifyKind::Osc99,
                        title: None,
                        body: payload.to_string(),
                    });
                } else {
                    out.push(Effect::Notify {
                        kind: NotifyKind::Osc99,
                        title: Some(payload.to_string()),
                        body: String::new(),
                    });
                }
            }
            "133" => {
                let mut it = rest.split(';');
                if let Some(k) = it.next().and_then(|k| k.chars().next()) {
                    let exit = if k == 'D' {
                        it.next().and_then(|x| x.parse().ok())
                    } else {
                        None
                    };
                    out.push(Effect::Mark { kind: k, exit });
                }
            }
            _ => {}
        }
    }

    fn on_event(&mut self, ev: Event, out: &mut Vec<Effect>) {
        match ev {
            Event::PtyWrite(s) => {
                // Identify consistently with the holder (01 §1.2).
                let s = if s == "\x1b[?6c" {
                    vk_proto::ident::DA1.to_string()
                } else if s.starts_with("\x1b[>0;") && s.ends_with('c') {
                    vk_proto::ident::DA2.to_string()
                } else {
                    s
                };
                out.push(Effect::Reply(s.into_bytes()));
            }
            Event::Bell => out.push(Effect::Bell),
            Event::Title(_) | Event::ResetTitle => out.push(Effect::TitleChanged),
            Event::ClipboardStore(ty, data) => out.push(Effect::Clipboard {
                primary: matches!(ty, term::ClipboardType::Selection),
                data: data.into_bytes(),
            }),
            Event::ClipboardLoad(ty, _) => out.push(Effect::ClipboardQuery {
                primary: matches!(ty, term::ClipboardType::Selection),
            }),
            Event::ColorRequest(idx, fmt) => {
                let rgb = match idx {
                    0..=15 => self.palette.ansi[idx],
                    256 | 267 => self.palette.fg,
                    257 | 268 => self.palette.bg,
                    258 => self.palette.cursor,
                    16..=255 => xterm256(idx as u8),
                    _ => self.palette.fg,
                };
                out.push(Effect::Reply(
                    fmt(Rgb {
                        r: rgb.0,
                        g: rgb.1,
                        b: rgb.2,
                    })
                    .into_bytes(),
                ));
            }
            Event::TextAreaSizeRequest(fmt) => {
                let ws = WindowSize {
                    num_lines: self.rows(),
                    num_cols: self.cols(),
                    cell_width: self.cell_px.0,
                    cell_height: self.cell_px.1,
                };
                out.push(Effect::Reply(fmt(ws).into_bytes()));
            }
            _ => {}
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let size = Size {
            cols: cols.max(2) as usize,
            rows: rows.max(1) as usize,
        };
        self.term.resize(size);
    }

    pub fn title(&self) -> String {
        // Term keeps the title private behind events; track via snapshot.
        self.term.title_str().unwrap_or_default().to_string()
    }

    pub fn modes(&self) -> PaneModes {
        let m = self.term.mode();
        PaneModes {
            alt_screen: m.contains(TermMode::ALT_SCREEN),
            mouse: m.intersects(TermMode::MOUSE_MODE),
            bracketed_paste: m.contains(TermMode::BRACKETED_PASTE),
            focus_events: m.contains(TermMode::FOCUS_IN_OUT),
            app_cursor: m.contains(TermMode::APP_CURSOR),
            kitty_flags: kitty_flags(*m),
        }
    }

    pub fn term_mode(&self) -> u32 {
        self.term.mode().bits()
    }

    pub fn modify_other_keys(&self) -> u8 {
        self.extra.modify_other_keys
    }

    pub fn cursor(&self) -> Cursor {
        let g = self.term.grid();
        let p = g.cursor.point;
        let style = self.term.cursor_style();
        let shape = match style.shape {
            ACursorShape::Underline => CursorShape::Underline,
            ACursorShape::Beam => CursorShape::Bar,
            _ => CursorShape::Block,
        };
        Cursor {
            col: p.column.0.min(g.columns().saturating_sub(1)) as u16,
            row: p.line.0.max(0) as u16,
            visible: self.term.mode().contains(TermMode::SHOW_CURSOR)
                && style.shape != ACursorShape::Hidden,
            shape,
            blink: style.blinking,
        }
    }

    /// Visible row `y` (0 = top) as render spans.
    pub fn row(&self, y: u16) -> Row {
        row_from(
            &self.term.grid()[Line(y as i32)],
            self.term.grid().columns(),
        )
    }

    pub fn visible_rows(&self) -> Vec<Row> {
        (0..self.rows()).map(|y| self.row(y)).collect()
    }

    /// Scrollback rows kept in memory (primary screen; the alt screen has none).
    pub fn history_len(&self) -> usize {
        self.term.primary_grid().history_size()
    }

    /// Scrollback row `idx` where 0 is the oldest row kept in memory.
    pub fn history_row(&self, idx: usize) -> Option<Row> {
        let h = self.history_len();
        if idx >= h {
            return None;
        }
        let line = Line(-(h as i32) + idx as i32);
        Some(row_from(
            &self.term.grid()[line],
            self.term.grid().columns(),
        ))
    }

    /// Lines (visible rows) damaged since the last call; `None` means everything.
    pub fn take_damage(&mut self) -> Option<Vec<u16>> {
        let r = match self.term.damage() {
            TermDamage::Full => None,
            TermDamage::Partial(it) => Some(it.map(|d| d.line as u16).collect()),
        };
        self.term.reset_damage();
        r
    }

    /// Lossless serialization incl. parser state (pending bytes of an incomplete sequence).
    pub fn snapshot(&self) -> Vec<u8> {
        let s = Snapshot {
            engine_version: ENGINE_VERSION.into(),
            term: self.term.snapshot(),
            pending: self.tracker.pending().to_vec(),
            extra: self.extra.clone(),
        };
        postcard::to_stdvec(&s).expect("snapshot serializes")
    }

    pub fn restore(bytes: &[u8], scrollback: usize) -> anyhow::Result<Self> {
        let s: Snapshot = postcard::from_bytes(bytes)?;
        anyhow::ensure!(
            s.engine_version == ENGINE_VERSION,
            "snapshot from engine {}",
            s.engine_version
        );
        let cols = s.term.grid.columns() as u16;
        let rows = s.term.grid.screen_lines() as u16;
        let mut e = Engine::new(cols, rows, scrollback);
        e.term.restore(s.term);
        e.extra = s.extra;
        // Re-enter the incomplete sequence: the parser ends in exactly the state it had.
        let mut sink = Vec::new();
        let replaying = e.replaying;
        e.replaying = true;
        e.feed(&s.pending, &mut sink);
        e.replaying = replaying;
        Ok(e)
    }

    /// Plain text of the visible screen (trailing spaces trimmed per row).
    pub fn screen_text(&self) -> String {
        self.visible_rows()
            .iter()
            .map(|r| r.text().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn kitty_flags(m: TermMode) -> u8 {
    let mut f = 0;
    if m.contains(TermMode::DISAMBIGUATE_ESC_CODES) {
        f |= 1;
    }
    if m.contains(TermMode::REPORT_EVENT_TYPES) {
        f |= 2;
    }
    if m.contains(TermMode::REPORT_ALTERNATE_KEYS) {
        f |= 4;
    }
    if m.contains(TermMode::REPORT_ALL_KEYS_AS_ESC) {
        f |= 8;
    }
    if m.contains(TermMode::REPORT_ASSOCIATED_TEXT) {
        f |= 16;
    }
    f
}

fn color(c: AColor, dim: &mut bool) -> Color {
    match c {
        AColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        AColor::Indexed(i) => Color::Indexed(i),
        AColor::Named(n) => {
            let i = n as usize;
            match n {
                NamedColor::Foreground | NamedColor::Background | NamedColor::Cursor => {
                    Color::Default
                }
                NamedColor::BrightForeground => Color::Default,
                NamedColor::DimForeground => {
                    *dim = true;
                    Color::Default
                }
                _ if i < 16 => Color::Indexed(i as u8),
                _ if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize)
                    .contains(&i) =>
                {
                    *dim = true;
                    Color::Indexed((i - NamedColor::DimBlack as usize) as u8)
                }
                _ => Color::Default,
            }
        }
    }
}

fn style_of(cell: &Cell) -> Style {
    let mut dim = false;
    let fg = color(cell.fg, &mut dim);
    let mut _bgdim = false;
    let bg = color(cell.bg, &mut _bgdim);
    let ul = cell
        .underline_color()
        .map(|c| color(c, &mut _bgdim))
        .unwrap_or_default();
    let f = cell.flags;
    let mut a = 0u16;
    let pairs = [
        (Flags::BOLD, attr::BOLD),
        (Flags::DIM, attr::DIM),
        (Flags::ITALIC, attr::ITALIC),
        (Flags::UNDERLINE, attr::UNDERLINE),
        (Flags::DOUBLE_UNDERLINE, attr::DOUBLE_UNDERLINE),
        (Flags::UNDERCURL, attr::UNDERCURL),
        (Flags::DOTTED_UNDERLINE, attr::DOTTED_UNDERLINE),
        (Flags::DASHED_UNDERLINE, attr::DASHED_UNDERLINE),
        (Flags::INVERSE, attr::INVERSE),
        (Flags::HIDDEN, attr::HIDDEN),
        (Flags::STRIKEOUT, attr::STRIKE),
    ];
    for (fl, at) in pairs {
        if f.contains(fl) {
            a |= at;
        }
    }
    if dim {
        a |= attr::DIM;
    }
    Style {
        fg,
        bg,
        ul,
        attrs: a,
    }
}

fn row_from(row: &alacritty_terminal::grid::Row<Cell>, cols: usize) -> Row {
    let mut spans: Vec<Span> = Vec::new();
    let mut wrapped = false;
    for x in 0..cols {
        let cell = &row[Column(x)];
        if x + 1 == cols && cell.flags.contains(Flags::WRAPLINE) {
            wrapped = true;
        }
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let style = style_of(cell);
        let (text, w): (String, u16) = if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
            (" ".into(), 1)
        } else {
            let mut s = String::new();
            s.push(cell.c);
            if let Some(z) = cell.zerowidth() {
                s.extend(z.iter());
            }
            (
                s,
                if cell.flags.contains(Flags::WIDE_CHAR) {
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
    Row { spans, wrapped }
}

fn xterm256(i: u8) -> (u8, u8, u8) {
    if i >= 232 {
        let v = 8 + (i - 232) * 10;
        return (v, v, v);
    }
    let i = i - 16;
    let c = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
    (c(i / 36), c((i / 6) % 6), c(i % 6))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl Engine {
    #[doc(hidden)]
    pub fn tracker_pending_is_empty(&self) -> bool {
        self.tracker.pending().is_empty()
    }
}

impl Engine {
    /// Absolute number of primary-screen lines ever scrolled into history. Row `i` of
    /// [`Engine::history_row`] has absolute line number `scrolled_total() - history_len() + i`.
    pub fn scrolled_total(&self) -> u64 {
        self.term.primary_grid().scrolled_total
    }
}
