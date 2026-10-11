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
//! known = { outcome = "suspected_compat_failure", reason = "…" }
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
//!
//! [normalize]
//! fields = ["project_id"]
//! ```
//!
//! Placeholders in strings: `{home}`, `{state}` (the plugin state dir), `{repo.<name>}`,
//! `{worktree.<repo>.<name>}`, `{stub.<id>.url}`, `{stub.<id>.file}`. [`expand`] resolves them.

use crate::outcome::Outcome;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
    /// Fake agent status for event steps: `working`, `blocked`, `done`, `idle`.
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
    Pane {
        pane: String,
        #[serde(default)]
        placement: Option<String>,
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
    /// The HTTP sink stub received a request whose body contains `contains`.
    StubRequest {
        stub: String,
        #[serde(default)]
        contains: Option<String>,
    },
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Skip {
    /// A surface name from [`crate::coverage::Surface::name`], such as `action:doctor`.
    pub surface: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NormalizeSpec {
    /// JSON object keys whose values are generated per run (18 §11).
    #[serde(default)]
    pub fields: Vec<String>,
}

impl Scenario {
    pub fn load(path: &Path) -> anyhow::Result<Scenario> {
        todo!("lane MODEL")
    }

    /// Checks that do not need the manifest: unique step ids, each step has the fields its
    /// kind needs, known outcomes are not `passed`, stub ids referenced by expectations exist,
    /// regexes compile.
    pub fn validate(&self) -> anyhow::Result<()> {
        todo!("lane MODEL")
    }
}

/// Values for scenario placeholders.
#[derive(Debug, Clone, Default)]
pub struct Vars {
    pub values: BTreeMap<String, String>,
}

/// Replace `{name}` placeholders with `vars`. An unknown placeholder is an error.
pub fn expand(s: &str, vars: &Vars) -> anyhow::Result<String> {
    todo!("lane MODEL")
}
