//! `plugins.enabled` in Hermes Agent's `config.yaml`, edited line by line so the user's comments,
//! key order and other settings stay as they are. Handles the block form Hermes writes itself
//! (`plugins:` / `  enabled:` / `    - name`) and an inline flow list (`  enabled: [a, b]`).
//! A plugin listed in `plugins.disabled` is taken out of that list on enable.

/// Whether [`set_enabled`] can edit `content`: `plugins` is missing or a block mapping, and its
/// `enabled`/`disabled` values are block lists, flow lists or empty. Anything else (a flow
/// mapping such as `plugins: {}`, a scalar) is left for the user to edit.
pub fn supported(content: &str) -> bool {
    let lines: Vec<String> = content.lines().map(str::to_string).collect();
    let Some(plugins) = lines.iter().position(|l| top_key(l) == Some("plugins")) else {
        return true;
    };
    if !value_of(&lines[plugins]).is_empty() {
        return false;
    }
    let end = section_end(&lines, plugins);
    (plugins + 1..end).all(|i| match sub_key(&lines[i]) {
        Some("enabled" | "disabled") => {
            value_of(&lines[i]).is_empty() || flow_items(&lines[i]).is_some()
        }
        _ => true,
    })
}

/// The inline value of a `key: value` line, without a comment.
fn value_of(line: &str) -> &str {
    strip_comment(line)
        .split_once(':')
        .map_or("", |(_, v)| v.trim())
}

/// `content` with `name` added to (or removed from) `plugins.enabled`.
pub fn set_enabled(content: &str, name: &str, enabled: bool) -> String {
    let newline = content.ends_with('\n') || content.is_empty();
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
    let Some(plugins) = lines.iter().position(|l| top_key(l) == Some("plugins")) else {
        if !enabled {
            return content.to_string();
        }
        let mut out = content.trim_end_matches('\n').to_string();
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("plugins:\n  enabled:\n    - {name}\n"));
        return out;
    };
    if enabled {
        remove_item(&mut lines, plugins, "disabled", name);
    }
    let end = section_end(&lines, plugins);
    let enabled_at = (plugins + 1..end).find(|&i| sub_key(&lines[i]) == Some("enabled"));
    match enabled_at {
        Some(i) => {
            if let Some(mut items) = flow_items(&lines[i]) {
                let has = items.iter().any(|x| x == name);
                if has == enabled {
                    return join(lines, newline);
                }
                if enabled {
                    items.push(name.to_string());
                } else {
                    items.retain(|x| x != name);
                }
                lines[i] = format!("{}enabled: [{}]", indent_of(&lines[i]), items.join(", "));
            } else if enabled {
                if !list_items(&lines, i).any(|(_, v)| v == name) {
                    let at = list_items(&lines, i).last().map_or(i + 1, |(j, _)| j + 1);
                    let item_indent = list_items(&lines, i)
                        .next()
                        .map(|(j, _)| indent_of(&lines[j]).to_string())
                        .unwrap_or_else(|| format!("{}  ", indent_of(&lines[i])));
                    lines.insert(at, format!("{item_indent}- {name}"));
                }
            } else {
                remove_item(&mut lines, plugins, "enabled", name);
            }
        }
        None if enabled => {
            lines.insert(plugins + 1, "  enabled:".to_string());
            lines.insert(plugins + 2, format!("    - {name}"));
        }
        None => {}
    }
    join(lines, newline)
}

/// Whether `name` is in `plugins.enabled`.
pub fn is_enabled(content: &str, name: &str) -> bool {
    let lines: Vec<String> = content.lines().map(str::to_string).collect();
    let Some(plugins) = lines.iter().position(|l| top_key(l) == Some("plugins")) else {
        return false;
    };
    let end = section_end(&lines, plugins);
    let Some(i) = (plugins + 1..end).find(|&i| sub_key(&lines[i]) == Some("enabled")) else {
        return false;
    };
    match flow_items(&lines[i]) {
        Some(items) => items.iter().any(|x| x == name),
        None => list_items(&lines, i).any(|(_, v)| v == name),
    }
}

fn remove_item(lines: &mut Vec<String>, plugins: usize, key: &str, name: &str) {
    let end = section_end(lines, plugins);
    let Some(i) = (plugins + 1..end).find(|&i| sub_key(&lines[i]) == Some(key)) else {
        return;
    };
    if let Some(mut items) = flow_items(&lines[i]) {
        if items.iter().any(|x| x == name) {
            items.retain(|x| x != name);
            lines[i] = format!("{}{key}: [{}]", indent_of(&lines[i]), items.join(", "));
        }
        return;
    }
    let hits: Vec<usize> = list_items(lines, i)
        .filter(|(_, v)| v == name)
        .map(|(j, _)| j)
        .collect();
    if hits.is_empty() {
        return;
    }
    for j in hits.into_iter().rev() {
        lines.remove(j);
    }
    // An emptied block list stays a list.
    if list_items(lines, i).next().is_none() {
        lines[i] = format!("{}{key}: []", indent_of(&lines[i]));
    }
}

fn join(lines: Vec<String>, newline: bool) -> String {
    let mut s = lines.join("\n");
    if newline {
        s.push('\n');
    }
    s
}

fn indent_of(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

fn strip_comment(s: &str) -> &str {
    match s.find(" #") {
        Some(i) => &s[..i],
        None if s.starts_with('#') => "",
        None => s,
    }
}

fn unquote(s: &str) -> &str {
    let s = s.trim();
    s.strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')))
        .unwrap_or(s)
}

/// The key of a top-level `key:` line.
fn top_key(line: &str) -> Option<&str> {
    if line.starts_with([' ', '\t', '#', '-']) {
        return None;
    }
    let (k, _) = strip_comment(line).split_once(':')?;
    Some(k.trim())
}

/// The key of an indented `  key:` line inside a mapping.
fn sub_key(line: &str) -> Option<&str> {
    let t = line.trim_start();
    if t.len() == line.len() || t.starts_with(['#', '-']) {
        return None;
    }
    let (k, _) = strip_comment(t).split_once(':')?;
    Some(k.trim())
}

/// The end (exclusive) of the top-level section that starts at `start`.
fn section_end(lines: &[String], start: usize) -> usize {
    (start + 1..lines.len())
        .find(|&i| {
            let l = &lines[i];
            !l.trim().is_empty() && !l.starts_with([' ', '\t', '#']) && !l.starts_with('-')
        })
        .unwrap_or(lines.len())
}

/// Items of an inline `key: [a, b]` list; `None` when the value is not a flow list.
fn flow_items(line: &str) -> Option<Vec<String>> {
    let (_, v) = strip_comment(line).split_once(':')?;
    let v = v.trim().strip_prefix('[')?.strip_suffix(']')?;
    Some(
        v.split(',')
            .map(unquote)
            .filter(|x| !x.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// `(line index, value)` of the `- item` lines under the key at `key_line`.
fn list_items(lines: &[String], key_line: usize) -> impl Iterator<Item = (usize, String)> + '_ {
    let key_indent = indent_of(&lines[key_line]).len();
    lines[key_line + 1..]
        .iter()
        .enumerate()
        .take_while(move |(_, l)| {
            let t = l.trim_start();
            t.is_empty()
                || t.starts_with('#')
                || (t.starts_with('-') && indent_of(l).len() >= key_indent)
        })
        .filter_map(move |(o, l)| {
            let t = strip_comment(l.trim_start());
            let v = t.strip_prefix('-')?;
            Some((key_line + 1 + o, unquote(v).to_string()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: &str = "model: x\nplugins:\n  enabled:\n    - herdr-agent-state # keep\n    - openviking\n  disabled: []\nhooks_auto_accept: false\n";

    #[test]
    fn adds_to_a_block_list_and_removes_again() {
        let on = set_enabled(BLOCK, "vibeke", true);
        assert_eq!(
            on,
            "model: x\nplugins:\n  enabled:\n    - herdr-agent-state # keep\n    - openviking\n    - vibeke\n  disabled: []\nhooks_auto_accept: false\n"
        );
        assert!(is_enabled(&on, "vibeke"));
        assert_eq!(set_enabled(&on, "vibeke", true), on);
        assert_eq!(set_enabled(&on, "vibeke", false), BLOCK);
        assert_eq!(set_enabled(BLOCK, "vibeke", false), BLOCK);
    }

    #[test]
    fn handles_flow_lists_missing_keys_and_disabled_entries() {
        let flow = "plugins:\n  enabled: [a, 'b']\n";
        let on = set_enabled(flow, "vibeke", true);
        assert_eq!(on, "plugins:\n  enabled: [a, b, vibeke]\n");
        assert_eq!(
            set_enabled(&on, "vibeke", false),
            "plugins:\n  enabled: [a, b]\n"
        );
        assert_eq!(
            set_enabled("model: x\n", "vibeke", true),
            "model: x\nplugins:\n  enabled:\n    - vibeke\n"
        );
        assert_eq!(
            set_enabled(
                "plugins:\n  disabled:\n    - vibeke\nother: 1\n",
                "vibeke",
                true
            ),
            "plugins:\n  enabled:\n    - vibeke\n  disabled: []\nother: 1\n"
        );
        assert_eq!(
            set_enabled("plugins:\n  enabled: []\n", "vibeke", true),
            "plugins:\n  enabled: [vibeke]\n"
        );
        assert!(!is_enabled(
            "plugins:\n  disabled:\n    - vibeke\n",
            "vibeke"
        ));
    }

    #[test]
    fn refuses_layouts_it_cannot_edit() {
        assert!(supported(BLOCK));
        assert!(supported("model: x\n"));
        assert!(supported("plugins:\n  enabled: [a]\n"));
        assert!(!supported("plugins: {}\n"));
        assert!(!supported("plugins: {enabled: [a]}\n"));
        assert!(!supported("plugins:\n  enabled: a\n"));
    }
}
