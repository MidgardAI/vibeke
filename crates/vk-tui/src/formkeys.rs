//! One key convention for every TUI form, dialog and tabbed view, so they all work the same way:
//!
//! - **←/→** (and `h`/`l` outside text fields) change the focused option: the next or previous
//!   value of a choice, on or off for a toggle. **space** changes it too (forward for a choice,
//!   flip for a toggle).
//! - **↑/↓** and **tab/shift+tab** (and `j`/`k` outside text fields) move between the fields.
//! - **enter** confirms or activates; **esc** goes back.
//! - A view with tabs switches them with **tab/shift+tab** from its list. In a form, tab moves
//!   between fields instead; **↑** from the top row moves onto the tab strip ([`Strip`]), where
//!   **←/→** switch tabs and **↓**, **enter** or **esc** go back into the tab.
//!
//! Letter shortcuts a view already has keep working alongside these keys. Footers say the same
//! thing in the same words ([`FORM_HINT`], [`STRIP_HINT`]).

use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};

/// What a key does in a form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    /// The next field (↓, tab, `j`).
    Next,
    /// The previous field (↑, shift+tab, `k`).
    Prev,
    /// The focused option's next value (→, `l`).
    Inc,
    /// The focused option's previous value (←, `h`).
    Dec,
    /// Space: forward for a choice, flip for a toggle.
    Toggle,
    /// Enter.
    Activate,
    /// Esc.
    Back,
    /// Anything else (letters, text input): the view's own.
    Other,
}

/// The [`Nav`] of `ev`. `typing` is true while a text field has the focus: then letters and
/// space are text, and only the named keys navigate.
pub fn nav(ev: &KeyEvent, typing: bool) -> Nav {
    if ev.kind == KeyKind::Release {
        return Nav::Other;
    }
    let plain = !ev.mods.ctrl() && !ev.mods.alt();
    match &ev.key {
        Key::Named(NamedKey::Tab) if plain && ev.mods.shift() => Nav::Prev,
        Key::Named(NamedKey::Tab) if plain => Nav::Next,
        Key::Named(NamedKey::Down) => Nav::Next,
        Key::Named(NamedKey::Up) => Nav::Prev,
        Key::Named(NamedKey::Right) => Nav::Inc,
        Key::Named(NamedKey::Left) => Nav::Dec,
        Key::Named(NamedKey::Enter) => Nav::Activate,
        Key::Named(NamedKey::Escape) => Nav::Back,
        Key::Named(NamedKey::Space) if !typing => Nav::Toggle,
        Key::Char(' ') if !typing => Nav::Toggle,
        Key::Char('j') if plain && !typing => Nav::Next,
        Key::Char('k') if plain && !typing => Nav::Prev,
        Key::Char('l') if plain && !typing => Nav::Inc,
        Key::Char('h') if plain && !typing => Nav::Dec,
        _ => Nav::Other,
    }
}

/// `+1` for [`Nav::Next`]/[`Nav::Inc`]/[`Nav::Toggle`], `-1` for [`Nav::Prev`]/[`Nav::Dec`].
pub fn step(n: Nav) -> Option<i32> {
    match n {
        Nav::Next | Nav::Inc | Nav::Toggle => Some(1),
        Nav::Prev | Nav::Dec => Some(-1),
        _ => None,
    }
}

/// `at` moved by `by` in a list of `len`, wrapping around.
pub fn wrap(at: usize, by: i32, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (at as i64 + by as i64).rem_euclid(len as i64) as usize
}

/// tab / shift+tab as a tab step (`+1` / `-1`), for a view's list level.
pub fn tab_step(ev: &KeyEvent) -> Option<i32> {
    if ev.kind == KeyKind::Release || ev.mods.ctrl() || ev.mods.alt() {
        return None;
    }
    match ev.key {
        Key::Named(NamedKey::Tab) => Some(if ev.mods.shift() { -1 } else { 1 }),
        _ => None,
    }
}

/// The focus of a view's tab strip. [`Strip::key`] handles a key while the strip has it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Strip {
    pub focused: bool,
}

/// What a key on a focused [`Strip`] asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripKey {
    /// Switch to the tab `by` steps away; the strip keeps the focus.
    Switch(i32),
    /// Back into the tab (the strip lets go of the focus).
    Leave,
    /// Ignored on the strip.
    None,
}

impl Strip {
    /// A key while the strip is focused. ←/→, `h`/`l` and tab/shift+tab switch tabs; ↓, `j`,
    /// enter and esc go back into the tab.
    pub fn key(&mut self, ev: &KeyEvent) -> StripKey {
        if ev.kind == KeyKind::Release {
            return StripKey::None;
        }
        match nav(ev, false) {
            Nav::Inc => StripKey::Switch(1),
            Nav::Dec => StripKey::Switch(-1),
            _ if tab_step(ev).is_some() => StripKey::Switch(tab_step(ev).unwrap_or(1)),
            Nav::Next | Nav::Activate | Nav::Back => {
                self.focused = false;
                StripKey::Leave
            }
            _ => StripKey::None,
        }
    }
}

/// The footer of a form, in every view.
pub const FORM_HINT: &str = "←/→ change · ↑/↓ or tab move · enter confirm · esc back";
/// The strip's hint while it is focused.
pub const STRIP_HINT: &str = "←/→ switch tabs · ↓ back";
/// The strip's hint while a list has the keys.
pub const LIST_TABS_HINT: &str = "tab switches tabs";

#[cfg(test)]
mod tests {
    use super::*;
    use vk_proto::input::Mods;

    fn k(key: Key, mods: Mods) -> KeyEvent {
        KeyEvent::new(key, mods)
    }
    fn named(n: NamedKey) -> KeyEvent {
        k(Key::Named(n), Mods::default())
    }

    #[test]
    fn the_keys_mean_the_same_everywhere() {
        assert_eq!(nav(&named(NamedKey::Tab), false), Nav::Next);
        assert_eq!(
            nav(&k(Key::Named(NamedKey::Tab), Mods::SHIFT), false),
            Nav::Prev
        );
        assert_eq!(nav(&named(NamedKey::Down), true), Nav::Next);
        assert_eq!(nav(&named(NamedKey::Up), true), Nav::Prev);
        assert_eq!(nav(&named(NamedKey::Right), false), Nav::Inc);
        assert_eq!(nav(&named(NamedKey::Left), false), Nav::Dec);
        assert_eq!(nav(&named(NamedKey::Space), false), Nav::Toggle);
        assert_eq!(nav(&named(NamedKey::Enter), false), Nav::Activate);
        assert_eq!(nav(&named(NamedKey::Escape), false), Nav::Back);
        assert_eq!(nav(&k(Key::Char('l'), Mods::default()), false), Nav::Inc);
        // In a text field, letters and space are text.
        assert_eq!(nav(&k(Key::Char('l'), Mods::default()), true), Nav::Other);
        assert_eq!(nav(&named(NamedKey::Space), true), Nav::Other);
        assert_eq!(wrap(0, -1, 3), 2);
        assert_eq!(wrap(2, 1, 3), 0);
        assert_eq!(wrap(0, 1, 0), 0);
    }

    #[test]
    fn the_strip_switches_and_lets_go() {
        let mut s = Strip { focused: true };
        assert_eq!(s.key(&named(NamedKey::Right)), StripKey::Switch(1));
        assert_eq!(s.key(&named(NamedKey::Left)), StripKey::Switch(-1));
        assert_eq!(
            s.key(&k(Key::Named(NamedKey::Tab), Mods::SHIFT)),
            StripKey::Switch(-1)
        );
        assert!(s.focused);
        assert_eq!(s.key(&named(NamedKey::Down)), StripKey::Leave);
        assert!(!s.focused);
    }
}
