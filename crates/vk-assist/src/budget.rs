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

/// What one request has reserved: the per-attempt allowance summed over every admitted
/// attempt (an automatic retry is admitted, and reserved, separately). Persisted before
/// dispatch so a restart can charge it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Reservation {
    pub day: i64,
    pub attempts: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    /// The request reached `running` (it may have been sent and billed).
    #[serde(default)]
    pub dispatched: bool,
}

impl Reservation {
    /// One attempt's allowance: estimated input, full output allowance, maximum cost.
    pub fn attempt(input_tokens: u64, output_tokens: u64, cost_usd: f64) -> Amount {
        Amount {
            requests: 1,
            tokens: input_tokens + output_tokens,
            cost_usd,
        }
    }

    pub fn add_attempt(&mut self, input_tokens: u64, output_tokens: u64, cost_usd: f64) {
        self.attempts += 1;
        self.input_tokens += input_tokens;
        self.output_tokens += output_tokens;
        self.cost_usd += cost_usd;
    }

    pub fn amount(&self) -> Amount {
        Amount {
            requests: self.attempts as u64,
            tokens: self.input_tokens + self.output_tokens,
            cost_usd: self.cost_usd,
        }
    }

    /// The amount charged when the request ends after `attempts` provider attempts with the
    /// reported usage. Each reported component replaces its reservation; an unknown component
    /// keeps its reservation (conservative), and so does the cost unless it can be computed
    /// from known pricing.
    pub fn settle(
        &self,
        attempts: u32,
        input: Option<u64>,
        output: Option<u64>,
        prices: Option<(f64, f64)>,
    ) -> Amount {
        let i = input.unwrap_or(self.input_tokens);
        let o = output.unwrap_or(self.output_tokens);
        let cost = match prices {
            Some((pi, po)) if input.is_some() || output.is_some() => {
                (i as f64 * pi + o as f64 * po) / 1_000_000.0
            }
            _ => self.cost_usd,
        };
        Amount {
            requests: attempts.max(self.attempts).max(1) as u64,
            tokens: i + o,
            cost_usd: cost,
        }
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
    fn partial_usage_keeps_unknown_components_reserved() {
        let mut r = Reservation::default();
        r.add_attempt(300, 1024, (300.0 + 1024.0 * 5.0) / 1e6);
        let prices = Some((1.0, 5.0));
        // Input only: output stays at its full reservation.
        let a = r.settle(1, Some(120), None, prices);
        assert_eq!(a.tokens, 120 + 1024);
        assert!((a.cost_usd - (120.0 + 1024.0 * 5.0) / 1e6).abs() < 1e-12);
        // Output only: input stays at its estimate.
        let a = r.settle(1, None, Some(30), prices);
        assert_eq!(a.tokens, 300 + 30);
        assert!((a.cost_usd - (300.0 + 150.0) / 1e6).abs() < 1e-12);
        // Nothing known: the whole reservation, never zero.
        assert_eq!(r.settle(1, None, None, prices), r.amount());
        assert_eq!(r.settle(1, None, None, None).cost_usd, r.cost_usd);
        // Both known.
        assert_eq!(r.settle(1, Some(1), Some(2), prices).tokens, 3);
        // Every admitted attempt counts as a request.
        r.add_attempt(300, 1024, 0.0);
        assert_eq!(r.settle(2, None, None, None).requests, 2);
        assert_eq!(r.settle(1, None, None, None).requests, 2);
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
