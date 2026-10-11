//! The per-scenario normalizer (18 §11). Stricter than the CLI differential normalizer: only
//! the fields a scenario names, plus the run's temp roots, are replaced. Stable plugin and
//! action ids are compared exactly.

use serde_json::Value;
use std::collections::BTreeMap;

pub struct Normalizer {
    /// Object keys whose string values become first-seen tokens `<field:N>` (a bijection per
    /// side, so equal token sequences mean the ids correspond one to one).
    fields: Vec<String>,
    /// Temp roots replaced by `<root>` inside strings.
    roots: Vec<String>,
    /// Timestamps: keys ending in `_at` or `_ms` with numeric values become `<n>`.
    times: bool,
    /// Per field, the values in the order they were first seen; the token number is the
    /// position plus one.
    seen: BTreeMap<String, Vec<String>>,
}

impl Normalizer {
    /// Use one normalizer per side, so each side numbers its own ids from 1.
    pub fn new(fields: &[String], roots: &[String]) -> Self {
        let mut roots: Vec<String> = roots.iter().filter(|r| !r.is_empty()).cloned().collect();
        // Longest first, so a root inside another root does not leave a partial path.
        roots.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        roots.dedup();
        Normalizer {
            fields: fields.to_vec(),
            roots,
            times: true,
            seen: BTreeMap::new(),
        }
    }

    /// Normalize a JSON value: named fields become tokens, timestamps become `<n>`, and temp
    /// roots inside any string, at any depth, become `<root>`.
    pub fn value(&mut self, v: &Value) -> Value {
        match v {
            Value::Object(map) => {
                let mut out = serde_json::Map::with_capacity(map.len());
                for (k, x) in map {
                    let nx = if self.fields.iter().any(|f| f == k) {
                        self.field(k, x)
                    } else if self.times
                        && (k.ends_with("_at") || k.ends_with("_ms"))
                        && x.is_number()
                    {
                        Value::String("<n>".into())
                    } else {
                        self.value(x)
                    };
                    out.insert(k.clone(), nx);
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(items.iter().map(|x| self.value(x)).collect()),
            Value::String(s) => Value::String(self.text(s)),
            Value::Null | Value::Bool(_) | Value::Number(_) => v.clone(),
        }
    }

    /// Replace the temp roots in `s` with `<root>`.
    pub fn text(&mut self, s: &str) -> String {
        let mut out = s.to_string();
        for r in &self.roots {
            if out.contains(r.as_str()) {
                out = out.replace(r.as_str(), "<root>");
            }
        }
        out
    }

    /// The value of a named field: strings and numbers become tokens, lists are tokenized
    /// item by item, and anything else is normalized as usual.
    fn field(&mut self, name: &str, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.token(name, s)),
            Value::Number(n) => Value::String(self.token(name, &n.to_string())),
            Value::Array(items) => {
                Value::Array(items.iter().map(|x| self.field(name, x)).collect())
            }
            _ => self.value(v),
        }
    }

    fn token(&mut self, name: &str, raw: &str) -> String {
        let seen = self.seen.entry(name.to_string()).or_default();
        let n = match seen.iter().position(|x| x == raw) {
            Some(i) => i + 1,
            None => {
                seen.push(raw.to_string());
                seen.len()
            }
        };
        format!("<{name}:{n}>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn n(fields: &[&str], roots: &[&str]) -> Normalizer {
        let f: Vec<String> = fields.iter().map(|s| s.to_string()).collect();
        let r: Vec<String> = roots.iter().map(|s| s.to_string()).collect();
        Normalizer::new(&f, &r)
    }

    #[test]
    fn fields_become_first_seen_tokens() {
        let mut a = n(&["project_id"], &[]);
        let v = a.value(&json!({
            "project_id": "p-9",
            "items": [{"project_id": "p-3"}, {"project_id": "p-9"}],
            "plugin_id": "acme.demo",
            "action_id": "open",
        }));
        assert_eq!(
            v,
            json!({
                "project_id": "<project_id:1>",
                "items": [{"project_id": "<project_id:2>"}, {"project_id": "<project_id:1>"}],
                "plugin_id": "acme.demo",
                "action_id": "open",
            })
        );
    }

    #[test]
    fn tokens_are_a_bijection_per_side() {
        // Different raw ids in the same pattern give equal output.
        let mut vibeke = n(&["id"], &[]);
        let mut herdr = n(&["id"], &[]);
        let a = vibeke.value(&json!([{"id": "x"}, {"id": "y"}, {"id": "x"}]));
        let b = herdr.value(&json!([{"id": 7}, {"id": 8}, {"id": 7}]));
        assert_eq!(a, b);
        // A different pattern does not.
        let mut other = n(&["id"], &[]);
        let c = other.value(&json!([{"id": 7}, {"id": 7}, {"id": 8}]));
        assert_ne!(a, c);
        // Fields are numbered separately.
        let mut two = n(&["a", "b"], &[]);
        assert_eq!(
            two.value(&json!({"a": "same", "b": "same"})),
            json!({"a": "<a:1>", "b": "<b:1>"})
        );
        // Lists under a named field are tokenized item by item.
        assert_eq!(
            two.value(&json!({"a": ["same", "new"]})),
            json!({"a": ["<a:1>", "<a:2>"]})
        );
    }

    #[test]
    fn roots_and_times() {
        let mut x = n(&[], &["/tmp/run", "/tmp/run/home", ""]);
        let v = x.value(&json!({
            "path": "/tmp/run/home/.config/x",
            "nested": {"argv": ["cd", "/tmp/run/state", "/elsewhere"]},
            "created_at": 1760000000,
            "duration_ms": 12,
            "started_at": "2026-10-11T10:00:00Z",
            "count": 3,
        }));
        assert_eq!(
            v,
            json!({
                "path": "<root>/.config/x",
                "nested": {"argv": ["cd", "<root>/state", "/elsewhere"]},
                "created_at": "<n>",
                "duration_ms": "<n>",
                "started_at": "2026-10-11T10:00:00Z",
                "count": 3,
            })
        );
        assert_eq!(
            x.text("log: /tmp/run/home/a and /tmp/run/b"),
            "log: <root>/a and <root>/b"
        );
        assert_eq!(x.text("nothing here"), "nothing here");
    }
}
