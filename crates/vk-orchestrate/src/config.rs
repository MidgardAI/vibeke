//! `[orchestrate]` user config. The section is owned by this crate; `vk-config` keeps it
//! verbatim in `Config::extra` (like `[isolation]`). Every feature is off by default.
//!
//! ```toml
//! [orchestrate.best_of_n]          # 05 §12
//! enabled = false                  # task new --agents claude:2,codex:1; task compare/pick
//! max_children = 8
//! check = true                     # run [tasks.check] / `check_command` in each child on finish
//! check_command = ""               # fallback when the repo declares no check
//! check_timeout = "10m"
//!
//! [orchestrate.split]              # 05 §11
//! enabled = false                  # task split: move a running agent's changes into a new task
//! quiet_for = "3s"                 # no file changes in the source for this long (writers quiesced)
//! resume = false                   # try the harness resume handle in the new cwd (else hand-off prompt)
//!
//! [orchestrate.merge]              # 12 "Merge orchestration"
//! enabled = false                  # claims, conflict prediction, merge queue
//! predict_every = "30s"            # background conflict prediction while 2+ tasks are live
//! queue_check = ""                 # command run in the integration worktree before a merge lands
//! queue_check_timeout = "15m"
//! strategy = "merge"               # merge | squash
//! target = ""                      # branch the queue merges into (default: the task's base)
//!
//! [orchestrate.planner]            # 12 "Goal to plan to tasks"
//! enabled = false
//! backend = "heuristic"            # heuristic | agent (a planning run submits the plan) | external (goal.plan.submit)
//! planner_harness = "claude"       # harness for the agent backend
//! approval_required = true         # nothing starts before `goal approve`
//! max_steps = 12
//! briefing_window = "12h"
//!
//! [orchestrate.quota]              # 12 "Quota and cost scheduling"
//! enabled = false
//! interval = "30s"
//! pause_at = 0.9                   # used share of a window at which low-priority work pauses
//! protect_priority = 5             # runs/tasks with priority >= this keep running
//! max_wait = "6h"                  # only pause for a reset closer than this
//! resume_below = 0.8
//! max_resumes_per_tick = 2
//! [orchestrate.quota.accounts]     # account label per harness (default: the harness id)
//! # claude = "claude-personal"
//! [orchestrate.quota.prices.example-model]  # USD per million tokens (no built-in table)
//! # input = 3.0
//! # output = 15.0
//! # cache_read = 0.3
//! # cache_write = 3.75
//! ```

use crate::parse_duration;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct OrchestrateConfig {
    pub best_of_n: BestOfNConfig,
    pub split: SplitConfig,
    pub merge: MergeConfig,
    pub planner: PlannerConfig,
    pub quota: QuotaConfig,
}

impl OrchestrateConfig {
    /// Parse the section; an invalid section yields defaults plus the error text.
    pub fn from_toml(v: Option<&toml::Value>) -> (OrchestrateConfig, Option<String>) {
        match v {
            None => (OrchestrateConfig::default(), None),
            Some(v) => match v.clone().try_into::<OrchestrateConfig>() {
                Ok(c) => (c, None),
                Err(e) => (OrchestrateConfig::default(), Some(e.to_string())),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BestOfNConfig {
    pub enabled: bool,
    pub max_children: u32,
    pub check: bool,
    pub check_command: String,
    pub check_timeout: String,
}
impl Default for BestOfNConfig {
    fn default() -> Self {
        BestOfNConfig {
            enabled: false,
            max_children: 8,
            check: true,
            check_command: String::new(),
            check_timeout: "10m".into(),
        }
    }
}
impl BestOfNConfig {
    pub fn check_timeout(&self) -> Duration {
        parse_duration(&self.check_timeout).unwrap_or(Duration::from_secs(600))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SplitConfig {
    pub enabled: bool,
    pub quiet_for: String,
    pub resume: bool,
}
impl Default for SplitConfig {
    fn default() -> Self {
        SplitConfig {
            enabled: false,
            quiet_for: "3s".into(),
            resume: false,
        }
    }
}
impl SplitConfig {
    pub fn quiet_for(&self) -> Duration {
        parse_duration(&self.quiet_for).unwrap_or(Duration::from_secs(3))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MergeConfig {
    pub enabled: bool,
    pub predict_every: String,
    pub queue_check: String,
    pub queue_check_timeout: String,
    /// `merge` | `squash`.
    pub strategy: String,
    pub target: String,
}
impl Default for MergeConfig {
    fn default() -> Self {
        MergeConfig {
            enabled: false,
            predict_every: "30s".into(),
            queue_check: String::new(),
            queue_check_timeout: "15m".into(),
            strategy: "merge".into(),
            target: String::new(),
        }
    }
}
impl MergeConfig {
    pub fn predict_every(&self) -> Duration {
        parse_duration(&self.predict_every)
            .filter(|d| !d.is_zero())
            .unwrap_or(Duration::from_secs(30))
    }
    pub fn queue_check_timeout(&self) -> Duration {
        parse_duration(&self.queue_check_timeout).unwrap_or(Duration::from_secs(900))
    }
    pub fn squash(&self) -> bool {
        self.strategy == "squash"
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlannerConfig {
    pub enabled: bool,
    /// `heuristic` | `agent` | `external`.
    pub backend: String,
    pub planner_harness: String,
    pub approval_required: bool,
    pub max_steps: u32,
    pub briefing_window: String,
}
impl Default for PlannerConfig {
    fn default() -> Self {
        PlannerConfig {
            enabled: false,
            backend: "heuristic".into(),
            planner_harness: "claude".into(),
            approval_required: true,
            max_steps: 12,
            briefing_window: "12h".into(),
        }
    }
}
impl PlannerConfig {
    pub fn briefing_window(&self) -> Duration {
        parse_duration(&self.briefing_window).unwrap_or(Duration::from_secs(12 * 3600))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotaConfig {
    pub enabled: bool,
    pub interval: String,
    pub pause_at: f64,
    pub protect_priority: i32,
    pub max_wait: String,
    pub resume_below: f64,
    pub max_resumes_per_tick: u32,
    pub accounts: BTreeMap<String, String>,
    pub prices: BTreeMap<String, Price>,
}
impl Default for QuotaConfig {
    fn default() -> Self {
        QuotaConfig {
            enabled: false,
            interval: "30s".into(),
            pause_at: 0.9,
            protect_priority: 5,
            max_wait: "6h".into(),
            resume_below: 0.8,
            max_resumes_per_tick: 2,
            accounts: BTreeMap::new(),
            prices: BTreeMap::new(),
        }
    }
}
impl QuotaConfig {
    pub fn interval(&self) -> Duration {
        parse_duration(&self.interval)
            .filter(|d| !d.is_zero())
            .unwrap_or(Duration::from_secs(30))
    }
    pub fn max_wait(&self) -> Duration {
        parse_duration(&self.max_wait).unwrap_or(Duration::from_secs(6 * 3600))
    }
    /// The account a harness bills to.
    pub fn account_of(&self, harness: &str) -> String {
        self.accounts
            .get(harness)
            .cloned()
            .unwrap_or_else(|| harness.to_string())
    }
}

/// USD per million tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off() {
        let (c, e) = OrchestrateConfig::from_toml(None);
        assert!(e.is_none());
        assert!(!c.best_of_n.enabled);
        assert!(!c.split.enabled);
        assert!(!c.merge.enabled);
        assert!(!c.planner.enabled);
        assert!(!c.quota.enabled);
        assert!(c.planner.approval_required);
    }

    #[test]
    fn parses_and_reports_errors() {
        let v: toml::Value = toml::from_str(
            r#"
            [best_of_n]
            enabled = true
            max_children = 3
            [quota]
            enabled = true
            pause_at = 0.75
            [quota.accounts]
            claude = "claude-work"
            [quota.prices.m1]
            input = 3.0
            output = 15.0
            "#,
        )
        .unwrap();
        let (c, e) = OrchestrateConfig::from_toml(Some(&v));
        assert!(e.is_none());
        assert!(c.best_of_n.enabled);
        assert_eq!(c.best_of_n.max_children, 3);
        assert_eq!(c.quota.pause_at, 0.75);
        assert_eq!(c.quota.account_of("claude"), "claude-work");
        assert_eq!(c.quota.account_of("codex"), "codex");
        assert_eq!(c.quota.prices["m1"].output, 15.0);
        let bad: toml::Value = toml::from_str("[best_of_n]\nenabled = \"yes\"").unwrap();
        let (d, e) = OrchestrateConfig::from_toml(Some(&bad));
        assert!(e.is_some());
        assert!(!d.best_of_n.enabled);
    }
}
