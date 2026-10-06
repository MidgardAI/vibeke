//! Price table for usage cost (04 §10): USD per million tokens, keyed by model id.
//!
//! Cost uses the harness-reported figure when there is one (pi `cost.total`, Claude stream-json
//! `total_cost_usd`). Otherwise it is computed from this table. The bundled table is compiled
//! in; a newer one arrives signed over the manifest channel (the server verifies the signature
//! before [`PriceTable::parse`] ever sees the bytes). A model that matches no row has no dollar
//! figure: tokens are shown instead, as for subscription-billed runs.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

pub const BUNDLED_JSON: &str = include_str!("../data/prices.json");

/// USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
}

#[derive(Debug, Clone, Deserialize)]
struct Row {
    model: String,
    #[serde(flatten)]
    price: Price,
}

#[derive(Debug, Clone, Deserialize)]
struct File {
    serial: u64,
    #[serde(default)]
    created_at: String,
    prices: Vec<Row>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PriceTable {
    pub serial: u64,
    pub created_at: String,
    /// `(normalised model prefix, price)`, longest prefix first.
    rows: Vec<(String, Price)>,
}

static BUNDLED: LazyLock<PriceTable> =
    LazyLock::new(|| PriceTable::parse(BUNDLED_JSON).expect("bundled price table is valid"));

/// `anthropic/claude-sonnet-4-20250514` and `Claude-Sonnet-4` both become
/// `claude-sonnet-4-20250514` / `claude-sonnet-4`.
pub fn normalize_model(m: &str) -> String {
    let m = m.trim().to_lowercase();
    let m = m.rsplit('/').next().unwrap_or(&m);
    // Bedrock / Vertex decorations: `anthropic.claude-…-v1:0`, `…@20250514`.
    let m = m.strip_prefix("anthropic.").unwrap_or(m);
    m.split('@')
        .next()
        .unwrap_or(m)
        .trim_end_matches(":0")
        .to_string()
}

impl PriceTable {
    pub fn bundled() -> &'static PriceTable {
        &BUNDLED
    }

    pub fn parse(json: &str) -> Result<PriceTable> {
        let f: File = serde_json::from_str(json).context("price table is not valid JSON")?;
        if f.prices.is_empty() {
            bail!("price table has no rows");
        }
        let mut rows = Vec::with_capacity(f.prices.len());
        for r in f.prices {
            let p = r.price;
            if [p.input, p.output, p.cache_read, p.cache_write]
                .iter()
                .any(|x| !x.is_finite() || *x < 0.0)
            {
                bail!("{}: negative or non-finite price", r.model);
            }
            rows.push((normalize_model(&r.model), p));
        }
        rows.sort_by_key(|(m, _)| std::cmp::Reverse(m.len()));
        Ok(PriceTable {
            serial: f.serial,
            created_at: f.created_at,
            rows,
        })
    }

    /// The row whose model id is the longest prefix of `model`.
    pub fn lookup(&self, model: &str) -> Option<(&str, &Price)> {
        let m = normalize_model(model);
        self.rows
            .iter()
            .find(|(p, _)| m.starts_with(p.as_str()))
            .map(|(p, price)| (p.as_str(), price))
    }

    /// Dollar cost of a token mix; `None` for a model without a row.
    pub fn cost(
        &self,
        model: &str,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
    ) -> Option<f64> {
        let (_, p) = self.lookup(model)?;
        let per = |tokens: u64, usd_per_m: f64| tokens as f64 * usd_per_m / 1_000_000.0;
        Some(
            per(input, p.input)
                + per(output, p.output)
                + per(cache_read, p.cache_read)
                + per(cache_write, p.cache_write),
        )
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_table_parses_and_matches_by_longest_prefix() {
        let t = PriceTable::bundled();
        assert!(t.len() >= 10);
        // `gpt-5-mini` must not fall into the broader `gpt-5` row.
        assert_eq!(t.lookup("gpt-5-mini-2026").unwrap().0, "gpt-5-mini");
        assert_eq!(t.lookup("gpt-5-codex").unwrap().0, "gpt-5");
        assert_eq!(
            t.lookup("claude-sonnet-4-20250514").unwrap().0,
            "claude-sonnet-4"
        );
        assert_eq!(
            t.lookup("anthropic/Claude-Opus-4-1").unwrap().0,
            "claude-opus-4"
        );
        assert!(t.lookup("some-local-model").is_none());
    }

    #[test]
    fn cost_is_tokens_times_price_per_million() {
        let t = PriceTable::parse(
            r#"{"serial":2,"prices":[{"model":"m","input":3.0,"output":15.0,"cache_read":0.3,"cache_write":3.75}]}"#,
        )
        .unwrap();
        let c = t
            .cost("m-1", 1_000_000, 100_000, 2_000_000, 400_000)
            .unwrap();
        // 3 + 1.5 + 0.6 + 1.5
        assert!((c - 6.6).abs() < 1e-9, "{c}");
        assert!(t.cost("other", 1, 1, 1, 1).is_none());
        assert_eq!(t.serial, 2);
    }

    #[test]
    fn bad_tables_are_rejected() {
        assert!(PriceTable::parse("{").is_err());
        assert!(PriceTable::parse(r#"{"serial":1,"prices":[]}"#).is_err());
        assert!(
            PriceTable::parse(r#"{"serial":1,"prices":[{"model":"m","input":-1,"output":1}]}"#)
                .is_err()
        );
    }

    #[test]
    fn model_ids_are_normalised() {
        assert_eq!(
            normalize_model("Anthropic/Claude-Haiku-4-5"),
            "claude-haiku-4-5"
        );
        assert_eq!(
            normalize_model("anthropic.claude-sonnet-4-20250514-v1:0"),
            "claude-sonnet-4-20250514-v1"
        );
        assert_eq!(
            normalize_model("claude-sonnet-4@20250514"),
            "claude-sonnet-4"
        );
    }
}
