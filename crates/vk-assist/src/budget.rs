//! Per-coordinator, per-UTC-day allowance (14 §8) and a one-minute rate window.
//!
//! Before dispatch a request reserves one attempt, its estimated input tokens plus its full
//! output allowance, and (when a cost cap is configured) the corresponding maximum estimated
//! cost. Completion replaces the reservation with reported usage. Unknown usage is recorded as
//! the reservation (conservative), never as zero.

use crate::config::AssistConfig;
use crate::{AssistError, Category, Result};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

pub const DAY_MS: i64 = 86_400_000;

pub fn utc_day(now_ms: i64) -> i64 {
    now_ms.div_euclid(DAY_MS)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Amount {
    pub requests: u64,
    pub tokens: u64,
    pub cost_usd: f64,
}

impl Amount {
    pub fn add(&mut self, o: Amount) {
        self.requests += o.requests;
        self.tokens += o.tokens;
        self.cost_usd += o.cost_usd;
    }
    pub fn sub(&mut self, o: Amount) {
        self.requests = self.requests.saturating_sub(o.requests);
        self.tokens = self.tokens.saturating_sub(o.tokens);
        self.cost_usd = (self.cost_usd - o.cost_usd).max(0.0);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Ledger {
    pub day: i64,
    pub used: Amount,
    /// Requests whose cost could not be estimated (pricing unknown).
    pub cost_unknown_requests: u64,
}

impl Ledger {
    pub fn roll(&mut self, now_ms: i64) {
        let d = utc_day(now_ms);
        if d != self.day {
            *self = Ledger {
                day: d,
                ..Default::default()
            };
        }
    }

    /// Check that `want` fits beside `used + reserved`.
    pub fn check(
        &self,
        cfg: &AssistConfig,
        reserved: Amount,
        want: Amount,
        cost_known: bool,
    ) -> Result<()> {
        let mut total = self.used;
        total.add(reserved);
        let out = |m: String| Err(AssistError::new(Category::BudgetExhausted, m));
        if total.requests + want.requests > cfg.daily_request_limit {
            return out(format!(
                "daily request limit reached ({} per UTC day)",
                cfg.daily_request_limit
            ));
        }
        if total.tokens + want.tokens > cfg.daily_token_limit {
            return out(format!(
                "daily token budget would be exceeded ({} used or reserved, {} requested, limit {})",
                total.tokens, want.tokens, cfg.daily_token_limit
            ));
        }
        if let Some(cap) = cfg.daily_cost_limit_usd {
            if !cost_known {
                return Err(AssistError::new(
                    Category::NotConfigured,
                    "a daily cost limit is set but this model's pricing is unknown; set input_usd_per_mtok and output_usd_per_mtok on the profile",
                ));
            }
            if total.cost_usd + want.cost_usd > cap + 1e-12 {
                return out(format!(
                    "daily estimated cost budget would be exceeded (${:.4} used or reserved, ${:.4} requested, limit ${cap:.4})",
                    total.cost_usd, want.cost_usd
                ));
            }
        }
        Ok(())
    }

    pub fn remaining(&self, cfg: &AssistConfig, reserved: Amount) -> serde_json::Value {
        let mut total = self.used;
        total.add(reserved);
        serde_json::json!({
            "requests": cfg.daily_request_limit.saturating_sub(total.requests),
            "tokens": cfg.daily_token_limit.saturating_sub(total.tokens),
            "cost_usd": cfg.daily_cost_limit_usd.map(|c| (c - total.cost_usd).max(0.0)),
        })
    }
}

/// Sliding one-minute window of provider attempts.
#[derive(Debug, Default)]
pub struct RateWindow {
    times: VecDeque<i64>,
}

impl RateWindow {
    pub fn admit(&mut self, now_ms: i64, per_minute: u32) -> Result<()> {
        while self.times.front().is_some_and(|t| now_ms - t >= 60_000) {
            self.times.pop_front();
        }
        if self.times.len() as u32 >= per_minute {
            return Err(AssistError::new(
                Category::RateLimited,
                format!("assistant rate limit: at most {per_minute} requests per minute"),
            ));
        }
        self.times.push_back(now_ms);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_rollover_and_limits() {
        let cfg = AssistConfig {
            daily_request_limit: 2,
            daily_token_limit: 1000,
            ..Default::default()
        };
        let mut l = Ledger::default();
        l.roll(DAY_MS * 5 + 10);
        let want = Amount {
            requests: 1,
            tokens: 400,
            cost_usd: 0.0,
        };
        assert!(l.check(&cfg, Amount::default(), want, true).is_ok());
        l.used.add(want);
        assert!(l.check(&cfg, Amount::default(), want, true).is_ok());
        let e = l.check(&cfg, want, want, true).unwrap_err();
        assert_eq!(e.category, Category::BudgetExhausted);
        l.roll(DAY_MS * 6);
        assert_eq!(l.used, Amount::default());
    }

    #[test]
    fn cost_cap_needs_pricing() {
        let cfg = AssistConfig {
            daily_cost_limit_usd: Some(0.01),
            ..Default::default()
        };
        let l = Ledger::default();
        let want = Amount {
            requests: 1,
            tokens: 10,
            cost_usd: 0.02,
        };
        assert_eq!(
            l.check(&cfg, Amount::default(), want, false)
                .unwrap_err()
                .category,
            Category::NotConfigured
        );
        assert_eq!(
            l.check(&cfg, Amount::default(), want, true)
                .unwrap_err()
                .category,
            Category::BudgetExhausted
        );
    }

    #[test]
    fn rate_window() {
        let mut w = RateWindow::default();
        assert!(w.admit(0, 2).is_ok());
        assert!(w.admit(1, 2).is_ok());
        assert!(w.admit(2, 2).is_err());
        assert!(w.admit(60_001, 2).is_ok());
    }
}
