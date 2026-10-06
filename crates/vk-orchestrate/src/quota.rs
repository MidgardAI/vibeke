//! Quota and cost scheduling (12 "Quota & cost scheduling").
//!
//! Subscriptions have 5-hour and weekly windows. Near a limit, low-priority runs pause so the
//! remaining headroom goes to the work that matters; once the window resets (or usage drops)
//! they resume. New work can be routed to the account with the most headroom.
//!
//! [`tick`] is a pure function of the current observations ([`AccountUse`], [`RunView`]) and
//! the persisted [`State`]; the server turns its [`Action`]s into `task.park` / `task.resume`
//! (or an interrupt for a run outside any task) and records them. Protected work
//! (`protect_priority`) is never paused. Runs are paused per task: all of a task's runs on the
//! account go together.

use crate::config::{Price, QuotaConfig};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What is known about one billing account right now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountUse {
    pub account: String,
    pub harness: String,
    pub scope: Option<String>,
    /// 0..=1 of the window used, when the harness reports it.
    pub used_fraction: Option<f64>,
    /// The harness is held by the limit right now.
    pub limited: bool,
    pub resets_at_ms: Option<i64>,
    pub observed_at_ms: i64,
}

impl AccountUse {
    /// Still over its limit at `now` (a reset time in the past clears a `limited` flag whose
    /// observation is older than the reset).
    pub fn is_limited(&self, now: i64) -> bool {
        self.limited && self.resets_at_ms.is_none_or(|t| t > now)
    }

    /// Remaining share of the window, 0..=1. Unknown usage counts as half.
    pub fn headroom(&self, now: i64) -> f64 {
        if self.is_limited(now) {
            return 0.0;
        }
        match self.used_fraction {
            // An observation from before a reset is stale: the window is fresh again.
            Some(_)
                if self
                    .resets_at_ms
                    .is_some_and(|t| t <= now && self.observed_at_ms < t) =>
            {
                1.0
            }
            Some(u) => (1.0 - u).clamp(0.0, 1.0),
            None => 0.5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunView {
    pub run: String,
    pub task: Option<String>,
    /// Handle shown to people (`k7.1`, or the run handle).
    pub handle: String,
    pub harness: String,
    /// The task's priority (runs without one are 0).
    pub priority: i32,
    /// Execution is `working` (not idle, not already ended).
    pub working: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Paused {
    /// The task, or the run id for a run outside any task.
    pub key: String,
    pub task: Option<String>,
    pub runs: Vec<String>,
    pub handle: String,
    pub account: String,
    pub priority: i32,
    pub paused_at_ms: i64,
    pub resumes_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct State {
    pub paused: BTreeMap<String, Paused>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    Pause {
        key: String,
        task: Option<String>,
        runs: Vec<String>,
        handle: String,
        account: String,
        priority: i32,
        reason: String,
        resumes_at_ms: Option<i64>,
    },
    Resume {
        key: String,
        task: Option<String>,
        handle: String,
        reason: String,
    },
    /// A paused entry whose work no longer exists; drop it from the state.
    Forget { key: String },
}

fn account_over(a: &AccountUse, now: i64, cfg: &QuotaConfig) -> Option<String> {
    if a.is_limited(now) {
        return Some(match a.resets_at_ms {
            Some(t) => format!(
                "{} is rate limited until {} min from now",
                a.account,
                ((t - now) / 60_000).max(1)
            ),
            None => format!("{} is rate limited", a.account),
        });
    }
    match a.used_fraction {
        Some(u) if u >= cfg.pause_at && a.headroom(now) < 1.0 => Some(format!(
            "{} is at {:.0}% of its window (pause at {:.0}%)",
            a.account,
            u * 100.0,
            cfg.pause_at * 100.0
        )),
        _ => None,
    }
}

/// Decide what to pause and resume now.
pub fn tick(
    now: i64,
    accounts: &[AccountUse],
    runs: &[RunView],
    state: &State,
    cfg: &QuotaConfig,
) -> Vec<Action> {
    let mut out = vec![];
    if !cfg.enabled {
        return out;
    }
    let acct = |name: &str| accounts.iter().find(|a| a.account == name);

    // Pauses: low-priority working runs on an account that is over, grouped per task.
    let mut groups: BTreeMap<String, (Vec<&RunView>, String)> = BTreeMap::new();
    for r in runs
        .iter()
        .filter(|r| r.working && r.priority < cfg.protect_priority)
    {
        let name = cfg.account_of(&r.harness);
        let Some(a) = acct(&name) else { continue };
        let Some(why) = account_over(a, now, cfg) else {
            continue;
        };
        // A reset further away than max_wait is not worth parking for.
        if let Some(t) = a.resets_at_ms
            && t > now
            && (t - now) as u128 > cfg.max_wait().as_millis()
        {
            continue;
        }
        let key = r.task.clone().unwrap_or_else(|| r.run.clone());
        if state.paused.contains_key(&key) {
            continue;
        }
        groups.entry(key).or_insert_with(|| (vec![], why)).0.push(r);
    }
    for (key, (rs, why)) in groups {
        // A task with a protected run on the same account stays up as a whole.
        let task = rs[0].task.clone();
        if let Some(t) = &task
            && runs
                .iter()
                .any(|r| r.task.as_deref() == Some(t) && r.priority >= cfg.protect_priority)
        {
            continue;
        }
        let name = cfg.account_of(&rs[0].harness);
        out.push(Action::Pause {
            key,
            task,
            runs: rs.iter().map(|r| r.run.clone()).collect(),
            handle: rs[0].handle.clone(),
            account: name.clone(),
            priority: rs[0].priority,
            reason: why,
            resumes_at_ms: acct(&name)
                .and_then(|a| a.resets_at_ms)
                .filter(|t| *t > now),
        });
    }

    // Resumes: the window reset or usage fell back, highest priority first, a few per tick.
    let mut due: Vec<&Paused> = vec![];
    for p in state.paused.values() {
        let exists = runs.iter().any(|r| {
            p.runs.contains(&r.run)
                || p.task
                    .as_deref()
                    .is_some_and(|t| r.task.as_deref() == Some(t))
        });
        // A parked task has no working runs; keep it while its task is known. Run-less entries
        // for a vanished run are dropped.
        if !exists && p.task.is_none() {
            out.push(Action::Forget { key: p.key.clone() });
            continue;
        }
        let a = acct(&p.account);
        let reset_passed = p.resumes_at_ms.is_some_and(|t| t <= now);
        let recovered = match a {
            Some(a) => {
                !a.is_limited(now)
                    && (reset_passed
                        || a.used_fraction.is_none_or(|u| u < cfg.resume_below)
                        || a.headroom(now) >= 1.0)
            }
            // No observation any more: after the expected reset, let it go.
            None => reset_passed,
        };
        if recovered {
            due.push(p);
        }
    }
    due.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then(a.paused_at_ms.cmp(&b.paused_at_ms))
    });
    for p in due
        .into_iter()
        .take(cfg.max_resumes_per_tick.max(1) as usize)
    {
        out.push(Action::Resume {
            key: p.key.clone(),
            task: p.task.clone(),
            handle: p.handle.clone(),
            reason: if p.resumes_at_ms.is_some_and(|t| t <= now) {
                "the limit window reset".into()
            } else {
                "usage is back under the resume threshold".into()
            },
        });
    }
    out
}

/// Apply a recorded action to the persisted state.
pub fn apply(state: &mut State, a: &Action, now: i64) {
    match a {
        Action::Pause {
            key,
            task,
            runs,
            handle,
            account,
            priority,
            resumes_at_ms,
            ..
        } => {
            state.paused.insert(
                key.clone(),
                Paused {
                    key: key.clone(),
                    task: task.clone(),
                    runs: runs.clone(),
                    handle: handle.clone(),
                    account: account.clone(),
                    priority: *priority,
                    paused_at_ms: now,
                    resumes_at_ms: *resumes_at_ms,
                },
            );
        }
        Action::Resume { key, .. } | Action::Forget { key } => {
            state.paused.remove(key);
        }
    }
}

/// Accounts with usable headroom first, for routing new work. Only `candidates` (harness ids
/// the caller accepts) are considered; limited accounts are left out.
pub fn rank_for_routing(
    now: i64,
    candidates: &[String],
    accounts: &[AccountUse],
    cfg: &QuotaConfig,
) -> Vec<(String, f64)> {
    let mut v: Vec<(String, f64)> = candidates
        .iter()
        .filter_map(|h| {
            let name = cfg.account_of(h);
            let a = accounts.iter().find(|a| a.account == name);
            let room = a.map(|a| a.headroom(now)).unwrap_or(0.5);
            (room > 0.0).then(|| (h.clone(), room))
        })
        .collect();
    v.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    v
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Tokens {
    /// What `self` added on top of `previous` (session totals); a total that went down means
    /// the counter restarted, so the whole of `self` counts.
    pub fn since(&self, previous: &Tokens) -> Tokens {
        if self.input < previous.input
            || self.output < previous.output
            || self.cache_read < previous.cache_read
            || self.cache_write < previous.cache_write
        {
            return *self;
        }
        Tokens {
            input: self.input - previous.input,
            output: self.output - previous.output,
            cache_read: self.cache_read - previous.cache_read,
            cache_write: self.cache_write - previous.cache_write,
        }
    }
}

/// Estimated USD for `t` on `model`, from the user's `[orchestrate.quota.prices]` (exact model
/// id, else the longest configured prefix). `None` when the model has no price: Vibeke ships no
/// price table of its own.
pub fn estimate_cost(prices: &BTreeMap<String, Price>, model: &str, t: &Tokens) -> Option<f64> {
    let p = prices.get(model).or_else(|| {
        prices
            .iter()
            .filter(|(k, _)| model.starts_with(k.as_str()))
            .max_by_key(|(k, _)| k.len())
            .map(|(_, v)| v)
    })?;
    let m = 1_000_000.0;
    Some(
        t.input as f64 / m * p.input
            + t.output as f64 / m * p.output
            + t.cache_read as f64 / m * p.cache_read
            + t.cache_write as f64 / m * p.cache_write,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;

    fn cfg() -> QuotaConfig {
        QuotaConfig {
            enabled: true,
            ..Default::default()
        }
    }

    fn acct(name: &str, used: Option<f64>, limited: bool, resets: Option<i64>) -> AccountUse {
        AccountUse {
            account: name.into(),
            harness: name.into(),
            scope: Some("5h".into()),
            used_fraction: used,
            limited,
            resets_at_ms: resets,
            observed_at_ms: 0,
        }
    }

    fn run(id: &str, task: Option<&str>, harness: &str, prio: i32, working: bool) -> RunView {
        RunView {
            run: id.into(),
            task: task.map(str::to_string),
            handle: id.into(),
            harness: harness.into(),
            priority: prio,
            working,
        }
    }

    #[test]
    fn headroom_and_limits() {
        let a = acct("claude", Some(0.25), false, None);
        assert!((a.headroom(0) - 0.75).abs() < 1e-9);
        assert_eq!(acct("c", None, false, None).headroom(0), 0.5);
        let l = acct("c", Some(1.0), true, Some(10 * MIN));
        assert!(l.is_limited(0));
        assert_eq!(l.headroom(0), 0.0);
        assert!(!l.is_limited(11 * MIN));
        // A stale pre-reset observation means a fresh window after the reset.
        assert_eq!(l.headroom(11 * MIN), 1.0);
        assert!(acct("c", None, true, None).is_limited(i64::MAX / 2));
    }

    #[test]
    fn disabled_does_nothing() {
        let mut c = cfg();
        c.enabled = false;
        let acc = [acct("claude", Some(0.99), false, None)];
        assert!(
            tick(
                0,
                &acc,
                &[run("a1", None, "claude", 0, true)],
                &State::default(),
                &c
            )
            .is_empty()
        );
    }

    #[test]
    fn near_the_limit_low_priority_work_pauses_and_protected_work_stays() {
        let acc = [
            acct("claude", Some(0.93), false, Some(120 * MIN)),
            acct("codex", Some(0.2), false, None),
        ];
        let runs = [
            run("a1", Some("t1"), "claude", 0, true),
            run("a2", Some("t1"), "claude", 1, true),
            run("a3", Some("t2"), "claude", 9, true),
            run("a4", Some("t3"), "codex", 0, true),
            run("a5", Some("t4"), "claude", 0, false),
            run("a6", None, "claude", 2, true),
        ];
        let acts = tick(0, &acc, &runs, &State::default(), &cfg());
        let pauses: Vec<_> = acts
            .iter()
            .filter_map(|a| match a {
                Action::Pause {
                    key,
                    runs,
                    resumes_at_ms,
                    ..
                } => Some((key.clone(), runs.clone(), *resumes_at_ms)),
                _ => None,
            })
            .collect();
        // t1 (both runs, one pause), a6 (no task); not the protected t2, not codex, not the idle run.
        assert_eq!(pauses.len(), 2, "{acts:#?}");
        assert!(
            pauses
                .iter()
                .any(|(k, r, t)| k == "t1" && r == &vec!["a1", "a2"] && *t == Some(120 * MIN))
        );
        assert!(pauses.iter().any(|(k, _, _)| k == "a6"));
    }

    #[test]
    fn a_task_with_a_protected_run_is_not_paused() {
        let acc = [acct("claude", Some(0.99), false, None)];
        let runs = [
            run("a1", Some("t1"), "claude", 0, true),
            run("a2", Some("t1"), "claude", 9, true),
        ];
        assert!(tick(0, &acc, &runs, &State::default(), &cfg()).is_empty());
    }

    #[test]
    fn a_reset_too_far_away_is_not_worth_pausing_for() {
        let acc = [acct("claude", Some(1.0), true, Some(24 * 60 * MIN))];
        let runs = [run("a1", Some("t1"), "claude", 0, true)];
        assert!(tick(0, &acc, &runs, &State::default(), &cfg()).is_empty());
        let mut c = cfg();
        c.max_wait = "48h".into();
        assert_eq!(tick(0, &acc, &runs, &State::default(), &c).len(), 1);
    }

    #[test]
    fn already_paused_work_is_not_paused_twice() {
        let acc = [acct("claude", Some(0.95), false, None)];
        let runs = [run("a1", Some("t1"), "claude", 0, true)];
        let mut st = State::default();
        let first = tick(0, &acc, &runs, &st, &cfg());
        assert_eq!(first.len(), 1);
        apply(&mut st, &first[0], 0);
        assert!(st.paused.contains_key("t1"));
        assert!(
            tick(0, &acc, &runs, &st, &cfg())
                .iter()
                .all(|a| !matches!(a, Action::Pause { .. }))
        );
    }

    #[test]
    fn paused_work_resumes_after_the_reset_or_when_usage_drops() {
        let mut st = State::default();
        let mut c = cfg();
        c.max_resumes_per_tick = 2;
        for (key, prio, at) in [("t1", 0, 1), ("t2", 3, 2), ("t3", 0, 3)] {
            st.paused.insert(
                key.into(),
                Paused {
                    key: key.into(),
                    task: Some(key.into()),
                    runs: vec![],
                    handle: key.into(),
                    account: "claude".into(),
                    priority: prio,
                    paused_at_ms: at,
                    resumes_at_ms: Some(100 * MIN),
                },
            );
        }
        // Still limited before the reset: nothing resumes.
        let acc = [acct("claude", Some(1.0), true, Some(100 * MIN))];
        assert!(tick(10 * MIN, &acc, &[], &st, &c).is_empty());
        // After the reset: the two highest-priority/oldest, not all three.
        let acts = tick(
            101 * MIN,
            &[acct("claude", None, false, None)],
            &[],
            &st,
            &c,
        );
        let resumed: Vec<_> = acts
            .iter()
            .filter_map(|a| match a {
                Action::Resume { key, reason, .. } => Some((key.clone(), reason.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(resumed.len(), 2);
        assert_eq!(resumed[0].0, "t2");
        assert_eq!(resumed[1].0, "t1");
        assert!(resumed[0].1.contains("reset"));
        // Usage fell back under the threshold before the reset.
        let acc = [acct("claude", Some(0.5), false, Some(100 * MIN))];
        let acts = tick(10 * MIN, &acc, &[], &st, &c);
        assert!(acts.iter().any(|a| matches!(a, Action::Resume { .. })));
        // Between resume_below and pause_at: hold.
        let acc = [acct("claude", Some(0.85), false, Some(100 * MIN))];
        assert!(tick(10 * MIN, &acc, &[], &st, &c).is_empty());
    }

    #[test]
    fn entries_for_vanished_runs_are_forgotten() {
        let mut st = State::default();
        st.paused.insert(
            "a9".into(),
            Paused {
                key: "a9".into(),
                task: None,
                runs: vec!["a9".into()],
                handle: "a9".into(),
                account: "claude".into(),
                priority: 0,
                paused_at_ms: 0,
                resumes_at_ms: None,
            },
        );
        let acts = tick(
            0,
            &[acct("claude", Some(0.99), false, None)],
            &[],
            &st,
            &cfg(),
        );
        assert!(
            acts.iter()
                .any(|a| matches!(a, Action::Forget { key } if key == "a9"))
        );
        apply(&mut st, &Action::Forget { key: "a9".into() }, 0);
        assert!(st.paused.is_empty());
    }

    #[test]
    fn account_mapping_pools_harnesses() {
        let mut c = cfg();
        c.accounts.insert("claude".into(), "work".into());
        c.accounts.insert("claude-headless".into(), "work".into());
        let acc = [acct("work", Some(0.95), false, None)];
        let runs = [
            run("a1", Some("t1"), "claude", 0, true),
            run("a2", Some("t2"), "claude-headless", 0, true),
        ];
        assert_eq!(tick(0, &acc, &runs, &State::default(), &c).len(), 2);
    }

    #[test]
    fn routing_prefers_headroom_and_skips_limited_accounts() {
        let acc = [
            acct("claude", Some(0.9), false, None),
            acct("codex", Some(0.2), false, None),
            acct("pi", Some(1.0), true, Some(MIN)),
        ];
        let cand = vec![
            "claude".to_string(),
            "codex".to_string(),
            "pi".to_string(),
            "omp".to_string(),
        ];
        let r = rank_for_routing(0, &cand, &acc, &cfg());
        let names: Vec<_> = r.iter().map(|x| x.0.as_str()).collect();
        assert_eq!(
            names,
            vec!["codex", "omp", "claude"],
            "pi is limited, omp unknown = half"
        );
        assert!(rank_for_routing(0, &[], &acc, &cfg()).is_empty());
    }

    #[test]
    fn token_deltas_and_cost_estimates() {
        let a = Tokens {
            input: 100,
            output: 50,
            cache_read: 10,
            cache_write: 5,
        };
        let b = Tokens {
            input: 160,
            output: 90,
            cache_read: 10,
            cache_write: 9,
        };
        assert_eq!(
            b.since(&a),
            Tokens {
                input: 60,
                output: 40,
                cache_read: 0,
                cache_write: 4
            }
        );
        assert_eq!(a.since(&b), a, "a restarted counter counts in full");
        let mut prices = BTreeMap::new();
        prices.insert(
            "model-x".to_string(),
            Price {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            },
        );
        prices.insert(
            "model-x-mini".to_string(),
            Price {
                input: 1.0,
                output: 2.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
        );
        let t = Tokens {
            input: 1_000_000,
            output: 100_000,
            cache_read: 0,
            cache_write: 0,
        };
        assert!((estimate_cost(&prices, "model-x", &t).unwrap() - 4.5).abs() < 1e-9);
        // longest prefix wins
        assert!((estimate_cost(&prices, "model-x-mini-2026", &t).unwrap() - 1.2).abs() < 1e-9);
        assert!((estimate_cost(&prices, "model-x-2026", &t).unwrap() - 4.5).abs() < 1e-9);
        assert_eq!(estimate_cost(&prices, "other", &t), None);
        assert_eq!(estimate_cost(&BTreeMap::new(), "model-x", &t), None);
    }
}
