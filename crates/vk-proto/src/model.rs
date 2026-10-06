//! Entity types (02 §1) shared by the control API (JSON) and the render stream's UI model
//! (postcard). Postcard constraints apply: no `skip_serializing_if`, no internally tagged or
//! untagged enums, no `serde_json::Value` in types that cross the render stream.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitDir {
    /// Children side by side (vertical divider).
    Horizontal,
    /// Children stacked (horizontal divider).
    Vertical,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LayoutNode {
    Leaf {
        pane: String,
    },
    Split {
        dir: SplitDir,
        children: Vec<(LayoutNode, f32)>,
    },
}

impl LayoutNode {
    pub fn panes(&self) -> Vec<String> {
        let mut v = Vec::new();
        self.collect(&mut v);
        v
    }
    fn collect(&self, v: &mut Vec<String>) {
        match self {
            LayoutNode::Leaf { pane } => v.push(pane.clone()),
            LayoutNode::Split { children, .. } => children.iter().for_each(|(c, _)| c.collect(v)),
        }
    }
    pub fn contains(&self, id: &str) -> bool {
        match self {
            LayoutNode::Leaf { pane } => pane == id,
            LayoutNode::Split { children, .. } => children.iter().any(|(c, _)| c.contains(id)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub handle: String,
    pub name: Option<String>,
    pub auto_name: String,
    pub root_path: String,
    pub task: Option<String>,
    pub order: f64,
    pub branch: Option<String>,
}

impl Workspace {
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.auto_name)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    pub id: String,
    pub handle: String,
    pub workspace: String,
    pub title: Option<String>,
    pub number: u32,
    pub layout: LayoutNode,
    pub focused_pane: Option<String>,
    pub zoomed_pane: Option<String>,
    pub order: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pane {
    pub id: String,
    pub handle: String,
    pub tab: String,
    pub workspace: String,
    pub title: Option<String>,
    pub auto_title: String,
    pub cwd: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub child_pid: Option<u32>,
    pub fg_cmdline: Vec<String>,
    pub exited: bool,
    pub exit_code: Option<i32>,
    pub unread: bool,
    pub marked_unread: bool,
    pub pinned: bool,
    pub created_by: String,
    pub recovered: Option<String>,
    /// Execution isolation of this pane's process tree (13 §12). Host by default.
    #[serde(default)]
    pub isolation: Isolation,
    /// Set for a browser pane (06 B3.2): a non-PTY pane whose content is a live Chromium
    /// viewport rendered on the viewing client's machine. Last field (postcard is positional).
    #[serde(default)]
    pub browser: Option<BrowserPane>,
}

impl Pane {
    pub fn display_title(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.auto_title)
    }
    pub fn is_browser(&self) -> bool {
        self.browser.is_some()
    }
}

/// Persisted state of a browser pane (06 B3.2 "Lifecycle"). The pane lives in the layout of
/// the machine that owns its tab; the Chromium that renders it runs on the viewing client's
/// machine, which reports navigation back so the URL and history survive restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BrowserPane {
    /// Last committed URL; the page reloads here after a server restart or reattach.
    pub url: String,
    /// Machine whose loopback `localhost` means (the route target). Empty = the machine that
    /// owns this pane.
    pub machine: String,
    /// Task whose profile to use when `preview.profile_scope = "task"`.
    pub task: Option<String>,
    /// Preview id this pane was opened for.
    pub preview: Option<String>,
    /// Pane the browser was opened next to.
    pub source_pane: Option<String>,
    /// Navigation history (most recent last, capped) and the current index into it.
    pub history: Vec<String>,
    pub history_index: u32,
    pub title: String,
    /// Watch mode (06 B7): the pane shows this agent browser session (handle such as `b3`,
    /// on this pane's machine) instead of a page of its own. Read-only until taken over.
    /// Last field (postcard is positional).
    #[serde(default)]
    pub watch: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StateSource {
    Structured,
    SelfReport,
    Screen,
    Process,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Execution {
    Starting,
    Working,
    Idle,
    Error,
    RateLimited,
    Exited,
    Unknown,
}

impl Execution {
    pub fn as_str(&self) -> &'static str {
        match self {
            Execution::Starting => "starting",
            Execution::Working => "working",
            Execution::Idle => "idle",
            Execution::Error => "error",
            Execution::RateLimited => "rate_limited",
            Execution::Exited => "exited",
            Execution::Unknown => "unknown",
        }
    }
    pub fn parse(s: &str) -> Option<Execution> {
        Some(match s {
            "starting" => Execution::Starting,
            "working" => Execution::Working,
            "idle" => Execution::Idle,
            "error" => Execution::Error,
            "rate_limited" => Execution::RateLimited,
            "exited" => Execution::Exited,
            "unknown" => Execution::Unknown,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Facet<T> {
    pub value: T,
    pub since_ms: i64,
    pub source: StateSource,
    pub confidence: f32,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdapterHealth {
    Healthy,
    Degraded,
    Disconnected,
    UnvalidatedVersion,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRun {
    pub id: String,
    pub handle: String,
    pub name: Option<String>,
    pub pane: String,
    pub harness: String,
    pub harness_version: Option<String>,
    pub integration: String,
    pub harness_session_id: Option<String>,
    pub transcript_path: Option<String>,
    pub resume_argv: Vec<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub task: Option<String>,
    pub execution: Facet<Execution>,
    pub health: AdapterHealth,
    pub yolo: bool,
    pub permission_mode: Option<String>,
    pub last_message: Option<String>,
    pub last_tool: Option<String>,
    pub turns_completed: u32,
    /// Bumped when execution goes idle after work; compared with per-client seen marks (done vs idle).
    pub done_rev: u64,
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub capabilities: Vec<String>,
    /// Token usage for the harness session (04 §10), from transcripts, extensions or hooks.
    #[serde(default)]
    pub usage: RunUsage,
    /// Last rate-limit observation (04 §10): `StopFailure{rate_limit}`, Codex `rate_limits`,
    /// pi/omp/OpenCode retry messages.
    #[serde(default)]
    pub rate_limit: Option<RateLimitInfo>,
}

/// Session token usage (04 §10). Totals for the harness session, not per turn.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Harness-reported cost (pi `cost`, OpenCode `cost`, Claude stream-json); no price table yet.
    pub cost_usd: Option<f64>,
    pub model: Option<String>,
    /// `transcript` | `extension` | `hook` | `acp`; empty when nothing was reported.
    pub source: String,
    pub updated_at_ms: i64,
}

/// A rate-limit observation (04 §10).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RateLimitInfo {
    /// The harness is currently held by the limit (false: a usage snapshot, e.g. Codex `used_percent`).
    pub limited: bool,
    pub resets_at_ms: Option<i64>,
    /// e.g. `primary` / `secondary` (Codex windows) or the harness's own label.
    pub scope: Option<String>,
    pub used_percent: Option<f32>,
    pub message: Option<String>,
    pub observed_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InteractionKind {
    Approval,
    Question,
    PlanReview,
    Notice,
}

impl InteractionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            InteractionKind::Approval => "approval",
            InteractionKind::Question => "question",
            InteractionKind::PlanReview => "plan_review",
            InteractionKind::Notice => "notice",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InteractionStatus {
    Open,
    Answered,
    ResolvedElsewhere,
    Expired,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryState {
    None,
    DecisionRecorded,
    Delivering,
    Delivered,
    DeliveryUnknown,
    Failed,
    Superseded,
    ResolvedElsewhere,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnswerChannel {
    Native,
    Keystrokes,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Risk {
    Low,
    Medium,
    High,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionInfo {
    pub tool: String,
    pub summary: String,
    pub command: Option<String>,
    pub paths: Vec<String>,
    pub diff: Option<String>,
    pub risk: Risk,
    pub risk_reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub prompt: String,
    pub header: Option<String>,
    pub multi: bool,
    pub options: Vec<QuestionOption>,
    pub allow_free_text: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    Allow,
    AllowAlways,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Answer {
    pub decision: Option<Decision>,
    /// question id → chosen option ids
    pub choices: Vec<(String, Vec<String>)>,
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Interaction {
    pub id: String,
    pub handle: String,
    pub run: String,
    pub pane: String,
    pub kind: InteractionKind,
    pub status: InteractionStatus,
    pub title: String,
    pub body_md: Option<String>,
    pub action: Option<ActionInfo>,
    pub questions: Vec<Question>,
    pub plan_md: Option<String>,
    pub answer_channel: AnswerChannel,
    pub native_ref: Option<String>,
    pub source: StateSource,
    pub confidence: f32,
    pub answerable: bool,
    pub gate: bool,
    pub decision_rev: u32,
    pub delivery: DeliveryState,
    pub delivery_error: Option<String>,
    pub answer: Option<Answer>,
    pub answered_by: Option<String>,
    /// Client idempotency key of the recorded answer (kept apart from `answered_by`, spec 16 §7.7).
    #[serde(default)]
    pub answer_key: Option<String>,
    pub opened_at_ms: i64,
    pub answered_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub id: String,
    pub kind: String,
    pub pane: Option<String>,
    pub title: String,
    pub body: String,
    pub urgency: String,
    pub created_at_ms: i64,
    pub read: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Task {
    pub id: String,
    pub handle: String,
    pub title: String,
    pub slug: String,
    pub workspace: Option<String>,
    pub repo_root: String,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
    pub base_ref: Option<String>,
    pub port_range: Option<(u16, u16)>,
    pub status: String,
    pub setup_status: Option<String>,
    pub created_at_ms: i64,
    /// `owned` (created by `task new`; 05 cleanup applies) or `attached` (tracking existing work;
    /// lifecycle actions never stop processes or delete files) — 15 §4.3.
    #[serde(default)]
    pub ownership: TaskOwnership,
    #[serde(default)]
    pub owner_machine: String,
    /// Current confirmed intent revision (15 §4.1), if tracked.
    #[serde(default)]
    pub intent_revision: Option<u32>,
    #[serde(default)]
    pub priority: Option<i32>,
    /// Object revision for expected-revision checks on mutations.
    #[serde(default)]
    pub rev: u64,
    /// Review readiness label (15 §7), independent of lifecycle `status`.
    #[serde(default)]
    pub review_label: Option<String>,
    /// Coarse user-set effort for the five-minute view (15 §8.2): quick | minutes | deep | unknown.
    #[serde(default)]
    pub effort: Option<String>,
    /// Execution isolation chosen at creation (13 §12); panes in the task inherit it.
    #[serde(default)]
    pub isolation: Isolation,
}

/// Execution isolation level (13 §2.1). Append-only: postcard encodes the variant index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IsolationLevel {
    #[default]
    Host,
    Sandbox,
    Container,
    Vm,
}

impl IsolationLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            IsolationLevel::Host => "host",
            IsolationLevel::Sandbox => "sandbox",
            IsolationLevel::Container => "container",
            IsolationLevel::Vm => "vm",
        }
    }
    pub fn parse(s: &str) -> Option<IsolationLevel> {
        Some(match s {
            "host" | "none" => IsolationLevel::Host,
            "sandbox" | "sbx" => IsolationLevel::Sandbox,
            "container" | "ctr" => IsolationLevel::Container,
            "vm" => IsolationLevel::Vm,
            _ => return None,
        })
    }
    /// Enforced containment vs cooperative guardrails (13 §2.2).
    pub fn containment(&self) -> &'static str {
        match self {
            IsolationLevel::Host => "guardrail",
            _ => "enforced",
        }
    }
}

/// Execution facts for a task, pane or run (13 §12). Every field is always serialized (postcard).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Isolation {
    #[serde(default)]
    pub level: IsolationLevel,
    /// Provider that implements the level: `seatbelt`, `bwrap`, `docker`, … (empty for host).
    #[serde(default)]
    pub provider: String,
    /// Network profile name (13 §7): `none`, `harness-apis`, `package-registries`, `dev`, `open`.
    #[serde(default)]
    pub network: String,
    /// Vibeke launched the harness with its approval-bypass flags.
    #[serde(default)]
    pub yolo: bool,
    /// `pane` (the whole process tree) or `run` (only an agent command typed into a host pane).
    #[serde(default)]
    pub scope: String,
    /// Host paths the contained processes can read: the client translates pasted paths outside
    /// them into the inbox (06 A11.4).
    #[serde(default)]
    pub visible_roots: Vec<String>,
}

impl Isolation {
    pub fn is_contained(&self) -> bool {
        self.level != IsolationLevel::Host
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskOwnership {
    #[default]
    Owned,
    Attached,
}

/// Everything a client needs to draw chrome (07 §3.1 `Layout`), plus per-client focus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SessionModel {
    pub session: String,
    pub machine: String,
    pub server_version: String,
    pub workspaces: Vec<Workspace>,
    pub tabs: Vec<Tab>,
    pub panes: Vec<Pane>,
    pub runs: Vec<AgentRun>,
    pub interactions: Vec<Interaction>,
    pub tasks: Vec<Task>,
    pub degraded: Option<String>,
    /// Previews on this machine (06 B2), suggestions included. Last field: postcard is
    /// positional, so new fields go after it.
    #[serde(default)]
    pub previews: Vec<Preview>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ClientFocus {
    pub workspace: Option<String>,
    pub tab: Option<String>,
    pub pane: Option<String>,
}

/// Preview lifecycle (06 B2): `suggested|declared → up ⇄ down → gone`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewStatus {
    /// Discovered (listener, output URL or banner), not confirmed by the user or an agent.
    Suggested,
    /// Declared and not probed yet.
    Declared,
    Up,
    Down,
    Gone,
}

impl PreviewStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            PreviewStatus::Suggested => "suggested",
            PreviewStatus::Declared => "declared",
            PreviewStatus::Up => "up",
            PreviewStatus::Down => "down",
            PreviewStatus::Gone => "gone",
        }
    }
}

/// Where a preview came from (06 B2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewSource {
    Declared,
    /// A LISTEN socket in the pane's process tree.
    Listener,
    /// A `http://localhost:<port>` URL printed in the pane.
    OutputUrl,
    /// A known dev-server banner (Vite `Local:`, Next `- Local:`).
    Banner,
}

/// A dev server reachable on a machine's loopback (02, 06 B2). Postcard-safe (crosses the
/// render stream inside [`SessionModel`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preview {
    pub id: String,
    /// `v<N>`.
    pub handle: String,
    pub machine: String,
    /// Pane id (ULID) the preview belongs to, if any.
    pub pane: Option<String>,
    pub task: Option<String>,
    pub port: u16,
    /// Path to open, starting with `/`.
    pub path: String,
    pub label: Option<String>,
    /// URL as the dev server printed it (or `http://localhost:<port><path>`).
    pub url: String,
    pub scheme: String,
    pub status: PreviewStatus,
    pub source: PreviewSource,
    pub pid: Option<u32>,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pane records stored before browser panes existed load with `browser: None`; browser panes
    /// round-trip through JSON (the store) and postcard (the render stream).
    #[test]
    fn browser_pane_persistence() {
        let old = serde_json::json!({
            "id": "P", "handle": "w1:p1", "tab": "T", "workspace": "W", "title": null,
            "auto_title": "zsh", "cwd": "/tmp", "cols": 80, "rows": 24, "child_pid": 1,
            "fg_cmdline": [], "exited": false, "exit_code": null, "unread": false,
            "marked_unread": false, "pinned": false, "created_by": "user", "recovered": null
        });
        let p: Pane = serde_json::from_value(old).unwrap();
        assert!(p.browser.is_none() && !p.is_browser());
        let mut b = p.clone();
        b.child_pid = None;
        b.browser = Some(BrowserPane {
            url: "http://localhost:5173/".into(),
            machine: String::new(),
            task: Some("K".into()),
            preview: Some("V".into()),
            source_pane: Some("P".into()),
            history: vec!["http://localhost:5173/".into()],
            history_index: 0,
            title: "app".into(),
            watch: None,
        });
        let j = serde_json::to_value(&b).unwrap();
        assert_eq!(j["browser"]["url"], "http://localhost:5173/");
        let back: Pane = serde_json::from_value(j).unwrap();
        assert_eq!(back, b);
        let m = SessionModel {
            panes: vec![p, b.clone()],
            ..Default::default()
        };
        let bytes = crate::frame::encode(&m).unwrap();
        let back: SessionModel = crate::frame::decode(&bytes[4..]).unwrap();
        assert_eq!(back.panes[1], b);
    }
}
