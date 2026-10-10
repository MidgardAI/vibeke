//! Handoffs in the TUI (16 §15.2): accepting work another host handed to this one, and handing a
//! pane off to a paired host.
//!
//! - **Incoming.** Each machine's `handoff.incoming.list` is kept here: read when a push-capable
//!   machine connects and whenever the handoffs list opens, then kept current by the
//!   `handoff.incoming`, `handoff.updated` and `handoff.expired` events. Pending and failed ones
//!   are `handoff` items in the attention inbox; enter on one opens the accept overlay.
//! - **Accept overlay** (`Popup::HandoffAccept`): what arrived (sender, repository, branch and
//!   head, untracked files, the secrets to bring yourself, the agent's last message, whether the
//!   conversation resumes), then the choices `handoff.incoming.get` suggests: the repository
//!   (a matching clone, **Browse…** for another clone, **Clone to…** a new folder), the worktree
//!   path and the branch, *Resume agent*, *Trust mise* / *Trust direnv*. Paths are picked with
//!   [`PathPicker`]. **Accept** sends `handoff.accept` on its own connection (a clone or import
//!   may take minutes; the render stream keeps flowing), shows the `handoff.updated` phases and
//!   focuses the new pane. A failure stays in the overlay for another try; `repo_mismatch` lists
//!   the clone's remotes. An import whose agent did not start offers **Retry resume**
//!   (`handoff.resume`).
//! - **Handoffs list**: the Handoffs tab of the Connections view ([`crate::connections`];
//!   `handoffs`, default `prefix+shift+h`, or the `⇣N` badge in the tab bar's right cluster
//!   while N handoffs wait): incoming handoffs and the ones being sent. `enter` opens (accept,
//!   or the imported pane), `d` declines, `r` retries the agent of an imported one, `x` cancels
//!   a send.
//! - **Send** (`handoff_send`, default `prefix+alt+h`): the peers from `handoff.peers` in a fuzzy
//!   list, then the user's other machines this TUI is attached to that are not peers yet ("Your
//!   machines (will pair)": choosing one runs `gateway.call peer.invite` there and `gateway.call
//!   peer.redeem` on the source first, the rules of the app's `planSend`), then a summary with
//!   *Interrupt agent if busy*; `handoff.send` starts a job the source gateway runs.
//!   `handoff.job` events drive the progress shown at the right of the tab bar ("⇢ marvin
//!   42%"); where a send ended shows as a toast.
//! - **Pane menu:** a right-click on a pane's sidebar row opens the palette on that pane's
//!   actions ([`PANE_MENU`]): **Hand off…**, **Share this pane or workspace with someone…**
//!   ([`crate::people`]) and, for a pane an import created, **Handoff details**
//!   (`handoff_details`, the accept overlay in its imported state).
//!
//! Peers, invitations and pasting a teammate's invitation live in [`crate::sharing`].

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use unicode_width::UnicodeWidthStr;
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};
use vk_proto::render::{ServerFrame, Style, attr};

use crate::app::{App, Connector, Mode, Pending, Popup, RpcErr};
use crate::draw::truncate;
use crate::inbox::{Item, ItemKey, fmt_age};
use crate::nav::{ListKey, fuzzy, highlight, list_frame, list_key, list_row};
use crate::path_picker::{DirSource, Outcome, PathPicker};
use crate::popups::frame;
use crate::screen::{Grid, Rect as SRect};

/// Harnesses an import can start (and resume) an agent for (`vk_handoff::known_harness`).
const KNOWN_HARNESSES: &[&str] = &["claude", "codex", "pi", "omp"];

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn text(v: &Value, p: &str) -> Option<String> {
    v.pointer(p)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The message of an error value (`{message}` or a plain string).
fn message_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        _ => v["message"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| v.to_string()),
    }
}

// ---- records ------------------------------------------------------------------------------------

/// One incoming handoff (`HandoffIncoming`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Rec {
    pub id: String,
    pub from_host: String,
    pub from_user: Option<String>,
    /// `self` (one of the user's own hosts) or `teammate`.
    pub owner: String,
    pub repo_name: String,
    pub origin: Option<String>,
    pub branch: Option<String>,
    pub head: String,
    pub harness: Option<String>,
    pub session_id: Option<String>,
    pub transcript: bool,
    /// (path, reason) of files that were not carried.
    pub skipped: Vec<(String, String)>,
    pub last_message: Option<String>,
    pub untracked: u64,
    pub redactions: u64,
    pub size: u64,
    /// `pending` | `importing` | `imported` | `failed` | `declined`.
    pub state: String,
    pub error: Option<Value>,
    pub result: Option<Value>,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
}

impl Rec {
    pub fn from_value(v: &Value) -> Option<Rec> {
        let id = v.get("id")?.as_str()?.to_string();
        let m = &v["manifest"];
        let skipped = m["skipped"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|x| {
                        (
                            x["path"].as_str().unwrap_or_default().to_string(),
                            x["reason"].as_str().unwrap_or_default().to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(Rec {
            id,
            from_host: text(v, "/from/host").unwrap_or_default(),
            from_user: text(v, "/from/user"),
            owner: text(v, "/from/owner").unwrap_or_default(),
            repo_name: text(m, "/repo_name").unwrap_or_default(),
            origin: text(m, "/origin"),
            branch: text(m, "/branch"),
            head: text(m, "/head").unwrap_or_default(),
            harness: text(m, "/harness"),
            session_id: text(m, "/session_id"),
            transcript: m["transcript"].as_bool().unwrap_or(false),
            skipped,
            last_message: text(m, "/last_message"),
            untracked: m["untracked"].as_u64().unwrap_or(0),
            redactions: m["redactions"].as_u64().unwrap_or(0),
            size: v["size"].as_u64().unwrap_or(0),
            state: text(v, "/state").unwrap_or_else(|| "pending".into()),
            error: v.get("error").filter(|e| !e.is_null()).cloned(),
            result: v.get("result").filter(|e| !e.is_null()).cloned(),
            created_at_ms: v["created_at_ms"].as_i64().unwrap_or(0),
            expires_at_ms: v["expires_at_ms"].as_i64().unwrap_or(0),
        })
    }

    pub fn branch(&self) -> &str {
        self.branch.as_deref().unwrap_or("detached")
    }

    /// A harness the import can start an agent for.
    pub fn agent(&self) -> Option<&str> {
        self.harness
            .as_deref()
            .filter(|h| KNOWN_HARNESSES.contains(h))
    }

    /// The conversation itself resumes (known harness, session id and transcript), rather than a
    /// fresh agent starting with a handoff note.
    pub fn resumable(&self) -> bool {
        self.agent().is_some() && self.session_id.is_some() && self.transcript
    }

    /// Waiting for the receiver: pending, or failed and accepted again.
    pub fn waiting(&self) -> bool {
        matches!(self.state.as_str(), "pending" | "failed")
    }

    pub fn error_message(&self) -> Option<String> {
        self.error.as_ref().map(message_of)
    }

    /// The imported pane.
    pub fn pane(&self) -> Option<String> {
        text(self.result.as_ref()?, "/pane")
    }

    /// Why the imported agent did not start.
    pub fn agent_error(&self) -> Option<String> {
        let e = self
            .result
            .as_ref()?
            .get("agent_error")
            .filter(|e| !e.is_null())?;
        Some(message_of(e))
    }

    /// Secret files the sender kept back (the receiver brings their own).
    pub fn secrets(&self) -> Vec<&str> {
        self.skipped
            .iter()
            .filter(|(_, r)| r == "secret")
            .map(|(p, _)| p.as_str())
            .collect()
    }

    /// "marvin (your host)", "laptop (teammate, Ann <ann@x>)".
    pub fn from_label(&self) -> String {
        let who = if self.owner == "self" {
            "your host"
        } else {
            "teammate"
        };
        match &self.from_user {
            Some(u) => format!("{} ({who}, {u})", self.from_host),
            None => format!("{} ({who})", self.from_host),
        }
    }
}

/// One outgoing handoff (`handoff.jobs`, `handoff.job`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Job {
    pub id: String,
    pub pane: String,
    pub peer: String,
    pub peer_name: String,
    /// `queued` | `exporting` | `sending` | `delivered` | `failed` | `cancelled`.
    pub state: String,
    pub sent: u64,
    pub total: u64,
    /// The destination's incoming state once delivered (`pending`, `imported`, …).
    pub incoming_state: Option<String>,
    pub error: Option<String>,
}

impl Job {
    pub fn from_value(v: &Value) -> Option<Job> {
        Some(Job {
            id: v.get("id")?.as_str()?.to_string(),
            pane: text(v, "/pane").unwrap_or_default(),
            peer: text(v, "/peer").unwrap_or_default(),
            peer_name: text(v, "/peer_name").unwrap_or_default(),
            state: text(v, "/state").unwrap_or_else(|| "queued".into()),
            sent: v["sent"].as_u64().unwrap_or(0),
            total: v["total"].as_u64().unwrap_or(0),
            incoming_state: text(v, "/incoming_state"),
            error: v.get("error").filter(|e| !e.is_null()).map(message_of),
        })
    }

    pub fn name(&self) -> &str {
        if self.peer_name.is_empty() {
            &self.peer
        } else {
            &self.peer_name
        }
    }

    /// Still on its way (cancellable).
    pub fn active(&self) -> bool {
        matches!(self.state.as_str(), "queued" | "exporting" | "sending")
    }
}

/// What a send shows: "⇢ marvin 42%" while sending, then where it ended.
pub fn job_status(j: &Job) -> String {
    let who = j.name();
    match j.state.as_str() {
        "queued" => format!("⇢ {who} queued"),
        "exporting" => format!("⇢ {who} exporting…"),
        "sending" => match (j.sent.saturating_mul(100)).checked_div(j.total) {
            Some(p) => format!("⇢ {who} {}%", p.min(100)),
            None => format!("⇢ {who} sending…"),
        },
        "delivered" => match j.incoming_state.as_deref() {
            Some("imported") => format!("⇢ {who} imported"),
            Some("importing") => format!("⇢ {who} importing…"),
            Some("declined") => format!("⇢ {who} declined"),
            Some("failed") => format!("⇢ {who} delivered — import failed there, waiting"),
            _ => format!("⇢ {who} delivered — waiting for accept"),
        },
        "failed" => format!(
            "⇢ {who} failed: {}",
            j.error.as_deref().unwrap_or("unknown error")
        ),
        "cancelled" => format!("⇢ {who} cancelled"),
        other => format!("⇢ {who} {other}"),
    }
}

/// A paired host a pane can be handed off to (`handoff.peers`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Peer {
    pub id: String,
    pub name: String,
    /// `self` or `teammate`.
    pub owner: String,
    /// When the pairing ends (unix seconds from the gateway, or epoch ms, or the server's
    /// text); none for your own hosts.
    pub expires_at: Option<Value>,
    /// The gateway says the pairing has ended.
    pub expired: bool,
}

/// Unix seconds or epoch ms as epoch ms (the gateway counts seconds; nothing in ms is that small).
pub fn epoch_ms(t: i64) -> i64 {
    if (1..100_000_000_000).contains(&t) {
        t * 1000
    } else {
        t
    }
}

impl Peer {
    pub fn from_value(v: &Value) -> Option<Peer> {
        let id = v.get("id")?.as_str()?.to_string();
        Some(Peer {
            name: text(v, "/name").unwrap_or_else(|| id.clone()),
            id,
            owner: text(v, "/owner").unwrap_or_default(),
            expires_at: v.get("expires_at").filter(|e| !e.is_null()).cloned(),
            expired: v["expired"].as_bool().unwrap_or(false),
        })
    }

    /// The pairing has ended (the gateway says so, or its expiry passed).
    pub fn is_expired(&self, now: i64) -> bool {
        self.expired
            || matches!(&self.expires_at, Some(Value::Number(n))
                if n.as_i64().is_some_and(|t| epoch_ms(t) <= now))
    }

    /// "your host", "teammate · expires in 3d".
    pub fn note(&self, now: i64) -> String {
        let who = if self.owner == "self" {
            "your host"
        } else {
            "teammate"
        };
        if self.expired {
            return format!("{who} · expired");
        }
        match &self.expires_at {
            Some(Value::Number(n)) => match n.as_i64().map(epoch_ms) {
                Some(t) if t > now => format!("{who} · expires in {}", fmt_age(t - now)),
                Some(_) => format!("{who} · expired"),
                None => who.to_string(),
            },
            Some(Value::String(s)) => format!("{who} · expires {s}"),
            _ => who.to_string(),
        }
    }
}

// ---- state --------------------------------------------------------------------------------------

#[derive(Default)]
pub struct State {
    /// Incoming handoffs per machine, newest first.
    pub incoming: BTreeMap<usize, Vec<Rec>>,
    /// Outgoing handoffs per machine, newest first.
    pub jobs: BTreeMap<usize, Vec<Job>>,
    /// Machines whose server has no incoming handoffs (`method_not_found`).
    pub unsupported: HashSet<usize>,
    pub accept: Option<Accept>,
    pub send: Option<SendForm>,
    /// Selected row of the handoffs list.
    pub list_sel: usize,
    /// The handoffs list asks before declining this one.
    pub list_confirm: Option<String>,
    /// Tests: where the pickers read folders (else the local disk on a local machine).
    pub dirs: Option<Arc<dyn DirSource>>,
    /// The pane a right-click on its sidebar row picked for the handoff actions (machine,
    /// pane); dropped once the palette it opened is gone.
    pub target: Option<(usize, String)>,
}

/// Replies routed back here (through `ux::Reply::Handoff`).
#[derive(Debug, Clone)]
pub enum Reply {
    List,
    Jobs,
    Get {
        id: String,
    },
    Accept {
        id: String,
    },
    Decline {
        id: String,
    },
    Resume {
        id: String,
    },
    Peers,
    /// `handoff.send` of the form whose `send_op` is `op` (a later form ignores it).
    Send {
        op: u64,
    },
    Cancel,
    /// Auto-pairing for a send: `gateway.call peer.invite` on the destination machine.
    PairInvite {
        op: PairOp,
    },
    /// Then `gateway.call peer.list` on the source: is the invitation's host (by id) already
    /// one of its own peers?
    PairList {
        op: PairOp,
        link: String,
        /// The invitation's pairing id on the destination (revoked when it goes unused).
        pid: Option<String>,
        /// The destination host's id, from the link.
        host: String,
    },
    /// Then `gateway.call peer.redeem` on the source.
    PairRedeem {
        op: PairOp,
    },
}

fn pend(r: Reply) -> Pending {
    Pending::Ux(crate::ux::Reply::Handoff(r))
}

/// Ask machine `mi` for its incoming handoffs and sends.
pub fn refresh(app: &mut App, mi: usize) {
    if !app.machines[mi].connected() {
        return;
    }
    if !app.ux.handoff.unsupported.contains(&mi) {
        app.command_on(mi, "handoff.incoming.list", json!({}), pend(Reply::List));
    }
    app.command_on(mi, "handoff.jobs", json!({}), pend(Reply::Jobs));
}

/// After (re)connecting: a server that pushes events keeps the list current from here on, so it
/// is read once now (the inbox shows waiting handoffs without opening anything). Older servers
/// are asked when the handoffs list opens.
pub fn on_connected(app: &mut App, mi: usize) {
    app.ux.handoff.unsupported.remove(&mi);
    if crate::push::supported(app, mi) {
        refresh(app, mi);
    }
}

fn upsert(app: &mut App, mi: usize, r: Rec) {
    if let Some(a) = app.ux.handoff.accept.as_mut()
        && a.mi == mi
        && a.id == r.id
    {
        a.rec = Some(r.clone());
    }
    let list = app.ux.handoff.incoming.entry(mi).or_default();
    match list.iter_mut().find(|x| x.id == r.id) {
        Some(x) => *x = r,
        None => list.insert(0, r),
    }
    crate::inbox::invalidate(app);
    app.dirty = true;
}

/// Record a job; a send that just ended (or whose destination moved on) shows a toast.
fn upsert_job(app: &mut App, mi: usize, j: Job) {
    let jobs = app.ux.handoff.jobs.entry(mi).or_default();
    let before = match jobs.iter().position(|x| x.id == j.id) {
        Some(i) => Some(std::mem::replace(&mut jobs[i], j.clone())),
        None => {
            jobs.insert(0, j.clone());
            None
        }
    };
    jobs.truncate(50);
    let changed = before
        .as_ref()
        .is_none_or(|b| b.state != j.state || b.incoming_state != j.incoming_state);
    if changed && !j.active() {
        app.toast(job_status(&j));
    }
    app.dirty = true;
}

/// Sends in progress, for the right side of the tab bar.
pub fn status(app: &App) -> Option<String> {
    let active: Vec<String> = app
        .ux
        .handoff
        .jobs
        .values()
        .flatten()
        .filter(|j| j.active())
        .map(job_status)
        .collect();
    match active.len() {
        0 => None,
        1 | 2 => Some(active.join(" · ")),
        n => Some(format!("{} · +{} more", active[..2].join(" · "), n - 2)),
    }
}

/// Incoming handoffs waiting for the user (pending or failed, not expired), on every machine.
pub fn waiting_count(app: &App) -> usize {
    inbox_items(app).len()
}

/// The `⇣N` badge in the tab bar's right cluster while handoffs wait: chrome only, like the
/// elevation notice (`elevate::notice`), never over a pane. A click on it opens the list.
pub fn badge(app: &App) -> Option<String> {
    match waiting_count(app) {
        0 => None,
        n => Some(format!(" ⇣{n} ")),
    }
}

/// A left click on the badge opens the handoffs list.
pub fn on_mouse(app: &mut App, me: &crossterm::event::MouseEvent) -> bool {
    use crossterm::event::{MouseButton, MouseEventKind};
    if !matches!(me.kind, MouseEventKind::Down(MouseButton::Left)) {
        return false;
    }
    let Some(b) = badge(app) else {
        return false;
    };
    if crate::draw::right_cluster_at(app, me.column, me.row).as_deref() != Some(b.as_str()) {
        return false;
    }
    let mi = app.cur;
    crate::connections::open_tab(app, crate::connections::Tab::Handoffs, mi);
    app.dirty = true;
    true
}

/// Forget a right-click target once the palette it opened is gone.
pub fn tick(app: &mut App) {
    if app.ux.handoff.target.is_some() && !matches!(app.mode, Mode::Popup(Popup::Palette { .. })) {
        app.ux.handoff.target = None;
    }
}

/// The actions a right-click on a pane's sidebar row offers (the palette shows only these while
/// a pane is targeted). Each takes its pane with [`take_target`].
pub const PANE_MENU: &[&str] = &[
    "handoff_send",
    "cloud_send",
    "cloud_bring_back",
    "share_pane",
    "handoff_details",
];

/// Right-click on a pane's sidebar row: its actions ([`PANE_MENU`]), in the palette.
pub fn pane_menu(app: &mut App, mi: usize, pane: &str) {
    app.ux.handoff.target = Some((mi, pane.to_string()));
    crate::nav::open_palette(app, String::new());
}

/// The pane an action of the pane menu applies to: the right-clicked one, else the focused
/// pane.
pub(crate) fn take_target(app: &mut App) -> Option<(usize, String)> {
    if let Some((mi, p)) = app.ux.handoff.target.take()
        && app
            .machines
            .get(mi)
            .is_some_and(|m| m.model.panes.iter().any(|x| x.id == p))
    {
        return Some((mi, p));
    }
    Some((app.cur, app.focused_pane()?))
}

/// The incoming handoff whose import created `pane` on machine `mi`.
pub fn imported_into(app: &App, mi: usize, pane: &str) -> Option<String> {
    app.ux
        .handoff
        .incoming
        .get(&mi)?
        .iter()
        .find(|r| r.state == "imported" && r.pane().as_deref() == Some(pane))
        .map(|r| r.id.clone())
}

/// `handoff_details`: the accept overlay of the handoff an imported pane came from (what
/// arrived, where it went, Retry resume).
pub fn open_details(app: &mut App) {
    let Some((mi, pane)) = take_target(app) else {
        app.toast("no focused pane");
        return;
    };
    match imported_into(app, mi, &pane) {
        Some(id) => open_accept(app, mi, &id),
        None => {
            if !app.ux.handoff.unsupported.contains(&mi)
                && !app.ux.handoff.incoming.contains_key(&mi)
            {
                refresh(app, mi);
            }
            app.toast("this pane didn't come from a handoff");
        }
    }
}

/// `handoff.updated` phases as the overlay shows them.
pub fn phase_text(phase: &str) -> String {
    match phase {
        "cloning" => "cloning the repository…".into(),
        "importing" => "importing the work…".into(),
        "starting" => "starting the agent…".into(),
        other => format!("{other}…"),
    }
}

/// Pushed `handoff.*` events.
pub fn on_event(app: &mut App, mi: usize, kind: &str, v: &Value) {
    let data = &v["data"];
    match kind {
        "handoff.incoming" | "handoff.updated" => {
            let Some(r) = Rec::from_value(&data["incoming"]) else {
                return;
            };
            if let Some(phase) = data["phase"].as_str()
                && let Some(a) = app.ux.handoff.accept.as_mut()
                && a.mi == mi
                && a.id == r.id
                && a.busy.is_some()
            {
                a.busy = Some(phase_text(phase));
            }
            upsert(app, mi, r);
        }
        "handoff.expired" => {
            let Some(id) = v["subject"]["incoming"].as_str() else {
                return;
            };
            if let Some(list) = app.ux.handoff.incoming.get_mut(&mi) {
                list.retain(|r| r.id != id);
            }
            if let Some(a) = app.ux.handoff.accept.as_mut()
                && a.mi == mi
                && a.id == id
                && a.busy.is_none()
            {
                a.error = Some("this handoff expired".into());
            }
            crate::inbox::invalidate(app);
        }
        "handoff.job" => {
            let job = data.get("job").unwrap_or(data);
            if let Some(j) = Job::from_value(job) {
                upsert_job(app, mi, j);
            }
        }
        _ => {}
    }
    app.dirty = true;
}

/// Waiting handoffs as attention-inbox items (kind `handoff`), oldest first.
pub fn inbox_items(app: &App) -> Vec<Item> {
    let now = now_ms();
    let mut v = Vec::new();
    for (&mi, list) in &app.ux.handoff.incoming {
        let Some(m) = app.machines.get(mi) else {
            continue;
        };
        for r in list
            .iter()
            .filter(|r| r.waiting() && (r.expires_at_ms == 0 || r.expires_at_ms > now))
        {
            let expires = if r.expires_at_ms > now {
                format!(" Expires in {}.", fmt_age(r.expires_at_ms - now))
            } else {
                String::new()
            };
            let explanation = match (r.state.as_str(), r.error_message()) {
                ("failed", Some(e)) => format!(
                    "The last import failed: {e}. Enter opens the accept screen to try again.{expires}"
                ),
                _ => format!(
                    "Enter opens the accept screen: choose the repository, worktree and branch.{expires}"
                ),
            };
            v.push(Item {
                key: ItemKey {
                    machine: mi,
                    kind: "handoff".into(),
                    id: r.id.clone(),
                },
                class: 3,
                title: format!("Incoming handoff from {}: {}", r.from_host, r.branch()),
                subtitle: format!("{} · {}", r.repo_name, r.from_label()),
                task: None,
                run: None,
                pane: None,
                interaction: None,
                explanation,
                age_ms: (now - r.created_at_ms).max(0),
                risk: None,
                effort: None,
                blocks_tasks: 0,
                effort_estimate: None,
                snoozed_until_ms: None,
                woke_from_snooze: None,
                urgent: false,
                raw_key: json!({"kind": "handoff", "id": r.id}),
                fallback: false,
                stale: !m.connected(),
                deadline_ms: None,
                deadline_source: None,
                batch: None,
            });
        }
    }
    v.sort_by_key(|i| std::cmp::Reverse(i.age_ms));
    v
}

/// Where the pickers of machine `mi` read folders: its disk when it is this machine, else its
/// server.
fn dirs(app: &App, mi: usize) -> Arc<dyn DirSource> {
    if let Some(d) = &app.ux.handoff.dirs {
        return d.clone();
    }
    crate::path_picker::dirs_for(app, mi)
}

// ---- a long call on its own connection ------------------------------------------------------------

/// Send `method` on a separate control connection and route the answer like a render-stream
/// command result: an import (or a clone) can take minutes, and the render stream answers its
/// commands in order. Tests (no worker) use the render stream.
fn call_long(app: &mut App, mi: usize, method: &str, params: Value, reply: Reply) {
    call_long_pending(app, mi, method, params, pend(reply));
}

/// [`call_long`] with any reply route (the Sharing view's `gateway.call`s use it too: a call
/// through the gateway may wait up to its `timeout_ms`).
pub(crate) fn call_long_pending(
    app: &mut App,
    mi: usize,
    method: &str,
    params: Value,
    pending: Pending,
) {
    let worker = app
        .uploads
        .worker
        .as_ref()
        .and_then(|w| Some((w.connectors.get(mi)?.clone(), w.inc.clone())));
    let Some((conn, inc)) = worker else {
        app.command_on(mi, method, params, pending);
        return;
    };
    let req = app.next_req;
    app.next_req += 1;
    app.machines[mi].pending.insert(req, pending);
    let line = json!({"jsonrpc": "2.0", "id": req, "method": method, "params": params}).to_string();
    tokio::spawn(async move {
        let json = match call_once(&conn, &line, req).await {
            Ok(s) => s,
            Err(e) => json!({"jsonrpc": "2.0", "id": req, "error": {"code": -32000, "message": e,
                "data": {"kind": "remote_unavailable", "details": null}}})
            .to_string(),
        };
        let _ = inc.send(crate::app::Incoming::Frame(
            mi,
            ServerFrame::CommandResult { req, json },
        ));
    });
}

async fn call_once(conn: &Arc<Connector>, line: &str, req: u64) -> Result<String, String> {
    let stream = (conn)().await.map_err(|e| format!("connect: {e}"))?;
    let (rd, mut wr) = tokio::io::split(stream);
    let mut rd = BufReader::new(rd);
    let io = |e: std::io::Error| format!("connection lost: {e}");
    wr.write_all(line.as_bytes()).await.map_err(io)?;
    wr.write_all(b"\n").await.map_err(io)?;
    wr.flush().await.map_err(io)?;
    loop {
        let mut buf = String::new();
        if rd.read_line(&mut buf).await.map_err(io)? == 0 {
            return Err("connection closed before the answer".into());
        }
        let v: Value = serde_json::from_str(&buf).map_err(|e| e.to_string())?;
        if v.get("id").and_then(Value::as_u64) == Some(req) {
            return Ok(buf.trim_end().to_string());
        }
    }
}

// ---- accept overlay ------------------------------------------------------------------------------

/// The repository an accepted handoff goes into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoSel {
    /// One of the clones `handoff.incoming.get` suggested.
    Suggested(usize),
    /// Another clone, picked by path.
    Browse,
    /// A fresh clone of the origin.
    CloneTo,
}

/// Focus stops of the overlay, top to bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Repo(usize),
    Browse,
    CloneTo,
    Worktree,
    Branch,
    Resume,
    TrustMise,
    TrustDirenv,
    Accept,
    Decline,
    Later,
}

/// What a path picker in the overlay is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickFor {
    Browse,
    CloneTo,
    Worktree,
}

/// `repo_mismatch`: the chosen clone's remotes don't include the origin.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Mismatch {
    pub repo: String,
    pub origin: String,
    pub remotes: Vec<String>,
}

/// Imported, but the agent did not start (or no workspace opened).
#[derive(Debug, Clone, PartialEq)]
pub struct Done {
    pub pane: Option<String>,
    pub note: String,
    pub problem: String,
}

pub struct Accept {
    pub mi: usize,
    pub id: String,
    pub rec: Option<Rec>,
    /// Waiting for `handoff.incoming.get`.
    pub loading: bool,
    pub repos: Vec<String>,
    pub suggested_repo: Option<String>,
    pub suggested_worktree: Option<String>,
    pub suggested_branch: String,
    pub repo: RepoSel,
    pub browse_path: Option<String>,
    pub clone_to: String,
    /// `None`: the server's default next to the repository.
    pub worktree: Option<String>,
    pub worktree_edited: bool,
    pub branch: String,
    pub resume: bool,
    pub trust_mise: bool,
    pub trust_direnv: bool,
    pub focus: Row,
    pub picker: Option<(PickFor, PathPicker)>,
    /// The import is running: its phase.
    pub busy: Option<String>,
    pub error: Option<String>,
    pub mismatch: Option<Mismatch>,
    pub done: Option<Done>,
    pub confirm_decline: bool,
    /// Opened from the handoffs list: closing goes back there.
    pub from_list: bool,
    fs: Arc<dyn DirSource>,
}

/// A folder name for a fresh clone of `repo_name`.
fn clone_name(repo_name: &str) -> String {
    let last = repo_name
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or_default();
    let last = last.strip_suffix(".git").unwrap_or(last);
    let name: String = last
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let name = name.trim_matches('.').to_string();
    if name.is_empty() { "repo".into() } else { name }
}

/// `<parent of the suggested clone, or ~/code>/<repo name>`, not one of the known clones.
pub fn default_clone_to(repo: Option<&str>, repo_name: &str, known: &[String]) -> String {
    let name = clone_name(repo_name);
    let parent = repo
        .and_then(|r| Path::new(r).parent())
        .map(|p| p.to_string_lossy().trim_end_matches('/').to_string())
        .filter(|p| !p.is_empty());
    let path = match parent {
        Some(p) => format!("{p}/{name}"),
        None => format!("~/code/{name}"),
    };
    if known.iter().any(|k| k.trim_end_matches('/') == path) {
        format!("{path}-2")
    } else {
        path
    }
}

fn parent_dir(p: &str) -> Option<String> {
    Path::new(p.trim_end_matches('/'))
        .parent()
        .map(|x| x.to_string_lossy().trim_end_matches('/').to_string())
        .map(|x| format!("{x}/"))
}

impl Accept {
    pub fn new(mi: usize, id: &str, rec: Option<Rec>, fs: Arc<dyn DirSource>) -> Accept {
        let agent = rec.as_ref().is_some_and(|r| r.agent().is_some());
        Accept {
            mi,
            id: id.to_string(),
            rec,
            loading: true,
            repos: Vec::new(),
            suggested_repo: None,
            suggested_worktree: None,
            suggested_branch: String::new(),
            repo: RepoSel::Browse,
            browse_path: None,
            clone_to: String::new(),
            worktree: None,
            worktree_edited: false,
            branch: String::new(),
            resume: agent,
            trust_mise: false,
            trust_direnv: false,
            focus: Row::Accept,
            picker: None,
            busy: None,
            error: None,
            mismatch: None,
            done: None,
            confirm_decline: false,
            from_list: false,
            fs,
        }
    }

    /// Fill in a `handoff.incoming.get` result.
    pub fn load(&mut self, v: &Value) {
        if let Some(r) = Rec::from_value(&v["incoming"]) {
            self.rec = Some(r);
        }
        let s = &v["suggested"];
        self.repos = strings(&s["repos"]);
        self.suggested_repo = text(s, "/repo");
        if let Some(r) = &self.suggested_repo
            && !self.repos.contains(r)
        {
            self.repos.insert(0, r.clone());
        }
        self.suggested_worktree = text(s, "/worktree_path");
        self.suggested_branch = text(s, "/branch").unwrap_or_default();
        self.branch = self.suggested_branch.clone();
        let origin = self.rec.as_ref().and_then(|r| r.origin.clone());
        self.repo = match &self.suggested_repo {
            Some(r) => RepoSel::Suggested(self.repos.iter().position(|x| x == r).unwrap_or(0)),
            None if !self.repos.is_empty() => RepoSel::Suggested(0),
            None if origin.is_some() => RepoSel::CloneTo,
            None => RepoSel::Browse,
        };
        let name = self
            .rec
            .as_ref()
            .map(|r| r.repo_name.clone())
            .unwrap_or_default();
        let base = self
            .suggested_repo
            .clone()
            .or_else(|| self.repos.first().cloned());
        self.clone_to = default_clone_to(base.as_deref(), &name, &self.repos);
        self.worktree_edited = false;
        self.worktree = self.default_worktree();
        self.resume = self.rec.as_ref().is_some_and(|r| r.agent().is_some());
        self.loading = false;
    }

    /// The path of the chosen repository (`None` for a clone or an unpicked Browse).
    pub fn repo_path(&self) -> Option<&str> {
        match self.repo {
            RepoSel::Suggested(i) => self.repos.get(i).map(String::as_str),
            RepoSel::Browse => self.browse_path.as_deref(),
            RepoSel::CloneTo => None,
        }
    }

    /// The suggested worktree belongs to the suggested repository; any other choice lets the
    /// server place it next to that repository.
    fn default_worktree(&self) -> Option<String> {
        match (self.repo, self.repo_path(), &self.suggested_repo) {
            (RepoSel::Suggested(_), Some(p), Some(s)) if p == s.as_str() => {
                self.suggested_worktree.clone()
            }
            _ => None,
        }
    }

    pub fn set_repo(&mut self, sel: RepoSel) {
        self.repo = sel;
        self.error = None;
        self.mismatch = None;
        if !self.worktree_edited {
            self.worktree = self.default_worktree();
        }
    }

    fn origin(&self) -> Option<&str> {
        self.rec.as_ref().and_then(|r| r.origin.as_deref())
    }

    fn has_agent(&self) -> bool {
        self.rec.as_ref().is_some_and(|r| r.agent().is_some())
    }

    pub fn rows(&self) -> Vec<Row> {
        let mut v: Vec<Row> = (0..self.repos.len()).map(Row::Repo).collect();
        v.push(Row::Browse);
        if self.origin().is_some() {
            v.push(Row::CloneTo);
        }
        v.extend([Row::Worktree, Row::Branch]);
        if self.has_agent() {
            v.push(Row::Resume);
        }
        v.extend([
            Row::TrustMise,
            Row::TrustDirenv,
            Row::Accept,
            Row::Decline,
            Row::Later,
        ]);
        v
    }

    fn move_focus(&mut self, by: i32, wrap: bool) {
        let rows = self.rows();
        let n = rows.len() as i32;
        let i = rows.iter().position(|r| *r == self.focus).unwrap_or(0) as i32;
        let j = if wrap {
            (i + by).rem_euclid(n)
        } else {
            (i + by).clamp(0, n - 1)
        };
        self.focus = rows[j as usize];
    }

    fn open_picker(&mut self, what: PickFor) {
        let initial = match what {
            PickFor::Browse => self.browse_path.clone().or_else(|| {
                self.suggested_repo
                    .as_deref()
                    .or(self.repos.first().map(String::as_str))
                    .and_then(parent_dir)
            }),
            PickFor::CloneTo => Some(self.clone_to.clone()),
            PickFor::Worktree => self
                .worktree
                .clone()
                .or_else(|| self.repo_path().and_then(parent_dir))
                .or_else(|| parent_dir(&self.clone_to)),
        }
        .unwrap_or_else(|| "~/".into());
        self.picker = Some((what, PathPicker::new(initial, self.fs.clone())));
    }

    fn picked(&mut self, what: PickFor, path: String) {
        let trimmed = path.trim_end_matches('/');
        let path = if trimmed.is_empty() {
            path.clone()
        } else {
            trimmed.to_string()
        };
        match what {
            PickFor::Browse => {
                self.browse_path = Some(path);
                self.set_repo(RepoSel::Browse);
            }
            PickFor::CloneTo => {
                self.clone_to = path;
                self.set_repo(RepoSel::CloneTo);
            }
            PickFor::Worktree => {
                self.worktree = Some(path);
                self.worktree_edited = true;
            }
        }
    }

    fn repo_param(&self) -> Result<Value, String> {
        match self.repo {
            RepoSel::Suggested(i) => self
                .repos
                .get(i)
                .map(|p| json!({"path": p}))
                .ok_or_else(|| "choose a repository".to_string()),
            RepoSel::Browse => self
                .browse_path
                .as_deref()
                .filter(|p| !p.trim().is_empty())
                .map(|p| json!({"path": p}))
                .ok_or_else(|| "pick a clone with Browse… (enter)".to_string()),
            RepoSel::CloneTo => {
                let to = self.clone_to.trim();
                if to.is_empty() {
                    Err("choose where to clone (enter on Clone to…)".into())
                } else {
                    Ok(json!({"clone_to": to}))
                }
            }
        }
    }

    /// `handoff.accept` params. The branch is sent only when it differs from the suggestion
    /// (the server then picks a free one), the worktree only when there is one to send.
    pub fn params(&self) -> Result<Value, String> {
        let mut p = json!({
            "id": self.id,
            "repo": self.repo_param()?,
            "start_agent": self.resume && self.has_agent(),
        });
        if let Some(w) = self
            .worktree
            .as_deref()
            .map(str::trim)
            .filter(|w| !w.is_empty())
        {
            p["worktree_path"] = json!(w);
        }
        let b = self.branch.trim();
        if !b.is_empty() && b != self.suggested_branch {
            p["branch"] = json!(b);
        }
        let trust: Vec<&str> = [("mise", self.trust_mise), ("direnv", self.trust_direnv)]
            .into_iter()
            .filter(|(_, on)| *on)
            .map(|(t, _)| t)
            .collect();
        if !trust.is_empty() {
            p["trust"] = json!(trust);
        }
        Ok(p)
    }
}

impl Mismatch {
    pub fn from_details(d: &Value) -> Mismatch {
        Mismatch {
            repo: text(d, "/repo").unwrap_or_default(),
            origin: text(d, "/origin").unwrap_or_default(),
            remotes: strings(&d["remotes"]),
        }
    }
}

/// Open the accept overlay for incoming handoff `id` on machine `mi`.
pub fn open_accept(app: &mut App, mi: usize, id: &str) {
    if !app.machines.get(mi).is_some_and(|m| m.connected()) {
        app.toast("that machine is offline");
        return;
    }
    let rec = app
        .ux
        .handoff
        .incoming
        .get(&mi)
        .and_then(|l| l.iter().find(|r| r.id == id))
        .cloned();
    let fs = dirs(app, mi);
    app.ux.handoff.accept = Some(Accept::new(mi, id, rec, fs));
    app.mode = Mode::Popup(Popup::HandoffAccept);
    app.command_on(
        mi,
        "handoff.incoming.get",
        json!({"id": id}),
        pend(Reply::Get { id: id.into() }),
    );
}

/// Close the overlay (the handoff keeps waiting), back to where it was opened from.
fn close(app: &mut App) {
    let from_list = app.ux.handoff.accept.take().is_some_and(|a| a.from_list);
    if from_list {
        app.mode = Mode::Popup(Popup::Handoffs);
    } else {
        app.restore_return();
    }
}

fn submit(app: &mut App) {
    let Some(a) = app.ux.handoff.accept.as_mut() else {
        return;
    };
    if a.loading || a.busy.is_some() {
        return;
    }
    match a.params() {
        Err(e) => {
            a.error = Some(e);
            a.mismatch = None;
        }
        Ok(p) => {
            a.error = None;
            a.mismatch = None;
            a.busy = Some("accepting…".into());
            let (mi, id) = (a.mi, a.id.clone());
            call_long(app, mi, "handoff.accept", p, Reply::Accept { id });
        }
    }
}

fn decline(app: &mut App, mi: usize, id: &str) {
    app.command_on(
        mi,
        "handoff.decline",
        json!({"id": id}),
        pend(Reply::Decline { id: id.into() }),
    );
}

fn resume(app: &mut App, mi: usize, id: &str) {
    app.command_on(
        mi,
        "handoff.resume",
        json!({"id": id}),
        pend(Reply::Resume { id: id.into() }),
    );
}

fn focus_imported(app: &mut App, mi: usize, pane: Option<String>) {
    app.ux.handoff.accept = None;
    app.return_to.clear();
    app.mode = Mode::Normal;
    if let Some(p) = pane {
        app.focus_pane(mi, &p);
    }
}

/// What a key in the overlay asks for, decided before anything outside the overlay changes.
enum Act {
    None,
    Close,
    Submit,
    Decline,
    Resume,
    OpenPane,
}

pub fn accept_key(app: &mut App, ev: KeyEvent) {
    app.mode = Mode::Popup(Popup::HandoffAccept);
    if ev.kind == KeyKind::Release {
        return;
    }
    let Some(a) = app.ux.handoff.accept.as_mut() else {
        app.mode = Mode::Normal;
        return;
    };
    let act = overlay_key(a, &ev);
    let (mi, id) = (a.mi, a.id.clone());
    request_listing(app);
    match act {
        Act::None => {}
        Act::Close => close(app),
        Act::Submit => submit(app),
        Act::Decline => decline(app, mi, &id),
        Act::Resume => resume(app, mi, &id),
        Act::OpenPane => {
            let pane = app
                .ux
                .handoff
                .accept
                .as_ref()
                .and_then(|a| a.done.as_ref())
                .and_then(|d| d.pane.clone());
            focus_imported(app, mi, pane);
        }
    }
    app.dirty = true;
}

fn overlay_key(a: &mut Accept, ev: &KeyEvent) -> Act {
    // A path picker on top takes every key.
    if let Some((what, mut p)) = a.picker.take() {
        match p.key(ev) {
            Outcome::Stay => a.picker = Some((what, p)),
            Outcome::Cancel => {}
            Outcome::Submit(_) => {
                let v = p.resolved();
                if !v.is_empty() {
                    a.picked(what, v);
                }
            }
        }
        return Act::None;
    }
    let esc = matches!(ev.key, Key::Named(NamedKey::Escape));
    let plain = !ev.mods.ctrl() && !ev.mods.alt();
    if a.done.is_some() {
        return match ev.key {
            _ if esc => Act::Close,
            Key::Char('r') if plain => Act::Resume,
            Key::Char('o') if plain => Act::OpenPane,
            Key::Named(NamedKey::Enter) => Act::OpenPane,
            _ => Act::None,
        };
    }
    if a.loading || a.busy.is_some() {
        // Closing leaves a running import alone; its outcome shows as a toast.
        return if esc { Act::Close } else { Act::None };
    }
    if a.confirm_decline {
        a.confirm_decline = false;
        return match ev.key {
            Key::Char('d' | 'y') | Key::Named(NamedKey::Enter) => Act::Decline,
            _ => Act::None,
        };
    }
    let button = matches!(a.focus, Row::Accept | Row::Decline | Row::Later);
    match &ev.key {
        _ if esc => return Act::Close,
        Key::Named(NamedKey::Tab) if ev.mods.shift() => a.move_focus(-1, true),
        Key::Named(NamedKey::Tab) => a.move_focus(1, true),
        Key::Named(NamedKey::Up) => a.move_focus(-1, false),
        Key::Named(NamedKey::Down) => a.move_focus(1, false),
        Key::Named(NamedKey::Left) if button => {
            a.focus = match a.focus {
                Row::Later => Row::Decline,
                _ => Row::Accept,
            }
        }
        Key::Named(NamedKey::Right) if button => {
            a.focus = match a.focus {
                Row::Accept => Row::Decline,
                _ => Row::Later,
            }
        }
        // The branch is typed in place.
        Key::Named(NamedKey::Backspace) if a.focus == Row::Branch => {
            a.branch.pop();
        }
        Key::Char('u') if a.focus == Row::Branch && ev.mods.ctrl() => a.branch.clear(),
        Key::Char(c) if a.focus == Row::Branch && plain && !c.is_whitespace() => {
            a.branch.push(*c);
        }
        Key::Named(NamedKey::Enter) => return activate(a, false),
        Key::Named(NamedKey::Space) => return activate(a, true),
        Key::Char(' ') if plain => return activate(a, true),
        Key::Char('a') if plain => return Act::Submit,
        Key::Char('d') if plain => a.confirm_decline = true,
        _ => {}
    }
    Act::None
}

/// `enter` (or `space`, which chooses without opening a picker) on the focused row.
fn activate(a: &mut Accept, space: bool) -> Act {
    match a.focus {
        Row::Repo(i) => a.set_repo(RepoSel::Suggested(i)),
        Row::Browse if space && a.browse_path.is_some() => a.set_repo(RepoSel::Browse),
        Row::Browse => a.open_picker(PickFor::Browse),
        Row::CloneTo if space => a.set_repo(RepoSel::CloneTo),
        Row::CloneTo => a.open_picker(PickFor::CloneTo),
        Row::Worktree => a.open_picker(PickFor::Worktree),
        Row::Branch => a.move_focus(1, false),
        Row::Resume => a.resume = !a.resume,
        Row::TrustMise => a.trust_mise = !a.trust_mise,
        Row::TrustDirenv => a.trust_direnv = !a.trust_direnv,
        Row::Accept => return Act::Submit,
        Row::Decline => a.confirm_decline = true,
        Row::Later => return Act::Close,
    }
    Act::None
}

/// Paste into the overlay: the open picker, or the branch field.
pub fn on_paste(app: &mut App, text: &str) {
    let Some(a) = app.ux.handoff.accept.as_mut() else {
        return;
    };
    if let Some((_, p)) = &mut a.picker {
        p.paste(text);
    } else if a.focus == Row::Branch && a.busy.is_none() {
        a.branch.extend(text.chars().filter(|c| !c.is_whitespace()));
    }
    request_listing(app);
    app.dirty = true;
}

/// A picker on a remote machine asks its server for the folder it shows.
fn request_listing(app: &mut App) {
    crate::path_picker::send_request(app, crate::path_picker::Owner::Handoff);
}

fn human(n: u64) -> String {
    crate::upload::human(n)
}

/// `~/…` for paths under this machine's home.
fn shown(app: &App, mi: usize, p: &str) -> String {
    if app.machines.get(mi).is_some_and(|m| m.local)
        && let Some(h) = std::env::var_os("HOME")
    {
        let h = h.to_string_lossy();
        let h = h.trim_end_matches('/');
        if !h.is_empty() {
            if p == h {
                return "~".into();
            }
            if let Some(rest) = p.strip_prefix(&format!("{h}/")) {
                return format!("~/{rest}");
            }
        }
    }
    p.to_string()
}

/// The overlay's summary of what arrived.
fn summary_lines(app: &App, a: &Accept, out: &mut Vec<(String, Style)>) {
    let t = app.theme;
    let Some(r) = &a.rec else {
        out.push(("loading…".into(), t.dim()));
        return;
    };
    out.push((format!("from     {}", r.from_label()), t.text()));
    let origin = r
        .origin
        .as_deref()
        .map(|o| format!(" · {o}"))
        .unwrap_or_default();
    out.push((format!("repo     {}{origin}", r.repo_name), t.text()));
    let head = r.head.get(..7).unwrap_or(r.head.as_str());
    let mut facts = format!("branch   {} @ {head}", r.branch());
    if r.untracked > 0 {
        facts.push_str(&format!(" · {} untracked file(s)", r.untracked));
    }
    if r.size > 0 {
        facts.push_str(&format!(" · {}", human(r.size)));
    }
    out.push((facts, t.text()));
    let agent = match (r.agent(), r.harness.as_deref()) {
        (Some(h), _) if r.resumable() => format!("{h} · the conversation resumes"),
        (Some(h), _) => format!("{h} · starts fresh with a handoff note (no transcript)"),
        (None, Some(h)) => format!("{h} · not started here (unknown harness)"),
        (None, None) => "none".into(),
    };
    out.push((format!("agent    {agent}"), t.text()));
    let secrets = r.secrets();
    if !secrets.is_empty() {
        let list: Vec<&str> = secrets.iter().take(4).copied().collect();
        let more = if secrets.len() > 4 {
            format!(" (+{})", secrets.len() - 4)
        } else {
            String::new()
        };
        out.push((
            format!("bring your own: {}{more}", list.join(", ")),
            t.s(t.yellow),
        ));
    }
    let others = r.skipped.len() - secrets.len();
    if others > 0 {
        out.push((
            format!("not carried: {others} other file(s) (too large, links, unsafe paths)"),
            t.dim(),
        ));
    }
    if r.redactions > 0 {
        out.push((
            format!("{} redaction(s) in the conversation", r.redactions),
            t.dim(),
        ));
    }
    if let Some(m) = &r.last_message {
        let one = m.split_whitespace().collect::<Vec<_>>().join(" ");
        out.push((format!("last     “{}”", truncate(&one, 74)), t.dim()));
    }
    if r.state == "failed"
        && let Some(e) = r.error_message()
    {
        out.push((format!("last try failed: {e}"), t.s(t.red)));
    }
}

/// Draw the overlay; the cursor position when a picker is open.
pub fn draw_accept(app: &App, g: &mut Grid) -> Option<(u16, u16)> {
    let a = app.ux.handoff.accept.as_ref()?;
    let t = app.theme;
    let mut lines: Vec<(String, Style)> = Vec::new();
    summary_lines(app, a, &mut lines);
    lines.push((String::new(), t.text()));
    let focus_st = Style {
        attrs: attr::BOLD,
        ..t.sel(t.fg)
    };
    if let Some(d) = &a.done {
        lines.push((d.note.clone(), t.s(t.green)));
        lines.push((d.problem.clone(), t.s(t.red)));
        lines.push((String::new(), t.text()));
        lines.push((
            "[r] retry resume  [o/enter] open the pane  [esc] close".into(),
            t.dim(),
        ));
    } else if !a.loading {
        let mark = |r: Row| if a.focus == r { "›" } else { " " };
        let st = |r: Row| if a.focus == r { focus_st } else { t.text() };
        let radio = |s: RepoSel| if a.repo == s { "(•)" } else { "( )" };
        lines.push(("Repository".into(), t.bold(t.fg)));
        for (i, p) in a.repos.iter().enumerate() {
            let row = Row::Repo(i);
            let tag = if Some(p) == a.suggested_repo.as_ref() && a.repos.len() > 1 {
                "  (suggested)"
            } else {
                ""
            };
            lines.push((
                format!(
                    "{} {} {}{tag}",
                    mark(row),
                    radio(RepoSel::Suggested(i)),
                    shown(app, a.mi, p)
                ),
                st(row),
            ));
        }
        let browse = a
            .browse_path
            .as_deref()
            .map(|p| shown(app, a.mi, p))
            .unwrap_or_else(|| "another clone on this host".into());
        lines.push((
            format!(
                "{} {} Browse…    {browse}",
                mark(Row::Browse),
                radio(RepoSel::Browse)
            ),
            st(Row::Browse),
        ));
        if a.origin().is_some() {
            lines.push((
                format!(
                    "{} {} Clone to…  {}",
                    mark(Row::CloneTo),
                    radio(RepoSel::CloneTo),
                    shown(app, a.mi, &a.clone_to)
                ),
                st(Row::CloneTo),
            ));
        }
        lines.push((String::new(), t.text()));
        let wt = a
            .worktree
            .as_deref()
            .map(|w| shown(app, a.mi, w))
            .unwrap_or_else(|| "(next to the repository)".into());
        lines.push((
            format!("{} Worktree   {wt}", mark(Row::Worktree)),
            st(Row::Worktree),
        ));
        let caret = if a.focus == Row::Branch { "▏" } else { "" };
        lines.push((
            format!("{} Branch     {}{caret}", mark(Row::Branch), a.branch),
            st(Row::Branch),
        ));
        let check = |on: bool| if on { "[x]" } else { "[ ]" };
        if a.has_agent() {
            let what = match &a.rec {
                Some(r) if r.resumable() => format!("Resume agent ({})", r.agent_name()),
                Some(r) => format!("Start agent ({}, fresh session)", r.agent_name()),
                None => "Resume agent".into(),
            };
            lines.push((
                format!("{} {} {what}", mark(Row::Resume), check(a.resume)),
                st(Row::Resume),
            ));
        }
        lines.push((
            format!(
                "{} {} Trust mise config (mise trust, if the worktree has one)",
                mark(Row::TrustMise),
                check(a.trust_mise)
            ),
            st(Row::TrustMise),
        ));
        lines.push((
            format!(
                "{} {} Trust direnv config (direnv allow, if the worktree has one)",
                mark(Row::TrustDirenv),
                check(a.trust_direnv)
            ),
            st(Row::TrustDirenv),
        ));
        lines.push((String::new(), t.text()));
        let btn = |r: Row, s: &str| {
            if a.focus == r {
                format!("[> {s} <]")
            } else {
                format!("[ {s} ]")
            }
        };
        let any_button = matches!(a.focus, Row::Accept | Row::Decline | Row::Later);
        lines.push((
            format!(
                "  {}  {}  {}",
                btn(Row::Accept, "Accept"),
                btn(Row::Decline, "Decline"),
                btn(Row::Later, "Later")
            ),
            if any_button {
                t.bold(t.accent)
            } else {
                t.text()
            },
        ));
        if let Some(b) = &a.busy {
            lines.push((format!("⏳ {b}"), t.s(t.yellow)));
        }
        if a.confirm_decline {
            lines.push((
                "Decline this handoff? Its bundle is deleted. [d/enter] decline  [other] keep"
                    .into(),
                t.bold(t.red),
            ));
        }
        if let Some(e) = &a.error {
            lines.push((format!("✗ {e}"), t.s(t.red)));
        }
        if let Some(m) = &a.mismatch {
            lines.push((
                format!(
                    "  {} has no remote {}; its remotes:",
                    shown(app, a.mi, &m.repo),
                    m.origin
                ),
                t.s(t.red),
            ));
            if m.remotes.is_empty() {
                lines.push(("    (none)".into(), t.dim()));
            }
            for r in m.remotes.iter().take(5) {
                lines.push((format!("    {r}"), t.dim()));
            }
        }
        lines.push((
            "↑↓/tab move · space choose · enter open/toggle · a accept · d decline · esc later"
                .into(),
            t.dim(),
        ));
    } else if let Some(e) = &a.error {
        lines.push((format!("✗ {e}"), t.s(t.red)));
    }
    let title = match &a.rec {
        Some(r) => format!("incoming handoff · {} · {}", r.from_host, r.branch()),
        None => "incoming handoff".into(),
    };
    {
        let h = (lines.len() + 2).min(48) as u16;
        let mut b = frame(app, g, 92, h, &title);
        for (s, st) in &lines {
            b.line(s, *st);
        }
    }
    let (what, p) = a.picker.as_ref()?;
    let title = match what {
        PickFor::Browse => "a clone of the repository",
        PickFor::CloneTo => "clone into (a new or empty folder)",
        PickFor::Worktree => "worktree path (a new folder)",
    };
    Some(p.draw(
        app,
        g,
        &format!("{title} · tab completes · ↑↓ choose · → into · enter"),
    ))
}

impl Rec {
    fn agent_name(&self) -> &str {
        self.harness.as_deref().unwrap_or("agent")
    }
}

// ---- replies --------------------------------------------------------------------------------------

pub fn on_reply(app: &mut App, mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    match r {
        Reply::List => match res {
            Ok(v) => {
                let list: Vec<Rec> = v["incoming"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Rec::from_value).collect())
                    .unwrap_or_default();
                // A reconnect lost the answer to a running accept: the list says how it ended.
                if let Some(a) = app.ux.handoff.accept.as_mut()
                    && a.mi == mi
                    && a.busy.is_some()
                    && let Some(r) = list.iter().find(|r| r.id == a.id)
                    && matches!(r.state.as_str(), "imported" | "failed" | "declined")
                {
                    a.busy = None;
                    a.rec = Some(r.clone());
                    if r.state != "imported" {
                        a.error = Some(
                            r.error_message()
                                .unwrap_or_else(|| format!("the handoff is {}", r.state)),
                        );
                    } else {
                        a.done = Some(Done {
                            pane: r.pane(),
                            note: format!("⇣ imported {}", r.branch()),
                            problem: r.agent_error().unwrap_or_else(|| {
                                "the connection dropped while it was imported".into()
                            }),
                        });
                    }
                }
                app.ux.handoff.incoming.insert(mi, list);
                crate::inbox::invalidate(app);
            }
            Err(e) if e.is_method_not_found() => {
                app.ux.handoff.unsupported.insert(mi);
                app.ux.handoff.incoming.remove(&mi);
            }
            Err(_) => {}
        },
        Reply::Jobs => {
            // Before gateway-to-gateway sending a server has no jobs: nothing to show.
            if let Ok(v) = res {
                let jobs: Vec<Job> = v["jobs"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Job::from_value).collect())
                    .unwrap_or_default();
                app.ux.handoff.jobs.insert(mi, jobs);
            }
        }
        Reply::Get { id } => {
            let open = app
                .ux
                .handoff
                .accept
                .as_ref()
                .is_some_and(|a| a.mi == mi && a.id == id);
            match res {
                Ok(v) => {
                    if let Some(r) = Rec::from_value(&v["incoming"]) {
                        upsert(app, mi, r);
                    }
                    if open && let Some(a) = app.ux.handoff.accept.as_mut() {
                        a.load(&v);
                        match a.rec.as_ref().map(|r| r.state.as_str()) {
                            Some("imported") => {
                                let r = a.rec.clone().unwrap_or_default();
                                a.done = Some(Done {
                                    pane: r.pane(),
                                    note: format!("⇣ already imported: {}", r.branch()),
                                    problem: r.agent_error().map_or_else(
                                        || "nothing left to accept".into(),
                                        |e| format!("the agent did not start: {e}"),
                                    ),
                                });
                            }
                            Some("declined") => a.error = Some("this handoff was declined".into()),
                            Some("importing") => a.busy = Some("importing…".into()),
                            _ => {}
                        }
                    }
                }
                Err(e) => {
                    if open && let Some(a) = app.ux.handoff.accept.as_mut() {
                        a.loading = false;
                        a.error = Some(if e.kind == "not_found" {
                            "this handoff is gone (expired or removed)".into()
                        } else {
                            e.message
                        });
                    }
                }
            }
        }
        Reply::Accept { id } => on_accepted(app, mi, &id, res),
        Reply::Decline { id } => {
            let open = app
                .ux
                .handoff
                .accept
                .as_ref()
                .is_some_and(|a| a.mi == mi && a.id == id);
            match res {
                Ok(v) => {
                    let from = Rec::from_value(&v["incoming"]).map(|r| {
                        let host = r.from_host.clone();
                        upsert(app, mi, r);
                        host
                    });
                    if open {
                        close(app);
                    }
                    app.toast(format!(
                        "declined the handoff from {}",
                        from.unwrap_or_else(|| "the sender".into())
                    ));
                }
                Err(e) => {
                    if open && let Some(a) = app.ux.handoff.accept.as_mut() {
                        a.error = Some(e.message);
                    } else {
                        app.toast(format!("✗ {}", e.message));
                    }
                }
            }
        }
        Reply::Resume { id } => {
            let open = app
                .ux
                .handoff
                .accept
                .as_ref()
                .is_some_and(|a| a.mi == mi && a.id == id && a.done.is_some());
            match res {
                Ok(v) => {
                    let rec = Rec::from_value(&v["incoming"]);
                    let pane = rec.as_ref().and_then(Rec::pane);
                    if let Some(r) = rec {
                        upsert(app, mi, r);
                    }
                    match v.get("agent_error").filter(|e| !e.is_null()) {
                        Some(e) => {
                            let msg = format!("the agent did not start: {}", message_of(e));
                            set_problem(app, open, msg);
                        }
                        None => {
                            app.toast("▶ agent resumed");
                            if open {
                                focus_imported(app, mi, pane);
                            }
                        }
                    }
                }
                Err(e) => set_problem(app, open, e.message),
            }
        }
        Reply::Peers => {
            if let Some(f) = app.ux.handoff.send.as_mut()
                && f.mi == mi
            {
                f.loading = false;
                match res {
                    Ok(v) => {
                        f.peers = v["peers"]
                            .as_array()
                            .map(|a| a.iter().filter_map(Peer::from_value).collect())
                            .unwrap_or_default();
                    }
                    Err(e) if e.is_method_not_found() => {
                        f.error = Some(
                            "this machine's vibeke can't send handoffs from the terminal yet (update it)"
                                .into(),
                        );
                    }
                    Err(e) => f.error = Some(e.message),
                }
            }
        }
        Reply::Send { op } => {
            // Only the form that sent it: a reply after Esc and a new form leaves that one be.
            let current = app
                .ux
                .handoff
                .send
                .as_ref()
                .is_some_and(|f| f.send_op == Some(op));
            match res {
                Ok(v) => {
                    let name = app
                        .ux
                        .handoff
                        .send
                        .as_ref()
                        .filter(|_| current)
                        .and_then(|f| f.chosen.as_ref())
                        .map(|d| d.name().to_string());
                    if let Some(j) = Job::from_value(&v["job"]) {
                        let who = name.unwrap_or_else(|| j.name().to_string());
                        upsert_job(app, mi, j);
                        app.toast(format!("⇢ handing off to {who}…"));
                    }
                    if current {
                        app.ux.handoff.send = None;
                        if matches!(app.mode, Mode::Popup(Popup::HandoffSend)) {
                            app.mode = Mode::Normal;
                        }
                    }
                }
                Err(e) => {
                    if let Some(f) = app.ux.handoff.send.as_mut().filter(|_| current) {
                        f.send_op = None;
                        f.busy = false;
                        f.pairing = None;
                        f.pair_op = None;
                        f.error = Some(e.message);
                    } else {
                        app.toast(format!("✗ {}", e.message));
                    }
                }
            }
        }
        Reply::Cancel => match res {
            Ok(v) => {
                if let Some(j) = Job::from_value(v.get("job").unwrap_or(&v)) {
                    upsert_job(app, mi, j);
                } else {
                    app.toast("handoff cancelled");
                }
            }
            Err(e) => app.toast(format!("✗ {}", e.message)),
        },
        Reply::PairInvite { op } => on_pair_invite(app, mi, op, res),
        Reply::PairList {
            op,
            link,
            pid,
            host,
        } => on_pair_list(app, op, link, pid, host, res),
        Reply::PairRedeem { op } => on_pair_redeem(app, op, res),
    }
    app.dirty = true;
}

/// A retried resume's outcome: in the overlay's imported view when it is open, else a toast.
fn set_problem(app: &mut App, open: bool, msg: String) {
    if open && let Some(d) = app.ux.handoff.accept.as_mut().and_then(|a| a.done.as_mut()) {
        d.problem = msg;
    } else {
        app.toast(format!("✗ {msg}"));
    }
}

fn on_accepted(app: &mut App, mi: usize, id: &str, res: Result<Value, RpcErr>) {
    let open = matches!(app.mode, Mode::Popup(Popup::HandoffAccept))
        && app
            .ux
            .handoff
            .accept
            .as_ref()
            .is_some_and(|a| a.mi == mi && a.id == id);
    match res {
        Ok(v) => {
            let Some(r) = Rec::from_value(&v["incoming"]) else {
                return;
            };
            upsert(app, mi, r.clone());
            let out = r.result.clone().unwrap_or_default();
            let wt = out["worktree"].as_str().unwrap_or_default();
            let mut note = format!(
                "⇣ imported {} into {}",
                out["branch"].as_str().unwrap_or(r.branch()),
                shown(app, mi, wt)
            );
            let not_written = out["not_written"].as_array().map_or(0, Vec::len);
            if not_written > 0 {
                note.push_str(&format!(" · {not_written} file(s) not written"));
            }
            let trust_failed = out["trust"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|x| matches!(x["status"].as_str(), Some("failed" | "not_installed")))
                .count();
            if trust_failed > 0 {
                note.push_str(" · trust step failed");
            }
            let problem = r
                .agent_error()
                .map(|e| format!("the agent did not start: {e}"))
                .or_else(|| {
                    out["workspace_error"]
                        .as_str()
                        .map(|e| format!("no workspace was opened: {e}"))
                });
            match (open, problem) {
                (true, Some(problem)) => {
                    if let Some(a) = app.ux.handoff.accept.as_mut() {
                        a.busy = None;
                        a.done = Some(Done {
                            pane: r.pane(),
                            note,
                            problem,
                        });
                    }
                }
                (true, None) => {
                    focus_imported(app, mi, r.pane());
                    app.toast(note);
                }
                (false, problem) => app.toast(match problem {
                    Some(p) => format!("{note} — {p}"),
                    None => note,
                }),
            }
        }
        Err(e) => {
            if open && let Some(a) = app.ux.handoff.accept.as_mut() {
                a.busy = None;
                a.mismatch = (e.reason() == Some("repo_mismatch"))
                    .then(|| Mismatch::from_details(&e.details));
                a.error = Some(if a.mismatch.is_some() {
                    "that clone is a different repository; choose another or clone it".into()
                } else {
                    e.message
                });
            } else {
                app.toast(format!("✗ handoff not imported: {}", e.message));
            }
        }
    }
}

// ---- handoffs list ---------------------------------------------------------------------------------

/// One row of the handoffs list.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    In(usize, Box<Rec>),
    Out(usize, Job),
}

/// Incoming handoffs (not declined), then sends, newest first within each.
pub fn entries(app: &App) -> Vec<Entry> {
    let now = now_ms();
    let mut v = Vec::new();
    for (&mi, list) in &app.ux.handoff.incoming {
        for r in list
            .iter()
            .filter(|r| r.state != "declined" && (r.expires_at_ms == 0 || r.expires_at_ms > now))
        {
            v.push(Entry::In(mi, Box::new(r.clone())));
        }
    }
    for (&mi, list) in &app.ux.handoff.jobs {
        for j in list {
            v.push(Entry::Out(mi, j.clone()));
        }
    }
    v
}

pub fn open_list(app: &mut App) {
    for mi in 0..app.machines.len() {
        refresh(app, mi);
    }
    app.ux.handoff.list_confirm = None;
    app.mode = Mode::Popup(Popup::Handoffs);
}

/// Leaving the tab for another.
pub(crate) fn leave(app: &mut App) {
    app.ux.handoff.list_confirm = None;
}

pub fn handoffs_key(app: &mut App, ev: KeyEvent) {
    app.mode = Mode::Popup(Popup::Handoffs);
    if ev.kind == KeyKind::Release {
        return;
    }
    let list = entries(app);
    let n = list.len();
    let sel = app.ux.handoff.list_sel.min(n.saturating_sub(1));
    app.ux.handoff.list_sel = sel;
    let confirm = app.ux.handoff.list_confirm.take();
    let plain = !ev.mods.ctrl() && !ev.mods.alt();
    let cur = list.get(sel).cloned();
    match ev.key {
        Key::Named(NamedKey::Escape) | Key::Char('q') => app.mode = Mode::Normal,
        Key::Char('j') | Key::Named(NamedKey::Down) => {
            app.ux.handoff.list_sel = (sel + 1).min(n.saturating_sub(1));
        }
        Key::Char('k') | Key::Named(NamedKey::Up) => {
            app.ux.handoff.list_sel = sel.saturating_sub(1);
        }
        Key::Named(NamedKey::Enter) => match cur {
            Some(Entry::In(mi, r)) if r.waiting() => {
                open_accept(app, mi, &r.id);
                if let Some(a) = app.ux.handoff.accept.as_mut() {
                    a.from_list = true;
                }
            }
            Some(Entry::In(mi, r)) if r.state == "imported" => match r.pane() {
                Some(p) => {
                    app.mode = Mode::Normal;
                    app.focus_pane(mi, &p);
                }
                None => app.toast("the import has no pane"),
            },
            Some(Entry::In(_, r)) => app.toast(format!("this handoff is {}", r.state)),
            Some(Entry::Out(_, j)) => app.toast(job_status(&j)),
            None => {}
        },
        Key::Char('d') if plain => match cur {
            Some(Entry::In(mi, r)) if r.waiting() => {
                if confirm.as_deref() == Some(r.id.as_str()) {
                    decline(app, mi, &r.id);
                } else {
                    app.ux.handoff.list_confirm = Some(r.id);
                }
            }
            _ => app.toast("only a waiting handoff can be declined"),
        },
        Key::Char('r') if plain => match cur {
            Some(Entry::In(mi, r)) if r.state == "imported" && r.agent().is_some() => {
                resume(app, mi, &r.id)
            }
            _ => app.toast("retry resume applies to an imported handoff with an agent"),
        },
        Key::Char('x') if plain => match cur {
            Some(Entry::Out(mi, j)) if j.active() => app.command_on(
                mi,
                "handoff.cancel",
                json!({"id": j.id}),
                pend(Reply::Cancel),
            ),
            _ => app.toast("only a send in progress can be cancelled"),
        },
        Key::Char('g') if plain => open_list(app),
        _ => {}
    }
    app.dirty = true;
}

/// One line of the handoffs list: (text, note).
pub fn entry_line(app: &App, e: &Entry, now: i64) -> (String, String) {
    let multi = app.machines.len() > 1;
    let on = |mi: usize| {
        if multi {
            format!(
                "{} · ",
                app.machines.get(mi).map_or("", |m| m.label.as_str())
            )
        } else {
            String::new()
        }
    };
    match e {
        Entry::In(mi, r) => {
            let state = match r.state.as_str() {
                "imported" if r.agent_error().is_some() => {
                    "imported · agent did not start (r retries)".to_string()
                }
                "failed" => format!(
                    "failed: {}",
                    truncate(&r.error_message().unwrap_or_default(), 40)
                ),
                s => s.to_string(),
            };
            (
                format!(
                    "⇣ {}from {} · {} · {} · {state}",
                    on(*mi),
                    r.from_host,
                    r.repo_name,
                    r.branch()
                ),
                fmt_age((now - r.created_at_ms).max(0)),
            )
        }
        Entry::Out(mi, j) => {
            let pane = app
                .machines
                .get(*mi)
                .and_then(|m| m.model.panes.iter().find(|p| p.id == j.pane))
                .map(|p| p.handle.clone())
                .unwrap_or_else(|| j.pane.clone());
            (
                format!("{}{} · pane {pane}", on(*mi), job_status(j)),
                if j.active() {
                    "x cancels".into()
                } else {
                    String::new()
                },
            )
        }
    }
}

/// The Handoffs tab: every machine's incoming handoffs and sends (the title names the machine
/// the Connections view was opened on).
pub fn draw_list(app: &App, g: &mut Grid) {
    let t = app.theme;
    let list = entries(app);
    let now = now_ms();
    let sel = app.ux.handoff.list_sel.min(list.len().saturating_sub(1));
    let mi = app.ux.connections.mi;
    let mut a = crate::connections::area(app, g, crate::connections::Tab::Handoffs, mi);
    a.line("Handoffs — incoming and sent", t.bold(t.fg));
    a.line("", t.text());
    let w = a.rest().w.saturating_sub(1) as usize;
    if list.is_empty() {
        a.line("  no incoming handoffs and nothing being sent", t.dim());
    }
    for (i, e) in list.iter().enumerate() {
        let (s, note) = entry_line(app, e, now);
        let note_w = UnicodeWidthStr::width(note.as_str());
        let mark = if i == sel { "›" } else { " " };
        let s = format!("{mark} {}", truncate(&s, w.saturating_sub(note_w + 4)));
        let pad = w.saturating_sub(UnicodeWidthStr::width(s.as_str()) + note_w);
        let line = format!("{s}{}{note}", " ".repeat(pad));
        a.line(&line, if i == sel { t.sel(t.fg) } else { t.text() });
    }
    a.line("", t.text());
    if app.ux.handoff.list_confirm.is_some() {
        a.line(
            "Decline the selected handoff? Its bundle is deleted. [d] decline  [other] keep",
            t.bold(t.red),
        );
    }
    a.footer(
        "j/k move · enter accept/open · d decline · r retry resume · x cancel send · g refresh · esc",
        t.dim(),
    );
}

// ---- send ---------------------------------------------------------------------------------------

/// Where a pane's work can go.
#[derive(Debug, Clone, PartialEq)]
pub enum Dest {
    /// A host the source is paired with (`handoff.peers`).
    Peer(Peer),
    /// One of the user's machines this TUI is attached to that is not a peer of the source yet:
    /// choosing it pairs the two first.
    Machine {
        mi: usize,
        name: String,
        online: bool,
    },
}

impl Dest {
    pub fn name(&self) -> &str {
        match self {
            Dest::Peer(p) => &p.name,
            Dest::Machine { name, .. } => name,
        }
    }

    fn note(&self, now: i64) -> String {
        match self {
            Dest::Peer(p) => p.note(now),
            Dest::Machine { online: true, .. } => "your machine · will pair".into(),
            Dest::Machine { online: false, .. } => "your machine · offline".into(),
        }
    }

    fn search_text(&self) -> String {
        match self {
            Dest::Peer(p) => format!("{} {} {}", p.name, p.owner, p.id),
            Dest::Machine { name, .. } => format!("{name} your machine will pair"),
        }
    }
}

/// What choosing a destination does (the decision rules of the app's `planSend`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Send to this peer.
    Send(String),
    /// Pair first: `peer.invite` on this machine, `peer.redeem` on the source, then send.
    Pair(usize),
    /// `expired` or `offline`.
    Unavailable(&'static str),
}

pub fn plan_send(d: &Dest, now: i64) -> Plan {
    match d {
        Dest::Peer(p) if p.is_expired(now) => Plan::Unavailable("expired"),
        Dest::Peer(p) => Plan::Send(p.id.clone()),
        Dest::Machine {
            mi, online: true, ..
        } => Plan::Pair(*mi),
        Dest::Machine { .. } => Plan::Unavailable("offline"),
    }
}

/// Two names of the same host: equal ignoring case and any domain (`mini` = `Mini.local`).
pub fn same_host(a: &str, b: &str) -> bool {
    let short = |s: &str| {
        s.trim()
            .split('.')
            .next()
            .unwrap_or_default()
            .to_lowercase()
    };
    let (a, b) = (short(a), short(b));
    !a.is_empty() && a == b
}

/// The machines this TUI is attached to, other than the source, by name.
fn other_machines(app: &App, source: usize) -> Vec<Dest> {
    let mut v: Vec<Dest> = app
        .machines
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != source)
        .map(|(i, m)| Dest::Machine {
            mi: i,
            name: m.label.clone(),
            online: m.connected(),
        })
        .collect();
    v.sort_by(|a, b| a.name().cmp(b.name()));
    v
}

pub struct SendForm {
    pub mi: usize,
    pub pane: String,
    pub peers: Vec<Peer>,
    /// The other machines this TUI is attached to (listed when no peer has their name).
    pub machines: Vec<Dest>,
    pub loading: bool,
    pub filter: String,
    pub sel: usize,
    /// The chosen destination: the summary step.
    pub chosen: Option<Dest>,
    pub interrupt: bool,
    /// `handoff.send` (or the pairing before it) in flight.
    pub busy: bool,
    /// Auto-pairing step in progress, as the summary shows it.
    pub pairing: Option<String>,
    /// The auto-pairing in flight: its replies are acted on only while this is it.
    pub pair_op: Option<PairOp>,
    /// The `handoff.send` in flight for this form.
    pub send_op: Option<u64>,
    pub error: Option<String>,
}

/// One auto-pairing run for a send, frozen when it starts: a reply that arrives after Esc (or
/// for another form) carries a different one and is ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairOp {
    /// Unique per run.
    pub id: u64,
    /// The source machine and the pane being handed off.
    pub src: usize,
    pub pane: String,
    /// The destination machine (where the invitation is made) and its name.
    pub dest_mi: usize,
    pub dest_name: String,
}

static NEXT_PAIR_OP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl SendForm {
    /// The source's peers (server order: own hosts first), then the user's machines that are not
    /// peers yet.
    pub fn dests(&self) -> Vec<Dest> {
        let mut v: Vec<Dest> = self.peers.iter().cloned().map(Dest::Peer).collect();
        v.extend(
            self.machines
                .iter()
                // Listing only: a teammate's host of the same name never hides your machine,
                // and the destination is resolved by host id when pairing.
                .filter(|d| {
                    !self
                        .peers
                        .iter()
                        .any(|p| p.owner == "self" && same_host(&p.name, d.name()))
                })
                .cloned(),
        );
        v
    }

    /// Destinations matching the filter, best first (peers before machines to pair), with
    /// highlight positions in the name.
    pub fn ranked(&self) -> Vec<(Dest, Vec<usize>)> {
        let mut v: Vec<(bool, i32, usize, Dest, Vec<usize>)> = self
            .dests()
            .into_iter()
            .enumerate()
            .filter_map(|(i, d)| {
                let m = fuzzy(&self.filter, &d.search_text())?;
                let n = d.name().chars().count();
                let pos = m.positions.into_iter().filter(|x| *x < n).collect();
                Some((matches!(d, Dest::Machine { .. }), m.score, i, d, pos))
            })
            .collect();
        if !self.filter.is_empty() {
            v.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
        }
        v.into_iter().map(|(_, _, _, d, pos)| (d, pos)).collect()
    }
}

/// `handoff.send` params for the chosen peer.
pub fn send_params(pane: &str, peer: &Peer, interrupt: bool) -> Value {
    json!({"pane": pane, "peer": peer.id, "interrupt": interrupt})
}

/// `handoff_send`: hand the focused (or right-clicked) pane off to a paired host, or to one of
/// the user's other machines (paired first).
pub fn open_send(app: &mut App) {
    let Some((mi, pane)) = take_target(app) else {
        app.toast("no focused pane to hand off");
        return;
    };
    if !app.machines[mi].connected() {
        app.toast(format!("{} offline — nothing sent", app.machines[mi].label));
        return;
    }
    let machines = other_machines(app, mi);
    app.ux.handoff.send = Some(SendForm {
        mi,
        pane,
        peers: Vec::new(),
        machines,
        loading: true,
        filter: String::new(),
        sel: 0,
        chosen: None,
        interrupt: false,
        busy: false,
        pairing: None,
        pair_op: None,
        send_op: None,
        error: None,
    });
    app.mode = Mode::Popup(Popup::HandoffSend);
    app.command_on(mi, "handoff.peers", json!({}), pend(Reply::Peers));
}

/// `handoff.send` for the form's pane to peer `peer`.
fn start_send(app: &mut App, peer: &str) {
    let Some(f) = app.ux.handoff.send.as_mut() else {
        return;
    };
    f.busy = true;
    f.error = None;
    f.pairing = None;
    f.pair_op = None;
    let op = NEXT_PAIR_OP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    f.send_op = Some(op);
    let p = json!({"pane": f.pane, "peer": peer, "interrupt": f.interrupt});
    let mi = f.mi;
    app.command_on(mi, "handoff.send", p, pend(Reply::Send { op }));
}

/// The chosen destination, by the `planSend` rules.
fn submit_send(app: &mut App) {
    let Some(f) = app.ux.handoff.send.as_mut() else {
        return;
    };
    let Some(dest) = f.chosen.clone() else {
        return;
    };
    match plan_send(&dest, now_ms()) {
        Plan::Send(peer) => start_send(app, &peer),
        Plan::Pair(dmi) => {
            let source = app
                .machines
                .get(f.mi)
                .map(|m| m.label.clone())
                .unwrap_or_default();
            let op = PairOp {
                id: NEXT_PAIR_OP.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                src: f.mi,
                pane: f.pane.clone(),
                dest_mi: dmi,
                dest_name: dest.name().to_string(),
            };
            f.busy = true;
            f.error = None;
            f.pairing = Some(format!(
                "pairing {} with {source}: creating an invitation on {}…",
                dest.name(),
                dest.name()
            ));
            f.pair_op = Some(op.clone());
            crate::handoff::call_long_pending(
                app,
                dmi,
                "gateway.call",
                json!({"method": "peer.invite", "params": {}}),
                pend(Reply::PairInvite { op }),
            );
        }
        Plan::Unavailable(why) => {
            f.error = Some(if why == "expired" {
                format!("the pairing with {} has expired", dest.name())
            } else {
                format!("{} is offline — reconnect it to pair", dest.name())
            });
        }
    }
}

/// Auto-pairing failed: the summary says where.
fn pair_failed(app: &mut App, msg: String) {
    if let Some(f) = app.ux.handoff.send.as_mut() {
        f.busy = false;
        f.pairing = None;
        f.pair_op = None;
        f.error = Some(msg);
    } else {
        app.toast(format!("✗ {msg}"));
    }
}

/// Whether `op` is the auto-pairing the open send form is waiting on (same run, same source
/// pane): anything else is a late reply for a form that is gone.
fn is_current(app: &App, op: &PairOp) -> bool {
    app.ux.handoff.send.as_ref().is_some_and(|f| {
        f.busy && f.pair_op.as_ref() == Some(op) && f.mi == op.src && f.pane == op.pane
    })
}

/// An invitation made for a pairing that is not used: cancel it on the machine that made it.
fn revoke_invitation(app: &mut App, dmi: usize, pid: Option<&str>) {
    if let Some(pid) = pid.filter(|p| !p.is_empty())
        && app.machines.get(dmi).is_some_and(|m| m.connected())
    {
        call_long_pending(
            app,
            dmi,
            "gateway.call",
            json!({"method": "share.revoke", "params": {"id": pid}}),
            Pending::Ignore,
        );
    }
}

/// `peer.invite` answered on the destination machine `dmi`: ask the source whether it is
/// already paired with that host (by host id, never by name), else redeem it there.
fn on_pair_invite(app: &mut App, dmi: usize, op: PairOp, res: Result<Value, RpcErr>) {
    let pid = res
        .as_ref()
        .ok()
        .and_then(|v| v["pid"].as_str())
        .map(str::to_string);
    if !is_current(app, &op) || dmi != op.dest_mi {
        // Esc (or another form) since: the invitation is not going to be used.
        return revoke_invitation(app, dmi, pid.as_deref());
    }
    let name = op.dest_name.clone();
    let v = match res {
        Ok(v) => v,
        Err(e) => {
            let why = crate::sharing::bridge_error(&e);
            return pair_failed(app, format!("pairing failed on {name}: {why}"));
        }
    };
    let Some(link) = v["link"]
        .as_str()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
    else {
        return pair_failed(
            app,
            format!("pairing failed: {name} returned no invitation"),
        );
    };
    let host = crate::sharing::parse_link(&link)
        .ok()
        .map(|inv| inv.host)
        .filter(|h| !h.is_empty());
    let Some(host) = host else {
        // No host id to match on: pair (the gateway checks the link).
        return redeem(app, op, link);
    };
    if let Some(f) = app.ux.handoff.send.as_mut() {
        f.pairing = Some(format!("pairing with {name}: checking the peers here…"));
    }
    let src = op.src;
    call_long_pending(
        app,
        src,
        "gateway.call",
        json!({"method": "peer.list", "params": {}}),
        pend(Reply::PairList {
            op,
            link,
            pid,
            host,
        }),
    );
}

/// `peer.list` answered on the source: an own, unexpired peer with the invitation's host id is
/// the destination (the invitation is revoked); otherwise the invitation is redeemed.
fn on_pair_list(
    app: &mut App,
    op: PairOp,
    link: String,
    pid: Option<String>,
    host: String,
    res: Result<Value, RpcErr>,
) {
    if !is_current(app, &op) {
        return revoke_invitation(app, op.dest_mi, pid.as_deref());
    }
    let now = now_ms();
    let known = res.ok().and_then(|v| {
        v["peers"].as_array().and_then(|a| {
            a.iter()
                .filter(|x| x["host"].as_str() == Some(host.as_str()))
                .filter_map(Peer::from_value)
                .find(|p| p.owner == "self" && !p.is_expired(now))
        })
    });
    let Some(p) = known else {
        return redeem(app, op, link);
    };
    // Already paired with that very host: the unused invitation goes, the work goes there.
    revoke_invitation(app, op.dest_mi, pid.as_deref());
    if let Some(f) = app.ux.handoff.send.as_mut() {
        f.chosen = Some(Dest::Peer(p.clone()));
    }
    start_send(app, &p.id);
}

/// Accept the invitation on the source.
fn redeem(app: &mut App, op: PairOp, link: String) {
    if let Some(f) = app.ux.handoff.send.as_mut() {
        f.pairing = Some(format!(
            "pairing with {}: accepting the invitation here…",
            op.dest_name
        ));
    }
    let src = op.src;
    call_long_pending(
        app,
        src,
        "gateway.call",
        json!({"method": "peer.redeem", "params": {"link": link, "share_user": false},
               "timeout_ms": crate::sharing::REDEEM_TIMEOUT_MS}),
        pend(Reply::PairRedeem { op }),
    );
}

/// `peer.redeem` answered on the source: the new peer is the destination.
fn on_pair_redeem(app: &mut App, op: PairOp, res: Result<Value, RpcErr>) {
    if !is_current(app, &op) {
        // The pairing (if it was made) stays; nothing is sent for a form that is gone.
        return;
    }
    let v = match res {
        Ok(v) => v,
        Err(e) => {
            let why = crate::sharing::bridge_error(&e);
            return pair_failed(app, format!("pairing failed: {why}"));
        }
    };
    let Some(p) = Peer::from_value(&v["peer"]) else {
        return pair_failed(app, "pairing failed: the gateway returned no peer".into());
    };
    if let Some(f) = app.ux.handoff.send.as_mut() {
        f.peers.retain(|x| x.id != p.id);
        f.peers.push(p.clone());
        f.chosen = Some(Dest::Peer(p.clone()));
    }
    app.toast(format!("paired with {}", p.name));
    start_send(app, &p.id);
}

pub fn send_key(app: &mut App, ev: KeyEvent) {
    app.mode = Mode::Popup(Popup::HandoffSend);
    if ev.kind == KeyKind::Release {
        return;
    }
    let Some(f) = app.ux.handoff.send.as_mut() else {
        app.mode = Mode::Normal;
        return;
    };
    let esc = matches!(ev.key, Key::Named(NamedKey::Escape));
    if f.busy {
        if esc {
            // The send may still start; its job shows up in the handoffs list.
            app.ux.handoff.send = None;
            app.mode = Mode::Normal;
        }
        return;
    }
    if f.chosen.is_some() {
        let plain = !ev.mods.ctrl() && !ev.mods.alt();
        match ev.key {
            _ if esc => {
                f.chosen = None;
                f.error = None;
            }
            Key::Char('i') if plain => f.interrupt = !f.interrupt,
            Key::Named(NamedKey::Space) => f.interrupt = !f.interrupt,
            Key::Char(' ') if plain => f.interrupt = !f.interrupt,
            Key::Named(NamedKey::Enter) => submit_send(app),
            _ => {}
        }
        app.dirty = true;
        return;
    }
    let ranked = f.ranked();
    match list_key(&ev, std::mem::take(&mut f.filter), f.sel, ranked.len()) {
        ListKey::Close => {
            app.ux.handoff.send = None;
            app.mode = Mode::Normal;
        }
        ListKey::Enter(i) | ListKey::EnterAlt(i) => {
            if let Some((d, _)) = ranked.get(i) {
                f.chosen = Some(d.clone());
                f.error = None;
            }
            f.filter = String::new();
        }
        ListKey::Stay(filter, sel) => {
            f.filter = filter;
            f.sel = sel;
        }
    }
    app.dirty = true;
}

/// "claude in api (w1:p1)" for the pane being handed off (or shared).
pub(crate) fn pane_label(app: &App, mi: usize, pane: &str) -> String {
    let Some(m) = app.machines.get(mi) else {
        return pane.to_string();
    };
    let p = m.model.panes.iter().find(|p| p.id == pane);
    let ws = p
        .and_then(|p| m.model.workspaces.iter().find(|w| w.id == p.workspace))
        .map(|w| w.display_name().to_string());
    let run = m
        .model
        .runs
        .iter()
        .find(|r| r.pane == pane && r.ended_at_ms.is_none());
    let what = run
        .map(|r| r.label().to_string())
        .unwrap_or_else(|| "pane".into());
    let handle = p.map(|p| p.handle.clone()).unwrap_or_else(|| pane.into());
    match ws {
        Some(w) => format!("{what} in {w} ({handle})"),
        None => format!("{what} ({handle})"),
    }
}

/// Draw the send flow; the filter's cursor position in the destination list.
pub fn draw_send(app: &App, g: &mut Grid) -> Option<(u16, u16)> {
    let f = app.ux.handoff.send.as_ref()?;
    let t = app.theme;
    if let Some(dest) = &f.chosen {
        let mut b = frame(app, g, 84, 14, "hand off pane");
        b.line(
            &format!(
                "Hand off {} to {}?",
                pane_label(app, f.mi, &f.pane),
                dest.name()
            ),
            t.bold(t.fg),
        );
        if let Dest::Machine { .. } = dest {
            let source = app.machines.get(f.mi).map_or("", |m| m.label.as_str());
            b.line(
                &format!(
                    "{} is one of your machines but not paired with {source} yet: Vibeke pairs",
                    dest.name()
                ),
                t.s(t.yellow),
            );
            b.line(
                &format!(
                    "them first (an invitation on {}, accepted on {source}), then hands off.",
                    dest.name()
                ),
                t.s(t.yellow),
            );
        }
        b.line(
            "After the agent's turn this host exports the work and delivers it; the receiver",
            t.dim(),
        );
        b.line(
            "chooses the repository, worktree and branch. Nothing here is closed.",
            t.dim(),
        );
        b.line("", t.text());
        b.line(
            &format!(
                "{} Interrupt agent if busy  (i toggles)",
                if f.interrupt { "[x]" } else { "[ ]" }
            ),
            t.text(),
        );
        b.line("", t.text());
        if let Some(p) = &f.pairing {
            b.line(&format!("⏳ {p}"), t.s(t.yellow));
        } else if f.busy {
            b.line("⏳ starting the handoff…", t.s(t.yellow));
        } else if let Some(e) = &f.error {
            b.line(&format!("✗ {e}"), t.s(t.red));
        }
        b.line("[enter] hand off  [esc] back", t.dim());
        return None;
    }
    let (x, y, w, rows) = list_frame(
        app,
        g,
        &format!(
            "hand off {} · to which host · enter",
            pane_label(app, f.mi, &f.pane)
        ),
        &f.filter,
    );
    let hi = Style {
        attrs: attr::BOLD | attr::UNDERLINE,
        ..t.s(t.accent)
    };
    let ranked = f.ranked();
    let now = now_ms();
    // A header row goes before the first machine to pair.
    let first_machine = ranked
        .iter()
        .position(|(d, _)| matches!(d, Dest::Machine { .. }));
    let disp = |i: usize| i + usize::from(first_machine.is_some_and(|m| i >= m));
    let rows = rows as usize;
    let skip = disp(f.sel).saturating_sub(rows.saturating_sub(1));
    let visible = |r: usize| (skip..skip + rows).contains(&r);
    for (i, (d, pos)) in ranked.iter().enumerate() {
        if Some(i) == first_machine {
            let hr = disp(i) - 1;
            if visible(hr) {
                g.put_str(
                    x + 1,
                    y + (hr - skip) as u16,
                    "Your machines (will pair)",
                    t.dim(),
                    w,
                );
            }
        }
        let r = disp(i);
        if !visible(r) {
            continue;
        }
        let at = SRect {
            x,
            y: y + (r - skip) as u16,
            w,
            h: 1,
        };
        list_row(
            g,
            at,
            highlight(d.name(), pos, t.text(), hi),
            &d.note(now),
            i == f.sel,
            app,
        );
    }
    if ranked.is_empty() {
        let msg = match (&f.error, f.loading, f.peers.is_empty()) {
            (Some(e), _, _) => format!("✗ {e}"),
            (None, true, _) => "loading paired hosts…".into(),
            (None, false, true) => {
                "no paired hosts — pair one in Connections → Hosts (:sharing) or `vibeke gateway peer add <link>`".into()
            }
            (None, false, false) => "nothing matches".into(),
        };
        g.put_str(x + 1, y, &msg, t.dim(), w);
    }
    Some((
        x + 2 + UnicodeWidthStr::width(f.filter.as_str()) as u16,
        y - 1,
    ))
}

// ---- palette ----------------------------------------------------------------------------------------

pub fn action(app: &mut App, action: &str) -> bool {
    match action {
        "handoff_send" | "handoff_pane" => open_send(app),
        "handoff_details" => open_details(app),
        _ => return false,
    }
    true
}

#[cfg(test)]
#[path = "handoff_tests.rs"]
mod tests;
