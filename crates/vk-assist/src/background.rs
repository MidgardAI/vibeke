//! Background planning (14 §10, A3): which opt-in background features have something to do.
//!
//! This is the pure part. It never contacts a provider and never creates a request itself; the
//! coordinator turns a planned action into a request only if the workspace consent **and** the
//! config both list the operation in `auto_send` (a background request has nobody to confirm a
//! preview), through the background profile and the background scheduling class.
//!
//! Rules, all of which must hold:
//! - `[assistant] enabled` and `background_enabled` are on (the master background opt-in);
//! - summaries additionally need `background_summaries`, stall notices `stall_notices`;
//! - summaries are **coalesced per workspace**: at most one per interval, and none while the
//!   workspace's state fingerprint equals the one last summarized;
//! - a stall notice needs a deterministic repetition signal ([`crate::stall`]) and is sent once
//!   per distinct signal, not on every sweep.

use crate::config::AssistConfig;
use crate::stall::{self, Obs, Signal};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct BgState {
    /// Workspace id -> (state fingerprint, time) of the last summary request.
    pub summarized: BTreeMap<String, (String, i64)>,
    /// Run id -> digest of the last stall signal announced.
    pub notified: BTreeMap<String, String>,
}

pub struct WorkspaceView {
    pub id: String,
    /// Digest of what a summary would be about (open interactions, live runs, task states).
    pub fingerprint: String,
    /// Anything worth summarizing at all.
    pub active: bool,
}

pub struct RunView {
    pub id: String,
    pub workspace: String,
    pub obs: Vec<Obs>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Summary {
        workspace: String,
        fingerprint: String,
    },
    Stall {
        run: String,
        workspace: String,
        signal: Signal,
    },
}

pub fn plan(
    cfg: &AssistConfig,
    now_ms: i64,
    state: &BgState,
    workspaces: &[WorkspaceView],
    runs: &[RunView],
) -> Vec<Action> {
    if !cfg.background_active() {
        return vec![];
    }
    let mut out = vec![];
    if cfg.background_summaries {
        let interval = cfg.background_interval_seconds.max(1) as i64 * 1000;
        for w in workspaces.iter().filter(|w| w.active) {
            let due = match state.summarized.get(&w.id) {
                Some((fp, at)) => *fp != w.fingerprint && now_ms - at >= interval,
                None => true,
            };
            if due {
                out.push(Action::Summary {
                    workspace: w.id.clone(),
                    fingerprint: w.fingerprint.clone(),
                });
            }
        }
    }
    if cfg.stall_notices {
        for r in runs {
            let Some(sig) = stall::detect(&r.obs, cfg.stall_repeat_threshold) else {
                continue;
            };
            if state
                .notified
                .get(&r.id)
                .is_some_and(|d| *d == sig.digest())
            {
                continue;
            }
            out.push(Action::Stall {
                run: r.id.clone(),
                workspace: r.workspace.clone(),
                signal: sig,
            });
        }
    }
    out
}

impl BgState {
    pub fn record(&mut self, a: &Action, now_ms: i64) {
        match a {
            Action::Summary {
                workspace,
                fingerprint,
            } => {
                self.summarized
                    .insert(workspace.clone(), (fingerprint.clone(), now_ms));
            }
            Action::Stall { run, signal, .. } => {
                self.notified.insert(run.clone(), signal.digest());
            }
        }
    }

    /// Forget runs and workspaces that no longer exist.
    pub fn retain(&mut self, workspaces: &[String], runs: &[String]) {
        self.summarized.retain(|k, _| workspaces.contains(k));
        self.notified.retain(|k, _| runs.contains(k));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(v: serde_json::Value) -> AssistConfig {
        AssistConfig::from_json(v).unwrap()
    }

    fn ws(id: &str, fp: &str) -> WorkspaceView {
        WorkspaceView {
            id: id.into(),
            fingerprint: fp.into(),
            active: true,
        }
    }

    fn failing(n: usize) -> Vec<Obs> {
        (0..n)
            .map(|i| Obs {
                command: "npm install".into(),
                exit_code: Some(1),
                ended_at_ms: Some(i as i64),
            })
            .collect()
    }

    fn run(id: &str, n: usize) -> RunView {
        RunView {
            id: id.into(),
            workspace: "w1".into(),
            obs: failing(n),
        }
    }

    #[test]
    fn nothing_is_planned_without_the_background_opt_in() {
        let st = BgState::default();
        for c in [
            json!({"enabled": true, "background_summaries": true, "stall_notices": true}),
            json!({"enabled": false, "background_enabled": true, "background_summaries": true}),
            json!({"enabled": true, "background_enabled": false, "stall_notices": true}),
        ] {
            assert!(plan(&cfg(c), 0, &st, &[ws("w1", "a")], &[run("r1", 5)]).is_empty());
        }
        // Each feature has its own switch on top of the master one.
        let master = json!({"enabled": true, "background_enabled": true});
        assert!(plan(&cfg(master), 0, &st, &[ws("w1", "a")], &[run("r1", 5)]).is_empty());
    }

    #[test]
    fn summaries_are_coalesced_per_workspace_and_wait_for_a_change_and_the_interval() {
        let c = cfg(
            json!({"enabled": true, "background_enabled": true, "background_summaries": true, "background_interval_seconds": 100}),
        );
        let mut st = BgState::default();
        let first = plan(&c, 0, &st, &[ws("w1", "a"), ws("w2", "x")], &[]);
        assert_eq!(first.len(), 2);
        for a in &first {
            st.record(a, 0);
        }
        // Unchanged state: nothing, however long it has been.
        assert!(plan(&c, 10_000_000, &st, &[ws("w1", "a"), ws("w2", "x")], &[]).is_empty());
        // Changed state but inside the interval: nothing yet.
        assert!(plan(&c, 50_000, &st, &[ws("w1", "b")], &[]).is_empty());
        // Changed and due: one summary for that workspace only.
        let due = plan(&c, 100_000, &st, &[ws("w1", "b"), ws("w2", "x")], &[]);
        assert_eq!(
            due,
            vec![Action::Summary {
                workspace: "w1".into(),
                fingerprint: "b".into()
            }]
        );
        // An idle workspace is never summarized.
        let mut idle = ws("w3", "z");
        idle.active = false;
        assert!(plan(&c, 0, &BgState::default(), &[idle], &[]).is_empty());
    }

    #[test]
    fn a_stall_is_announced_once_per_signal() {
        let c = cfg(
            json!({"enabled": true, "background_enabled": true, "stall_notices": true, "stall_repeat_threshold": 3}),
        );
        let mut st = BgState::default();
        assert!(
            plan(&c, 0, &st, &[], &[run("r1", 2)]).is_empty(),
            "below threshold"
        );
        let a = plan(&c, 0, &st, &[], &[run("r1", 3)]);
        assert_eq!(a.len(), 1);
        match &a[0] {
            Action::Stall { run, signal, .. } => {
                assert_eq!(run, "r1");
                assert_eq!(signal.repeats, 3);
            }
            other => panic!("{other:?}"),
        }
        st.record(&a[0], 1);
        assert!(
            plan(&c, 2, &st, &[], &[run("r1", 3)]).is_empty(),
            "same loop is not re-announced"
        );
        assert_eq!(
            plan(&c, 3, &st, &[], &[run("r1", 4)]).len(),
            1,
            "a longer loop is new"
        );
        assert_eq!(
            plan(&c, 3, &st, &[], &[run("r2", 3)]).len(),
            1,
            "another run is independent"
        );
    }

    #[test]
    fn state_for_vanished_objects_is_dropped() {
        let mut st = BgState::default();
        st.summarized.insert("w1".into(), ("a".into(), 0));
        st.summarized.insert("w2".into(), ("a".into(), 0));
        st.notified.insert("r1".into(), "d".into());
        st.retain(&["w1".to_string()], &[]);
        assert_eq!(st.summarized.len(), 1);
        assert!(st.notified.is_empty());
    }
}
