//! The per-plugin result file, `target/herdr-plugins/<stem>-<os>.json` (18 §12). The reporting
//! job reads these files as hostile data (18 §5), so the schema is closed and small:
//! `scripts/herdr-plugins-report.ts` validates exactly this shape.

use crate::lock::DependencyMode;
use crate::outcome::{Outcome, SideResult};
use serde::{Deserialize, Serialize};

pub const SCHEMA: &str = "vibeke.herdr-plugin-result/1";

/// The longest text field `scripts/herdr-plugins-report.ts` accepts (`LIMITS.text`), counted
/// in UTF-16 code units as JavaScript counts string length.
pub const REPORT_TEXT_LIMIT: usize = 2000;
/// What the harness keeps of a reason or an evidence line: well under the report's limit, so
/// redaction markers and later additions still fit.
pub const TEXT_MAX: usize = 1800;
/// The report accepts at most this many evidence lines per step (`LIMITS.listItems`).
pub const EVIDENCE_MAX: usize = 200;

/// `s` made safe for the report: terminal escape sequences removed, carriage returns turned
/// into newlines, other control and bidirectional override characters replaced by `?`, and
/// cut to `max` UTF-16 code units. A cut keeps the head and the longer tail (where build tools
/// and stack traces put the error), joined by ` … `.
pub fn clip(s: &str, max: usize) -> String {
    let s = sanitize(s);
    let units = |t: &str| t.encode_utf16().count();
    if units(&s) <= max {
        return s;
    }
    const SEP: &str = " … ";
    let budget = max.saturating_sub(units(SEP));
    let head_budget = budget / 5;
    let tail_budget = budget - head_budget;
    let mut head = String::new();
    let mut n = 0;
    for c in s.chars() {
        if n + c.len_utf16() > head_budget {
            break;
        }
        n += c.len_utf16();
        head.push(c);
    }
    let mut tail = Vec::new();
    let mut n = 0;
    for c in s.chars().rev() {
        if n + c.len_utf16() > tail_budget {
            break;
        }
        n += c.len_utf16();
        tail.push(c);
    }
    let tail: String = tail.into_iter().rev().collect();
    format!("{head}{SEP}{tail}")
}

/// Remove ANSI escape sequences and replace the characters the report refuses.
fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                // CSI (`ESC [ … final`), OSC (`ESC ] … BEL|ST`) or a two-character escape.
                match chars.peek() {
                    Some('[') => {
                        chars.next();
                        for d in chars.by_ref() {
                            if ('@'..='~').contains(&d) {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        chars.next();
                        while let Some(d) = chars.next() {
                            if d == '\u{7}' {
                                break;
                            }
                            if d == '\u{1b}' && chars.peek() == Some(&'\\') {
                                chars.next();
                                break;
                            }
                        }
                    }
                    Some(_) => {
                        chars.next();
                    }
                    None => {}
                }
            }
            '\r' => {
                if chars.peek() != Some(&'\n') {
                    out.push('\n');
                }
            }
            '\t' | '\n' => out.push(c),
            c if (c as u32) < 0x20
                || (0x7f..=0x9f).contains(&(c as u32))
                || ('\u{202a}'..='\u{202e}').contains(&c)
                || ('\u{2066}'..='\u{2069}').contains(&c) =>
            {
                out.push('?')
            }
            c => out.push(c),
        }
    }
    out
}

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
    /// Serialize with the text fields redacted by `vk-redact` and cut to the report's limits
    /// ([`clip`], [`EVIDENCE_MAX`]), so a long build log never makes the whole file invalid.
    pub fn to_json_redacted(&self) -> String {
        let red = |s: &str| clip(&vk_redact::redact(s), TEXT_MAX);
        let mut copy = self.clone();
        for c in &mut copy.lifecycle {
            c.reason = c.reason.as_deref().map(red);
        }
        for s in &mut copy.steps {
            s.reason = s.reason.as_deref().map(red);
            if s.evidence.len() > EVIDENCE_MAX {
                let extra = s.evidence.len() - (EVIDENCE_MAX - 1);
                s.evidence.truncate(EVIDENCE_MAX - 1);
                s.evidence
                    .push(format!("… {extra} more evidence line(s) cut"));
            }
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
    fn clip_keeps_head_and_tail_within_the_report_limit() {
        let long = format!("HEAD{}TAIL", "x".repeat(9000));
        let c = clip(&long, TEXT_MAX);
        assert!(c.encode_utf16().count() <= TEXT_MAX, "{}", c.len());
        assert!(c.starts_with("HEAD") && c.ends_with("TAIL"));
        assert!(c.contains(" … "));
        assert_eq!(clip("short", TEXT_MAX), "short");
        // Wide characters count as two UTF-16 units, as in JavaScript.
        let wide = "😀".repeat(3000);
        assert!(clip(&wide, TEXT_MAX).encode_utf16().count() <= TEXT_MAX);
    }

    #[test]
    fn clip_removes_control_characters() {
        let s = "\u{1b}[31merror\u{1b}[0m: bad\r\nnext\rline\u{7}\u{202e}x\u{1b}]0;title\u{7}end";
        assert_eq!(clip(s, TEXT_MAX), "error: bad\nnext\nline??xend");
    }

    #[test]
    fn long_fields_are_cut_on_write() {
        let mut r = result();
        r.lifecycle[0].reason = Some("y".repeat(8179));
        r.steps[0].evidence = (0..300).map(|i| format!("line {i}")).collect();
        let back: PluginResult = serde_json::from_str(&r.to_json_redacted()).unwrap();
        let reason = back.lifecycle[0].reason.as_deref().unwrap();
        assert!(reason.encode_utf16().count() <= REPORT_TEXT_LIMIT);
        assert_eq!(back.steps[0].evidence.len(), EVIDENCE_MAX);
        assert!(back.steps[0].evidence.last().unwrap().contains("101 more"));
    }

    #[test]
    fn closed_schema_rejects_extra_fields() {
        let mut v = serde_json::to_value(result()).unwrap();
        v["extra"] = serde_json::json!(1);
        assert!(serde_json::from_value::<PluginResult>(v).is_err());
    }
}
