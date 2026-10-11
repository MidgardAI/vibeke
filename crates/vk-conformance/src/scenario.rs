//! Scenarios (18 §8): one TOML file per locked plugin at
//! `tests/compat/herdr/0.9.3/plugins/scenarios/<stem>.toml`.
//!
//! ```toml
//! plugin_id = "herdr-projects"
//! repository = "eliasstravik/herdr-projects"
//! timeout_ms = 120000                  # per step unless the step sets its own
//!
//! [setup]
//! fake_agent = true                    # a pane whose agent status the harness can change
//! workspace_cwd = "{repo.app}"         # the context workspace starts in this directory
//! path_exclude = ["/opt/homebrew/bin"] # directories kept off the plugin PATH on both sides
//! github_api = true                    # authenticated GitHub API reads of the plugin's repository
//! [[setup.repo]]
//! name = "app"                         # created under {home}/src/app, `git init` + one commit
//! files = { "README.md" = "hello\n" }
//! worktrees = ["feature"]
//!
//! [[stub]]                             # external services and credentials only, never Herdr
//! id = "notify"
//! kind = "http_sink"                   # http_sink | env | file
//! env = { REMOTE_URL = "{stub.notify.url}" }
//!
//! [[step]]
//! id = "open-popup"
//! kind = "action"                      # see StepKind
//! action = "open-popup"
//! context = "workspace"
//! timeout_ms = 30000
//! expect_calls = [{ method = "popup.open", ok = true }]
//! known = { outcome = "suspected_compat_failure", reason = "…", platforms = ["linux"] }
//! platforms = ["linux", "macos"]       # the step runs only on these platforms (default: all)
//! [[step.expect]]
//! kind = "popup"
//! pane = "projects"
//! [[step.expect]]
//! kind = "screen_text"
//! pane = "projects"
//! text = "Projects"
//! [[step.compare]]                     # visual comparison of one plugin pane (18 §10)
//! pane = "projects"
//! size = { cols = 100, rows = 30 }
//! masks = [{ rect = { row = 0, col = 60, rows = 1, cols = 20 } }, { regex = "\\d+s ago" }]
//!
//! [[skip]]
//! surface = "action:doctor"
//! reason = "…"
//! platforms = ["linux"]                # the skip applies only on these platforms (default: all)
//!
//! [normalize]
//! fields = ["project_id"]
//! ```
//!
//! Placeholders in strings: `{home}`, `{state}` (the plugin state dir), `{config}` (the plugin
//! config dir), `{herdr_config}` (the Herdr config file the side's `herdr` command uses),
//! `{workspace}`, `{tab}`, `{pane}`, `{agent_pane}`, `{repo.<name>}`,
//! `{worktree.<repo>.<name>}`, `{stub.<id>.url}`, `{stub.<id>.file}`. [`expand`] resolves them.
//!
//! Contexts (`step.context`): `global`, `workspace`, `tab`, `pane` (the context pane is the
//! focused pane) and `agent_pane` (the fake agent pane is the focused pane; needs
//! `setup.fake_agent`).

use crate::outcome::Outcome;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub plugin_id: String,
    pub repository: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub setup: Setup,
    #[serde(default, rename = "stub")]
    pub stubs: Vec<Stub>,
    #[serde(default, rename = "step")]
    pub steps: Vec<Step>,
    #[serde(default, rename = "skip")]
    pub skips: Vec<Skip>,
    #[serde(default)]
    pub normalize: NormalizeSpec,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Setup {
    #[serde(default)]
    pub fake_agent: bool,
    #[serde(default, rename = "repo")]
    pub repos: Vec<RepoSetup>,
    /// Extra files under `{home}`, path → contents.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// Plugin config written before install (`{state}/config.toml` or the manifest's config).
    #[serde(default)]
    pub config: Option<String>,
    /// The directory the context workspace starts in, on both sides (for plugins that read the
    /// focused pane's directory, such as a git repository). Placeholders are expanded after
    /// the repositories exist. The default is `{home}`.
    #[serde(default)]
    pub workspace_cwd: Option<String>,
    /// Directories removed from the PATH of plugin processes on both sides, for example a
    /// package manager the plugin user cannot write to.
    #[serde(default)]
    pub path_exclude: Vec<String>,
    /// The plugin reads its own repository from the GitHub API (for example the release its
    /// build installs). The workflow then sends the plugin's downloads through the dependency
    /// proxy also in `unpinned` mode, and the proxy adds the workflow's read-only token to
    /// GET and HEAD requests for that repository's API paths (18 §7.2). The token never
    /// enters the boundary. The harness itself does not read this field.
    #[serde(default)]
    pub github_api: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RepoSetup {
    pub name: String,
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    #[serde(default)]
    pub worktrees: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StubKind {
    /// A local HTTP server that records every request as a JSON line.
    HttpSink,
    /// Only environment variables (fake credentials, disabled telemetry).
    Env,
    /// A file the plugin reads, such as a fake token file.
    File,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Stub {
    pub id: String,
    pub kind: StubKind,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub contents: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// Run a manifest action (`action`, `context`).
    Action,
    /// Make the host emit a Herdr event (`event`), by running `cli` (Herdr CLI arguments that
    /// both sides accept) or by `agent_status` on the fake agent.
    Event,
    /// Open a URL (`url`) through the link handler surface.
    Link,
    /// Kill and restart the server, then wait for it to be ready.
    Restart,
    /// A lifecycle command (`lifecycle`): see [`LifecycleOp`].
    Lifecycle,
    /// Open a declared plugin pane (`pane`) directly.
    Pane,
    /// Send text or keys to a plugin pane (`pane`, `text` or `keys`).
    Input,
    /// Only wait for the expectations (for example a startup command becoming ready).
    Wait,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleOp {
    Install,
    Trust,
    Build,
    Link,
    Unlink,
    Enable,
    Disable,
    Revoke,
    Reinstall,
    Uninstall,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub id: String,
    pub kind: StepKind,
    #[serde(default)]
    pub action: Option<String>,
    /// `global`, `workspace`, `tab` or `pane`.
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub event: Option<String>,
    #[serde(default)]
    pub cli: Vec<String>,
    /// Fake agent status for event steps: `idle`, `working`, `blocked` or `unknown`, the states
    /// `herdr pane report-agent --state` accepts. Herdr derives `done` itself: an agent that
    /// goes from `working` or `blocked` to `idle` while its tab is not the active tab (see the
    /// `agent_status` expectation).
    #[serde(default)]
    pub agent_status: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub lifecycle: Option<LifecycleOp>,
    #[serde(default)]
    pub pane: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub keys: Vec<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub expect_calls: Vec<ExpectedCall>,
    #[serde(default)]
    pub expect: Vec<Expectation>,
    #[serde(default)]
    pub compare: Vec<Compare>,
    #[serde(default)]
    pub known: Option<Known>,
    /// The platforms the step runs on (`linux`, `macos`); `None` is every platform. On other
    /// platforms the step is left out of the run and does not cover its surface.
    #[serde(default)]
    pub platforms: Option<Vec<String>>,
}

/// Whether an optional platform list includes `os` (`None` is every platform).
pub fn on_platform(platforms: Option<&Vec<String>>, os: &str) -> bool {
    platforms.is_none_or(|p| p.iter().any(|x| x == os))
}

impl Step {
    /// Whether the step runs on `os`.
    pub fn runs_on(&self, os: &str) -> bool {
        on_platform(self.platforms.as_ref(), os)
    }

    /// The step's known result on `os`, if it has one there.
    pub fn known_on(&self, os: &str) -> Option<&Known> {
        self.known.as_ref().filter(|k| k.applies_on(os))
    }
}

/// One Herdr call the step should make (18 §8). `ok = false` with a `code` is a call that
/// should fail; the step owns that error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExpectedCall {
    pub method: String,
    #[serde(default = "yes")]
    pub ok: bool,
    #[serde(default)]
    pub code: Option<String>,
    /// At least this many calls (default 1).
    #[serde(default)]
    pub min: Option<u32>,
}

fn yes() -> bool {
    true
}

/// Positive evidence that a step ran (18 §8.2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Expectation {
    /// The invocation reaches a final log state. `status` is `completed` (default) or `failed`.
    InvocationFinal {
        #[serde(default)]
        status: Option<String>,
    },
    /// A plugin pane opens at the declared placement (`split`, `tab`, `overlay`, `zoomed`).
    /// With `reuse = true`, a pane of that entrypoint that an earlier step opened and that is
    /// still open counts too (an action that switches the mode of an open pane opens none).
    Pane {
        pane: String,
        #[serde(default)]
        placement: Option<String>,
        #[serde(default)]
        reuse: bool,
    },
    /// A popup opens, found through the popup surface, not `pane.screenshot`.
    Popup { pane: String },
    /// The pane screen contains `text` (or matches `regex`) once stable.
    ScreenText {
        pane: String,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        regex: Option<String>,
    },
    /// A file exists (and contains `contains`, if set).
    File {
        path: String,
        #[serde(default)]
        contains: Option<String>,
    },
    /// The event hook log contains a line with `text`.
    HookLog { text: String },
    /// A startup command printed `text` (its readiness signal).
    Ready { text: String },
    /// The host recorded an event of this type.
    Event {
        #[serde(rename = "type")]
        event_type: String,
    },
    /// The HTTP sink stub received a request whose method, path or body contains `contains`.
    StubRequest {
        stub: String,
        #[serde(default)]
        contains: Option<String>,
    },
    /// The fake agent pane's `agent_status` in Herdr's `pane.get` is `status`, on both sides.
    /// `done` is the status Herdr derives for an idle agent whose pane was not seen.
    AgentStatus { status: String },
}

/// Visual comparison of one plugin pane (18 §10).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Compare {
    pub pane: String,
    /// Inner PTY size both sides resize the pane to before comparing contents.
    #[serde(default)]
    pub size: Option<GridSize>,
    #[serde(default)]
    pub masks: Vec<crate::mask::Mask>,
    /// Compare placement only (live external data, or sizes that cannot match).
    #[serde(default)]
    pub placement_only: bool,
    /// How long the masked grid must stay unchanged, in ms (default 500).
    #[serde(default)]
    pub stable_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GridSize {
    pub cols: u16,
    pub rows: u16,
}

/// A step that cannot pass yet: its expected outcome and why (18 §8).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Known {
    pub outcome: Outcome,
    pub reason: String,
    /// The platforms the known result applies to; `None` is every platform. On another
    /// platform the step must pass like any other.
    #[serde(default)]
    pub platforms: Option<Vec<String>>,
}

impl Known {
    pub fn applies_on(&self, os: &str) -> bool {
        on_platform(self.platforms.as_ref(), os)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Skip {
    /// A surface name from [`crate::coverage::Surface::name`], such as `action:doctor`.
    pub surface: String,
    pub reason: String,
    /// The platforms the skip applies to; `None` is every platform. On another platform the
    /// surface must be covered by a step.
    #[serde(default)]
    pub platforms: Option<Vec<String>>,
}

impl Skip {
    pub fn applies_on(&self, os: &str) -> bool {
        on_platform(self.platforms.as_ref(), os)
    }
}

/// The platforms a scenario may name.
pub const PLATFORMS: [&str; 2] = ["linux", "macos"];

fn validate_platforms(p: Option<&Vec<String>>, what: &str) -> anyhow::Result<()> {
    let Some(p) = p else { return Ok(()) };
    if p.is_empty() {
        anyhow::bail!("{what}: `platforms` must not be empty (leave it out for every platform)");
    }
    for x in p {
        if !PLATFORMS.contains(&x.as_str()) {
            anyhow::bail!("{what}: unknown platform `{x}` (expected linux or macos)");
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NormalizeSpec {
    /// JSON object keys whose values are generated per run (18 §11).
    #[serde(default)]
    pub fields: Vec<String>,
}

impl Scenario {
    /// Read and validate a scenario file.
    pub fn load(path: &Path) -> anyhow::Result<Scenario> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        let scenario: Scenario =
            toml::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        scenario
            .validate()
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        Ok(scenario)
    }

    /// Checks that do not need the manifest: unique step ids, each step has the fields its
    /// kind needs, known outcomes are not `passed`, stub ids referenced by expectations exist,
    /// regexes compile.
    pub fn validate(&self) -> anyhow::Result<()> {
        use anyhow::bail;
        if self.plugin_id.is_empty() {
            bail!("plugin_id is empty");
        }
        if self.repository.split('/').count() != 2 || self.repository.split('/').any(str::is_empty)
        {
            bail!("repository `{}` is not owner/repo", self.repository);
        }
        if self.timeout_ms == Some(0) {
            bail!("timeout_ms must be positive");
        }
        let mut repos = BTreeSet::new();
        for r in &self.setup.repos {
            if r.name.is_empty() || r.name.contains('/') || !repos.insert(r.name.as_str()) {
                bail!("setup.repo `{}`: empty, nested or duplicate name", r.name);
            }
        }
        let mut stubs = BTreeMap::new();
        for s in &self.stubs {
            if s.id.is_empty() || stubs.insert(s.id.as_str(), s.kind).is_some() {
                bail!("stub `{}`: empty or duplicate id", s.id);
            }
            if s.kind == StubKind::File && s.path.is_none() {
                bail!("stub `{}`: a file stub needs `path`", s.id);
            }
        }
        let mut ids = BTreeSet::new();
        for step in &self.steps {
            if step.id.is_empty() || !ids.insert(step.id.as_str()) {
                bail!("step `{}`: empty or duplicate id", step.id);
            }
            step.validate(&self.setup, &stubs)
                .map_err(|e| anyhow::anyhow!("step `{}`: {e}", step.id))?;
        }
        let mut skipped = BTreeSet::new();
        for s in &self.skips {
            if s.surface.is_empty() || !skipped.insert(s.surface.as_str()) {
                bail!("skip `{}`: empty or duplicate surface", s.surface);
            }
            if s.reason.trim().is_empty() {
                bail!("skip `{}`: a skip needs a reason", s.surface);
            }
            validate_platforms(s.platforms.as_ref(), &format!("skip `{}`", s.surface))?;
        }
        if self.setup.path_exclude.iter().any(|d| !d.starts_with('/')) {
            bail!("setup.path_exclude: every directory must be absolute");
        }
        if self
            .setup
            .workspace_cwd
            .as_deref()
            .is_some_and(|c| c.trim().is_empty())
        {
            bail!("setup.workspace_cwd is empty");
        }
        if self.normalize.fields.iter().any(|f| f.is_empty()) {
            bail!("normalize.fields contains an empty name");
        }
        Ok(())
    }
}

const CONTEXTS: [&str; 5] = ["global", "workspace", "tab", "pane", "agent_pane"];
const AGENT_STATUSES: [&str; 4] = ["idle", "working", "blocked", "unknown"];
/// The `agent_status` values of Herdr's `PaneInfo`: the reported states and the derived `done`.
const PANE_AGENT_STATUSES: [&str; 5] = ["idle", "working", "blocked", "done", "unknown"];

impl Step {
    fn validate(&self, setup: &Setup, stubs: &BTreeMap<&str, StubKind>) -> anyhow::Result<()> {
        use anyhow::bail;
        let set = |o: &Option<String>| o.as_deref().is_some_and(|s| !s.is_empty());
        match self.kind {
            StepKind::Action => {
                if !set(&self.action) {
                    bail!("an action step needs `action`");
                }
            }
            StepKind::Event => {
                if !set(&self.event) {
                    bail!("an event step needs `event`");
                }
                match (self.cli.is_empty(), &self.agent_status) {
                    (false, None) => {}
                    (true, Some(st)) => {
                        if !AGENT_STATUSES.contains(&st.as_str()) {
                            bail!("unknown agent_status `{st}`");
                        }
                        if !setup.fake_agent {
                            bail!("agent_status needs setup.fake_agent = true");
                        }
                    }
                    _ => bail!("an event step needs exactly one of `cli` or `agent_status`"),
                }
            }
            StepKind::Link => {
                if !set(&self.url) {
                    bail!("a link step needs `url`");
                }
            }
            StepKind::Lifecycle => {
                if self.lifecycle.is_none() {
                    bail!("a lifecycle step needs `lifecycle`");
                }
            }
            StepKind::Pane => {
                if !set(&self.pane) {
                    bail!("a pane step needs `pane`");
                }
            }
            StepKind::Input => {
                if !set(&self.pane) {
                    bail!("an input step needs `pane`");
                }
                if self.text.is_none() && self.keys.is_empty() {
                    bail!("an input step needs `text` or `keys`");
                }
            }
            StepKind::Restart | StepKind::Wait => {}
        }
        if let Some(c) = &self.context
            && !CONTEXTS.contains(&c.as_str())
        {
            bail!("unknown context `{c}`");
        }
        if self.context.as_deref() == Some("agent_pane") && !setup.fake_agent {
            bail!("context `agent_pane` needs setup.fake_agent = true");
        }
        validate_platforms(self.platforms.as_ref(), "step")?;
        if self.timeout_ms == Some(0) {
            bail!("timeout_ms must be positive");
        }
        if let Some(k) = &self.known {
            if matches!(k.outcome, Outcome::Passed | Outcome::NotApplicable) {
                bail!("a known result cannot be `passed` or `not_applicable`");
            }
            if k.reason.trim().is_empty() {
                bail!("a known result needs a reason");
            }
            validate_platforms(k.platforms.as_ref(), "known")?;
        }
        for c in &self.expect_calls {
            if c.method.is_empty() {
                bail!("expect_calls entry without a method");
            }
            if c.ok && c.code.is_some() {
                bail!("expected call `{}`: `code` needs `ok = false`", c.method);
            }
            if c.min == Some(0) {
                bail!("expected call `{}`: `min` must be positive", c.method);
            }
        }
        for e in &self.expect {
            e.validate(stubs)?;
            if matches!(e, Expectation::AgentStatus { .. }) && !setup.fake_agent {
                bail!("an agent_status expectation needs setup.fake_agent = true");
            }
        }
        for c in &self.compare {
            if c.pane.is_empty() {
                bail!("compare without a pane");
            }
            for m in &c.masks {
                if let crate::mask::Mask::Regex { regex } = m {
                    regex::Regex::new(regex)
                        .map_err(|e| anyhow::anyhow!("compare `{}`: mask regex: {e}", c.pane))?;
                }
            }
            if let Some(size) = c.size {
                if size.cols == 0 || size.rows == 0 {
                    bail!("compare `{}`: size must be positive", c.pane);
                }
                crate::mask::validate(&c.masks, size.cols, size.rows)
                    .map_err(|e| anyhow::anyhow!("compare `{}`: {e}", c.pane))?;
            }
        }
        Ok(())
    }
}

impl Expectation {
    fn validate(&self, stubs: &BTreeMap<&str, StubKind>) -> anyhow::Result<()> {
        use anyhow::bail;
        match self {
            Expectation::InvocationFinal { status } => {
                if let Some(s) = status
                    && s != "completed"
                    && s != "failed"
                {
                    bail!("invocation_final status `{s}` is not completed or failed");
                }
            }
            Expectation::Pane { pane, .. } | Expectation::Popup { pane } => {
                if pane.is_empty() {
                    bail!("pane expectation without a pane");
                }
            }
            Expectation::ScreenText { pane, text, regex } => {
                if pane.is_empty() {
                    bail!("screen_text expectation without a pane");
                }
                match (text, regex) {
                    (Some(_), None) => {}
                    (None, Some(r)) => {
                        regex::Regex::new(r)
                            .map_err(|e| anyhow::anyhow!("screen_text regex: {e}"))?;
                    }
                    _ => bail!("screen_text needs exactly one of `text` or `regex`"),
                }
            }
            Expectation::File { path, .. } => {
                if path.is_empty() {
                    bail!("file expectation without a path");
                }
            }
            Expectation::HookLog { text } | Expectation::Ready { text } => {
                if text.is_empty() {
                    bail!("hook_log or ready expectation without text");
                }
            }
            Expectation::Event { event_type } => {
                if event_type.is_empty() {
                    bail!("event expectation without a type");
                }
            }
            Expectation::StubRequest { stub, .. } => match stubs.get(stub.as_str()) {
                Some(StubKind::HttpSink) => {}
                Some(_) => bail!("stub_request names stub `{stub}`, which is not an http_sink"),
                None => bail!("stub_request names unknown stub `{stub}`"),
            },
            Expectation::AgentStatus { status } => {
                if !PANE_AGENT_STATUSES.contains(&status.as_str()) {
                    bail!("unknown pane agent status `{status}`");
                }
            }
        }
        Ok(())
    }
}

/// Values for scenario placeholders.
#[derive(Debug, Clone, Default)]
pub struct Vars {
    pub values: BTreeMap<String, String>,
}

/// Replace `{name}` placeholders with `vars`. An unknown placeholder is an error.
///
/// A placeholder is `{` + one or more of `[A-Za-z0-9_.-]` + `}`. Any other brace is kept as
/// it is, so JSON in setup files stays intact. A brace right after `$` is also kept, so shell
/// text such as `${HOME}` is not taken for a placeholder.
pub fn expand(s: &str, vars: &Vars) -> anyhow::Result<String> {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\{([A-Za-z0-9_.\-]+)\}").expect("placeholder regex")
    });
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for c in RE.captures_iter(s) {
        let whole = c.get(0).expect("match");
        if s[..whole.start()].ends_with('$') {
            continue;
        }
        let name = &c[1];
        let value = vars
            .values
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown placeholder `{{{name}}}` in `{s}`"))?;
        out.push_str(&s[last..whole.start()]);
        out.push_str(value);
        last = whole.end();
    }
    out.push_str(&s[last..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
plugin_id = "herdr-projects"
repository = "eliasstravik/herdr-projects"
timeout_ms = 120000

[setup]
fake_agent = true
[[setup.repo]]
name = "app"
files = { "README.md" = "hello\n" }
worktrees = ["feature"]

[[stub]]
id = "notify"
kind = "http_sink"
env = { REMOTE_URL = "{stub.notify.url}" }

[[step]]
id = "open-popup"
kind = "action"
action = "open-popup"
context = "workspace"
timeout_ms = 30000
expect_calls = [{ method = "popup.open", ok = true }, { method = "pane.get", ok = false, code = "not_found", min = 2 }]
known = { outcome = "suspected_compat_failure", reason = "popup sizing" }
[[step.expect]]
kind = "popup"
pane = "projects"
[[step.expect]]
kind = "screen_text"
pane = "projects"
text = "Projects"
[[step.compare]]
pane = "projects"
masks = [{ rect = { row = 0, col = 60, rows = 1, cols = 20 } }, { regex = "\\d+s ago" }]

[[step]]
id = "status"
kind = "event"
event = "pane.agent_status_changed"
agent_status = "blocked"
[[step.expect]]
kind = "stub_request"
stub = "notify"
contains = "blocked"

[[step]]
id = "cli-event"
kind = "event"
event = "workspace.created"
cli = ["workspace", "create"]

[[step]]
id = "link"
kind = "link"
url = "https://github.com/a/b"

[[step]]
id = "restart"
kind = "restart"

[[step]]
id = "uninstall"
kind = "lifecycle"
lifecycle = "uninstall"

[[step]]
id = "pane"
kind = "pane"
pane = "projects"

[[step]]
id = "type"
kind = "input"
pane = "projects"
keys = ["Enter"]

[[step]]
id = "ready"
kind = "wait"
[[step.expect]]
kind = "ready"
text = "listening"

[[skip]]
surface = "action:doctor"
reason = "needs a cloud account"

[normalize]
fields = ["project_id"]
"#;

    fn good() -> Scenario {
        let s: Scenario = toml::from_str(GOOD).unwrap();
        s.validate().unwrap();
        s
    }

    #[test]
    fn parses_the_documented_shape() {
        let s = good();
        assert_eq!(s.steps.len(), 9);
        let first = &s.steps[0];
        assert!(first.expect_calls[0].ok);
        assert_eq!(first.expect_calls[1].code.as_deref(), Some("not_found"));
        assert_eq!(
            first.known.as_ref().unwrap().outcome,
            Outcome::SuspectedCompatFailure
        );
        assert!(matches!(first.expect[0], Expectation::Popup { .. }));
        assert_eq!(first.compare[0].masks.len(), 2);
        assert_eq!(s.steps[5].lifecycle, Some(LifecycleOp::Uninstall));
        assert_eq!(s.normalize.fields, ["project_id"]);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let text = GOOD.replace("timeout_ms = 120000", "timeout_ms = 120000\nextra = 1");
        assert!(toml::from_str::<Scenario>(&text).is_err());
        let text = GOOD.replace("kind = \"restart\"", "kind = \"reboot\"");
        assert!(toml::from_str::<Scenario>(&text).is_err());
    }

    fn rejects(edit: impl FnOnce(&mut Scenario), what: &str) {
        let mut s = good();
        edit(&mut s);
        let e = format!("{:#}", s.validate().expect_err(what));
        assert!(e.contains(what), "`{e}` does not mention `{what}`");
    }

    fn step(s: &mut Scenario, id: &str) -> usize {
        s.steps.iter().position(|x| x.id == id).unwrap()
    }

    #[test]
    fn validation_failures() {
        rejects(|s| s.steps[1].id = "open-popup".into(), "duplicate id");
        rejects(|s| s.steps[0].action = None, "needs `action`");
        rejects(|s| s.steps[1].event = None, "needs `event`");
        rejects(
            |s| s.steps[1].cli = vec!["x".into()],
            "exactly one of `cli` or `agent_status`",
        );
        rejects(
            |s| {
                let i = step(s, "cli-event");
                s.steps[i].cli.clear();
            },
            "exactly one of `cli` or `agent_status`",
        );
        // Herdr derives `done`; `pane report-agent` does not accept it.
        rejects(
            |s| s.steps[1].agent_status = Some("done".into()),
            "unknown agent_status",
        );
        rejects(
            |s| {
                s.steps[1].expect = vec![Expectation::AgentStatus {
                    status: "finished".into(),
                }]
            },
            "unknown pane agent status",
        );
        rejects(
            |s| {
                let i = step(s, "cli-event");
                s.steps[i].expect = vec![Expectation::AgentStatus {
                    status: "done".into(),
                }];
                s.steps.retain(|x| x.agent_status.is_none());
                s.setup.fake_agent = false;
            },
            "agent_status expectation needs setup.fake_agent",
        );
        rejects(
            |s| s.steps[1].agent_status = Some("sleepy".into()),
            "unknown agent_status",
        );
        rejects(|s| s.setup.fake_agent = false, "fake_agent");
        rejects(
            |s| {
                let i = step(s, "link");
                s.steps[i].url = None;
            },
            "needs `url`",
        );
        rejects(
            |s| {
                let i = step(s, "uninstall");
                s.steps[i].lifecycle = None;
            },
            "needs `lifecycle`",
        );
        rejects(
            |s| {
                let i = step(s, "pane");
                s.steps[i].pane = None;
            },
            "needs `pane`",
        );
        rejects(
            |s| {
                let i = step(s, "type");
                s.steps[i].keys.clear();
            },
            "`text` or `keys`",
        );
        rejects(
            |s| {
                let i = step(s, "type");
                s.steps[i].pane = None;
            },
            "needs `pane`",
        );
        rejects(
            |s| s.steps[0].known.as_mut().unwrap().outcome = Outcome::Passed,
            "cannot be `passed`",
        );
        rejects(
            |s| s.steps[0].known.as_mut().unwrap().outcome = Outcome::NotApplicable,
            "or `not_applicable`",
        );
        rejects(
            |s| s.steps[0].known.as_mut().unwrap().reason = " ".into(),
            "needs a reason",
        );
        rejects(|s| s.stubs[0].kind = StubKind::Env, "not an http_sink");
        rejects(|s| s.stubs.clear(), "unknown stub `notify`");
        rejects(
            |s| {
                s.steps[0].expect[1] = Expectation::ScreenText {
                    pane: "projects".into(),
                    text: None,
                    regex: Some("(".into()),
                }
            },
            "regex",
        );
        rejects(
            |s| {
                s.steps[0].expect[1] = Expectation::ScreenText {
                    pane: "projects".into(),
                    text: Some("a".into()),
                    regex: Some("a".into()),
                }
            },
            "exactly one of `text` or `regex`",
        );
        rejects(
            |s| s.steps[0].compare[0].masks = vec![crate::mask::Mask::Regex { regex: "[".into() }],
            "mask regex",
        );
        rejects(|s| s.steps[0].context = Some("window".into()), "context");
        rejects(
            |s| s.steps[0].expect_calls[0].code = Some("x".into()),
            "`code` needs `ok = false`",
        );
        rejects(|s| s.skips[0].reason.clear(), "needs a reason");
        rejects(
            |s| {
                let dup = s.skips[0].clone();
                s.skips.push(dup);
            },
            "duplicate surface",
        );
        rejects(|s| s.repository = "nope".into(), "owner/repo");
        rejects(
            |s| s.steps[0].platforms = Some(vec!["windows".into()]),
            "unknown platform",
        );
        rejects(|s| s.steps[0].platforms = Some(vec![]), "must not be empty");
        rejects(
            |s| s.steps[0].known.as_mut().unwrap().platforms = Some(vec!["bsd".into()]),
            "unknown platform",
        );
        rejects(
            |s| s.skips[0].platforms = Some(vec!["win".into()]),
            "unknown platform",
        );
        rejects(
            |s| {
                s.steps[0].context = Some("agent_pane".into());
                s.setup.fake_agent = false;
                s.steps.retain(|x| x.agent_status.is_none());
            },
            "agent_pane",
        );
        rejects(
            |s| s.setup.path_exclude = vec!["opt/homebrew/bin".into()],
            "absolute",
        );
        rejects(
            |s| s.setup.workspace_cwd = Some(" ".into()),
            "workspace_cwd",
        );
        rejects(
            |s| {
                let dup = s.stubs[0].clone();
                s.stubs.push(dup);
            },
            "duplicate id",
        );
    }

    #[test]
    fn platform_scoped_steps_known_results_and_skips() {
        let text = GOOD
            .replace(
                "known = { outcome = \"suspected_compat_failure\", reason = \"popup sizing\" }",
                "known = { outcome = \"suspected_compat_failure\", reason = \"popup sizing\", platforms = [\"linux\"] }\nplatforms = [\"linux\", \"macos\"]",
            )
            .replace(
                "reason = \"needs a cloud account\"",
                "reason = \"needs a cloud account\"\nplatforms = [\"macos\"]",
            )
            .replace(
                "[setup]\nfake_agent = true",
                "[setup]\nfake_agent = true\nworkspace_cwd = \"{repo.app}\"\npath_exclude = [\"/opt/homebrew/bin\"]",
            );
        let s: Scenario = toml::from_str(&text).unwrap();
        s.validate().unwrap();
        let step = &s.steps[0];
        assert!(step.runs_on("linux") && step.runs_on("macos"));
        assert!(step.known_on("linux").is_some());
        assert!(step.known_on("macos").is_none());
        assert!(s.skips[0].applies_on("macos"));
        assert!(!s.skips[0].applies_on("linux"));
        assert_eq!(s.setup.workspace_cwd.as_deref(), Some("{repo.app}"));
        assert_eq!(s.setup.path_exclude, ["/opt/homebrew/bin"]);
        // Without `platforms`, everything applies everywhere.
        let g = good();
        assert!(g.steps[1].runs_on("linux") && g.skips[0].applies_on("linux"));
        assert!(g.steps[0].known_on("macos").is_some());
        let mut only_mac = g.steps[1].clone();
        only_mac.platforms = Some(vec!["macos".into()]);
        assert!(!only_mac.runs_on("linux"));
    }

    #[test]
    fn the_agent_pane_context_is_valid_with_a_fake_agent() {
        let mut s = good();
        s.steps[0].context = Some("agent_pane".into());
        s.validate().unwrap();
    }

    #[test]
    fn expand_placeholders() {
        let mut vars = Vars::default();
        vars.values.insert("home".into(), "/h".into());
        vars.values.insert("repo.app".into(), "/h/src/app".into());
        vars.values
            .insert("stub.notify.url".into(), "http://127.0.0.1:9".into());
        assert_eq!(
            expand("{home}/x {repo.app} {stub.notify.url}/hook", &vars).unwrap(),
            "/h/x /h/src/app http://127.0.0.1:9/hook"
        );
        assert_eq!(expand("plain", &vars).unwrap(), "plain");
        assert_eq!(
            expand(r#"{"a": 1} {} ${HOME}"#, &vars).unwrap(),
            r#"{"a": 1} {} ${HOME}"#
        );
        let e = expand("{state}/x", &vars).unwrap_err().to_string();
        assert!(e.contains("{state}"), "{e}");
    }

    #[test]
    fn load_reads_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a__b.toml");
        std::fs::write(&path, GOOD).unwrap();
        assert_eq!(Scenario::load(&path).unwrap().plugin_id, "herdr-projects");
        std::fs::write(
            &path,
            GOOD.replace("fake_agent = true", "fake_agent = false"),
        )
        .unwrap();
        assert!(Scenario::load(&path).is_err());
    }

    #[test]
    fn checked_in_scenarios_parse() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/compat/herdr/0.9.3/plugins/scenarios");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        for e in entries {
            let path = e.unwrap().path();
            if path.extension().is_some_and(|x| x == "toml") {
                Scenario::load(&path).unwrap();
            }
        }
    }
}
