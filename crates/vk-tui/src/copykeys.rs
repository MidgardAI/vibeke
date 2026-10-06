//! Copy-mode keys (03 §11.1, 08 §10.2): a vi or emacs base table from
//! `[keys.copy_mode] mode`, with per-key overrides (`key = "action"`, `""` unbinds) on top.
//! Action names are `vk_config::COPY_MODE_ACTIONS`; invalid entries were already reported as
//! config warnings and are skipped here.

use crate::keymap::key_matches;
use std::sync::{Arc, OnceLock};
use vk_proto::input::KeyEvent;
use vk_term::keygrammar::parse_key;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyAction {
    Exit,
    /// Clear the selection, or exit when there is none.
    Cancel,
    Left,
    Right,
    Up,
    Down,
    HalfPageUp,
    HalfPageDown,
    PageUp,
    PageDown,
    LineStart,
    LineEnd,
    Top,
    Bottom,
    ViewTop,
    ViewMiddle,
    ViewBottom,
    WordNext,
    WordPrev,
    WordEnd,
    SelectChar,
    SelectLine,
    SelectBlock,
    SearchForward,
    SearchBackward,
    SearchNext,
    SearchPrev,
    Copy,
    /// Open the scrollback viewer / `$EDITOR` at this position (08 §13, `edit_scrollback`).
    EditScrollback,
    /// Previous / next OSC 133 prompt (03 §8, §11.1).
    PromptPrev,
    PromptNext,
    /// Select the output of the command under the cursor (OSC 133).
    SelectOutput,
}

/// Config name → action (every `vk_config::COPY_MODE_ACTIONS` entry).
pub fn action_named(name: &str) -> Option<CopyAction> {
    use CopyAction::*;
    Some(match name {
        "exit" => Exit,
        "cancel" => Cancel,
        "left" => Left,
        "right" => Right,
        "up" => Up,
        "down" => Down,
        "half_page_up" => HalfPageUp,
        "half_page_down" => HalfPageDown,
        "page_up" => PageUp,
        "page_down" => PageDown,
        "line_start" => LineStart,
        "line_end" => LineEnd,
        "top" => Top,
        "bottom" => Bottom,
        "view_top" => ViewTop,
        "view_middle" => ViewMiddle,
        "view_bottom" => ViewBottom,
        "word_next" => WordNext,
        "word_prev" => WordPrev,
        "word_end" => WordEnd,
        "select_char" => SelectChar,
        "select_line" => SelectLine,
        "select_block" => SelectBlock,
        "search_forward" => SearchForward,
        "search_backward" => SearchBackward,
        "search_next" => SearchNext,
        "search_prev" => SearchPrev,
        "copy" => Copy,
        "edit_scrollback" => EditScrollback,
        "prompt_prev" => PromptPrev,
        "prompt_next" => PromptNext,
        "select_output" => SelectOutput,
        _ => return None,
    })
}

const VI: &[(&str, &str)] = &[
    ("esc", "cancel"),
    ("q", "exit"),
    ("ctrl+c", "exit"),
    ("ctrl+u", "half_page_up"),
    ("ctrl+d", "half_page_down"),
    ("ctrl+b", "page_up"),
    ("ctrl+f", "page_down"),
    ("pageup", "page_up"),
    ("pagedown", "page_down"),
    ("h", "left"),
    ("left", "left"),
    ("l", "right"),
    ("right", "right"),
    ("k", "up"),
    ("up", "up"),
    ("j", "down"),
    ("down", "down"),
    ("0", "line_start"),
    ("home", "line_start"),
    ("$", "line_end"),
    ("end", "line_end"),
    ("g", "top"),
    ("G", "bottom"),
    ("H", "view_top"),
    ("M", "view_middle"),
    ("L", "view_bottom"),
    ("w", "word_next"),
    ("b", "word_prev"),
    ("e", "word_end"),
    ("v", "select_char"),
    ("V", "select_line"),
    ("ctrl+v", "select_block"),
    ("/", "search_forward"),
    ("?", "search_backward"),
    ("n", "search_next"),
    ("N", "search_prev"),
    ("y", "copy"),
    ("enter", "copy"),
    ("[", "prompt_prev"),
    ("]", "prompt_next"),
    ("o", "select_output"),
];

const EMACS: &[(&str, &str)] = &[
    ("esc", "cancel"),
    ("ctrl+g", "cancel"),
    ("q", "exit"),
    ("ctrl+c", "exit"),
    ("ctrl+b", "left"),
    ("left", "left"),
    ("ctrl+f", "right"),
    ("right", "right"),
    ("ctrl+p", "up"),
    ("up", "up"),
    ("ctrl+n", "down"),
    ("down", "down"),
    ("ctrl+a", "line_start"),
    ("home", "line_start"),
    ("ctrl+e", "line_end"),
    ("end", "line_end"),
    ("alt+<", "top"),
    ("alt+>", "bottom"),
    ("ctrl+v", "page_down"),
    ("alt+v", "page_up"),
    ("pagedown", "page_down"),
    ("pageup", "page_up"),
    ("alt+f", "word_next"),
    ("alt+b", "word_prev"),
    ("ctrl+space", "select_char"),
    ("alt+l", "select_line"),
    ("alt+r", "select_block"),
    ("ctrl+s", "search_forward"),
    ("ctrl+r", "search_backward"),
    ("n", "search_next"),
    ("N", "search_prev"),
    ("alt+w", "copy"),
    ("enter", "copy"),
    ("alt+{", "prompt_prev"),
    ("alt+}", "prompt_next"),
    ("alt+o", "select_output"),
];

/// Resolved copy-mode keys; the first matching entry wins.
#[derive(Debug, Clone, PartialEq)]
pub struct CopyKeys {
    pub emacs: bool,
    binds: Vec<(KeyEvent, CopyAction)>,
}

fn table(t: &[(&str, &str)]) -> Vec<(KeyEvent, CopyAction)> {
    t.iter()
        .filter_map(|(k, a)| Some((parse_key(k).ok()?, action_named(a)?)))
        .collect()
}

impl CopyKeys {
    pub fn vi() -> Self {
        CopyKeys {
            emacs: false,
            binds: table(VI),
        }
    }

    pub fn emacs() -> Self {
        CopyKeys {
            emacs: true,
            binds: table(EMACS),
        }
    }

    /// The shared default (vi, no overrides).
    pub fn default_arc() -> Arc<CopyKeys> {
        static D: OnceLock<Arc<CopyKeys>> = OnceLock::new();
        D.get_or_init(|| Arc::new(CopyKeys::vi())).clone()
    }

    pub fn from_config(c: &vk_config::CopyMode) -> Self {
        let mut keys = match c.mode {
            vk_config::CopyModeKind::Emacs => CopyKeys::emacs(),
            vk_config::CopyModeKind::Vi => CopyKeys::vi(),
        };
        let mut over = Vec::new();
        for (k, a) in &c.overrides {
            let Ok(ev) = parse_key(k) else {
                continue;
            };
            // The override replaces whatever the base table had on that key.
            keys.binds
                .retain(|(b, _)| !(key_matches(b, &ev) && key_matches(&ev, b)));
            if let Some(act) = action_named(a) {
                over.push((ev, act));
            }
        }
        over.append(&mut keys.binds);
        keys.binds = over;
        keys
    }

    pub fn resolve(&self, ev: &KeyEvent) -> Option<CopyAction> {
        self.binds
            .iter()
            .find(|(b, _)| key_matches(b, ev))
            .map(|(_, a)| *a)
    }

    /// First key bound to `action` (for hints).
    pub fn key_for(&self, action: CopyAction) -> Option<String> {
        self.binds
            .iter()
            .find(|(_, a)| *a == action)
            .map(|(k, _)| vk_term::keygrammar::format_key(k))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vk_proto::input::{Key, Mods, NamedKey};

    fn ev(k: Key, m: Mods) -> KeyEvent {
        KeyEvent::new(k, m)
    }

    #[test]
    fn every_config_action_maps_and_tables_parse() {
        for a in vk_config::COPY_MODE_ACTIONS {
            assert!(action_named(a).is_some(), "{a}");
        }
        assert_eq!(table(VI).len(), VI.len(), "every vi entry parses");
        assert_eq!(table(EMACS).len(), EMACS.len(), "every emacs entry parses");
    }

    #[test]
    fn vi_defaults() {
        let k = CopyKeys::vi();
        use CopyAction::*;
        for (e, want) in [
            (ev(Key::Char('j'), Mods::empty()), Down),
            (ev(Key::Char('G'), Mods::empty()), Bottom),
            (ev(Key::Char('G'), Mods::SHIFT), Bottom),
            (ev(Key::Char('$'), Mods::SHIFT), LineEnd),
            (ev(Key::Char('?'), Mods::empty()), SearchBackward),
            (ev(Key::Char('N'), Mods::empty()), SearchPrev),
            (ev(Key::Char('v'), Mods::CTRL), SelectBlock),
            (ev(Key::Named(NamedKey::Escape), Mods::empty()), Cancel),
            (ev(Key::Named(NamedKey::Enter), Mods::empty()), Copy),
        ] {
            assert_eq!(k.resolve(&e), Some(want), "{e:?}");
        }
        assert_eq!(k.resolve(&ev(Key::Char('q'), Mods::CTRL)), None);
        assert_eq!(k.resolve(&ev(Key::Char('z'), Mods::empty())), None);
    }

    #[test]
    fn emacs_base_and_overrides() {
        let mut c = vk_config::CopyMode {
            mode: vk_config::CopyModeKind::Emacs,
            ..Default::default()
        };
        let k = CopyKeys::from_config(&c);
        use CopyAction::*;
        assert!(k.emacs);
        assert_eq!(k.resolve(&ev(Key::Char('n'), Mods::CTRL)), Some(Down));
        assert_eq!(k.resolve(&ev(Key::Char(' '), Mods::CTRL)), Some(SelectChar));
        assert_eq!(k.resolve(&ev(Key::Char('w'), Mods::ALT)), Some(Copy));
        assert_eq!(k.resolve(&ev(Key::Char('<'), Mods::ALT)), Some(Top));
        // Plain letters do nothing in emacs mode (no vi motions).
        assert_eq!(k.resolve(&ev(Key::Char('j'), Mods::empty())), None);
        c.overrides
            .insert("ctrl+e".into(), "edit_scrollback".into());
        c.overrides.insert("q".into(), String::new());
        c.overrides.insert("x".into(), "copy".into());
        c.overrides.insert("bogus+key".into(), "copy".into());
        let k = CopyKeys::from_config(&c);
        assert_eq!(
            k.resolve(&ev(Key::Char('e'), Mods::CTRL)),
            Some(EditScrollback)
        );
        assert_eq!(k.resolve(&ev(Key::Char('q'), Mods::empty())), None);
        assert_eq!(k.resolve(&ev(Key::Char('x'), Mods::empty())), Some(Copy));
        assert_eq!(k.key_for(EditScrollback).as_deref(), Some("ctrl+e"));
    }
}
