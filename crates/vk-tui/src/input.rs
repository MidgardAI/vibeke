//! The client's own host-input decoder (03 §7.1). Every host's input goes through it, whatever
//! keyboard protocol the host speaks:
//!
//! - kitty `CSI code[:shifted[:base]] ; mods[:event] ; text[:text…] u` →
//!   [`KeyEvent`]`{key, shifted, base_layout_key, mods, kind, text}` (the associated text is
//!   kept, so AltGr and dead-key text reach the keymap);
//! - xterm modifyOtherKeys `CSI 27 ; mods ; code ~`;
//! - legacy bytes, `CSI 1;mods X` / `CSI n;mods ~` keys, SS3 keys and the SS3 keypad,
//!   `ESC ESC <sequence>` alt-prefixed keys;
//! - SGR and X10 mouse, bracketed paste and focus in/out, as crossterm [`Event`]s, so the rest
//!   of the client is unchanged.
//!
//! Replies and strings the host sends (DCS, OSC, APC, `CSI ?…`, cursor reports) are skipped.
//! Window resizes come from `SIGWINCH`.
//!
//! Timing: a lone `ESC` (or `ESC` plus one introducer) settles quickly as Escape (alt+key); a
//! started sequence or UTF-8 character waits much longer for a split read (ssh), and when one is
//! given up on, its late tail is dropped instead of arriving as typed text.

use crate::event::{
    Event, KeyModifiers, MouseButton as CtButton, MouseEvent as CtMouse, MouseEventKind,
};
use crate::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use crate::time::Instant;
use vk_proto::input::{Key, KeyEvent, KeyKind, Mods, NamedKey};

/// One decoded host input.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    Event(Event),
    Key(KeyEvent),
}

/// Bytes kept while waiting for the rest of a sequence (a longer one is dropped).
const PENDING_MAX: usize = 64 * 1024;
/// A paste longer than this is cut (the rest is dropped).
const PASTE_MAX: usize = 64 << 20;
/// At most this much of a given-up sequence's tail is dropped.
const TAIL_MAX: usize = 256;
/// An SGR mouse report tail longer than this is not one.
const MOUSE_TAIL_MAX: usize = 32;

/// A lone `ESC`: the Escape key unless more follows at once.
pub const ESC_WAIT: Duration = Duration::from_millis(30);
/// `ESC` plus one byte that can start a longer sequence (`[`, `O`, `]`, `ESC`…): alt+key unless
/// more follows.
pub const INTRO_WAIT: Duration = Duration::from_millis(50);
/// Inside a sequence or a UTF-8 character: long enough for a read split over a slow link.
pub const SEQ_WAIT: Duration = Duration::from_millis(500);

const ESC: u8 = 0x1b;

/// How bytes that mean different keys on different hosts are read.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Policy {
    /// The host's erase character is `^H`: 0x08 is its Backspace key, not ctrl+h.
    pub erase_is_bs: bool,
    /// `ESC ESC <sequence>` is the alt-modified key (macOS terminals send Option+arrow so)
    /// rather than Escape followed by the key.
    pub doubled_escape_is_alt: bool,
}

impl Policy {
    /// From the host's tty (its erase character) and platform.
    pub fn from_host() -> Policy {
        Policy {
            erase_is_bs: host_erase() == Some(0x08),
            doubled_escape_is_alt: cfg!(target_os = "macos"),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn host_erase() -> Option<u8> {
    // SAFETY: tcgetattr fills the zeroed termios for fd 0 (or fails and leaves it unused).
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    (unsafe { libc::tcgetattr(0, &mut t) } == 0).then(|| t.c_cc[libc::VERASE] as u8)
}

/// What is left of a sequence that was given up on, dropped when it arrives late.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Tail {
    /// A CSI's parameters and final byte.
    Csi,
    /// An OSC body (BEL or ST ends it), or a DCS/APC/PM/SOS body (ST only).
    Str { osc: bool },
    /// An SGR mouse report whose `ESC` (and, with `bracket`, its `[`) already settled as a key.
    Mouse { bracket: bool },
}

enum TailStep {
    /// Drop `n` bytes; `done`: the tail is over (`n == 0, done`: it was no tail at all).
    Drop(usize, bool),
    /// Could still be the tail: wait for more.
    Wait,
}

/// Streaming decoder: feed bytes as they arrive; an incomplete sequence waits for more, and
/// [`Decoder::flush`] (after [`Decoder::wait`]) settles it.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
    paste: Option<Vec<u8>>,
    policy: Policy,
    tail: Option<Tail>,
    tail_len: usize,
    /// The pending bytes outlived a flush (a partial UTF-8 character): wait without a timer.
    held: bool,
    /// Bytes consumed so far; the reader restarts its timer when this moves.
    consumed: u64,
}

enum Step {
    /// Consumed `n` bytes, producing these inputs.
    Done(usize, Vec<Input>),
    /// Consumed `n` bytes: a bracketed paste starts.
    Paste(usize),
    /// Need more bytes.
    More,
}

fn key(key: Key, mods: Mods) -> KeyEvent {
    let mut e = KeyEvent::new(key, mods);
    if let Key::Char(c) = key
        && !mods.ctrl()
        && !mods.alt()
        && !mods.sup()
    {
        e.text = Some(c.to_string());
    }
    e
}

fn named(n: NamedKey, mods: Mods, kind: KeyKind) -> Input {
    let mut e = KeyEvent::new(Key::Named(n), mods);
    e.kind = kind;
    Input::Key(e)
}

/// Kitty modifier parameter (`1 + bits`) → mods (caps/num lock ignored).
fn mods_of(p: u32) -> Mods {
    let b = p.saturating_sub(1);
    let mut m = Mods::empty();
    for (bit, x) in [
        (1, Mods::SHIFT),
        (2, Mods::ALT),
        (4, Mods::CTRL),
        (8, Mods::SUPER),
        (16, Mods::HYPER),
        (32, Mods::META),
    ] {
        if b & bit != 0 {
            m = m.union(x);
        }
    }
    m
}

fn kind_of(p: u32) -> KeyKind {
    match p {
        2 => KeyKind::Repeat,
        3 => KeyKind::Release,
        _ => KeyKind::Press,
    }
}

/// Kitty functional key codes (private-use area) and the C0 ones the protocol uses. The rest of
/// the functional range (KP Begin, media keys, hyper/meta/ISO level shifts) has no key here and
/// is ignored, like every other private-use code: never text.
fn functional(code: u32) -> Option<NamedKey> {
    use NamedKey as N;
    Some(match code {
        27 => N::Escape,
        13 => N::Enter,
        9 => N::Tab,
        127 | 8 => N::Backspace,
        57358 => N::CapsLock,
        57359 => N::ScrollLock,
        57360 => N::NumLock,
        57361 => N::PrintScreen,
        57362 => N::Pause,
        57363 => N::Menu,
        57364..=57375 => N::F((code - 57364 + 1) as u8),
        57376..=57398 => N::F((code - 57376 + 13) as u8),
        57414 => N::Enter,
        57417 => N::Left,
        57418 => N::Right,
        57419 => N::Up,
        57420 => N::Down,
        57421 => N::PageUp,
        57422 => N::PageDown,
        57423 => N::Home,
        57424 => N::End,
        57425 => N::Insert,
        57426 => N::Delete,
        57441 => N::LeftShift,
        57442 => N::LeftControl,
        57443 => N::LeftAlt,
        57444 => N::LeftSuper,
        57447 => N::RightShift,
        57448 => N::RightControl,
        57449 => N::RightAlt,
        57450 => N::RightSuper,
        _ => return None,
    })
}

/// Keypad keys that produce characters.
fn keypad_char(code: u32) -> Option<char> {
    Some(match code {
        57399..=57408 => char::from_digit(code - 57399, 10)?,
        57409 => '.',
        57410 => '/',
        57411 => '*',
        57412 => '-',
        57413 => '+',
        57415 => '=',
        57416 => ',',
        _ => return None,
    })
}

/// A code point that can be a typed character: not a control and not private-use (kitty's
/// functional keys and macOS function-key markers live there).
fn text_char(code: u32) -> Option<char> {
    let c = char::from_u32(code)?;
    let private = matches!(code, 0xe000..=0xf8ff | 0xf0000..);
    (!c.is_control() && !private).then_some(c)
}

/// `CSI n ~` key numbers.
fn tilde_key(n: u32) -> Option<NamedKey> {
    use NamedKey as N;
    Some(match n {
        1 | 7 => N::Home,
        2 => N::Insert,
        3 => N::Delete,
        4 | 8 => N::End,
        5 => N::PageUp,
        6 => N::PageDown,
        11..=15 => N::F((n - 10) as u8),
        17..=21 => N::F((n - 11) as u8),
        23 | 24 => N::F((n - 12) as u8),
        29 => N::Menu,
        _ => return None,
    })
}

fn letter_key(f: u8) -> Option<NamedKey> {
    use NamedKey as N;
    Some(match f {
        b'A' => N::Up,
        b'B' => N::Down,
        b'C' => N::Right,
        b'D' => N::Left,
        b'H' => N::Home,
        b'F' => N::End,
        b'P' => N::F(1),
        b'Q' => N::F(2),
        b'R' => N::F(3),
        b'S' => N::F(4),
        _ => return None,
    })
}

/// SS3 application-keypad finals.
fn ss3_keypad(f: u8) -> Option<Key> {
    Some(match f {
        b'p'..=b'y' => Key::Char((b'0' + f - b'p') as char),
        b'j' => Key::Char('*'),
        b'k' => Key::Char('+'),
        b'l' => Key::Char(','),
        b'm' => Key::Char('-'),
        b'n' => Key::Char('.'),
        b'o' => Key::Char('/'),
        b'X' => Key::Char('='),
        b'M' => Key::Named(NamedKey::Enter),
        _ => return None,
    })
}

/// Parameters `a:b:c;d:e;…` as numbers (missing = None).
fn params(p: &[u8]) -> Vec<Vec<Option<u32>>> {
    std::str::from_utf8(p)
        .unwrap_or("")
        .split(';')
        .map(|g| g.split(':').map(|x| x.parse().ok()).collect())
        .collect()
}

fn first(ps: &[Vec<Option<u32>>], i: usize) -> Option<u32> {
    ps.get(i).and_then(|g| g.first().copied().flatten())
}

fn sub(ps: &[Vec<Option<u32>>], i: usize, j: usize) -> Option<u32> {
    ps.get(i).and_then(|g| g.get(j).copied().flatten())
}

/// A `CSI … u` key (kitty keyboard protocol).
fn csi_u(p: &[u8]) -> Option<Input> {
    let ps = params(p);
    let code = first(&ps, 0)?;
    let mut shifted = sub(&ps, 0, 1).and_then(text_char);
    let base = sub(&ps, 0, 2).and_then(text_char);
    let mut mods = mods_of(first(&ps, 1).unwrap_or(1));
    let kind = kind_of(sub(&ps, 1, 1).unwrap_or(1));
    let text: Option<String> = ps.get(2).map(|g| {
        g.iter()
            .filter_map(|c| c.and_then(char::from_u32))
            .collect::<String>()
    });
    let text = text.filter(|t| !t.is_empty() && !t.chars().any(char::is_control));
    if let Some(n) = functional(code) {
        return Some(named(n, mods, kind));
    }
    let mut c = keypad_char(code).or_else(|| text_char(code))?;
    // A ctrl chord on a non-Latin layout (ctrl+с on Russian) is the shortcut on the physical
    // key the host names as the base-layout key (ctrl+c), as kitty itself sends ^C for it.
    if mods.ctrl()
        && !c.is_ascii()
        && let Some(b) = base.filter(char::is_ascii_graphic)
    {
        c = b;
        shifted = None;
    }
    // A shifted alternate is only reported with shift held; some hosts leave the bit out.
    if shifted.is_some_and(|s| s != c) {
        mods = mods.union(Mods::SHIFT);
    }
    // As crossterm does: with shift and a reported shifted key, the key is the shifted one.
    let ch = match shifted {
        Some(s) if mods.shift() => s,
        _ if mods.shift() && c.is_ascii_lowercase() => c.to_ascii_uppercase(),
        _ => c,
    };
    // Shift is implicit in a shifted non-letter (`?`), as for legacy hosts.
    if !ch.is_alphabetic() && ch != ' ' && mods == Mods::SHIFT && shifted.is_some() {
        mods = Mods::empty();
    }
    let mut e = self::key(Key::Char(ch), mods);
    e.kind = kind;
    e.shifted = shifted;
    e.base_layout_key = base;
    if text.is_some() {
        e.text = text;
    } else if kind == KeyKind::Release {
        e.text = None;
    }
    Some(Input::Key(e))
}

/// An xterm modifyOtherKeys key, `CSI 27 ; mods ; code ~`.
fn modify_other_keys(ps: &[Vec<Option<u32>>]) -> Option<Input> {
    let mut mods = mods_of(first(ps, 1)?);
    let code = first(ps, 2)?;
    if let Some(n) = functional(code) {
        return Some(named(n, mods, KeyKind::Press));
    }
    let mut c = text_char(code)?;
    if mods.shift() {
        if c.is_ascii_lowercase() {
            c = c.to_ascii_uppercase();
        } else if !c.is_alphanumeric() && c != ' ' {
            // The code is already the shifted symbol (`ctrl+shift+1` → `!`).
            mods = mods.without(Mods::SHIFT);
        }
    }
    Some(Input::Key(key(Key::Char(c), mods)))
}

fn mouse(p: &[u8], release: bool) -> Option<Input> {
    let ps = params(p);
    let b = first(&ps, 0)?;
    let x = first(&ps, 1)?.saturating_sub(1).min(u16::MAX as u32) as u16;
    let y = first(&ps, 2)?.saturating_sub(1).min(u16::MAX as u32) as u16;
    Some(Input::Event(Event::Mouse(mouse_event(b, x, y, release))))
}

fn mouse_event(b: u32, column: u16, row: u16, release: bool) -> CtMouse {
    let mut modifiers = KeyModifiers::empty();
    if b & 4 != 0 {
        modifiers |= KeyModifiers::SHIFT;
    }
    if b & 8 != 0 {
        modifiers |= KeyModifiers::ALT;
    }
    if b & 16 != 0 {
        modifiers |= KeyModifiers::CONTROL;
    }
    let button = match b & 3 {
        0 => Some(CtButton::Left),
        1 => Some(CtButton::Middle),
        2 => Some(CtButton::Right),
        _ => None,
    };
    let kind = if b & 64 != 0 {
        match b & 3 {
            0 => MouseEventKind::ScrollUp,
            1 => MouseEventKind::ScrollDown,
            2 => MouseEventKind::ScrollLeft,
            _ => MouseEventKind::ScrollRight,
        }
    } else if b & 32 != 0 {
        match button {
            Some(bt) => MouseEventKind::Drag(bt),
            None => MouseEventKind::Moved,
        }
    } else {
        match (button, release) {
            (Some(bt), false) => MouseEventKind::Down(bt),
            (Some(bt), true) => MouseEventKind::Up(bt),
            (None, _) => MouseEventKind::Up(CtButton::Left),
        }
    };
    CtMouse {
        kind,
        column,
        row,
        modifiers,
    }
}

/// Add alt to decoded keys (`ESC` + key); their text goes (alt+x types nothing).
fn with_alt(ins: &mut [Input]) {
    for x in ins {
        if let Input::Key(k) = x {
            k.mods = k.mods.union(Mods::ALT);
            k.text = None;
        }
    }
}

/// `b` is the start of a UTF-8 character still missing bytes.
fn partial_utf8(b: &[u8]) -> bool {
    let Some(&c) = b.first() else {
        return false;
    };
    let n = utf8_len(c);
    n > 1 && b.len() < n && b[1..].iter().all(|x| x & 0xc0 == 0x80)
}

impl Decoder {
    pub fn new() -> Decoder {
        Decoder::default()
    }

    pub fn with_policy(policy: Policy) -> Decoder {
        Decoder {
            policy,
            ..Decoder::default()
        }
    }

    /// How long to wait for more input before [`Decoder::flush`]; `None`: nothing to settle.
    pub fn wait(&self) -> Option<Duration> {
        if self.paste.is_some() || self.held {
            return None;
        }
        // A tail being dropped is only expected for a while.
        if self.tail.is_some() {
            return Some(SEQ_WAIT);
        }
        if self.buf.is_empty() {
            return None;
        }
        Some(match self.buf.as_slice() {
            [ESC] => ESC_WAIT,
            [ESC, b'[' | b'O' | b']' | b'P' | b'_' | b'^' | b'X' | ESC] => INTRO_WAIT,
            _ => SEQ_WAIT,
        })
    }

    /// Bytes consumed so far (moves whenever a sequence completes or is given up on).
    pub fn consumed(&self) -> u64 {
        self.consumed
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Input> {
        self.buf.extend_from_slice(bytes);
        self.held = false;
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.buf.len() {
            if let Some(p) = self.paste.as_mut() {
                // Inside a bracketed paste: everything up to `ESC [ 201 ~` is text.
                let rest = &self.buf[i..];
                match find(rest, PASTE_END) {
                    Some(end) => {
                        if p.len() < PASTE_MAX {
                            p.extend_from_slice(&rest[..end]);
                        }
                        let text = String::from_utf8_lossy(p).into_owned();
                        self.paste = None;
                        out.push(Input::Event(Event::Paste(text)));
                        i += end + PASTE_END.len();
                        continue;
                    }
                    None => {
                        // Keep a possible partial terminator for the next read.
                        let keep = (1..PASTE_END.len().min(rest.len() + 1))
                            .rev()
                            .find(|&k| PASTE_END.starts_with(&rest[rest.len() - k..]))
                            .unwrap_or(0);
                        let take = rest.len() - keep;
                        if p.len() < PASTE_MAX {
                            p.extend_from_slice(&rest[..take]);
                        }
                        i += take;
                        break;
                    }
                }
            }
            if let Some(t) = self.tail {
                match tail_step(t, &self.buf[i..]) {
                    TailStep::Drop(n, done) => {
                        i += n;
                        self.tail_len += n;
                        if done || self.tail_len > TAIL_MAX {
                            self.tail = None;
                            self.tail_len = 0;
                        }
                        continue;
                    }
                    TailStep::Wait => break,
                }
            }
            match parse(&self.buf[i..], self.policy) {
                Step::Done(n, ins) => {
                    out.extend(ins);
                    i += n.max(1);
                }
                Step::Paste(n) => {
                    self.paste = Some(Vec::new());
                    i += n;
                }
                Step::More => break,
            }
        }
        let i = i.min(self.buf.len());
        self.consumed += i as u64;
        self.buf.drain(..i);
        if self.buf.len() > PENDING_MAX && self.paste.is_none() {
            self.consumed += self.buf.len() as u64;
            self.buf.clear();
        }
        out
    }

    /// The wait passed: settle what is pending. A lone `ESC` is the Escape key and `ESC` plus an
    /// introducer is alt+key; an unfinished sequence is dropped (and so is its tail if it comes
    /// later); a partial UTF-8 character is kept for the rest.
    pub fn flush(&mut self) -> Vec<Input> {
        if self.paste.is_some() {
            return vec![];
        }
        if self.buf.is_empty() {
            // No late tail came: what follows is input again.
            self.tail = None;
            self.tail_len = 0;
            return vec![];
        }
        let b = std::mem::take(&mut self.buf);
        self.consumed += b.len() as u64;
        match self.tail.take() {
            // Not a mouse report's tail after all: typed input.
            Some(Tail::Mouse { .. }) => return self.feed(&b),
            // The unfinished end of a dropped tail.
            Some(_) => {
                self.tail_len = 0;
                return vec![];
            }
            None => {}
        }
        if partial_utf8(&b) || (b[0] == ESC && partial_utf8(&b[1..])) {
            self.buf = b;
            self.held = true;
            return vec![];
        }
        match b.as_slice() {
            [ESC] => {
                // A mouse report split right after its ESC must not type its tail.
                self.tail = Some(Tail::Mouse { bracket: true });
                vec![named(NamedKey::Escape, Mods::empty(), KeyKind::Press)]
            }
            [ESC, ESC] => vec![named(NamedKey::Escape, Mods::ALT, KeyKind::Press)],
            [ESC, c @ (b'[' | b'O' | b']' | b'P' | b'_' | b'^' | b'X')] => {
                if *c == b'[' {
                    self.tail = Some(Tail::Mouse { bracket: false });
                }
                let mut ins = match parse(&[*c], self.policy) {
                    Step::Done(_, ins) => ins,
                    _ => vec![],
                };
                with_alt(&mut ins);
                ins
            }
            [ESC, b'[', b'M', ..] | [ESC, ESC, b'[', b'M', ..] => vec![],
            [ESC, b'[', ..] | [ESC, ESC, b'[', ..] => {
                self.tail = Some(Tail::Csi);
                vec![]
            }
            [ESC, b']', ..] => {
                self.tail = Some(Tail::Str { osc: true });
                vec![]
            }
            [ESC, b'P' | b'_' | b'^' | b'X', ..] => {
                self.tail = Some(Tail::Str { osc: false });
                vec![]
            }
            _ => vec![],
        }
    }
}

/// How much of `b` is the late tail `t`.
fn tail_step(t: Tail, b: &[u8]) -> TailStep {
    match t {
        Tail::Csi => {
            for (k, &c) in b.iter().enumerate() {
                match c {
                    0x20..=0x3f => {}
                    0x40..=0x7e => return TailStep::Drop(k + 1, true),
                    _ => return TailStep::Drop(k, true),
                }
            }
            TailStep::Drop(b.len(), false)
        }
        Tail::Str { osc } => {
            for (k, &c) in b.iter().enumerate() {
                match c {
                    0x07 if osc => return TailStep::Drop(k + 1, true),
                    ESC => {
                        return match b.get(k + 1) {
                            Some(b'\\') => TailStep::Drop(k + 2, true),
                            Some(_) => TailStep::Drop(k, true),
                            None if k == 0 => TailStep::Wait,
                            None => TailStep::Drop(k, false),
                        };
                    }
                    // Not a reply's body: typed input after all.
                    0x00..=0x1f | 0x7f if osc => return TailStep::Drop(k, true),
                    _ => {}
                }
            }
            TailStep::Drop(b.len(), false)
        }
        Tail::Mouse { bracket } => {
            let intro: &[u8] = if bracket { b"[<" } else { b"<" };
            let mut semis = 0;
            for (k, &c) in b.iter().enumerate() {
                if k >= MOUSE_TAIL_MAX {
                    break;
                }
                let ok = match intro.get(k) {
                    Some(&w) => c == w,
                    None => match c {
                        b'0'..=b'9' => true,
                        b';' => {
                            semis += 1;
                            semis <= 2
                        }
                        b'M' | b'm' if semis == 2 => return TailStep::Drop(k + 1, true),
                        _ => false,
                    },
                };
                if !ok {
                    return TailStep::Drop(0, true);
                }
            }
            if b.len() >= MOUSE_TAIL_MAX {
                TailStep::Drop(0, true)
            } else {
                TailStep::Wait
            }
        }
    }
}

const PASTE_END: &[u8] = b"\x1b[201~";

/// One input at the start of `b`.
fn parse(b: &[u8], policy: Policy) -> Step {
    match b[0] {
        ESC => escape(b, policy),
        // Only CR is Enter. LF is ctrl+j, which re-encodes to LF for the pane: terminals set up
        // to send `\n` for shift+enter (iTerm2 after Claude Code's `/terminal-setup`) rely on
        // that byte reaching the agent as a newline, not as a submit.
        0x0d => Step::Done(
            1,
            vec![named(NamedKey::Enter, Mods::empty(), KeyKind::Press)],
        ),
        0x09 => Step::Done(1, vec![named(NamedKey::Tab, Mods::empty(), KeyKind::Press)]),
        0x7f => Step::Done(
            1,
            vec![named(NamedKey::Backspace, Mods::empty(), KeyKind::Press)],
        ),
        // A host whose erase character is ^H sends it for Backspace; elsewhere it is ctrl+h
        // (which re-encodes to 0x08 for the pane).
        0x08 if policy.erase_is_bs => Step::Done(
            1,
            vec![named(NamedKey::Backspace, Mods::empty(), KeyKind::Press)],
        ),
        0x00 => Step::Done(1, vec![Input::Key(key(Key::Char(' '), Mods::CTRL))]),
        c @ 0x01..=0x1a => Step::Done(
            1,
            vec![Input::Key(key(
                Key::Char((b'a' + c - 1) as char),
                Mods::CTRL,
            ))],
        ),
        c @ 0x1c..=0x1f => Step::Done(
            1,
            vec![Input::Key(key(
                Key::Char(['\\', ']', '^', '_'][(c - 0x1c) as usize]),
                Mods::CTRL,
            ))],
        ),
        c => {
            // UTF-8 text.
            let len = utf8_len(c);
            if c >= 0x80 && len == 1 {
                return Step::Done(1, vec![]);
            }
            if b.len() < len {
                return if partial_utf8(b) {
                    Step::More
                } else {
                    Step::Done(1, vec![])
                };
            }
            match std::str::from_utf8(&b[..len])
                .ok()
                .and_then(|s| s.chars().next())
            {
                Some(ch) => {
                    let mods = if ch.is_uppercase() {
                        Mods::SHIFT
                    } else {
                        Mods::empty()
                    };
                    let mut e = key(Key::Char(ch), mods);
                    e.text = Some(ch.to_string());
                    Step::Done(len, vec![Input::Key(e)])
                }
                None => Step::Done(1, vec![]),
            }
        }
    }
}

fn escape(b: &[u8], policy: Policy) -> Step {
    let Some(&c1) = b.get(1) else {
        return Step::More;
    };
    match c1 {
        b'[' => csi(b),
        b'O' => ss3(b),
        // Strings the host sends (replies): skip to the terminator. Only OSC also ends at BEL.
        b']' | b'P' | b'_' | b'^' | b'X' => {
            let body = &b[2..];
            for (k, w) in body.iter().enumerate() {
                if *w == 0x07 && c1 == b']' {
                    return Step::Done(2 + k + 1, vec![]);
                }
                if *w == ESC && body.get(k + 1) == Some(&b'\\') {
                    return Step::Done(2 + k + 2, vec![]);
                }
            }
            Step::More
        }
        ESC => {
            let escape = Step::Done(
                1,
                vec![named(NamedKey::Escape, Mods::empty(), KeyKind::Press)],
            );
            if !policy.doubled_escape_is_alt {
                return escape;
            }
            // `ESC ESC [A`: alt+Up from a terminal that prefixes ESC for Option/alt.
            match b.get(2) {
                None => Step::More,
                Some(b'[' | b'O') => match escape_seq(&b[1..]) {
                    Step::More => Step::More,
                    Step::Done(n, mut ins) if ins.len() == 1 && matches!(ins[0], Input::Key(_)) => {
                        with_alt(&mut ins);
                        Step::Done(1 + n, ins)
                    }
                    // A mouse report, reply or paste after a lone ESC: Escape, then that.
                    _ => escape,
                },
                Some(_) => escape,
            }
        }
        _ => match parse(&b[1..], policy) {
            // ESC + key: alt+key (legacy encoding).
            Step::Done(n, mut ins) => {
                with_alt(&mut ins);
                Step::Done(1 + n, ins)
            }
            Step::More => Step::More,
            Step::Paste(_) => Step::Done(1, vec![]),
        },
    }
}

/// [`escape`] for a CSI or SS3 (no further `ESC ESC` nesting).
fn escape_seq(b: &[u8]) -> Step {
    match b.get(1) {
        Some(b'[') => csi(b),
        Some(b'O') => ss3(b),
        _ => Step::Done(1, vec![]),
    }
}

/// `ESC O [mods] final`: F1–F4, cursor keys, and the application keypad.
fn ss3(b: &[u8]) -> Step {
    let mut k = 2;
    while b.get(k).is_some_and(u8::is_ascii_digit) && k < 5 {
        k += 1;
    }
    let Some(&f) = b.get(k) else {
        return Step::More;
    };
    let mods = std::str::from_utf8(&b[2..k])
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(Mods::empty(), mods_of);
    let ins = if let Some(n) = letter_key(f) {
        vec![named(n, mods, KeyKind::Press)]
    } else {
        match ss3_keypad(f) {
            Some(Key::Named(n)) => vec![named(n, mods, KeyKind::Press)],
            Some(k) => vec![Input::Key(key(k, mods))],
            None => vec![],
        }
    };
    Step::Done(k + 1, ins)
}

fn csi(b: &[u8]) -> Step {
    // ESC [ params (0x30-0x3F) intermediates (0x20-0x2F) final (0x40-0x7E).
    let mut k = 2;
    while k < b.len() && (0x30..=0x3f).contains(&b[k]) {
        k += 1;
    }
    while k < b.len() && (0x20..=0x2f).contains(&b[k]) {
        k += 1;
    }
    let Some(&f) = b.get(k) else {
        return Step::More;
    };
    if !(0x40..=0x7e).contains(&f) {
        // Not a sequence: alt+[ (legacy), then what follows.
        if k == 2 {
            let mut ins = vec![Input::Key(key(Key::Char('['), Mods::empty()))];
            with_alt(&mut ins);
            return Step::Done(2, ins);
        }
        return Step::Done(k, vec![]);
    }
    let p = &b[2..k];
    let n = k + 1;
    // X10 mouse: `CSI M Cb Cx Cy`.
    if f == b'M' && p.is_empty() {
        if b.len() < n + 3 {
            return Step::More;
        }
        let cb = (b[n] as u32).saturating_sub(32);
        let ev = mouse_event(
            cb,
            b[n + 1].saturating_sub(33) as u16,
            b[n + 2].saturating_sub(33) as u16,
            cb & 3 == 3,
        );
        return Step::Done(n + 3, vec![Input::Event(Event::Mouse(ev))]);
    }
    if p.first() == Some(&b'<') {
        return Step::Done(n, mouse(&p[1..], f == b'm').into_iter().collect());
    }
    if p.first().is_some_and(|c| matches!(c, b'?' | b'>' | b'=')) {
        // Replies (DECRPM, kitty flags, DA, colour-scheme reports): not input.
        return Step::Done(n, vec![]);
    }
    let ps = params(p);
    let mods = |ps: &[Vec<Option<u32>>]| {
        (
            mods_of(sub(ps, 1, 0).unwrap_or(1)),
            kind_of(sub(ps, 1, 1).unwrap_or(1)),
        )
    };
    let ins: Vec<Input> = match f {
        b'u' => csi_u(p).into_iter().collect(),
        b'~' => match first(&ps, 0) {
            Some(200) => return Step::Paste(n),
            Some(27) if ps.len() >= 3 => modify_other_keys(&ps).into_iter().collect(),
            Some(num) => tilde_key(num)
                .map(|nk| {
                    let (m, kd) = mods(&ps);
                    named(nk, m, kd)
                })
                .into_iter()
                .collect(),
            None => vec![],
        },
        b'I' if p.is_empty() => vec![Input::Event(Event::FocusGained)],
        b'O' if p.is_empty() => vec![Input::Event(Event::FocusLost)],
        b'Z' => vec![named(NamedKey::Tab, Mods::SHIFT, KeyKind::Press)],
        // `CSI row ; col R` is a cursor position report, and so is a bare `CSI R` (kitty sends
        // F3 as `CSI 13 ~`); only `CSI 1 ; mods R` with modifiers is modified F3 (xterm).
        b'R' => match (first(&ps, 0), mods(&ps)) {
            (Some(1), (m, kd)) if !m.is_empty() => vec![named(NamedKey::F(3), m, kd)],
            _ => vec![],
        },
        f => match letter_key(f) {
            Some(nk) => {
                let (m, kd) = mods(&ps);
                vec![named(nk, m, kd)]
            }
            None => vec![],
        },
    };
    Step::Done(n, ins)
}

fn utf8_len(b: u8) -> usize {
    match b {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

// ---- the reader -------------------------------------------------------------------------------

/// Reads the host's input on a thread (stdin, `poll` with a stop pipe: no timer wake-ups while
/// idle), decodes it, and resizes from `SIGWINCH`. Dropping it stops reading (the external
/// editor and the appearance re-probe need stdin).
#[cfg(not(target_arch = "wasm32"))]
pub struct Reader {
    rx: tokio::sync::mpsc::UnboundedReceiver<Input>,
    stop_w: libc::c_int,
    thread: Option<std::thread::JoinHandle<()>>,
    winch: Option<tokio::signal::unix::Signal>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Reader {
    pub fn new() -> Reader {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: pipe() fills two fds.
        let ok = unsafe { libc::pipe(fds.as_mut_ptr()) } == 0;
        let (stop_r, stop_w) = if ok { (fds[0], fds[1]) } else { (-1, -1) };
        let policy = Policy::from_host();
        let thread = std::thread::spawn(move || read_loop(tx, stop_r, policy));
        let winch =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).ok();
        Reader {
            rx,
            stop_w,
            thread: Some(thread),
            winch,
        }
    }

    pub async fn next(&mut self) -> Option<std::io::Result<Input>> {
        tokio::select! {
            i = self.rx.recv() => i.map(Ok),
            Some(()) = async {
                match self.winch.as_mut() {
                    Some(s) => s.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                let (c, r) = crossterm::terminal::size().unwrap_or((80, 24));
                Some(Ok(Input::Event(Event::Resize(c, r))))
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Default for Reader {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for Reader {
    fn drop(&mut self) {
        if self.stop_w >= 0 {
            // SAFETY: writing one byte to our pipe, then closing it.
            unsafe {
                libc::write(self.stop_w, [1u8].as_ptr().cast(), 1);
                libc::close(self.stop_w);
            }
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn read_loop(tx: tokio::sync::mpsc::UnboundedSender<Input>, stop_r: libc::c_int, policy: Policy) {
    let mut dec = Decoder::with_policy(policy);
    // When what is pending now started waiting: the wait is counted from there, so input that
    // keeps arriving into an unfinished sequence does not hold it open forever.
    let mut since = Instant::now();
    let mut seen = dec.consumed();
    loop {
        let was_waiting = dec.wait().is_some();
        let mut fds = [
            libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stop_r,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let timeout = match dec.wait() {
            Some(w) => w.saturating_sub(since.elapsed()).as_millis() as i32,
            None => -1,
        };
        // SAFETY: two valid pollfds.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if fds[1].revents != 0 {
            break;
        }
        let ins = if n == 0 {
            dec.flush()
        } else if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            let mut b = [0u8; 4096];
            // SAFETY: reading into a local buffer of the stated length.
            let k = unsafe { libc::read(0, b.as_mut_ptr().cast(), b.len()) };
            if k <= 0 {
                if k < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                break;
            }
            dec.feed(&b[..k as usize])
        } else {
            vec![]
        };
        if dec.consumed() != seen || n == 0 || !was_waiting {
            seen = dec.consumed();
            since = Instant::now();
        }
        for i in ins {
            if tx.send(i).is_err() {
                return;
            }
        }
    }
    if stop_r >= 0 {
        // SAFETY: closing our pipe's read end.
        unsafe { libc::close(stop_r) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(b: &[u8]) -> Vec<KeyEvent> {
        let mut d = Decoder::new();
        d.feed(b)
            .into_iter()
            .filter_map(|i| match i {
                Input::Key(k) => Some(k),
                _ => None,
            })
            .collect()
    }

    fn one(b: &[u8]) -> KeyEvent {
        let k = keys(b);
        assert_eq!(k.len(), 1, "{b:?} → {k:?}");
        k[0].clone()
    }

    #[test]
    fn csi_u_keeps_associated_text_alternate_keys_and_event_types() {
        // Norwegian AltGr+2 on Windows/Linux-style reporting: ctrl+alt, text "@".
        let k = one(b"\x1b[50;7;64u");
        assert_eq!(k.key, Key::Char('2'));
        assert_eq!(k.mods, Mods::CTRL | Mods::ALT);
        assert_eq!(k.text.as_deref(), Some("@"));
        // macOS Option+2 (alt) with text "@"; base layout key reported.
        let k = one(b"\x1b[50::50;3;64u");
        assert_eq!(k.mods, Mods::ALT);
        assert_eq!(k.base_layout_key, Some('2'));
        assert_eq!(k.text.as_deref(), Some("@"));
        // Shifted key with alternate keys: the shifted char, shift implicit for non-letters.
        let k = one(b"\x1b[47:63;2;63u");
        assert_eq!(k.key, Key::Char('?'));
        assert_eq!(k.mods, Mods::empty());
        assert_eq!(k.shifted, Some('?'));
        let k = one(b"\x1b[116:84;2;84u");
        assert_eq!((k.key, k.mods), (Key::Char('T'), Mods::SHIFT));
        // Ctrl+shift+p, no text.
        let k = one(b"\x1b[112:80;6u");
        assert_eq!((k.key, k.mods), (Key::Char('P'), Mods::CTRL | Mods::SHIFT));
        assert_eq!(k.text, None);
        // Release and repeat.
        assert_eq!(one(b"\x1b[97;1:3u").kind, KeyKind::Release);
        assert_eq!(one(b"\x1b[97;1:2u").kind, KeyKind::Repeat);
        // Plain text keys (flag 8: all keys as escapes) get their text.
        let k = one(b"\x1b[229;1;229u");
        assert_eq!((k.key, k.text.as_deref()), (Key::Char('å'), Some("å")));
        let k = one(b"\x1b[97u");
        assert_eq!(k.text.as_deref(), Some("a"));
        // Dead key composition: text only.
        let k = one(b"\x1b[101;1;233u");
        assert_eq!(k.text.as_deref(), Some("é"));
        // Functional keys.
        assert_eq!(one(b"\x1b[13u").key, Key::Named(NamedKey::Enter));
        assert_eq!(one(b"\x1b[27u").key, Key::Named(NamedKey::Escape));
        assert_eq!(one(b"\x1b[13;2u").mods, Mods::SHIFT);
        assert_eq!(one(b"\x1b[57376u").key, Key::Named(NamedKey::F(13)));
        assert_eq!(one(b"\x1b[57441;2u").key, Key::Named(NamedKey::LeftShift));
        assert_eq!(one(b"\x1b[57400u").key, Key::Char('1'), "keypad 1");
        // Control characters in the text field are ignored.
        assert_eq!(one(b"\x1b[13;1;13u").text, None);
    }

    #[test]
    fn legacy_and_functional_forms() {
        assert_eq!(one(b"\x1b[A").key, Key::Named(NamedKey::Up));
        let k = one(b"\x1b[1;5C");
        assert_eq!((k.key, k.mods), (Key::Named(NamedKey::Right), Mods::CTRL));
        assert_eq!(one(b"\x1b[1;1:3D").kind, KeyKind::Release);
        assert_eq!(one(b"\x1b[3~").key, Key::Named(NamedKey::Delete));
        assert_eq!(one(b"\x1b[5;3~").mods, Mods::ALT);
        assert_eq!(one(b"\x1b[15~").key, Key::Named(NamedKey::F(5)));
        assert_eq!(one(b"\x1bOP").key, Key::Named(NamedKey::F(1)));
        assert_eq!(one(b"\x1b[Z").mods, Mods::SHIFT);
        assert_eq!(one(b"\r").key, Key::Named(NamedKey::Enter));
        let lf = one(b"\n");
        assert_eq!((lf.key, lf.mods), (Key::Char('j'), Mods::CTRL));
        assert_eq!(
            vk_term::encode::encode_key(&lf, &Default::default()),
            b"\n",
            "LF reaches the pane as LF"
        );
        assert_eq!(one(b"\x7f").key, Key::Named(NamedKey::Backspace));
        assert_eq!(
            (one(b"\x02").key, one(b"\x02").mods),
            (Key::Char('b'), Mods::CTRL)
        );
        let k = one("é".as_bytes());
        assert_eq!(k.text.as_deref(), Some("é"));
        let k = one(b"\x1bx");
        assert_eq!((k.key, k.mods), (Key::Char('x'), Mods::ALT));
    }

    #[test]
    fn mouse_paste_focus_and_replies() {
        let mut d = Decoder::new();
        let v = d.feed(b"\x1b[<0;10;5M\x1b[<0;10;5m\x1b[<64;1;1M\x1b[<32;3;4M\x1b[<35;3;4M");
        let kinds: Vec<MouseEventKind> = v
            .iter()
            .map(|i| match i {
                Input::Event(Event::Mouse(m)) => m.kind,
                o => panic!("{o:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            [
                MouseEventKind::Down(CtButton::Left),
                MouseEventKind::Up(CtButton::Left),
                MouseEventKind::ScrollUp,
                MouseEventKind::Drag(CtButton::Left),
                MouseEventKind::Moved
            ]
        );
        if let Input::Event(Event::Mouse(m)) = &v[0] {
            assert_eq!((m.column, m.row), (9, 4));
        }
        let v = d.feed(b"\x1b[200~line\x1b[A\r\n\x1b[201~\x1b[I\x1b[O");
        assert_eq!(
            v,
            [
                Input::Event(Event::Paste("line\x1b[A\r\n".into())),
                Input::Event(Event::FocusGained),
                Input::Event(Event::FocusLost)
            ]
        );
        // Replies are skipped: DECRPM, kitty flags, OSC 11, DA1, cursor report.
        let v = d.feed(b"\x1b[?2026;2$y\x1b[?31u\x1b]11;rgb:0000/0000/0000\x1b\\\x1b[?62;4c\x1b[12;40R\x1bP>|kitty\x1b\\");
        assert!(v.is_empty(), "{v:?}");
    }

    #[test]
    fn split_sequences_wait_and_a_lone_escape_flushes() {
        let mut d = Decoder::new();
        assert!(d.feed(b"\x1b[50;7").is_empty());
        assert_eq!(d.wait(), Some(SEQ_WAIT));
        let v = d.feed(b";64u");
        assert!(matches!(&v[0], Input::Key(k) if k.text.as_deref() == Some("@")));
        assert!(d.feed(b"\x1b").is_empty());
        assert_eq!(
            d.flush(),
            [named(NamedKey::Escape, Mods::empty(), KeyKind::Press)]
        );
        // A paste split anywhere, terminator included.
        assert!(d.feed(b"\x1b[200~ab").is_empty());
        assert!(d.feed(b"c\x1b[20").is_empty());
        assert_eq!(d.feed(b"1~"), [Input::Event(Event::Paste("abc".into()))]);
        // UTF-8 split across reads.
        let e = "€".as_bytes();
        assert!(d.feed(&e[..1]).is_empty());
        assert!(matches!(&d.feed(&e[1..])[0], Input::Key(k) if k.key == Key::Char('€')));
    }

    use vk_term::encode::{InputModes, encode_key};

    fn all(d: &mut Decoder, b: &[u8]) -> Vec<KeyEvent> {
        d.feed(b)
            .into_iter()
            .filter_map(|i| match i {
                Input::Key(k) => Some(k),
                _ => None,
            })
            .collect()
    }

    fn km(k: &KeyEvent) -> (Key, Mods) {
        (k.key, k.mods)
    }

    fn legacy_lf() -> InputModes {
        InputModes {
            shift_enter_lf: true,
            ..Default::default()
        }
    }

    #[test]
    fn modify_other_keys_sequences_decode() {
        let k = one(b"\x1b[27;2;13~");
        assert_eq!(km(&k), (Key::Named(NamedKey::Enter), Mods::SHIFT));
        let k = one(b"\x1b[27;6;108~");
        assert_eq!(km(&k), (Key::Char('L'), Mods::CTRL | Mods::SHIFT));
        let k = one(b"\x1b[27;6;76~");
        assert_eq!(km(&k), (Key::Char('L'), Mods::CTRL | Mods::SHIFT));
        let k = one(b"\x1b[27;5;9~");
        assert_eq!(km(&k), (Key::Named(NamedKey::Tab), Mods::CTRL));
        // The code is already the shifted symbol: shift is implicit.
        let k = one(b"\x1b[27;6;33~");
        assert_eq!(km(&k), (Key::Char('!'), Mods::CTRL));
        let k = one(b"\x1b[27;3;97~");
        assert_eq!((km(&k), k.text), ((Key::Char('a'), Mods::ALT), None));
        // Private-use codes are never text.
        assert!(keys(b"\x1b[27;1;57428~").is_empty());
        // Back to the pane: ctrl+shift+l through a modifyOtherKeys-2 pane round-trips.
        let mok2 = InputModes {
            modify_other_keys: 2,
            ..Default::default()
        };
        assert_eq!(encode_key(&one(b"\x1b[27;6;108~"), &mok2), b"\x1b[27;6;76~");
        assert_eq!(encode_key(&one(b"\x1b[27;5;105~"), &legacy_lf()), b"\t");
    }

    #[test]
    fn shift_enter_reaches_a_legacy_pane_as_a_newline() {
        // The server's default is `keys.shift_enter_legacy = "lf"` (render.rs → shift_enter_lf).
        assert_eq!(
            vk_config::Config::default().keys.shift_enter_legacy,
            vk_config::ShiftEnterLegacy::Lf
        );
        // iTerm2 mapped to `\n`, kitty `CSI 13;2u`, modifyOtherKeys `CSI 27;2;13~`.
        for b in [&b"\n"[..], b"\x1b[13;2u", b"\x1b[27;2;13~"] {
            assert_eq!(encode_key(&one(b), &legacy_lf()), b"\n", "{b:?}");
        }
        // With `cr`, the protocol forms are a plain Enter; LF stays LF.
        let cr = InputModes::default();
        assert_eq!(encode_key(&one(b"\x1b[13;2u"), &cr), b"\r");
        assert_eq!(encode_key(&one(b"\x1b[27;2;13~"), &cr), b"\r");
        assert_eq!(encode_key(&one(b"\n"), &cr), b"\n");
        assert_eq!(encode_key(&one(b"\r"), &legacy_lf()), b"\r");
        // A kitty pane hears shift+enter as such.
        let kitty = InputModes {
            kitty_flags: 1,
            ..legacy_lf()
        };
        assert_eq!(encode_key(&one(b"\x1b[27;2;13~"), &kitty), b"\x1b[13;2u");
    }

    #[test]
    fn split_sequences_wait_long_and_their_tails_never_become_text() {
        let mut d = Decoder::new();
        // Only an ESC (plus one introducer) settles quickly.
        d.feed(b"\x1b");
        assert_eq!(d.wait(), Some(ESC_WAIT));
        d.feed(b"[");
        assert_eq!(d.wait(), Some(INTRO_WAIT));
        d.feed(b"1;");
        assert_eq!(d.wait(), Some(SEQ_WAIT));
        assert_eq!(all(&mut d, b"5A")[0].mods, Mods::CTRL);
        assert_eq!(d.wait(), None);
        // A sequence given up on: its late tail is dropped, what follows it is kept.
        assert!(d.feed(b"\x1b[<0;10;").is_empty());
        assert!(d.flush().is_empty());
        assert_eq!(d.wait(), Some(SEQ_WAIT), "a tail is expected for a while");
        let k = all(&mut d, b"5Mx");
        assert_eq!(k.len(), 1);
        assert_eq!(k[0].key, Key::Char('x'));
        // A kitty key split in two, with the tail across two late reads.
        d.feed(b"\x1b[57");
        assert!(d.flush().is_empty());
        assert!(d.feed(b"44").is_empty());
        assert!(d.feed(b"1;2u").is_empty());
        assert_eq!(all(&mut d, b"y")[0].key, Key::Char('y'));
        // Once the wait for a tail passes, letters are typing again (they are CSI finals too).
        d.feed(b"\x1b[1;");
        d.flush();
        assert!(d.flush().is_empty());
        assert_eq!(d.wait(), None);
        assert_eq!(all(&mut d, b"A")[0].key, Key::Char('A'));
        // An OSC reply split after a timeout: the rest is dropped up to its terminator.
        d.feed(b"\x1b]11;rgb:00");
        d.flush();
        assert!(d.feed(b"00/0000/00").is_empty());
        assert!(d.feed(b"00\x1b").is_empty());
        assert_eq!(all(&mut d, b"\\a")[0].key, Key::Char('a'));
        // ... unless what follows cannot be a reply's body.
        d.feed(b"\x1b]11;rg");
        d.flush();
        let k = all(&mut d, b"\rq");
        assert_eq!(k[0].key, Key::Named(NamedKey::Enter));
        assert_eq!(k[1].key, Key::Char('q'));
        // A partial UTF-8 character outlives the timeout and completes later.
        let e = "€".as_bytes();
        d.feed(&e[..2]);
        assert_eq!(d.wait(), Some(SEQ_WAIT));
        assert!(d.flush().is_empty());
        assert_eq!(d.wait(), None, "held without a timer");
        assert_eq!(all(&mut d, &e[2..])[0].key, Key::Char('€'));
        // An invalid lead byte is dropped, not held.
        assert_eq!(all(&mut d, b"\xffz")[0].key, Key::Char('z'));
        // A split bracketed paste is never flushed.
        d.feed(b"\x1b[200~abc");
        assert_eq!(d.wait(), None);
        assert!(d.flush().is_empty());
        assert_eq!(
            d.feed(b"\x1b[201~"),
            [Input::Event(Event::Paste("abc".into()))]
        );
    }

    #[test]
    fn a_mouse_report_split_after_escape_never_types_its_tail() {
        let mut d = Decoder::new();
        d.feed(b"\x1b");
        assert_eq!(
            d.flush(),
            [named(NamedKey::Escape, Mods::empty(), KeyKind::Press)]
        );
        assert!(d.feed(b"[<0;3").is_empty());
        assert!(d.feed(b";4M").is_empty());
        assert_eq!(all(&mut d, b"k")[0].key, Key::Char('k'));
        // Split after `ESC [` (alt+[ by then): the `<…M` tail is dropped too.
        d.feed(b"\x1b[");
        let k = d.flush();
        assert!(
            matches!(&k[..], [Input::Key(k)] if k.key == Key::Char('[') && k.mods == Mods::ALT)
        );
        assert!(d.feed(b"<35;1;1M").is_empty());
        // Not a mouse tail: typed as usual, at once or after the wait.
        d.feed(b"\x1b");
        d.flush();
        let k = all(&mut d, b"[x");
        assert_eq!(
            k.iter().map(|k| k.key).collect::<Vec<_>>(),
            [Key::Char('['), Key::Char('x')]
        );
        d.feed(b"\x1b");
        d.flush();
        assert!(d.feed(b"[").is_empty());
        assert!(matches!(&d.flush()[..], [Input::Key(k)] if k.key == Key::Char('[')));
        // ESC and a whole report in one read: Escape, then the mouse event.
        let mac = Policy {
            doubled_escape_is_alt: true,
            ..Policy::default()
        };
        for p in [Policy::default(), mac] {
            let mut d = Decoder::with_policy(p);
            let v = d.feed(b"\x1b\x1b[<0;1;1M");
            assert_eq!(v.len(), 2, "{v:?}");
            assert_eq!(v[0], named(NamedKey::Escape, Mods::empty(), KeyKind::Press));
            assert!(matches!(v[1], Input::Event(Event::Mouse(_))));
        }
    }

    #[test]
    fn private_use_codes_are_named_keys_or_nothing() {
        assert_eq!(one(b"\x1b[57364u").key, Key::Named(NamedKey::F(1)));
        assert_eq!(one(b"\x1b[57375;5u").key, Key::Named(NamedKey::F(12)));
        // KP Begin, media keys, hyper/meta/ISO level shifts, Cocoa function-key markers.
        for code in [
            57427, 57428, 57440, 57445, 57446, 57451, 57452, 57453, 57454, 63232,
        ] {
            let b = format!("\x1b[{code}u");
            assert!(keys(b.as_bytes()).is_empty(), "{code}");
            let b = format!("\x1b[{code};1:3u");
            assert!(keys(b.as_bytes()).is_empty(), "{code} release");
        }
        // A private-use shifted or base-layout alternate is dropped, the key kept.
        let k = one(b"\x1b[97:57428:57428u");
        assert_eq!(
            (k.key, k.shifted, k.base_layout_key),
            (Key::Char('a'), None, None)
        );
    }

    #[test]
    fn ss3_keypad_and_modified_ss3() {
        let k = one(b"\x1bOp");
        assert_eq!((k.key, k.text.as_deref()), (Key::Char('0'), Some("0")));
        assert_eq!(one(b"\x1bOy").key, Key::Char('9'));
        assert_eq!(one(b"\x1bOj").key, Key::Char('*'));
        assert_eq!(one(b"\x1bOk").key, Key::Char('+'));
        assert_eq!(one(b"\x1bOm").key, Key::Char('-'));
        assert_eq!(one(b"\x1bOn").key, Key::Char('.'));
        assert_eq!(one(b"\x1bOo").key, Key::Char('/'));
        assert_eq!(one(b"\x1bOM").key, Key::Named(NamedKey::Enter));
        assert_eq!(one(b"\x1bOA").key, Key::Named(NamedKey::Up));
        let k = one(b"\x1bO2P");
        assert_eq!(km(&k), (Key::Named(NamedKey::F(1)), Mods::SHIFT));
        assert_eq!(encode_key(&one(b"\x1bOp"), &legacy_lf()), b"0");
        // Unknown SS3: consumed, nothing typed, what follows kept.
        assert_eq!(keys(b"\x1bOzq").len(), 1);
        // Waits for its final byte.
        let mut d = Decoder::new();
        assert!(d.feed(b"\x1bO").is_empty());
        assert_eq!(all(&mut d, b"q")[0].key, Key::Char('1'));
    }

    #[test]
    fn doubled_escape_is_alt_where_the_host_sends_it_so() {
        let mac = Policy {
            doubled_escape_is_alt: true,
            ..Policy::default()
        };
        let mut d = Decoder::with_policy(mac);
        let k = all(&mut d, b"\x1b\x1b[A");
        assert_eq!(k.len(), 1);
        assert_eq!(km(&k[0]), (Key::Named(NamedKey::Up), Mods::ALT));
        assert_eq!(
            km(&all(&mut d, b"\x1b\x1bOD")[0]),
            (Key::Named(NamedKey::Left), Mods::ALT)
        );
        assert_eq!(
            encode_key(&all(&mut d, b"\x1b\x1b[C")[0], &legacy_lf()),
            b"\x1b[1;3C"
        );
        // `ESC ESC` alone waits briefly, then is alt+Escape.
        assert!(d.feed(b"\x1b\x1b").is_empty());
        assert_eq!(d.wait(), Some(INTRO_WAIT));
        assert_eq!(
            d.flush(),
            [named(NamedKey::Escape, Mods::ALT, KeyKind::Press)]
        );
        // Escape then a letter is Escape, alt+letter.
        let k = all(&mut d, b"\x1b\x1bx");
        assert_eq!(k[0].key, Key::Named(NamedKey::Escape));
        assert_eq!(km(&k[1]), (Key::Char('x'), Mods::ALT));
        // Elsewhere it is Escape, then the key.
        let k = keys(b"\x1b\x1b[D");
        assert_eq!(k.len(), 2);
        assert_eq!(km(&k[0]), (Key::Named(NamedKey::Escape), Mods::empty()));
        assert_eq!(km(&k[1]), (Key::Named(NamedKey::Left), Mods::empty()));
    }

    #[test]
    fn c0_punctuation_backspace_and_f3() {
        for (b, c) in [(0x1c, '\\'), (0x1d, ']'), (0x1e, '^'), (0x1f, '_')] {
            let k = one(&[b]);
            assert_eq!(km(&k), (Key::Char(c), Mods::CTRL));
            assert_eq!(
                encode_key(&k, &legacy_lf()),
                [b],
                "legacy pane gets the byte back"
            );
        }
        let kitty = InputModes {
            kitty_flags: 1,
            ..Default::default()
        };
        assert_eq!(encode_key(&one(b"\x1c"), &kitty), b"\x1b[92;5u");
        // 0x08: ctrl+h (the pane gets 0x08 back), or Backspace where the host's erase is ^H.
        let k = one(b"\x08");
        assert_eq!(km(&k), (Key::Char('h'), Mods::CTRL));
        assert_eq!(encode_key(&k, &legacy_lf()), b"\x08");
        let mut d = Decoder::with_policy(Policy {
            erase_is_bs: true,
            ..Policy::default()
        });
        let k = all(&mut d, b"\x08\x1b\x08");
        assert_eq!(km(&k[0]), (Key::Named(NamedKey::Backspace), Mods::empty()));
        assert_eq!(km(&k[1]), (Key::Named(NamedKey::Backspace), Mods::ALT));
        assert_eq!(encode_key(&k[0], &legacy_lf()), b"\x7f");
        // Modified F3 vs cursor position reports.
        let k = one(b"\x1b[1;5R");
        assert_eq!(km(&k), (Key::Named(NamedKey::F(3)), Mods::CTRL));
        assert_eq!(one(b"\x1b[13~").key, Key::Named(NamedKey::F(3)));
        for b in [&b"\x1b[R"[..], b"\x1b[1;1R", b"\x1b[12;40R"] {
            assert!(keys(b).is_empty(), "{b:?}");
        }
        assert_eq!(one(b"\x1b[14;3~").mods, Mods::ALT);
    }

    #[test]
    fn kitty_layout_and_shift_recovery() {
        // A shifted alternate without the shift bit: shift was held.
        let k = one(b"\x1b[114:82;1u");
        assert_eq!(km(&k), (Key::Char('R'), Mods::SHIFT));
        assert_eq!(one(b"\x1b[114:82;1:3u").kind, KeyKind::Release);
        for b in [&b"\x1b[114;1u"[..], b"\x1b[114:114;1u", b"\x1b[114::113;1u"] {
            assert_eq!(km(&one(b)), (Key::Char('r'), Mods::empty()), "{b:?}");
        }
        // Ctrl on a Russian layout: the physical key's shortcut, ^C for a legacy pane.
        let k = one(b"\x1b[1089::99;5u");
        assert_eq!(km(&k), (Key::Char('c'), Mods::CTRL));
        assert_eq!(encode_key(&k, &legacy_lf()), [0x03]);
        let k = one(b"\x1b[1089:1057:99;6u");
        assert_eq!(km(&k), (Key::Char('C'), Mods::CTRL | Mods::SHIFT));
        // Without ctrl the layout's own character stays.
        let k = one(b"\x1b[1089::99u");
        assert_eq!(k.key, Key::Char('с'));
    }

    #[test]
    fn replies_and_strings_mixed_with_keys() {
        let mut d = Decoder::new();
        let k = all(
            &mut d,
            b"a\x1b]11;rgb:1/2/3\x07b\x1b]10;rgb:1/2/3\x1b\\c\x1b[?997;1nd\x1b[I",
        );
        assert_eq!(
            k.iter().map(|k| k.key).collect::<Vec<_>>(),
            [
                Key::Char('a'),
                Key::Char('b'),
                Key::Char('c'),
                Key::Char('d')
            ]
        );
        // Only OSC ends at BEL; DCS/APC run to ST.
        let k = all(&mut d, b"\x1bP>|x\x07y\x1b\\z\x1b_G;OK\x07w\x1b\\v");
        assert_eq!(
            k.iter().map(|k| k.key).collect::<Vec<_>>(),
            [Key::Char('z'), Key::Char('v')]
        );
        // A lone `ESC [` followed by a non-sequence byte: alt+[, then the byte.
        let k = keys(b"\x1b[\r");
        assert_eq!(km(&k[0]), (Key::Char('['), Mods::ALT));
        assert_eq!(k[1].key, Key::Named(NamedKey::Enter));
    }
}

#[cfg(target_arch = "wasm32")]
fn host_erase() -> Option<u8> {
    None
}
