//! The one canonical server-side input encoder (03 §7.2).
//!
//! Turns logical [`KeyEvent`]/[`MouseEvent`]/paste/focus into the bytes a pane's app asked
//! for. Mode precedence for keys: kitty keyboard protocol (when flag 1 or 8 is set) >
//! xterm `modifyOtherKeys` (1 or 2) > legacy xterm.
//!
//! Judgement calls (also noted inline):
//! - Kitty flags without 1 or 8 (only 2/4/16) behave like legacy (press/repeat) and
//!   report nothing for releases; real apps always push flag 1 with them.
//! - Legacy/`modifyOtherKeys` have no encoding for super/hyper on text keys; legacy sends
//!   nothing for them. `meta` is folded into `alt`.
//! - Keypad keys and SGR-pixel mouse (1016) are not modelled by the input types.

use vk_proto::input::{Key, KeyEvent, KeyKind, Mods, MouseButton, MouseEvent, MouseKind, NamedKey};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseMode {
    #[default]
    Off,
    /// DECSET 9: button presses only, no modifiers.
    X10,
    /// DECSET 1000: press + release + wheel.
    Normal,
    /// DECSET 1002: also motion while a button is held.
    ButtonEvent,
    /// DECSET 1003: all motion.
    AnyEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InputModes {
    /// DECCKM.
    pub app_cursor: bool,
    /// DECKPAM (no keypad keys exist in the logical model; carried for completeness).
    pub app_keypad: bool,
    /// DECSET 2004.
    pub bracketed_paste: bool,
    /// DECSET 1004.
    pub focus_events: bool,
    /// Kitty keyboard progressive enhancement flags, 0..31.
    pub kitty_flags: u8,
    /// xterm modifyOtherKeys level: 0, 1 or 2.
    pub modify_other_keys: u8,
    pub mouse: MouseMode,
    /// DECSET 1006.
    pub mouse_sgr: bool,
    /// DECSET 1005.
    pub mouse_utf8: bool,
    /// `keys.shift_enter_legacy = "lf"`: legacy Shift+Enter sends `\n` instead of `\r`.
    pub shift_enter_lf: bool,
}

const ESC: u8 = 0x1b;
const KITTY_DISAMBIGUATE: u8 = 1;
const KITTY_EVENT_TYPES: u8 = 2;
const KITTY_ALTERNATE: u8 = 4;
const KITTY_ALL_KEYS: u8 = 8;
const KITTY_TEXT: u8 = 16;

// ---------------------------------------------------------------------------------------
// Key encoding
// ---------------------------------------------------------------------------------------

/// Encode a key event for the pane's modes. Empty = send nothing.
pub fn encode_key(ev: &KeyEvent, m: &InputModes) -> Vec<u8> {
    let flags = m.kitty_flags & 0x1f;
    let kitty = flags & (KITTY_DISAMBIGUATE | KITTY_ALL_KEYS) != 0;
    if ev.kind == KeyKind::Release && !(kitty && flags & KITTY_EVENT_TYPES != 0) {
        return Vec::new();
    }
    let (key, mods) = normalize(ev);

    // AltGr: ctrl+alt with associated text is text input, not a chord (03 §7.1).
    if let (Key::Char(_), true, Some(t)) = (key, mods.ctrl() && mods.alt(), ev.text.as_deref())
        && !t.is_empty()
        && !t.chars().any(char::is_control)
    {
        return if ev.kind == KeyKind::Release {
            Vec::new()
        } else {
            t.as_bytes().to_vec()
        };
    }

    if kitty {
        encode_kitty(ev, key, mods, m)
    } else if m.modify_other_keys > 0 {
        encode_mok(ev, key, mods, m)
    } else {
        encode_legacy(ev, key, mods, m)
    }
}

/// Space becomes `Char(' ')`; uppercase letters become lowercase + SHIFT.
fn normalize(ev: &KeyEvent) -> (Key, Mods) {
    let mut mods = ev.mods;
    let key = match ev.key {
        Key::Named(NamedKey::Space) => Key::Char(' '),
        Key::Char(c) if c.is_uppercase() => {
            let mut l = c.to_lowercase();
            match (l.next(), l.next()) {
                (Some(lc), None) => {
                    mods = mods | Mods::SHIFT;
                    Key::Char(lc)
                }
                _ => Key::Char(c),
            }
        }
        k => k,
    };
    (key, mods)
}

fn us_shift(c: char) -> char {
    match c {
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '`' => '~',
        '-' => '_',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        c if c.is_alphabetic() => {
            let mut u = c.to_uppercase();
            match (u.next(), u.next()) {
                (Some(x), None) => x,
                _ => c,
            }
        }
        c => c,
    }
}

/// The character this key produces given shift (ignoring `ev.text`).
fn typed_char(c: char, mods: Mods, ev: &KeyEvent) -> char {
    if mods.shift() {
        ev.shifted.unwrap_or_else(|| us_shift(c))
    } else {
        c
    }
}

/// Text to send for a plain (no ctrl/alt/super) text key: host text if known, else
/// derived from the key and shift.
fn plain_text(c: char, mods: Mods, ev: &KeyEvent) -> String {
    match ev.text.as_deref() {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => typed_char(c, mods, ev).to_string(),
    }
}

fn is_alt(mods: Mods) -> bool {
    mods.alt() || mods.contains(Mods::META)
}

fn has_super_or_hyper(mods: Mods) -> bool {
    mods.sup() || mods.contains(Mods::HYPER)
}

/// xterm modifier parameter: 1 + shift(1) + alt(2) + ctrl(4) + super/hyper(8).
fn xterm_mod(mods: Mods) -> u32 {
    1 + u32::from(mods.shift())
        + 2 * u32::from(is_alt(mods))
        + 4 * u32::from(mods.ctrl())
        + 8 * u32::from(has_super_or_hyper(mods))
}

/// Kitty modifier parameter: 1 + bitfield (shift 1, alt 2, ctrl 4, super 8, hyper 16, meta 32).
fn kitty_mod(mods: Mods) -> u32 {
    1 + u32::from(mods.0)
}

fn with_alt(alt: bool, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(body.len() + 1);
    if alt {
        v.push(ESC);
    }
    v.extend_from_slice(body);
    v
}

/// C0 control byte for ctrl+`c` per xterm, if one exists.
fn ctrl_byte(c: char) -> Option<u8> {
    Some(match c {
        'a'..='z' => c as u8 & 0x1f,
        'A'..='Z' => c as u8 & 0x1f,
        ' ' | '@' | '2' => 0x00,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '-' | '7' | '/' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

fn ctrl_byte_for(c: char, mods: Mods, ev: &KeyEvent) -> Option<u8> {
    ctrl_byte(typed_char(c, mods, ev)).or_else(|| ctrl_byte(c))
}

enum Special {
    /// `CSI 1 ; mod F` / `SS3 F`.
    Final(u8),
    /// `CSI n ; mod ~`.
    Tilde(u32),
}

fn special(key: NamedKey) -> Option<Special> {
    Some(match key {
        NamedKey::Up => Special::Final(b'A'),
        NamedKey::Down => Special::Final(b'B'),
        NamedKey::Right => Special::Final(b'C'),
        NamedKey::Left => Special::Final(b'D'),
        NamedKey::Home => Special::Final(b'H'),
        NamedKey::End => Special::Final(b'F'),
        NamedKey::Insert => Special::Tilde(2),
        NamedKey::Delete => Special::Tilde(3),
        NamedKey::PageUp => Special::Tilde(5),
        NamedKey::PageDown => Special::Tilde(6),
        NamedKey::F(n) => match n {
            1 => Special::Final(b'P'),
            2 => Special::Final(b'Q'),
            3 => Special::Final(b'R'),
            4 => Special::Final(b'S'),
            5 => Special::Tilde(15),
            6 => Special::Tilde(17),
            7 => Special::Tilde(18),
            8 => Special::Tilde(19),
            9 => Special::Tilde(20),
            10 => Special::Tilde(21),
            11 => Special::Tilde(23),
            12 => Special::Tilde(24),
            13 => Special::Tilde(25),
            14 => Special::Tilde(26),
            15 => Special::Tilde(28),
            16 => Special::Tilde(29),
            17 => Special::Tilde(31),
            18 => Special::Tilde(32),
            19 => Special::Tilde(33),
            20 => Special::Tilde(34),
            // F21+ have no xterm legacy encoding.
            _ => return None,
        },
        _ => return None,
    })
}

fn legacy_special(key: NamedKey, mods: Mods, m: &InputModes) -> Vec<u8> {
    let Some(sp) = special(key) else {
        return Vec::new();
    };
    let modp = xterm_mod(mods);
    match sp {
        Special::Final(f) => {
            if modp != 1 {
                format!("\x1b[1;{modp}{}", f as char).into_bytes()
            } else if matches!(f, b'P'..=b'S') || m.app_cursor {
                vec![ESC, b'O', f]
            } else {
                vec![ESC, b'[', f]
            }
        }
        Special::Tilde(n) => {
            if modp != 1 {
                format!("\x1b[{n};{modp}~").into_bytes()
            } else {
                format!("\x1b[{n}~").into_bytes()
            }
        }
    }
}

fn encode_legacy(ev: &KeyEvent, key: Key, mods: Mods, m: &InputModes) -> Vec<u8> {
    match key {
        Key::Named(n) if special(n).is_some() => legacy_special(n, mods, m),
        Key::Named(NamedKey::Enter | NamedKey::Tab | NamedKey::Backspace | NamedKey::Escape) => {
            if has_super_or_hyper(mods) {
                return Vec::new();
            }
            let alt = is_alt(mods);
            let Key::Named(n) = key else { unreachable!() };
            match n {
                NamedKey::Enter => {
                    let b: &[u8] = if mods.shift() && m.shift_enter_lf && !mods.ctrl() {
                        b"\n"
                    } else {
                        b"\r"
                    };
                    with_alt(alt, b)
                }
                NamedKey::Tab => {
                    if mods.shift() {
                        with_alt(alt, b"\x1b[Z")
                    } else {
                        with_alt(alt, b"\t")
                    }
                }
                NamedKey::Backspace => with_alt(alt, if mods.ctrl() { b"\x08" } else { b"\x7f" }),
                _ => with_alt(alt, &[ESC]),
            }
        }
        Key::Char(c) => {
            if has_super_or_hyper(mods) {
                return Vec::new();
            }
            let alt = is_alt(mods);
            if !mods.ctrl() && !alt {
                return plain_text(c, mods, ev).into_bytes();
            }
            if mods.ctrl()
                && let Some(b) = ctrl_byte_for(c, mods, ev)
            {
                return with_alt(alt, &[b]);
            }
            // Alt (or ctrl without a C0 mapping, which xterm sends unmodified).
            let mut buf = [0u8; 4];
            let s = typed_char(c, mods, ev).encode_utf8(&mut buf);
            with_alt(alt, s.as_bytes())
        }
        // Lock keys, modifier-only keys, Menu, PrintScreen, Pause: nothing in legacy.
        Key::Named(_) => Vec::new(),
    }
}

fn key_code(key: Key) -> Option<u32> {
    match key {
        Key::Char(c) => Some(c as u32),
        Key::Named(NamedKey::Enter) => Some(13),
        Key::Named(NamedKey::Tab) => Some(9),
        Key::Named(NamedKey::Backspace) => Some(127),
        Key::Named(NamedKey::Escape) => Some(27),
        Key::Named(NamedKey::Space) => Some(32),
        _ => None,
    }
}

/// Whether modifyOtherKeys level 1 reports this combination as `CSI 27;m;c~`, i.e. the
/// legacy encoding would be ambiguous or impossible.
fn mok1_needs_csi(key: Key, mods: Mods, ev: &KeyEvent) -> bool {
    let sh = mods.shift();
    let ct = mods.ctrl();
    let sh_or_hyper = has_super_or_hyper(mods);
    match key {
        Key::Char(c) => {
            sh_or_hyper
                || (ct && ctrl_byte_for(c, mods, ev).is_none())
                || (ct && sh && c.is_alphabetic())
        }
        Key::Named(NamedKey::Enter) => sh || ct || sh_or_hyper,
        Key::Named(NamedKey::Tab) => ct || sh_or_hyper,
        Key::Named(NamedKey::Escape) => sh || ct || sh_or_hyper,
        Key::Named(NamedKey::Backspace) => sh_or_hyper,
        _ => false,
    }
}

fn encode_mok(ev: &KeyEvent, key: Key, mods: Mods, m: &InputModes) -> Vec<u8> {
    let Some(code) = key_code(key) else {
        // Arrows, F-keys, ... use the xterm modified forms like legacy.
        return match key {
            Key::Named(n) => legacy_special(n, mods, m),
            Key::Char(_) => Vec::new(),
        };
    };
    let level2 = m.modify_other_keys >= 2;
    let is_text_char = matches!(key, Key::Char(c) if c != ' ');
    let need = if level2 {
        let others = mods.ctrl() || is_alt(mods) || has_super_or_hyper(mods);
        // Shift alone on a text key just produces different text.
        others || (mods.shift() && !is_text_char)
    } else {
        mok1_needs_csi(key, mods, ev)
    };
    if !need {
        return encode_legacy(ev, key, mods, m);
    }
    let code = match key {
        Key::Char(c) => typed_char(c, mods, ev) as u32,
        _ => code,
    };
    format!("\x1b[27;{};{code}~", xterm_mod(mods)).into_bytes()
}

// ---- kitty ----------------------------------------------------------------------------

fn event_type(kind: KeyKind, flags: u8) -> Option<u8> {
    if flags & KITTY_EVENT_TYPES == 0 {
        return None;
    }
    match kind {
        KeyKind::Press => None,
        KeyKind::Repeat => Some(2),
        KeyKind::Release => Some(3),
    }
}

fn mod_field(modp: u32, evt: Option<u8>) -> String {
    match (modp, evt) {
        (1, None) => String::new(),
        (m, None) => m.to_string(),
        (m, Some(e)) => format!("{m}:{e}"),
    }
}

fn csi_u(
    code: u32,
    shifted: Option<u32>,
    base: Option<u32>,
    modp: u32,
    evt: Option<u8>,
    text: Option<&str>,
) -> Vec<u8> {
    let mut s = format!("\x1b[{code}");
    match (shifted, base) {
        (Some(a), Some(b)) => s.push_str(&format!(":{a}:{b}")),
        (Some(a), None) => s.push_str(&format!(":{a}")),
        (None, Some(b)) => s.push_str(&format!("::{b}")),
        (None, None) => {}
    }
    let mf = mod_field(modp, evt);
    if !mf.is_empty() || text.is_some() {
        s.push(';');
        s.push_str(&mf);
    }
    if let Some(t) = text {
        s.push(';');
        let cps: Vec<String> = t.chars().map(|c| (c as u32).to_string()).collect();
        s.push_str(&cps.join(":"));
    }
    s.push('u');
    s.into_bytes()
}

/// Kitty functional key codes for keys that only exist as `CSI u`.
fn kitty_functional_code(key: NamedKey) -> Option<u32> {
    Some(match key {
        NamedKey::F(n @ 13..=35) => 57376 + u32::from(n) - 13,
        NamedKey::CapsLock => 57358,
        NamedKey::ScrollLock => 57359,
        NamedKey::NumLock => 57360,
        NamedKey::PrintScreen => 57361,
        NamedKey::Pause => 57362,
        NamedKey::Menu => 57363,
        NamedKey::LeftShift => 57441,
        NamedKey::LeftControl => 57442,
        NamedKey::LeftAlt => 57443,
        NamedKey::LeftSuper => 57444,
        NamedKey::RightShift => 57447,
        NamedKey::RightControl => 57448,
        NamedKey::RightAlt => 57449,
        NamedKey::RightSuper => 57450,
        _ => return None,
    })
}

fn own_modifier(key: NamedKey) -> Option<Mods> {
    Some(match key {
        NamedKey::LeftShift | NamedKey::RightShift => Mods::SHIFT,
        NamedKey::LeftControl | NamedKey::RightControl => Mods::CTRL,
        NamedKey::LeftAlt | NamedKey::RightAlt => Mods::ALT,
        NamedKey::LeftSuper | NamedKey::RightSuper => Mods::SUPER,
        _ => return None,
    })
}

fn encode_kitty(ev: &KeyEvent, key: Key, mods: Mods, m: &InputModes) -> Vec<u8> {
    let flags = m.kitty_flags & 0x1f;
    let all = flags & KITTY_ALL_KEYS != 0;
    let evt = event_type(ev.kind, flags);
    let released = ev.kind == KeyKind::Release;
    let modp = kitty_mod(mods);

    match key {
        Key::Char(c) => {
            let non_text_mods = mods.0 & !Mods::SHIFT.0 != 0;
            if !all && !non_text_mods && !released {
                // Plain text keeps being sent as text.
                return plain_text(c, mods, ev).into_bytes();
            }
            let alt_keys = flags & KITTY_ALTERNATE != 0;
            let shifted = if alt_keys && mods.shift() {
                let s = typed_char(c, mods, ev);
                (s != c).then_some(s as u32)
            } else {
                None
            };
            let base = if alt_keys {
                ev.base_layout_key.filter(|b| *b != c).map(|b| b as u32)
            } else {
                None
            };
            let text = if all && flags & KITTY_TEXT != 0 && !non_text_mods && !released {
                let t = plain_text(c, mods, ev);
                (!t.is_empty() && !t.chars().any(char::is_control)).then_some(t)
            } else {
                None
            };
            csi_u(c as u32, shifted, base, modp, evt, text.as_deref())
        }
        Key::Named(n @ (NamedKey::Enter | NamedKey::Tab | NamedKey::Backspace)) => {
            let code = key_code(key).unwrap_or(0);
            if !all && mods.is_empty() {
                // Release of these is only reported with flag 8; presses stay legacy.
                if released {
                    return Vec::new();
                }
                return encode_legacy(ev, key, mods, m);
            }
            let _ = n;
            csi_u(code, None, None, modp, evt, None)
        }
        Key::Named(NamedKey::Escape) => csi_u(27, None, None, modp, evt, None),
        Key::Named(n) => {
            if let Some(sp) = special(n).filter(|_| !matches!(n, NamedKey::F(13..))) {
                if modp == 1 && evt.is_none() {
                    return legacy_special(n, mods, m);
                }
                let mf = mod_field(modp, evt);
                return match sp {
                    Special::Final(b'R') => format!("\x1b[13;{mf}~").into_bytes(),
                    Special::Final(f) => format!("\x1b[1;{mf}{}", f as char).into_bytes(),
                    Special::Tilde(num) => format!("\x1b[{num};{mf}~").into_bytes(),
                };
            }
            let Some(code) = kitty_functional_code(n) else {
                return Vec::new();
            };
            let is_mod_or_lock = own_modifier(n).is_some()
                || matches!(
                    n,
                    NamedKey::CapsLock | NamedKey::ScrollLock | NamedKey::NumLock
                );
            if is_mod_or_lock && !all {
                return Vec::new();
            }
            let mut mods = mods;
            if let Some(own) = own_modifier(n) {
                mods = if released {
                    mods.without(own)
                } else {
                    mods | own
                };
            }
            csi_u(code, None, None, kitty_mod(mods), evt, None)
        }
    }
}

// ---------------------------------------------------------------------------------------
// Paste, focus
// ---------------------------------------------------------------------------------------

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Encode pasted text.
///
/// Bracketed mode: `ESC[200~ text ESC[201~`, with every embedded `ESC[201~` removed
/// (repeatedly, so removal can't splice a new terminator together). Newlines are left
/// as-is (xterm-compatible apps accept LF/CR inside brackets).
///
/// Not bracketed: like xterm and tmux, `\r\n` and lone `\n` become `\r`, as if typed
/// (Enter sends CR). Nothing is stripped because without bracketing there is no
/// terminator to inject.
pub fn encode_paste(text: &str, m: &InputModes) -> Vec<u8> {
    if m.bracketed_paste {
        let mut body = text.as_bytes().to_vec();
        while let Some(pos) = body.windows(PASTE_END.len()).position(|w| w == PASTE_END) {
            body.drain(pos..pos + PASTE_END.len());
        }
        let mut out = Vec::with_capacity(body.len() + 12);
        out.extend_from_slice(PASTE_START);
        out.extend_from_slice(&body);
        out.extend_from_slice(PASTE_END);
        out
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

/// `ESC[I` / `ESC[O` only if the pane enabled focus reporting (DECSET 1004).
pub fn encode_focus(focus_in: bool, m: &InputModes) -> Vec<u8> {
    if !m.focus_events {
        return Vec::new();
    }
    vec![ESC, b'[', if focus_in { b'I' } else { b'O' }]
}

// ---------------------------------------------------------------------------------------
// Mouse
// ---------------------------------------------------------------------------------------

fn is_wheel(b: MouseButton) -> bool {
    matches!(
        b,
        MouseButton::WheelUp
            | MouseButton::WheelDown
            | MouseButton::WheelLeft
            | MouseButton::WheelRight
    )
}

fn button_code(b: MouseButton) -> Option<u32> {
    Some(match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
        MouseButton::WheelLeft => 66,
        MouseButton::WheelRight => 67,
        MouseButton::None => return None,
    })
}

/// Encode a mouse event per the pane's mouse mode and encoding. Events the mode does not
/// report, and coordinates a legacy/UTF-8 encoding cannot represent, produce nothing.
pub fn encode_mouse(ev: &MouseEvent, m: &InputModes) -> Vec<u8> {
    let mode = m.mouse;
    if mode == MouseMode::Off {
        return Vec::new();
    }
    let wheel = is_wheel(ev.button);
    match ev.kind {
        MouseKind::Press => {
            if ev.button == MouseButton::None {
                return Vec::new();
            }
            if mode == MouseMode::X10 && wheel {
                return Vec::new();
            }
        }
        MouseKind::Release => {
            if wheel || mode == MouseMode::X10 {
                return Vec::new();
            }
        }
        MouseKind::Drag => {
            if wheel || !matches!(mode, MouseMode::ButtonEvent | MouseMode::AnyEvent) {
                return Vec::new();
            }
        }
        MouseKind::Move => {
            if mode != MouseMode::AnyEvent || wheel {
                return Vec::new();
            }
        }
    }

    let sgr = m.mouse_sgr;
    let mut cb = match ev.kind {
        MouseKind::Press => button_code(ev.button).unwrap_or(0),
        // Legacy encodings can't say which button was released.
        MouseKind::Release => {
            if sgr {
                button_code(ev.button).unwrap_or(0)
            } else {
                3
            }
        }
        MouseKind::Drag | MouseKind::Move => button_code(ev.button).unwrap_or(3) + 32,
    };
    if mode != MouseMode::X10 {
        if ev.mods.shift() {
            cb += 4;
        }
        if is_alt(ev.mods) {
            cb += 8;
        }
        if ev.mods.ctrl() {
            cb += 16;
        }
    }
    let x = u32::from(ev.col) + 1;
    let y = u32::from(ev.row) + 1;

    if sgr {
        let fin = if ev.kind == MouseKind::Release {
            'm'
        } else {
            'M'
        };
        return format!("\x1b[<{cb};{x};{y}{fin}").into_bytes();
    }
    let mut out = vec![ESC, b'[', b'M'];
    if m.mouse_utf8 {
        if x > 2015 || y > 2015 {
            return Vec::new();
        }
        out.push((32 + cb) as u8);
        for v in [x, y] {
            let ch = char::from_u32(32 + v).unwrap_or(' ');
            let mut buf = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        }
    } else {
        if x > 223 || y > 223 {
            return Vec::new();
        }
        out.extend_from_slice(&[(32 + cb) as u8, (32 + x) as u8, (32 + y) as u8]);
    }
    out
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keygrammar::parse_key;

    fn legacy() -> InputModes {
        InputModes::default()
    }
    fn kitty(flags: u8) -> InputModes {
        InputModes {
            kitty_flags: flags,
            ..Default::default()
        }
    }
    fn mok(level: u8) -> InputModes {
        InputModes {
            modify_other_keys: level,
            ..Default::default()
        }
    }
    fn k(s: &str) -> KeyEvent {
        parse_key(s).unwrap()
    }
    fn enc(s: &str, m: &InputModes) -> Vec<u8> {
        encode_key(&k(s), m)
    }
    fn s(b: Vec<u8>) -> String {
        String::from_utf8(b).unwrap().replace('\x1b', "\\e")
    }
    fn es(key: &str, m: &InputModes) -> String {
        s(enc(key, m))
    }
    fn kind(mut e: KeyEvent, kd: KeyKind) -> KeyEvent {
        e.kind = kd;
        e
    }

    // ---- 03 §7.2 table ----

    #[test]
    fn table_shift_enter() {
        assert_eq!(es("shift+enter", &kitty(1)), "\\e[13;2u");
        assert_eq!(es("shift+enter", &kitty(31)), "\\e[13;2u");
        assert_eq!(es("shift+enter", &mok(2)), "\\e[27;2;13~");
        assert_eq!(es("shift+enter", &legacy()), "\r");
        let lf = InputModes {
            shift_enter_lf: true,
            ..Default::default()
        };
        assert_eq!(enc("shift+enter", &lf), b"\n");
        assert_eq!(enc("enter", &lf), b"\r");
    }

    #[test]
    fn table_ctrl_i_vs_tab() {
        assert_eq!(es("ctrl+i", &kitty(1)), "\\e[105;5u");
        assert_eq!(enc("tab", &kitty(1)), b"\t");
        assert_eq!(es("ctrl+i", &mok(2)), "\\e[27;5;105~");
        assert_eq!(enc("tab", &mok(2)), b"\t");
        assert_eq!(enc("ctrl+i", &legacy()), b"\t");
        assert_eq!(enc("tab", &legacy()), b"\t");
        assert_eq!(enc("ctrl+i", &mok(1)), b"\t");
    }

    #[test]
    fn table_alt_x() {
        assert_eq!(es("alt+x", &kitty(1)), "\\e[120;3u");
        assert_eq!(es("alt+x", &mok(2)), "\\e[27;3;120~");
        assert_eq!(es("alt+x", &legacy()), "\\ex");
        assert_eq!(es("alt+x", &mok(1)), "\\ex");
    }

    // ---- legacy ----

    #[test]
    fn legacy_text() {
        assert_eq!(enc("a", &legacy()), b"a");
        assert_eq!(enc("A", &legacy()), b"A");
        assert_eq!(enc("shift+a", &legacy()), b"A");
        assert_eq!(enc("shift+1", &legacy()), b"!");
        assert_eq!(enc("é", &legacy()), "é".as_bytes());
        assert_eq!(enc("space", &legacy()), b" ");
        assert_eq!(enc("shift+space", &legacy()), b" ");
        assert_eq!(enc("minus", &legacy()), b"-");
        let mut e = k("shift+a");
        e.text = Some("A".into());
        assert_eq!(encode_key(&e, &legacy()), b"A");
        // Layout: host says shift+7 on a Norwegian layout types "/".
        let mut e = k("shift+7");
        e.text = Some("/".into());
        assert_eq!(encode_key(&e, &legacy()), b"/");
    }

    #[test]
    fn legacy_ctrl_letters() {
        for (i, c) in ('a'..='z').enumerate() {
            let e = KeyEvent::new(Key::Char(c), Mods::CTRL);
            assert_eq!(encode_key(&e, &legacy()), vec![i as u8 + 1], "ctrl+{c}");
        }
        assert_eq!(enc("ctrl+space", &legacy()), [0]);
        assert_eq!(enc("ctrl+shift+2", &legacy()), [0]);
        assert_eq!(enc("ctrl+lbracket", &legacy()), [0x1b]);
        assert_eq!(enc("ctrl+backslash", &legacy()), [0x1c]);
        assert_eq!(enc("ctrl+rbracket", &legacy()), [0x1d]);
        assert_eq!(enc("ctrl+shift+6", &legacy()), [0x1e]);
        assert_eq!(enc("ctrl+shift+minus", &legacy()), [0x1f]);
        assert_eq!(enc("ctrl+minus", &legacy()), [0x1f]);
        assert_eq!(enc("ctrl+slash", &legacy()), [0x1f]);
        assert_eq!(enc("ctrl+8", &legacy()), [0x7f]);
        assert_eq!(enc("ctrl+shift+c", &legacy()), [3]);
        // No C0 mapping: xterm sends the plain character.
        assert_eq!(enc("ctrl+1", &legacy()), b"1");
        assert_eq!(enc("ctrl+period", &legacy()), b".");
        assert_eq!(es("ctrl+alt+c", &legacy()), "\\e\x03");
    }

    #[test]
    fn legacy_alt() {
        assert_eq!(es("alt+a", &legacy()), "\\ea");
        assert_eq!(es("alt+shift+p", &legacy()), "\\eP");
        assert_eq!(es("alt+é", &legacy()), "\\eé");
        assert_eq!(es("alt+enter", &legacy()), "\\e\r");
        assert_eq!(es("alt+backspace", &legacy()), "\\e\x7f");
        assert_eq!(es("alt+esc", &legacy()), "\\e\\e");
        assert_eq!(es("alt+tab", &legacy()), "\\e\t");
        assert_eq!(es("alt+space", &legacy()), "\\e ");
        let e = KeyEvent::new(Key::Char('x'), Mods::META);
        assert_eq!(encode_key(&e, &legacy()), b"\x1bx");
        // macOS Option+x text ("≈") must not leak when alt is a chord.
        let mut e = k("alt+x");
        e.text = Some("≈".into());
        assert_eq!(es_ev(&e, &legacy()), "\\ex");
    }

    fn es_ev(e: &KeyEvent, m: &InputModes) -> String {
        s(encode_key(e, m))
    }

    #[test]
    fn legacy_super_is_dropped() {
        assert!(enc("cmd+k", &legacy()).is_empty());
        assert!(enc("super+enter", &legacy()).is_empty());
    }

    #[test]
    fn legacy_basic_named() {
        assert_eq!(enc("enter", &legacy()), b"\r");
        assert_eq!(enc("tab", &legacy()), b"\t");
        assert_eq!(es("shift+tab", &legacy()), "\\e[Z");
        assert_eq!(es("alt+shift+tab", &legacy()), "\\e\\e[Z");
        assert_eq!(enc("backspace", &legacy()), [0x7f]);
        assert_eq!(enc("ctrl+backspace", &legacy()), [0x08]);
        assert_eq!(enc("esc", &legacy()), [0x1b]);
        assert_eq!(enc("ctrl+enter", &legacy()), b"\r");
    }

    #[test]
    fn legacy_cursor_keys() {
        let app = InputModes {
            app_cursor: true,
            ..Default::default()
        };
        for (name, f) in [
            ("up", 'A'),
            ("down", 'B'),
            ("right", 'C'),
            ("left", 'D'),
            ("home", 'H'),
            ("end", 'F'),
        ] {
            assert_eq!(es(name, &legacy()), format!("\\e[{f}"));
            assert_eq!(es(name, &app), format!("\\eO{f}"));
            // Modified forms are identical in both modes.
            assert_eq!(
                es(&format!("ctrl+{name}"), &legacy()),
                format!("\\e[1;5{f}")
            );
            assert_eq!(es(&format!("ctrl+{name}"), &app), format!("\\e[1;5{f}"));
        }
        assert_eq!(es("shift+up", &legacy()), "\\e[1;2A");
        assert_eq!(es("alt+left", &legacy()), "\\e[1;3D");
        assert_eq!(es("ctrl+alt+shift+right", &legacy()), "\\e[1;8C");
        assert_eq!(es("shift+home", &legacy()), "\\e[1;2H");
    }

    #[test]
    fn legacy_tilde_keys() {
        for (name, n) in [("insert", 2), ("delete", 3), ("pageup", 5), ("pagedown", 6)] {
            assert_eq!(es(name, &legacy()), format!("\\e[{n}~"));
            assert_eq!(es(&format!("alt+{name}"), &legacy()), format!("\\e[{n};3~"));
            assert_eq!(
                es(&format!("ctrl+{name}"), &legacy()),
                format!("\\e[{n};5~")
            );
        }
    }

    #[test]
    fn legacy_function_keys() {
        let app = InputModes {
            app_cursor: true,
            ..Default::default()
        };
        for (i, f) in ['P', 'Q', 'R', 'S'].iter().enumerate() {
            let name = format!("f{}", i + 1);
            assert_eq!(es(&name, &legacy()), format!("\\eO{f}"));
            assert_eq!(es(&name, &app), format!("\\eO{f}"));
            assert_eq!(
                es(&format!("shift+{name}"), &legacy()),
                format!("\\e[1;2{f}")
            );
        }
        let codes = [
            15, 17, 18, 19, 20, 21, 23, 24, 25, 26, 28, 29, 31, 32, 33, 34,
        ];
        for (i, c) in codes.iter().enumerate() {
            let name = format!("f{}", i + 5);
            assert_eq!(es(&name, &legacy()), format!("\\e[{c}~"), "{name}");
        }
        assert_eq!(es("ctrl+f5", &legacy()), "\\e[15;5~");
        assert_eq!(es("shift+f12", &legacy()), "\\e[24;2~");
        assert!(enc("f21", &legacy()).is_empty());
    }

    #[test]
    fn legacy_ignores_unrepresentable_keys() {
        assert!(encode_key(&KeyEvent::named(NamedKey::CapsLock), &legacy()).is_empty());
        assert!(encode_key(&KeyEvent::named(NamedKey::LeftShift), &legacy()).is_empty());
        assert!(encode_key(&KeyEvent::named(NamedKey::Menu), &legacy()).is_empty());
    }

    #[test]
    fn altgr_is_text() {
        let mut e = KeyEvent::new(Key::Char('2'), Mods::CTRL | Mods::ALT);
        e.text = Some("@".into());
        for m in [legacy(), mok(2), kitty(1), kitty(31)] {
            assert_eq!(encode_key(&e, &m), b"@");
        }
        let mut e = KeyEvent::new(Key::Char('e'), Mods::CTRL | Mods::ALT);
        e.text = Some("€".into());
        assert_eq!(encode_key(&e, &legacy()), "€".as_bytes());
        // Without text it is a chord.
        let e = KeyEvent::new(Key::Char('e'), Mods::CTRL | Mods::ALT);
        assert_eq!(encode_key(&e, &kitty(1)), b"\x1b[101;7u");
    }

    #[test]
    fn text_preferred_in_legacy() {
        let mut e = KeyEvent::ch('a');
        e.text = Some("å".into());
        assert_eq!(encode_key(&e, &legacy()), "å".as_bytes());
        let mut e = KeyEvent::named(NamedKey::Enter);
        e.text = Some("\r".into());
        assert_eq!(encode_key(&e, &legacy()), b"\r");
    }

    #[test]
    fn release_is_silent_without_event_types() {
        let e = kind(KeyEvent::ch('a'), KeyKind::Release);
        assert!(encode_key(&e, &legacy()).is_empty());
        assert!(encode_key(&e, &mok(2)).is_empty());
        assert!(encode_key(&e, &kitty(1)).is_empty());
        assert!(encode_key(&e, &kitty(8)).is_empty());
        assert!(encode_key(&e, &kitty(4 | 16)).is_empty());
        // Repeat is a normal press elsewhere.
        let e = kind(KeyEvent::ch('a'), KeyKind::Repeat);
        assert_eq!(encode_key(&e, &legacy()), b"a");
        assert_eq!(encode_key(&e, &kitty(1)), b"a");
    }

    // ---- modifyOtherKeys ----

    #[test]
    fn mok2_more() {
        assert_eq!(es("ctrl+c", &mok(2)), "\\e[27;5;99~");
        assert_eq!(es("ctrl+shift+a", &mok(2)), "\\e[27;6;65~");
        assert_eq!(es("ctrl+enter", &mok(2)), "\\e[27;5;13~");
        assert_eq!(es("alt+enter", &mok(2)), "\\e[27;3;13~");
        assert_eq!(es("shift+tab", &mok(2)), "\\e[27;2;9~");
        assert_eq!(es("ctrl+backspace", &mok(2)), "\\e[27;5;127~");
        assert_eq!(es("ctrl+space", &mok(2)), "\\e[27;5;32~");
        assert_eq!(es("shift+esc", &mok(2)), "\\e[27;2;27~");
        assert_eq!(es("cmd+k", &mok(2)), "\\e[27;9;107~");
        assert_eq!(es("ctrl+1", &mok(2)), "\\e[27;5;49~");
        // Plain and shift-only text keys are not reported specially.
        assert_eq!(enc("a", &mok(2)), b"a");
        assert_eq!(enc("shift+a", &mok(2)), b"A");
        assert_eq!(enc("enter", &mok(2)), b"\r");
        assert_eq!(enc("backspace", &mok(2)), [0x7f]);
        assert_eq!(enc("esc", &mok(2)), [0x1b]);
        // Special keys keep xterm forms.
        assert_eq!(es("ctrl+up", &mok(2)), "\\e[1;5A");
        assert_eq!(es("f5", &mok(2)), "\\e[15~");
        assert_eq!(es("delete", &mok(2)), "\\e[3~");
    }

    #[test]
    fn mok1_only_ambiguous() {
        assert_eq!(enc("ctrl+c", &mok(1)), [3]);
        assert_eq!(es("alt+x", &mok(1)), "\\ex");
        assert_eq!(es("shift+enter", &mok(1)), "\\e[27;2;13~");
        assert_eq!(es("ctrl+enter", &mok(1)), "\\e[27;5;13~");
        assert_eq!(enc("enter", &mok(1)), b"\r");
        assert_eq!(es("ctrl+1", &mok(1)), "\\e[27;5;49~");
        assert_eq!(es("ctrl+shift+a", &mok(1)), "\\e[27;6;65~");
        assert_eq!(es("shift+tab", &mok(1)), "\\e[Z");
        assert_eq!(es("ctrl+tab", &mok(1)), "\\e[27;5;9~");
        assert_eq!(es("cmd+k", &mok(1)), "\\e[27;9;107~");
    }

    // ---- kitty ----

    #[test]
    fn kitty_disambiguate_basics() {
        let m = kitty(1);
        assert_eq!(enc("a", &m), b"a");
        assert_eq!(enc("A", &m), b"A");
        assert_eq!(enc("é", &m), "é".as_bytes());
        assert_eq!(enc("space", &m), b" ");
        assert_eq!(enc("enter", &m), b"\r");
        assert_eq!(enc("tab", &m), b"\t");
        assert_eq!(enc("backspace", &m), [0x7f]);
        assert_eq!(es("esc", &m), "\\e[27u");
        assert_eq!(es("ctrl+c", &m), "\\e[99;5u");
        assert_eq!(es("ctrl+shift+a", &m), "\\e[97;6u");
        assert_eq!(es("ctrl+space", &m), "\\e[32;5u");
        assert_eq!(es("shift+tab", &m), "\\e[9;2u");
        assert_eq!(es("ctrl+enter", &m), "\\e[13;5u");
        assert_eq!(es("alt+backspace", &m), "\\e[127;3u");
        assert_eq!(es("alt+shift+p", &m), "\\e[112;4u");
        assert_eq!(es("cmd+k", &m), "\\e[107;9u");
        assert_eq!(es("hyper+k", &m), "\\e[107;17u");
        assert_eq!(es("ctrl+alt+shift+super+x", &m), "\\e[120;16u");
        let meta = KeyEvent::new(Key::Char('x'), Mods::META);
        assert_eq!(es_ev(&meta, &m), "\\e[120;33u");
        assert_eq!(es("alt+minus", &m), "\\e[45;3u");
    }

    #[test]
    fn kitty_functional_keys() {
        let m = kitty(1);
        let app = InputModes {
            kitty_flags: 1,
            app_cursor: true,
            ..Default::default()
        };
        assert_eq!(es("up", &m), "\\e[A");
        assert_eq!(es("up", &app), "\\eOA");
        assert_eq!(es("ctrl+up", &m), "\\e[1;5A");
        assert_eq!(es("ctrl+up", &app), "\\e[1;5A");
        assert_eq!(es("shift+home", &m), "\\e[1;2H");
        assert_eq!(es("end", &m), "\\e[F");
        assert_eq!(es("delete", &m), "\\e[3~");
        assert_eq!(es("shift+delete", &m), "\\e[3;2~");
        assert_eq!(es("pageup", &m), "\\e[5~");
        assert_eq!(es("alt+pagedown", &m), "\\e[6;3~");
        assert_eq!(es("insert", &m), "\\e[2~");
        assert_eq!(es("f1", &m), "\\eOP");
        assert_eq!(es("shift+f1", &m), "\\e[1;2P");
        assert_eq!(es("ctrl+f3", &m), "\\e[13;5~");
        assert_eq!(es("f5", &m), "\\e[15~");
        assert_eq!(es("ctrl+f12", &m), "\\e[24;5~");
        assert_eq!(es("f13", &m), "\\e[57376u");
        assert_eq!(es("shift+f24", &m), "\\e[57387;2u");
        assert_eq!(es("menu", &m), "\\e[57363u");
        assert_eq!(es("printscreen", &m), "\\e[57361u");
        assert_eq!(es("pause", &m), "\\e[57362u");
        // Lock and modifier keys need flag 8.
        assert!(enc("capslock", &m).is_empty());
        assert!(enc("leftshift", &m).is_empty());
    }

    #[test]
    fn kitty_event_types() {
        let m = kitty(1 | 2);
        let press = k("ctrl+c");
        let rep = kind(k("ctrl+c"), KeyKind::Repeat);
        let rel = kind(k("ctrl+c"), KeyKind::Release);
        assert_eq!(es_ev(&press, &m), "\\e[99;5u");
        assert_eq!(es_ev(&rep, &m), "\\e[99;5:2u");
        assert_eq!(es_ev(&rel, &m), "\\e[99;5:3u");
        // Arrows.
        assert_eq!(es_ev(&kind(k("up"), KeyKind::Release), &m), "\\e[1;1:3A");
        assert_eq!(
            es_ev(&kind(k("ctrl+up"), KeyKind::Repeat), &m),
            "\\e[1;5:2A"
        );
        assert_eq!(
            es_ev(&kind(k("delete"), KeyKind::Release), &m),
            "\\e[3;1:3~"
        );
        assert_eq!(es_ev(&kind(k("f3"), KeyKind::Release), &m), "\\e[13;1:3~");
        // Escape release is reported; Enter/Tab/Backspace releases are not without flag 8.
        assert_eq!(es_ev(&kind(k("esc"), KeyKind::Release), &m), "\\e[27;1:3u");
        for n in ["enter", "tab", "backspace"] {
            assert!(encode_key(&kind(k(n), KeyKind::Release), &m).is_empty());
            assert_eq!(encode_key(&kind(k(n), KeyKind::Repeat), &m).len(), 1);
        }
        // Plain text: press/repeat are text, release is an escape code.
        assert_eq!(encode_key(&kind(k("a"), KeyKind::Repeat), &m), b"a");
        assert_eq!(es_ev(&kind(k("a"), KeyKind::Release), &m), "\\e[97;1:3u");
        // Without flag 2 the type is never added to presses.
        assert_eq!(
            es_ev(&kind(k("ctrl+c"), KeyKind::Repeat), &kitty(1)),
            "\\e[99;5u"
        );
    }

    #[test]
    fn kitty_all_keys() {
        let m = kitty(1 | 2 | 8);
        assert_eq!(es("a", &m), "\\e[97u");
        assert_eq!(es("shift+a", &m), "\\e[97;2u");
        assert_eq!(es("enter", &m), "\\e[13u");
        assert_eq!(es("tab", &m), "\\e[9u");
        assert_eq!(es("backspace", &m), "\\e[127u");
        assert_eq!(es("space", &m), "\\e[32u");
        assert_eq!(
            es_ev(&kind(k("enter"), KeyKind::Release), &m),
            "\\e[13;1:3u"
        );
        assert_eq!(
            es_ev(&kind(k("backspace"), KeyKind::Repeat), &m),
            "\\e[127;1:2u"
        );
        assert_eq!(es_ev(&kind(k("a"), KeyKind::Release), &m), "\\e[97;1:3u");
        // Lock and modifier keys.
        assert_eq!(es("capslock", &m), "\\e[57358u");
        assert_eq!(es("numlock", &m), "\\e[57360u");
        assert_eq!(es("scrolllock", &m), "\\e[57359u");
        assert_eq!(es("leftshift", &m), "\\e[57441;2u");
        assert_eq!(es("leftctrl", &m), "\\e[57442;5u");
        assert_eq!(es("leftalt", &m), "\\e[57443;3u");
        assert_eq!(es("leftsuper", &m), "\\e[57444;9u");
        assert_eq!(es("rightshift", &m), "\\e[57447;2u");
        assert_eq!(es("rightctrl", &m), "\\e[57448;5u");
        assert_eq!(es("rightalt", &m), "\\e[57449;3u");
        assert_eq!(es("rightsuper", &m), "\\e[57450;9u");
        assert_eq!(
            es_ev(&kind(k("leftshift"), KeyKind::Release), &m),
            "\\e[57441;1:3u"
        );
        // Release with the host already reporting the modifier as held is still cleared.
        let mut rel = kind(k("shift+leftshift"), KeyKind::Release);
        rel.mods = Mods::SHIFT;
        assert_eq!(es_ev(&rel, &m), "\\e[57441;1:3u");
    }

    #[test]
    fn kitty_associated_text() {
        let m = kitty(1 | 8 | 16);
        let plain = KeyEvent::ch('a');
        assert_eq!(es_ev(&plain, &m), "\\e[97;;97u");
        let shifted = k("shift+a");
        assert_eq!(es_ev(&shifted, &m), "\\e[97;2;65u");
        let mut e = KeyEvent::ch('a');
        e.text = Some("å".into());
        assert_eq!(es_ev(&e, &m), "\\e[97;;229u");
        let mut e = KeyEvent::ch('e');
        e.text = Some("é".into());
        assert_eq!(es_ev(&e, &m), "\\e[101;;233u");
        // Multi codepoint text (dead key composition / IME).
        let mut e = KeyEvent::ch('a');
        e.text = Some("ab".into());
        assert_eq!(es_ev(&e, &m), "\\e[97;;97:98u");
        // No text with ctrl/alt, nor on release, nor for control text.
        assert_eq!(es("ctrl+a", &m), "\\e[97;5u");
        let mut rel = kind(KeyEvent::ch('a'), KeyKind::Release);
        rel.text = Some("a".into());
        assert_eq!(es_ev(&rel, &kitty(1 | 2 | 8 | 16)), "\\e[97;1:3u");
        // With event types the modifier slot must be explicit before the text.
        let rep = kind(KeyEvent::ch('a'), KeyKind::Repeat);
        assert_eq!(es_ev(&rep, &kitty(31)), "\\e[97;1:2;97u");
        // Flag 16 without 8 does not change plain text.
        assert_eq!(enc("a", &kitty(1 | 16)), b"a");
    }

    #[test]
    fn kitty_alternate_keys() {
        let m = kitty(1 | 4);
        let mut e = k("shift+a");
        e.shifted = Some('A');
        e.base_layout_key = Some('a');
        // Shift-only plain text is text.
        assert_eq!(es_ev(&e, &m), "A");
        // ctrl+shift+a: shifted key and no redundant base.
        let mut e = k("ctrl+shift+a");
        e.shifted = Some('A');
        e.base_layout_key = Some('a');
        assert_eq!(es_ev(&e, &m), "\\e[97:65;6u");
        // Non-US layout: key 'ø', base layout 'ö'... base reported when different.
        let mut e = KeyEvent::new(Key::Char('ø'), Mods::CTRL);
        e.base_layout_key = Some('\'');
        assert_eq!(es_ev(&e, &m), "\\e[248::39;5u");
        // Both.
        let mut e = KeyEvent::new(Key::Char('1'), Mods::CTRL | Mods::SHIFT);
        e.shifted = Some('!');
        e.base_layout_key = Some('&');
        assert_eq!(es_ev(&e, &m), "\\e[49:33:38;6u");
        // Derived shifted key for letters.
        assert_eq!(es("ctrl+shift+q", &m), "\\e[113:81;6u");
        // Not reported without flag 4.
        assert_eq!(es_ev(&e, &kitty(1)), "\\e[49;6u");
    }

    #[test]
    fn kitty_flags_without_disambiguate_behave_legacy() {
        let m = kitty(2 | 4);
        assert_eq!(es("alt+x", &m), "\\ex");
        assert_eq!(es("shift+enter", &m), "\r");
    }

    #[test]
    fn kitty_wins_over_mok() {
        let m = InputModes {
            kitty_flags: 1,
            modify_other_keys: 2,
            ..Default::default()
        };
        assert_eq!(es("alt+x", &m), "\\e[120;3u");
    }

    // ---- paste / focus ----

    #[test]
    fn paste() {
        let b = InputModes {
            bracketed_paste: true,
            ..Default::default()
        };
        assert_eq!(s(encode_paste("hi", &b)), "\\e[200~hi\\e[201~");
        assert_eq!(s(encode_paste("", &b)), "\\e[200~\\e[201~");
        assert_eq!(
            s(encode_paste("a\nb\r\nc", &b)),
            "\\e[200~a\nb\r\nc\\e[201~"
        );
        // Injection defence.
        assert_eq!(
            s(encode_paste("x\x1b[201~rm -rf /\n", &b)),
            "\\e[200~xrm -rf /\n\\e[201~"
        );
        // Re-splicing: removing the inner terminator must not create a new one.
        assert_eq!(
            s(encode_paste("\x1b[2\x1b[201~01~", &b)),
            "\\e[200~\\e[201~"
        );
        // Start marker inside is left alone (harmless).
        assert_eq!(s(encode_paste("\x1b[200~", &b)), "\\e[200~\\e[200~\\e[201~");
        // Unicode survives.
        assert_eq!(encode_paste("æøå", &b)[6..][..6], "æøå".as_bytes()[..]);

        let n = legacy();
        assert_eq!(encode_paste("a\nb\r\nc\rd", &n), b"a\rb\rc\rd");
        assert_eq!(encode_paste("plain", &n), b"plain");
        assert_eq!(encode_paste("", &n), b"");
    }

    #[test]
    fn focus() {
        let on = InputModes {
            focus_events: true,
            ..Default::default()
        };
        assert_eq!(encode_focus(true, &on), b"\x1b[I");
        assert_eq!(encode_focus(false, &on), b"\x1b[O");
        assert!(encode_focus(true, &legacy()).is_empty());
        assert!(encode_focus(false, &legacy()).is_empty());
    }

    // ---- mouse ----

    fn me(kind: MouseKind, button: MouseButton, col: u16, row: u16, mods: Mods) -> MouseEvent {
        MouseEvent {
            kind,
            button,
            col,
            row,
            mods,
        }
    }
    fn mm(mode: MouseMode, sgr: bool, utf8: bool) -> InputModes {
        InputModes {
            mouse: mode,
            mouse_sgr: sgr,
            mouse_utf8: utf8,
            ..Default::default()
        }
    }
    use MouseButton as B;
    use MouseKind as K;

    #[test]
    fn mouse_off() {
        let e = me(K::Press, B::Left, 0, 0, Mods::empty());
        assert!(encode_mouse(&e, &legacy()).is_empty());
        assert!(encode_mouse(&e, &mm(MouseMode::Off, true, false)).is_empty());
    }

    #[test]
    fn mouse_legacy_normal() {
        let m = mm(MouseMode::Normal, false, false);
        let press = me(K::Press, B::Left, 0, 0, Mods::empty());
        assert_eq!(encode_mouse(&press, &m), [0x1b, b'[', b'M', 32, 33, 33]);
        let press = me(K::Press, B::Right, 9, 19, Mods::empty());
        assert_eq!(encode_mouse(&press, &m), [0x1b, b'[', b'M', 34, 42, 52]);
        let rel = me(K::Release, B::Left, 9, 19, Mods::empty());
        assert_eq!(encode_mouse(&rel, &m), [0x1b, b'[', b'M', 35, 42, 52]);
        let mid = me(K::Press, B::Middle, 0, 0, Mods::empty());
        assert_eq!(encode_mouse(&mid, &m)[3], 33);
        // Modifiers.
        let e = me(
            K::Press,
            B::Left,
            0,
            0,
            Mods::SHIFT | Mods::ALT | Mods::CTRL,
        );
        assert_eq!(encode_mouse(&e, &m)[3], 32 + 4 + 8 + 16);
        // Wheel.
        for (b, c) in [
            (B::WheelUp, 64),
            (B::WheelDown, 65),
            (B::WheelLeft, 66),
            (B::WheelRight, 67),
        ] {
            let e = me(K::Press, b, 1, 1, Mods::empty());
            assert_eq!(encode_mouse(&e, &m), [0x1b, b'[', b'M', 32 + c, 34, 34]);
            // Wheel releases are never reported.
            let e = me(K::Release, b, 1, 1, Mods::empty());
            assert!(encode_mouse(&e, &m).is_empty());
        }
        // Last representable cell is 223 (col 222); beyond it nothing is sent.
        let edge = me(K::Press, B::Left, 222, 222, Mods::empty());
        assert_eq!(encode_mouse(&edge, &m), [0x1b, b'[', b'M', 32, 255, 255]);
        let over = me(K::Press, B::Left, 223, 0, Mods::empty());
        assert!(encode_mouse(&over, &m).is_empty());
        let over = me(K::Press, B::Left, 0, 300, Mods::empty());
        assert!(encode_mouse(&over, &m).is_empty());
    }

    #[test]
    fn mouse_mode_filters() {
        let drag = me(K::Drag, B::Left, 1, 1, Mods::empty());
        let mv = me(K::Move, B::None, 1, 1, Mods::empty());
        let press = me(K::Press, B::Left, 1, 1, Mods::empty());
        let rel = me(K::Release, B::Left, 1, 1, Mods::empty());
        let wheel = me(K::Press, B::WheelUp, 1, 1, Mods::empty());

        let x10 = mm(MouseMode::X10, false, false);
        assert!(!encode_mouse(&press, &x10).is_empty());
        assert!(encode_mouse(&rel, &x10).is_empty());
        assert!(encode_mouse(&drag, &x10).is_empty());
        assert!(encode_mouse(&mv, &x10).is_empty());
        assert!(encode_mouse(&wheel, &x10).is_empty());
        // X10 never reports modifiers.
        let e = me(K::Press, B::Left, 1, 1, Mods::CTRL | Mods::SHIFT);
        assert_eq!(encode_mouse(&e, &x10), [0x1b, b'[', b'M', 32, 34, 34]);

        let n = mm(MouseMode::Normal, false, false);
        assert!(!encode_mouse(&press, &n).is_empty());
        assert!(!encode_mouse(&rel, &n).is_empty());
        assert!(!encode_mouse(&wheel, &n).is_empty());
        assert!(encode_mouse(&drag, &n).is_empty());
        assert!(encode_mouse(&mv, &n).is_empty());

        let b = mm(MouseMode::ButtonEvent, false, false);
        assert!(!encode_mouse(&drag, &b).is_empty());
        assert!(encode_mouse(&mv, &b).is_empty());
        assert!(!encode_mouse(&press, &b).is_empty());

        let a = mm(MouseMode::AnyEvent, false, false);
        assert!(!encode_mouse(&drag, &a).is_empty());
        assert!(!encode_mouse(&mv, &a).is_empty());
        // Drag code = button + 32.
        assert_eq!(encode_mouse(&drag, &a), [0x1b, b'[', b'M', 32 + 32, 34, 34]);
        // Motion with no button = 3 + 32.
        assert_eq!(encode_mouse(&mv, &a), [0x1b, b'[', b'M', 32 + 35, 34, 34]);
        // Press with no button is meaningless.
        let nb = me(K::Press, B::None, 1, 1, Mods::empty());
        assert!(encode_mouse(&nb, &a).is_empty());
        // Wheel motion is meaningless.
        let wd = me(K::Drag, B::WheelUp, 1, 1, Mods::empty());
        assert!(encode_mouse(&wd, &a).is_empty());
    }

    #[test]
    fn mouse_sgr() {
        let m = mm(MouseMode::AnyEvent, true, false);
        let press = me(K::Press, B::Left, 0, 0, Mods::empty());
        assert_eq!(s(encode_mouse(&press, &m)), "\\e[<0;1;1M");
        let rel = me(K::Release, B::Left, 4, 9, Mods::empty());
        assert_eq!(s(encode_mouse(&rel, &m)), "\\e[<0;5;10m");
        let rel = me(K::Release, B::Right, 4, 9, Mods::CTRL);
        assert_eq!(s(encode_mouse(&rel, &m)), "\\e[<18;5;10m");
        let drag = me(K::Drag, B::Left, 4, 9, Mods::empty());
        assert_eq!(s(encode_mouse(&drag, &m)), "\\e[<32;5;10M");
        let mv = me(K::Move, B::None, 4, 9, Mods::empty());
        assert_eq!(s(encode_mouse(&mv, &m)), "\\e[<35;5;10M");
        let w = me(K::Press, B::WheelDown, 4, 9, Mods::SHIFT);
        assert_eq!(s(encode_mouse(&w, &m)), "\\e[<69;5;10M");
        let w = me(K::Press, B::WheelRight, 0, 0, Mods::ALT);
        assert_eq!(s(encode_mouse(&w, &m)), "\\e[<75;1;1M");
        // Large coordinates.
        let big = me(K::Press, B::Left, 299, 499, Mods::empty());
        assert_eq!(s(encode_mouse(&big, &m)), "\\e[<0;300;500M");
        let big = me(K::Release, B::Middle, 223, 223, Mods::empty());
        assert_eq!(s(encode_mouse(&big, &m)), "\\e[<1;224;224m");
        let huge = me(K::Press, B::Left, u16::MAX, u16::MAX, Mods::empty());
        assert_eq!(s(encode_mouse(&huge, &m)), "\\e[<0;65536;65536M");
        // SGR also applies in X10/Normal, with their filters.
        let n = mm(MouseMode::Normal, true, false);
        assert!(encode_mouse(&drag, &n).is_empty());
        assert_eq!(s(encode_mouse(&press, &n)), "\\e[<0;1;1M");
        // SGR wins over UTF-8 when both are on.
        let both = mm(MouseMode::Normal, true, true);
        assert_eq!(s(encode_mouse(&press, &both)), "\\e[<0;1;1M");
    }

    #[test]
    fn mouse_utf8() {
        let m = mm(MouseMode::Normal, false, true);
        let press = me(K::Press, B::Left, 0, 0, Mods::empty());
        assert_eq!(encode_mouse(&press, &m), [0x1b, b'[', b'M', 32, 33, 33]);
        // col 299 -> x=300 -> 332 -> UTF-8 of U+014C.
        let big = me(K::Press, B::Left, 299, 0, Mods::empty());
        let mut expect = vec![0x1b, b'[', b'M', 32];
        expect.extend_from_slice("\u{14c}".as_bytes());
        expect.push(33);
        assert_eq!(encode_mouse(&big, &m), expect);
        // Edge of the 1005 range: coordinate 2015 -> U+07FF (two bytes).
        let edge = me(K::Press, B::Left, 2014, 2014, Mods::empty());
        let out = encode_mouse(&edge, &m);
        assert_eq!(&out[4..], "\u{7ff}\u{7ff}".as_bytes());
        let over = me(K::Press, B::Left, 2015, 0, Mods::empty());
        assert!(encode_mouse(&over, &m).is_empty());
        // Release is button 3.
        let rel = me(K::Release, B::Left, 0, 0, Mods::empty());
        assert_eq!(encode_mouse(&rel, &m), [0x1b, b'[', b'M', 35, 33, 33]);
    }
}
