//! Minimal internal grammar check for binding strings (08 §10.1).
//!
//! This is deliberately small: it validates and canonicalises binding strings so the
//! config can report syntax errors and conflicts. The full key matcher lives elsewhere.

const MODIFIERS: &[&str] = &["ctrl", "alt", "shift", "super", "hyper", "meta", "altgr"];

const NAMED_KEYS: &[&str] = &[
    "enter",
    "tab",
    "esc",
    "backspace",
    "space",
    "up",
    "down",
    "left",
    "right",
    "home",
    "end",
    "pageup",
    "pagedown",
    "insert",
    "delete",
    "minus",
    "comma",
    "period",
    "slash",
    "backslash",
    "semicolon",
    "quote",
    "backtick",
    "lbracket",
    "rbracket",
    "equal",
    "plus",
    "ampersand",
    "colon",
    "question",
    "at",
    "hash",
    "dollar",
    "percent",
    "caret",
    "asterisk",
    "lparen",
    "rparen",
    "underscore",
    "pipe",
    "tilde",
    "less",
    "greater",
    "exclamation",
    "lbrace",
    "rbrace",
    "dquote",
    "apostrophe",
];

/// A parsed binding: optional prefix, then one or more chords. Chords are canonical strings
/// such as `ctrl+shift+x`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub prefix: bool,
    pub chords: Vec<String>,
    /// Inclusive digit range on the final chord (`1..9`), if any.
    pub range: Option<(u8, u8)>,
}

impl Binding {
    /// Canonical strings for the binding; a range expands to one entry per digit.
    pub fn expand(&self) -> Vec<String> {
        let render = |last: Option<String>| {
            let mut parts: Vec<String> = self.chords.clone();
            if let Some(l) = last {
                *parts.last_mut().unwrap() = l;
            }
            let body = parts.join(" ");
            if self.prefix {
                format!("prefix+{body}")
            } else {
                body
            }
        };
        match self.range {
            None => vec![render(None)],
            Some((a, b)) => {
                let last = self.chords.last().unwrap();
                let mods = last.rsplit_once("..").map(|(m, _)| m).unwrap_or("");
                // `last` is stored as "<mods>+<a>..<b>"; rebuild per digit.
                let mod_prefix = mods
                    .trim_end_matches(|c: char| c.is_ascii_digit())
                    .to_string();
                (a..=b)
                    .map(|d| render(Some(format!("{mod_prefix}{d}"))))
                    .collect()
            }
        }
    }

    /// True for a direct (no `prefix+`) binding.
    pub fn is_direct(&self) -> bool {
        !self.prefix
    }

    /// True when any chord uses `super` (the canonical form of `cmd`).
    pub fn uses_super(&self) -> bool {
        self.chords
            .iter()
            .any(|c| c.split('+').any(|p| p == "super"))
    }
}

fn is_valid_key(k: &str) -> bool {
    if NAMED_KEYS.contains(&k) {
        return true;
    }
    if let Some(n) = k.strip_prefix('f')
        && let Ok(n) = n.parse::<u8>()
    {
        return (1..=24).contains(&n);
    }
    let mut it = k.chars();
    match (it.next(), it.next()) {
        (Some(c), None) => !c.is_whitespace() && !c.is_control(),
        _ => false,
    }
}

fn parse_chord(chord: &str, allow_range: bool) -> Result<(String, Option<(u8, u8)>), String> {
    if chord.is_empty() {
        return Err("empty chord".into());
    }
    // A trailing literal '+' key ("ctrl++" or "+").
    let (mod_part, key_raw): (&str, String) = if chord == "+" {
        ("", "+".into())
    } else if let Some(m) = chord.strip_suffix("++") {
        (m, "+".into())
    } else {
        match chord.rsplit_once('+') {
            Some((m, k)) => (m, k.to_string()),
            None => ("", chord.to_string()),
        }
    };

    let mut mods: Vec<&'static str> = Vec::new();
    if !mod_part.is_empty() {
        for m in mod_part.split('+') {
            let lower = m.to_ascii_lowercase();
            let canon = match lower.as_str() {
                "cmd" => "super",
                other => MODIFIERS
                    .iter()
                    .copied()
                    .find(|x| *x == other)
                    .ok_or_else(|| format!("unknown modifier `{m}`"))?,
            };
            if mods.contains(&canon) {
                return Err(format!("duplicate modifier `{m}`"));
            }
            mods.push(canon);
        }
    }

    let mut key = key_raw;
    let mut range = None;
    if let Some((a, b)) = key.split_once("..") {
        if !allow_range {
            return Err("ranges are only allowed in the last chord of an action binding".into());
        }
        let a: u8 = a
            .parse()
            .map_err(|_| format!("invalid range start `{a}`"))?;
        let b: u8 = b.parse().map_err(|_| format!("invalid range end `{b}`"))?;
        if !(1..=9).contains(&a) || !(1..=9).contains(&b) || a > b {
            return Err("range must be within 1..9 and ascending".into());
        }
        range = Some((a, b));
        key = format!("{a}..{b}");
    } else {
        // Uppercase ASCII letter is shift+lowercase.
        if key.len() == 1 && key.as_bytes()[0].is_ascii_uppercase() {
            if !mods.contains(&"shift") {
                mods.push("shift");
            }
            key = key.to_ascii_lowercase();
        } else if key.len() > 1 {
            key = key.to_ascii_lowercase();
        }
        if !is_valid_key(&key) {
            return Err(format!("unknown key `{key}`"));
        }
    }

    mods.sort_by_key(|m| MODIFIERS.iter().position(|x| x == m).unwrap());
    let mut out = String::new();
    for m in &mods {
        out.push_str(m);
        out.push('+');
    }
    out.push_str(&key);
    Ok((out, range))
}

/// Parse and canonicalise a binding string. The empty string is not a binding (it unbinds)
/// and is rejected here; callers check for it first.
pub fn parse_binding(s: &str) -> Result<Binding, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty binding".into());
    }
    let (prefix, rest) = match s.strip_prefix("prefix+") {
        Some(r) => (true, r),
        None => (false, s),
    };
    let raw: Vec<&str> = rest.split_whitespace().collect();
    if raw.is_empty() {
        return Err("binding has no key after `prefix+`".into());
    }
    if !prefix && raw.len() > 1 {
        return Err("multi-chord sequences require the `prefix+` form".into());
    }
    let mut chords = Vec::new();
    let mut range = None;
    for (i, c) in raw.iter().enumerate() {
        let last = i + 1 == raw.len();
        let (canon, r) = parse_chord(c, last)?;
        if r.is_some() {
            range = r;
        }
        chords.push(canon);
    }
    Ok(Binding {
        prefix,
        chords,
        range,
    })
}

/// Validate the `keys.prefix` value: a single chord, no `prefix+`, no range.
pub fn parse_prefix_key(s: &str) -> Result<String, String> {
    let (c, r) = parse_chord(s.trim(), false)?;
    debug_assert!(r.is_none());
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalises() {
        let b = parse_binding("prefix+Shift+Ctrl+R").unwrap();
        assert_eq!(b.expand(), vec!["prefix+ctrl+shift+r"]);
        assert_eq!(
            parse_binding("prefix+shift+r").unwrap().expand(),
            vec!["prefix+shift+r"]
        );
        assert_eq!(
            parse_binding("prefix+R").unwrap().expand(),
            vec!["prefix+shift+r"]
        );
        assert_eq!(parse_binding("cmd+k").unwrap().expand(), vec!["super+k"]);
        assert!(parse_binding("cmd+k").unwrap().uses_super());
    }

    #[test]
    fn sequences_and_ranges() {
        let b = parse_binding("prefix+g w").unwrap();
        assert_eq!(b.chords, vec!["g", "w"]);
        let r = parse_binding("prefix+1..9").unwrap();
        assert_eq!(r.expand().len(), 9);
        assert_eq!(r.expand()[0], "prefix+1");
        let r = parse_binding("prefix+alt+1..3").unwrap();
        assert_eq!(
            r.expand(),
            vec!["prefix+alt+1", "prefix+alt+2", "prefix+alt+3"]
        );
        assert!(parse_binding("prefix+0..9").is_err());
        assert!(parse_binding("ctrl+1 2").is_err());
    }

    #[test]
    fn keys() {
        for ok in [
            "prefix+?",
            "prefix+[",
            "prefix+:",
            "prefix+minus",
            "ctrl+v",
            "f12",
            "prefix+f24",
            "prefix+tab",
            "prefix+shift+tab",
            "ctrl++",
            "prefix++",
            "altgr+e",
            "hyper+meta+x",
        ] {
            assert!(parse_binding(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "prefix+",
            "prefix+f25",
            "prefix+hyperx+a",
            "prefix+ctrl+ctrl+a",
            "prefix+nokey",
            "ctrl+",
        ] {
            assert!(parse_binding(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn prefix_key() {
        assert_eq!(parse_prefix_key("ctrl+b").unwrap(), "ctrl+b");
        assert_eq!(parse_prefix_key("-").unwrap(), "-");
        assert_eq!(parse_prefix_key("f12").unwrap(), "f12");
        assert!(parse_prefix_key("ctrl+1..9").is_err());
    }
}
