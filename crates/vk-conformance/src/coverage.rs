//! The coverage check (18 §8.1): every action, event hook, pane, startup command and link
//! handler a manifest declares for the current platform appears in a scenario step, or in an
//! explicit `skip` with a reason. A missing entry fails the run.

use crate::scenario::Scenario;
use vk_compat::herdr::manifest::Manifest;

/// One declared surface. [`Surface::name`] is the spelling `skip.surface` uses.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Surface {
    /// `action:<id>`
    Action(String),
    /// `event:<on>`; one per distinct event name.
    Event(String),
    /// `pane:<id>`
    Pane(String),
    /// `startup:<index>` (0-based, in manifest order)
    Startup(usize),
    /// `link:<id>`
    Link(String),
}

impl Surface {
    pub fn name(&self) -> String {
        todo!("lane MODEL")
    }
}

/// The surfaces `manifest` declares for `os` (`"linux"` or `"macos"`), honouring per-entry
/// `platforms`.
pub fn declared(manifest: &Manifest, os: &str) -> Vec<Surface> {
    todo!("lane MODEL")
}

/// Which surface a step covers, if any. An action step covers `action:<id>`; an event step
/// covers `event:<event>`; a pane step or a `pane`/`popup` expectation covers `pane:<id>`; a
/// link step covers every link handler whose pattern matches its URL; a `ready` expectation, or
/// any step after a successful activation, covers the startup commands.
pub fn covered(scenario: &Scenario, manifest: &Manifest) -> Vec<Surface> {
    todo!("lane MODEL")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageReport {
    /// Declared, but neither covered nor skipped.
    pub missing: Vec<Surface>,
    /// Skipped surfaces that the manifest does not declare (stale skips also fail).
    pub unknown_skips: Vec<String>,
    /// Skipped and also covered (a contradiction that fails too).
    pub skipped_and_covered: Vec<Surface>,
}

impl CoverageReport {
    pub fn ok(&self) -> bool {
        self.missing.is_empty()
            && self.unknown_skips.is_empty()
            && self.skipped_and_covered.is_empty()
    }
}

pub fn check(scenario: &Scenario, manifest: &Manifest, os: &str) -> CoverageReport {
    todo!("lane MODEL")
}
