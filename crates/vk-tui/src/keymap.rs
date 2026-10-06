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
}

#[derive(Debug, Clone)]
pub struct Keymap {
    pub prefix: KeyEvent,
    pub bindings: Vec<Bound>,
    pub prefix_timeout_ms: u64,
    pub passthrough: bool,
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
            passthrough: cfg.keys.prefix_passthrough,
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
        });
        true
    }

    pub fn is_prefix(&self, ev: &KeyEvent) -> bool {
        key_matches(&self.prefix, ev)
    }

    /// Binding for the chord following the prefix.
    pub fn prefixed(&self, ev: &KeyEvent) -> Option<&Bound> {
        self.bindings
            .iter()
            .find(|b| b.prefix && b.chords.len() == 1 && key_matches(&b.chords[0], ev))
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
