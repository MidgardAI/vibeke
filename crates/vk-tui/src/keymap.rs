//! Client-side keybinding resolution (01 §1.4, 08 §10). Bindings use the shared key grammar and
//! match on the logical key + modifiers, so they are layout-independent.

use crossterm::event::{KeyCode, KeyEvent as CtKey, KeyEventKind, KeyModifiers};
use vk_proto::input::{Key, KeyEvent, KeyKind, Mods, NamedKey};
use vk_term::keygrammar::{expand_range, parse_binding};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    pub prefix: bool,
    pub chords: Vec<KeyEvent>,
    pub action: String,
    /// For indexed bindings (`switch_tab = prefix+1..9`): the index.
    pub index: Option<usize>,
    /// The binding names `altgr+…` explicitly: it matches AltGr text keys (03 §7.1).
    pub altgr: bool,
}

#[derive(Debug, Clone)]
pub struct Keymap {
    pub prefix: KeyEvent,
    pub bindings: Vec<Bound>,
    pub prefix_timeout_ms: u64,
    /// `keys.prefix_menu_ms` while `keys.prefix_menu` is on: the prefix menu appears this long
    /// after the prefix without a second key. `None` never shows it on its own.
    pub menu_ms: Option<u64>,
    pub passthrough: bool,
    /// `keys.altgr_mode`: AltGr text keys are text (`text`, `auto`) or chords (`chord`).
    pub altgr_text: bool,
}

impl Keymap {
    pub fn from_config(cfg: &vk_config::Config) -> Self {
        let prefix = vk_term::keygrammar::parse_key(&cfg.keys.prefix)
            .unwrap_or_else(|_| KeyEvent::new(Key::Char('b'), Mods::CTRL));
        let mut bindings = Vec::new();
        let mut add = |action: &str, spec: &str| {
            if spec.is_empty() {
                return;
            }
            let expanded = expand_range(spec).unwrap_or_else(|| vec![spec.to_string()]);
            let indexed = expanded.len() > 1;
            for (i, s) in expanded.iter().enumerate() {
                if let Ok(b) = parse_binding(s)
                    && !b.chords.is_empty()
                {
                    bindings.push(Bound {
                        prefix: b.prefix,
                        chords: b.chords,
                        action: action.to_string(),
                        index: indexed.then_some(i),
                        altgr: names_altgr(s),
                    });
                }
            }
        };
        for (action, spec) in &cfg.keys.bindings {
            add(action, spec);
        }
        // Vibeke actions without a config key yet (08 §6.5, §6.6).
        for (action, spec) in [
            ("inbox", "prefix+i"),
            ("next_attention_focus", "prefix+shift+a"),
            // `prefix+/` is search_scrollback (copy-mode search); the cross-pane search popup
            // takes alt+/ (M4).
            ("search_global", "prefix+alt+/"),
        ] {
            if !cfg.keys.bindings.contains_key(action) {
                add(action, spec);
            }
        }
        for (i, c) in cfg.keys.command.iter().enumerate() {
            add(&format!("command:{i}"), &c.key);
        }
        let mut km = Keymap {
            prefix,
            bindings,
            prefix_timeout_ms: cfg.keys.prefix_timeout_ms as u64,
            menu_ms: cfg
                .keys
                .prefix_menu
                .then_some(cfg.keys.prefix_menu_ms as u64),
            passthrough: cfg.keys.prefix_passthrough,
            altgr_text: cfg.keys.altgr_mode != vk_config::AltgrMode::Chord,
        };
        // `ctrl+shift+p` opens the palette directly when the host reports it unambiguously
        // (kitty keyboard; legacy hosts send plain ctrl+p, which never matches). A user binding
        // on the chord wins (08 §6.3).
        if cfg
            .keys
            .bindings
            .get("command_palette")
            .is_none_or(|b| !b.is_empty())
        {
            km.add_plugin_binding("command_palette", "ctrl+shift+p");
        }
        km
    }

    /// Add a plugin's default binding for `action`. Existing bindings (the user's, the
    /// defaults, earlier plugins) win: a chord already in use is not taken over. False when the
    /// binding is unusable or clashes.
    pub fn add_plugin_binding(&mut self, action: &str, spec: &str) -> bool {
        let Ok(b) = parse_binding(spec) else {
            return false;
        };
        if b.chords.is_empty() {
            return false;
        }
        let clash = self.bindings.iter().any(|o| {
            o.prefix == b.prefix
                && o.chords
                    .iter()
                    .zip(b.chords.iter())
                    .all(|(x, y)| key_matches(x, y))
        });
        if clash {
            return false;
        }
        self.bindings.push(Bound {
            prefix: b.prefix,
            chords: b.chords,
            action: action.to_string(),
            index: None,
            altgr: names_altgr(spec),
        });
        true
    }

    /// Apply `keys.altgr_mode` to a decoded key (03 §7.1). An AltGr text key (alt, with or
    /// without ctrl, whose associated text is not the key itself: `@` from AltGr+2) is **text
    /// input** in `text` mode (the default; `auto` behaves the same, since only hosts that
    /// report associated text produce such keys): it becomes the typed character, so it never
    /// matches a ctrl+alt / alt chord and reaches the pane as text, unless a binding names
    /// `altgr+…` for it, which then keeps it a chord. In `chord` mode the text is dropped and
    /// the key stays a chord for bindings and for the pane.
    pub fn altgr(&self, ev: KeyEvent) -> KeyEvent {
        if !is_altgr_text(&ev) {
            return ev;
        }
        let mut ev = ev;
        if !self.altgr_text {
            ev.text = None;
            return ev;
        }
        let as_chord = KeyEvent {
            mods: ev.mods.union(Mods::CTRL | Mods::ALT),
            text: None,
            ..ev.clone()
        };
        let bound = self
            .bindings
            .iter()
            .any(|b| b.altgr && b.chords.len() == 1 && key_matches(&b.chords[0], &as_chord));
        if bound {
            ev.mods = as_chord.mods;
            return ev;
        }
        let text = ev.text.clone().unwrap_or_default();
        let c = text.chars().next().unwrap_or(' ');
        KeyEvent {
            key: Key::Char(c),
            mods: Mods::empty(),
            kind: ev.kind,
            base_layout_key: None,
            shifted: None,
            text: Some(text),
        }
    }

    pub fn is_prefix(&self, ev: &KeyEvent) -> bool {
        key_matches(&self.prefix, ev)
    }

    /// Binding for the chord following the prefix.
    pub fn prefixed(&self, ev: &KeyEvent) -> Option<&Bound> {
        match self.resolve(&[], ev) {
            Resolve::Exact(b) => Some(b),
            _ => None,
        }
    }

    /// Resolve the next chord `ev` after the prefix and the chords `seq` already pressed. A
    /// binding of exactly `seq + ev` runs (first match wins, so an exact binding beats a
    /// sequence it shadows); a longer sequence through `seq + ev` opens a submenu.
    pub fn resolve(&self, seq: &[KeyEvent], ev: &KeyEvent) -> Resolve<'_> {
        let mut deeper = false;
        for b in self.bindings.iter().filter(|b| b.prefix) {
            if b.chords.len() <= seq.len() || !starts_with(&b.chords, seq) {
                continue;
            }
            if !key_matches(&b.chords[seq.len()], ev) {
                continue;
            }
            if b.chords.len() == seq.len() + 1 {
                return Resolve::Exact(b);
            }
            deeper = true;
        }
        if deeper {
            Resolve::Descend
        } else {
            Resolve::None
        }
    }

    /// The next chord of every prefix binding under `seq`, in binding order, one entry per
    /// distinct chord: the binding it runs, or the number of bindings behind a submenu.
    pub fn level(&self, seq: &[KeyEvent]) -> Vec<(KeyEvent, LevelEntry<'_>)> {
        let mut out: Vec<(KeyEvent, LevelEntry<'_>)> = Vec::new();
        for b in self.bindings.iter().filter(|b| b.prefix) {
            if b.chords.len() <= seq.len() || !starts_with(&b.chords, seq) {
                continue;
            }
            let next = &b.chords[seq.len()];
            let exact = b.chords.len() == seq.len() + 1;
            match out.iter_mut().find(|(k, _)| key_matches(k, next)) {
                Some((_, LevelEntry::Submenu(n))) if !exact => *n += 1,
                // An exact binding runs (`resolve`), so it is what the menu shows.
                Some((_, e @ LevelEntry::Submenu(_))) => *e = LevelEntry::Action(b),
                Some(_) => {}
                None => out.push((
                    next.clone(),
                    if exact {
                        LevelEntry::Action(b)
                    } else {
                        LevelEntry::Submenu(1)
                    },
                )),
            }
        }
        out
    }

    /// Direct (non-prefix) binding.
    pub fn direct(&self, ev: &KeyEvent) -> Option<&Bound> {
        self.bindings
            .iter()
            .find(|b| !b.prefix && b.chords.len() == 1 && key_matches(&b.chords[0], ev))
    }

    pub fn binding_for(&self, action: &str) -> Option<String> {
        self.bindings
            .iter()
            .find(|b| b.action == action && b.index.is_none_or(|i| i == 0))
            .map(|b| {
                let keys: Vec<String> = b
                    .chords
                    .iter()
                    .map(vk_term::keygrammar::format_key)
                    .collect();
                format!(
                    "{}{}",
                    if b.prefix { "prefix+" } else { "" },
                    keys.join(" ")
                )
            })
    }
}

/// Outcome of a chord after the prefix (see `Keymap::resolve`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolve<'a> {
    Exact(&'a Bound),
    Descend,
    None,
}

/// One entry of a prefix-menu level (see `Keymap::level`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LevelEntry<'a> {
    Action(&'a Bound),
    Submenu(usize),
}

fn starts_with(chords: &[KeyEvent], seq: &[KeyEvent]) -> bool {
    chords.len() >= seq.len()
        && chords
            .iter()
            .zip(seq.iter())
            .all(|(c, s)| key_matches(c, s))
}

/// The binding spec names the `altgr` modifier.
fn names_altgr(spec: &str) -> bool {
    spec.to_ascii_lowercase()
        .split(|c: char| c == '+' || c.is_whitespace())
        .any(|p| p == "altgr")
}

/// An AltGr text key: alt (ctrl optional) with associated text that is not the key's own
/// character (03 §7.1).
pub fn is_altgr_text(ev: &KeyEvent) -> bool {
    let Some(t) = ev.text.as_deref() else {
        return false;
    };
    if !ev.mods.alt() || t.is_empty() || t.chars().any(char::is_control) || ev.mods.sup() {
        return false;
    }
    let own = match ev.key {
        Key::Char(c) => {
            let mut b = [0u8; 4];
            t == c.encode_utf8(&mut b)
                || ev.shifted.is_some_and(|s| t == s.encode_utf8(&mut b))
                || t.to_lowercase() == c.to_lowercase().to_string()
        }
        Key::Named(_) => true,
    };
    !own
}

fn norm(ev: &KeyEvent) -> (Key, Mods) {
    let mut mods = ev.mods.without(Mods::META);
    if ev.mods.contains(Mods::META) {
        mods = mods.union(Mods::ALT);
    }
    let key = match ev.key {
        Key::Char(c) if c.is_uppercase() => {
            mods = mods.union(Mods::SHIFT);
            Key::Char(c.to_lowercase().next().unwrap_or(c))
        }
        Key::Named(NamedKey::Space) => Key::Char(' '),
        k => k,
    };
    (key, mods)
}

/// Compare a binding chord with an event. For non-letter printable characters the host
/// already applied shift (`?` arrives as `?` with SHIFT on some hosts), so SHIFT is ignored
/// when the binding doesn't mention it.
pub fn key_matches(binding: &KeyEvent, ev: &KeyEvent) -> bool {
    let (bk, bm) = norm(binding);
    let (ek, em) = norm(ev);
    if bk != ek {
        // Base-layout key (kitty) lets bindings work on non-US layouts.
        if let (Key::Char(b), Some(base)) = (bk, ev.base_layout_key) {
            return b == base.to_ascii_lowercase() && bm == em;
        }
        return false;
    }
    match ek {
        Key::Char(c) if !c.is_alphabetic() && !bm.shift() => bm == em.without(Mods::SHIFT),
        _ => bm == em,
    }
}

/// crossterm → logical key event.
pub fn from_crossterm(k: &CtKey) -> Option<KeyEvent> {
    let mut mods = Mods::empty();
    let m = k.modifiers;
    if m.contains(KeyModifiers::SHIFT) {
        mods = mods.union(Mods::SHIFT);
    }
    if m.contains(KeyModifiers::CONTROL) {
        mods = mods.union(Mods::CTRL);
    }
    if m.contains(KeyModifiers::ALT) {
        mods = mods.union(Mods::ALT);
    }
    if m.contains(KeyModifiers::SUPER) {
        mods = mods.union(Mods::SUPER);
    }
    if m.contains(KeyModifiers::HYPER) {
        mods = mods.union(Mods::HYPER);
    }
    if m.contains(KeyModifiers::META) {
        mods = mods.union(Mods::META);
    }
    let key = match k.code {
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Enter => Key::Named(NamedKey::Enter),
        KeyCode::Tab => Key::Named(NamedKey::Tab),
        KeyCode::BackTab => {
            mods = mods.union(Mods::SHIFT);
            Key::Named(NamedKey::Tab)
        }
        KeyCode::Backspace => Key::Named(NamedKey::Backspace),
        KeyCode::Esc => Key::Named(NamedKey::Escape),
        KeyCode::Up => Key::Named(NamedKey::Up),
        KeyCode::Down => Key::Named(NamedKey::Down),
        KeyCode::Left => Key::Named(NamedKey::Left),
        KeyCode::Right => Key::Named(NamedKey::Right),
        KeyCode::Home => Key::Named(NamedKey::Home),
        KeyCode::End => Key::Named(NamedKey::End),
        KeyCode::PageUp => Key::Named(NamedKey::PageUp),
        KeyCode::PageDown => Key::Named(NamedKey::PageDown),
        KeyCode::Insert => Key::Named(NamedKey::Insert),
        KeyCode::Delete => Key::Named(NamedKey::Delete),
        KeyCode::F(n) => Key::Named(NamedKey::F(n)),
        KeyCode::CapsLock => Key::Named(NamedKey::CapsLock),
        KeyCode::ScrollLock => Key::Named(NamedKey::ScrollLock),
        KeyCode::NumLock => Key::Named(NamedKey::NumLock),
        KeyCode::PrintScreen => Key::Named(NamedKey::PrintScreen),
        KeyCode::Pause => Key::Named(NamedKey::Pause),
        KeyCode::Menu => Key::Named(NamedKey::Menu),
        KeyCode::Null => {
            mods = mods.union(Mods::CTRL);
            Key::Char(' ')
        }
        KeyCode::Modifier(mk) => {
            use crossterm::event::ModifierKeyCode as M;
            Key::Named(match mk {
                M::LeftShift => NamedKey::LeftShift,
                M::LeftControl => NamedKey::LeftControl,
                M::LeftAlt => NamedKey::LeftAlt,
                M::LeftSuper => NamedKey::LeftSuper,
                M::RightShift => NamedKey::RightShift,
                M::RightControl => NamedKey::RightControl,
                M::RightAlt => NamedKey::RightAlt,
                M::RightSuper => NamedKey::RightSuper,
                _ => return None,
            })
        }
        _ => return None,
    };
    // Shift is implicit in uppercase/shifted chars for legacy hosts; keep the char as typed.
    if let Key::Char(c) = key
        && !c.is_alphabetic()
        && c != ' '
        && mods == Mods::SHIFT
    {
        mods = Mods::empty();
    }
    let kind = match k.kind {
        KeyEventKind::Press => KeyKind::Press,
        KeyEventKind::Repeat => KeyKind::Repeat,
        KeyEventKind::Release => KeyKind::Release,
    };
    let text = match key {
        Key::Char(c) if !mods.ctrl() && !mods.alt() && !mods.sup() => Some(c.to_string()),
        _ => None,
    };
    Some(KeyEvent {
        key,
        mods,
        kind,
        base_layout_key: None,
        shifted: None,
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Keymap {
        Keymap::from_config(&vk_config::Config::default())
    }

    fn altgr_key(text: &str) -> KeyEvent {
        KeyEvent {
            key: Key::Char('2'),
            mods: Mods::CTRL | Mods::ALT,
            kind: KeyKind::Press,
            base_layout_key: Some('2'),
            shifted: None,
            text: Some(text.into()),
        }
    }

    fn with_keys(extra: &[(&str, &str)], mode: &str) -> Keymap {
        let mut cfg = vk_config::Config::default();
        for (a, b) in extra {
            cfg.keys.bindings.insert(a.to_string(), b.to_string());
        }
        cfg.keys.altgr_mode = match mode {
            "chord" => vk_config::AltgrMode::Chord,
            "text" => vk_config::AltgrMode::Text,
            _ => vk_config::AltgrMode::Auto,
        };
        Keymap::from_config(&cfg)
    }

    #[test]
    fn altgr_text_is_text_unless_a_binding_names_altgr() {
        // A ctrl+alt+2 binding must not fire for AltGr+2 = "@" (Norwegian layout).
        let km = with_keys(&[("split_vertical", "ctrl+alt+2")], "auto");
        assert!(km.altgr_text, "auto behaves as text");
        let ev = km.altgr(altgr_key("@"));
        assert_eq!(ev.key, Key::Char('@'));
        assert_eq!(ev.mods, Mods::empty());
        assert_eq!(ev.text.as_deref(), Some("@"));
        assert!(km.direct(&ev).is_none());
        // macOS Option+2 (alt only) with text "@" too.
        let mut mac = altgr_key("@");
        mac.mods = Mods::ALT;
        assert_eq!(km.altgr(mac).key, Key::Char('@'));
        // A binding that names altgr+2 keeps it a chord.
        let km = with_keys(&[("split_vertical", "altgr+2")], "text");
        let ev = km.altgr(altgr_key("@"));
        assert_eq!(ev.mods, Mods::CTRL | Mods::ALT);
        assert_eq!(km.direct(&ev).unwrap().action, "split_vertical");
        // Chord mode: the text is dropped and the chord matches.
        let km = with_keys(&[("split_vertical", "ctrl+alt+2")], "chord");
        let ev = km.altgr(altgr_key("@"));
        assert_eq!(ev.text, None);
        assert_eq!(km.direct(&ev).unwrap().action, "split_vertical");
        // Not AltGr text: alt+x reporting its own character, or no text at all.
        let km = with_keys(&[], "text");
        let mut alt_x = KeyEvent::new(Key::Char('x'), Mods::ALT);
        alt_x.text = Some("x".into());
        assert_eq!(km.altgr(alt_x.clone()), alt_x);
        let plain = KeyEvent::new(Key::Char('q'), Mods::CTRL | Mods::ALT);
        assert_eq!(km.altgr(plain.clone()), plain);
    }

    #[test]
    fn sequences_descend_and_resolve() {
        let km = with_keys(&[("new_tab", "prefix+m w"), ("zoom", "prefix+m t")], "auto");
        let m = KeyEvent::ch('m');
        assert_eq!(km.resolve(&[], &m), Resolve::Descend);
        assert!(km.prefixed(&m).is_none());
        match km.resolve(&[m.clone()], &KeyEvent::ch('w')) {
            Resolve::Exact(b) => assert_eq!(b.action, "new_tab"),
            other => panic!("{other:?}"),
        }
        assert_eq!(km.resolve(&[m.clone()], &KeyEvent::ch('q')), Resolve::None);
        assert_eq!(km.resolve(&[], &KeyEvent::ch('~')), Resolve::None);
        // The level under `m` lists both; the top level shows one submenu of two.
        let level = km.level(&[m.clone()]);
        assert_eq!(level.len(), 2);
        assert!(matches!(level[0].1, LevelEntry::Action(b) if b.action == "new_tab"));
        let top = km.level(&[]);
        let (_, entry) = top.iter().find(|(k, _)| key_matches(k, &m)).unwrap();
        assert_eq!(*entry, LevelEntry::Submenu(2));
        // An exact binding beats a sequence it shadows (config check reports the clash).
        let km = with_keys(&[("new_tab", "prefix+c"), ("zoom", "prefix+c t")], "auto");
        assert!(matches!(
            km.resolve(&[], &KeyEvent::ch('c')),
            Resolve::Exact(_)
        ));
        // Whichever comes first, and the menu says so.
        let km = with_keys(&[("new_tab", "prefix+m w"), ("zoom", "prefix+m")], "auto");
        assert!(matches!(km.resolve(&[], &m), Resolve::Exact(b) if b.action == "zoom"));
        let top = km.level(&[]);
        let (_, entry) = top.iter().find(|(k, _)| key_matches(k, &m)).unwrap();
        assert!(matches!(entry, LevelEntry::Action(b) if b.action == "zoom"));
        // Disabled menu: no delay.
        let mut cfg = vk_config::Config::default();
        assert_eq!(Keymap::from_config(&cfg).menu_ms, Some(400));
        cfg.keys.prefix_menu = false;
        assert_eq!(Keymap::from_config(&cfg).menu_ms, None);
    }

    #[test]
    fn base_layout_key_matches_bindings_on_other_layouts() {
        // Norwegian: the key at US `[` produces `å`; kitty reports base layout key `[`.
        let km = cfg();
        let mut ev = KeyEvent::new(Key::Char('å'), Mods::empty());
        ev.base_layout_key = Some('[');
        assert_eq!(km.prefixed(&ev).unwrap().action, "enter_copy_mode");
    }

    #[test]
    fn herdr_defaults_resolve() {
        let km = cfg();
        assert!(km.is_prefix(&KeyEvent::new(Key::Char('b'), Mods::CTRL)));
        let v = km.prefixed(&KeyEvent::ch('v')).unwrap();
        assert_eq!(v.action, "split_vertical");
        let minus = km.prefixed(&KeyEvent::ch('-')).unwrap();
        assert_eq!(minus.action, "split_horizontal");
        let t = km
            .prefixed(&KeyEvent::new(Key::Char('T'), Mods::SHIFT))
            .unwrap();
        assert_eq!(t.action, "rename_tab");
        let three = km.prefixed(&KeyEvent::ch('3')).unwrap();
        assert_eq!(
            (three.action.as_str(), three.index),
            ("switch_tab", Some(2))
        );
        // `?` arrives with SHIFT from some hosts.
        assert_eq!(
            km.prefixed(&KeyEvent::new(Key::Char('?'), Mods::SHIFT))
                .unwrap()
                .action,
            "help"
        );
        assert_eq!(
            km.prefixed(&KeyEvent::ch('[')).unwrap().action,
            "enter_copy_mode"
        );
    }
}
