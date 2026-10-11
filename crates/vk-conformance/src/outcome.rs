//! Step outcomes and the single attribution rule (18 §8.4).

use serde::{Deserialize, Serialize};

/// Ordered from best to worst; a plugin's outcome is its worst step outcome.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Passed,
    NotExercised,
    DependencyFailure,
    SuspectedCompatFailure,
    CompatFailure,
    HarnessError,
}

/// What one side did with a step.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SideResult {
    Passed,
    Failed,
    /// The side did not run the step (for Herdr: not run yet in this mode).
    NotRun,
    /// The harness, runner or scenario broke while running this side.
    HarnessError,
}

/// Facts the rule needs for one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepFacts {
    pub vibeke: SideResult,
    pub herdr: SideResult,
    /// Both sides received the same dependency bytes: guaranteed in verified mode, compared by
    /// download hashes in unpinned mode. `None` when Herdr did not run (nothing to compare).
    pub same_dependency_bytes: Option<bool>,
    /// A build or external dependency failed on the Vibeke side before the step could run.
    pub dependency_failed: bool,
    /// The result breaks the pinned schema (a `compat_failure` even without a Herdr run).
    pub schema_violation: bool,
}

/// The rule, in order:
/// 0. A harness error on either side is `harness_error`; a dependency failure is
///    `dependency_failure`; a schema violation is `compat_failure`.
/// 1. Different dependency bytes on the two sides: `dependency_failure`.
/// 2. Herdr did not run the step: a Vibeke failure is `suspected_compat_failure`.
/// 3. Herdr passes and Vibeke fails: `compat_failure`.
/// 4. Both fail: `not_exercised`, with the Herdr result as the reason.
/// Otherwise `passed`.
pub fn attribute(f: &StepFacts) -> (Outcome, Option<String>) {
    todo!("lane MODEL")
}

/// The final outcome of a step with a `known` result: a known step that now passes is an
/// unexpected pass and fails the run; a known step whose outcome differs from the recorded one
/// keeps its new outcome (the scenario must be updated).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reconciled {
    pub outcome: Outcome,
    pub unexpected_pass: bool,
    /// The known outcome matched exactly; this step does not fail the run.
    pub matches_known: bool,
}

pub fn reconcile(actual: Outcome, known: Option<Outcome>) -> Reconciled {
    todo!("lane MODEL")
}

/// The worst outcome, `Passed` for an empty list.
pub fn worst(outcomes: impl IntoIterator<Item = Outcome>) -> Outcome {
    todo!("lane MODEL")
}
