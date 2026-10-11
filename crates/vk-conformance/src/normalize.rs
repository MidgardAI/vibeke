//! The per-scenario normalizer (18 §11). Stricter than the CLI differential normalizer: only
//! the fields a scenario names, plus the run's temp roots, are replaced. Stable plugin and
//! action ids are compared exactly.

use serde_json::Value;

pub struct Normalizer {
    /// Object keys whose string values become first-seen tokens `<field:N>` (a bijection per
    /// side, so equal token sequences mean the ids correspond one to one).
    fields: Vec<String>,
    /// Temp roots replaced by `<root>` inside strings.
    roots: Vec<String>,
    /// Timestamps: keys ending in `_at` or `_ms` with numeric values become `<n>`.
    times: bool,
}

impl Normalizer {
    pub fn new(fields: &[String], roots: &[String]) -> Self {
        todo!("lane MODEL")
    }

    pub fn value(&mut self, v: &Value) -> Value {
        todo!("lane MODEL")
    }

    pub fn text(&mut self, s: &str) -> String {
        todo!("lane MODEL")
    }
}
