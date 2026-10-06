//! Request budgets and spawn limits for pane-scoped callers (09 §5.1 rules 7 and 8, 07 §1.4
//! `rate_limited`). Cooperative guardrails against a runaway or prompt-injected agent; full-scope
//! callers (the user's TUI and CLI) are never limited.
//!
//! - Every pane token gets a token bucket: `burst` requests, refilled at `rate` per second
//!   (default 50 / 10).
//! - `agent.spawn`, `agent.start` and `task.create` from a pane: at most `spawns_per_min` per
//!   pane in any 60 s window (default 10).
//! - Panes created by agents carry depth through `created_by = agent:<pane>`: a pane-scoped
//!   call that would create a pane deeper than `max_spawn_depth` (default 3) is refused, and an
//!   agent start is refused while the root's lineage has `max_descendant_runs` live runs
//!   (default 20).
//!
//! Exceeding a limit returns `rate_limited` (budgets, `retryable: true`, `details.retry_after_ms`)
//! or `permission_denied` (`details.reason: spawn_depth_exceeded | descendant_runs_exceeded`),
//! records a `security.rate_limited` event and, at most once a minute per pane, a notification.
//! Configuration: `[security.limits]` (`burst`, `rate`, `spawns_per_min`, `max_spawn_depth`,
//! `max_descendant_runs`).

use crate::Server;
use crate::api::{Ctx, err};
use crate::core::Tx;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use vk_proto::model::Pane;
use vk_proto::rpc::{ErrorKind, RpcError};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub burst: f64,
    pub rate: f64,
    pub spawns_per_min: usize,
    pub max_spawn_depth: usize,
    pub max_descendant_runs: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            burst: 50.0,
            rate: 10.0,
            spawns_per_min: 10,
            max_spawn_depth: 3,
            max_descendant_runs: 20,
        }
    }
}

impl Limits {
    pub fn from_config(cfg: &vk_config::Config) -> Limits {
        let d = Limits::default();
        let t = cfg
            .extra
            .get("security")
            .and_then(|s| s.get("limits"))
            .and_then(|l| l.as_table());
        let Some(t) = t else { return d };
        let num = |k: &str| {
            t.get(k).and_then(|v| {
                v.as_integer()
                    .map(|i| i as f64)
                    .or_else(|| v.as_float())
                    .filter(|x| *x >= 0.0)
            })
        };
        Limits {
            burst: num("burst").unwrap_or(d.burst).max(1.0),
            rate: num("rate").unwrap_or(d.rate),
            spawns_per_min: num("spawns_per_min").map_or(d.spawns_per_min, |x| x as usize),
            max_spawn_depth: num("max_spawn_depth").map_or(d.max_spawn_depth, |x| x as usize),
            max_descendant_runs: num("max_descendant_runs")
                .map_or(d.max_descendant_runs, |x| x as usize),
        }
    }
}

#[derive(Default)]
struct State {
    limits: Option<Limits>,
    /// pane -> (tokens, last refill)
    buckets: HashMap<String, (f64, Instant)>,
    /// pane -> spawn times in the last minute
    spawns: HashMap<String, VecDeque<Instant>>,
    /// pane -> last notification
    notified: HashMap<String, Instant>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(Mutex::default)
}

/// Apply a (re)loaded config.
pub fn refresh(cfg: &vk_config::Config) {
    state().lock().unwrap().limits = Some(Limits::from_config(cfg));
}

pub fn limits() -> Limits {
    let l = state().lock().unwrap().limits;
    match l {
        Some(l) => l,
        None => {
            let l = Limits::from_config(&crate::config_api::current());
            state().lock().unwrap().limits = Some(l);
            l
        }
    }
}

/// Methods that start an agent.
fn starts_agent(method: &str, p: &Value) -> bool {
    match method {
        "agent.spawn" | "agent.start" => true,
        "task.create" => p
            .get("agents")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty()),
        _ => false,
    }
}

/// Methods that create panes (a new level of agent lineage).
fn creates_pane(method: &str) -> bool {
    matches!(
        method,
        "pane.split"
            | "pane.float"
            | "tab.create"
            | "workspace.create"
            | "agent.spawn"
            | "task.create"
            | "layout.apply"
            | "browser.pane.create"
    )
}

/// The agent lineage of `pane`: depth (0 for a pane the user created) and root pane id.
pub fn lineage(panes: &[Pane], pane: &str) -> (usize, String) {
    let mut cur = pane.to_string();
    let mut depth = 0;
    for _ in 0..64 {
        let Some(p) = panes.iter().find(|p| p.id == cur) else {
            break;
        };
        match p.created_by.strip_prefix("agent:") {
            Some(parent) if panes.iter().any(|x| x.id == parent) => {
                depth += 1;
                cur = parent.to_string();
            }
            Some(_) => {
                depth += 1;
                break;
            }
            None => break,
        }
    }
    (depth, cur)
}

fn refuse(server: &Server, pane: &str, method: &str, limit: &str, e: RpcError) -> RpcError {
    let p = server.with_core(|c| c.pane(pane).cloned());
    let handle = p
        .as_ref()
        .map(|p| p.handle.clone())
        .unwrap_or_else(|| pane.to_string());
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.event(
            "security.rate_limited",
            json!({"pane": pane}),
            json!({"method": method, "limit": limit}),
        );
        let _ = server.commit(&mut c, tx);
    }
    let notify = {
        let mut st = state().lock().unwrap();
        let now = Instant::now();
        let due = st
            .notified
            .get(pane)
            .is_none_or(|t| now.duration_since(*t) >= Duration::from_secs(60));
        if due {
            st.notified.insert(pane.to_string(), now);
        }
        due
    };
    if notify {
        crate::audit::rate_limited(server, pane, &handle, method, limit);
        let what = if limit == "requests" {
            "is calling the API rapidly"
        } else {
            "is spawning agents rapidly"
        };
        server.notify(
            "security",
            Some(pane),
            &format!("{handle} {what}"),
            &format!("{method} refused ({limit} limit)"),
            "normal",
        );
    }
    e
}

/// Test hook: in-process tests that sweep every method with one pane token (the scope
/// catalog) would otherwise measure the budget instead of the authorization.
#[cfg(test)]
pub(crate) static UNLIMITED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Check the budgets of a pane-scoped call before dispatch. Full scope is never limited.
pub fn check(server: &Server, ctx: &Ctx, method: &str, p: &Value) -> Result<(), RpcError> {
    let Some(pane) = ctx.pane_scope.as_deref() else {
        return Ok(());
    };
    #[cfg(test)]
    if UNLIMITED.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(());
    }
    let l = limits();
    let now = Instant::now();
    // Request budget (token bucket).
    let wait = {
        let mut st = state().lock().unwrap();
        let (tokens, last) = st.buckets.entry(pane.to_string()).or_insert((l.burst, now));
        *tokens = (*tokens + now.duration_since(*last).as_secs_f64() * l.rate).min(l.burst);
        *last = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            None
        } else {
            Some(if l.rate > 0.0 {
                ((1.0 - *tokens) / l.rate * 1000.0).ceil() as u64
            } else {
                60_000
            })
        }
    };
    if let Some(ms) = wait {
        let e = err(
            ErrorKind::RateLimited,
            format!("rate_limited: pane request budget exceeded ({}/s)", l.rate),
        )
        .details(json!({"limit": "requests", "retry_after_ms": ms, "scope": "pane"}));
        return Err(refuse(server, pane, method, "requests", e));
    }
    if creates_pane(method) || starts_agent(method, p) {
        let panes = server.with_core(|c| c.model.panes.clone());
        let (depth, root) = lineage(&panes, pane);
        if creates_pane(method) && depth + 1 > l.max_spawn_depth {
            let e = err(
                ErrorKind::PermissionDenied,
                format!(
                    "spawn_depth_exceeded: agents may nest {} levels deep",
                    l.max_spawn_depth
                ),
            )
            .details(json!({"reason": "spawn_depth_exceeded", "depth": depth + 1, "max": l.max_spawn_depth, "scope": "pane"}));
            return Err(refuse(server, pane, method, "depth", e));
        }
        if starts_agent(method, p) {
            let live = server.with_core(|c| {
                c.model
                    .runs
                    .iter()
                    .filter(|r| r.ended_at_ms.is_none() && r.pane != root)
                    .filter(|r| lineage(&panes, &r.pane).1 == root)
                    .count()
            });
            if live >= l.max_descendant_runs {
                let e = err(
                    ErrorKind::PermissionDenied,
                    format!(
                        "descendant_runs_exceeded: {live} live agent runs under this root (max {})",
                        l.max_descendant_runs
                    ),
                )
                .details(json!({"reason": "descendant_runs_exceeded", "live": live, "max": l.max_descendant_runs, "scope": "pane"}));
                return Err(refuse(server, pane, method, "descendants", e));
            }
            let window = Duration::from_secs(60);
            let wait = {
                let mut st = state().lock().unwrap();
                let q = st.spawns.entry(pane.to_string()).or_default();
                while q.front().is_some_and(|t| now.duration_since(*t) >= window) {
                    q.pop_front();
                }
                if q.len() >= l.spawns_per_min {
                    q.front()
                        .map(|t| (window - now.duration_since(*t)).as_millis() as u64)
                } else {
                    q.push_back(now);
                    None
                }
            };
            if let Some(ms) = wait {
                let e = err(
                    ErrorKind::RateLimited,
                    format!(
                        "rate_limited: at most {} agent starts per minute from a pane",
                        l.spawns_per_min
                    ),
                )
                .details(json!({"limit": "spawn", "retry_after_ms": ms, "scope": "pane"}));
                return Err(refuse(server, pane, method, "spawn", e));
            }
        }
    }
    Ok(())
}

/// Forget a closed pane's budgets.
pub fn forget(pane: &str) {
    let mut st = state().lock().unwrap();
    st.buckets.remove(pane);
    st.spawns.remove(pane);
    st.notified.remove(pane);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: &str, by: &str) -> Pane {
        Pane {
            id: id.into(),
            handle: id.into(),
            tab: "t".into(),
            workspace: "w".into(),
            title: None,
            auto_title: String::new(),
            cwd: None,
            cols: 80,
            rows: 24,
            child_pid: None,
            fg_cmdline: vec![],
            exited: false,
            exit_code: None,
            unread: false,
            marked_unread: false,
            pinned: false,
            created_by: by.into(),
            recovered: None,
            isolation: Default::default(),
            browser: None,
        }
    }

    #[test]
    fn lineage_counts_agent_links() {
        let panes = vec![
            pane("a", "user"),
            pane("b", "agent:a"),
            pane("c", "agent:b"),
            pane("d", "agent:gone"),
        ];
        assert_eq!(lineage(&panes, "a"), (0, "a".into()));
        assert_eq!(lineage(&panes, "b"), (1, "a".into()));
        assert_eq!(lineage(&panes, "c"), (2, "a".into()));
        assert_eq!(lineage(&panes, "d"), (1, "d".into()));
    }

    #[test]
    fn limits_from_config() {
        let (cfg, _) = vk_config::Config::parse(
            "[security.limits]\nburst = 5\nrate = 0.5\nmax_spawn_depth = 1\n",
            std::path::Path::new("x"),
        )
        .unwrap();
        let l = Limits::from_config(&cfg);
        assert_eq!(l.burst, 5.0);
        assert_eq!(l.rate, 0.5);
        assert_eq!(l.max_spawn_depth, 1);
        assert_eq!(l.spawns_per_min, 10);
    }
}
