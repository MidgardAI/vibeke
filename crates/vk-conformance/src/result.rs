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
        let red = |s: &str| vk_redact::redact(s).into_owned();
        let mut copy = self.clone();
        for c in &mut copy.lifecycle {
            c.reason = c.reason.as_deref().map(red);
        }
        for s in &mut copy.steps {
            s.reason = s.reason.as_deref().map(red);
            for e in &mut s.evidence {
                *e = red(e);
            }
        }
        let mut v = serde_json::to_value(&copy).expect("a plugin result is plain data");
        vk_redact::redact_json(&mut v);
        serde_json::to_string_pretty(&v).expect("a JSON value serializes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";

    fn result() -> PluginResult {
        PluginResult {
            schema: SCHEMA.into(),
            plugin_id: "acme.demo".into(),
            repository: "acme/demo".into(),
            rank: 1,
            os: "linux".into(),
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
            dependency_mode: DependencyMode::Unpinned,
            reproducible: false,
            outcome: Outcome::CompatFailure,
            fails_run: false,
            surfaces: vec!["action".into(), "pane:popup".into()],
            coverage: CoverageSummary {
                ok: true,
                missing: vec![],
                skipped: vec!["action:doctor".into()],
            },
            lifecycle: vec![CheckResult {
                check: "startup_runs_once".into(),
                outcome: Outcome::Passed,
                reason: Some(format!("token {SECRET}")),
            }],
            steps: vec![StepResult {
                id: "open".into(),
                kind: "action".into(),
                outcome: Outcome::CompatFailure,
                vibeke: SideResult::Failed,
                herdr: SideResult::Passed,
                known: None,
                unexpected_pass: false,
                reason: Some(format!("failed with {SECRET}")),
                evidence: vec![format!("popup {SECRET}"), "invocation completed".into()],
                grids: vec![GridArtifact {
                    pane: "main".into(),
                    vibeke: Some("open-main-vibeke.json".into()),
                    herdr: None,
                    image: None,
                    diff_cells: None,
                    placement_matches: Some(true),
                }],
                duration_ms: 12,
            }],
            duration_ms: 34,
        }
    }

    #[test]
    fn redacted_json_round_trips_without_secrets() {
        let r = result();
        let json = r.to_json_redacted();
        assert!(!json.contains(SECRET), "{json}");
        assert!(json.contains("invocation completed"));
        assert!(json.contains('\n'), "pretty-printed");
        let back: PluginResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.plugin_id, r.plugin_id);
        assert_eq!(back.steps[0].outcome, Outcome::CompatFailure);
        assert_eq!(back.steps[0].herdr, SideResult::Passed);
        assert_eq!(back.dependency_mode, DependencyMode::Unpinned);
        assert_eq!(back.schema, SCHEMA);
    }

    #[test]
    fn closed_schema_rejects_extra_fields() {
        let mut v = serde_json::to_value(result()).unwrap();
        v["extra"] = serde_json::json!(1);
        assert!(serde_json::from_value::<PluginResult>(v).is_err());
    }
}
