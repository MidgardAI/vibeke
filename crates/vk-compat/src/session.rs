//! Herdr `session.json` (v3) importer: workspaces, tabs, split layouts, cwds and agent
//! resume candidates (08 §12, 07 compat table).
//!
//! Observed schema (Herdr 3): `{version, workspaces[{id, custom_name, identity_cwd,
//! public_*_numbers, tabs[{custom_name, layout, panes{<id>: {cwd, agent_session?}}, zoomed,
//! focused, root_pane}], active_tab}], active, selected, sidebar_width, …}`. A leaf layout
//! is `{"Pane": <id>}`. Split nodes have `direction`, `ratio`, `first`, `second`; the exact
//! serialisation of splits was not present in the sample file, so several spellings are
//! accepted (`{"Split": {...}}`, `{"type": "split", ...}`).

use std::path::Path;

use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum SessionImportError {
    #[error("session.json is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("session.json has no `workspaces` array")]
    NoWorkspaces,
}

/// The agent whose conversation a pane was running.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRef {
    /// Harness id as Herdr names it (`claude`, `codex`, `pi`, …).
    pub harness: String,
    /// Conversation id, or a transcript path when `kind` is [`SessionRefKind::Path`].
    pub session_id: String,
    pub kind: SessionRefKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRefKind {
    Id,
    Path,
}

impl AgentRef {
    /// The harness resume argv, when we know one (04): `claude --resume <id>`,
    /// `codex resume <id>`, `pi --session <path>`.
    pub fn resume_argv(&self) -> Option<Vec<String>> {
        let v = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut argv = match (self.harness.as_str(), self.kind) {
            ("claude", SessionRefKind::Id) => v(&["claude", "--resume"]),
            ("codex", SessionRefKind::Id) => v(&["codex", "resume"]),
            ("pi", SessionRefKind::Path) => v(&["pi", "--session"]),
            _ => return None,
        };
        argv.push(self.session_id.clone());
        Some(argv)
    }
}

/// A terminal pane.
#[derive(Clone, Debug, PartialEq)]
pub struct Leaf {
    /// Herdr's pane id (stable within the file; useful for diagnostics).
    pub pane_id: u64,
    pub cwd: String,
    /// Explicit command to run instead of a shell. Herdr does not store one, so the importer
    /// leaves this `None`; use [`AgentRef::resume_argv`] for resume.
    pub command: Option<Vec<String>>,
    pub agent: Option<AgentRef>,
    /// The tab's focused pane.
    pub focused: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orientation {
    /// Panes next to each other (a vertical divider).
    SideBySide,
    /// Panes above each other (a horizontal divider).
    Stacked,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Layout {
    Leaf(Leaf),
    Split {
        orientation: Orientation,
        /// Share of the space given to `first`, in `0.0..=1.0`.
        ratio: f32,
        first: Box<Layout>,
        second: Box<Layout>,
    },
}

impl Layout {
    /// Leaves in layout order (first before second).
    pub fn leaves(&self) -> Vec<&Leaf> {
        match self {
            Layout::Leaf(l) => vec![l],
            Layout::Split { first, second, .. } => {
                let mut v = first.leaves();
                v.extend(second.leaves());
                v
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Tab {
    /// Custom title; `None` means auto-titled.
    pub title: Option<String>,
    /// Herdr's public tab number, when recorded.
    pub number: Option<u32>,
    pub zoomed: bool,
    pub layout: Layout,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Workspace {
    pub id: String,
    /// Custom name, else the cwd's basename.
    pub name: String,
    pub cwd: String,
    pub active_tab: usize,
    pub tabs: Vec<Tab>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResumeCandidate {
    pub workspace: String,
    pub tab: usize,
    pub pane_id: u64,
    pub cwd: String,
    pub agent: AgentRef,
    /// `None` for harnesses we have no resume argv for.
    pub argv: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionPlan {
    pub version: Option<u64>,
    pub workspaces: Vec<Workspace>,
    /// Index of the active workspace, when valid.
    pub active_workspace: Option<usize>,
    /// Non-fatal oddities (missing pane records, unknown version, …).
    pub warnings: Vec<String>,
}

impl SessionPlan {
    /// Every pane that had an `agent_session`, in file order.
    pub fn resume_candidates(&self) -> Vec<ResumeCandidate> {
        let mut out = Vec::new();
        for w in &self.workspaces {
            for (ti, t) in w.tabs.iter().enumerate() {
                for l in t.layout.leaves() {
                    if let Some(a) = &l.agent {
                        out.push(ResumeCandidate {
                            workspace: w.name.clone(),
                            tab: ti,
                            pane_id: l.pane_id,
                            cwd: l.cwd.clone(),
                            agent: a.clone(),
                            argv: a.resume_argv(),
                        });
                    }
                }
            }
        }
        out
    }
}

/// Parse Herdr's `session.json`.
pub fn import_session(session_json: &str) -> Result<SessionPlan, SessionImportError> {
    let root: Value = serde_json::from_str(session_json)?;
    let mut warnings = Vec::new();
    let version = root.get("version").and_then(Value::as_u64);
    if version != Some(3) {
        warnings.push(format!(
            "session.json version is {version:?}, expected 3; importing best-effort"
        ));
    }
    let wss = root
        .get("workspaces")
        .and_then(Value::as_array)
        .ok_or(SessionImportError::NoWorkspaces)?;

    let mut workspaces = Vec::new();
    for (wi, w) in wss.iter().enumerate() {
        let id = str_of(w, "id").unwrap_or_else(|| format!("w{wi}"));
        let cwd = str_of(w, "identity_cwd").unwrap_or_default();
        let name = str_of(w, "custom_name")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| basename(&cwd));
        let numbers: Vec<Option<u32>> = w
            .get("public_tab_numbers")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(|n| n.as_u64().map(|n| n as u32)).collect())
            .unwrap_or_default();

        let mut tabs = Vec::new();
        for (ti, t) in w
            .get("tabs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let panes = t.get("panes").and_then(Value::as_object);
            let focused = t.get("focused").and_then(Value::as_u64);
            let ctx = Ctx {
                panes,
                focused,
                ws_cwd: &cwd,
                where_: format!("workspace {id} tab {ti}"),
            };
            let Some(layout_v) = t.get("layout") else {
                warnings.push(format!("{}: no layout, skipped", ctx.where_));
                continue;
            };
            match parse_layout(layout_v, &ctx, &mut warnings) {
                Some(layout) => tabs.push(Tab {
                    title: str_of(t, "custom_name").filter(|s| !s.is_empty()),
                    number: numbers.get(ti).copied().flatten(),
                    zoomed: t.get("zoomed").and_then(Value::as_bool).unwrap_or(false),
                    layout,
                }),
                None => warnings.push(format!("{}: unreadable layout, skipped", ctx.where_)),
            }
        }

        let mut active_tab = w.get("active_tab").and_then(Value::as_u64).unwrap_or(0) as usize;
        if active_tab >= tabs.len() {
            if !tabs.is_empty() {
                warnings.push(format!(
                    "workspace {id}: active_tab {active_tab} out of range; using 0"
                ));
            }
            active_tab = 0;
        }
        workspaces.push(Workspace {
            id,
            name,
            cwd,
            active_tab,
            tabs,
        });
    }

    let active_workspace = root
        .get("active")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .filter(|n| *n < workspaces.len());

    Ok(SessionPlan {
        version,
        workspaces,
        active_workspace,
        warnings,
    })
}

struct Ctx<'a> {
    panes: Option<&'a serde_json::Map<String, Value>>,
    focused: Option<u64>,
    ws_cwd: &'a str,
    where_: String,
}

fn str_of(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn basename(p: &str) -> String {
    Path::new(p)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("workspace")
        .to_string()
}

fn parse_layout(v: &Value, ctx: &Ctx, warnings: &mut Vec<String>) -> Option<Layout> {
    let obj = v.as_object()?;

    // Tagged by `type`: {"type": "pane", "pane_id": 3} / {"type": "split", ...}
    if let Some(t) = obj.get("type").and_then(Value::as_str) {
        return match t.to_ascii_lowercase().as_str() {
            "pane" => {
                let id = obj
                    .get("pane_id")
                    .or_else(|| obj.get("id"))
                    .and_then(Value::as_u64)?;
                Some(Layout::Leaf(leaf(id, ctx, warnings)))
            }
            "split" => parse_split(obj, ctx, warnings),
            _ => None,
        };
    }

    // Externally tagged: {"Pane": 3} / {"Split": {...}}
    for (k, inner) in obj {
        match k.to_ascii_lowercase().as_str() {
            "pane" => {
                return inner
                    .as_u64()
                    .map(|id| Layout::Leaf(leaf(id, ctx, warnings)));
            }
            "split" => return parse_split(inner.as_object()?, ctx, warnings),
            _ => {}
        }
    }
    None
}

fn parse_split(
    obj: &serde_json::Map<String, Value>,
    ctx: &Ctx,
    warnings: &mut Vec<String>,
) -> Option<Layout> {
    let orientation = match obj
        .get("direction")
        .and_then(Value::as_str)?
        .to_ascii_lowercase()
        .as_str()
    {
        // `right` is the API spelling (new pane to the right); `horizontal` is the layout
        // axis in ratatui terms (children laid out left to right).
        "right" | "horizontal" | "h" | "row" => Orientation::SideBySide,
        "down" | "vertical" | "v" | "column" | "col" => Orientation::Stacked,
        other => {
            warnings.push(format!(
                "{}: unknown split direction `{other}`, assuming side by side",
                ctx.where_
            ));
            Orientation::SideBySide
        }
    };
    let ratio = match obj.get("ratio").and_then(Value::as_f64) {
        Some(r) if (0.0..=1.0).contains(&r) => r as f32,
        Some(r) => {
            warnings.push(format!("{}: split ratio {r} out of range", ctx.where_));
            r.clamp(0.05, 0.95) as f32
        }
        None => 0.5,
    };
    let first = parse_layout(obj.get("first")?, ctx, warnings)?;
    let second = parse_layout(obj.get("second")?, ctx, warnings)?;
    Some(Layout::Split {
        orientation,
        ratio,
        first: Box::new(first),
        second: Box::new(second),
    })
}

fn leaf(id: u64, ctx: &Ctx, warnings: &mut Vec<String>) -> Leaf {
    let rec = ctx.panes.and_then(|p| p.get(&id.to_string()));
    let cwd = rec.and_then(|r| str_of(r, "cwd")).unwrap_or_else(|| {
        warnings.push(format!(
            "{}: pane {id} has no record; using the workspace cwd",
            ctx.where_
        ));
        ctx.ws_cwd.to_string()
    });
    let agent = rec.and_then(|r| r.get("agent_session")).and_then(|a| {
        let harness = str_of(a, "agent")?;
        let session_id = str_of(a, "value")?;
        if harness.is_empty() || session_id.is_empty() {
            return None;
        }
        let kind = match str_of(a, "kind").as_deref() {
            Some("path") => SessionRefKind::Path,
            _ => SessionRefKind::Id,
        };
        Some(AgentRef {
            harness,
            session_id,
            kind,
        })
    });
    Leaf {
        pane_id: id,
        cwd,
        command: None,
        agent,
        focused: ctx.focused == Some(id),
    }
}
