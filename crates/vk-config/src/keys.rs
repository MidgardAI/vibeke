//! Default keymap (08 §10.2), action aliases and the conflict check.

use std::collections::BTreeMap;

use crate::binding::parse_binding;
use crate::types::Config;

/// `(action, default binding)`. An empty binding is an action that exists but is unbound.
pub const DEFAULT_KEYMAP: &[(&str, &str)] = &[
    ("help", "prefix+?"),
    ("settings", "prefix+s"),
    ("detach", "prefix+q"),
    ("reload_config", "prefix+shift+r"),
    ("open_notification_target", "prefix+o"),
    ("workspace_picker", "prefix+w"),
    ("goto", "prefix+g"),
    ("new_workspace", "prefix+shift+n"),
    ("new_worktree", "prefix+shift+g"),
    ("open_worktree", ""),
    ("remove_worktree", ""),
    ("rename_workspace", "prefix+shift+w"),
    ("close_workspace", "prefix+shift+d"),
    ("previous_workspace", ""),
    ("next_workspace", ""),
    ("previous_agent", ""),
    ("next_agent", ""),
    ("focus_agent", ""),
    ("switch_workspace", ""),
    ("new_tab", "prefix+c"),
    ("rename_tab", "prefix+shift+t"),
    ("previous_tab", "prefix+p"),
    ("next_tab", "prefix+n"),
    ("switch_tab", "prefix+1..9"),
    ("close_tab", "prefix+shift+x"),
    ("rename_pane", "prefix+shift+p"),
    ("remote_image_paste", "ctrl+v"),
    ("split_vertical", "prefix+v"),
    ("split_horizontal", "prefix+minus"),
    ("close_pane", "prefix+x"),
    ("zoom", "prefix+z"),
    ("resize_mode", "prefix+r"),
    ("toggle_sidebar", "prefix+b"),
    ("focus_pane_left", "prefix+h"),
    ("focus_pane_down", "prefix+j"),
    ("focus_pane_up", "prefix+k"),
    ("focus_pane_right", "prefix+l"),
    ("cycle_pane_next", "prefix+tab"),
    ("cycle_pane_previous", "prefix+shift+tab"),
    ("last_pane", ""),
    ("edit_scrollback", "prefix+e"),
    ("enter_copy_mode", "prefix+["),
    ("paste_buffer", "prefix+]"),
    ("command_palette", "prefix+:"),
    ("search_scrollback", "prefix+/"),
    ("next_attention", "prefix+a"),
    ("mark_unread", "prefix+u"),
    ("pin_pane", "prefix+alt+p"),
    ("float_new", "prefix+f"),
    ("toggle_floats", "prefix+shift+f"),
    ("sync_input", "prefix+shift+s"),
    ("new_task", "prefix+shift+k"),
    ("preview_list", "prefix+shift+o"),
];

/// Herdr action names accepted as aliases for ours (`from`, `to`).
pub const ACTION_ALIASES: &[(&str, &str)] = &[("fullscreen", "zoom")];

pub fn default_bindings() -> BTreeMap<String, String> {
    DEFAULT_KEYMAP
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

pub fn is_known_action(name: &str) -> bool {
    DEFAULT_KEYMAP.iter().any(|(a, _)| *a == name)
}

pub fn canonical_action(name: &str) -> &str {
    ACTION_ALIASES
        .iter()
        .find(|(f, _)| *f == name)
        .map(|(_, t)| *t)
        .unwrap_or(name)
}

/// Two or more actions that cannot be told apart by their bindings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    /// The canonical binding (or, for sequence overlaps, the shorter binding).
    pub binding: String,
    /// Actions involved, sorted. Custom commands appear as `command[<index>]`.
    pub actions: Vec<String>,
    pub reason: ConflictReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictReason {
    /// Identical canonical bindings.
    Duplicate,
    /// One binding is a strict prefix of another sequence (`prefix+g` vs `prefix+g w`).
    ShadowedSequence,
}

/// Report duplicate bindings for different actions. Unparsable bindings are skipped (the
/// loader reports them as errors). Empty bindings are unbound and ignored.
pub fn check_keys(cfg: &Config) -> Vec<Conflict> {
    let mut entries: Vec<(String, String, Vec<String>)> = Vec::new(); // (action, canon, chords+prefix)
    let mut add = |action: String, raw: &str| {
        if raw.trim().is_empty() {
            return;
        }
        if let Ok(b) = parse_binding(raw) {
            for canon in b.expand() {
                let body = canon.strip_prefix("prefix+").unwrap_or(&canon);
                let mut seq: Vec<String> = body.split_whitespace().map(String::from).collect();
                if b.prefix {
                    seq.insert(0, "prefix".into());
                }
                entries.push((action.clone(), canon, seq));
            }
        }
    };
    for (action, raw) in &cfg.keys.bindings {
        add(action.clone(), raw);
    }
    for (i, c) in cfg.keys.command.iter().enumerate() {
        add(format!("command[{i}]"), &c.key);
    }

    let mut out = Vec::new();
    let mut by_binding: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (action, canon, _) in &entries {
        let v = by_binding.entry(canon.as_str()).or_default();
        if !v.contains(&action.as_str()) {
            v.push(action.as_str());
        }
    }
    for (binding, mut actions) in by_binding {
        if actions.len() > 1 {
            actions.sort();
            out.push(Conflict {
                binding: binding.to_string(),
                actions: actions.into_iter().map(String::from).collect(),
                reason: ConflictReason::Duplicate,
            });
        }
    }

    // Sequence shadowing among prefix bindings.
    for (a_act, a_canon, a_seq) in &entries {
        for (b_act, _, b_seq) in &entries {
            if a_act != b_act && a_seq.len() < b_seq.len() && b_seq.starts_with(a_seq) {
                let mut actions = vec![a_act.clone(), b_act.clone()];
                actions.sort();
                let c = Conflict {
                    binding: a_canon.clone(),
                    actions,
                    reason: ConflictReason::ShadowedSequence,
                };
                if !out.contains(&c) {
                    out.push(c);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_defaults_have_no_conflicts() {
        let cfg = Config::default();
        assert_eq!(check_keys(&cfg), vec![]);
    }

    #[test]
    fn defaults_all_parse() {
        for (a, b) in DEFAULT_KEYMAP {
            if !b.is_empty() {
                parse_binding(b).unwrap_or_else(|e| panic!("{a}: {e}"));
            }
        }
    }

    #[test]
    fn duplicate_detected() {
        let mut cfg = Config::default();
        cfg.keys.bindings.insert("help".into(), "prefix+z".into());
        let c = check_keys(&cfg);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].binding, "prefix+z");
        assert_eq!(c[0].actions, vec!["help", "zoom"]);
    }

    #[test]
    fn normalisation_catches_spelling_variants() {
        let mut cfg = Config::default();
        cfg.keys
            .bindings
            .insert("help".into(), "prefix+Shift+R".into());
        let c = check_keys(&cfg);
        assert!(c.iter().any(|c| c.actions == vec!["help", "reload_config"]));
    }

    #[test]
    fn range_overlap_detected() {
        let mut cfg = Config::default();
        cfg.keys.bindings.insert("help".into(), "prefix+3".into());
        let c = check_keys(&cfg);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].binding, "prefix+3");
    }

    #[test]
    fn unbound_is_ignored_and_sequences_shadow() {
        let mut cfg = Config::default();
        cfg.keys.bindings.insert("help".into(), String::new());
        cfg.keys.bindings.insert("settings".into(), String::new());
        assert!(check_keys(&cfg).is_empty());
        cfg.keys.bindings.insert("help".into(), "prefix+g w".into());
        let c = check_keys(&cfg);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].reason, ConflictReason::ShadowedSequence);
        assert_eq!(c[0].binding, "prefix+g");
    }

    #[test]
    fn custom_commands_participate() {
        let mut cfg = Config::default();
        cfg.keys.command.push(crate::types::KeyCommand {
            key: "prefix+z".into(),
            command: "lazygit".into(),
            ..Default::default()
        });
        let c = check_keys(&cfg);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].actions, vec!["command[0]", "zoom"]);
    }
}
