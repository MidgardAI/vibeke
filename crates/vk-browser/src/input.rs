//! Logical key events (`vk_proto::input::KeyEvent`, decoded from the kitty keyboard protocol)
//! → CDP `Input.dispatchKeyEvent` / `Input.insertText` (spec 06 B3.2 "Input").
//!
//! Rules:
//! - `code` (physical key) comes from the kitty base-layout key (the US key at that position),
//!   so page shortcuts that look at `event.code` work on any layout; `key` is what the layout
//!   produced.
//! - Associated text is inserted through the `keyDown`'s `text` (Chromium emits the `keypress`
//!   / `input` events itself).
//! - AltGr / macOS Option text (Norwegian `Option+8` → `[`, `Option+2` → `@`) is text, not a
//!   chord: the Alt/Ctrl modifier bits are dropped (spec 03 §7.1 `altgr_mode = "text"`).
//! - Dead-key compositions arrive as one event whose text differs from its key (`e` + `é`);
//!   they are sent as `Input.insertText`. A dead key on its own (no text while the host reports
//!   associated text) becomes a `key: "Dead"` keydown that inserts nothing.
//! - macOS editing shortcuts (Cmd+A/C/X/V/Z) carry Chromium `commands`, since headless
//!   Chromium does not map them by itself.

use serde_json::{Value, json};
use vk_proto::input::{Key, KeyEvent, KeyKind, Mods, NamedKey};

/// CDP modifier bits.
pub const CDP_ALT: u32 = 1;
pub const CDP_CTRL: u32 = 2;
pub const CDP_META: u32 = 4;
pub const CDP_SHIFT: u32 = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyParams {
    /// `keyDown` (with text), `rawKeyDown` (no text) or `keyUp`.
    pub kind: &'static str,
    pub key: String,
    pub code: String,
    pub text: Option<String>,
    pub windows_virtual_key_code: u32,
    pub modifiers: u32,
    pub auto_repeat: bool,
    /// 0 standard, 1 left, 2 right, 3 numpad.
    pub location: u32,
    /// Chromium editing commands (`selectAll`, `copy`, …).
    pub commands: Vec<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CdpInput {
    Key(KeyParams),
    InsertText(String),
    /// `Input.dispatchMouseEvent` params as given (mouse and wheel; the watch view forwards a
    /// taken-over pane's mouse this way).
    Mouse(Value),
}

impl CdpInput {
    /// The CDP method and params.
    pub fn to_command(&self) -> (&'static str, Value) {
        match self {
            CdpInput::InsertText(t) => ("Input.insertText", json!({ "text": t })),
            CdpInput::Mouse(p) => ("Input.dispatchMouseEvent", p.clone()),
            CdpInput::Key(k) => {
                let mut p = json!({
                    "type": k.kind,
                    "key": k.key,
                    "code": k.code,
                    "windowsVirtualKeyCode": k.windows_virtual_key_code,
                    "modifiers": k.modifiers,
                    "autoRepeat": k.auto_repeat,
                    "location": k.location,
                });
                if let Some(t) = &k.text {
                    p["text"] = json!(t);
                    p["unmodifiedText"] = json!(t);
                }
                if !k.commands.is_empty() {
                    p["commands"] = json!(k.commands);
                }
                ("Input.dispatchKeyEvent", p)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MapOptions {
    /// The host reports associated text (kitty flag 16). Then "printable key, no text" means a
    /// dead key; without it, text is derived from the key.
    pub host_reports_text: bool,
    /// Add Chromium editing `commands` for Cmd shortcuts (macOS semantics).
    pub mac_commands: bool,
}

impl Default for MapOptions {
    fn default() -> Self {
        MapOptions {
            host_reports_text: true,
            mac_commands: cfg!(target_os = "macos"),
        }
    }
}

pub fn cdp_modifiers(m: Mods) -> u32 {
    let mut v = 0;
    if m.alt() {
        v |= CDP_ALT;
    }
    if m.ctrl() {
        v |= CDP_CTRL;
    }
    if m.sup() || m.contains(Mods::META) {
        v |= CDP_META;
    }
    if m.shift() {
        v |= CDP_SHIFT;
    }
    v
}

/// DOM `code` and Windows virtual key code for a US-layout character (shifted forms included).
pub fn us_code(c: char) -> Option<(String, u32)> {
    let lower = c.to_ascii_lowercase();
    if lower.is_ascii_lowercase() {
        let up = lower.to_ascii_uppercase();
        return Some((format!("Key{up}"), up as u32));
    }
    let digit = match c {
        '0'..='9' => Some(c),
        '!' => Some('1'),
        '@' => Some('2'),
        '#' => Some('3'),
        '$' => Some('4'),
        '%' => Some('5'),
        '^' => Some('6'),
        '&' => Some('7'),
        '*' => Some('8'),
        '(' => Some('9'),
        ')' => Some('0'),
        _ => None,
    };
    if let Some(d) = digit {
        return Some((format!("Digit{d}"), d as u32));
    }
    let (code, vk) = match c {
        ' ' => ("Space", 0x20),
        ';' | ':' => ("Semicolon", 0xBA),
        '=' | '+' => ("Equal", 0xBB),
        ',' | '<' => ("Comma", 0xBC),
        '-' | '_' => ("Minus", 0xBD),
        '.' | '>' => ("Period", 0xBE),
        '/' | '?' => ("Slash", 0xBF),
        '`' | '~' => ("Backquote", 0xC0),
        '[' | '{' => ("BracketLeft", 0xDB),
        '\\' | '|' => ("Backslash", 0xDC),
        ']' | '}' => ("BracketRight", 0xDD),
        '\'' | '"' => ("Quote", 0xDE),
        _ => return None,
    };
    Some((code.to_owned(), vk))
}

/// Physical position of Norwegian-layout letters when the host gives no base-layout key.
fn nordic_fallback(c: char) -> Option<char> {
    match c.to_lowercase().next()? {
        'ø' | 'ö' => Some(';'),
        'æ' | 'ä' => Some('\''),
        'å' | 'ü' => Some('['),
        _ => None,
    }
}

struct Named {
    key: &'static str,
    code: &'static str,
    vk: u32,
    text: Option<&'static str>,
    location: u32,
}

fn named(n: NamedKey) -> Named {
    let (key, code, vk, text, location): (&'static str, &'static str, u32, Option<&str>, u32) =
        match n {
            NamedKey::Enter => ("Enter", "Enter", 13, Some("\r"), 0),
            NamedKey::Tab => ("Tab", "Tab", 9, None, 0),
            NamedKey::Backspace => ("Backspace", "Backspace", 8, None, 0),
            NamedKey::Escape => ("Escape", "Escape", 27, None, 0),
            NamedKey::Space => (" ", "Space", 32, Some(" "), 0),
            NamedKey::Up => ("ArrowUp", "ArrowUp", 38, None, 0),
            NamedKey::Down => ("ArrowDown", "ArrowDown", 40, None, 0),
            NamedKey::Left => ("ArrowLeft", "ArrowLeft", 37, None, 0),
            NamedKey::Right => ("ArrowRight", "ArrowRight", 39, None, 0),
            NamedKey::Home => ("Home", "Home", 36, None, 0),
            NamedKey::End => ("End", "End", 35, None, 0),
            NamedKey::PageUp => ("PageUp", "PageUp", 33, None, 0),
            NamedKey::PageDown => ("PageDown", "PageDown", 34, None, 0),
            NamedKey::Insert => ("Insert", "Insert", 45, None, 0),
            NamedKey::Delete => ("Delete", "Delete", 46, None, 0),
            NamedKey::F(_) => ("", "", 0, None, 0), // handled below
            NamedKey::CapsLock => ("CapsLock", "CapsLock", 20, None, 0),
            NamedKey::ScrollLock => ("ScrollLock", "ScrollLock", 145, None, 0),
            NamedKey::NumLock => ("NumLock", "NumLock", 144, None, 0),
            NamedKey::PrintScreen => ("PrintScreen", "PrintScreen", 44, None, 0),
            NamedKey::Pause => ("Pause", "Pause", 19, None, 0),
            NamedKey::Menu => ("ContextMenu", "ContextMenu", 93, None, 0),
            NamedKey::LeftShift => ("Shift", "ShiftLeft", 16, None, 1),
            NamedKey::RightShift => ("Shift", "ShiftRight", 16, None, 2),
            NamedKey::LeftControl => ("Control", "ControlLeft", 17, None, 1),
            NamedKey::RightControl => ("Control", "ControlRight", 17, None, 2),
            NamedKey::LeftAlt => ("Alt", "AltLeft", 18, None, 1),
            NamedKey::RightAlt => ("Alt", "AltRight", 18, None, 2),
            NamedKey::LeftSuper => ("Meta", "MetaLeft", 91, None, 1),
            NamedKey::RightSuper => ("Meta", "MetaRight", 92, None, 2),
        };
    Named {
        key,
        code,
        vk,
        text,
        location,
    }
}

fn mac_command(c: char, mods: Mods) -> Option<&'static str> {
    if !mods.sup() || mods.ctrl() || mods.alt() {
        return None;
    }
    Some(match (c.to_ascii_lowercase(), mods.shift()) {
        ('a', false) => "selectAll",
        ('c', false) => "copy",
        ('x', false) => "cut",
        ('v', false) => "paste",
        ('z', false) => "undo",
        ('z', true) => "redo",
        _ => return None,
    })
}

fn kind_for(ev: &KeyEvent, has_text: bool) -> &'static str {
    match ev.kind {
        KeyKind::Release => "keyUp",
        _ if has_text => "keyDown",
        _ => "rawKeyDown",
    }
}

/// Map one key event to CDP input commands.
pub fn map_key(ev: &KeyEvent, opts: MapOptions) -> Vec<CdpInput> {
    let auto_repeat = ev.kind == KeyKind::Repeat;
    let release = ev.kind == KeyKind::Release;
    match ev.key {
        Key::Named(NamedKey::F(n)) => {
            let text = None;
            vec![CdpInput::Key(KeyParams {
                kind: kind_for(ev, false),
                key: format!("F{n}"),
                code: format!("F{n}"),
                text,
                windows_virtual_key_code: 111 + n as u32,
                modifiers: cdp_modifiers(ev.mods),
                auto_repeat,
                location: 0,
                commands: vec![],
            })]
        }
        Key::Named(n) => {
            let nk = named(n);
            // Enter/Space type text only without command modifiers.
            let text = nk
                .text
                .filter(|_| !release && !ev.mods.ctrl() && !ev.mods.sup() && !ev.mods.alt())
                .map(str::to_owned);
            vec![CdpInput::Key(KeyParams {
                kind: kind_for(ev, text.is_some()),
                key: nk.key.to_owned(),
                code: nk.code.to_owned(),
                windows_virtual_key_code: nk.vk,
                text,
                modifiers: cdp_modifiers(ev.mods),
                auto_repeat,
                location: nk.location,
                commands: vec![],
            })]
        }
        Key::Char(c) => map_char(ev, c, opts, auto_repeat, release),
    }
}

fn map_char(
    ev: &KeyEvent,
    c: char,
    opts: MapOptions,
    auto_repeat: bool,
    release: bool,
) -> Vec<CdpInput> {
    let mods = ev.mods;
    let base = ev
        .base_layout_key
        .or_else(|| c.is_ascii().then_some(c))
        .or_else(|| nordic_fallback(c));
    let (code, vk) = base.and_then(us_code).unwrap_or_default();
    // What an unmodified (or shift-only) press of this key would type.
    let plain: String = if mods.shift() {
        ev.shifted
            .map(String::from)
            .unwrap_or_else(|| c.to_uppercase().collect())
    } else {
        c.to_string()
    };
    let command_mods = mods.ctrl() || mods.sup();
    let text: Option<String> = match &ev.text {
        Some(t) if !t.is_empty() && !t.chars().any(char::is_control) => Some(t.clone()),
        Some(_) => None,
        None if !opts.host_reports_text && !command_mods && !mods.alt() => Some(plain.clone()),
        None => None,
    };

    if release {
        let key = text.clone().unwrap_or_else(|| plain.clone());
        return vec![CdpInput::Key(KeyParams {
            kind: "keyUp",
            key,
            code,
            text: None,
            windows_virtual_key_code: vk,
            modifiers: cdp_modifiers(mods),
            auto_repeat: false,
            location: 0,
            commands: vec![],
        })];
    }

    match text {
        // Text produced with Alt/AltGr (Option on macOS): text input, not a chord.
        Some(t) if mods.alt() && !mods.sup() && t != plain => {
            vec![CdpInput::Key(KeyParams {
                kind: "keyDown",
                key: t.clone(),
                code,
                text: Some(t),
                windows_virtual_key_code: vk,
                modifiers: cdp_modifiers(mods.without(Mods::ALT).without(Mods::CTRL)),
                auto_repeat,
                location: 0,
                commands: vec![],
            })]
        }
        // Dead-key composition (or IME commit) delivered on the final key.
        Some(t) if !command_mods && t != plain && t != c.to_string() => {
            vec![CdpInput::InsertText(t)]
        }
        Some(t) if !command_mods => vec![CdpInput::Key(KeyParams {
            kind: "keyDown",
            key: t.clone(),
            code,
            text: Some(t),
            windows_virtual_key_code: vk,
            modifiers: cdp_modifiers(mods),
            auto_repeat,
            location: 0,
            commands: vec![],
        })],
        // Printable key, no text, host reports text, no modifiers: a dead key.
        None if opts.host_reports_text && !command_mods && !mods.alt() => {
            vec![CdpInput::Key(KeyParams {
                kind: "rawKeyDown",
                key: "Dead".into(),
                code,
                text: None,
                windows_virtual_key_code: vk,
                modifiers: cdp_modifiers(mods),
                auto_repeat,
                location: 0,
                commands: vec![],
            })]
        }
        // Chords: Ctrl/Cmd/Alt + key. `key` is the unmodified character.
        _ => {
            let commands = if opts.mac_commands {
                mac_command(base.unwrap_or(c), mods).into_iter().collect()
            } else {
                vec![]
            };
            vec![CdpInput::Key(KeyParams {
                kind: "rawKeyDown",
                key: plain,
                code,
                text: None,
                windows_virtual_key_code: vk,
                modifiers: cdp_modifiers(mods),
                auto_repeat,
                location: 0,
                commands,
            })]
        }
    }
}

/// For hosts that don't report releases: the press commands followed by a matching `keyUp`.
pub fn map_key_with_release(ev: &KeyEvent, opts: MapOptions) -> Vec<CdpInput> {
    let mut v = map_key(ev, opts);
    let mut up = ev.clone();
    up.kind = KeyKind::Release;
    if v.iter().all(|c| matches!(c, CdpInput::Key(_))) {
        v.extend(map_key(&up, opts));
    }
    v
}

/// Bracketed paste / IME commit / dropped text.
pub fn map_paste(text: &str) -> CdpInput {
    CdpInput::InsertText(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> MapOptions {
        MapOptions {
            host_reports_text: true,
            mac_commands: true,
        }
    }

    fn ev(c: char, mods: Mods, text: Option<&str>, base: Option<char>) -> KeyEvent {
        let mut e = KeyEvent::new(Key::Char(c), mods);
        e.text = text.map(str::to_owned);
        e.base_layout_key = base;
        e
    }

    fn one_key(v: Vec<CdpInput>) -> KeyParams {
        assert_eq!(v.len(), 1, "{v:?}");
        match v.into_iter().next().unwrap() {
            CdpInput::Key(k) => k,
            other => panic!("expected key, got {other:?}"),
        }
    }

    #[test]
    fn us_letters_digits_punctuation() {
        let k = one_key(map_key(&ev('a', Mods::empty(), Some("a"), None), opts()));
        assert_eq!(
            (k.kind, k.key.as_str(), k.code.as_str()),
            ("keyDown", "a", "KeyA")
        );
        assert_eq!(k.text.as_deref(), Some("a"));
        assert_eq!(k.windows_virtual_key_code, 65);
        assert_eq!(k.modifiers, 0);

        let mut e = ev('a', Mods::SHIFT, Some("A"), None);
        e.shifted = Some('A');
        let k = one_key(map_key(&e, opts()));
        assert_eq!((k.key.as_str(), k.modifiers), ("A", CDP_SHIFT));

        let mut e = ev('1', Mods::SHIFT, Some("!"), None);
        e.shifted = Some('!');
        let k = one_key(map_key(&e, opts()));
        assert_eq!(
            (k.key.as_str(), k.code.as_str(), k.windows_virtual_key_code),
            ("!", "Digit1", 0x31)
        );

        let k = one_key(map_key(&ev('/', Mods::empty(), Some("/"), None), opts()));
        assert_eq!(
            (k.code.as_str(), k.windows_virtual_key_code),
            ("Slash", 0xBF)
        );
    }

    #[test]
    fn norwegian_letters_use_base_layout_code() {
        for (c, base, code) in [
            ('ø', ';', "Semicolon"),
            ('æ', '\'', "Quote"),
            ('å', '[', "BracketLeft"),
        ] {
            let k = one_key(map_key(
                &ev(c, Mods::empty(), Some(&c.to_string()), Some(base)),
                opts(),
            ));
            assert_eq!(k.key, c.to_string());
            assert_eq!(k.text, Some(c.to_string()));
            assert_eq!(k.code, code);
            // Without a base-layout key the nordic fallback still finds the position.
            let k = one_key(map_key(
                &ev(c, Mods::empty(), Some(&c.to_string()), None),
                opts(),
            ));
            assert_eq!(k.code, code);
        }
        let mut e = ev('ø', Mods::SHIFT, Some("Ø"), Some(';'));
        e.shifted = Some('Ø');
        let k = one_key(map_key(&e, opts()));
        assert_eq!((k.key.as_str(), k.modifiers), ("Ø", CDP_SHIFT));
        // Norwegian '+' sits on the US '-' key.
        let k = one_key(map_key(
            &ev('+', Mods::empty(), Some("+"), Some('-')),
            opts(),
        ));
        assert_eq!((k.key.as_str(), k.code.as_str()), ("+", "Minus"));
    }

    #[test]
    fn altgr_and_option_text_is_text() {
        // macOS Norwegian: Option+8 → '[', Option+2 → '@', Option+Shift+7 → '\'.
        for (c, mods, text, base) in [
            ('8', Mods::ALT, "[", '8'),
            ('2', Mods::ALT, "@", '2'),
            ('7', Mods::ALT | Mods::SHIFT, "\\", '7'),
            // Windows/Linux AltGr reports as Ctrl+Alt.
            ('e', Mods::ALT | Mods::CTRL, "€", 'e'),
        ] {
            let k = one_key(map_key(&ev(c, mods, Some(text), Some(base)), opts()));
            assert_eq!(k.kind, "keyDown");
            assert_eq!(k.key, text);
            assert_eq!(k.text.as_deref(), Some(text));
            assert_eq!(
                k.modifiers & (CDP_ALT | CDP_CTRL),
                0,
                "{text}: chord bits dropped"
            );
        }
        // Alt without text stays a chord.
        let k = one_key(map_key(&ev('f', Mods::ALT, None, None), opts()));
        assert_eq!(
            (k.kind, k.key.as_str(), k.modifiers),
            ("rawKeyDown", "f", CDP_ALT)
        );
    }

    #[test]
    fn dead_keys_and_composition() {
        // Dead acute alone (Norwegian key left of backspace): no text → "Dead".
        let k = one_key(map_key(&ev('´', Mods::empty(), None, Some('=')), opts()));
        assert_eq!(
            (k.kind, k.key.as_str(), k.code.as_str()),
            ("rawKeyDown", "Dead", "Equal")
        );
        // Then 'e' arrives with composed text "é" → insertText.
        let v = map_key(&ev('e', Mods::empty(), Some("é"), Some('e')), opts());
        assert_eq!(v, vec![CdpInput::InsertText("é".into())]);
        // Dead key followed by a non-composing key types both characters.
        let v = map_key(&ev('q', Mods::empty(), Some("´q"), Some('q')), opts());
        assert_eq!(v, vec![CdpInput::InsertText("´q".into())]);
        // Legacy host (no associated text): plain char is typed, never "Dead".
        let o = MapOptions {
            host_reports_text: false,
            mac_commands: false,
        };
        let k = one_key(map_key(&ev('x', Mods::empty(), None, None), o));
        assert_eq!(k.text.as_deref(), Some("x"));
    }

    #[test]
    fn chords_and_mac_commands() {
        let k = one_key(map_key(&ev('c', Mods::CTRL, None, None), opts()));
        assert_eq!(
            (k.kind, k.key.as_str(), k.code.as_str(), k.modifiers),
            ("rawKeyDown", "c", "KeyC", CDP_CTRL)
        );
        assert!(k.commands.is_empty());
        let k = one_key(map_key(&ev('a', Mods::SUPER, None, None), opts()));
        assert_eq!(k.modifiers, CDP_META);
        assert_eq!(k.commands, vec!["selectAll"]);
        let k = one_key(map_key(
            &ev('z', Mods::SUPER | Mods::SHIFT, None, None),
            opts(),
        ));
        assert_eq!(k.commands, vec!["redo"]);
        // Cmd on a Norwegian layout: base key decides the command.
        let k = one_key(map_key(&ev('å', Mods::SUPER, None, Some('[')), opts()));
        assert!(k.commands.is_empty());
        // Ctrl with associated text (some hosts report control chars): no text inserted.
        let k = one_key(map_key(&ev('c', Mods::CTRL, Some("\u{3}"), None), opts()));
        assert_eq!(k.text, None);
    }

    #[test]
    fn named_keys() {
        let k = one_key(map_key(&KeyEvent::named(NamedKey::Enter), opts()));
        assert_eq!(
            (k.kind, k.key.as_str(), k.windows_virtual_key_code),
            ("keyDown", "Enter", 13)
        );
        assert_eq!(k.text.as_deref(), Some("\r"));
        let k = one_key(map_key(
            &KeyEvent::new(Key::Named(NamedKey::Enter), Mods::SHIFT),
            opts(),
        ));
        assert_eq!(k.modifiers, CDP_SHIFT);
        let k = one_key(map_key(
            &KeyEvent::new(Key::Named(NamedKey::Enter), Mods::CTRL),
            opts(),
        ));
        assert_eq!((k.kind, k.text), ("rawKeyDown", None));
        let k = one_key(map_key(
            &KeyEvent::new(Key::Named(NamedKey::Tab), Mods::SHIFT),
            opts(),
        ));
        assert_eq!(
            (k.kind, k.key.as_str(), k.modifiers),
            ("rawKeyDown", "Tab", CDP_SHIFT)
        );
        let k = one_key(map_key(&KeyEvent::named(NamedKey::F(5)), opts()));
        assert_eq!((k.key.as_str(), k.windows_virtual_key_code), ("F5", 116));
        let k = one_key(map_key(&KeyEvent::named(NamedKey::Up), opts()));
        assert_eq!(k.code, "ArrowUp");
        let k = one_key(map_key(&KeyEvent::named(NamedKey::RightAlt), opts()));
        assert_eq!((k.code.as_str(), k.location), ("AltRight", 2));
        let k = one_key(map_key(&KeyEvent::named(NamedKey::Space), opts()));
        assert_eq!(k.text.as_deref(), Some(" "));
    }

    #[test]
    fn repeat_release_and_paste() {
        let mut e = ev('ø', Mods::empty(), Some("ø"), Some(';'));
        e.kind = KeyKind::Repeat;
        assert!(one_key(map_key(&e, opts())).auto_repeat);
        e.kind = KeyKind::Release;
        let k = one_key(map_key(&e, opts()));
        assert_eq!(
            (k.kind, k.key.as_str(), k.text.as_deref()),
            ("keyUp", "ø", None)
        );
        let v = map_key_with_release(&ev('a', Mods::empty(), Some("a"), None), opts());
        assert_eq!(v.len(), 2);
        let (m, p) = v[1].to_command();
        assert_eq!(m, "Input.dispatchKeyEvent");
        assert_eq!(p["type"], "keyUp");
        // Composition commits have no keyUp pair.
        let v = map_key_with_release(&ev('e', Mods::empty(), Some("é"), Some('e')), opts());
        assert_eq!(v.len(), 1);
        let (m, p) = map_paste("hei på deg").to_command();
        assert_eq!(m, "Input.insertText");
        assert_eq!(p["text"], "hei på deg");
    }

    #[test]
    fn command_json_shape() {
        let v = map_key(&ev('a', Mods::SUPER, None, None), opts());
        let (m, p) = v[0].to_command();
        assert_eq!(m, "Input.dispatchKeyEvent");
        assert_eq!(p["type"], "rawKeyDown");
        assert_eq!(p["code"], "KeyA");
        assert_eq!(p["windowsVirtualKeyCode"], 65);
        assert_eq!(p["modifiers"], CDP_META);
        assert_eq!(p["commands"][0], "selectAll");
        assert!(p.get("text").is_none());
    }
}
