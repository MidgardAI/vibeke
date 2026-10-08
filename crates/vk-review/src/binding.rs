//! Task ↔ run bindings and their boundary rules (§4.2).
//!
//! Turn ranges are **half-open**: a binding covers turns `start_turn .. end_turn` (`end_turn`
//! exclusive, `None` = open-ended). Turn numbers are the run's per-run turn ordinals.
//!
//! The binding history is append-only: closing, suspending or continuing never rewrites the
//! covered range of an earlier binding beyond pinning its end; continuations are recorded as
//! new bindings with a [`BindingOrigin`] edge.

use serde::{Deserialize, Serialize};

use crate::{Actor, new_id};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingRole {
    Implementation,
    Review,
    Verification,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingState {
    /// Observations from covered turns are attributed to the task.
    Active,
    /// Automatic association stopped at a native conversation boundary; the user is offered
    /// **Continue task** or **Track new work**. Covered range is pinned (`end_turn` set).
    Suspended,
    /// Explicitly closed (switch/unbind). Covered range is pinned.
    Closed,
}

/// How a binding came to exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BindingOrigin {
    /// Explicit user `task.track` / `task.bind`.
    Explicit,
    /// A queued or immediate binding switch closed `from` and opened this one.
    Switch { from: String },
    /// User chose **Continue task** after a conversation boundary suspended `from`.
    ContinueAfterBoundary { from: String },
    /// Verified reboot/same-session resume moved the binding from a previous run.
    ResumeContinuation { from: String },
    /// A subagent inherited its parent's binding via structured parent identity.
    SubagentOf { parent_binding: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRunBinding {
    pub id: String,
    pub task_id: String,
    pub run_id: String,
    pub native_conversation_id: String,
    pub role: BindingRole,
    pub start_turn: u32,
    /// Exclusive end; `None` while open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_turn: Option<u32>,
    pub state: BindingState,
    pub origin: BindingOrigin,
    pub actor: Actor,
    pub created_at_ms: i64,
}

impl TaskRunBinding {
    /// Whether `turn` falls inside this binding's covered range.
    pub fn covers(&self, turn: u32) -> bool {
        turn >= self.start_turn && self.end_turn.is_none_or(|e| turn < e)
    }

    fn overlaps(&self, start: u32, end: Option<u32>) -> bool {
        let a_end = self.end_turn.unwrap_or(u32::MAX);
        let b_end = end.unwrap_or(u32::MAX);
        self.start_turn < b_end && start < a_end
    }

    /// Pin the end boundary (exclusive) and change state.
    fn pinned(&self, end_turn: u32, state: BindingState) -> TaskRunBinding {
        let mut b = self.clone();
        b.end_turn = Some(end_turn.max(self.start_turn));
        b.state = state;
        b
    }
}

/// Whether the run identity was established deterministically per spec 04 (pane token,
/// hook-reported session, Vibeke launch). Guessed/shared-daemon associations are not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityEvidence {
    pub deterministic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindRequest {
    pub task_id: String,
    pub run_id: String,
    pub native_conversation_id: String,
    pub role: BindingRole,
    pub start_turn: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_turn: Option<u32>,
    pub actor: Actor,
}

/// Refusals map to 07's `conflict` error with the given `reason`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum BindError {
    /// `reason=binding_unverified`: suggested/unlinked run identity; **Link run** first.
    #[error("run identity is not deterministically verified; link the run first")]
    BindingUnverified,
    /// `reason=binding_changed`: another foreground binding already covers these turns.
    #[error("turns overlap existing binding {existing}")]
    BindingChanged { existing: String },
    #[error("empty turn range")]
    EmptyRange,
}

impl BindError {
    /// The structured `conflict` reason string (§10.2).
    pub fn reason(&self) -> &'static str {
        match self {
            BindError::BindingUnverified => "binding_unverified",
            BindError::BindingChanged { .. } => "binding_changed",
            BindError::EmptyRange => "invalid_range",
        }
    }
}

/// Create an explicit binding. Refuses unverified identities and any overlap with another
/// binding of the same run (at most one foreground binding per run turn, whatever the task or
/// role, including suspended/closed bindings whose pinned range covers the turns).
pub fn bind(
    existing: &[TaskRunBinding],
    req: BindRequest,
    identity: IdentityEvidence,
    now_ms: i64,
) -> Result<TaskRunBinding, BindError> {
    if !identity.deterministic {
        return Err(BindError::BindingUnverified);
    }
    if req.end_turn.is_some_and(|e| e <= req.start_turn) {
        return Err(BindError::EmptyRange);
    }
    if let Some(b) = existing
        .iter()
        .find(|b| b.run_id == req.run_id && b.overlaps(req.start_turn, req.end_turn))
    {
        return Err(BindError::BindingChanged {
            existing: b.id.clone(),
        });
    }
    Ok(TaskRunBinding {
        id: new_id(),
        task_id: req.task_id,
        run_id: req.run_id,
        native_conversation_id: req.native_conversation_id,
        role: req.role,
        start_turn: req.start_turn,
        end_turn: req.end_turn,
        state: BindingState::Active,
        origin: BindingOrigin::Explicit,
        actor: req.actor,
        created_at_ms: now_ms,
    })
}

/// Where the run is relative to a turn boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TurnPosition {
    /// Turn `turn` is in progress; its checks/interactions stay with the current binding.
    Running { turn: u32 },
    /// Between turns; the next turn will be `next_turn`.
    Idle { next_turn: u32 },
}

/// A binding switch queued while a turn runs; shown as pending until the next turn boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingSwitch {
    pub from_binding: String,
    pub to_task: String,
    pub role: BindingRole,
    /// The running turn at request time; the switch takes effect at `after_turn + 1` or later.
    pub after_turn: u32,
    pub requested_by: Actor,
    pub requested_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "plan", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // short-lived return value
pub enum SwitchPlan {
    /// Idle: close the current binding at `next_turn` and open the new one there.
    Immediate {
        closed: TaskRunBinding,
        opened: TaskRunBinding,
    },
    /// A turn is running: nothing changes until [`complete_switch`] at the next boundary.
    Pending(PendingSwitch),
}

/// **Start another task in this session** (§4.2).
pub fn queue_switch(
    current: &TaskRunBinding,
    to_task: &str,
    role: BindingRole,
    position: TurnPosition,
    actor: Actor,
    now_ms: i64,
) -> SwitchPlan {
    match position {
        TurnPosition::Running { turn } => SwitchPlan::Pending(PendingSwitch {
            from_binding: current.id.clone(),
            to_task: to_task.to_string(),
            role,
            after_turn: turn,
            requested_by: actor,
            requested_at_ms: now_ms,
        }),
        TurnPosition::Idle { next_turn } => {
            let (closed, opened) = switch_at(current, to_task, role, next_turn, actor, now_ms);
            SwitchPlan::Immediate { closed, opened }
        }
    }
}

/// Apply a pending switch at a turn boundary. `next_turn` is the first turn of the new binding;
/// it is clamped so the in-flight turn recorded in `pending.after_turn` stays with the old task.
pub fn complete_switch(
    pending: &PendingSwitch,
    current: &TaskRunBinding,
    next_turn: u32,
    now_ms: i64,
) -> (TaskRunBinding, TaskRunBinding) {
    let boundary = next_turn.max(pending.after_turn + 1);
    switch_at(
        current,
        &pending.to_task,
        pending.role,
        boundary,
        pending.requested_by.clone(),
        now_ms,
    )
}

fn switch_at(
    current: &TaskRunBinding,
    to_task: &str,
    role: BindingRole,
    boundary: u32,
    actor: Actor,
    now_ms: i64,
) -> (TaskRunBinding, TaskRunBinding) {
    let closed = current.pinned(boundary, BindingState::Closed);
    let opened = TaskRunBinding {
        id: new_id(),
        task_id: to_task.to_string(),
        run_id: current.run_id.clone(),
        native_conversation_id: current.native_conversation_id.clone(),
        role,
        start_turn: closed.end_turn.unwrap_or(boundary),
        end_turn: None,
        state: BindingState::Active,
        origin: BindingOrigin::Switch {
            from: current.id.clone(),
        },
        actor,
        created_at_ms: now_ms,
    };
    (closed, opened)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuspendOffer {
    ContinueTask,
    TrackNew,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum BindingDecision {
    /// Same native conversation (compaction, same-session resume): no boundary.
    Keep,
    /// Different or unknown native conversation (`/clear`, `/new`, fork, `/resume` other):
    /// suspend automatic association and offer the choices.
    Suspend { offers: Vec<SuspendOffer> },
}

/// Boundaries follow native conversation identity, not command names. An unknown new identity
/// (`None`) is treated as a boundary.
pub fn on_conversation_change(
    binding: &TaskRunBinding,
    new_native_id: Option<&str>,
) -> BindingDecision {
    if new_native_id == Some(binding.native_conversation_id.as_str()) {
        BindingDecision::Keep
    } else {
        BindingDecision::Suspend {
            offers: vec![SuspendOffer::ContinueTask, SuspendOffer::TrackNew],
        }
    }
}

/// Pin a binding at a conversation boundary; `boundary_turn` is the first turn of the new
/// conversation. Pending interactions keep the association they had when opened (callers key
/// them by the turn they were opened in, which stays covered).
pub fn suspend(binding: &TaskRunBinding, boundary_turn: u32) -> TaskRunBinding {
    binding.pinned(boundary_turn, BindingState::Suspended)
}

/// The user chose **Continue task** after a boundary: a new binding in the new conversation
/// with a continuation edge.
pub fn continue_after_boundary(
    suspended: &TaskRunBinding,
    new_native_id: &str,
    start_turn: u32,
    actor: Actor,
    now_ms: i64,
) -> TaskRunBinding {
    TaskRunBinding {
        id: new_id(),
        task_id: suspended.task_id.clone(),
        run_id: suspended.run_id.clone(),
        native_conversation_id: new_native_id.to_string(),
        role: suspended.role,
        start_turn: start_turn.max(suspended.end_turn.unwrap_or(start_turn)),
        end_turn: None,
        state: BindingState::Active,
        origin: BindingOrigin::ContinueAfterBoundary {
            from: suspended.id.clone(),
        },
        actor,
        created_at_ms: now_ms,
    }
}

/// Facts about a run relevant to resume continuation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFacts {
    pub run_id: String,
    pub harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    pub repo_root: String,
    pub owner: String,
    pub active: bool,
    /// Identity established deterministically (verified resume handle, or an observed
    /// same-session restart under the same checks).
    pub identity_verified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ContinuationDecision {
    /// Exactly one predecessor matched: record this new binding (continuation edge).
    Continue { binding: TaskRunBinding },
    /// Several predecessors matched (concurrent resumes): explicit resolution required.
    Ambiguous { candidates: Vec<String> },
    /// No verified match; an explicit link is needed.
    NoMatch,
}

/// Vibeke-controlled reboot/task resume (§4.2). A predecessor binding transfers when its run's
/// harness, native session id, repository and owner match the new run, the old run is no
/// longer active and both identities are verified. cwd/title/prompt similarity is never used.
pub fn resume_continuation(
    predecessors: &[(TaskRunBinding, RunFacts)],
    new_run: &RunFacts,
    start_turn: u32,
    now_ms: i64,
) -> ContinuationDecision {
    if !new_run.identity_verified || new_run.native_session_id.is_none() {
        return ContinuationDecision::NoMatch;
    }
    let matches: Vec<&TaskRunBinding> = predecessors
        .iter()
        .filter(|(b, old)| {
            b.state != BindingState::Closed
                && b.run_id == old.run_id
                && old.run_id != new_run.run_id
                && old.identity_verified
                && !old.active
                && old.harness == new_run.harness
                && old.native_session_id == new_run.native_session_id
                && old.native_session_id.as_deref() == Some(b.native_conversation_id.as_str())
                && old.repo_root == new_run.repo_root
                && old.owner == new_run.owner
        })
        .map(|(b, _)| b)
        .collect();
    match matches.as_slice() {
        [] => ContinuationDecision::NoMatch,
        [b] => ContinuationDecision::Continue {
            binding: TaskRunBinding {
                id: new_id(),
                task_id: b.task_id.clone(),
                run_id: new_run.run_id.clone(),
                native_conversation_id: b.native_conversation_id.clone(),
                role: b.role,
                start_turn,
                end_turn: None,
                state: BindingState::Active,
                origin: BindingOrigin::ResumeContinuation { from: b.id.clone() },
                actor: Actor::system("resume"),
                created_at_ms: now_ms,
            },
        },
        many => ContinuationDecision::Ambiguous {
            candidates: many.iter().map(|b| b.id.clone()).collect(),
        },
    }
}

/// Structured parent identity reported by the adapter for a subagent run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentIdentity {
    pub parent_run_id: String,
    /// Parent turn during which the subagent was spawned.
    pub parent_turn_at_spawn: u32,
}

/// Subagents inherit the parent's binding at spawn only through structured parent identity
/// (`None` → stays unassigned). The parent binding must be active and cover the spawn turn.
pub fn inherit_for_subagent(
    parent: Option<&ParentIdentity>,
    parent_bindings: &[TaskRunBinding],
    child_run_id: &str,
    child_native_id: &str,
    now_ms: i64,
) -> Option<TaskRunBinding> {
    let p = parent?;
    let pb = parent_bindings.iter().find(|b| {
        b.run_id == p.parent_run_id
            && b.state == BindingState::Active
            && b.covers(p.parent_turn_at_spawn)
    })?;
    Some(TaskRunBinding {
        id: new_id(),
        task_id: pb.task_id.clone(),
        run_id: child_run_id.to_string(),
        native_conversation_id: child_native_id.to_string(),
        role: pb.role,
        start_turn: 0,
        end_turn: None,
        state: BindingState::Active,
        origin: BindingOrigin::SubagentOf {
            parent_binding: pb.id.clone(),
        },
        actor: Actor::system("subagent"),
        created_at_ms: now_ms,
    })
}

/// The binding (if any) that attributes `run`'s `turn` to a task.
pub fn binding_for_turn<'a>(
    bindings: &'a [TaskRunBinding],
    run_id: &str,
    turn: u32,
) -> Option<&'a TaskRunBinding> {
    bindings
        .iter()
        .find(|b| b.run_id == run_id && b.covers(turn))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(task: &str, start: u32, end: Option<u32>) -> BindRequest {
        BindRequest {
            task_id: task.into(),
            run_id: "run1".into(),
            native_conversation_id: "conv1".into(),
            role: BindingRole::Implementation,
            start_turn: start,
            end_turn: end,
            actor: Actor::user("alice"),
        }
    }
    const OK: IdentityEvidence = IdentityEvidence {
        deterministic: true,
    };

    #[test]
    fn unverified_identity_refused() {
        let e = bind(
            &[],
            req("a", 0, None),
            IdentityEvidence {
                deterministic: false,
            },
            0,
        )
        .unwrap_err();
        assert_eq!(e, BindError::BindingUnverified);
        assert_eq!(e.reason(), "binding_unverified");
    }

    #[test]
    fn overlapping_foreground_bindings_refused() {
        let a = bind(&[], req("a", 0, Some(5)), OK, 0).unwrap();
        // Overlap at turn 4.
        let e = bind(std::slice::from_ref(&a), req("b", 4, None), OK, 0).unwrap_err();
        assert_eq!(e.reason(), "binding_changed");
        // Adjacent (half-open) is fine.
        let b = bind(std::slice::from_ref(&a), req("b", 5, None), OK, 0).unwrap();
        // Open-ended b blocks everything after.
        assert!(bind(&[a.clone(), b.clone()], req("c", 100, Some(101)), OK, 0).is_err());
        // Another run is independent.
        let mut r = req("c", 0, None);
        r.run_id = "run2".into();
        assert!(bind(&[a, b], r, OK, 0).is_ok());
        assert_eq!(
            bind(&[], req("a", 3, Some(3)), OK, 0).unwrap_err(),
            BindError::EmptyRange
        );
    }

    #[test]
    fn switch_while_running_is_pending_and_keeps_in_flight_turn() {
        let a = bind(&[], req("a", 0, None), OK, 0).unwrap();
        let plan = queue_switch(
            &a,
            "b",
            BindingRole::Implementation,
            TurnPosition::Running { turn: 3 },
            Actor::user("alice"),
            10,
        );
        let SwitchPlan::Pending(p) = plan else {
            panic!("expected pending")
        };
        // Turn 3 still belongs to a.
        assert!(a.covers(3));
        // Even if the boundary is reported too early, turn 3 stays with a.
        let (closed, opened) = complete_switch(&p, &a, 3, 20);
        assert_eq!(closed.end_turn, Some(4));
        assert_eq!(closed.state, BindingState::Closed);
        assert!(closed.covers(3) && !closed.covers(4));
        assert_eq!(opened.task_id, "b");
        assert_eq!(opened.start_turn, 4);
        assert_eq!(opened.origin, BindingOrigin::Switch { from: a.id.clone() });
        let all = [closed, opened];
        assert_eq!(binding_for_turn(&all, "run1", 3).unwrap().task_id, "a");
        assert_eq!(binding_for_turn(&all, "run1", 9).unwrap().task_id, "b");
    }

    #[test]
    fn switch_when_idle_is_immediate() {
        let a = bind(&[], req("a", 0, None), OK, 0).unwrap();
        let SwitchPlan::Immediate { closed, opened } = queue_switch(
            &a,
            "b",
            BindingRole::Implementation,
            TurnPosition::Idle { next_turn: 6 },
            Actor::user("alice"),
            1,
        ) else {
            panic!()
        };
        assert_eq!(closed.end_turn, Some(6));
        assert_eq!(opened.start_turn, 6);
    }

    #[test]
    fn clear_new_resume_other_suspend_compaction_keeps() {
        let a = bind(&[], req("a", 0, None), OK, 0).unwrap();
        assert_eq!(
            on_conversation_change(&a, Some("conv1")),
            BindingDecision::Keep
        );
        let d = on_conversation_change(&a, Some("conv2"));
        assert_eq!(
            d,
            BindingDecision::Suspend {
                offers: vec![SuspendOffer::ContinueTask, SuspendOffer::TrackNew]
            }
        );
        assert!(matches!(
            on_conversation_change(&a, None),
            BindingDecision::Suspend { .. }
        ));
        let s = suspend(&a, 7);
        assert_eq!(s.state, BindingState::Suspended);
        assert!(s.covers(6) && !s.covers(7));
        let c = continue_after_boundary(&s, "conv2", 7, Actor::user("alice"), 2);
        assert_eq!(c.native_conversation_id, "conv2");
        assert_eq!(c.task_id, "a");
        assert_eq!(
            c.origin,
            BindingOrigin::ContinueAfterBoundary { from: s.id.clone() }
        );
        // The continuation does not overlap the suspended range.
        assert!(bind(&[s, c], req("z", 6, Some(7)), OK, 0).is_err());
    }

    fn facts(run: &str, active: bool) -> RunFacts {
        RunFacts {
            run_id: run.into(),
            harness: "claude".into(),
            native_session_id: Some("conv1".into()),
            repo_root: "/r".into(),
            owner: "laptop".into(),
            active,
            identity_verified: true,
        }
    }

    #[test]
    fn reboot_resume_continues_only_on_full_match() {
        let a = bind(&[], req("a", 0, None), OK, 0).unwrap();
        let new = facts("run9", true);
        let d = resume_continuation(&[(a.clone(), facts("run1", false))], &new, 0, 5);
        let ContinuationDecision::Continue { binding } = d else {
            panic!("expected continue, got {d:?}")
        };
        assert_eq!(binding.run_id, "run9");
        assert_eq!(binding.task_id, "a");
        assert_eq!(
            binding.origin,
            BindingOrigin::ResumeContinuation { from: a.id.clone() }
        );

        // Old run still active → no transfer.
        assert_eq!(
            resume_continuation(&[(a.clone(), facts("run1", true))], &new, 0, 5),
            ContinuationDecision::NoMatch
        );
        // Different repo / harness / session → no transfer.
        for tweak in [
            |f: &mut RunFacts| f.repo_root = "/other".into(),
            |f: &mut RunFacts| f.harness = "codex".into(),
            |f: &mut RunFacts| f.native_session_id = Some("convX".into()),
            |f: &mut RunFacts| f.owner = "devbox".into(),
            |f: &mut RunFacts| f.identity_verified = false,
        ] {
            let mut old = facts("run1", false);
            tweak(&mut old);
            assert_eq!(
                resume_continuation(&[(a.clone(), old)], &new, 0, 5),
                ContinuationDecision::NoMatch
            );
        }
        // Two possible predecessors → explicit resolution.
        let mut r2 = req("b", 0, None);
        r2.run_id = "run2".into();
        let b = bind(&[], r2, OK, 0).unwrap();
        assert!(matches!(
            resume_continuation(
                &[(a, facts("run1", false)), (b, facts("run2", false))],
                &new,
                0,
                5
            ),
            ContinuationDecision::Ambiguous { .. }
        ));
    }

    #[test]
    fn subagent_inherits_only_with_structured_parent() {
        let a = bind(&[], req("a", 0, None), OK, 0).unwrap();
        assert!(inherit_for_subagent(None, std::slice::from_ref(&a), "sub", "sconv", 0).is_none());
        let p = ParentIdentity {
            parent_run_id: "run1".into(),
            parent_turn_at_spawn: 2,
        };
        let c =
            inherit_for_subagent(Some(&p), std::slice::from_ref(&a), "sub", "sconv", 0).unwrap();
        assert_eq!(c.task_id, "a");
        assert_eq!(
            c.origin,
            BindingOrigin::SubagentOf {
                parent_binding: a.id.clone()
            }
        );
        // Spawned after the parent's binding was closed → unassigned.
        let closed = a.pinned(2, BindingState::Closed);
        assert!(inherit_for_subagent(Some(&p), &[closed], "sub", "sconv", 0).is_none());
    }

    #[test]
    fn serde_tags() {
        let d = BindingDecision::Suspend {
            offers: vec![SuspendOffer::ContinueTask],
        };
        let j = serde_json::to_value(&d).unwrap();
        assert_eq!(j["decision"], "suspend");
        assert_eq!(j["offers"][0], "continue_task");
        let e = serde_json::to_value(BindError::BindingUnverified).unwrap();
        assert_eq!(e["reason"], "binding_unverified");
    }
}
