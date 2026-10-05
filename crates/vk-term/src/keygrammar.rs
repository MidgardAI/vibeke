//! Shared key grammar (07 §2.6.1, 08 §10.1): `pane.send_keys` and config keybindings.
//!
//! Normalisation: a letter is always stored as its **lowercase** `Key::Char` plus a
//! `Mods::SHIFT` bit when shifted, so `"shift+p"`, `"P"` and `"alt+P"` all match the
//! same way (`'p'` + SHIFT). The encoder turns that back into `P` when it needs text.
//! `meta`/`opt` parse as `alt`. The `altgr` modifier (meaningful only in config
//! bindings) parses as `ctrl+alt`.

use vk_proto::input::{Key, KeyEvent, Mods, NamedKey};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyGrammarError {
    #[error("empty key")]
    Empty,
    #[error("unknown key name `{0}`")]
    UnknownKey(String),
    #[error("unknown modifier `{0}`")]
    UnknownModifier(String),
    #[error("duplicate modifier `{0}`")]
    DuplicateModifier(String),
    #[error("missing key after modifiers in `{0}`")]
    MissingKey(String),
    #[error("tmux key syntax `{input}` is not supported; use `{hint}`")]
    TmuxSyntax { input: String, hint: String },
    #[error("`prefix` must be followed by a key (`prefix+v`)")]
    BarePrefix,
}

impl KeyGrammarError {
    /// A human hint for the API's `invalid_key` error, if there is one.
    pub fn hint(&self) -> Option<String> {
        match self {
            KeyGrammarError::TmuxSyntax { hint, .. } => Some(format!("use `{hint}`")),
            KeyGrammarError::UnknownKey(_) => Some(
                "named keys: enter tab esc space backspace delete insert home end pageup \
                 pagedown up down left right f1..f24, minus comma period slash ..."
                    .into(),
            ),
            _ => None,
        }
    }
}

const PUNCT: &[(&str, char)] = &[
    ("minus", '-'),
    ("comma", ','),
    ("period", '.'),
    ("slash", '/'),
    ("backslash", '\\'),
    ("semicolon", ';'),
    ("quote", '\''),
    ("backtick", '`'),
    ("lbracket", '['),
    ("rbracket", ']'),
    ("equal", '='),
    ("plus", '+'),
    ("ampersand", '&'),
    ("colon", ':'),
    ("question", '?'),
];

// The first entry for each key is its canonical spelling.
const NAMED: &[(&str, NamedKey)] = &[
    ("enter", NamedKey::Enter),
    ("return", NamedKey::Enter),
    ("tab", NamedKey::Tab),
    ("esc", NamedKey::Escape),
    ("escape", NamedKey::Escape),
    ("space", NamedKey::Space),
    ("backspace", NamedKey::Backspace),
    ("bs", NamedKey::Backspace),
    ("delete", NamedKey::Delete),
    ("del", NamedKey::Delete),
    ("insert", NamedKey::Insert),
    ("home", NamedKey::Home),
    ("end", NamedKey::End),
    ("pageup", NamedKey::PageUp),
    ("pgup", NamedKey::PageUp),
    ("pagedown", NamedKey::PageDown),
    ("pgdn", NamedKey::PageDown),
    ("up", NamedKey::Up),
    ("down", NamedKey::Down),
    ("left", NamedKey::Left),
    ("right", NamedKey::Right),
    ("capslock", NamedKey::CapsLock),
    ("scrolllock", NamedKey::ScrollLock),
    ("numlock", NamedKey::NumLock),
    ("printscreen", NamedKey::PrintScreen),
    ("pause", NamedKey::Pause),
    ("menu", NamedKey::Menu),
    ("leftshift", NamedKey::LeftShift),
    ("leftctrl", NamedKey::LeftControl),
    ("leftalt", NamedKey::LeftAlt),
    ("leftsuper", NamedKey::LeftSuper),
    ("rightshift", NamedKey::RightShift),
    ("rightctrl", NamedKey::RightControl),
    ("rightalt", NamedKey::RightAlt),
    ("rightsuper", NamedKey::RightSuper),
];

fn named_str(n: NamedKey) -> String {
    if let NamedKey::F(i) = n {
        return format!("f{i}");
    }
    NAMED
        .iter()
        .find(|(_, k)| *k == n)
        .map(|(name, _)| (*name).to_string())
        .unwrap_or_default()
}

fn modifier_bit(name: &str) -> Option<Mods> {
    Some(match name {
        "ctrl" | "control" => Mods::CTRL,
        "shift" => Mods::SHIFT,
        "alt" | "meta" | "opt" | "option" => Mods::ALT,
        "cmd" | "super" | "command" | "win" => Mods::SUPER,
        "hyper" => Mods::HYPER,
        "altgr" => Mods::CTRL | Mods::ALT,
        _ => return None,
    })
}

fn parse_key_token(tok: &str) -> Result<Key, KeyGrammarError> {
    let mut chars = tok.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Ok(if c == ' ' {
            Key::Named(NamedKey::Space)
        } else {
            Key::Char(c)
        });
    }
    let lower = tok.to_lowercase();
    if let Some((_, k)) = NAMED.iter().find(|(n, _)| *n == lower) {
        return Ok(Key::Named(*k));
    }
    if let Some((_, c)) = PUNCT.iter().find(|(n, _)| *n == lower) {
        return Ok(Key::Char(*c));
    }
    if let Some(num) = lower.strip_prefix('f')
        && !num.is_empty()
        && !num.starts_with('0')
        && num.bytes().all(|b| b.is_ascii_digit())
        && let Ok(n) = num.parse::<u8>()
        && (1..=24).contains(&n)
    {
        return Ok(Key::Named(NamedKey::F(n)));
    }
    Err(KeyGrammarError::UnknownKey(tok.to_string()))
}

/// Detect tmux syntax (`C-c`, `M-x`, `C-M-Left`) and suggest the Vibeke spelling.
fn tmux_syntax(s: &str) -> Option<KeyGrammarError> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut parts: Vec<String> = Vec::new();
    while i + 2 < b.len() && b[i + 1] == b'-' && matches!(b[i], b'C' | b'M' | b'S') {
        parts.push(
            match b[i] {
                b'C' => "ctrl",
                b'M' => "alt",
                _ => "shift",
            }
            .to_string(),
        );
        i += 2;
    }
    if parts.is_empty() {
        return None;
    }
    parts.push(s[i..].to_lowercase());
    Some(KeyGrammarError::TmuxSyntax {
        input: s.to_string(),
        hint: parts.join("+"),
    })
}

/// Parse one chord such as `ctrl+c`, `alt+shift+p`, `f12`, `é`, `ctrl++`.
pub fn parse_key(s: &str) -> Result<KeyEvent, KeyGrammarError> {
    if s.is_empty() {
        return Err(KeyGrammarError::Empty);
    }
    if let Some(e) = tmux_syntax(s) {
        return Err(e);
    }
    // Split off the key (last token), tolerating a literal `+` as the key.
    let (mod_part, key_tok): (&str, &str) = if s == "+" {
        ("", "+")
    } else if let Some(head) = s.strip_suffix("++") {
        (head, "+")
    } else if let Some(i) = s.rfind('+') {
        (&s[..i], &s[i + 1..])
    } else {
        ("", s)
    };
    if key_tok.is_empty()
        || (key_tok.chars().count() > 1 && modifier_bit(&key_tok.to_lowercase()).is_some())
    {
        return Err(KeyGrammarError::MissingKey(s.to_string()));
    }
    let mut mods = Mods::empty();
    if !mod_part.is_empty() {
        for m in mod_part.split('+') {
            let Some(bit) = modifier_bit(&m.to_lowercase()) else {
                return Err(KeyGrammarError::UnknownModifier(m.to_string()));
            };
            if mods.0 & bit.0 != 0 {
                return Err(KeyGrammarError::DuplicateModifier(m.to_string()));
            }
            mods = mods | bit;
        }
    }
    let mut key = parse_key_token(key_tok)?;
    if let Key::Char(c) = key
        && c.is_uppercase()
    {
        let mut l = c.to_lowercase();
        if let (Some(lc), None) = (l.next(), l.next()) {
            key = Key::Char(lc);
            mods = mods | Mods::SHIFT;
        }
    }
    Ok(KeyEvent::new(key, mods))
}

fn char_name(c: char) -> String {
    if c == ' ' {
        return "space".into();
    }
    PUNCT
        .iter()
        .find(|(_, p)| *p == c)
        .map(|(n, _)| (*n).to_string())
        .unwrap_or_else(|| c.to_string())
}

/// Canonical text form; `parse_key(&format_key(e))` yields the same key and modifiers
/// (for events produced by the parser; `meta` is spelled `alt`).
/// Modifier order: ctrl, alt, shift, super, hyper.
pub fn format_key(ev: &KeyEvent) -> String {
    let mut mods = ev.mods;
    if mods.contains(Mods::META) {
        mods = mods.without(Mods::META) | Mods::ALT;
    }
    let key = match ev.key {
        Key::Char(c) => {
            let mut l = c.to_lowercase();
            match (c.is_uppercase(), l.next(), l.next()) {
                (true, Some(lc), None) => {
                    mods = mods | Mods::SHIFT;
                    char_name(lc)
                }
                _ => char_name(c),
            }
        }
        Key::Named(n) => named_str(n),
    };
    let mut out = String::new();
    for (bit, name) in [
        (Mods::CTRL, "ctrl"),
        (Mods::ALT, "alt"),
        (Mods::SHIFT, "shift"),
        (Mods::SUPER, "super"),
        (Mods::HYPER, "hyper"),
    ] {
        if mods.contains(bit) {
            out.push_str(name);
            out.push('+');
        }
    }
    out.push_str(&key);
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// Requires the prefix key first.
    pub prefix: bool,
    /// One chord (direct or `prefix+x`) or a sequence (`prefix+g w`). Empty = unbound.
    pub chords: Vec<KeyEvent>,
}

/// Parse `"prefix+v"`, `"prefix+g w"`, `"ctrl+b"`, `""` (unbound: empty `chords`).
pub fn parse_binding(s: &str) -> Result<Binding, KeyGrammarError> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(Binding {
            prefix: false,
            chords: Vec::new(),
        });
    }
    if s.eq_ignore_ascii_case("prefix") {
        return Err(KeyGrammarError::BarePrefix);
    }
    let (prefix, rest) = match s.get(..7) {
        Some(p) if p.eq_ignore_ascii_case("prefix+") => (true, &s[7..]),
        _ => (false, s),
    };
    let chords = rest
        .split_whitespace()
        .map(parse_key)
        .collect::<Result<Vec<_>, _>>()?;
    if chords.is_empty() {
        return Err(KeyGrammarError::Empty);
    }
    Ok(Binding { prefix, chords })
}

/// Expand an indexed binding like `"prefix+1..9"` into `["prefix+1", ..., "prefix+9"]`.
/// Returns `None` if `s` has no range. Ranges are single ASCII digits or lowercase letters.
pub fn expand_range(s: &str) -> Option<Vec<String>> {
    let pos = s.rfind("..")?;
    let head = &s[..pos];
    let tail = &s[pos + 2..];
    let a = head.chars().last()?;
    let mut tc = tail.chars();
    let b = tc.next()?;
    if tc.next().is_some() {
        return None;
    }
    let ok = (a.is_ascii_digit() && b.is_ascii_digit())
        || (a.is_ascii_lowercase() && b.is_ascii_lowercase());
    if !ok || a > b {
        return None;
    }
    let base = &head[..head.len() - a.len_utf8()];
    // The range char must start a chord (after `+`, a space, or the string start).
    if !(base.is_empty() || base.ends_with('+') || base.ends_with(' ')) {
        return None;
    }
    Some((a..=b).map(|c| format!("{base}{c}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> KeyEvent {
        parse_key(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn basics() {
        assert_eq!(p("enter"), KeyEvent::named(NamedKey::Enter));
        assert_eq!(p("Enter"), KeyEvent::named(NamedKey::Enter));
        assert_eq!(p("ESC"), KeyEvent::named(NamedKey::Escape));
        assert_eq!(p("escape"), KeyEvent::named(NamedKey::Escape));
        assert_eq!(p("bs"), KeyEvent::named(NamedKey::Backspace));
        assert_eq!(p("del"), KeyEvent::named(NamedKey::Delete));
        assert_eq!(p("pgup"), KeyEvent::named(NamedKey::PageUp));
        assert_eq!(p("PgDn"), KeyEvent::named(NamedKey::PageDown));
        assert_eq!(p("space"), KeyEvent::named(NamedKey::Space));
        assert_eq!(p(" "), KeyEvent::named(NamedKey::Space));
        assert_eq!(p("f12"), KeyEvent::named(NamedKey::F(12)));
        assert_eq!(p("F24"), KeyEvent::named(NamedKey::F(24)));
        assert_eq!(p("y"), KeyEvent::ch('y'));
        assert_eq!(p("é"), KeyEvent::ch('é'));
        assert_eq!(p("1"), KeyEvent::ch('1'));
        assert_eq!(p("["), KeyEvent::ch('['));
        assert_eq!(p("minus"), KeyEvent::ch('-'));
        assert_eq!(p("plus"), KeyEvent::ch('+'));
        assert_eq!(p("+"), KeyEvent::ch('+'));
        assert_eq!(p("question"), KeyEvent::ch('?'));
    }

    #[test]
    fn modifiers() {
        assert_eq!(p("ctrl+c"), KeyEvent::new(Key::Char('c'), Mods::CTRL));
        assert_eq!(p("CTRL+C").mods, Mods::CTRL | Mods::SHIFT);
        assert_eq!(
            p("alt+shift+p"),
            KeyEvent::new(Key::Char('p'), Mods::ALT | Mods::SHIFT)
        );
        assert_eq!(p("shift+p"), p("P"));
        assert_eq!(p("cmd+k").mods, Mods::SUPER);
        assert_eq!(p("super+x").mods, Mods::SUPER);
        assert_eq!(p("opt+x").mods, Mods::ALT);
        assert_eq!(p("meta+x").mods, Mods::ALT);
        assert_eq!(p("hyper+x").mods, Mods::HYPER);
        assert_eq!(p("altgr+q").mods, Mods::CTRL | Mods::ALT);
        assert_eq!(p("shift+alt+p"), p("alt+shift+p"));
        assert_eq!(p("ctrl++"), KeyEvent::new(Key::Char('+'), Mods::CTRL));
        assert_eq!(p("ctrl+plus"), p("ctrl++"));
        assert_eq!(p("shift+tab").mods, Mods::SHIFT);
        assert_eq!(p("ctrl+pageup").key, Key::Named(NamedKey::PageUp));
    }

    #[test]
    fn errors() {
        assert_eq!(parse_key(""), Err(KeyGrammarError::Empty));
        assert!(matches!(
            parse_key("C-c"),
            Err(KeyGrammarError::TmuxSyntax { ref hint, .. }) if hint == "ctrl+c"
        ));
        assert!(matches!(
            parse_key("M-x"),
            Err(KeyGrammarError::TmuxSyntax { ref hint, .. }) if hint == "alt+x"
        ));
        assert!(matches!(
            parse_key("C-M-Left"),
            Err(KeyGrammarError::TmuxSyntax { ref hint, .. }) if hint == "ctrl+alt+left"
        ));
        assert!(parse_key("C-c").unwrap_err().hint().is_some());
        assert!(matches!(
            parse_key("f25"),
            Err(KeyGrammarError::UnknownKey(_))
        ));
        assert!(matches!(
            parse_key("f0"),
            Err(KeyGrammarError::UnknownKey(_))
        ));
        assert!(matches!(
            parse_key("f01"),
            Err(KeyGrammarError::UnknownKey(_))
        ));
        assert!(matches!(
            parse_key("foo"),
            Err(KeyGrammarError::UnknownKey(_))
        ));
        assert!(matches!(
            parse_key("ab"),
            Err(KeyGrammarError::UnknownKey(_))
        ));
        assert!(matches!(
            parse_key("ctrl+"),
            Err(KeyGrammarError::MissingKey(_))
        ));
        assert!(matches!(
            parse_key("ctrl"),
            Err(KeyGrammarError::MissingKey(_))
        ));
        assert!(matches!(
            parse_key("ctrl+shift"),
            Err(KeyGrammarError::MissingKey(_))
        ));
        assert!(matches!(
            parse_key("ctrl+ctrl+c"),
            Err(KeyGrammarError::DuplicateModifier(_))
        ));
        assert!(matches!(
            parse_key("hyperx+c"),
            Err(KeyGrammarError::UnknownModifier(_))
        ));
        assert!(matches!(
            parse_key("enter+c"),
            Err(KeyGrammarError::UnknownModifier(_))
        ));
    }

    #[test]
    fn round_trip() {
        let mut cases: Vec<KeyEvent> = Vec::new();
        for n in [
            NamedKey::Enter,
            NamedKey::Tab,
            NamedKey::Backspace,
            NamedKey::Escape,
            NamedKey::Space,
            NamedKey::Up,
            NamedKey::Down,
            NamedKey::Left,
            NamedKey::Right,
            NamedKey::Home,
            NamedKey::End,
            NamedKey::PageUp,
            NamedKey::PageDown,
            NamedKey::Insert,
            NamedKey::Delete,
            NamedKey::CapsLock,
            NamedKey::Menu,
            NamedKey::LeftShift,
            NamedKey::RightSuper,
        ] {
            cases.push(KeyEvent::named(n));
            cases.push(KeyEvent::new(Key::Named(n), Mods::CTRL | Mods::SHIFT));
        }
        for i in 1..=24 {
            cases.push(KeyEvent::new(Key::Named(NamedKey::F(i)), Mods::ALT));
        }
        for c in "az09é-,./\\;'`[]=+&:?!#".chars() {
            cases.push(KeyEvent::ch(c));
            cases.push(KeyEvent::new(
                Key::Char(c),
                Mods::CTRL | Mods::ALT | Mods::SHIFT | Mods::SUPER | Mods::HYPER,
            ));
        }
        for e in cases {
            let s = format_key(&e);
            let back = parse_key(&s).unwrap_or_else(|err| panic!("{s}: {err}"));
            assert_eq!(back, e, "via {s}");
            assert_eq!(format_key(&back), s);
        }
    }

    #[test]
    fn format_canonical() {
        assert_eq!(format_key(&p("shift+alt+ctrl+x")), "ctrl+alt+shift+x");
        assert_eq!(format_key(&p("pgup")), "pageup");
        assert_eq!(format_key(&p("esc")), "esc");
        assert_eq!(format_key(&p("ctrl+minus")), "ctrl+minus");
        assert_eq!(format_key(&KeyEvent::ch('P')), "shift+p");
        assert_eq!(format_key(&p("cmd+k")), "super+k");
        assert_eq!(format_key(&p("return")), "enter");
        assert_eq!(format_key(&p("bs")), "backspace");
    }

    #[test]
    fn binding() {
        let b = parse_binding("prefix+v").unwrap();
        assert!(b.prefix);
        assert_eq!(b.chords, vec![p("v")]);
        let b = parse_binding("prefix+shift+t").unwrap();
        assert_eq!(b.chords, vec![p("shift+t")]);
        let b = parse_binding("prefix+g w").unwrap();
        assert!(b.prefix);
        assert_eq!(b.chords, vec![p("g"), p("w")]);
        let b = parse_binding("ctrl+b").unwrap();
        assert!(!b.prefix);
        assert_eq!(b.chords, vec![p("ctrl+b")]);
        let b = parse_binding("").unwrap();
        assert!(!b.prefix && b.chords.is_empty());
        let b = parse_binding("PREFIX+minus").unwrap();
        assert!(b.prefix);
        assert_eq!(b.chords, vec![p("-")]);
        assert_eq!(parse_binding("prefix"), Err(KeyGrammarError::BarePrefix));
        assert!(parse_binding("prefix+").is_err());
        assert!(parse_binding("prefix+g C-w").is_err());
        assert!(parse_binding("prefix+nope").is_err());
        assert!(parse_binding("ctrl+prefix").is_err());
    }

    #[test]
    fn ranges() {
        assert_eq!(
            expand_range("prefix+1..9").unwrap(),
            (1..=9).map(|i| format!("prefix+{i}")).collect::<Vec<_>>()
        );
        assert_eq!(
            expand_range("alt+a..c").unwrap(),
            vec!["alt+a", "alt+b", "alt+c"]
        );
        assert_eq!(expand_range("1..3").unwrap(), vec!["1", "2", "3"]);
        assert_eq!(expand_range("prefix+v"), None);
        assert_eq!(expand_range("prefix+9..1"), None);
        assert_eq!(expand_range("prefix+1..10"), None);
        assert_eq!(expand_range("prefix+a..9"), None);
        assert_eq!(expand_range("prefix+period"), None);
    }
}
