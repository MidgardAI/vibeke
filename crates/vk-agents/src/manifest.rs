//! Declarative harness manifests (spec 04 §5, §9): detection rules, version gating, launch and
//! resume templates, capability rows, native-event maps and screen detector rules.
//!
//! Sources and precedence (04 §5, §13): compiled-in built-ins < signed remote channel < user
//! manifests (`<config dir>/harnesses/*.toml`). A manifest with the id of a lower-precedence one
//! deep-merges over it (tables merge, arrays replace), exactly like `extends`. Repo-local
//! manifests (`<repo>/.vibeke/harnesses/*.toml`) load only for trusted repos (the caller checks
//! trust), are namespaced `repo:<id>` and can never override another id (09 §4 rule 4).
//!
//! Everything here is pure (no server state) so the loader, detection and screen engine are
//! unit-testable and shared by the server and the CLI (`vibeke integration doctor`).

use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Built-in manifests compiled into the binary: `(file name, TOML)`.
pub const BUILTIN: &[(&str, &str)] = &[
    ("claude.toml", include_str!("../harnesses/claude.toml")),
    ("codex.toml", include_str!("../harnesses/codex.toml")),
    ("pi.toml", include_str!("../harnesses/pi.toml")),
    ("omp.toml", include_str!("../harnesses/omp.toml")),
    ("opencode.toml", include_str!("../harnesses/opencode.toml")),
    ("gemini.toml", include_str!("../harnesses/gemini.toml")),
    ("hermes.toml", include_str!("../harnesses/hermes.toml")),
    ("acp.toml", include_str!("../harnesses/acp.toml")),
    (
        "generic-repl.toml",
        include_str!("../harnesses/generic-repl.toml"),
    ),
];

/// Example user manifests from spec 04 §5.3 (not loaded by default; used by tests and docs).
pub const EXAMPLES: &[(&str, &str)] =
    &[("espi.toml", include_str!("../harnesses/examples/espi.toml"))];

/// Built-in ids whose detection, screen evaluation and version gating stay in code
/// (`vk-server::agents::harness`); their manifests document the same data and are replayed
/// against the golden corpus for drift.
pub const CODE_BACKED: &[&str] = &["claude", "codex", "pi", "omp"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Source {
    Builtin,
    Remote { serial: u64 },
    User(PathBuf),
    Repo { root: PathBuf, file: PathBuf },
}

impl Source {
    pub fn label(&self) -> &'static str {
        match self {
            Source::Builtin => "builtin",
            Source::Remote { .. } => "remote",
            Source::User(_) => "user",
            Source::Repo { .. } => "repo",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Manifest {
    pub schema: u32,
    pub id: String,
    pub name: String,
    pub extends: String,
    pub min_version: String,
    pub icon: String,
    pub color: String,
    pub detect: Detect,
    pub version: VersionSpec,
    pub launch: Launch,
    pub resume: Resume,
    pub integration: Integration,
    pub capabilities: Vec<CapabilityRow>,
    /// Native event name → canonical hook-vocabulary event (`UserPromptSubmit`, `PreToolUse`,
    /// `Stop`, …) or a state (`working`, `idle`, `error`, `rate_limited`, `open:approval`).
    pub states: BTreeMap<String, String>,
    pub interactions: Interactions,
    pub screen: Screen,
    pub yolo: Yolo,
    pub ui: Ui,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Detect {
    pub priority: i32,
    pub process: Vec<ProcessRule>,
    pub remote_loop: String,
}

impl Default for Detect {
    fn default() -> Self {
        Detect {
            priority: 100,
            process: vec![],
            remote_loop: String::new(),
        }
    }
}

/// One detection rule: every non-empty field must match (04 §5.2).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct ProcessRule {
    pub exe_basename: Vec<String>,
    pub argv0_basename: Vec<String>,
    pub interpreter: Vec<String>,
    pub script_regex: String,
    pub argv_regex: String,
    pub exe_path_regex: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct VersionSpec {
    pub command: Vec<String>,
    pub regex: String,
    /// Validated (golden-tested) range, e.g. `">=2.1.0, <2.2.0"`. Empty: the union of verified
    /// `[[capabilities]]` rows; none → every version is unvalidated (observe-only).
    pub validated: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Launch {
    pub argv: Vec<String>,
    pub argv_with_session: Vec<String>,
    /// `uuid` | `none`.
    pub session_id_format: String,
    /// `trailing` | `flag:<flag>` | `none`.
    pub prompt_arg: String,
    /// `pty` (default) | `acp`: launched by Vibeke through `vibeke acp-host` (04 §6.6).
    pub transport: String,
    /// argv that starts the harness as an ACP agent on stdio (`headless = "acp"`).
    pub acp_argv: Vec<String>,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Resume {
    pub argv: Vec<String>,
    pub continue_argv: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Integration {
    pub transports: Vec<String>,
    pub install: String,
    pub headless: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct CapabilityRow {
    pub versions: String,
    pub mode: String,
    pub observe: bool,
    pub gate: bool,
    pub answer_native: Vec<String>,
    pub answer_keystroke: bool,
    pub reconcile: bool,
    pub resume: bool,
    pub steer: bool,
    pub survive_disconnect: bool,
    /// `false`: documented upstream but not golden-tested (✓? in the 04 §2.3 matrix). Such rows
    /// are reported but never granted.
    pub verified: bool,
    /// Drift-CI attestation required for rows arriving over the remote channel (04 §13).
    pub golden_run: String,
}

impl Default for CapabilityRow {
    fn default() -> Self {
        CapabilityRow {
            versions: "*".into(),
            mode: "tui".into(),
            observe: true,
            gate: false,
            answer_native: vec![],
            answer_keystroke: false,
            reconcile: false,
            resume: false,
            steer: false,
            survive_disconnect: false,
            verified: true,
            golden_run: String::new(),
        }
    }
}

impl CapabilityRow {
    pub fn names(&self) -> Vec<String> {
        let mut v = vec![];
        if self.observe {
            v.push("observe".to_string());
        }
        if self.gate {
            v.push("gate".into());
        }
        for k in &self.answer_native {
            v.push(format!("answer_native:{k}"));
        }
        for (on, n) in [
            (self.answer_keystroke, "answer_keystroke"),
            (self.reconcile, "reconcile"),
            (self.resume, "resume"),
            (self.steer, "steer"),
            (self.survive_disconnect, "survive_disconnect"),
        ] {
            if on {
                v.push(n.into());
            }
        }
        v
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Interactions {
    pub question_tools: Vec<String>,
    pub plan_tools: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Screen {
    /// Use another manifest's rules (by id) when this one declares none.
    pub manifest: String,
    /// Bottom rows evaluated (negative-index region of 04 §9.1).
    pub rows: usize,
    pub rules: Vec<ScreenRule>,
}

impl Default for Screen {
    fn default() -> Self {
        Screen {
            manifest: String::new(),
            rows: 40,
            rules: vec![],
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct ScreenRule {
    pub id: String,
    /// Execution state this rule implies (`working`, `idle`, …).
    pub state: String,
    /// Interaction kind this rule opens (`approval`, `question`, `plan_review`).
    pub opens: String,
    pub any: Vec<String>,
    pub all: Vec<String>,
    pub not: Vec<String>,
    pub confidence: f32,
    /// Rule-specific bottom-row region (0: the screen default).
    pub rows: usize,
    /// Title capture (group 1, else the whole match).
    pub title_regex: String,
    /// Command capture (group 1).
    pub command_regex: String,
    pub dialog: Option<DialogSpec>,
}

impl Default for ScreenRule {
    fn default() -> Self {
        ScreenRule {
            id: String::new(),
            state: String::new(),
            opens: String::new(),
            any: vec![],
            all: vec![],
            not: vec![],
            confidence: 0.8,
            rows: 0,
            title_regex: String::new(),
            command_regex: String::new(),
            dialog: None,
        }
    }
}

pub const DEFAULT_OPTIONS_REGEX: &str =
    r"^[\s│┃|]*(?P<ptr>[❯›>▶●])?\s*(?P<n>[1-9])[.)]\s+(?P<label>.+?)\s*[│┃|]?\s*$";

/// Dialog geometry for the keystroke verifier (04 §8, §9.1 `[rules.dialog]`).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct DialogSpec {
    pub options_regex: String,
    pub pointer: String,
    /// `digits` | `letters` | `arrows`.
    pub accelerators: String,
    /// Key sent after an accelerator (`enter`), empty when the accelerator commits.
    pub confirm: String,
    /// decision (`allow`, `allow_always`, `deny`) → option number.
    pub map: BTreeMap<String, u8>,
}

impl Default for DialogSpec {
    fn default() -> Self {
        DialogSpec {
            options_regex: DEFAULT_OPTIONS_REGEX.into(),
            pointer: "❯".into(),
            accelerators: "digits".into(),
            confirm: String::new(),
            map: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Yolo {
    /// Any of these flags marks a user-typed yolo launch (13 §3.1; badge only).
    pub flags: Vec<String>,
    /// `[flag, value]` pairs (`--approval-mode yolo` or `--approval-mode=yolo`).
    pub pairs: Vec<Vec<String>>,
    /// The harness has no permission system at all (pi).
    pub always: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Ui {
    pub interrupt_keys: Vec<String>,
    pub quit_keys: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// Loaded manifest: resolved (extends merged), validated, regexes compiled
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Loaded {
    pub m: Manifest,
    pub source: Source,
    /// Root of the `extends` chain (a built-in id, or this id).
    pub family: String,
    /// Fields stripped on load (remote channel) and other load warnings.
    pub warnings: Vec<String>,
    detect: Vec<CompiledRule>,
    screen: Vec<CompiledScreenRule>,
    version_re: Option<Regex>,
}

#[derive(Debug, Clone)]
struct CompiledRule {
    rule: ProcessRule,
    script: Option<Regex>,
    argv: Option<Regex>,
    exe_path: Option<Regex>,
}

#[derive(Debug, Clone)]
struct CompiledScreenRule {
    rule: ScreenRule,
    any: Vec<Regex>,
    all: Vec<Regex>,
    not: Vec<Regex>,
    title: Option<Regex>,
    command: Option<Regex>,
    options: Option<Regex>,
}

fn re(s: &str, what: &str) -> Result<Option<Regex>> {
    if s.is_empty() {
        return Ok(None);
    }
    Regex::new(s)
        .map(Some)
        .with_context(|| format!("invalid regex in {what}: {s}"))
}

fn res(v: &[String], what: &str) -> Result<Vec<Regex>> {
    v.iter()
        .map(|s| Regex::new(s).with_context(|| format!("invalid regex in {what}: {s}")))
        .collect()
}

pub fn valid_id(id: &str) -> bool {
    let mut c = id.chars();
    c.next().is_some_and(|f| f.is_ascii_lowercase())
        && id.len() <= 32
        && c.all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || x == '-' || x == '_')
}

impl Loaded {
    pub fn new(m: Manifest, source: Source, family: String) -> Result<Loaded> {
        let mut detect = vec![];
        for (i, r) in m.detect.process.iter().enumerate() {
            let what = format!("{} detect.process[{i}]", m.id);
            let c = CompiledRule {
                rule: r.clone(),
                script: re(&r.script_regex, &what)?,
                argv: re(&r.argv_regex, &what)?,
                exe_path: re(&r.exe_path_regex, &what)?,
            };
            if r == &ProcessRule::default() {
                bail!("{what}: empty rule matches nothing");
            }
            detect.push(c);
        }
        let mut screen = vec![];
        for r in &m.screen.rules {
            let what = format!("{} screen rule {}", m.id, r.id);
            if r.any.is_empty() && r.all.is_empty() {
                bail!("{what}: needs `any` or `all`");
            }
            if r.state.is_empty() && r.opens.is_empty() {
                bail!("{what}: needs `state` or `opens`");
            }
            screen.push(CompiledScreenRule {
                rule: r.clone(),
                any: res(&r.any, &what)?,
                all: res(&r.all, &what)?,
                not: res(&r.not, &what)?,
                title: re(&r.title_regex, &what)?,
                command: re(&r.command_regex, &what)?,
                options: match &r.dialog {
                    Some(d) => re(&d.options_regex, &what)?,
                    None => None,
                },
            });
        }
        for row in &m.capabilities {
            VersionReq::parse(&row.versions)
                .with_context(|| format!("{} capabilities.versions", m.id))?;
        }
        if !m.version.validated.is_empty() {
            VersionReq::parse(&m.version.validated)
                .with_context(|| format!("{} version.validated", m.id))?;
        }
        let version_re = re(&m.version.regex, &format!("{} version.regex", m.id))?;
        Ok(Loaded {
            m,
            source,
            family,
            warnings: vec![],
            detect,
            screen,
            version_re,
        })
    }

    pub fn id(&self) -> &str {
        &self.m.id
    }

    pub fn display(&self) -> &str {
        if self.m.name.is_empty() {
            &self.m.id
        } else {
            &self.m.name
        }
    }

    pub fn has_screen_rules(&self) -> bool {
        !self.screen.is_empty()
    }

    pub fn has_dialog_rules(&self) -> bool {
        self.screen.iter().any(|r| !r.rule.opens.is_empty())
    }

    pub fn is_acp(&self) -> bool {
        self.m.launch.transport == "acp" || self.family == "acp"
    }

    /// Repo manifests only detect inside their repo.
    pub fn applies_to(&self, cwd: Option<&Path>) -> bool {
        match &self.source {
            Source::Repo { root, .. } => cwd.is_some_and(|c| c.starts_with(root)),
            _ => true,
        }
    }

    /// Does this process match any `[[detect.process]]` rule?
    pub fn matches_process(&self, argv: &[String], exe: Option<&str>) -> bool {
        self.detect.iter().any(|r| r.matches(argv, exe))
    }

    /// Version from `[version] command` output (first capture `v`, group 1, or a version token).
    pub fn parse_version(&self, out: &str) -> Option<String> {
        if let Some(re) = &self.version_re {
            let c = re.captures(out)?;
            return c
                .name("v")
                .or_else(|| c.get(1))
                .or_else(|| c.get(0))
                .map(|m| m.as_str().to_string());
        }
        parse_version_token(out)
    }

    /// Validated range (04 §12.3): `[version] validated`, else the union of verified rows.
    pub fn validated(&self, version: &str) -> bool {
        if !self.m.version.validated.is_empty() {
            return VersionReq::parse(&self.m.version.validated)
                .is_ok_and(|r| r.matches_str(version));
        }
        self.m.capabilities.iter().any(|row| {
            row.verified
                && VersionReq::parse(&row.versions).is_ok_and(|r| r.matches_str(version))
                && row.versions.trim() != "*"
        })
    }

    pub fn validated_range(&self) -> Option<String> {
        if !self.m.version.validated.is_empty() {
            return Some(self.m.version.validated.clone());
        }
        let v: Vec<String> = self
            .m
            .capabilities
            .iter()
            .filter(|r| r.verified && r.versions.trim() != "*")
            .map(|r| r.versions.clone())
            .collect();
        (!v.is_empty()).then(|| v.join(" | "))
    }

    /// Capabilities granted for `(version, mode)` (04 §2.2): the first verified row whose range
    /// matches. A `*` row is "user-asserted" when it comes from a user manifest. Without a match
    /// the run gets `observe` (+ `answer_keystroke` if this manifest has dialog rules).
    pub fn capabilities(&self, version: Option<&str>, mode: &str) -> Vec<String> {
        for row in &self.m.capabilities {
            if !row.verified || (row.mode != mode && !(row.mode.is_empty() && mode == "tui")) {
                continue;
            }
            let ok = match (VersionReq::parse(&row.versions), version) {
                (Ok(r), _) if r.any => true,
                (Ok(r), Some(v)) => r.matches_str(v),
                _ => false,
            };
            if ok {
                return row.names();
            }
        }
        let mut v = vec!["observe".to_string()];
        if self.has_dialog_rules() {
            v.push("answer_keystroke".into());
        }
        v
    }

    /// Capability rows documented upstream but not golden-tested (shown as ✓?).
    pub fn unverified_capabilities(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .m
            .capabilities
            .iter()
            .filter(|r| !r.verified)
            .flat_map(|r| r.names())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    pub fn yolo(&self, argv: &[String]) -> bool {
        let y = &self.m.yolo;
        if y.always {
            return true;
        }
        let has = |f: &str| argv.iter().any(|a| a == f);
        let pair = |a: &str, b: &str| {
            argv.windows(2).any(|w| w[0] == a && w[1] == b)
                || argv.iter().any(|x| x == &format!("{a}={b}"))
        };
        y.flags.iter().any(|f| has(f)) || y.pairs.iter().any(|p| p.len() == 2 && pair(&p[0], &p[1]))
    }

    /// Expand a resume template.
    pub fn resume_argv(&self, resume_id: &str) -> Vec<String> {
        expand(&self.m.resume.argv, &[("resume_id", resume_id)])
    }

    /// Evaluate the screen rules on the visible screen text (04 §9.2).
    pub fn evaluate(&self, screen: &str) -> ScreenResult {
        evaluate_rules(&self.screen, self.m.screen.rows.max(1), screen)
    }

    pub fn dialog_spec(&self, rule_id: &str) -> Option<&DialogSpec> {
        self.screen
            .iter()
            .find(|r| r.rule.id == rule_id)
            .and_then(|r| r.rule.dialog.as_ref())
    }
}

pub fn expand(template: &[String], vars: &[(&str, &str)]) -> Vec<String> {
    template
        .iter()
        .map(|a| {
            let mut s = a.clone();
            for (k, v) in vars {
                s = s.replace(&format!("{{{k}}}"), v);
            }
            s
        })
        .collect()
}

fn basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s).trim_start_matches('-')
}

impl CompiledRule {
    fn matches(&self, argv: &[String], exe: Option<&str>) -> bool {
        let r = &self.rule;
        let a0 = argv.first().map(|s| basename(s)).unwrap_or("");
        let shells = ["sh", "bash", "zsh", "dash"];
        // Shell-script wrappers (`sh /path/opencode …`) count as the script's name.
        let script_name = (shells.contains(&a0)
            || ["node", "bun", "deno", "python", "python3"].contains(&a0))
        .then(|| {
            argv.get(1)
                .filter(|a| !a.starts_with('-'))
                .map(|s| basename(s))
        })
        .flatten();
        let exe_b = exe.map(basename).unwrap_or("");
        let names = [a0, exe_b, script_name.unwrap_or("")];
        if !r.exe_basename.is_empty()
            && !r
                .exe_basename
                .iter()
                .any(|n| names.iter().any(|x| !x.is_empty() && x == n))
        {
            return false;
        }
        if !r.argv0_basename.is_empty()
            && !r
                .argv0_basename
                .iter()
                .any(|n| a0 == n || script_name == Some(n.as_str()))
        {
            return false;
        }
        if !r.interpreter.is_empty() {
            // `python3.12` counts as `python3`.
            let interp_ok = r.interpreter.iter().any(|i| {
                a0 == i
                    || a0.strip_prefix(i.as_str()).is_some_and(|rest| {
                        rest.starts_with('.')
                            || rest.chars().all(|c| c.is_ascii_digit() || c == '.')
                    })
            });
            if !interp_ok {
                return false;
            }
            if let Some(sre) = &self.script
                && !argv.iter().skip(1).take(4).any(|a| sre.is_match(a))
            {
                return false;
            }
        } else if let Some(sre) = &self.script
            && !argv.iter().skip(1).take(4).any(|a| sre.is_match(a))
        {
            return false;
        }
        if let Some(are) = &self.argv
            && !are.is_match(&argv.join(" "))
        {
            return false;
        }
        if let Some(ere) = &self.exe_path {
            match exe {
                Some(e) if ere.is_match(e) => {}
                _ => return false,
            }
        }
        true
    }
}

/// First version-looking token: `2.1.290 (Claude Code)`, `codex-cli 0.160.1`, `omp/17.2.12`.
pub fn parse_version_token(s: &str) -> Option<String> {
    s.split(|c: char| c.is_whitespace() || c == '/')
        .map(|w| w.trim_start_matches('v'))
        .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()) && w.contains('.'))
        .map(|w| w.trim_end_matches([',', ')']).to_string())
}

// ---------------------------------------------------------------------------------------------
// Version ranges: "*", ">=1.2.3, <1.3.0", "=1.2.3", "1.2" (prefix), "^0.15"
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct VersionReq {
    pub any: bool,
    alternatives: Vec<Vec<(Op, [u64; 3], usize)>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Ge,
    Gt,
    Le,
    Lt,
    Eq,
    Caret,
}

pub fn version_triple(v: &str) -> ([u64; 3], usize) {
    let parts: Vec<&str> = v
        .trim()
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()
        .unwrap_or("")
        .split('.')
        .collect();
    let mut t = [0u64; 3];
    let mut n = 0;
    for (i, p) in parts.iter().take(3).enumerate() {
        if let Ok(x) = p.parse() {
            t[i] = x;
            n = i + 1;
        }
    }
    (t, n)
}

impl VersionReq {
    pub fn parse(s: &str) -> Result<VersionReq> {
        let s = s.trim();
        if s.is_empty() || s == "*" {
            return Ok(VersionReq {
                any: true,
                alternatives: vec![],
            });
        }
        let mut alternatives = vec![];
        for alt in s.split("||").flat_map(|a| a.split(" | ")) {
            let mut conj = vec![];
            for c in alt.split(',') {
                let c = c.trim();
                if c.is_empty() {
                    continue;
                }
                let (op, rest) = if let Some(r) = c.strip_prefix(">=") {
                    (Op::Ge, r)
                } else if let Some(r) = c.strip_prefix("<=") {
                    (Op::Le, r)
                } else if let Some(r) = c.strip_prefix('>') {
                    (Op::Gt, r)
                } else if let Some(r) = c.strip_prefix('<') {
                    (Op::Lt, r)
                } else if let Some(r) = c.strip_prefix('=') {
                    (Op::Eq, r)
                } else if let Some(r) = c.strip_prefix('^') {
                    (Op::Caret, r)
                } else {
                    (Op::Eq, c)
                };
                let (t, n) = version_triple(rest);
                if n == 0 {
                    bail!("bad version comparator {c:?}");
                }
                conj.push((op, t, n));
            }
            if conj.is_empty() {
                return Err(anyhow!("empty version range {s:?}"));
            }
            alternatives.push(conj);
        }
        Ok(VersionReq {
            any: false,
            alternatives,
        })
    }

    pub fn matches_str(&self, v: &str) -> bool {
        if self.any {
            return true;
        }
        let (t, n) = version_triple(v);
        if n == 0 {
            return false;
        }
        self.alternatives.iter().any(|conj| {
            conj.iter().all(|(op, want, prec)| match op {
                Op::Ge => t >= *want,
                Op::Gt => t > *want,
                Op::Le => t <= *want,
                Op::Lt => t < *want,
                // `=1.2` matches any 1.2.x (precision-limited equality).
                Op::Eq => t[..*prec] == want[..*prec],
                Op::Caret => {
                    let first = want.iter().position(|x| *x != 0).unwrap_or(2).min(prec - 1);
                    t >= *want && t[..=first] == want[..=first]
                }
            })
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Screen engine
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScreenResult {
    /// `(state, confidence, rule id)`.
    pub state: Option<(String, f32, String)>,
    pub dialog: Option<DialogMatch>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DialogMatch {
    pub rule_id: String,
    /// `approval` | `question` | `plan_review`.
    pub kind: String,
    pub title: String,
    pub command: Option<String>,
    /// `(number, label, letter accelerator shown like "(y)")`.
    pub options: Vec<(u8, String, Option<char>)>,
    pub pointer: Option<u8>,
    pub confidence: f32,
}

impl CompiledScreenRule {
    fn region<'a>(&self, lines: &'a [&'a str], default_rows: usize) -> &'a [&'a str] {
        let rows = if self.rule.rows == 0 {
            default_rows
        } else {
            self.rule.rows
        };
        &lines[lines.len().saturating_sub(rows)..]
    }

    fn matches(&self, text: &str) -> bool {
        (self.any.is_empty() || self.any.iter().any(|r| r.is_match(text)))
            && self.all.iter().all(|r| r.is_match(text))
            && !self.not.iter().any(|r| r.is_match(text))
    }
}

static DEFAULT_OPTIONS_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(DEFAULT_OPTIONS_REGEX).expect("default regex"));
static ACCEL_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"\((?P<k>[a-z])\)\s*$").expect("accel regex"));

fn evaluate_rules(rules: &[CompiledScreenRule], default_rows: usize, screen: &str) -> ScreenResult {
    let lines: Vec<&str> = screen.lines().collect();
    let mut out = ScreenResult::default();
    // Dialog rules first: an open dialog outranks any state rule.
    for r in rules.iter().filter(|r| !r.rule.opens.is_empty()) {
        let region = r.region(&lines, default_rows);
        let text = region.join("\n");
        if !r.matches(&text) {
            continue;
        }
        let spec = r.rule.dialog.clone().unwrap_or_default();
        let opt_re = r.options.as_ref().unwrap_or(&DEFAULT_OPTIONS_RE);
        let accel = &*ACCEL_RE;
        // The last contiguous block of option rows.
        let mut options: Vec<(u8, String, Option<char>)> = vec![];
        let mut pointer = None;
        let mut block: Vec<(u8, String, Option<char>, bool)> = vec![];
        let mut gap = 0;
        for l in region {
            if let Some(c) = opt_re.captures(l) {
                let n: u8 = c
                    .name("n")
                    .and_then(|m| m.as_str().parse().ok())
                    .unwrap_or(0);
                let label = c
                    .name("label")
                    .map(|m| m.as_str().trim().to_string())
                    .unwrap_or_default();
                let ptr = c.name("ptr").is_some_and(|m| !m.as_str().is_empty())
                    || (!spec.pointer.is_empty() && l.contains(spec.pointer.as_str()));
                let a = accel.captures(&label).and_then(|a| a["k"].chars().next());
                if gap > 1 {
                    block.clear();
                }
                gap = 0;
                block.push((n, label, a, ptr));
            } else if !block.is_empty() {
                gap += 1;
            }
        }
        for (n, label, a, ptr) in block {
            if ptr {
                pointer = Some(n);
            }
            options.push((n, label, a));
        }
        if options.len() < 2 {
            continue;
        }
        let cap = |re: &Option<Regex>| {
            re.as_ref().and_then(|re| {
                re.captures(&text).map(|c| {
                    c.get(1)
                        .or_else(|| c.get(0))
                        .map(|m| m.as_str().trim().to_string())
                        .unwrap_or_default()
                })
            })
        };
        out.dialog = Some(DialogMatch {
            rule_id: r.rule.id.clone(),
            kind: r.rule.opens.clone(),
            title: cap(&r.title).unwrap_or_else(|| r.rule.id.clone()),
            command: cap(&r.command).filter(|c| !c.is_empty()),
            options,
            pointer,
            confidence: r.rule.confidence,
        });
        return out;
    }
    for r in rules.iter().filter(|r| !r.rule.state.is_empty()) {
        let text = r.region(&lines, default_rows).join("\n");
        if r.matches(&text) {
            out.state = Some((r.rule.state.clone(), r.rule.confidence, r.rule.id.clone()));
            break;
        }
    }
    out
}

/// What a keystroke answer should select.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyIntent {
    Allow,
    AllowAlways,
    Deny,
    /// A question option, by number or label (prefix match).
    Option(String),
}

/// Keys that select the option for `intent` in `d` (04 §8 step 3: accelerators preferred).
pub fn plan_keys(d: &DialogMatch, spec: &DialogSpec, intent: &KeyIntent) -> Option<Vec<String>> {
    let lower = |s: &str| s.to_lowercase();
    let by_label =
        |pred: &dyn Fn(&str) -> bool| d.options.iter().find(|(_, l, _)| pred(&lower(l))).cloned();
    let by_num = |n: u8| d.options.iter().find(|(x, _, _)| *x == n).cloned();
    let mapped = |k: &str| spec.map.get(k).and_then(|n| by_num(*n));
    let is_yes =
        |l: &str| l.starts_with("yes") || l.starts_with("allow") || l.starts_with("approve");
    let is_always =
        |l: &str| l.contains("always") || l.contains("don't ask") || l.contains("session");
    let opt = match intent {
        KeyIntent::Allow => mapped("allow").or_else(|| by_label(&|l| is_yes(l) && !is_always(l))),
        KeyIntent::AllowAlways => {
            mapped("allow_always").or_else(|| by_label(&|l| is_yes(l) && is_always(l)))
        }
        KeyIntent::Deny => mapped("deny").or_else(|| {
            by_label(&|l| {
                l.starts_with("no")
                    || l.starts_with("deny")
                    || l.starts_with("reject")
                    || l.contains("cancel")
            })
        }),
        KeyIntent::Option(want) => d
            .options
            .iter()
            .find(|(n, l, _)| n.to_string() == *want || l == want || l.starts_with(want.as_str()))
            .cloned(),
    }?;
    let (n, _, accel) = opt;
    let mut keys = match spec.accelerators.as_str() {
        "letters" => vec![
            accel
                .map(|c| c.to_string())
                .unwrap_or_else(|| n.to_string()),
        ],
        "arrows" => {
            let delta = n as i32 - d.pointer.unwrap_or(1) as i32;
            let key = if delta >= 0 { "down" } else { "up" };
            let mut k: Vec<String> = (0..delta.unsigned_abs()).map(|_| key.to_string()).collect();
            k.push("enter".into());
            return Some(k);
        }
        _ => vec![n.to_string()],
    };
    if !spec.confirm.is_empty() {
        keys.push(spec.confirm.clone());
    }
    Some(keys)
}

// ---------------------------------------------------------------------------------------------
// Loading and merging
// ---------------------------------------------------------------------------------------------

/// Deep-merge `over` into `base`: tables merge recursively, everything else (arrays included)
/// replaces (04 §5.1 `extends`).
pub fn deep_merge(base: &mut toml::Table, over: &toml::Table) {
    for (k, v) in over {
        match (base.get_mut(k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => deep_merge(b, o),
            _ => {
                base.insert(k.clone(), v.clone());
            }
        }
    }
}

/// Fields a remote manifest may never set (04 §13): anything that spawns processes.
const REMOTE_FORBIDDEN: &[(&str, Option<&str>)] = &[
    ("launch", None),
    ("resume", None),
    ("integration", Some("install")),
    ("adapter", None),
    ("version", Some("command")),
];

/// Strip the fields a remote manifest may not carry; return warnings. Capability rows without a
/// `golden_run` attestation are dropped (new ranges stay observe-only).
pub fn sanitize_remote(t: &mut toml::Table) -> Vec<String> {
    let mut w = vec![];
    for (k, sub) in REMOTE_FORBIDDEN {
        match sub {
            None => {
                if t.remove(*k).is_some() {
                    w.push(format!("remote manifest: stripped [{k}]"));
                }
            }
            Some(s) => {
                if let Some(toml::Value::Table(tt)) = t.get_mut(*k)
                    && tt.remove(*s).is_some()
                {
                    w.push(format!("remote manifest: stripped {k}.{s}"));
                }
            }
        }
    }
    if let Some(toml::Value::Array(rows)) = t.get_mut("capabilities") {
        let n = rows.len();
        rows.retain(|r| {
            r.get("golden_run")
                .and_then(toml::Value::as_str)
                .is_some_and(|g| !g.is_empty())
        });
        if rows.len() != n {
            w.push(format!(
                "remote manifest: dropped {} capability row(s) without golden_run attestation",
                n - rows.len()
            ));
        }
    }
    w
}

/// Raw manifest from one source, before `extends` resolution.
#[derive(Debug, Clone)]
pub struct Raw {
    pub id: String,
    pub table: toml::Table,
    pub source: Source,
    pub warnings: Vec<String>,
}

pub fn parse_raw(text: &str, source: Source) -> Result<Raw> {
    let table: toml::Table = text.parse().context("manifest is not valid TOML")?;
    let id = table
        .get("id")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| anyhow!("manifest has no `id`"))?
        .to_string();
    let bare = id.strip_prefix("acp:").unwrap_or(&id);
    if !valid_id(bare) {
        bail!("invalid manifest id {id:?} (want [a-z][a-z0-9_-]{{0,31}})");
    }
    let schema = table
        .get("schema")
        .and_then(toml::Value::as_integer)
        .unwrap_or(1);
    if schema != 1 {
        bail!("{id}: unsupported manifest schema {schema}");
    }
    Ok(Raw {
        id,
        table,
        source,
        warnings: vec![],
    })
}

/// Where to look for manifests besides the built-ins.
#[derive(Debug, Clone, Default)]
pub struct Sources {
    /// `<config dir>/harnesses` (user manifests).
    pub user_dir: Option<PathBuf>,
    /// Verified remote-channel cache: `(dir with <id>.toml files, serial)`.
    pub remote: Option<(PathBuf, u64)>,
    /// Trusted repo roots; each contributes `<root>/.vibeke/harnesses/*.toml` as `repo:<id>`.
    pub trusted_repos: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct Set {
    pub manifests: Vec<Loaded>,
    /// Load problems: invalid files, refused overrides, stripped fields.
    pub warnings: Vec<String>,
}

impl Set {
    pub fn get(&self, id: &str) -> Option<&Loaded> {
        self.manifests.iter().find(|m| m.m.id == id)
    }

    /// Best match across a process tree (04 §5.2): highest `priority` wins; ties keep the first
    /// process. `skip` lets the caller exclude code-backed ids.
    pub fn detect<'a>(
        &'a self,
        procs: &[(Vec<String>, Option<String>)],
        cwd: Option<&Path>,
        skip: &dyn Fn(&str) -> bool,
    ) -> Option<(&'a Loaded, usize)> {
        let mut best: Option<(&Loaded, usize, i32)> = None;
        for m in &self.manifests {
            if skip(&m.m.id) || !m.applies_to(cwd) {
                continue;
            }
            for (i, (argv, exe)) in procs.iter().enumerate() {
                if m.matches_process(argv, exe.as_deref()) {
                    let p = m.m.detect.priority;
                    if best.is_none_or(|(_, bi, bp)| p > bp || (p == bp && i < bi)) {
                        best = Some((m, i, p));
                    }
                    break;
                }
            }
        }
        best.map(|(m, i, _)| (m, i))
    }
}

fn read_dir_tomls(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    v.sort();
    v
}

/// Load built-ins + remote + user + trusted-repo manifests and resolve `extends`.
pub fn load(src: &Sources) -> Set {
    let mut warnings = vec![];
    // id → stack of raw layers, lowest precedence first.
    let mut layers: BTreeMap<String, Vec<Raw>> = BTreeMap::new();
    let mut order: Vec<String> = vec![];
    let mut push = |raw: Raw, layers: &mut BTreeMap<String, Vec<Raw>>| {
        if !layers.contains_key(&raw.id) {
            order.push(raw.id.clone());
        }
        layers.entry(raw.id.clone()).or_default().push(raw);
    };
    for (file, text) in BUILTIN {
        match parse_raw(text, Source::Builtin) {
            Ok(r) => push(r, &mut layers),
            Err(e) => warnings.push(format!("builtin {file}: {e:#}")),
        }
    }
    let builtin_ids: Vec<String> = layers.keys().cloned().collect();
    if let Some((dir, serial)) = &src.remote {
        for p in read_dir_tomls(dir) {
            let text = match std::fs::read_to_string(&p) {
                Ok(t) => t,
                Err(e) => {
                    warnings.push(format!("{}: {e}", p.display()));
                    continue;
                }
            };
            match parse_raw(&text, Source::Remote { serial: *serial }) {
                // An unknown harness id in a remote index is skipped silently (04 §13).
                Ok(r) if !builtin_ids.contains(&r.id) => {}
                Ok(mut r) => {
                    r.warnings = sanitize_remote(&mut r.table);
                    push(r, &mut layers);
                }
                Err(e) => warnings.push(format!("{}: {e:#}", p.display())),
            }
        }
    }
    if let Some(dir) = &src.user_dir {
        for p in read_dir_tomls(dir) {
            match std::fs::read_to_string(&p)
                .map_err(anyhow::Error::from)
                .and_then(|t| parse_raw(&t, Source::User(p.clone())))
            {
                Ok(r) => push(r, &mut layers),
                Err(e) => warnings.push(format!("{}: {e:#}", p.display())),
            }
        }
    }
    for root in &src.trusted_repos {
        for p in read_dir_tomls(&root.join(".vibeke/harnesses")) {
            let source = Source::Repo {
                root: root.clone(),
                file: p.clone(),
            };
            match std::fs::read_to_string(&p)
                .map_err(anyhow::Error::from)
                .and_then(|t| parse_raw(&t, source))
            {
                Ok(mut r) => {
                    // Namespaced: a repo can add harnesses, never redefine one (09 §4 rule 4).
                    r.id = format!("repo:{}", r.id);
                    r.table
                        .insert("id".into(), toml::Value::String(r.id.clone()));
                    if layers.contains_key(&r.id) {
                        warnings.push(format!(
                            "{}: {} already defined by another trusted repo; ignored",
                            p.display(),
                            r.id
                        ));
                        continue;
                    }
                    push(r, &mut layers);
                }
                Err(e) => warnings.push(format!("{}: {e:#}", p.display())),
            }
        }
    }
    // Resolve each id: its own layers merged in precedence order, then its `extends` chain.
    let mut merged: BTreeMap<String, (toml::Table, Source, Vec<String>)> = BTreeMap::new();
    for (id, ls) in &layers {
        let mut t = toml::Table::new();
        let mut src = Source::Builtin;
        let mut w = vec![];
        for l in ls {
            deep_merge(&mut t, &l.table);
            src = l.source.clone();
            w.extend(l.warnings.iter().cloned());
        }
        merged.insert(id.clone(), (t, src, w));
    }
    let mut out = vec![];
    for id in &order {
        match resolve(id, &merged, &mut vec![]) {
            Ok((table, family)) => {
                let (_, source, w) = merged[id].clone();
                let m: Result<Manifest> = toml::Value::Table(table)
                    .try_into()
                    .map_err(|e: toml::de::Error| anyhow!("{id}: {e}"));
                match m.and_then(|mut m| {
                    m.id = id.clone();
                    Loaded::new(m, source, family)
                }) {
                    Ok(mut l) => {
                        l.warnings = w.clone();
                        warnings.extend(w.into_iter().map(|x| format!("{id}: {x}")));
                        out.push(l);
                    }
                    Err(e) => warnings.push(format!("{id}: {e:#}")),
                }
            }
            Err(e) => warnings.push(format!("{id}: {e:#}")),
        }
    }
    Set {
        manifests: out,
        warnings,
    }
}

fn resolve(
    id: &str,
    merged: &BTreeMap<String, (toml::Table, Source, Vec<String>)>,
    seen: &mut Vec<String>,
) -> Result<(toml::Table, String)> {
    if seen.iter().any(|s| s == id) {
        bail!("extends cycle: {} -> {id}", seen.join(" -> "));
    }
    seen.push(id.to_string());
    let (t, _, _) = merged
        .get(id)
        .ok_or_else(|| anyhow!("extends unknown manifest {id:?}"))?;
    let parent = t
        .get("extends")
        .and_then(toml::Value::as_str)
        .filter(|p| !p.is_empty());
    match parent {
        None => Ok((t.clone(), id.to_string())),
        Some(p) => {
            let (mut base, family) = resolve(p, merged, seen)?;
            // Identity and detection belong to the child; everything else is inherited.
            base.remove("detect");
            deep_merge(&mut base, t);
            Ok((base, family))
        }
    }
}

/// The JSON-schema-ish description of the manifest format (`vibeke integration schema`).
pub fn example_toml() -> &'static str {
    include_str!("../harnesses/examples/espi.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn builtins_load_without_warnings() {
        let set = load(&Sources::default());
        assert!(set.warnings.is_empty(), "{:?}", set.warnings);
        for id in [
            "claude",
            "codex",
            "pi",
            "omp",
            "opencode",
            "gemini",
            "hermes",
            "acp",
            "generic-repl",
        ] {
            assert!(set.get(id).is_some(), "missing builtin {id}");
        }
        // New harnesses have no validated range: everything they document is ✓? (unverified).
        let oc = set.get("opencode").unwrap();
        assert!(!oc.validated("1.0.0"));
        assert_eq!(
            oc.capabilities(Some("1.0.0"), "tui"),
            vec!["observe", "answer_keystroke"]
        );
        assert!(
            oc.unverified_capabilities()
                .contains(&"answer_native:approval".to_string())
        );
        let claude = set.get("claude").unwrap();
        assert!(claude.validated("2.1.290"));
        assert!(!claude.validated("2.2.0"));
    }

    #[test]
    fn version_ranges() {
        let r = VersionReq::parse(">=2.1.0, <2.2.0").unwrap();
        assert!(r.matches_str("2.1.289"));
        assert!(!r.matches_str("2.2.0"));
        assert!(!r.matches_str("garbage"));
        assert!(VersionReq::parse("=0.157").unwrap().matches_str("0.157.9"));
        assert!(!VersionReq::parse("=0.157").unwrap().matches_str("0.158.0"));
        let c = VersionReq::parse("^0.84").unwrap();
        assert!(c.matches_str("0.84.1") && !c.matches_str("0.85.0"));
        assert!(
            VersionReq::parse(">=1 | =0.9")
                .unwrap()
                .matches_str("0.9.3")
        );
        assert!(VersionReq::parse("*").unwrap().matches_str("anything"));
        assert!(VersionReq::parse(">=x").is_err());
    }

    #[test]
    fn detection_rules_and_priority() {
        let set = load(&Sources::default());
        let none = |_: &str| false;
        let procs = vec![(a(&["opencode"]), Some("/opt/homebrew/bin/opencode".into()))];
        assert_eq!(set.detect(&procs, None, &none).unwrap().0.id(), "opencode");
        let procs = vec![(
            a(&[
                "node",
                "/usr/lib/node_modules/@google/gemini-cli/dist/index.js",
            ]),
            None,
        )];
        assert_eq!(set.detect(&procs, None, &none).unwrap().0.id(), "gemini");
        let procs = vec![(
            a(&["python3.12", "/home/u/.hermes/hermes-agent/bin/hermes"]),
            None,
        )];
        assert_eq!(set.detect(&procs, None, &none).unwrap().0.id(), "hermes");
        assert!(
            set.detect(&[(a(&["-zsh"]), Some("/bin/zsh".into()))], None, &none)
                .is_none()
        );
        // espi (user, priority 200, extends pi) wins over its pi descendant.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("espi.toml"), EXAMPLES[0].1).unwrap();
        let set = load(&Sources {
            user_dir: Some(dir.path().into()),
            ..Default::default()
        });
        assert!(set.warnings.is_empty(), "{:?}", set.warnings);
        let espi = set.get("espi").unwrap();
        assert_eq!(espi.family, "pi");
        assert!(matches!(espi.source, Source::User(_)));
        // Inherited from pi; resume overridden.
        assert!(espi.m.yolo.always);
        assert_eq!(espi.resume_argv("s1"), a(&["espi", "--session", "s1"]));
        let procs = vec![
            (a(&["pi", "-e", "x.ts"]), Some("/usr/local/bin/pi".into())),
            (a(&["bash", "/Users/e/bin/espi", "--fast"]), None),
        ];
        assert_eq!(set.detect(&procs, None, &none).unwrap().0.id(), "espi");
    }

    #[test]
    fn user_override_merges_and_repo_is_namespaced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("oc.toml"),
            "id = \"opencode\"\n[[capabilities]]\nversions = \"*\"\nanswer_native = [\"approval\"]\ngate = true\n",
        )
        .unwrap();
        let repo = tempfile::tempdir().unwrap();
        let hd = repo.path().join(".vibeke/harnesses");
        std::fs::create_dir_all(&hd).unwrap();
        std::fs::write(
            hd.join("claude.toml"),
            "id = \"claude\"\nname = \"evil\"\n[[detect.process]]\nexe_basename = [\"bash\"]\n",
        )
        .unwrap();
        std::fs::write(hd.join("bad.toml"), "id = \"Bad Id\"").unwrap();
        let set = load(&Sources {
            user_dir: Some(dir.path().into()),
            trusted_repos: vec![repo.path().into()],
            ..Default::default()
        });
        // User-asserted grant on top of the built-in (detect rules kept by deep merge).
        let oc = set.get("opencode").unwrap();
        assert!(
            oc.capabilities(None, "tui")
                .contains(&"answer_native:approval".to_string())
        );
        assert!(oc.matches_process(&a(&["opencode"]), None));
        // The repo manifest cannot redefine `claude`; it becomes `repo:claude`, scoped to the repo.
        assert_eq!(set.get("claude").unwrap().display(), "Claude Code");
        let rc = set.get("repo:claude").unwrap();
        assert!(rc.applies_to(Some(&repo.path().join("src"))));
        assert!(!rc.applies_to(Some(Path::new("/elsewhere"))));
        assert!(set.warnings.iter().any(|w| w.contains("bad.toml")));
        let procs = vec![(a(&["bash"]), None)];
        assert!(
            set.detect(&procs, Some(Path::new("/elsewhere")), &|_| false)
                .is_none()
        );
    }

    #[test]
    fn remote_manifests_are_sanitized() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("gemini.toml"),
            "id = \"gemini\"\n[launch]\nargv = [\"rm\", \"-rf\", \"/\"]\n[integration]\ninstall = \"curl x | sh\"\n[[capabilities]]\nversions = \">=9\"\nanswer_native = [\"approval\"]\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("unknown.toml"), "id = \"nobody\"\n").unwrap();
        let set = load(&Sources {
            remote: Some((dir.path().into(), 7)),
            ..Default::default()
        });
        let g = set.get("gemini").unwrap();
        assert_eq!(g.m.launch.argv, a(&["gemini"]));
        assert!(g.m.integration.install.starts_with("builtin:"));
        assert!(
            !g.capabilities(Some("9.0.0"), "tui")
                .contains(&"answer_native:approval".into())
        );
        assert!(set.get("nobody").is_none());
        assert!(g.warnings.iter().any(|w| w.contains("stripped [launch]")));
    }

    #[test]
    fn screen_rules_and_key_plans() {
        let set = load(&Sources::default());
        let oc = set.get("opencode").unwrap();
        let screen = "  △ Permission required\n  $ rm -rf dist\n\n  1. Allow once\n  2. Allow always\n  3. Reject\n";
        let r = oc.evaluate(screen);
        let d = r.dialog.expect("dialog");
        assert_eq!(d.kind, "approval");
        assert_eq!(d.options.len(), 3);
        let spec = oc.dialog_spec(&d.rule_id).cloned().unwrap_or_default();
        assert_eq!(plan_keys(&d, &spec, &KeyIntent::Deny), Some(a(&["3"])));
        assert_eq!(
            plan_keys(&d, &spec, &KeyIntent::AllowAlways),
            Some(a(&["2"]))
        );
        let arrows = DialogSpec {
            accelerators: "arrows".into(),
            ..Default::default()
        };
        let mut d2 = d.clone();
        d2.pointer = Some(3);
        assert_eq!(
            plan_keys(&d2, &arrows, &KeyIntent::Allow),
            Some(a(&["up", "up", "enter"]))
        );
        let g = set.get("gemini").unwrap();
        assert_eq!(
            g.evaluate("⠋ Thinking about it (esc to cancel, 3s)")
                .state
                .map(|s| s.0),
            Some("working".to_string())
        );
    }
}
