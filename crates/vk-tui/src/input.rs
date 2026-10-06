//! The client's own host-input decoder (03 §7.1), used when the host speaks the kitty keyboard
//! protocol. crossterm's decoder drops the third CSI u field (associated text, flag 16), so
//! AltGr and dead-key text never reached the keymap; this one keeps every field:
//!
//! `CSI code[:shifted[:base]] ; mods[:event] ; text[:text…] u` →
//! [`KeyEvent`]`{key, shifted, base_layout_key, mods, kind, text}`.
//!
//! It also decodes what else the host sends while Vibeke runs (legacy and `CSI 1;mods X` /
//! `CSI n;mods ~` keys, SS3 keys, SGR and X10 mouse, bracketed paste, focus in/out) into
//! crossterm [`Event`]s, so the rest of the client is unchanged; replies and strings the host
//! sends (DCS, OSC, APC, `CSI ?…`, cursor reports) are skipped. Window resizes come from
//! `SIGWINCH`. Hosts without the kitty protocol keep crossterm's `EventStream`.

use crossterm::event::{
    Event, KeyModifiers, MouseButton as CtButton, MouseEvent as CtMouse, MouseEventKind,
};
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

/// Streaming decoder: feed bytes as they arrive; an incomplete sequence waits for more, and
/// [`Decoder::flush`] (after a short quiet period) settles it (a lone `ESC` is the Escape key).
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
    paste: Option<Vec<u8>>,
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

/// Kitty functional key codes (private-use area) and the C0 ones the protocol uses.
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

/// A `CSI … u` key (kitty keyboard protocol).
fn csi_u(p: &[u8]) -> Option<Input> {
    let ps = params(p);
    let code = first(&ps, 0)?;
    let shifted = ps
        .first()
        .and_then(|g| g.get(1).copied().flatten())
        .and_then(char::from_u32);
    let base = ps
        .first()
        .and_then(|g| g.get(2).copied().flatten())
        .and_then(char::from_u32);
    let mods = mods_of(first(&ps, 1).unwrap_or(1));
    let kind = kind_of(
        ps.get(1)
            .and_then(|g| g.get(1).copied().flatten())
            .unwrap_or(1),
    );
    let text: Option<String> = ps.get(2).map(|g| {
        g.iter()
            .filter_map(|c| c.and_then(char::from_u32))
            .collect::<String>()
    });
    let text = text.filter(|t| !t.is_empty() && !t.chars().any(char::is_control));
    if let Some(n) = functional(code) {
        let mut e = KeyEvent::new(Key::Named(n), mods);
        e.kind = kind;
        return Some(Input::Key(e));
    }
    let c = keypad_char(code).or_else(|| char::from_u32(code))?;
    let mut mods = mods;
    // As crossterm does: with shift and a reported shifted key, the key is the shifted one.
    let ch = match shifted {
        Some(s) if mods.shift() => s,
        _ => c,
    };
    let key = Key::Char(ch);
    // Shift is implicit in a shifted non-letter (`?`), as for legacy hosts.
    if !ch.is_alphabetic() && ch != ' ' && mods == Mods::SHIFT && shifted.is_some() {
        mods = Mods::empty();
    }
    let mut e = self::key(key, mods);
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

impl Decoder {
    pub fn new() -> Decoder {
        Decoder::default()
    }

    /// An incomplete sequence is waiting (the caller flushes it after a short quiet period).
    pub fn pending(&self) -> bool {
        !self.buf.is_empty() && self.paste.is_none()
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Input> {
        self.buf.extend_from_slice(bytes);
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
            match parse(&self.buf[i..]) {
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
        self.buf.drain(..i.min(self.buf.len()));
        if self.buf.len() > PENDING_MAX && self.paste.is_none() {
            self.buf.clear();
        }
        out
    }

    /// The quiet period passed: what is waiting is complete as it is (a lone `ESC` is the
    /// Escape key; an unfinished sequence is dropped).
    pub fn flush(&mut self) -> Vec<Input> {
        if self.paste.is_some() {
            return vec![];
        }
        let b = std::mem::take(&mut self.buf);
        if b == [0x1b] {
            return vec![named(NamedKey::Escape, Mods::empty(), KeyKind::Press)];
        }
        vec![]
    }
}

const PASTE_END: &[u8] = b"\x1b[201~";

/// One input at the start of `b`.
fn parse(b: &[u8]) -> Step {
    match b[0] {
        0x1b => escape(b),
        0x0d | 0x0a => Step::Done(
            1,
            vec![named(NamedKey::Enter, Mods::empty(), KeyKind::Press)],
        ),
        0x09 => Step::Done(1, vec![named(NamedKey::Tab, Mods::empty(), KeyKind::Press)]),
        0x7f => Step::Done(
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
                Key::Char((b'4' + c - 0x1c) as char),
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
                return Step::More;
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

fn escape(b: &[u8]) -> Step {
    let Some(&c1) = b.get(1) else {
        return Step::More;
    };
    match c1 {
        b'[' => csi(b),
        b'O' => {
            let Some(&f) = b.get(2) else {
                return Step::More;
            };
            match letter_key(f) {
                Some(n) => Step::Done(3, vec![named(n, Mods::empty(), KeyKind::Press)]),
                None => Step::Done(3, vec![]),
            }
        }
        // Strings the host sends (replies): skip to ST or BEL.
        b']' | b'P' | b'_' | b'^' | b'X' => {
            let body = &b[2..];
            for (k, w) in body.iter().enumerate() {
                if *w == 0x07 {
                    return Step::Done(2 + k + 1, vec![]);
                }
                if *w == 0x1b && body.get(k + 1) == Some(&b'\\') {
                    return Step::Done(2 + k + 2, vec![]);
                }
            }
            Step::More
        }
        0x1b => Step::Done(
            1,
            vec![named(NamedKey::Escape, Mods::empty(), KeyKind::Press)],
        ),
        _ => match parse(&b[1..]) {
            // ESC + key: alt+key (legacy encoding).
            Step::Done(n, mut ins) => {
                for x in &mut ins {
                    if let Input::Key(k) = x {
                        k.mods = k.mods.union(Mods::ALT);
                        k.text = None;
                    }
                }
                Step::Done(1 + n, ins)
            }
            Step::More => Step::More,
            Step::Paste(_) => Step::Done(1, vec![]),
        },
    }
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
        // Malformed: drop the introducer.
        return Step::Done(2, vec![]);
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
        let m = ps.get(1);
        (
            mods_of(m.and_then(|g| g.first().copied().flatten()).unwrap_or(1)),
            kind_of(m.and_then(|g| g.get(1).copied().flatten()).unwrap_or(1)),
        )
    };
    let ins: Vec<Input> = match f {
        b'u' => csi_u(p).into_iter().collect(),
        b'~' => match first(&ps, 0) {
            Some(200) => return Step::Paste(n),
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
        // `CSI row ; col R` is a cursor position report (kitty sends F3 as `CSI 13 ~`).
        b'R' if !p.is_empty() => vec![],
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

/// How long an incomplete sequence (a lone `ESC`) waits for the rest.
const ESC_WAIT_MS: i32 = 30;

/// Reads the host's input on a thread (stdin, `poll` with a stop pipe: no timer wake-ups while
/// idle), decodes it, and resizes from `SIGWINCH`. Dropping it stops reading (the external
/// editor and the appearance re-probe need stdin).
pub struct Reader {
    rx: tokio::sync::mpsc::UnboundedReceiver<Input>,
    stop_w: libc::c_int,
    thread: Option<std::thread::JoinHandle<()>>,
    winch: Option<tokio::signal::unix::Signal>,
}

impl Reader {
    pub fn new() -> Reader {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: pipe() fills two fds.
        let ok = unsafe { libc::pipe(fds.as_mut_ptr()) } == 0;
        let (stop_r, stop_w) = if ok { (fds[0], fds[1]) } else { (-1, -1) };
        let thread = std::thread::spawn(move || read_loop(tx, stop_r));
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

impl Default for Reader {
    fn default() -> Self {
        Self::new()
    }
}

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

fn read_loop(tx: tokio::sync::mpsc::UnboundedSender<Input>, stop_r: libc::c_int) {
    let mut dec = Decoder::new();
    loop {
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
        let timeout = if dec.pending() { ESC_WAIT_MS } else { -1 };
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

/// The client's input source: this decoder on kitty hosts, crossterm's elsewhere.
pub enum Source {
    Crossterm(crossterm::event::EventStream),
    Own(Reader),
}

impl Source {
    pub fn new(kitty: bool) -> Source {
        if kitty && std::env::var("VIBEKE_CROSSTERM_INPUT").map_or(true, |v| v != "1") {
            Source::Own(Reader::new())
        } else {
            Source::Crossterm(crossterm::event::EventStream::new())
        }
    }

    pub async fn next(&mut self) -> Option<std::io::Result<Input>> {
        use futures::StreamExt;
        match self {
            Source::Crossterm(s) => s.next().await.map(|r| r.map(Input::Event)),
            Source::Own(r) => r.next().await,
        }
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
        assert!(d.pending());
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
}
