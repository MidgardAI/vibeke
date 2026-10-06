//! Navigation (08 §6.2 goto, §6.3 command palette, §6.7 hints and title; research R5):
//!
//! - [`fuzzy`]: an fzf-style subsequence matcher (word-boundary, consecutive and prefix
//!   bonuses, gap penalties) that also returns the matched character positions for
//!   highlighting. Whitespace-separated tokens must all match.
//! - **Command palette** (`prefix+:`): every keymap action, the browser actions, TUI commands,
//!   custom `[[keys.command]]` entries and live entries (watch an agent's browser session),
//!   each with a description and its current binding; recently used first.
//! - **Goto** (`prefix+g`): workspaces, tabs, panes/agents and tasks, ranked by fuzzy score,
//!   then this client's recent targets, then urgency. Searches titles, repo/cwd, branch, the
//!   harness's native session id, agent names and harnesses.
//! - **Recent history** per client (`<state>/<session>/nav-<client>.json`; the client name is
//!   `$VIBEKE_CLIENT_NAME`, default `default`): recent targets, recently used palette actions
//!   and the previous workspace for `last_workspace` (`prefix+shift+l`).
//! - **Hints** (`url_hints`, `prefix+shift+u`): label the URLs, `file:line` paths, git SHAs and
//!   Vibeke handles visible in the focused pane; a label opens (URLs) or copies (the rest),
//!   SHIFT+label always copies.
//! - **Title sync**: the outer terminal's title (OSC 2) follows the focused workspace/pane
//!   (`ui.title_sync`, `ui.title_format`), pushed/popped with `CSI 22/23 ; 2 t`.

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::screen::{Grid, Rect as SRect};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use unicode_width::UnicodeWidthStr;
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};
use vk_proto::model::{InteractionStatus, Workspace};
use vk_proto::render::{Row, Style, attr};

// ---- fuzzy matching ----------------------------------------------------------------------------

/// A fuzzy match: higher scores are better; `positions` are char indices into the candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub score: i32,
    pub positions: Vec<usize>,
}

const SCORE_MATCH: i32 = 16;
const BONUS_BOUNDARY: i32 = 10;
const BONUS_FIRST: i32 = 8;
const BONUS_CONSECUTIVE: i32 = 8;
const PENALTY_GAP_START: i32 = 3;
const PENALTY_GAP: i32 = 1;

fn is_sep(c: char) -> bool {
    c.is_whitespace() || matches!(c, '/' | '-' | '_' | ':' | '.' | '@' | '#' | '·' | '(' | '[')
}

fn boundary_bonus(chars: &[char], j: usize) -> i32 {
    if j == 0 {
        return BONUS_BOUNDARY + BONUS_FIRST;
    }
    let (p, c) = (chars[j - 1], chars[j]);
    if is_sep(p) || (p.is_lowercase() && c.is_uppercase()) || (!p.is_numeric() && c.is_numeric()) {
        BONUS_BOUNDARY
    } else {
        0
    }
}

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Best alignment of one token (no whitespace) in `chars`.
fn match_token(tok: &[char], chars: &[char]) -> Option<Match> {
    let (n, m) = (tok.len(), chars.len());
    if n == 0 {
        return Some(Match {
            score: 0,
            positions: vec![],
        });
    }
    if n > m {
        return None;
    }
    const NEG: i32 = i32::MIN / 4;
    // best[i][j]: best score with tok[i] matched at chars[j]; from[i][j]: where tok[i-1] was.
    let mut best = vec![vec![NEG; m]; n];
    let mut from = vec![vec![usize::MAX; m]; n];
    for i in 0..n {
        let tc = fold(tok[i]);
        // Running max over k <= j-2 of best[i-1][k] + k (linear gap penalty), with its k.
        let mut run: (i32, usize) = (NEG, usize::MAX);
        for j in 0..m {
            if i > 0 && j >= 2 && best[i - 1][j - 2] > NEG {
                let v = best[i - 1][j - 2] + (j - 2) as i32;
                if v > run.0 {
                    run = (v, j - 2);
                }
            }
            if fold(chars[j]) != tc {
                continue;
            }
            let base =
                SCORE_MATCH + boundary_bonus(chars, j) + if chars[j] == tok[i] { 1 } else { 0 };
            if i == 0 {
                // Leading gap costs a little so earlier matches win ties.
                best[0][j] = base - (j as i32).min(15) / 3;
                continue;
            }
            let mut cand = (NEG, usize::MAX);
            if j >= 1 && best[i - 1][j - 1] > NEG {
                cand = (best[i - 1][j - 1] + BONUS_CONSECUTIVE, j - 1);
            }
            if run.0 > NEG {
                // gap = j - k - 1 ≥ 1: -(start + (gap-1)·per) = -(start - per) - per·(j-k-1)
                let v = run.0 - (j as i32 - 1) * PENALTY_GAP - (PENALTY_GAP_START - PENALTY_GAP);
                if v > cand.0 {
                    cand = (v, run.1);
                }
            }
            if cand.0 > NEG {
                best[i][j] = base + cand.0;
                from[i][j] = cand.1;
            }
        }
    }
    let (mut j, score) = best[n - 1]
        .iter()
        .enumerate()
        .filter(|(_, s)| **s > NEG)
        .max_by_key(|(j, s)| (**s, std::cmp::Reverse(*j)))
        .map(|(j, s)| (j, *s))?;
    let mut positions = vec![0; n];
    for i in (0..n).rev() {
        positions[i] = j;
        if i > 0 {
            j = from[i][j];
        }
    }
    Some(Match { score, positions })
}

/// Match `query` against `cand`. Every whitespace-separated token must match; scores add up.
/// An empty query matches everything with score 0. Callers break ties (recency, length).
pub fn fuzzy(query: &str, cand: &str) -> Option<Match> {
    let chars: Vec<char> = cand.chars().collect();
    let mut score = 0;
    let mut positions = Vec::new();
    for tok in query.split_whitespace() {
        let t: Vec<char> = tok.chars().collect();
        let m = match_token(&t, &chars)?;
        score += m.score;
        positions.extend(m.positions);
    }
    positions.sort_unstable();
    positions.dedup();
    Some(Match { score, positions })
}

/// Segments of `text` with the chars at `positions` (char indices) highlighted.
pub fn highlight(text: &str, positions: &[usize], base: Style, hi: Style) -> Vec<(String, Style)> {
    let mut out: Vec<(String, Style)> = Vec::new();
    for (i, c) in text.chars().enumerate() {
        let st = if positions.binary_search(&i).is_ok() {
            hi
        } else {
            base
        };
        match out.last_mut() {
            Some((s, last)) if *last == st => s.push(c),
            _ => out.push((c.to_string(), st)),
        }
    }
    out
}

// ---- per-client history ------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetRef {
    /// Machine label.
    pub machine: String,
    /// `pane` | `task`.
    pub kind: String,
    pub id: String,
    /// Workspace of a pane target (for `last_workspace` and ranking).
    #[serde(default)]
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct History {
    /// Most recent first.
    pub targets: Vec<TargetRef>,
    /// Palette actions, most recent first.
    pub actions: Vec<String>,
    /// (machine label, workspace id) of the focused and the previous workspace.
    pub cur_ws: Option<(String, String)>,
    pub last_ws: Option<(String, String)>,
}

pub const MAX_TARGETS: usize = 50;
pub const MAX_ACTIONS: usize = 20;

impl History {
    pub fn note_target(&mut self, t: TargetRef) {
        self.targets
            .retain(|x| !(x.machine == t.machine && x.id == t.id));
        self.targets.insert(0, t);
        self.targets.truncate(MAX_TARGETS);
    }
    pub fn note_action(&mut self, a: &str) {
        self.actions.retain(|x| x != a);
        self.actions.insert(0, a.to_string());
        self.actions.truncate(MAX_ACTIONS);
    }
    /// Focus moved to workspace `ws` on `machine`; returns true when it changed.
    pub fn note_workspace(&mut self, machine: &str, ws: &str) -> bool {
        let new = (machine.to_string(), ws.to_string());
        if self.cur_ws.as_ref() == Some(&new) {
            return false;
        }
        if let Some(prev) = self.cur_ws.take() {
            self.last_ws = Some(prev);
        }
        self.cur_ws = Some(new);
        true
    }
    pub fn recency(&self, machine: &str, id: &str) -> Option<usize> {
        self.targets
            .iter()
            .position(|t| t.machine == machine && t.id == id)
    }
}

/// The per-client history key: `$VIBEKE_CLIENT_NAME`, else `default`.
pub fn client_key() -> String {
    let k = std::env::var("VIBEKE_CLIENT_NAME").unwrap_or_default();
    let k: String = k
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(64)
        .collect();
    if k.is_empty() { "default".into() } else { k }
}

pub fn history_path(dir: &std::path::Path, key: &str) -> PathBuf {
    dir.join(format!("nav-{key}.json"))
}

pub fn load_history(path: &std::path::Path) -> History {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save_history(path: &std::path::Path, h: &History) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(h).unwrap_or_default())?;
    std::fs::rename(tmp, path)
}

/// An agent browser session on a machine (from `browser.list`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSession {
    pub handle: String,
    pub url: String,
    pub owner_pane: Option<String>,
    pub human_control: bool,
}

#[derive(Debug, Clone)]
pub enum Reply {
    Sessions,
}

/// Navigation state on the App.
#[derive(Default)]
pub struct Nav {
    pub hist: History,
    pub path: Option<PathBuf>,
    pub session: String,
    /// Last focus observed (machine, workspace, pane).
    observed: Option<(usize, Option<String>, Option<String>)>,
    /// Title last written to the host terminal.
    pub title: Option<String>,
    pub sessions: HashMap<usize, Vec<AgentSession>>,
    /// Tests: URLs the hint overlay would have opened with the OS opener.
    pub opened: Vec<String>,
}

impl Nav {
    pub fn open(dir: PathBuf, key: &str, session: &str) -> Nav {
        let path = history_path(&dir, key);
        Nav {
            hist: load_history(&path),
            path: Some(path),
            session: session.to_string(),
            ..Default::default()
        }
    }

    pub fn save(&self) {
        if let Some(p) = &self.path {
            let _ = save_history(p, &self.hist);
        }
    }
}

/// Record focus changes (call once per draw): recent targets and the previous workspace.
pub fn observe(app: &mut App) {
    let cur = app.cur;
    let f = app.machines[cur].focus.clone();
    let now = (cur, f.workspace.clone(), f.pane.clone());
    if app.nav.observed.as_ref() == Some(&now) {
        return;
    }
    app.nav.observed = Some(now);
    let label = app.machines[cur].label.clone();
    let mut changed = false;
    if let Some(ws) = &f.workspace {
        changed |= app.nav.hist.note_workspace(&label, ws);
    }
    if let Some(p) = &f.pane {
        app.nav.hist.note_target(TargetRef {
            machine: label,
            kind: "pane".into(),
            id: p.clone(),
            workspace: f.workspace.clone(),
        });
        changed = true;
    }
    if changed {
        app.nav.save();
    }
}

/// Focus a workspace: its most recently visited pane (this client), else its first tab.
pub fn focus_workspace(app: &mut App, mi: usize, ws: &str) -> bool {
    let label = app.machines[mi].label.clone();
    let m = &app.machines[mi];
    let alive = |p: &str| m.model.panes.iter().any(|x| x.id == p && x.workspace == ws);
    let recent = app
        .nav
        .hist
        .targets
        .iter()
        .find(|t| t.machine == label && t.kind == "pane" && alive(&t.id))
        .map(|t| t.id.clone());
    let pane = recent.or_else(|| {
        m.model
            .tabs
            .iter()
            .filter(|t| t.workspace == ws)
            .min_by(|a, b| a.order.total_cmp(&b.order))
            .and_then(|t| {
                t.focused_pane
                    .clone()
                    .or_else(|| t.layout.panes().first().cloned())
            })
    });
    match pane {
        Some(p) => {
            app.focus_pane(mi, &p);
            true
        }
        None => false,
    }
}

/// `last_workspace`: back to the previously focused workspace (and again to toggle).
pub fn last_workspace(app: &mut App) {
    let Some((machine, ws)) = app.nav.hist.last_ws.clone() else {
        app.toast("no previous workspace yet");
        return;
    };
    let Some(mi) = app.machines.iter().position(|m| m.label == machine) else {
        app.toast(format!("{machine} is not connected"));
        return;
    };
    if !app.machines[mi].model.workspaces.iter().any(|w| w.id == ws) {
        app.toast("the previous workspace is gone");
        return;
    }
    if !focus_workspace(app, mi, &ws) {
        app.toast("the previous workspace has no panes");
    }
}

// ---- agent browser sessions (palette, peek) ----------------------------------------------------

pub fn refresh_sessions(app: &mut App, mi: usize) {
    if app.machines[mi].connected() {
        app.command_on(mi, "browser.list", json!({}), Pending::Nav(Reply::Sessions));
    }
}

pub fn on_reply(app: &mut App, mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    match r {
        Reply::Sessions => {
            let list = match res {
                Ok(v) => parse_sessions(&v),
                Err(_) => vec![],
            };
            app.nav.sessions.insert(mi, list);
            app.dirty = true;
        }
    }
}

pub fn parse_sessions(v: &Value) -> Vec<AgentSession> {
    v["sessions"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| {
                    Some(AgentSession {
                        handle: s["session"].as_str()?.to_string(),
                        url: s["url"].as_str().unwrap_or("").to_string(),
                        owner_pane: s["owner"]["pane"].as_str().map(str::to_string),
                        human_control: s["human_control"].as_bool().unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The newest agent browser session owned by `pane` on machine `mi` (as last listed).
pub fn session_of_pane<'a>(app: &'a App, mi: usize, pane: &str) -> Option<&'a AgentSession> {
    app.nav
        .sessions
        .get(&mi)?
        .iter()
        .rev()
        .find(|s| s.owner_pane.as_deref() == Some(pane))
}

/// Open a watch pane for agent session `handle` on machine `mi` (06 B7).
pub fn watch_session(app: &mut App, mi: usize, params: Value) {
    app.command_on(
        mi,
        "browser.watch",
        params,
        Pending::Toast("watching the agent's browser (read-only) — prefix+t takes over".into()),
    );
}

// ---- command palette ---------------------------------------------------------------------------

/// Descriptions for the palette (actions without one show their name).
pub const ACTION_INFO: &[(&str, &str)] = &[
    ("help", "Show key help"),
    ("settings", "Settings"),
    ("detach", "Detach this client"),
    ("reload_config", "Reload the config file"),
    (
        "open_notification_target",
        "Open the latest notification's pane / the pane's preview",
    ),
    ("workspace_picker", "Navigate the sidebar"),
    ("goto", "Goto anything (workspaces, tabs, agents, tasks)"),
    ("new_workspace", "New workspace"),
    ("new_worktree", "New git worktree"),
    ("open_worktree", "Open a git worktree"),
    ("remove_worktree", "Remove a git worktree"),
    ("rename_workspace", "Rename the workspace"),
    ("close_workspace", "Close the workspace"),
    ("previous_workspace", "Previous workspace"),
    ("next_workspace", "Next workspace"),
    ("last_workspace", "Toggle to the last workspace"),
    ("previous_agent", "Previous agent"),
    ("next_agent", "Next agent"),
    ("focus_agent", "Focus agent by number"),
    ("switch_workspace", "Switch workspace by number"),
    ("new_tab", "New tab"),
    ("rename_tab", "Rename the tab"),
    ("previous_tab", "Previous tab"),
    ("next_tab", "Next tab"),
    ("switch_tab", "Switch to tab 1"),
    ("close_tab", "Close the tab"),
    ("rename_pane", "Rename the pane"),
    ("remote_image_paste", "Paste an image into a remote pane"),
    ("split_vertical", "Split side by side"),
    ("split_horizontal", "Split stacked"),
    ("close_pane", "Close the pane"),
    ("zoom", "Zoom the pane"),
    ("resize_mode", "Resize mode"),
    ("toggle_sidebar", "Toggle the sidebar"),
    ("focus_pane_left", "Focus the pane to the left"),
    ("focus_pane_down", "Focus the pane below"),
    ("focus_pane_up", "Focus the pane above"),
    ("focus_pane_right", "Focus the pane to the right"),
    ("cycle_pane_next", "Next pane"),
    ("cycle_pane_previous", "Previous pane"),
    ("last_pane", "Last pane"),
    ("edit_scrollback", "Edit the scrollback in $EDITOR"),
    ("enter_copy_mode", "Copy mode"),
    ("paste_buffer", "Paste the copy buffer"),
    ("command_palette", "Command palette"),
    ("search_scrollback", "Search the scrollback"),
    ("next_attention", "Next agent that needs you"),
    (
        "next_attention_focus",
        "Focus the next agent that needs you",
    ),
    ("inbox", "Attention inbox"),
    ("mark_unread", "Mark the pane unread"),
    ("pin_pane", "Pin the pane"),
    ("float_new", "New floating pane"),
    ("toggle_floats", "Show/hide floating panes"),
    ("sync_input", "Synchronized input"),
    ("new_task", "New task (git worktree)"),
    ("preview_list", "Previews"),
    ("cancel_transfer", "Cancel file transfers"),
    ("review_clipboard", "Review clipboard requests"),
    ("url_hints", "Label URLs and IDs in the pane (open / copy)"),
    ("track_work", "Track this work (task)"),
    ("task_details", "Task details for this agent"),
    (
        "pending_operations",
        "Pending operations with unknown outcomes",
    ),
    ("open_preview", "Open the pane's preview in a browser pane"),
    ("browser_address", "Browser: edit the URL"),
    ("browser_back", "Browser: back"),
    ("browser_forward", "Browser: forward"),
    ("browser_reload", "Browser: reload"),
    ("browser_hard_reload", "Browser: hard reload"),
    ("browser_stop", "Browser: stop loading"),
    ("browser_screenshot", "Browser: screenshot"),
    (
        "browser_window",
        "Browser: open in a window ⇄ back to the pane",
    ),
    ("browser_console", "Browser: console/network split"),
    (
        "browser_take_over",
        "Browser: take over ⇄ release the watched agent session",
    ),
    ("browser_watch", "Watch the focused agent's browser session"),
    ("float_pane", "Float this pane ⇄ back into the tiling"),
    ("embed_pane", "Embed this floating pane into the tiling"),
    (
        "group_new",
        "New workspace group (moves this workspace into it)",
    ),
    ("group_move", "Move this workspace into a group…"),
    ("group_rename", "Rename this workspace's group"),
    ("group_collapse", "Collapse/expand this workspace's group"),
    ("search_global", "Search all panes (scrollback + archive)"),
    ("layout_save", "Save this tab's layout…"),
    ("layout_apply", "Apply a saved layout…"),
    ("status_bar_toggle", "Show/hide the status bar"),
    (
        "theme_detect",
        "Re-detect the terminal's light/dark appearance",
    ),
    ("screenshots", "Screenshot gallery (diff, open, delete)"),
    (
        "screenshot_pane",
        "Screenshot pane: the latest screenshot, following new ones",
    ),
    (
        "desk",
        "Session desk: find and reopen previous conversations",
    ),
    ("drafts", "Drafts composer for this workspace"),
    ("notes", "Workspace notes (never sent unless included)"),
    ("assist_briefing", "Briefing for this workspace (assistant)"),
    (
        "assist_pane_title",
        "Suggest a title for this pane (assistant)",
    ),
];

/// Actions only reachable from the palette (no keymap entry).
const EXTRA_ACTIONS: &[&str] = &[
    "track_work",
    "task_details",
    "pending_operations",
    "open_preview",
    "browser_stop",
    "browser_watch",
    "float_pane",
    "embed_pane",
    "group_new",
    "group_move",
    "group_rename",
    "group_collapse",
    "search_global",
    "layout_save",
    "layout_apply",
    "status_bar_toggle",
    "theme_detect",
    "screenshots",
    "screenshot_pane",
    "desk",
    "drafts",
    "notes",
    "assist_briefing",
    "assist_pane_title",
];

pub fn describe(action: &str) -> String {
    ACTION_INFO
        .iter()
        .find(|(a, _)| *a == action)
        .map(|(_, d)| d.to_string())
        .unwrap_or_else(|| action.replace('_', " "))
}

#[derive(Debug, Clone, PartialEq)]
pub struct PaletteEntry {
    /// Action name (`split_vertical`, `command:0`, `watch:<machine>:<session>`,
    /// `plugin:<machine>:<plugin>.<action>`).
    pub id: String,
    pub desc: String,
    pub binding: Option<String>,
    /// Listed but not runnable (an untrusted or disabled plugin's action); `desc` says why.
    pub disabled: bool,
}

impl PaletteEntry {
    /// The searchable/displayed text: description, then the action name.
    pub fn text(&self) -> String {
        format!("{}  {}", self.desc, self.id)
    }
}

/// Every palette entry, unranked.
pub fn palette_entries(app: &App) -> Vec<PaletteEntry> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |out: &mut Vec<PaletteEntry>, id: String, desc: String, b: Option<String>| {
        if seen.insert(id.clone()) {
            out.push(PaletteEntry {
                id,
                desc,
                binding: b,
                disabled: false,
            });
        }
    };
    for (action, _) in vk_config::DEFAULT_KEYMAP {
        if *action == "command_palette" {
            continue;
        }
        let b = app.keymap.binding_for(action);
        push(&mut out, action.to_string(), describe(action), b);
    }
    for (action, default) in crate::browser::DEFAULT_BROWSER_KEYS {
        let b = app
            .config
            .keys
            .bindings
            .get(*action)
            .cloned()
            .unwrap_or_else(|| default.to_string());
        push(
            &mut out,
            action.to_string(),
            describe(action),
            Some(format!("{b} (browser pane)")),
        );
    }
    for a in EXTRA_ACTIONS {
        push(
            &mut out,
            a.to_string(),
            describe(a),
            app.keymap.binding_for(a),
        );
    }
    for (i, c) in app.config.keys.command.iter().enumerate() {
        let desc = c
            .title
            .clone()
            .map(|t| format!("Command: {t}"))
            .unwrap_or_else(|| format!("Command: {}", c.command));
        if c.kind == vk_config::CommandType::PluginAction {
            // Listed with the plugin's own entry below (binding shown there).
            continue;
        }
        let b = (!c.key.is_empty()).then(|| c.key.clone());
        push(&mut out, format!("command:{i}"), desc, b);
    }
    // Herdr plugin actions per machine (M5); untrusted/disabled ones listed but disabled.
    for (id, desc, disabled) in crate::plugins::palette_entries(app) {
        let q = id.splitn(3, ':').nth(2).unwrap_or_default().to_string();
        let b = crate::plugins::binding_for(app, &q);
        let n = out.len();
        push(&mut out, id, desc, b);
        if let Some(e) = out.get_mut(n) {
            e.disabled = disabled;
        }
    }
    let multi = app.machines.len() > 1;
    let mut mis: Vec<&usize> = app.nav.sessions.keys().collect();
    mis.sort();
    for mi in mis {
        for s in &app.nav.sessions[mi] {
            let on = if multi {
                format!(" ({})", app.machines[*mi].label)
            } else {
                String::new()
            };
            let ctl = if s.human_control {
                " · taken over"
            } else {
                ""
            };
            push(
                &mut out,
                format!("watch:{mi}:{}", s.handle),
                format!(
                    "Watch agent browser {} — {}{on}{ctl}",
                    s.handle,
                    s.url
                        .trim_start_matches("http://")
                        .trim_start_matches("https://")
                ),
                None,
            );
        }
    }
    out
}

/// Ranked palette entries for `filter` with match positions into [`PaletteEntry::text`].
pub fn palette_ranked(app: &App, filter: &str) -> Vec<(PaletteEntry, Vec<usize>)> {
    let recent = &app.nav.hist.actions;
    let rank = |id: &str| recent.iter().position(|a| a == id);
    let mut v: Vec<(i32, usize, PaletteEntry, Vec<usize>)> = palette_entries(app)
        .into_iter()
        .enumerate()
        .filter_map(|(i, e)| {
            let m = fuzzy(filter, &e.text())?;
            let bonus = rank(&e.id).map_or(0, |r| 24 - r as i32);
            Some((m.score + bonus, i, e, m.positions))
        })
        .collect();
    if filter.trim().is_empty() {
        // Recently used first (in recency order), then the catalogue order.
        v.sort_by_key(|(_, i, e, _)| (rank(&e.id).unwrap_or(usize::MAX), *i));
    } else {
        v.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    }
    v.into_iter().map(|(_, _, e, p)| (e, p)).collect()
}

pub fn open_palette(app: &mut App, filter: String) {
    for mi in 0..app.machines.len() {
        refresh_sessions(app, mi);
        crate::plugins::refresh(app, mi);
    }
    app.mode = Mode::Popup(Popup::Palette { filter, sel: 0 });
}

/// Run a palette entry and remember it.
pub fn run_palette(app: &mut App, id: &str) {
    app.nav.hist.note_action(id);
    app.nav.save();
    if let Some(rest) = id.strip_prefix("watch:") {
        let mut it = rest.splitn(2, ':');
        let mi: usize = it.next().and_then(|x| x.parse().ok()).unwrap_or(usize::MAX);
        if let (Some(h), true) = (it.next(), mi < app.machines.len()) {
            watch_session(app, mi, json!({"session": h}));
        }
        return;
    }
    app.action(id, None);
}

/// List-popup key handling shared by palette and goto. Returns the new (filter, sel) or the
/// outcome.
pub enum ListKey {
    Stay(String, usize),
    Enter(usize),
    Close,
}

pub fn list_key(ev: &KeyEvent, mut filter: String, sel: usize, n: usize) -> ListKey {
    if ev.kind == KeyKind::Release {
        return ListKey::Stay(filter, sel);
    }
    match ev.key {
        Key::Named(NamedKey::Escape) => ListKey::Close,
        Key::Named(NamedKey::Enter) => ListKey::Enter(sel),
        Key::Named(NamedKey::Down) | Key::Named(NamedKey::Tab) => {
            ListKey::Stay(filter, (sel + 1).min(n.saturating_sub(1)))
        }
        Key::Char('n' | 'j') if ev.mods.ctrl() => {
            ListKey::Stay(filter, (sel + 1).min(n.saturating_sub(1)))
        }
        Key::Named(NamedKey::Up) => ListKey::Stay(filter, sel.saturating_sub(1)),
        Key::Char('p' | 'k') if ev.mods.ctrl() => ListKey::Stay(filter, sel.saturating_sub(1)),
        Key::Named(NamedKey::Backspace) => {
            filter.pop();
            ListKey::Stay(filter, 0)
        }
        Key::Char('u') if ev.mods.ctrl() => ListKey::Stay(String::new(), 0),
        Key::Char(c) if !ev.mods.ctrl() && !ev.mods.alt() => {
            filter.push(c);
            ListKey::Stay(filter, 0)
        }
        _ => ListKey::Stay(filter, sel),
    }
}

pub fn palette_key(app: &mut App, ev: KeyEvent, filter: String, sel: usize) {
    let ranked = palette_ranked(app, &filter);
    match list_key(&ev, filter, sel, ranked.len()) {
        ListKey::Close => {}
        ListKey::Enter(i) => {
            if let Some((e, _)) = ranked.get(i) {
                let id = e.id.clone();
                run_palette(app, &id);
            }
        }
        ListKey::Stay(filter, sel) => app.mode = Mode::Popup(Popup::Palette { filter, sel }),
    }
}

/// Draw a list popup row: highlighted label, then a dim right-aligned note.
fn list_row(
    g: &mut Grid,
    at: SRect,
    segs: Vec<(String, Style)>,
    note: &str,
    selected: bool,
    app: &App,
) {
    let (x, y, w) = (at.x, at.y, at.w);
    let t = app.theme;
    let row_st = if selected { t.sel(t.fg) } else { t.text() };
    g.fill(SRect { x, y, w, h: 1 }, row_st);
    let note_w = UnicodeWidthStr::width(note) as u16;
    let avail = w.saturating_sub(note_w + 2);
    let mut cx = x;
    for (s, st) in segs {
        let st = if selected {
            Style {
                bg: t.selection,
                ..st
            }
        } else {
            st
        };
        let used = g.put_str(cx, y, &s, st, (x + avail).saturating_sub(cx));
        cx += used;
        if cx >= x + avail {
            break;
        }
    }
    if !note.is_empty() && note_w + 2 < w {
        let st = if selected {
            Style {
                bg: t.selection,
                ..t.dim()
            }
        } else {
            t.dim()
        };
        g.put_str(x + w - note_w - 1, y, note, st, note_w);
    }
}

fn list_frame(app: &App, g: &mut Grid, title: &str, filter: &str) -> (u16, u16, u16, u16) {
    let area = app.pane_area();
    let w = 90u16.min(area.w.saturating_sub(2)).max(20);
    let h = 22u16.min(area.h.saturating_sub(1)).max(6);
    let mut b = crate::popups::frame(app, g, w, h, title);
    b.line(&format!("> {filter}"), app.theme.bold(app.theme.fg));
    let x = area.x + (area.w.saturating_sub(w)) / 2 + 1;
    let y = area.y + (area.h.saturating_sub(h)) / 3 + 2;
    (x, y, w.saturating_sub(2), h.saturating_sub(3))
}

pub fn draw_palette(app: &App, g: &mut Grid, filter: &str, sel: usize) -> (u16, u16) {
    let (x, y, w, rows) = list_frame(
        app,
        g,
        "command palette · type to search · enter runs · esc",
        filter,
    );
    let t = app.theme;
    let ranked = palette_ranked(app, filter);
    let hi = Style {
        attrs: attr::BOLD | attr::UNDERLINE,
        ..t.s(t.accent)
    };
    let skip = sel.saturating_sub(rows.saturating_sub(1) as usize);
    for (row, (i, (e, pos))) in ranked
        .iter()
        .enumerate()
        .skip(skip)
        .take(rows as usize)
        .enumerate()
    {
        let desc_len = e.desc.chars().count();
        let base = if e.disabled { t.dim() } else { t.text() };
        let mut segs = highlight(&e.desc, pos, base, hi);
        let name_pos: Vec<usize> = pos
            .iter()
            .filter(|p| **p >= desc_len + 2)
            .map(|p| p - desc_len - 2)
            .collect();
        segs.push(("  ".into(), t.dim()));
        segs.extend(highlight(&e.id, &name_pos, t.dim(), hi));
        let note = e.binding.clone().unwrap_or_default();
        let at = SRect {
            x,
            y: y + row as u16,
            w,
            h: 1,
        };
        list_row(g, at, segs, &note, i == sel, app);
    }
    if ranked.is_empty() {
        g.put_str(x + 1, y, "no matching command", t.dim(), w);
    }
    (x + 2 + UnicodeWidthStr::width(filter) as u16, y - 1)
}

// ---- goto --------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GotoTarget {
    Workspace(String),
    Tab(String),
    Pane(String),
    Task(String),
}

#[derive(Debug, Clone)]
pub struct GotoEntry {
    pub mi: usize,
    pub target: GotoTarget,
    /// Shown (and matched; highlight positions index into it).
    pub label: String,
    /// Matched but not shown: repo/cwd, branch, native session id, harness.
    pub extra: String,
    /// 0 = nothing; higher = more urgent (open interaction, done).
    pub urgency: u8,
    /// `@` agent, `#` task, `:` tab, `~` workspace.
    pub kind: char,
    pub running: Option<String>,
}

fn ws_extra(w: &Workspace) -> String {
    format!(
        "{} {} {}",
        w.root_path,
        w.branch.clone().unwrap_or_default(),
        w.handle
    )
}

/// Every goto candidate across machines.
pub fn goto_entries(app: &App) -> Vec<GotoEntry> {
    let mut out = Vec::new();
    for (mi, m) in app.machines.iter().enumerate() {
        let mp = if app.machines.len() > 1 {
            format!("{}/", m.label)
        } else {
            String::new()
        };
        for w in &m.model.workspaces {
            let branch = w
                .branch
                .as_ref()
                .map(|b| format!(" ⎇ {b}"))
                .unwrap_or_default();
            out.push(GotoEntry {
                mi,
                target: GotoTarget::Workspace(w.id.clone()),
                label: format!("{mp}{}{branch}", w.display_name()),
                extra: ws_extra(w),
                urgency: 0,
                kind: '~',
                running: None,
            });
            for t in m.model.tabs.iter().filter(|t| t.workspace == w.id) {
                let panes = t.layout.panes();
                let title = t.title.clone().unwrap_or_else(|| {
                    t.focused_pane
                        .as_ref()
                        .or(panes.first())
                        .and_then(|p| m.model.panes.iter().find(|x| &x.id == p))
                        .map(|p| p.display_title().to_string())
                        .unwrap_or_default()
                });
                out.push(GotoEntry {
                    mi,
                    target: GotoTarget::Tab(t.id.clone()),
                    label: format!("{mp}{} :{} {title}", w.display_name(), t.number),
                    extra: format!("{} {}", t.handle, ws_extra(w)),
                    urgency: 0,
                    kind: ':',
                    running: None,
                });
                for pid in panes {
                    let Some(p) = m.model.panes.iter().find(|x| x.id == pid) else {
                        continue;
                    };
                    let run = m.model.runs.iter().find(|r| r.pane == pid);
                    let open = m
                        .model
                        .interactions
                        .iter()
                        .any(|i| i.pane == pid && i.status == InteractionStatus::Open);
                    let (agent, urgency, running, extra) = match run {
                        Some(r) => (
                            format!(
                                " @{} {}",
                                r.name.clone().unwrap_or_else(|| r.harness.clone()),
                                r.execution.value.as_str()
                            ),
                            crate::draw::urgency(app, m, r),
                            Some(r.execution.value.as_str().to_string()),
                            format!(
                                "{} {} {} {} {}",
                                r.harness,
                                r.harness_session_id.clone().unwrap_or_default(),
                                r.cwd.clone().unwrap_or_default(),
                                r.model.clone().unwrap_or_default(),
                                r.handle,
                            ),
                        ),
                        None => (String::new(), u8::from(open) * 8, None, String::new()),
                    };
                    out.push(GotoEntry {
                        mi,
                        target: GotoTarget::Pane(pid.clone()),
                        label: format!(
                            "{mp}{} :{} {}{agent}",
                            w.display_name(),
                            t.number,
                            p.display_title()
                        ),
                        extra: format!(
                            "{extra} {} {} {}",
                            p.cwd.clone().unwrap_or_default(),
                            p.handle,
                            ws_extra(w)
                        ),
                        urgency: urgency.max(u8::from(open) * 8),
                        kind: if run.is_some() { '@' } else { ' ' },
                        running,
                    });
                }
            }
        }
    }
    for (mi, m) in app.machines.iter().enumerate() {
        let mp = if app.machines.len() > 1 {
            format!("{}/", m.label)
        } else {
            String::new()
        };
        for t in m.model.tasks.iter().filter(|t| t.status != "archived") {
            out.push(GotoEntry {
                mi,
                target: GotoTarget::Task(t.id.clone()),
                label: format!(
                    "{mp}#{} {} · {}",
                    t.handle,
                    t.title,
                    t.review_label
                        .as_deref()
                        .map(crate::tasks::label_text)
                        .unwrap_or("task")
                ),
                extra: format!(
                    "{} {} {}",
                    t.repo_root,
                    t.branch.clone().unwrap_or_default(),
                    t.worktree_path.clone().unwrap_or_default()
                ),
                urgency: 0,
                kind: '#',
                running: None,
            });
        }
    }
    out
}

fn target_id(t: &GotoTarget) -> &str {
    match t {
        GotoTarget::Workspace(x)
        | GotoTarget::Tab(x)
        | GotoTarget::Pane(x)
        | GotoTarget::Task(x) => x,
    }
}

/// Ranked goto entries for `filter` (08 §6.2): kind prefixes `@agent`, `#task`, `:tab`,
/// `~workspace`, `!state`; then fuzzy score, this client's recency, urgency.
pub fn goto_ranked(app: &App, filter: &str) -> Vec<(GotoEntry, Vec<usize>)> {
    let mut kinds: Vec<char> = Vec::new();
    let mut states: Vec<String> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    for tok in filter.split_whitespace() {
        if let Some(st) = tok.strip_prefix('!') {
            states.push(st.to_lowercase());
            continue;
        }
        let mut rest = tok;
        if let Some(c) = tok.chars().next()
            && matches!(c, '@' | '#' | ':' | '~')
        {
            kinds.push(c);
            rest = &tok[c.len_utf8()..];
        }
        if !rest.is_empty() {
            words.push(rest.to_string());
        }
    }
    let query = words.join(" ");
    let mut v: Vec<(i32, usize, usize, u8, GotoEntry, Vec<usize>)> = Vec::new();
    for (i, e) in goto_entries(app).into_iter().enumerate() {
        if !kinds.is_empty() && !kinds.contains(&e.kind) {
            continue;
        }
        // Tasks only show by default when tracked or asked for.
        if kinds.is_empty() && e.kind == '#' && query.is_empty() {
            let tracked = matches!(&e.target, GotoTarget::Task(id)
                if app.machines[e.mi].model.tasks.iter().any(|t| &t.id == id && t.intent_revision.is_some()));
            if !tracked {
                continue;
            }
        }
        if !states.is_empty() {
            let ok = states.iter().all(|st| {
                e.running
                    .as_deref()
                    .is_some_and(|r| r.starts_with(st.as_str()))
                    || (st.starts_with("appr") && e.urgency >= 8 && e.kind != ':')
            });
            if !ok {
                continue;
            }
        }
        let full = format!("{} {}", e.label, e.extra);
        let Some(m) = fuzzy(&query, &full) else {
            continue;
        };
        let label_len = e.label.chars().count();
        let pos: Vec<usize> = m.positions.into_iter().filter(|p| *p < label_len).collect();
        let label = &app.machines[e.mi].label;
        let rec = app
            .nav
            .hist
            .recency(label, target_id(&e.target))
            .unwrap_or(usize::MAX);
        v.push((m.score, rec, i, e.urgency, e, pos));
    }
    if query.is_empty() {
        // Recent first, then urgent, then the natural order.
        v.sort_by(|a, b| a.1.cmp(&b.1).then(b.3.cmp(&a.3)).then(a.2.cmp(&b.2)));
    } else {
        v.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then(a.1.cmp(&b.1))
                .then(b.3.cmp(&a.3))
                .then(a.2.cmp(&b.2))
        });
    }
    v.into_iter().map(|x| (x.4, x.5)).collect()
}

pub fn goto_key(app: &mut App, ev: KeyEvent, filter: String, sel: usize) {
    // `>` switches to the command palette (08 §6.2).
    if filter.is_empty() && ev.key == Key::Char('>') {
        open_palette(app, String::new());
        return;
    }
    let ranked = goto_ranked(app, &filter);
    match list_key(&ev, filter, sel, ranked.len()) {
        ListKey::Close => {}
        ListKey::Enter(i) => {
            if let Some((e, _)) = ranked.get(i).cloned() {
                jump(app, &e);
            }
        }
        ListKey::Stay(filter, sel) => app.mode = Mode::Popup(Popup::Goto { filter, sel }),
    }
}

pub fn jump(app: &mut App, e: &GotoEntry) {
    let mi = e.mi;
    match &e.target {
        GotoTarget::Pane(p) => app.focus_pane(mi, p),
        GotoTarget::Workspace(w) => {
            if !focus_workspace(app, mi, w) {
                app.toast("that workspace has no panes");
            }
        }
        GotoTarget::Tab(t) => {
            let pane = app.machines[mi]
                .model
                .tabs
                .iter()
                .find(|x| &x.id == t)
                .and_then(|t| {
                    t.focused_pane
                        .clone()
                        .or_else(|| t.layout.panes().first().cloned())
                });
            if let Some(p) = pane {
                app.focus_pane(mi, &p);
            }
        }
        GotoTarget::Task(t) => {
            let label = app.machines[mi].label.clone();
            app.nav.hist.note_target(TargetRef {
                machine: label,
                kind: "task".into(),
                id: t.clone(),
                workspace: None,
            });
            app.nav.save();
            crate::tasks::open_task(app, mi, t);
        }
    }
}

pub fn draw_goto(app: &App, g: &mut Grid, filter: &str, sel: usize) -> (u16, u16) {
    let (x, y, w, rows) = list_frame(
        app,
        g,
        "goto · @agent #task :tab ~workspace !state · > commands",
        filter,
    );
    let t = app.theme;
    let ranked = goto_ranked(app, filter);
    let hi = Style {
        attrs: attr::BOLD | attr::UNDERLINE,
        ..t.s(t.accent)
    };
    let skip = sel.saturating_sub(rows.saturating_sub(1) as usize);
    for (row, (i, (e, pos))) in ranked
        .iter()
        .enumerate()
        .skip(skip)
        .take(rows as usize)
        .enumerate()
    {
        let glyph = match (e.kind, e.urgency) {
            (_, 8) => "⚑ ",
            (_, 7) => "? ",
            ('@', _) => "@ ",
            ('#', _) => "# ",
            (':', _) => ": ",
            ('~', _) => "~ ",
            _ => "  ",
        };
        let mut segs = vec![(glyph.to_string(), t.dim())];
        segs.extend(highlight(&e.label, pos, t.text(), hi));
        let note = match &e.target {
            GotoTarget::Workspace(_) => "workspace",
            GotoTarget::Tab(_) => "tab",
            GotoTarget::Pane(_) if e.kind == '@' => "agent",
            GotoTarget::Pane(_) => "pane",
            GotoTarget::Task(_) => "task",
        };
        let at = SRect {
            x,
            y: y + row as u16,
            w,
            h: 1,
        };
        list_row(g, at, segs, note, i == sel, app);
    }
    if ranked.is_empty() {
        g.put_str(x + 1, y, "nothing matches", t.dim(), w);
    }
    (x + 2 + UnicodeWidthStr::width(filter) as u16, y - 1)
}

// ---- hints -------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintKind {
    Url,
    Path,
    Sha,
    Id,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub label: String,
    pub text: String,
    pub kind: HintKind,
    /// Pane-local cell position of the first character.
    pub row: u16,
    pub col: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hints {
    pub machine: usize,
    pub pane: String,
    pub items: Vec<Hint>,
    pub typed: String,
}

/// Label alphabet: home row first.
const LABEL_CHARS: &str = "asdfjklghqwertyuiopzxcvbnm";

/// `n` distinct labels: single letters while they suffice, else two letters (prefix-free).
pub fn labels(n: usize) -> Vec<String> {
    let cs: Vec<char> = LABEL_CHARS.chars().collect();
    if n <= cs.len() {
        return cs.iter().take(n).map(|c| c.to_string()).collect();
    }
    let mut out = Vec::new();
    'outer: for a in &cs {
        for b in &cs {
            if out.len() == n {
                break 'outer;
            }
            out.push(format!("{a}{b}"));
        }
    }
    out
}

fn classify(tok: &str) -> Option<HintKind> {
    if tok.starts_with("http://") || tok.starts_with("https://") {
        return (tok.len() > 8).then_some(HintKind::Url);
    }
    // path:line[:col]
    let mut parts = tok.rsplitn(3, ':');
    let last = parts.next().unwrap_or("");
    if last.chars().all(|c| c.is_ascii_digit()) && !last.is_empty() {
        let rest: Vec<&str> = parts.collect();
        let path = match rest.as_slice() {
            [p] => *p,
            [l2, p] if l2.chars().all(|c| c.is_ascii_digit()) && !l2.is_empty() => *p,
            _ => "",
        };
        if !path.is_empty()
            && (path.contains('/') || path.contains('.'))
            && path
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '~' | '+'))
        {
            return Some(HintKind::Path);
        }
    }
    // Vibeke handles: w2:p1, w2:t1, b3, v4, k12, #k12, r5, i7.
    let h = tok.trim_start_matches('#');
    let handle = {
        let ws_pane = h.split_once(':').is_some_and(|(a, b)| {
            a.starts_with('w')
                && a.len() > 1
                && a[1..].chars().all(|c| c.is_ascii_digit())
                && matches!(b.chars().next(), Some('p' | 't'))
                && b.len() > 1
                && b[1..].chars().all(|c| c.is_ascii_digit())
        });
        let single = h.len() >= 2
            && h.len() <= 6
            && matches!(h.chars().next(), Some('b' | 'v' | 'k' | 'r' | 'i'))
            && h[1..].chars().all(|c| c.is_ascii_digit());
        ws_pane || single
    };
    if handle {
        return Some(HintKind::Id);
    }
    // ULIDs (26 Crockford base32, upper case).
    if tok.len() == 26
        && tok
            .chars()
            .all(|c| c.is_ascii_digit() || (c.is_ascii_uppercase() && !"ILOU".contains(c)))
    {
        return Some(HintKind::Id);
    }
    // Git SHAs: 7–40 hex with a digit and a letter.
    if (7..=40).contains(&tok.len())
        && tok
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        && tok.chars().any(|c| c.is_ascii_digit())
        && tok.chars().any(|c| c.is_ascii_alphabetic())
    {
        return Some(HintKind::Sha);
    }
    None
}

/// Find hint targets in visible rows: (row, col, text, kind), top to bottom.
pub fn scan(lines: &[Row]) -> Vec<(u16, u16, String, HintKind)> {
    let mut out = Vec::new();
    for (y, row) in lines.iter().enumerate() {
        let mut cells: Vec<(u16, char)> = Vec::new();
        let mut c = 0u16;
        for span in &row.spans {
            for ch in span.text.chars() {
                cells.push((c, ch));
                c += UnicodeWidthStr::width(ch.to_string().as_str()).max(1) as u16;
            }
        }
        let n = cells.len();
        let mut i = 0;
        while i < n {
            let stop = |ch: char| {
                ch.is_whitespace()
                    || matches!(
                        ch,
                        '"' | '\''
                            | '<'
                            | '>'
                            | '`'
                            | '('
                            | ')'
                            | '['
                            | ']'
                            | '{'
                            | '}'
                            | '|'
                            | ','
                    )
            };
            if stop(cells[i].1) {
                i += 1;
                continue;
            }
            let start = i;
            while i < n && !stop(cells[i].1) {
                i += 1;
            }
            let mut tok: String = cells[start..i].iter().map(|x| x.1).collect();
            // A URL may be glued to a prefix (`url=http://…`).
            let mut off = 0;
            if let Some(p) = tok.find("http://").or_else(|| tok.find("https://"))
                && p > 0
            {
                off = tok[..p].chars().count();
                tok = tok[p..].to_string();
            }
            let trimmed = tok
                .trim_end_matches(['.', ',', ';', ':', '!', '?'])
                .to_string();
            if let Some(kind) = classify(&trimmed) {
                out.push((y as u16, cells[start + off].0, trimmed, kind));
            }
        }
    }
    out
}

pub fn open_hints(app: &mut App) {
    let Some(pane) = app.focused_pane() else {
        return;
    };
    let mi = app.cur;
    let Some(buf) = app.machines[mi].panes.get(&pane) else {
        app.toast("nothing to label here");
        return;
    };
    let found = scan(&buf.lines);
    crate::plugins::refresh(app, mi);
    if found.is_empty() {
        app.toast("no URLs or IDs visible in this pane");
        return;
    }
    let ls = labels(found.len());
    let items = found
        .into_iter()
        .zip(ls)
        .map(|((row, col, text, kind), label)| Hint {
            label,
            text,
            kind,
            row,
            col,
        })
        .collect();
    app.mode = Mode::Popup(Popup::Hints(Hints {
        machine: mi,
        pane,
        items,
        typed: String::new(),
    }));
}

/// What a typed label does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HintAction {
    Open(String),
    Copy(String),
}

/// One key in the hint overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HintStep {
    /// Part of a longer label (or a release): keep the overlay.
    Wait,
    Done(HintAction),
    Close,
}

/// Feed one key to the hint overlay.
pub fn hint_key(h: &mut Hints, ev: &KeyEvent) -> HintStep {
    if ev.kind == KeyKind::Release {
        return HintStep::Wait;
    }
    let c = match ev.key {
        Key::Char(c) if !ev.mods.ctrl() && !ev.mods.alt() => c,
        Key::Named(NamedKey::Backspace) => {
            h.typed.pop();
            return HintStep::Wait;
        }
        _ => return HintStep::Close,
    };
    let copy = c.is_uppercase() || ev.mods.shift() || h.typed.starts_with('!');
    let lc = fold(c);
    if !LABEL_CHARS.contains(lc) {
        return HintStep::Close;
    }
    let marker = if copy && h.typed.is_empty() { "!" } else { "" };
    h.typed.push_str(marker);
    h.typed.push(lc);
    let typed = h.typed.trim_start_matches('!').to_string();
    let copy = h.typed.starts_with('!');
    if let Some(hint) = h.items.iter().find(|x| x.label == typed) {
        let text = hint.text.clone();
        return HintStep::Done(if copy || hint.kind != HintKind::Url {
            HintAction::Copy(text)
        } else {
            HintAction::Open(text)
        });
    }
    if h.items.iter().any(|x| x.label.starts_with(&typed)) {
        HintStep::Wait
    } else {
        HintStep::Close
    }
}

pub fn hints_key(app: &mut App, ev: KeyEvent, mut h: Hints) {
    let step = hint_key(&mut h, &ev);
    // Plugin link handlers matching the target are offered first (07 §7.7); SHIFT+label
    // still copies.
    if let HintStep::Done(a) = &step
        && !h.typed.starts_with('!')
    {
        let (text, open) = match a {
            HintAction::Open(t) => (t.clone(), true),
            HintAction::Copy(t) => (t.clone(), false),
        };
        if crate::plugins::offer_link(app, h.machine, &h.pane, &text, open) {
            return;
        }
    }
    match step {
        HintStep::Close => {}
        HintStep::Wait => app.mode = Mode::Popup(Popup::Hints(h)),
        HintStep::Done(HintAction::Copy(t)) => app.set_clipboard(t.as_bytes(), false),
        HintStep::Done(HintAction::Open(url)) => open_url(app, h.machine, &h.pane, &url),
    }
}

/// Open a URL from a pane: loopback URLs in a browser pane next to it (that machine's
/// `localhost`), others with the OS opener on this machine (http/https only).
pub fn open_url(app: &mut App, mi: usize, pane: &str, url: &str) {
    let host = url
        .split("://")
        .nth(1)
        .and_then(|r| r.split(['/', '?', '#']).next())
        .map(|a| {
            if let Some(r) = a.strip_prefix('[') {
                r.split(']').next().unwrap_or("").to_string()
            } else {
                a.rsplit_once(':').map_or(a, |(h, _)| h).to_string()
            }
        })
        .unwrap_or_default();
    let loopback = matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1" | "0.0.0.0")
        || host.ends_with(".localhost");
    if loopback {
        crate::browser::open_url(app, mi, pane, &url.replace("://0.0.0.0", "://localhost"));
        return;
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        app.set_clipboard(url.as_bytes(), false);
        return;
    }
    app.nav.opened.push(url.to_string());
    if !cfg!(test) {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let _ = std::process::Command::new(opener)
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    app.toast(format!("opened {url}"));
}

pub fn draw_hints(app: &App, g: &mut Grid, h: &Hints) {
    let t = app.theme;
    let Some((_, r)) = app.pane_rects().into_iter().find(|(p, _)| *p == h.pane) else {
        return;
    };
    let typed = h.typed.trim_start_matches('!');
    let label_st = Style {
        attrs: attr::BOLD,
        ..t.rev()
    };
    let matched_st = Style {
        attrs: attr::BOLD,
        ..t.sel(t.yellow)
    };
    for it in &h.items {
        if !it.label.starts_with(typed) || it.row >= r.h || it.col >= r.w {
            continue;
        }
        let st = if typed.is_empty() {
            label_st
        } else {
            matched_st
        };
        g.put_str(r.x + it.col, r.y + it.row, &it.label, st, r.w - it.col);
    }
    let y = r.y + r.h.saturating_sub(1);
    g.fill(
        SRect {
            x: r.x,
            y,
            w: r.w,
            h: 1,
        },
        t.text(),
    );
    g.put_str(
        r.x,
        y,
        " hints: type a label — URLs open, the rest copy · SHIFT+label copies · esc",
        t.bold(t.yellow),
        r.w,
    );
}

// ---- terminal title ----------------------------------------------------------------------------

/// The title for the focused workspace/pane per `ui.title_format` (None when disabled).
pub fn title(app: &App) -> Option<String> {
    if !app.config.ui.title_sync {
        return None;
    }
    // A plugin's `client.window_title.set` replaces the formatted title until cleared (M5).
    if let Some(t) = crate::plugins::window_title(app) {
        return Some(t.to_string());
    }
    let m = app.m();
    let ws = app
        .focused_ws()
        .map(|w| w.display_name().to_string())
        .unwrap_or_default();
    let tab = app
        .focused_tab()
        .map(|t| t.title.unwrap_or_else(|| t.number.to_string()))
        .unwrap_or_default();
    let pane = app
        .focused_pane()
        .and_then(|p| {
            let run = m.model.runs.iter().find(|r| r.pane == p);
            run.map(|r| r.name.clone().unwrap_or_else(|| r.harness.clone()))
                .or_else(|| {
                    m.model
                        .panes
                        .iter()
                        .find(|x| x.id == p)
                        .map(|x| x.display_title().to_string())
                })
        })
        .unwrap_or_default();
    let s = app
        .config
        .ui
        .title_format
        .replace("{workspace}", &ws)
        .replace("{tab}", &tab)
        .replace("{pane}", &pane)
        .replace("{machine}", &m.label)
        .replace("{session}", &app.nav.session);
    // No control characters in OSC 2; keep it short.
    let s: String = s.chars().filter(|c| !c.is_control()).take(120).collect();
    let s = s.trim().trim_matches('·').trim().to_string();
    Some(if s.is_empty() { "vibeke".into() } else { s })
}

/// Bytes to write when the title changed: the first time the host's title is pushed
/// (`CSI 22;2t`, popped on exit), then OSC 2.
pub fn title_update(app: &mut App) -> Option<Vec<u8>> {
    let t = title(app)?;
    if app.nav.title.as_deref() == Some(t.as_str()) {
        return None;
    }
    let mut out = Vec::new();
    if app.nav.title.is_none() {
        out.extend_from_slice(b"\x1b[22;2t");
        crate::term::title_pushed();
    }
    out.extend_from_slice(format!("\x1b]2;{t}\x07").as_bytes());
    app.nav.title = Some(t);
    Some(out)
}

#[cfg(test)]
#[path = "nav_tests.rs"]
mod tests;
