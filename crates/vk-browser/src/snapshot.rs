//! Text snapshots of a page for agents (`browser.snapshot`): the accessibility tree from
//! `Accessibility.getFullAXTree` as an indented outline (`role "name" [value] {states}`), with
//! ignored and nameless generic nodes folded away, bounded in size.

use serde_json::Value;
use std::collections::HashMap;

/// Roles that only add nesting noise when they carry no name.
const QUIET: &[&str] = &[
    "generic",
    "none",
    "presentation",
    "InlineTextBox",
    "LineBreak",
    "StaticText",
    "group",
    "Section",
    "paragraph",
    "div",
];

fn ax_str(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.get("value"))
        .map(|x| match x {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        })
        .unwrap_or_default()
}

/// Format `Accessibility.getFullAXTree` nodes. Returns (text, truncated).
pub fn format_ax(nodes: &[Value], max_bytes: usize) -> (String, bool) {
    let by_id: HashMap<&str, &Value> = nodes
        .iter()
        .filter_map(|n| Some((n.get("nodeId")?.as_str()?, n)))
        .collect();
    let roots: Vec<&Value> = nodes
        .iter()
        .filter(|n| {
            n.get("parentId")
                .and_then(Value::as_str)
                .is_none_or(|p| !by_id.contains_key(p))
        })
        .collect();
    let mut out = String::new();
    let mut truncated = false;
    let mut stack: Vec<(&Value, usize)> = roots.into_iter().rev().map(|r| (r, 0)).collect();
    while let Some((n, depth)) = stack.pop() {
        let role = ax_str(n, "role");
        let name = ax_str(n, "name");
        let value = ax_str(n, "value");
        let ignored = n.get("ignored").and_then(Value::as_bool).unwrap_or(false);
        let quiet = ignored || (name.trim().is_empty() && QUIET.contains(&role.as_str()));
        // StaticText repeats its parent's name; show it only when the parent didn't.
        let child_depth = if quiet { depth } else { depth + 1 };
        if !quiet {
            let mut line = format!("{}{role}", "  ".repeat(depth));
            if !name.is_empty() {
                let name: String = name.chars().take(200).collect();
                line.push_str(&format!(" {name:?}"));
            }
            if !value.is_empty() {
                let value: String = value.chars().take(200).collect();
                line.push_str(&format!(" [{value}]"));
            }
            let mut states = Vec::new();
            if let Some(props) = n.get("properties").and_then(Value::as_array) {
                for p in props {
                    let pname = p.get("name").and_then(Value::as_str).unwrap_or("");
                    let pv = p.get("value").and_then(|v| v.get("value"));
                    match (pname, pv) {
                        (
                            "focused" | "disabled" | "checked" | "selected" | "expanded"
                            | "required" | "invalid",
                            Some(Value::Bool(true)),
                        ) => states.push(pname.to_string()),
                        ("checked", Some(Value::String(s))) if s == "true" || s == "mixed" => {
                            states.push(format!("checked={s}"))
                        }
                        ("level", Some(v)) => states.push(format!("level={v}")),
                        _ => {}
                    }
                }
            }
            if !states.is_empty() {
                line.push_str(&format!(" {{{}}}", states.join(", ")));
            }
            if out.len() + line.len() + 1 > max_bytes {
                truncated = true;
                break;
            }
            out.push_str(&line);
            out.push('\n');
        }
        if let Some(kids) = n.get("childIds").and_then(Value::as_array) {
            for k in kids.iter().rev() {
                if let Some(c) = k.as_str().and_then(|id| by_id.get(id)) {
                    // Skip StaticText children that only repeat the parent's name.
                    if ax_str(c, "role") == "StaticText" && ax_str(c, "name") == name {
                        continue;
                    }
                    stack.push((c, child_depth));
                }
            }
        }
    }
    (out, truncated)
}

/// Truncate `s` to at most `max` bytes on a char boundary. Returns (text, truncated).
pub fn bound(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn outline_folds_noise() {
        let nodes = vec![
            json!({"nodeId": "1", "role": {"value": "RootWebArea"}, "name": {"value": "Shop"}, "childIds": ["2", "5"]}),
            json!({"nodeId": "2", "parentId": "1", "role": {"value": "generic"}, "name": {"value": ""}, "childIds": ["3", "4"]}),
            json!({"nodeId": "3", "parentId": "2", "role": {"value": "heading"}, "name": {"value": "Cart"},
                   "properties": [{"name": "level", "value": {"type": "integer", "value": 1}}], "childIds": ["6"]}),
            json!({"nodeId": "6", "parentId": "3", "role": {"value": "StaticText"}, "name": {"value": "Cart"}, "childIds": []}),
            json!({"nodeId": "4", "parentId": "2", "role": {"value": "textbox"}, "name": {"value": "Qty"}, "value": {"value": "2"},
                   "properties": [{"name": "focused", "value": {"type": "boolean", "value": true}}], "childIds": []}),
            json!({"nodeId": "5", "parentId": "1", "ignored": true, "role": {"value": "button"}, "childIds": ["7"]}),
            json!({"nodeId": "7", "parentId": "5", "role": {"value": "button"}, "name": {"value": "Checkout"}, "childIds": []}),
        ];
        let (t, trunc) = format_ax(&nodes, 10_000);
        assert!(!trunc);
        assert_eq!(
            t,
            "RootWebArea \"Shop\"\n  heading \"Cart\" {level=1}\n  textbox \"Qty\" [2] {focused}\n  button \"Checkout\"\n"
        );
        let (t, trunc) = format_ax(&nodes, 30);
        assert!(trunc);
        assert!(t.len() <= 30);
        assert_eq!(bound("æøå", 3), ("æ".to_string(), true));
        assert_eq!(bound("abc", 3), ("abc".to_string(), false));
    }
}
