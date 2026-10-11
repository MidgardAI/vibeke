//! `herdr config check` and `herdr --default-config` for the CLI shim.
//!
//! Herdr 0.9.3 checks its `config.toml` locally, without a server (`src/cli.rs` `config_check`,
//! `src/config/io.rs` and `src/config/keybinds.rs`): `config: ok` and exit 0, or `config: issues
//! found` with one diagnostic per line and exit 1. This module repeats the checks that decide
//! whether a plugin may write a config: TOML syntax, unknown top-level sections and keys, the
//! types and unknown keys of `[keys]`, `[keys.indexed]` and `[[keys.command]]`, and the syntax of
//! every keybinding. It does not repeat Herdr's keybinding conflict checks or the value checks of
//! the other sections (theme, sounds, tab bar, window title, sidebar bounds), which only produce
//! warnings for files that Herdr itself loads.

use std::path::{Path, PathBuf};

use toml::Value;

/// `herdr --default-config` of the baseline, as captured.
pub const DEFAULT_CONFIG: &str =
    include_str!("../../../../tests/compat/herdr/0.9.3/default-config.toml");

const KNOWN_TOP_LEVEL: &[&str] = &[
    "advanced",
    "experimental",
    "keys",
    "onboarding",
    "remote",
    "server",
    "session",
    "terminal",
    "theme",
    "ui",
    "update",
    "worktrees",
];

/// Navigate-mode bindings (no prefix, no `esc`), in Herdr's validation order.
const NAVIGATE_FIELDS: &[&str] = &[
    "navigate_workspace_up",
    "navigate_workspace_down",
    "navigate_pane_left",
    "navigate_pane_down",
    "navigate_pane_up",
    "navigate_pane_right",
];

/// Action bindings in Herdr's validation order; `true` marks the indexed (`1..9`) actions.
const ACTION_FIELDS: &[(&str, bool)] = &[
    ("help", false),
    ("settings", false),
    ("new_workspace", false),
    ("new_worktree", false),
    ("open_worktree", false),
    ("remove_worktree", false),
    ("rename_workspace", false),
    ("close_workspace", false),
    ("workspace_picker", false),
    ("goto", false),
    ("detach", false),
    ("reload_config", false),
    ("open_notification_target", false),
    ("previous_workspace", false),
    ("next_workspace", false),
    ("previous_agent", false),
    ("next_agent", false),
    ("focus_agent", true),
    ("new_tab", false),
    ("rename_tab", false),
    ("previous_tab", false),
    ("next_tab", false),
    ("move_tab_previous", false),
    ("move_tab_next", false),
    ("switch_tab", true),
    ("switch_workspace", true),
    ("close_tab", false),
    ("rename_pane", false),
    ("edit_scrollback", false),
    ("clear_pane", false),
    ("copy_mode", false),
    ("focus_pane_left", false),
    ("focus_pane_down", false),
    ("focus_pane_up", false),
    ("focus_pane_right", false),
    ("swap_pane_left", false),
    ("swap_pane_down", false),
    ("swap_pane_up", false),
    ("swap_pane_right", false),
    ("last_pane", false),
    ("cycle_pane_next", false),
    ("cycle_pane_previous", false),
    ("split_vertical", false),
    ("split_horizontal", false),
    ("close_pane", false),
    ("zoom", false),
    ("resize_mode", false),
    ("resize_pane_left", false),
    ("resize_pane_down", false),
    ("resize_pane_up", false),
    ("resize_pane_right", false),
    ("toggle_sidebar", false),
];

/// Herdr's config file: `HERDR_CONFIG_PATH`, else `$XDG_CONFIG_HOME/herdr/config.toml`, else
/// `~/.config/herdr/config.toml`.
pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("HERDR_CONFIG_PATH") {
        return PathBuf::from(p);
    }
    let dir = match (std::env::var("XDG_CONFIG_HOME"), std::env::var("HOME")) {
        (Ok(x), _) => PathBuf::from(x).join("herdr"),
        (_, Ok(h)) => PathBuf::from(h).join(".config/herdr"),
        _ => std::env::temp_dir().join("herdr"),
    };
    dir.join("config.toml")
}

/// The update channel Herdr's config selects (`[update] channel`): `preview` or `stable`.
pub fn update_channel() -> String {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|t| t.parse::<toml::Table>().ok())
        .and_then(|t| {
            t.get("update")?
                .get("channel")?
                .as_str()
                .map(str::to_string)
        })
        .filter(|c| c == "preview")
        .unwrap_or_else(|| "stable".into())
}

/// The diagnostics `herdr config check` prints for the file at `path`; empty when it is fine
/// (a missing file is fine: Herdr uses its defaults).
pub fn check_file(path: &Path) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => check(text.strip_prefix('\u{feff}').unwrap_or(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => vec![format!("config read error: {e}; using defaults")],
    }
}

/// `herdr config check` output and exit code for a list of diagnostics.
pub fn report(diagnostics: &[String]) -> (String, i32) {
    if diagnostics.is_empty() {
        return ("config: ok\n".into(), 0);
    }
    let mut out = String::from("config: issues found\n");
    for d in diagnostics {
        out.push_str(d);
        out.push('\n');
    }
    (out, 1)
}

/// The diagnostics for a config text.
pub fn check(text: &str) -> Vec<String> {
    let table: toml::Table = match text.parse::<toml::Table>() {
        Ok(t) => t,
        Err(e) => return vec![format!("config parse error: {e}; using defaults")],
    };
    // A wrong type anywhere Herdr deserializes fails the whole file.
    let keys = table.get("keys");
    if let Some(e) = keys.and_then(|k| keys_type_error(k).err()) {
        return vec![format!("config parse error: {e}; using defaults")];
    }
    if let Some(v) = table.get("onboarding")
        && !v.is_bool()
    {
        return vec![
            "config parse error: invalid type for onboarding: expected a boolean; using defaults"
                .to_string(),
        ];
    }
    let mut out = Vec::new();
    let mut unknown_keys: Vec<Vec<String>> = Vec::new();
    for (k, v) in &table {
        if KNOWN_TOP_LEVEL.contains(&k.as_str()) {
            continue;
        }
        let header = if v.is_table() {
            Some(format!("[{k}]"))
        } else if v
            .as_array()
            .is_some_and(|a| !a.is_empty() && a.iter().all(Value::is_table))
        {
            Some(format!("[[{k}]]"))
        } else {
            None
        };
        match header {
            Some(h) if k == "toast" => out.push(format!(
                "unknown config section {h}; did you mean [ui.toast]? ignoring section"
            )),
            Some(h) => out.push(format!("unknown config section {h}; ignoring section")),
            None => unknown_keys.push(vec![k.clone()]),
        }
    }
    if let Some(Value::Table(keys)) = keys {
        unknown_keys.extend(unknown_keys_in_keys(keys));
    }
    unknown_keys.sort();
    unknown_keys.dedup();
    for path in unknown_keys {
        out.push(format!(
            "unknown config key {}; ignoring key",
            format_key_path(&path)
        ));
    }
    if let Some(Value::Table(keys)) = keys {
        out.extend(keybinding_diagnostics(keys));
    }
    out
}

fn format_key_path(path: &[String]) -> String {
    path.iter()
        .map(|seg| {
            let plain = !seg.is_empty()
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'));
            if plain || seg.parse::<usize>().is_ok() {
                seg.clone()
            } else {
                Value::String(seg.clone()).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

fn is_binding(v: &Value) -> bool {
    v.is_str() || v.as_array().is_some_and(|a| a.iter().all(Value::is_str))
}

fn known_keys_field(k: &str) -> bool {
    matches!(
        k,
        "prefix" | "extra_prefixes" | "remote_image_paste" | "indexed" | "command" | "fullscreen"
    ) || NAVIGATE_FIELDS.contains(&k)
        || ACTION_FIELDS.iter().any(|(f, _)| *f == k)
}

/// Type errors in `[keys]` (Herdr fails to load the whole file).
fn keys_type_error(keys: &Value) -> Result<(), String> {
    let Some(keys) = keys.as_table() else {
        return Err("invalid type for keys: expected a table".into());
    };
    for (k, v) in keys {
        let ok = match k.as_str() {
            "remote_image_paste" => v.is_str(),
            "indexed" => v.as_table().is_some_and(|t| {
                ["tabs", "workspaces", "agents"]
                    .iter()
                    .all(|f| t.get(*f).is_none_or(Value::is_str))
            }),
            "command" => {
                let Some(cmds) = v.as_array() else {
                    return Err("invalid type for keys.command: expected an array of tables".into());
                };
                for (i, c) in cmds.iter().enumerate() {
                    command_type_error(c).map_err(|e| format!("keys.command[{i}]: {e}"))?;
                }
                true
            }
            f if known_keys_field(f) => is_binding(v),
            _ => true,
        };
        if !ok {
            return Err(format!("invalid type for keys.{k}"));
        }
    }
    Ok(())
}

fn command_type_error(c: &Value) -> Result<(), String> {
    let Some(t) = c.as_table() else {
        return Err("expected a table".into());
    };
    for (k, v) in t {
        let ok = match k.as_str() {
            "key" => is_binding(v),
            "command" => v.is_str(),
            "description" => v.is_str(),
            "type" => matches!(
                v.as_str(),
                Some("shell" | "pane" | "popup" | "plugin_action")
            ),
            "width" | "height" => match v {
                Value::Integer(n) => (0..=i64::from(u16::MAX)).contains(n),
                Value::String(s) => s
                    .strip_suffix('%')
                    .and_then(|p| p.parse::<u8>().ok())
                    .is_some_and(|p| (1..=100).contains(&p)),
                _ => false,
            },
            _ => true,
        };
        if !ok {
            return Err(format!("invalid value for {k}"));
        }
    }
    Ok(())
}

fn unknown_keys_in_keys(keys: &toml::Table) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let path = |rest: &[&str]| {
        std::iter::once("keys")
            .chain(rest.iter().copied())
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    for (k, v) in keys {
        if !known_keys_field(k) {
            out.push(path(&[k.as_str()]));
            continue;
        }
        match (k.as_str(), v) {
            ("indexed", Value::Table(t)) => {
                for f in t.keys() {
                    if !matches!(f.as_str(), "tabs" | "workspaces" | "agents") {
                        out.push(path(&["indexed", f.as_str()]));
                    }
                }
            }
            ("command", Value::Array(cmds)) => {
                for (i, c) in cmds.iter().enumerate() {
                    for f in c.as_table().into_iter().flat_map(|t| t.keys()) {
                        if !matches!(
                            f.as_str(),
                            "key" | "command" | "type" | "description" | "width" | "height"
                        ) {
                            out.push(path(&["command", i.to_string().as_str(), f.as_str()]));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// A binding's values (`"x"` or `["x", "y"]`).
fn values(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// What the checks need to know about a parsed key: `esc`, a digit `1`..`9`, or another key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Esc,
    Digit,
    Other,
}

fn modifier(token: &str) -> bool {
    matches!(
        token.to_lowercase().as_str(),
        "ctrl"
            | "control"
            | "shift"
            | "alt"
            | "option"
            | "meta"
            | "cmd"
            | "command"
            | "super"
            | "hyper"
    )
}

/// Herdr's `parse_key_combo` (validity and the key code only).
fn parse_key_combo(s: &str) -> Option<Key> {
    let mut key: Option<&str> = None;
    for part in s.split('+') {
        let t = part.trim();
        if t.is_empty() {
            return None;
        }
        if modifier(t) {
            continue;
        }
        if key.is_some() {
            return None;
        }
        key = Some(t);
    }
    let key = key?;
    let mut chars = key.chars();
    let single = match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    };
    let lower = key.to_lowercase();
    let named = [
        "space",
        " ",
        "enter",
        "return",
        "tab",
        "backspace",
        "bs",
        "left",
        "right",
        "up",
        "down",
        "minus",
        "comma",
        "period",
        "slash",
        "backslash",
        "quote",
        "double_quote",
        "double-quote",
        "semicolon",
        "colon",
        "percent",
        "ampersand",
        "backtick",
        "plus",
    ];
    Some(match lower.as_str() {
        "esc" | "escape" => Key::Esc,
        s if named.contains(&s) => Key::Other,
        _ if single.is_some() => match single? {
            '1'..='9' => Key::Digit,
            _ => Key::Other,
        },
        s if s.starts_with('f') => {
            s[1..].parse::<u8>().ok()?;
            Key::Other
        }
        _ => return None,
    })
}

/// `1..9` with modifiers only (an indexed range binding).
fn is_range(body: &str) -> bool {
    let mut saw = false;
    for part in body.split('+') {
        let t = part.trim();
        if t == "1..9" {
            if saw {
                return false;
            }
            saw = true;
        } else if !modifier(t) {
            return false;
        }
    }
    saw
}

/// Modifiers only, at least one (a legacy `[keys.indexed]` value).
fn is_modifier_combo(s: &str) -> bool {
    !s.is_empty()
        && s.split('+').all(|p| {
            let t = p.trim();
            !t.is_empty() && modifier(t)
        })
}

enum Parsed {
    Single { prefix: bool, key: Key },
    Range,
}

/// Herdr's `parse_binding_string`.
fn parse_binding(raw: &str) -> Option<Parsed> {
    let t = raw.trim();
    let (prefix, body) = match t.strip_prefix("prefix+") {
        Some(rest) => (true, rest),
        None => (false, t),
    };
    if is_range(body) {
        return Some(Parsed::Range);
    }
    parse_key_combo(body).map(|key| Parsed::Single { prefix, key })
}

/// Syntax diagnostics for one binding field (Herdr's `parse_*_bindings`).
fn binding_diagnostics(field: &str, raws: &[String], indexed: bool, navigate: bool) -> Vec<String> {
    let mut out = Vec::new();
    for raw in raws {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        match parse_binding(raw) {
            None => out.push(format!(
                "invalid keybinding: {field} = {raw:?}; disabling binding"
            )),
            Some(Parsed::Range) if !indexed => out.push(format!(
                "range keybinding is only valid for indexed actions: {field} = {raw:?}; disabling binding"
            )),
            Some(Parsed::Range) => {}
            Some(Parsed::Single { prefix, key }) => {
                if navigate && prefix {
                    out.push(format!(
                        "navigate keybinding must not include prefix: {field} = {raw:?}; disabling binding"
                    ));
                } else if navigate && key == Key::Esc {
                    out.push(format!(
                        "navigate keybinding cannot use esc: {field} = {raw:?}; disabling binding"
                    ));
                } else if indexed && key != Key::Digit {
                    out.push(format!(
                        "indexed keybinding must use 1..9: {field} = {raw:?}; disabling binding"
                    ));
                }
            }
        }
    }
    out
}

/// Herdr's keybinding diagnostics for the user's `[keys]`, in Herdr's order (prefix first).
fn keybinding_diagnostics(keys: &toml::Table) -> Vec<String> {
    let mut out = Vec::new();
    let mut prefix_values = values(keys.get("prefix"));
    prefix_values.extend(values(keys.get("extra_prefixes")));
    if keys.contains_key("prefix") || keys.contains_key("extra_prefixes") {
        let mut entries = Vec::new();
        let mut valid = 0;
        for raw in &prefix_values {
            let raw = raw.trim();
            if parse_key_combo(raw).is_some() {
                valid += 1;
            } else {
                entries.push(format!(
                    "invalid keybinding: keys.prefix = {raw:?}; ignoring prefix"
                ));
            }
        }
        if valid == 0 {
            out.push("invalid keybinding: keys.prefix; using fallback ctrl+b".to_string());
        }
        out.extend(entries);
    }
    for f in NAVIGATE_FIELDS {
        out.extend(binding_diagnostics(
            &format!("keys.{f}"),
            &values(keys.get(*f)),
            false,
            true,
        ));
    }
    for (f, indexed) in ACTION_FIELDS {
        let v = match *f {
            "zoom" => keys.get("zoom").or(keys.get("fullscreen")),
            f => keys.get(f),
        };
        out.extend(binding_diagnostics(
            &format!("keys.{f}"),
            &values(v),
            *indexed,
            false,
        ));
    }
    if let Some(Value::Table(t)) = keys.get("indexed") {
        for f in ["tabs", "workspaces", "agents"] {
            let raw = t.get(f).and_then(Value::as_str).unwrap_or("");
            if !raw.trim().is_empty() && !is_modifier_combo(raw) {
                out.push(format!(
                    "invalid indexed keybinding: keys.indexed.{f} = {raw:?}; disabling binding"
                ));
            }
        }
    }
    if let Some(Value::Array(cmds)) = keys.get("command") {
        for (i, c) in cmds.iter().enumerate() {
            let command = c.get("command").and_then(Value::as_str).unwrap_or("");
            if command.trim().is_empty() {
                out.push(format!(
                    "empty custom command: keys.command[{i}].command; disabling custom command"
                ));
                continue;
            }
            let key_values = values(c.get("key"));
            let diags =
                binding_diagnostics(&format!("keys.command[{i}].key"), &key_values, false, false);
            let usable = key_values.iter().filter(|r| !r.trim().is_empty()).count() > diags.len();
            out.extend(diags);
            if !usable {
                continue;
            }
            let popup = c.get("type").and_then(Value::as_str) == Some("popup");
            if !popup && (c.get("width").is_some() || c.get("height").is_some()) {
                out.push(format!(
                    "popup size on non-popup custom command: keys.command[{i}]; ignoring width and height"
                ));
            }
        }
    }
    if let Some(raw) = keys.get("remote_image_paste").and_then(Value::as_str) {
        let raw = raw.trim();
        if !raw.is_empty() && parse_key_combo(raw).is_none() {
            out.push(format!(
                "invalid keybinding: keys.remote_image_paste = {raw:?}; disabling binding"
            ));
        }
    }
    out
}

#[cfg(test)]
#[path = "cli_config_tests.rs"]
mod tests;
