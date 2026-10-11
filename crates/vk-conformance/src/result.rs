//! The per-plugin result file, `target/herdr-plugins/<stem>-<os>.json` (18 §12). The reporting
//! job reads these files as hostile data (18 §5), so the schema is closed and small:
//! `scripts/herdr-plugins-report.ts` validates exactly this shape.

use crate::lock::DependencyMode;
use crate::outcome::{Outcome, SideResult};
use serde::{Deserialize, Serialize};

pub const SCHEMA: &str = "vibeke.herdr-plugin-result/1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PluginResult {
    pub schema: String,
    pub plugin_id: String,
    pub repository: String,
    pub rank: u32,
    /// `linux` or `macos`.
    pub os: String,
    pub commit: String,
    pub dependency_mode: DependencyMode,
    /// False in unpinned mode: reports label the result non-reproducible.
    pub reproducible: bool,
    /// The worst step or lifecycle outcome, after reconciling known results.
    pub outcome: Outcome,
    /// True when any step is an unexpected pass or the coverage check failed.
    pub fails_run: bool,
    /// Herdr surfaces the plugin uses: `action`, `event:<on>`, `pane:<placement>`, `startup`,
    /// `link`, `socket`.
    pub surfaces: Vec<String>,
    pub coverage: CoverageSummary,
    pub lifecycle: Vec<CheckResult>,
    pub steps: Vec<StepResult>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CoverageSummary {
    pub ok: bool,
    pub missing: Vec<String>,
    pub skipped: Vec<String>,
}

/// One lifecycle assertion (18 §8.3), such as `startup_runs_once`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CheckResult {
    pub check: String,
    pub outcome: Outcome,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StepResult {
    pub id: String,
    pub kind: String,
    pub outcome: Outcome,
    pub vibeke: SideResult,
    pub herdr: SideResult,
    #[serde(default)]
    pub known: Option<Outcome>,
    pub unexpected_pass: bool,
    /// Why the step has its outcome; always set unless `passed`.
    #[serde(default)]
    pub reason: Option<String>,
    /// Short, redacted evidence lines (for example `invocation completed`, `popup projects`).
    pub evidence: Vec<String>,
    /// Paths relative to the result file's directory.
    #[serde(default)]
    pub grids: Vec<GridArtifact>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GridArtifact {
    pub pane: String,
    /// Canonical grid JSON files.
    #[serde(default)]
    pub vibeke: Option<String>,
    #[serde(default)]
    pub herdr: Option<String>,
    /// The rendered side-by-side SVG.
    #[serde(default)]
    pub image: Option<String>,
    /// Unmasked cells that differ; `None` when only one side ran.
    #[serde(default)]
    pub diff_cells: Option<u32>,
    pub placement_matches: Option<bool>,
}

impl PluginResult {
    /// Serialize with the text fields redacted by `vk-redact`.
    pub fn to_json_redacted(&self) -> String {
        todo!("lane MODEL")
    }
}
