//! The TUI client (03 §6, 08): connects one render stream per machine, composes chrome + pane
//! cells into a host-sized grid, and resolves keybindings client-side. The focused pane belongs
//! to the agent: Vibeke draws over it only for popups the user explicitly opened.

use crate::copy::CopyMode;
use crate::keymap::{self, Keymap};
use crate::screen::{Grid, HostCaps};
use crate::theme::Theme;
use crate::{draw, paste, term};
use anyhow::{Context, Result};
use crossterm::event::{Event, MouseButton as CtButton, MouseEventKind};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use vk_proto::frame::asyncio;
use vk_proto::input::{
    InputEvent, Key, KeyEvent, KeyKind, Mods, MouseButton, MouseEvent, MouseKind, NamedKey,
};
use vk_proto::layout::{self, Direction, Rect};
use vk_proto::model::*;
use vk_proto::render::*;

pub struct PaneBuf {
    pub epoch: u32,
    pub rev: u64,
    pub cols: u16,
    pub rows: u16,
    pub lines: Vec<Row>,
    pub cursor: Cursor,
    pub modes: PaneModes,
    pub title: String,
}

impl PaneBuf {
    pub fn blank() -> Self {
        PaneBuf {
            epoch: 0,
            rev: 0,
            cols: 0,
            rows: 0,
            lines: Vec::new(),
            cursor: Cursor::default(),
            modes: PaneModes::default(),
            title: String::new(),
        }
    }
}

/// One connected (or connecting) machine.
pub struct Machine {
    pub label: String,
    pub local: bool,
    pub tx: Option<mpsc::UnboundedSender<ClientFrame>>,
    pub model: SessionModel,
    pub focus: ClientFocus,
    pub seen: HashMap<String, u64>,
    pub panes: HashMap<String, PaneBuf>,
    pub status: String,
    pub last_hint: Vec<PaneRect>,
    pub clipboard_allowed: Option<bool>,
    pub pending: HashMap<u64, Pending>,
    pub auto_ws: bool,
    /// `render.attach` features of this machine's server (`event_push`, …).
    pub features: Vec<String>,
    /// Link state of a remote machine (RTT, degraded, last seen; 06 A7).
    pub link: Option<crate::remote_view::LinkProbe>,
}

#[derive(Debug, Clone)]
pub enum Pending {
    Ignore,
    Toast(String),
    /// Task surfaces (track form, task view, run → task map).
    Task(crate::tasks::Reply),
    /// The attention inbox.
    Attn(crate::inbox::Reply),
    /// Phone-gateway support (presence, confirm overlay, devices).
    Gateway(crate::gateway::Reply),
    /// Navigation (agent browser sessions for the palette and peek).
    Nav(crate::nav::Reply),
    /// Screenshot gallery / screenshot pane.
    Gallery(crate::gallery::Reply),
    /// Session desk.
    Desk(crate::desk::Reply),
    /// Drafts composer and notes.
    Drafts(crate::drafts::Reply),
    /// Assist actions (spec 14).
    Assist(crate::assist::Reply),
    /// A durable mutation (persisted in `client-pending.json` before dispatch); `then` handles
    /// the response once the operation is forgotten.
    Op {
        key: String,
        then: Box<Pending>,
    },
    /// `task.operation.get` for a pending operation after a reconnect or restart.
    Reconcile {
        key: String,
    },
    /// M4 parity surfaces (status bar, search, groups, layouts, appearance).
    Parity(crate::parity::Reply),
    /// Herdr plugin surfaces (actions, link handlers, UI state; M5).
    Plugin(crate::plugins::Reply),
    /// Preview proxy / mirror commands (06 B4).
    Preview(crate::browser::Reply),
    /// Remote link upkeep: event head and the reconnect replay (06 A7).
    Remote(crate::remote_view::Reply),
    /// Batch 2B surfaces (fleet, trust, popups, batch approvals).
    Ux(crate::ux::Reply),
}

/// A JSON-RPC error from a machine (07 canonical errors).
#[derive(Debug, Clone)]
pub struct RpcErr {
    pub kind: String,
    pub message: String,
    pub details: Value,
}

impl RpcErr {
    pub fn from_value(e: &Value) -> RpcErr {
        RpcErr {
            kind: e
                .pointer("/data/kind")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            message: e
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("error")
                .to_string(),
            details: e.pointer("/data/details").cloned().unwrap_or(Value::Null),
        }
    }
    /// The structured reason (`binding_unverified`, `send_unsafe`, `review_changed`, …).
    pub fn reason(&self) -> Option<&str> {
        self.details.get("reason").and_then(Value::as_str)
    }
    pub fn is_method_not_found(&self) -> bool {
        self.kind == "method_not_found"
    }
    /// The request may or may not have been applied.
    pub fn outcome_unknown(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "timeout" | "remote_unavailable" | "stalled"
        )
    }
}

impl Machine {
    pub fn new(label: &str, local: bool) -> Self {
        Machine {
            label: label.into(),
            local,
            tx: None,
            model: SessionModel::default(),
            focus: ClientFocus::default(),
            seen: HashMap::new(),
            panes: HashMap::new(),
            status: "connecting".into(),
            last_hint: Vec::new(),
            clipboard_allowed: None,
            pending: HashMap::new(),
            auto_ws: false,
            features: Vec::new(),
            link: None,
        }
    }
    pub fn send(&self, f: ClientFrame) -> bool {
        self.tx.as_ref().is_some_and(|t| t.send(f).is_ok())
    }
    pub fn connected(&self) -> bool {
        self.tx.is_some()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PromptKind {
    RenameTab,
    RenameWorkspace,
    RenamePane,
    NewWorkspace,
    Command,
    AgentReply {
        pane: String,
    },
    CardText {
        interaction: String,
    },
    TaskTitle,
    /// The browser pane's address bar.
    BrowserUrl {
        pane: String,
    },
    /// New workspace group (moving `ws` into it when set).
    GroupNew {
        mi: usize,
        ws: Option<String>,
    },
    GroupRename {
        mi: usize,
        group: String,
    },
    /// Name for `layout_save` (exports `tab`).
    LayoutSave {
        mi: usize,
        tab: String,
    },
    /// The argument of a palette action (`split: size?`, 08 §6.3).
    ActionArg {
        action: String,
    },
}

#[derive(Debug, Clone)]
pub struct Prompt {
    pub kind: PromptKind,
    pub label: String,
    pub input: String,
}

#[derive(Debug, Clone)]
pub enum Popup {
    Help,
    /// Goto list (`prefix+g`): filter text + selection.
    Goto {
        filter: String,
        sel: usize,
    },
    /// Searchable command palette (`prefix+:`, 08 §6.3).
    Palette {
        filter: String,
        sel: usize,
    },
    /// URL/ID hint labels over the focused pane (`url_hints`).
    Hints(crate::nav::Hints),
    /// Interaction card for an unfocused agent (08 §8).
    Card {
        interaction: String,
        sel: usize,
    },
    /// Peek at an agent without focusing it (08 §6.4).
    Peek {
        pane: String,
    },
    /// The attention inbox (08 §6.6, 15 §8); state lives in `App::inbox`.
    Inbox,
    /// **Track this work** form; state in `App::track`.
    Track,
    /// Task detail view (replaces the pane area); state in `App::task_view`.
    Task,
    /// Screenshot gallery / screenshot pane (06 B8); state in `App::gallery`.
    Gallery,
    /// Session desk (08 §6.7); state in `App::desk`.
    Desk,
    /// Drafts composer and notes (08 §6.7); state in `App::drafts`.
    Drafts,
    /// Assist preview → confirm → editable draft (14); state in `App::assist`.
    Assist,
    /// Pending client operations with unknown outcomes (15 §10.3).
    PendingOps {
        sel: usize,
        confirm: bool,
    },
    Confirm {
        message: String,
        action: Box<Action>,
    },
    /// One-line yes/no for a machine wanting to set the clipboard (06 A9).
    ClipboardAsk {
        machine: usize,
        data: Vec<u8>,
        primary: bool,
    },
    /// `paste.translate = "ask"`: confirm uploading dropped local files before translating
    /// the paste (06 A11). `sel` is the highlighted button: 0 upload, 1 original, 2 cancel.
    PasteAsk {
        machine: usize,
        pane: String,
        original: String,
        parsed: paste::ParsedPaste,
        items: Vec<crate::upload::Item>,
        sel: usize,
    },
    Message {
        title: String,
        body: String,
    },
    /// Move a workspace into a group (or out of one).
    GroupPick {
        mi: usize,
        ws: String,
        sel: usize,
    },
    /// Search across panes' scrollback and archive (`search_global`).
    Search(Box<crate::search::GlobalSearch>),
    /// Named layouts from `layout.list` (`layout_apply`).
    LayoutPick {
        mi: usize,
        layouts: Vec<crate::layouts::Entry>,
        sel: usize,
    },
    /// Plugin link handlers matching an activated link (07 §7.7).
    PluginLink(Box<crate::plugins::LinkChoice>),
    /// A pane wants to read the clipboard (OSC 52 read, `clipboard.osc52_read = "ask"`).
    ClipboardRead(crate::osc::ReadReq),
    /// Edit-scrollback viewer; state in `App::scrollback`.
    Scrollback,
    /// Local files pasted/dropped onto a browser pane: confirm before the page gets them
    /// (06 B3.2).
    BrowserDrop(Box<crate::browser_io::DropAsk>),
    /// First-run onboarding / `:setup` (08 §9); state in `App::ux.onboarding`.
    Onboarding,
    /// Batch approvals (08 §8); state in `App::ux.batch`.
    Batch,
    /// Fleet grid (08 §6.6); state in `App::ux.fleet`.
    Fleet,
    /// Repo-local config review (08 §11.1); state in `App::ux.trust`.
    TrustRepo,
    /// Elevation request review (09 §3.2), replacing the pane area; state in
    /// `App::ux.elevate`.
    Elevate,
    /// Shared-checkout collision view (05 §10); state in `App::ux.collision`.
    Collision,
    /// Every agent on every machine by attention (`agent_list`, 08 §6.5).
    Agents {
        filter: String,
        sel: usize,
    },
    /// A path input with a directory list standing in for a text prompt (`new_workspace`).
    Path(Box<crate::path_picker::PathPrompt>),
    /// Accept an incoming handoff (16 §15.2); state in `App::ux.handoff.accept`.
    HandoffAccept,
    /// Incoming handoffs and the ones being sent (the Connections view's Handoffs tab); state
    /// in `App::ux.handoff`.
    Handoffs,
    /// Hand the focused pane off to a paired host; state in `App::ux.handoff.send`.
    HandoffSend,
    /// Hosts (16 §15.3–§15.5; the Connections view's Hosts tab): peers, invitations, pasting
    /// an invitation, invited hosts; a full pane-area view, state in `App::ux.sharing`.
    Sharing,
    /// Devices (the Connections view's Devices tab: your paired phones): list, pair, revoke;
    /// state in `App::ux.devices`.
    Devices,
    /// People (the Connections view's People tab: colleagues' shares of a pane or workspace):
    /// list, share, revoke; state in `App::ux.people`.
    People,
    Updates,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    ClosePane(String),
    CloseTab(String),
    CloseWorkspace(String),
    Detach,
    /// Forget a task marked missing (`task.forget`).
    ForgetTask {
        machine: usize,
        task: String,
    },
}

#[derive(Debug, Clone)]
pub enum Mode {
    Normal,
    Prefix(Instant),
    Navigate { sel: usize },
    Resize,
    Copy(Box<CopyMode>),
    Prompt(Prompt),
    Popup(Popup),
}

/// A remote clipboard write waiting for the user's review (06 A9).
#[derive(Debug, Clone)]
pub struct ClipRequest {
    pub machine: usize,
    pub pane: String,
    pub data: Vec<u8>,
    pub primary: bool,
}

/// Unsolicited clipboard writes never take over the UI: they queue here as a non-modal notice
/// until the user opens the review popup (`review_clipboard`).
#[derive(Default)]
pub struct ClipGate {
    pub pending: Vec<ClipRequest>,
    last_prompt: HashMap<(usize, String), Instant>,
    /// Writes dropped for size or rate (shown in the review popup and notice).
    pub dropped: u64,
}

const CLIP_QUEUE_MAX: usize = 8;

pub struct Toast {
    pub text: String,
    pub until: Instant,
    pub pane: Option<(usize, String)>,
}

pub struct App {
    pub machines: Vec<Machine>,
    /// Machine owning the current focus.
    pub cur: usize,
    pub size: (u16, u16),
    pub caps: HostCaps,
    pub osc52: bool,
    pub kitty: bool,
    pub theme: Theme,
    pub keymap: Keymap,
    pub config: vk_config::Config,
    pub mode: Mode,
    pub toasts: Vec<Toast>,
    pub sidebar: bool,
    pub sidebar_w: u16,
    pub next_input: u64,
    pub client_id: String,
    pub prev: Grid,
    pub dirty: bool,
    pub quit: Option<String>,
    pub host_focused: bool,
    pub last_mouse_pane: Option<String>,
    pub next_req: u64,
    pub history_reqs: HashMap<u64, String>,
    pub uploads: crate::upload::Uploads,
    pub clip: ClipGate,
    /// Terminal effects on this client: OSC 52 reads, link hover, exit badges (03 §8).
    pub osc: crate::osc::State,
    /// Inbound kitty graphics of terminal panes (03 §9).
    pub images: crate::pane_images::State,
    /// Tests only: capture clipboard writes instead of touching the host terminal/clipboard.
    pub clipboard_sink: Option<Vec<(Vec<u8>, bool)>>,
    /// How copies reach the user's clipboard (OSC 52, platform tool, iTerm2 hint).
    pub copyout: crate::copyout::Delivery,
    /// The last text copied from this client (middle-click pastes it with `primary_selection`).
    pub last_copy: Option<String>,
    pub inbox: crate::inbox::InboxState,
    pub pending_ops: crate::pending::PendingStore,
    pub track: Option<crate::tasks::TrackForm>,
    pub task_view: Option<crate::tasks::TaskView>,
    /// (machine, run) → tracked task, from `task.detail` bindings (runs don't carry it).
    pub task_runs: HashMap<(usize, String), String>,
    pub task_runs_rev: HashMap<(usize, String), u64>,
    /// Surfaces to return to when a nested one closes (inbox → card/task, peek → track).
    pub return_to: Vec<Popup>,
    pub ui_seq: u64,
    pub gateway: crate::gateway::State,
    /// Browser panes on this client (06 B3.2).
    pub browser: crate::browser::BrowserUi,
    /// Event push per machine (07 §3).
    pub push: crate::push::State,
    /// Palette/goto history, last workspace, agent browser sessions, title sync.
    pub nav: crate::nav::Nav,
    /// M4 parity state: floats, groups, status bar, search, appearance, notifications.
    pub parity: crate::parity::State,
    /// Screenshot gallery, 📷 counters and the gallery's kitty image.
    pub gallery: crate::gallery::GalleryState,
    /// `!N` console-error badges and sidebar preview thumbnails.
    pub previews_ui: crate::preview_ui::State,
    pub desk: Option<crate::desk::Desk>,
    pub drafts: Option<crate::drafts::DraftsView>,
    pub assist: Option<crate::assist::Flow>,
    /// Herdr plugin surfaces: actions, link handlers, window title, scroll reports (M5).
    pub plugins: crate::plugins::State,
    /// Edit-scrollback viewer (`edit_scrollback`, 03 §11.3).
    pub scrollback: Option<crate::scrollback::ScrollbackView>,
    /// A program to run with the TUI suspended (the editor for edit-scrollback); taken by the
    /// main loop.
    pub external: Option<crate::scrollback::External>,
    /// Resolved `[keys.copy_mode]`.
    pub copy_keys: std::sync::Arc<crate::copykeys::CopyKeys>,
    /// Remote links: frame pacing by ack, reconnect replay (06 A7).
    pub remote: crate::remote_view::State,
    /// Batch 2B (spec 08): onboarding, trust, batch, fleet, tabs, sidebar, popups, sync input.
    pub ux: crate::ux::State,
}

pub struct Opts {
    pub update_args: Option<Vec<String>>,
    pub session: String,
    pub config: vk_config::Config,
    /// Machine to show first (e.g. the remote for `vibeke ssh host`).
    pub initial_machine: usize,
}

/// A connection request for a machine (local socket or remote bridge stream).
pub type Stream = Box<dyn AsyncReadWrite>;
pub trait AsyncReadWrite: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncReadWrite for T {}

pub enum Incoming {
    Frame(usize, ServerFrame),
    /// Render stream up, with the server's `render.attach` features.
    Connected(usize, mpsc::UnboundedSender<ClientFrame>, Vec<String>),
    Disconnected(usize, String),
    /// Progress from a transfer task (separate connection, see `upload`).
    Upload(crate::upload::UploadEvent),
    Update(crate::updates::Event),
}

/// Attach the render stream on `stream` for machine `idx`: JSON-RPC `render.attach`, then
/// binary frames. Spawns reader/writer tasks feeding `inc`.
pub async fn attach_stream(
    idx: usize,
    stream: Stream,
    client_id: String,
    remote: bool,
    inc: mpsc::UnboundedSender<Incoming>,
) -> Result<()> {
    let fps = if remote { 60 } else { 120 };
    attach_stream_fps(idx, stream, client_id, remote, fps, inc).await
}

/// [`attach_stream`] with an explicit frame cap (RTT-driven for remote links, 06 A7).
pub async fn attach_stream_fps(
    idx: usize,
    stream: Stream,
    client_id: String,
    remote: bool,
    max_fps: u32,
    inc: mpsc::UnboundedSender<Incoming>,
) -> Result<()> {
    let (rd, mut wr) = tokio::io::split(stream);
    let mut rd = BufReader::new(rd);
    let mut req = json!({"jsonrpc":"2.0","id":1,"method":"render.attach","params":{
        "client_id": client_id, "remote": remote, "protocol": vk_proto::render::PROTOCOL,
        "caps": {"max_fps": max_fps, "kitty_keyboard": true, "osc52": true, "truecolor": true}}});
    // Host terminal identity for native notifications and click-to-focus (08 §7.1): only to a
    // server on this machine, which is the one that can raise this terminal window.
    if !remote && let Some(h) = crate::notifications::host_meta() {
        req["params"]["host"] = h;
    }
    wr.write_all(format!("{req}\n").as_bytes()).await?;
    wr.flush().await?;
    let mut line = String::new();
    rd.read_line(&mut line).await?;
    let v: Value = serde_json::from_str(&line).context("render.attach reply")?;
    vk_proto::render::check_attach_reply(&v).map_err(|e| anyhow::anyhow!(e))?;
    let features: Vec<String> = v
        .pointer("/result/features")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|f| f.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let (tx, mut rx) = mpsc::unbounded_channel::<ClientFrame>();
    let _ = inc.send(Incoming::Connected(idx, tx, features));
    let inc2 = inc.clone();
    tokio::spawn(async move {
        loop {
            match asyncio::read_frame::<_, ServerFrame>(&mut rd).await {
                Ok(f) => {
                    if inc2.send(Incoming::Frame(idx, f)).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = inc2.send(Incoming::Disconnected(idx, e.to_string()));
                    break;
                }
            }
        }
    });
    tokio::spawn(async move {
        let mut w = tokio::io::BufWriter::new(wr);
        while let Some(f) = rx.recv().await {
            if asyncio::write_frame(&mut w, &f).await.is_err() {
                break;
            }
            while let Ok(f) = rx.try_recv() {
                if asyncio::write_frame(&mut w, &f).await.is_err() {
                    return;
                }
            }
            if w.flush().await.is_err() {
                break;
            }
        }
    });
    Ok(())
}

/// Something that can (re)connect a machine; returns a fresh stream.
pub type Connector =
    Box<dyn Fn() -> futures::future::BoxFuture<'static, Result<Stream>> + Send + Sync>;

pub struct MachineSpec {
    pub label: String,
    pub local: bool,
    pub connect: Connector,
    /// Connection for bulk uploads (a blob-class channel on a remote link, 06 A4); `None` =
    /// use `connect`.
    pub bulk: Option<Connector>,
    /// Link state of a remote machine (RTT, degraded, last seen).
    pub link: Option<crate::remote_view::LinkProbe>,
}

/// Run the TUI until detach or the last machine goes away.
pub async fn run(opts: Opts, machines: Vec<MachineSpec>) -> Result<String> {
    term::raw()?;
    let (mut probe, gcaps) = term::probe();
    // `terminal.host_overrides` (03 §6.1) before anything uses the probe.
    let _ = probe.apply_overrides(&opts.config.terminal.host_overrides);
    term::install_panic_hook();
    term::enter(probe.kitty_keyboard)?;
    let result = run_inner(opts, machines, probe, gcaps).await;
    term::leave();
    if let Ok(reason) = &result
        && let Some(message) = crate::updates::relaunch(reason)
    {
        return Ok(message);
    }
    result
}

async fn run_inner(
    opts: Opts,
    specs: Vec<MachineSpec>,
    probe: crate::caps::ProbeResult,
    gcaps: vk_browser::probe::GraphicsCaps,
) -> Result<String> {
    let client_id = format!("tui-{}-{}", std::process::id(), rand_suffix());
    let overrides = opts.config.terminal.host_overrides.clone();
    let (inc_tx, mut inc_rx) = mpsc::unbounded_channel::<Incoming>();
    let mut app = App::new(
        opts.config,
        specs
            .iter()
            .map(|s| Machine::new(&s.label, s.local))
            .collect(),
        client_id.clone(),
        HostCaps {
            truecolor: probe.truecolor
                || gcaps.kitty_graphics
                || std::env::var("COLORTERM")
                    .is_ok_and(|c| c.contains("truecolor") || c.contains("24bit")),
            sync_update: probe.sync_update,
            undercurl: probe.undercurl,
            osc8: probe.osc8,
            focus_events: probe.focus_events,
            sixel: probe.sixel,
            notifications: probe.notifications,
            ..crate::caps::graphics_overrides(
                crate::browser::host_caps(&gcaps, probe.xtversion.as_deref()),
                &overrides,
            )
        },
        matches!(probe.osc52, crate::caps::Osc52::Allowed),
        probe.kitty_keyboard,
    );
    app.cur = opts.initial_machine.min(specs.len().saturating_sub(1));
    app.pending_ops =
        crate::pending::PendingStore::open(crate::pending::default_dir(&opts.session), &client_id);
    if let Some(e) = app.pending_ops.load_error.clone() {
        app.toast(format!("pending operations unreadable: {e}"));
    }
    app.nav = crate::nav::Nav::open(
        crate::pending::default_dir(&opts.session),
        &crate::nav::client_key(),
        &opts.session,
    );
    if !app.pending_ops.ops.is_empty() {
        app.toast(format!(
            "{} pending operation(s) from a previous session — checking outcomes",
            app.pending_ops.ops.len()
        ));
    }
    crate::updates::init(&mut app, opts.update_args, inc_tx.clone());
    // Connect every machine (in the background; reconnect with backoff, 06 A7).
    let mut connectors: Vec<std::sync::Arc<Connector>> = Vec::new();
    let mut bulk: Vec<std::sync::Arc<Connector>> = Vec::new();
    for (i, s) in specs.into_iter().enumerate() {
        let c = std::sync::Arc::new(s.connect);
        bulk.push(s.bulk.map(std::sync::Arc::new).unwrap_or_else(|| c.clone()));
        connectors.push(c);
        app.machines[i].link = s.link;
    }
    let pacers: Vec<Option<crate::remote_view::LinkProbe>> =
        app.machines.iter().map(|m| m.link.clone()).collect();
    app.uploads.worker = Some(crate::upload::Worker {
        connectors: bulk,
        inc: inc_tx.clone(),
    });
    for (i, c) in connectors.iter().enumerate() {
        spawn_connect(
            i,
            c.clone(),
            client_id.clone(),
            (!app.machines[i].local, pacers[i].clone()),
            inc_tx.clone(),
            Duration::ZERO,
        );
    }
    // Host appearance from the startup probe (OSC 11 / `CSI ? 996 n`, theme auto).
    crate::appearance::on_detect(&mut app, crate::appearance::startup());
    crate::sidebar::load(
        &mut app,
        crate::sidebar::width_path(&opts.session, &crate::nav::client_key()),
    );
    crate::onboarding::maybe_open(&mut app);
    let mut events = crate::input::Reader::new();
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    loop {
        if app.dirty {
            let wait = Duration::from_millis(1000 / 120).saturating_sub(last_draw.elapsed());
            if wait.is_zero() {
                app.draw()?;
                last_draw = Instant::now();
            }
        }
        if let Some(r) = app.quit.take()
            && !crate::updates::prevent_quit(&mut app)
        {
            return Ok(r);
        }
        // Edit-scrollback's editor: suspend the TUI (stop reading input, restore the host
        // terminal), run it in the foreground, then take the terminal back and repaint.
        if let Some(x) = app.external.take() {
            drop(events);
            term::leave();
            let res = crate::scrollback::run_external(&x);
            let _ = term::raw();
            let _ = term::enter(app.kitty);
            events = crate::input::Reader::new();
            app.prev = Grid::new(0, 0);
            app.dirty = true;
            if let Err(e) = res {
                app.toast(e);
            }
        }
        // Re-query the host's light/dark appearance (after a focus change): the event reader
        // is stopped while the replies are read raw.
        if crate::appearance::take_reprobe(&mut app) {
            drop(events);
            let det = crate::appearance::reprobe();
            events = crate::input::Reader::new();
            crate::appearance::on_detect(&mut app, det);
        }
        let redraw_in = if app.dirty {
            Duration::from_millis(1000 / 120).saturating_sub(last_draw.elapsed())
        } else {
            Duration::from_secs(3600)
        };
        // No fixed tick (spec 10 §1.3.1): sleep until the earliest armed deadline, or until
        // input / a server frame when nothing needs time.
        let now = Instant::now();
        let wake = crate::deadline::wake_at(app.next_deadline(now), now);
        let timer = async move {
            match wake {
                Some(t) => tokio::time::sleep_until(tokio::time::Instant::from_std(t)).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            ev = events.next() => {
                match ev {
                    Some(Ok(ev)) => app.on_input(ev),
                    Some(Err(_)) | None => return Ok("input closed".into()),
                }
            }
            inc = inc_rx.recv() => {
                let Some(inc) = inc else { return Ok("disconnected".into()) };
                match inc {
                    Incoming::Connected(i, tx, features) => {
                        app.machines[i].tx = Some(tx);
                        app.machines[i].features = features;
                        app.machines[i].status = "connected".into();
                        app.machines[i].panes.clear();
                        app.machines[i].last_hint.clear();
                        app.on_connected(i);
                        app.dirty = true;
                    }
                    Incoming::Frame(i, f) => app.on_frame(i, f),
                    Incoming::Upload(e) => crate::upload::on_event(&mut app, e),
                    Incoming::Update(e) => crate::updates::on_event(&mut app, e),
                    Incoming::Disconnected(i, why) => {
                        app.machines[i].tx = None;
                        app.on_disconnected(i);
                        if why.contains("server stopped") {
                            app.machines[i].status = "stopped".into();
                        } else {
                            app.machines[i].status = "offline".into();
                        }
                        app.dirty = true;
                        if app.machines.iter().all(|m| !m.connected()) && app.machines.len() == 1 && app.machines[0].local && app.machines[0].status == "stopped" {
                            return Ok("server stopped".into());
                        }
                        spawn_connect(i, connectors[i].clone(), client_id.clone(), (!app.machines[i].local, pacers[i].clone()), inc_tx.clone(), Duration::from_millis(500));
                    }
                }
                while let Ok(more) = inc_rx.try_recv() {
                    match more {
                        Incoming::Frame(i, f) => app.on_frame(i, f),
                        Incoming::Upload(e) => crate::upload::on_event(&mut app, e),
                        Incoming::Update(e) => crate::updates::on_event(&mut app, e),
                        Incoming::Connected(i, tx, features) => { app.machines[i].tx = Some(tx); app.machines[i].features = features; app.machines[i].status = "connected".into(); app.machines[i].panes.clear(); app.machines[i].last_hint.clear(); app.on_connected(i); }
                        Incoming::Disconnected(i, _) => {
                            app.machines[i].tx = None;
                            app.on_disconnected(i);
                            app.machines[i].status = "offline".into();
                            spawn_connect(i, connectors[i].clone(), client_id.clone(), (!app.machines[i].local, pacers[i].clone()), inc_tx.clone(), Duration::from_millis(500));
                        }
                    }
                }
                app.dirty = true;
            }
            _ = timer => app.on_deadline(Instant::now()),
            _ = tokio::time::sleep(redraw_in) => {}
        }
        // Housekeeping after every wakeup: whatever became due (by time or because of what
        // just arrived) is handled now; the next deadline is computed from the result.
        app.on_tick();
    }
}

impl App {
    pub fn new(
        config: vk_config::Config,
        machines: Vec<Machine>,
        client_id: String,
        caps: HostCaps,
        osc52: bool,
        kitty: bool,
    ) -> App {
        let copy_keys = std::sync::Arc::new(crate::copykeys::CopyKeys::from_config(
            &config.keys.copy_mode,
        ));
        App {
            machines,
            cur: 0,
            size: term::size(),
            caps,
            osc52,
            kitty,
            theme: Theme::named(&config.theme.name),
            keymap: Keymap::from_config(&config),
            sidebar: !config.ui.sidebar.collapsed,
            sidebar_w: config.ui.sidebar.width.clamp(18, 48),
            config,
            mode: Mode::Normal,
            toasts: Vec::new(),
            gateway: Default::default(),
            next_input: 1,
            client_id,
            prev: Grid::new(0, 0),
            dirty: true,
            quit: None,
            host_focused: true,
            last_mouse_pane: None,
            next_req: 1,
            history_reqs: HashMap::new(),
            uploads: Default::default(),
            clip: Default::default(),
            osc: crate::osc::State::from_env(),
            images: Default::default(),
            clipboard_sink: None,
            copyout: crate::copyout::Delivery::from_env(),
            last_copy: None,
            inbox: Default::default(),
            pending_ops: Default::default(),
            track: None,
            task_view: None,
            task_runs: HashMap::new(),
            task_runs_rev: HashMap::new(),
            return_to: Vec::new(),
            ui_seq: 1,
            browser: Default::default(),
            push: Default::default(),
            nav: Default::default(),
            parity: Default::default(),
            gallery: Default::default(),
            previews_ui: Default::default(),
            desk: None,
            drafts: None,
            assist: None,
            plugins: Default::default(),
            scrollback: None,
            external: None,
            copy_keys,
            remote: Default::default(),
            ux: Default::default(),
        }
    }
}

fn rand_suffix() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{t:x}")
}

/// (Re)connect machine `i` with exponential backoff from 0.5 s to a 30 s cap, ±20% jitter
/// (06 A7). `link.0` = remote; `link.1` = its link probe, whose RTT sets the attach frame cap.
fn spawn_connect(
    i: usize,
    c: std::sync::Arc<Connector>,
    client_id: String,
    link: (bool, Option<crate::remote_view::LinkProbe>),
    inc: mpsc::UnboundedSender<Incoming>,
    initial: Duration,
) {
    let (remote, probe) = link;
    tokio::spawn(async move {
        let mut delay = initial;
        loop {
            if !delay.is_zero() {
                tokio::time::sleep(delay.mul_f64(crate::remote_view::jitter())).await;
            }
            if let Ok(stream) = (c)().await {
                let fps = match (&probe, remote) {
                    (Some(p), _) => {
                        crate::remote_view::target_hz(crate::remote_view::CLIENT_HZ, p().rtt_ms)
                    }
                    (None, true) => crate::remote_view::CLIENT_HZ,
                    (None, false) => 120,
                };
                if let Ok(()) =
                    attach_stream_fps(i, stream, client_id.clone(), remote, fps, inc.clone()).await
                {
                    return;
                }
            }
            delay = (delay * 2).clamp(Duration::from_millis(500), Duration::from_secs(30));
        }
    });
}

// ---- helpers over the current machine -----------------------------------------------------

impl App {
    pub fn m(&self) -> &Machine {
        &self.machines[self.cur]
    }
    pub fn m_mut(&mut self) -> &mut Machine {
        &mut self.machines[self.cur]
    }
    pub fn focused_pane(&self) -> Option<String> {
        self.m().focus.pane.clone()
    }
    pub fn focused_tab(&self) -> Option<Tab> {
        let m = self.m();
        let t = m.focus.tab.as_ref()?;
        m.model.tabs.iter().find(|x| &x.id == t).cloned()
    }
    pub fn focused_ws(&self) -> Option<Workspace> {
        let m = self.m();
        let w = m.focus.workspace.as_ref()?;
        m.model.workspaces.iter().find(|x| &x.id == w).cloned()
    }
    pub fn pane_area(&self) -> Rect {
        let rows = self.size.1;
        // Sidebar left or right, tab bar top/bottom/hidden (08 §2.4, §3).
        let (x, w) = crate::chrome::main_x(self);
        let (tt, tb) = crate::chrome::tab_rows(self);
        // The status bar takes a row next to the tab bar or at the far edge (08 §4).
        let (top, bottom) = crate::statusbar::reserved(self);
        Rect {
            x,
            y: tt + top,
            w,
            h: rows.saturating_sub(tt + tb + top + bottom),
        }
    }
    /// Tiled pane rects for the focused tab (zoom applied).
    pub fn tiled_rects(&self) -> Vec<(String, Rect)> {
        let Some(tab) = self.focused_tab() else {
            return vec![];
        };
        let area = self.pane_area();
        if let Some(z) = &tab.zoomed_pane {
            return vec![(z.clone(), area)];
        }
        layout::rects(&tab.layout, area)
    }
    /// Every visible pane of the focused tab: floating panes first (topmost first, content
    /// rects inside their frames), then the tiling. Hit tests take the first match, so floats
    /// win; `ViewHint` reports them so their PTYs get real sizes (08 §5).
    /// Plugin popups/overlays come before everything (M5); nothing under an overlay is
    /// visible, so it is the only pane (with a popup over it) then.
    pub fn pane_rects(&self) -> Vec<(String, Rect)> {
        let surf = crate::plugins::surfaces(self);
        let mut v: Vec<(String, Rect)> = surf
            .iter()
            .rev()
            .map(|s| (s.pane.clone(), s.inner))
            .collect();
        if surf
            .iter()
            .any(|s| s.info.kind == vk_proto::model::SurfaceKind::Overlay)
        {
            return v;
        }
        v.extend(
            crate::floats::visible(self)
                .into_iter()
                .rev()
                .map(|f| (f.pane, f.inner)),
        );
        v.extend(self.tiled_rects());
        v
    }

    pub(crate) fn command(&mut self, method: &str, params: Value, pending: Pending) {
        let req = self.next_req;
        self.next_req += 1;
        let json = json!({"jsonrpc":"2.0","id":req,"method":method,"params":params}).to_string();
        let m = self.m_mut();
        m.pending.insert(req, pending);
        m.send(ClientFrame::Command { req, json });
    }

    pub(crate) fn command_on(
        &mut self,
        machine: usize,
        method: &str,
        params: Value,
        pending: Pending,
    ) {
        let saved = self.cur;
        self.cur = machine;
        self.command(method, params, pending);
        self.cur = saved;
    }

    pub(crate) fn next_ui_id(&mut self) -> u64 {
        self.ui_seq += 1;
        self.ui_seq
    }

    /// A fresh caller-scoped idempotency key.
    pub(crate) fn new_idempotency_key(&mut self, what: &str) -> String {
        let n = self.next_ui_id();
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{}-{what}-{t:x}-{n}", self.client_id)
    }

    /// Dispatch a task mutation durably: the operation (with its idempotency key) is written to
    /// the local pending file before anything is sent; returns false (and sends nothing) when it
    /// can't be persisted or the machine is offline.
    pub(crate) fn mutate(
        &mut self,
        mi: usize,
        method: &str,
        mut params: Value,
        then: Pending,
    ) -> bool {
        if !self.machines[mi].connected() {
            self.toast(format!(
                "{} offline — nothing sent",
                self.machines[mi].label
            ));
            return false;
        }
        let key = match params.get("idempotency_key").and_then(Value::as_str) {
            Some(k) => k.to_string(),
            None => {
                let k = self.new_idempotency_key("op");
                params["idempotency_key"] = json!(k);
                k
            }
        };
        let op = crate::pending::PendingOp {
            key: key.clone(),
            method: method.into(),
            params: params.clone(),
            machine: self.machines[mi].label.clone(),
            created_at_ms: vk_now_ms(),
            status: crate::pending::OpStatus::InFlight,
            note: None,
        };
        if let Err(e) = self.pending_ops.add(op) {
            self.toast(format!(
                "✗ couldn't record pending operation ({e}); not sent"
            ));
            return false;
        }
        self.command_on(
            mi,
            method,
            params,
            Pending::Op {
                key,
                then: Box::new(then),
            },
        );
        true
    }

    /// Ask the owner what happened to operations dispatched before a restart or link drop.
    /// Never resubmits.
    pub(crate) fn reconcile(&mut self, mi: usize) {
        if !self.machines[mi].connected() {
            return;
        }
        let label = self.machines[mi].label.clone();
        for key in self.pending_ops.needs_reconcile(&label) {
            self.pending_ops.reconciling.insert(key.clone());
            self.command_on(
                mi,
                "task.operation.get",
                json!({"idempotency_key": key}),
                Pending::Reconcile { key },
            );
        }
    }

    pub(crate) fn on_connected(&mut self, i: usize) {
        crate::browser::on_connected(self, i);
        crate::inbox::on_connected(self, i);
        crate::gateway::on_connected(self, i);
        crate::push::on_connected(self, i);
        crate::parity::on_connected(self, i);
        crate::plugins::on_connected(self, i);
        crate::remote_view::on_connected(self, i);
        crate::ux::on_connected(self, i);
        // Another client of this session may have crashed since we started: adopt its pending
        // operations (never a live client's) so their outcomes get asked for too.
        let n = self.pending_ops.adopt_orphans();
        if n > 0 {
            self.toast(format!(
                "{n} pending operation(s) from a closed client — checking outcomes"
            ));
        }
        self.reconcile(i);
    }

    pub(crate) fn on_disconnected(&mut self, i: usize) {
        let label = self.machines[i].label.clone();
        self.pending_ops.connection_lost(&label);
        // Responses to requests on the old connection will never arrive.
        self.machines[i].pending.clear();
        self.inbox.outstanding.remove(&i);
        crate::tasks::on_disconnect(self, i);
        crate::remote_view::on_disconnected(self, i);
        crate::taskbadge::on_disconnected(self, i);
        crate::collision::on_disconnected(self, i);
        if self.inbox.outstanding.is_empty()
            && let Some(f) = self.inbox.next_after.take()
        {
            crate::inbox::next_attention(self, f);
        }
    }

    /// Close the current surface, returning to the one that opened it (if any).
    pub(crate) fn restore_return(&mut self) {
        self.mode = match self.return_to.pop() {
            Some(Popup::Inbox) => {
                let v = crate::inbox::view(self);
                crate::inbox::sync_selection(&mut self.inbox, &v.items);
                Mode::Popup(Popup::Inbox)
            }
            Some(Popup::Task) if self.task_view.is_some() => Mode::Popup(Popup::Task),
            Some(p @ Popup::Peek { .. }) => Mode::Popup(p),
            _ => Mode::Normal,
        };
    }

    pub fn toast(&mut self, text: impl Into<String>) {
        self.toasts.push(Toast {
            text: text.into(),
            until: Instant::now() + Duration::from_secs(6),
            pane: None,
        });
        if self.toasts.len() > 3 {
            self.toasts.remove(0);
        }
        self.dirty = true;
    }

    fn input_id(&mut self) -> u64 {
        let id = self.next_input;
        self.next_input += 1;
        id
    }

    pub(crate) fn send_key(&mut self, ev: KeyEvent) {
        if crate::browser::focused_browser(self).is_some() {
            crate::browser::send_key(self, ev);
            return;
        }
        let Some(pane) = self.focused_pane() else {
            return;
        };
        if !self.m().connected() {
            self.toast(format!("{} offline — input not sent", self.m().label));
            return;
        }
        let id = self.input_id();
        self.m().send(ClientFrame::Key {
            input_id: id,
            pane: pane.clone(),
            key: ev.clone(),
        });
        // Synchronized input (08 §5).
        crate::sync_input::mirror_key(self, &pane, &ev);
    }

    pub(crate) fn focus_pane(&mut self, machine: usize, pane: &str) {
        self.return_to.clear();
        self.cur = machine;
        let p = pane.to_string();
        if let Some(pn) = self.machines[machine]
            .model
            .panes
            .iter()
            .find(|x| x.id == p)
            .cloned()
        {
            let m = &mut self.machines[machine];
            m.focus = ClientFocus {
                workspace: Some(pn.workspace.clone()),
                tab: Some(pn.tab.clone()),
                pane: Some(p.clone()),
            };
        }
        self.machines[machine].send(ClientFrame::Focus { pane: p });
        self.dirty = true;
    }

    // ---- server frames ------------------------------------------------------------------

    pub(crate) fn on_frame(&mut self, i: usize, f: ServerFrame) {
        self.dirty = true;
        match f {
            ServerFrame::Hello { machine, .. } => {
                if self.machines[i].local && self.machines[i].label.is_empty() {
                    self.machines[i].label = machine;
                    self.reconcile(i);
                }
            }
            ServerFrame::Model { model, focus, seen } => {
                let m = &mut self.machines[i];
                m.model = *model;
                m.seen = seen.into_iter().collect();
                // The server owns per-client focus; adopt it unless we're mid-switch.
                m.focus = focus;
                if m.focus.pane.is_none()
                    && let Some(t) = m.model.tabs.first()
                {
                    let p = t
                        .focused_pane
                        .clone()
                        .or_else(|| t.layout.panes().first().cloned());
                    if let Some(p) = p {
                        m.send(ClientFrame::Focus { pane: p });
                    }
                }
                let empty_here = self.machines[i].model.workspaces.is_empty()
                    && self.machines[i].connected()
                    && !self.machines[i].auto_ws;
                let all_empty = self.machines.iter().all(|m| m.model.workspaces.is_empty());
                if empty_here && (!self.machines[i].local || all_empty) {
                    // Fresh session: first workspace in the cwd (local) or the remote home.
                    self.machines[i].auto_ws = true;
                    let params = if self.machines[i].local {
                        let cwd = std::env::current_dir()
                            .map(|d| d.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        json!({"cwd": cwd, "focus": true})
                    } else {
                        json!({"focus": true})
                    };
                    self.command_on(i, "workspace.create", params, Pending::Ignore);
                }
                crate::osc::on_model(self, i);
                crate::tasks::on_model(self, i);
                crate::gateway::on_model(self, i);
                crate::parity::on_model(self, i);
                crate::inbox::invalidate(self);
                if let Mode::Popup(Popup::Card { interaction, .. }) = &self.mode
                    && !self.machines.iter().any(|m| {
                        m.model
                            .interactions
                            .iter()
                            .any(|x| &x.id == interaction && x.status == InteractionStatus::Open)
                    })
                {
                    self.restore_return();
                    self.toast("interaction resolved");
                }
            }
            ServerFrame::PaneFull {
                pane,
                epoch,
                rev,
                cols,
                rows,
                lines,
                cursor,
                modes,
                title,
            } => {
                crate::pane_images::on_pane_full(self, i, &pane);
                self.machines[i].panes.insert(
                    pane.clone(),
                    PaneBuf {
                        epoch,
                        rev,
                        cols,
                        rows,
                        lines,
                        cursor,
                        modes,
                        title,
                    },
                );
                crate::remote_view::ack(self, i, pane, epoch, rev);
            }
            ServerFrame::PaneDiff {
                pane,
                epoch,
                base_rev,
                rev,
                ops,
                cursor,
                modes,
                title,
            } => {
                let ok = match self.machines[i].panes.get_mut(&pane) {
                    Some(b) if b.epoch == epoch && b.rev == base_rev => {
                        for op in ops {
                            match op {
                                DiffOp::ScrollUp { n } => {
                                    let n = (n as usize).min(b.lines.len());
                                    b.lines.drain(..n);
                                    b.lines.extend(std::iter::repeat_n(Row::default(), n));
                                }
                                DiffOp::Rows(rows) => {
                                    for (y, r) in rows {
                                        if let Some(slot) = b.lines.get_mut(y as usize) {
                                            *slot = r;
                                        }
                                    }
                                }
                            }
                        }
                        b.rev = rev;
                        b.cursor = cursor;
                        b.modes = modes;
                        b.title = title;
                        true
                    }
                    _ => false,
                };
                if ok {
                    crate::remote_view::ack(self, i, pane, epoch, rev);
                } else {
                    self.machines[i].send(ClientFrame::Resync { pane });
                }
            }
            ServerFrame::History {
                pane,
                req,
                start,
                total,
                lines,
            } => {
                // The edit-scrollback editor's styled rows (`editor_include_ansi`).
                if crate::scrollback::on_history(self, i, &pane, req, start, total, &lines) {
                    return;
                }
                let mut follow = None;
                if let Mode::Copy(cm) = &mut self.mode
                    && cm.pane == pane
                    && let Some((s, c)) = cm.on_history(req, start, total, lines)
                {
                    let r = self.next_req;
                    self.next_req += 1;
                    cm.pending_req = Some(r);
                    follow = Some((r, s, c));
                }
                if let Some((r, s, c)) = follow {
                    self.machines[i].send(ClientFrame::FetchHistory {
                        req: r,
                        pane,
                        start: s,
                        count: c,
                    });
                }
                self.history_reqs.remove(&req);
                // Copy mode jumping to a search hit continues once history is in.
                crate::search::after_history(self);
            }
            ServerFrame::Notify {
                title,
                body,
                pane,
                delivered,
                ..
            } => crate::notifications::on_notify(self, i, title, body, pane, delivered),
            ServerFrame::Bell { pane } => {
                if Some(&pane) != self.focused_pane().as_ref() {
                    let _ = std::io::stdout().write_all(b"\x07");
                }
            }
            ServerFrame::Clipboard {
                selection,
                data,
                pane,
            } => {
                // A browser pane's page clipboard counts as a write from the pane's owner.
                if let Some(data) = crate::browser_io::on_clipboard(self, i, &pane, data) {
                    self.on_clipboard(i, pane, matches!(selection, ClipSel::Primary), data)
                }
            }
            ServerFrame::InputAck { status, .. } => {
                if status == AckStatus::DroppedOffline {
                    self.toast("offline — input not sent");
                }
            }
            ServerFrame::CommandResult { req, json } => self.on_command_result(i, req, &json),
            ServerFrame::Pong { .. } => {}
            ServerFrame::Media(m) => crate::browser::on_media(self, i, *m),
            ServerFrame::BrowserState { pane, state } => {
                crate::browser::on_state(self, i, pane, state)
            }
            ServerFrame::Events { events, lagged } => {
                crate::push::on_events(self, i, events, lagged)
            }
            ServerFrame::ClipboardQuery {
                req,
                pane,
                selection,
            } => crate::osc::on_query(self, i, req, pane, selection),
            ServerFrame::Image {
                hash,
                width,
                height,
                rgba_z,
            } => crate::pane_images::on_image(self, i, hash, width, height, rgba_z),
            ServerFrame::PaneImages { pane, places, .. } => {
                crate::pane_images::on_places(self, i, pane, places)
            }
            ServerFrame::Goodbye { reason } => {
                self.machines[i].status = if reason.contains("stop") {
                    "stopped".into()
                } else {
                    reason
                };
            }
        }
    }

    fn on_command_result(&mut self, i: usize, req: u64, json: &str) {
        let pending = self.machines[i]
            .pending
            .remove(&req)
            .unwrap_or(Pending::Ignore);
        let v: Value = serde_json::from_str(json).unwrap_or(Value::Null);
        let res = match v.get("error") {
            Some(e) => Err(RpcErr::from_value(e)),
            None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
        };
        self.dispatch_result(i, pending, res);
    }

    fn dispatch_result(&mut self, i: usize, pending: Pending, res: Result<Value, RpcErr>) {
        match pending {
            Pending::Ignore => {
                if let Err(e) = res {
                    self.toast(format!("✗ {}", e.message));
                }
            }
            Pending::Toast(t) => match res {
                Ok(_) => self.toast(t),
                Err(e) => self.toast(format!("✗ {}", e.message)),
            },
            Pending::Task(r) => crate::tasks::on_reply(self, i, r, res),
            Pending::Attn(r) => crate::inbox::on_reply(self, i, r, res),
            Pending::Gateway(r) => crate::gateway::on_reply(self, i, r, res),
            Pending::Nav(r) => crate::nav::on_reply(self, i, r, res),
            Pending::Gallery(r) => crate::gallery::on_reply(self, i, r, res),
            Pending::Desk(r) => crate::desk::on_reply(self, i, r, res),
            Pending::Drafts(r) => crate::drafts::on_reply(self, i, r, res),
            Pending::Assist(r) => crate::assist::on_reply(self, i, r, res),
            Pending::Op { key, then } => {
                if let Err(e) = &res
                    && e.outcome_unknown()
                {
                    // Not a definitive answer: keep the operation and ask the owner later.
                    self.pending_ops.live.remove(&key);
                    self.toast(format!(
                        "⚠ outcome unknown ({}) — will check with {}",
                        e.message, self.machines[i].label
                    ));
                    self.reconcile(i);
                } else {
                    self.pending_ops.remove(&key);
                }
                self.dispatch_result(i, *then, res);
            }
            Pending::Reconcile { key } => self.on_reconciled(i, &key, res),
            Pending::Parity(r) => crate::parity::on_reply(self, i, r, res),
            Pending::Plugin(r) => crate::plugins::on_reply(self, i, r, res),
            Pending::Preview(r) => crate::browser::on_reply(self, i, r, res),
            Pending::Remote(r) => crate::remote_view::on_reply(self, i, r, res),
            Pending::Ux(r) => crate::ux::on_reply(self, i, r, res),
        }
    }

    fn on_reconciled(&mut self, i: usize, key: &str, res: Result<Value, RpcErr>) {
        let Some(op) = self.pending_ops.get(key).cloned() else {
            return;
        };
        let what = op.describe();
        match res {
            Ok(v) if v.get("known").and_then(Value::as_bool) == Some(true) => {
                let result = v.get("result").cloned().unwrap_or(Value::Null);
                if op.method == "task.track"
                    && let (Some(b), Some(t)) = (result.get("binding"), result.get("task"))
                {
                    let run = b.get("run_id").and_then(Value::as_str).unwrap_or("");
                    let tid = t.get("id").and_then(Value::as_str).unwrap_or("");
                    self.task_runs.insert((i, run.into()), tid.into());
                }
                let msg = format!("✓ {what} — it completed (confirmed after reconnect)");
                self.pending_ops.remove(key);
                self.pending_ops.push_outcome(msg.clone());
                self.toast(msg);
            }
            Ok(v) => {
                let note = v
                    .get("note")
                    .and_then(Value::as_str)
                    .unwrap_or("no receipt")
                    .to_string();
                let expired = v.get("expired").and_then(Value::as_bool) == Some(true);
                self.pending_ops.mark_unknown(
                    key,
                    if expired {
                        format!("receipt expired — {note}")
                    } else {
                        note
                    },
                );
                self.toast(format!(
                    "⚠ {what}: outcome unknown — :pending_operations to review"
                ));
            }
            Err(e) => {
                let note = if e.is_method_not_found() {
                    "this machine can't report operation outcomes".to_string()
                } else {
                    e.message
                };
                self.pending_ops.mark_unknown(key, note);
                self.toast(format!(
                    "⚠ {what}: outcome unknown — :pending_operations to review"
                ));
            }
        }
    }

    /// An OSC 52 write from a pane. Never replaces the current UI mode: when consent is needed it
    /// queues a non-modal notice (`review_clipboard` opens the prompt). Oversized writes are
    /// dropped and prompts are rate-limited per pane.
    pub fn on_clipboard(&mut self, i: usize, pane: String, primary: bool, data: Vec<u8>) {
        let remote = !self.machines[i].local;
        let policy = &self.config.clipboard;
        if data.len() as u64 > policy.remote_write_max_bytes.0 {
            self.clip.dropped += 1;
            let msg = format!(
                "dropped {} clipboard write from {} (limit {})",
                crate::upload::human(data.len() as u64),
                self.machines[i].label,
                crate::upload::human(policy.remote_write_max_bytes.0),
            );
            self.toast(msg);
            return;
        }
        let allowed = if !remote {
            !matches!(policy.osc52_write, vk_config::AllowDeny::Deny)
        } else {
            match policy.remote_write {
                vk_config::RemoteWrite::Allow => true,
                vk_config::RemoteWrite::Deny => false,
                vk_config::RemoteWrite::AskOnce => match self.machines[i].clipboard_allowed {
                    Some(a) => a,
                    None => {
                        let interval = policy.remote_write_min_interval.0;
                        let key = (i, pane.clone());
                        let now = Instant::now();
                        if self
                            .clip
                            .last_prompt
                            .get(&key)
                            .is_some_and(|t| now.duration_since(*t) < interval)
                        {
                            self.clip.dropped += 1;
                            return;
                        }
                        self.clip.last_prompt.insert(key, now);
                        // One queued request per machine (the latest wins), bounded overall.
                        self.clip.pending.retain(|r| r.machine != i);
                        if self.clip.pending.len() >= CLIP_QUEUE_MAX {
                            self.clip.pending.remove(0);
                            self.clip.dropped += 1;
                        }
                        self.clip.pending.push(ClipRequest {
                            machine: i,
                            pane,
                            data,
                            primary,
                        });
                        let label = self.machines[i].label.clone();
                        self.toast(format!(
                            "⎘ {label} wants to set your clipboard — prefix+y to review"
                        ));
                        return;
                    }
                },
            }
        };
        if allowed {
            self.set_clipboard(&data, primary);
        }
    }

    /// `review_clipboard`: open the prompt for the oldest queued request.
    fn review_clipboard(&mut self) {
        // Pending clipboard reads first: a program is waiting for the answer.
        if crate::osc::review_read(self) {
            return;
        }
        if self.clip.pending.is_empty() {
            self.toast("no clipboard request pending");
            return;
        }
        let r = self.clip.pending.remove(0);
        self.mode = Mode::Popup(Popup::ClipboardAsk {
            machine: r.machine,
            data: r.data,
            primary: r.primary,
        });
    }

    /// Put `data` on the user's clipboard (`crate::copyout`: OSC 52, plus the platform tool
    /// when not over SSH) and toast `copied N chars` / `copy failed — why`. PRIMARY writes
    /// are silent.
    pub fn set_clipboard(&mut self, data: &[u8], primary: bool) {
        let res = if let Some(sink) = &mut self.clipboard_sink {
            sink.push((data.to_vec(), primary));
            Ok(())
        } else {
            let max = self.config.clipboard.remote_write_max_bytes.0;
            let plan = crate::copyout::plan(data, primary, &self.copyout.env, max);
            let cmd = self.copyout.native_cmd.clone();
            crate::copyout::deliver(data, primary, &plan, &mut std::io::stdout(), &mut |d, p| {
                crate::copyout::native_copy(cmd.as_deref(), d, p)
            })
        };
        if primary {
            return;
        }
        match res {
            Ok(()) => {
                let text = String::from_utf8_lossy(data).into_owned();
                let n = text.chars().count();
                self.last_copy = Some(text);
                self.toast(format!("copied {n} char{}", if n == 1 { "" } else { "s" }));
                if let Some(h) = self.copyout.take_hint() {
                    self.toast(h);
                }
            }
            Err(why) => self.toast(format!("copy failed — {why}")),
        }
    }

    /// A user copy (copy-mode yank, copy-on-select): the clipboard, plus PRIMARY when
    /// `clipboard.primary_selection` is on (03 §11.1).
    pub fn copy_text(&mut self, text: &str) {
        self.set_clipboard(text.as_bytes(), false);
        if self.config.clipboard.primary_selection {
            self.set_clipboard(text.as_bytes(), true);
        }
    }

    /// Act on a copy-mode outcome (keys or mouse wheel).
    pub(crate) fn copy_outcome(&mut self, mut cm: Box<CopyMode>, out: crate::copy::Outcome) {
        match out {
            crate::copy::Outcome::Stay => self.mode = Mode::Copy(cm),
            crate::copy::Outcome::Exit => {}
            crate::copy::Outcome::Yank(text) => self.copy_text(&text),
            crate::copy::Outcome::Fetch { start, count } => {
                let req = self.next_req;
                self.next_req += 1;
                self.history_reqs.insert(req, cm.pane.clone());
                cm.pending_req = Some(req);
                self.m().send(ClientFrame::FetchHistory {
                    req,
                    pane: cm.pane.clone(),
                    start,
                    count,
                });
                self.mode = Mode::Copy(cm);
            }
            // No match in memory: search the archive (and unloaded scrollback).
            crate::copy::Outcome::Search { q, back } => {
                crate::search::copy_search(self, cm, q, back)
            }
            // At the top of everything in memory: page older rows from the archive.
            crate::copy::Outcome::Archive => crate::search::copy_page(self, cm),
            // The viewer / editor at copy mode's view (03 §11.3).
            crate::copy::Outcome::EditScrollback { line } => {
                self.mode = Mode::Normal;
                crate::scrollback::open(self, Some(line));
            }
        }
    }

    pub(crate) fn on_tick(&mut self) {
        let now = Instant::now();
        let before = self.toasts.len();
        self.toasts.retain(|t| t.until > now);
        if self.toasts.len() != before {
            self.dirty = true;
        }
        if let Mode::Prefix(at) = self.mode
            && at.elapsed() > Duration::from_millis(self.keymap.prefix_timeout_ms)
        {
            self.mode = Mode::Normal;
            self.dirty = true;
        }
        crate::inbox::tick(self);
        crate::tasks::tick(self);
        crate::gateway::tick(self);
        crate::parity::on_tick(self);
        crate::assist::tick(self);
        crate::browser::tick(self);
        crate::plugins::report_scroll(self, now);
        crate::selection::tick(self, now);
        crate::remote_view::release_due(self, now);
        crate::ux::on_tick(self);
    }

    /// A deadline woke the loop: repaint when a redraw-only one passed (an age label, the
    /// confirm countdown, the clock); `on_tick` follows and handles the rest.
    fn on_deadline(&mut self, now: Instant) {
        if self.deadlines(now).redraw_due(now) {
            self.dirty = true;
        }
    }

    /// Everything that needs time in the current state (spec 10 §1.3.1). An idle client (no
    /// toast, no countdown, no working agent, no open poller, push-capable servers) arms none.
    pub fn deadlines(&self, now: Instant) -> crate::deadline::Deadlines {
        let mut d = crate::deadline::Deadlines::default();
        if let Some(t) = self.toasts.iter().map(|t| t.until).min() {
            d.at("toast", t);
        }
        if let Mode::Prefix(at) = self.mode {
            d.at(
                "prefix",
                at + Duration::from_millis(self.keymap.prefix_timeout_ms),
            );
        }
        crate::draw::deadlines(self, now, &mut d);
        crate::inbox::deadlines(self, now, &mut d);
        crate::tasks::deadlines(self, now, &mut d);
        crate::gateway::deadlines(self, now, &mut d);
        crate::statusbar::deadlines(self, now, &mut d);
        crate::assist::deadlines(self, now, &mut d);
        crate::browser::deadlines(self, now, &mut d);
        crate::plugins::deadlines(self, now, &mut d);
        crate::selection::deadlines(self, &mut d);
        crate::remote_view::deadlines(self, &mut d);
        crate::osc::deadlines(self, now, &mut d);
        crate::ux::deadlines(self, now, &mut d);
        d
    }

    /// The earliest of [`App::deadlines`]; `None` = sleep until input or a server frame.
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        self.deadlines(now).next()
    }

    // ---- host events --------------------------------------------------------------------

    /// Host input from [`crate::input::Reader`]: keys the client decoded itself, or events
    /// (mouse, paste, focus, resize).
    fn on_input(&mut self, i: crate::input::Input) {
        match i {
            crate::input::Input::Event(ev) => self.on_event(ev),
            crate::input::Input::Key(k) => {
                self.dirty = true;
                crate::gateway::on_input(self);
                let k = self.keymap.altgr(k);
                self.on_key(k);
            }
        }
    }

    fn on_event(&mut self, ev: Event) {
        self.dirty = true;
        if matches!(ev, Event::Key(_) | Event::Mouse(_) | Event::Paste(_)) {
            crate::gateway::on_input(self);
        }
        // The confirm overlay is modal: pointer and paste events never reach panes behind it.
        if matches!(ev, Event::Mouse(_) | Event::Paste(_)) && self.gateway.modal() {
            return;
        }
        match ev {
            Event::Key(k) => {
                if let Some(ev) = keymap::from_crossterm(&k) {
                    let ev = self.keymap.altgr(ev);
                    self.on_key(ev);
                }
            }
            Event::Paste(text) => self.on_paste(text),
            Event::Mouse(me) => self.on_mouse(me),
            Event::Resize(c, r) => {
                crate::browser::on_resize(self);
                self.size = (c, r);
                self.prev = Grid::new(0, 0);
            }
            Event::FocusGained => {
                self.host_focused = true;
                self.send_view_hints(true);
                crate::appearance::on_focus_gained(self);
            }
            Event::FocusLost => {
                self.host_focused = false;
                self.send_view_hints(true);
            }
        }
    }

    pub(crate) fn on_key(&mut self, ev: KeyEvent) {
        if ev.kind != KeyKind::Release && matches!(self.mode, Mode::Normal) {
            crate::osc::dismiss_recovery(self);
        }
        if crate::gateway::key(self, &ev) {
            return;
        }
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
        let from_card = matches!(
            mode,
            Mode::Popup(Popup::Card { .. })
                | Mode::Prompt(Prompt {
                    kind: PromptKind::CardText { .. },
                    ..
                })
        );
        self.on_key_mode(ev, mode);
        // A card opened from the inbox returns there once answered or dismissed.
        if from_card && matches!(self.mode, Mode::Normal) && !self.return_to.is_empty() {
            self.restore_return();
        }
    }

    fn on_key_mode(&mut self, ev: KeyEvent, mode: Mode) {
        match mode {
            Mode::Normal => {
                if ev.kind == KeyKind::Release {
                    // Releases go to the pane only (the encoder drops them unless asked).
                    if !self.keymap.is_prefix(&ev) {
                        self.send_key(ev);
                    }
                    return;
                }
                if self.keymap.is_prefix(&ev) {
                    self.mode = Mode::Prefix(Instant::now());
                    return;
                }
                // A plugin popup is modal: its terminal gets every key (08 §5).
                if crate::plugins::normal_key(self, &ev) {
                    return;
                }
                // Direct bindings never steal keys from a focused browser page.
                if let Some(b) = self.keymap.direct(&ev).cloned()
                    && crate::browser::focused_browser(self).is_none()
                    && crate::ux::binding_active(self, &b.action)
                {
                    if b.action == "remote_image_paste" && !self.m().local {
                        crate::upload::image_paste(self);
                        return;
                    }
                    if b.action != "remote_image_paste" {
                        self.action(&b.action, b.index);
                        return;
                    }
                }
                self.send_key(ev);
            }
            Mode::Prefix(_) => {
                if ev.kind == KeyKind::Release {
                    self.mode = Mode::Prefix(Instant::now());
                    return;
                }
                if matches!(
                    ev.key,
                    Key::Named(
                        NamedKey::LeftShift
                            | NamedKey::RightShift
                            | NamedKey::LeftControl
                            | NamedKey::RightControl
                            | NamedKey::LeftAlt
                            | NamedKey::RightAlt
                            | NamedKey::LeftSuper
                            | NamedKey::RightSuper
                    )
                ) {
                    self.mode = Mode::Prefix(Instant::now());
                    return;
                }
                if self.keymap.is_prefix(&ev) && self.keymap.passthrough {
                    self.send_key(ev);
                    return;
                }
                if crate::plugins::prefix_key(self, &ev) || crate::browser::prefix_key(self, &ev) {
                } else if let Some(b) = self.keymap.prefixed(&ev).cloned() {
                    self.action(&b.action, b.index);
                } else if matches!(ev.key, Key::Named(NamedKey::Escape)) {
                } else {
                    self.toast(format!(
                        "no binding for prefix+{}",
                        vk_term::keygrammar::format_key(&ev)
                    ));
                }
            }
            Mode::Navigate { sel } => self.navigate_key(ev, sel),
            Mode::Resize => self.resize_key(ev),
            Mode::Copy(mut cm) => {
                if ev.kind == KeyKind::Release {
                    self.mode = Mode::Copy(cm);
                    return;
                }
                let out = cm.key(&ev);
                self.copy_outcome(cm, out);
            }
            Mode::Prompt(p) => self.prompt_key(ev, p),
            Mode::Popup(p) => self.popup_key(ev, p),
        }
    }

    pub(crate) fn on_paste(&mut self, text: String) {
        match &mut self.mode {
            Mode::Prompt(p) => {
                p.input.push_str(&text.replace(['\n', '\r'], " "));
                return;
            }
            Mode::Normal => {}
            Mode::Popup(Popup::Path(p)) => {
                p.picker.paste(&text);
                return;
            }
            Mode::Popup(Popup::HandoffAccept) => {
                crate::handoff::on_paste(self, &text);
                return;
            }
            Mode::Popup(Popup::Sharing) => {
                crate::sharing::on_paste(self, &text);
                return;
            }
            Mode::Popup(Popup::People) => {
                crate::people::on_paste(self, &text);
                return;
            }
            Mode::Popup(Popup::Devices | Popup::Updates) => return,
            Mode::Popup(_) => {
                // Editors in the drafts, desk and assist views.
                crate::drafts::on_paste(self, &text);
                return;
            }
            _ => return,
        }
        if crate::browser::on_paste(self, &text) {
            return;
        }
        let Some(pane) = self.focused_pane() else {
            return;
        };
        // Dropped/pasted local paths into panes that can't see them (06 A11): remote panes,
        // and local sandboxed/container panes for paths outside what the box can read (13 §5).
        let contained_roots = self
            .m()
            .model
            .panes
            .iter()
            .find(|p| p.id == pane && p.isolation.is_contained())
            .map(|p| p.isolation.visible_roots.clone());
        let local = self.m().local;
        if (!local || contained_roots.is_some())
            && !matches!(self.config.paste.translate, vk_config::PasteTranslate::Off)
        {
            let parsed = match self.config.paste.translate {
                vk_config::PasteTranslate::Embedded => paste::parse_embedded(&text),
                _ => paste::parse_paste(&text),
            };
            if let Some(parsed) = parsed {
                let home = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_default();
                let visible_in_box = local
                    && contained_roots
                        .as_ref()
                        .is_some_and(|roots| paste::all_under(&parsed, &home, roots));
                if paste::existing_local_paths(&parsed, &home) && !visible_in_box {
                    crate::upload::translate_paste(self, &pane, text, parsed, &home);
                    return;
                }
            }
        }
        let id = self.input_id();
        crate::sync_input::mirror_paste(self, &pane, &text);
        self.m().send(ClientFrame::Paste {
            input_id: id,
            pane,
            text,
        });
    }

    pub(crate) fn on_mouse(&mut self, me: crossterm::event::MouseEvent) {
        let (me, px) = crate::browser::cellify(self, me);
        // Moving/resizing a popup by its frame (08 §5).
        if crate::popup_pane::on_mouse(self, &me) {
            return;
        }
        // Plugin popups are modal; overlay headers and popup frames are chrome.
        if crate::plugins::on_mouse(self, me.column, me.row) {
            return;
        }
        // A selection drag in progress follows the pointer past the pane (03 §11.1); the
        // scrollback viewer selects over its own text.
        if crate::selection::on_active_drag(self, &me) || crate::scrollback::on_mouse(self, &me) {
            return;
        }
        // Tab drag/overflow/middle-click, sidebar border and rail, focus follows mouse (08).
        if crate::ux::on_mouse(self, &me) {
            return;
        }
        // Float frames (move/resize/raise), group rows and drags, the status bar.
        if crate::parity::on_mouse(self, &me) {
            return;
        }
        // OSC 8 links: ctrl/alt hover underlines, ctrl/alt+click opens (03 §8).
        if crate::osc::on_mouse(self, &me) {
            return;
        }
        if crate::browser::on_mouse(self, &me, px) {
            return;
        }
        let (x, y) = (me.column, me.row);
        // Sidebar clicks.
        if crate::chrome::in_sidebar(self, x) {
            match me.kind {
                MouseEventKind::Down(CtButton::Left) => {
                    if let Some((mi, pane)) = draw::sidebar_hit(self, y) {
                        self.focus_pane(mi, &pane);
                    }
                }
                // The pane's context menu (08 §2): its handoff actions.
                MouseEventKind::Down(CtButton::Right) => {
                    if let Some((mi, pane)) = draw::sidebar_hit(self, y) {
                        crate::handoff::pane_menu(self, mi, &pane);
                    }
                }
                _ => {}
            }
            return;
        }
        if crate::chrome::tab_row(self) == Some(y) {
            if let MouseEventKind::Down(CtButton::Left) = me.kind
                && let Some(tab) = draw::tabbar_hit(self, x)
            {
                self.command("tab.focus", json!({"tab": tab}), Pending::Ignore);
            }
            return;
        }
        let rects = self.pane_rects();
        let Some((pane, r)) = rects.into_iter().find(|(_, r)| r.contains(x, y)) else {
            return;
        };
        let mouse_mode = self.m().panes.get(&pane).is_some_and(|b| b.modes.mouse);
        let shift = me.modifiers.contains(crossterm::event::KeyModifiers::SHIFT);
        if let MouseEventKind::Down(b) = me.kind
            && self.focused_pane().as_deref() != Some(&pane)
        {
            let cur = self.cur;
            self.focus_pane(cur, &pane);
            // A left press also starts a selection (or reaches the app) in the new pane.
            if !mouse_mode && b != CtButton::Left {
                return;
            }
        }
        // Drag-to-select / copy-on-select, and the mouse inside copy mode (03 §11.1).
        if crate::selection::on_mouse(self, &me, &pane, r, mouse_mode) {
            return;
        }
        if mouse_mode && !shift {
            self.forward_mouse(&pane, &me, r);
            return;
        }
        if let MouseEventKind::ScrollUp = me.kind {
            self.enter_copy(Some(3));
        }
        let _ = InputEvent::FocusIn;
    }

    /// Send a host mouse event to the app in `pane` (content rect `r`) as a pane-local event.
    pub(crate) fn forward_mouse(&mut self, pane: &str, me: &crossterm::event::MouseEvent, r: Rect) {
        let (kind, button) = match me.kind {
            MouseEventKind::Down(b) => (MouseKind::Press, btn(b)),
            MouseEventKind::Up(b) => (MouseKind::Release, btn(b)),
            MouseEventKind::Drag(b) => (MouseKind::Drag, btn(b)),
            MouseEventKind::Moved => (MouseKind::Move, MouseButton::None),
            MouseEventKind::ScrollUp => (MouseKind::Press, MouseButton::WheelUp),
            MouseEventKind::ScrollDown => (MouseKind::Press, MouseButton::WheelDown),
            MouseEventKind::ScrollLeft => (MouseKind::Press, MouseButton::WheelLeft),
            MouseEventKind::ScrollRight => (MouseKind::Press, MouseButton::WheelRight),
        };
        let mut mods = Mods::empty();
        if me
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
        {
            mods = mods | Mods::CTRL;
        }
        if me.modifiers.contains(crossterm::event::KeyModifiers::ALT) {
            mods = mods | Mods::ALT;
        }
        let id = self.input_id();
        self.m().send(ClientFrame::Mouse {
            input_id: id,
            pane: pane.to_string(),
            event: MouseEvent {
                kind,
                button,
                col: me.column.saturating_sub(r.x),
                row: me.row.saturating_sub(r.y),
                mods,
            },
        });
    }

    // ---- actions --------------------------------------------------------------------------

    pub fn action(&mut self, action: &str, index: Option<usize>) {
        if crate::plugins::action(self, action) || crate::ux::action(self, action) {
            return;
        }
        if crate::browser::action_name(self, action) || crate::parity::action(self, action) {
            return;
        }
        // Gallery, desk, drafts/notes and assist palette commands (08 §6.7, 06 B8, 14).
        if crate::gallery::action(self, action)
            || crate::desk::action(self, action)
            || crate::drafts::action(self, action)
            || crate::assist::action(self, action)
            || crate::chrome::action(self, action)
            || crate::scrollback::action(self, action)
        {
            return;
        }
        let pane = self.focused_pane();
        let tab = self.focused_tab();
        let ws = self.focused_ws();
        match action {
            "help" => self.mode = Mode::Popup(Popup::Help),
            "detach" => self.quit = Some("detached".into()),
            "cancel_transfer" => crate::upload::cancel_all(self),
            "review_clipboard" => self.review_clipboard(),
            "split_vertical" | "split_horizontal" => {
                if let Some(p) = pane {
                    let dir = if action == "split_vertical" {
                        "right"
                    } else {
                        "down"
                    };
                    self.command(
                        "pane.split",
                        json!({"pane": p, "direction": dir, "focus": true}),
                        Pending::Ignore,
                    );
                }
            }
            "close_pane" => {
                if let Some(p) = pane {
                    let busy = self.pane_busy(&p);
                    if busy
                        && !matches!(self.config.ui.confirm_close, vk_config::ConfirmClose::Never)
                    {
                        self.mode = Mode::Popup(Popup::Confirm {
                            message: "Close pane? A process is running.".into(),
                            action: Box::new(Action::ClosePane(p)),
                        });
                    } else {
                        self.command("pane.close", json!({"pane": p}), Pending::Ignore);
                    }
                }
            }
            "zoom" => {
                if let Some(p) = pane {
                    self.command("pane.zoom", json!({"pane": p}), Pending::Ignore);
                }
            }
            "resize_mode" => self.mode = Mode::Resize,
            "toggle_sidebar" => self.sidebar = !self.sidebar,
            "focus_pane_left" | "focus_pane_right" | "focus_pane_up" | "focus_pane_down" => {
                let d = match action {
                    "focus_pane_left" => Direction::Left,
                    "focus_pane_right" => Direction::Right,
                    "focus_pane_up" => Direction::Up,
                    _ => Direction::Down,
                };
                if let Some(p) = pane
                    && let Some(n) = layout::neighbor(&self.pane_rects(), &p, d)
                {
                    let cur = self.cur;
                    self.focus_pane(cur, &n);
                }
            }
            "cycle_pane_next" | "cycle_pane_previous" => {
                let rects = self.pane_rects();
                if let Some(p) = pane
                    && let Some(i) = rects.iter().position(|(x, _)| *x == p)
                {
                    let n = rects.len();
                    let j = if action == "cycle_pane_next" {
                        (i + 1) % n
                    } else {
                        (i + n - 1) % n
                    };
                    let target = rects[j].0.clone();
                    let cur = self.cur;
                    self.focus_pane(cur, &target);
                }
            }
            "new_tab" => {
                if let Some(w) = ws {
                    let cwd = pane
                        .as_ref()
                        .and_then(|p| self.m().model.panes.iter().find(|x| &x.id == p))
                        .and_then(|x| x.cwd.clone());
                    self.command(
                        "tab.create",
                        json!({"workspace": w.id, "focus": true, "cwd": cwd}),
                        Pending::Ignore,
                    );
                }
            }
            "next_tab" | "previous_tab" | "switch_tab" => {
                if let Some(w) = ws {
                    let tabs: Vec<Tab> = self
                        .m()
                        .model
                        .tabs
                        .iter()
                        .filter(|t| t.workspace == w.id)
                        .cloned()
                        .collect();
                    if tabs.is_empty() {
                        return;
                    }
                    let cur = tab
                        .as_ref()
                        .and_then(|t| tabs.iter().position(|x| x.id == t.id))
                        .unwrap_or(0);
                    let j = match action {
                        "next_tab" => (cur + 1) % tabs.len(),
                        "previous_tab" => (cur + tabs.len() - 1) % tabs.len(),
                        _ => match index {
                            Some(i) if i < tabs.len() => i,
                            _ => return,
                        },
                    };
                    self.switch_tab(&tabs[j]);
                }
            }
            "close_tab" => {
                if let Some(t) = tab {
                    self.mode = Mode::Popup(Popup::Confirm {
                        message: format!("Close tab {}?", t.number),
                        action: Box::new(Action::CloseTab(t.id)),
                    });
                }
            }
            "rename_tab" => self.prompt(
                PromptKind::RenameTab,
                "tab title",
                tab.and_then(|t| t.title).unwrap_or_default(),
            ),
            "rename_workspace" => self.prompt(
                PromptKind::RenameWorkspace,
                "workspace name",
                ws.map(|w| w.display_name().to_string()).unwrap_or_default(),
            ),
            "rename_pane" => self.prompt(PromptKind::RenamePane, "pane title", String::new()),
            "new_workspace" => {
                crate::path_picker::open(self, PromptKind::NewWorkspace, "new workspace dir", "~/")
            }
            "close_workspace" => {
                if let Some(w) = ws {
                    self.mode = Mode::Popup(Popup::Confirm {
                        message: format!("Close workspace {}?", w.display_name()),
                        action: Box::new(Action::CloseWorkspace(w.id)),
                    });
                }
            }
            "workspace_picker" => {
                self.mode = Mode::Navigate {
                    sel: draw::sidebar_index_of_focus(self),
                }
            }
            "goto" => {
                self.mode = Mode::Popup(Popup::Goto {
                    filter: String::new(),
                    sel: 0,
                })
            }
            "last_workspace" => crate::nav::last_workspace(self),
            "url_hints" => crate::nav::open_hints(self),
            "browser_watch" => match pane {
                Some(p) => {
                    let cur = self.cur;
                    crate::nav::watch_session(self, cur, json!({"agent_pane": p}));
                }
                None => self.toast("no focused agent"),
            },
            "next_workspace" | "previous_workspace" => {
                let list: Vec<Workspace> = self.m().model.workspaces.clone();
                if let Some(w) = ws
                    && let Some(i) = list.iter().position(|x| x.id == w.id)
                {
                    let j = if action == "next_workspace" {
                        (i + 1) % list.len()
                    } else {
                        (i + list.len() - 1) % list.len()
                    };
                    self.command(
                        "workspace.focus",
                        json!({"workspace": list[j].id}),
                        Pending::Ignore,
                    );
                }
            }
            "enter_copy_mode" | "search_scrollback" => {
                self.enter_copy(None);
                if action == "search_scrollback"
                    && let Mode::Copy(cm) = &mut self.mode
                {
                    cm.start_search(false);
                }
            }
            "next_attention" => crate::inbox::next_attention(self, false),
            "next_attention_focus" => crate::inbox::next_attention(self, true),
            "track_work" | "track" => match pane {
                Some(p) => {
                    let cur = self.cur;
                    crate::tasks::open_track(self, cur, &p)
                }
                None => self.toast("no focused agent to track"),
            },
            "task_details" | "task" => {
                let cur = self.cur;
                let run = pane
                    .as_ref()
                    .and_then(|p| self.m().model.runs.iter().find(|r| &r.pane == p).cloned());
                match run.and_then(|r| crate::tasks::task_for_run(self, cur, &r)) {
                    Some(t) => crate::tasks::open_task(self, cur, &t),
                    None => self.toast("this agent isn't tracked — :track_work"),
                }
            }
            "pending_operations" | "pending" => {
                self.mode = Mode::Popup(Popup::PendingOps {
                    sel: 0,
                    confirm: false,
                })
            }
            "mark_unread" => {
                if let Some(p) = pane {
                    self.command(
                        "pane.mark_unread",
                        json!({"pane": p}),
                        Pending::Toast("marked unread".into()),
                    );
                }
            }
            "pin_pane" => {
                if let Some(p) = pane {
                    self.command("pane.pin", json!({"pane": p}), Pending::Ignore);
                }
            }
            "reload_config" => {
                match vk_config::Config::load(vk_config::config_path()) {
                    Ok((c, _)) => {
                        self.keymap = Keymap::from_config(&c);
                        self.copy_keys = std::sync::Arc::new(
                            crate::copykeys::CopyKeys::from_config(&c.keys.copy_mode),
                        );
                        self.theme = Theme::named(&c.theme.name);
                        self.config = c;
                        crate::appearance::apply(self, true);
                        // The reloaded keymap lacks the plugins' default bindings: re-read them.
                        for mi in 0..self.machines.len() {
                            crate::plugins::refresh(self, mi);
                        }
                        self.toast("config reloaded");
                    }
                    Err(e) => self.toast(format!("config error: {e}")),
                }
                self.command("server.reload_config", json!({}), Pending::Ignore);
            }
            "open_notification_target" => {
                if let Some((mi, p)) = self.toasts.iter().rev().find_map(|t| t.pane.clone()) {
                    self.focus_pane(mi, &p);
                }
            }
            "command_palette" => crate::nav::open_palette(self, String::new()),
            "new_task" => self.prompt(PromptKind::TaskTitle, "new task title", String::new()),
            "inbox" => crate::inbox::open(self),
            "paste_buffer" => {}
            a if a.starts_with("command:") => {
                let i: usize = a[8..].parse().unwrap_or(usize::MAX);
                if let Some(c) = self.config.keys.command.get(i).cloned() {
                    if crate::ux::command_active(self, &c) {
                        self.run_key_command(&c);
                    } else {
                        self.toast(format!(
                            "{}: only for {}",
                            c.title.clone().unwrap_or_else(|| c.command.clone()),
                            c.when.clone().unwrap_or_default()
                        ));
                    }
                }
            }
            other => self.toast(format!("{other}: not available yet")),
        }
    }

    pub(crate) fn run_key_command(&mut self, c: &vk_config::KeyCommand) {
        if c.kind == vk_config::CommandType::PluginAction {
            crate::plugins::run_key_command(self, c);
            return;
        }
        // Popups and floats (08 §5).
        if crate::popup_pane::run_key_command(self, c) {
            return;
        }
        let Some(pane) = self.focused_pane() else {
            return;
        };
        let cmd = vec!["/bin/sh".to_string(), "-c".into(), c.command.clone()];
        match c.kind {
            vk_config::CommandType::Shell => {
                let _ = std::process::Command::new("/bin/sh")
                    .arg("-c")
                    .arg(&c.command)
                    .spawn();
            }
            vk_config::CommandType::Pane => {
                self.command("pane.split", json!({"pane": pane, "direction": "right", "focus": true, "command": cmd, "title": c.title}), Pending::Ignore);
            }
            _ => {
                let ws = self.focused_ws().map(|w| w.id);
                self.command("tab.create", json!({"workspace": ws, "focus": true, "command": cmd, "title": c.title.clone().unwrap_or_else(|| c.command.clone())}), Pending::Ignore);
            }
        }
    }

    fn switch_tab(&mut self, t: &Tab) {
        let p = t
            .focused_pane
            .clone()
            .or_else(|| t.layout.panes().first().cloned());
        if let Some(p) = p {
            let cur = self.cur;
            self.focus_pane(cur, &p);
        }
    }

    fn pane_busy(&self, pane: &str) -> bool {
        let Some(p) = self.m().model.panes.iter().find(|x| x.id == pane) else {
            return false;
        };
        let shell = |s: &str| {
            matches!(
                s.rsplit('/').next().unwrap_or(s).trim_start_matches('-'),
                "zsh" | "bash" | "fish" | "sh" | "dash" | "nu" | "tcsh" | "ksh"
            )
        };
        !p.fg_cmdline.is_empty() && !shell(&p.fg_cmdline[0])
    }

    fn prompt(&mut self, kind: PromptKind, label: &str, initial: String) {
        self.mode = Mode::Prompt(Prompt {
            kind,
            label: label.into(),
            input: initial,
        });
    }

    pub fn enter_copy(&mut self, scroll: Option<usize>) {
        let Some(pane) = self.focused_pane() else {
            return;
        };
        let Some(buf) = self.m().panes.get(&pane) else {
            return;
        };
        let mut cm = CopyMode::new(&pane, buf.lines.clone(), buf.cols, buf.cursor);
        cm.set_keys(self.copy_keys.clone());
        // Where in-memory rows end and the archive begins (archive paging, M4); asked first so
        // the first history page can stay in memory.
        crate::search::request_bounds(self, &pane);
        let req = self.next_req;
        self.next_req += 1;
        cm.pending_req = Some(req);
        self.m().send(ClientFrame::FetchHistory {
            req,
            pane: pane.clone(),
            start: 0,
            count: 0,
        });
        if let Some(n) = scroll {
            cm.scroll_up(n);
        }
        self.mode = Mode::Copy(Box::new(cm));
    }

    /// `prefix+a`: card for the oldest open interaction on an unfocused agent, else focus the
    /// oldest done run (08 §6.5).
    pub(crate) fn next_attention_m1(&mut self, focus_instead: bool) {
        let focused = self.focused_pane();
        let mut best: Option<(i64, usize, Interaction)> = None;
        for (mi, m) in self.machines.iter().enumerate() {
            for i in &m.model.interactions {
                if i.status != InteractionStatus::Open
                    || (mi == self.cur && Some(&i.pane) == focused.as_ref())
                {
                    continue;
                }
                if best.as_ref().is_none_or(|(t, _, _)| i.opened_at_ms < *t) {
                    best = Some((i.opened_at_ms, mi, i.clone()));
                }
            }
        }
        if let Some((_, mi, i)) = best {
            if focus_instead {
                self.focus_pane(mi, &i.pane);
            } else {
                self.cur_card(mi, &i.id);
            }
            return;
        }
        // Oldest done (idle + unseen).
        for (mi, m) in self.machines.iter().enumerate() {
            if let Some(r) = m.model.runs.iter().find(|r| {
                r.execution.value == Execution::Idle
                    && r.done_rev > *m.seen.get(&r.pane).unwrap_or(&0)
                    && Some(&r.pane) != focused.as_ref()
            }) {
                let p = r.pane.clone();
                self.focus_pane(mi, &p);
                return;
            }
        }
        self.toast("nothing needs you");
    }

    fn cur_card(&mut self, mi: usize, interaction: &str) {
        // `ui.interaction_overlay` decides (08 §8).
        crate::popup_pane::open_card(self, mi, interaction);
    }

    pub fn answer(&mut self, mi: usize, interaction: &str, params: Value) {
        let mut p = params;
        p["interaction"] = json!(interaction);
        p["idempotency_key"] = json!(format!("{}-{}", self.client_id, interaction));
        self.command_on(
            mi,
            "interaction.answer",
            p,
            Pending::Toast("answer sent".into()),
        );
    }

    fn navigate_key(&mut self, ev: KeyEvent, sel: usize) {
        if crate::ux::navigate_key(self, &ev, sel) || crate::groups::navigate_key(self, &ev, sel) {
            return;
        }
        let rows = draw::sidebar_targets(self);
        let n = rows.len().max(1);
        match ev.key {
            Key::Named(NamedKey::Escape) | Key::Char('q') => {}
            Key::Named(NamedKey::Down) | Key::Char('j') => {
                self.mode = Mode::Navigate { sel: (sel + 1) % n }
            }
            Key::Named(NamedKey::Up) | Key::Char('k') => {
                self.mode = Mode::Navigate {
                    sel: (sel + n - 1) % n,
                }
            }
            Key::Named(NamedKey::Enter) => {
                if let Some((mi, pane)) = rows.get(sel).cloned() {
                    self.focus_pane(mi, &pane);
                }
            }
            Key::Char(' ') => {
                if let Some((mi, pane)) = rows.get(sel).cloned() {
                    self.cur = mi;
                    // The peek shows the agent's browser session (`[w] watch`).
                    crate::nav::refresh_sessions(self, mi);
                    self.mode = Mode::Popup(Popup::Peek { pane });
                }
            }
            Key::Char('a') => {
                if let Some((mi, pane)) = rows.get(sel).cloned() {
                    let int = self.machines[mi]
                        .model
                        .interactions
                        .iter()
                        .find(|i| i.pane == pane && i.status == InteractionStatus::Open)
                        .map(|i| i.id.clone());
                    match int {
                        Some(i) => self.cur_card(mi, &i),
                        None => self.mode = Mode::Navigate { sel },
                    }
                }
            }
            Key::Char('u') => {
                if let Some((mi, pane)) = rows.get(sel).cloned() {
                    self.command_on(
                        mi,
                        "pane.mark_unread",
                        json!({"pane": pane}),
                        Pending::Ignore,
                    );
                }
                self.mode = Mode::Navigate { sel };
            }
            Key::Char('x') => {
                if let Some((_, pane)) = rows.get(sel).cloned() {
                    self.mode = Mode::Popup(Popup::Confirm {
                        message: "Close pane?".into(),
                        action: Box::new(Action::ClosePane(pane)),
                    });
                }
            }
            Key::Char('n') => {
                crate::path_picker::open(self, PromptKind::NewWorkspace, "new workspace dir", "~/")
            }
            Key::Char('r') => {
                self.prompt(PromptKind::RenameWorkspace, "workspace name", String::new())
            }
            Key::Char(c) if c.is_ascii_digit() => {
                let i = c.to_digit(10).unwrap_or(1).saturating_sub(1) as usize;
                if let Some((mi, pane)) = rows.get(i).cloned() {
                    self.focus_pane(mi, &pane);
                }
            }
            _ => self.mode = Mode::Navigate { sel },
        }
    }

    fn resize_key(&mut self, ev: KeyEvent) {
        if crate::floats::resize_key(self, &ev) {
            return;
        }
        let Some(pane) = self.focused_pane() else {
            return;
        };
        let step = if ev.mods.shift() || matches!(ev.key, Key::Char(c) if c.is_uppercase()) {
            25
        } else {
            5
        };
        let dir = match ev.key {
            Key::Char('h' | 'H') | Key::Named(NamedKey::Left) => Some("left"),
            Key::Char('l' | 'L') | Key::Named(NamedKey::Right) => Some("right"),
            Key::Char('k' | 'K') | Key::Named(NamedKey::Up) => Some("up"),
            Key::Char('j' | 'J') | Key::Named(NamedKey::Down) => Some("down"),
            _ => None,
        };
        match (dir, ev.key) {
            (Some(d), _) => {
                self.command(
                    "pane.resize",
                    json!({"pane": pane, "direction": d, "percent": step}),
                    Pending::Ignore,
                );
                self.mode = Mode::Resize;
            }
            (None, Key::Char('=')) => {
                self.command("pane.equalize", json!({}), Pending::Ignore);
                self.mode = Mode::Resize;
            }
            (None, Key::Named(NamedKey::Escape | NamedKey::Enter)) | (None, Key::Char('q')) => {}
            _ => self.mode = Mode::Resize,
        }
    }

    fn prompt_key(&mut self, ev: KeyEvent, mut p: Prompt) {
        if ev.kind == KeyKind::Release {
            // Kitty hosts report releases; only presses type.
            self.mode = Mode::Prompt(p);
            return;
        }
        match ev.key {
            Key::Named(NamedKey::Escape) => {}
            Key::Named(NamedKey::Enter) => self.submit_prompt(p),
            Key::Named(NamedKey::Backspace) => {
                p.input.pop();
                self.mode = Mode::Prompt(p);
            }
            Key::Char('u') if ev.mods.ctrl() => {
                p.input.clear();
                self.mode = Mode::Prompt(p);
            }
            // Peek reply box: Save as draft (08 §6.7).
            Key::Char('d') if ev.mods.ctrl() && matches!(p.kind, PromptKind::AgentReply { .. }) => {
                if let PromptKind::AgentReply { pane } = &p.kind {
                    let pane = pane.clone();
                    crate::drafts::save_reply_as_draft(self, &pane, &p.input);
                    self.mode = Mode::Popup(Popup::Peek { pane });
                }
            }
            Key::Char(c) if !ev.mods.ctrl() && !ev.mods.alt() => {
                p.input.push(c);
                self.mode = Mode::Prompt(p);
            }
            _ => self.mode = Mode::Prompt(p),
        }
    }

    pub(crate) fn submit_prompt(&mut self, p: Prompt) {
        let v = p.input.trim().to_string();
        match p.kind {
            PromptKind::RenameTab => {
                if let Some(t) = self.focused_tab() {
                    self.command(
                        "tab.rename",
                        json!({"tab": t.id, "title": v}),
                        Pending::Ignore,
                    );
                }
            }
            PromptKind::RenameWorkspace => {
                if let Some(w) = self.focused_ws() {
                    self.command(
                        "workspace.rename",
                        json!({"workspace": w.id, "name": v}),
                        Pending::Ignore,
                    );
                }
            }
            PromptKind::RenamePane => {
                if let Some(pn) = self.focused_pane() {
                    self.command(
                        "pane.rename",
                        json!({"pane": pn, "title": v}),
                        Pending::Ignore,
                    );
                }
            }
            PromptKind::NewWorkspace => {
                let dir = if let Some(rest) = v.strip_prefix("~/") {
                    format!("{}/{rest}", std::env::var("HOME").unwrap_or_default())
                } else {
                    v
                };
                self.command(
                    "workspace.create",
                    json!({"cwd": dir, "focus": true}),
                    Pending::Ignore,
                );
            }
            PromptKind::TaskTitle => {
                if !v.is_empty() {
                    let repo = self.focused_pane().and_then(|pn| {
                        self.m()
                            .model
                            .panes
                            .iter()
                            .find(|x| x.id == pn)
                            .and_then(|x| x.cwd.clone())
                    });
                    self.command(
                        "task.create",
                        json!({"title": v, "repo": repo, "focus": true}),
                        Pending::Toast("task created".into()),
                    );
                }
            }
            PromptKind::AgentReply { pane } => {
                if !v.is_empty() {
                    self.command(
                        "agent.prompt",
                        json!({"target": pane, "text": v}),
                        Pending::Toast("sent".into()),
                    );
                }
            }
            PromptKind::CardText { interaction } => {
                let mi = self.cur;
                self.answer(mi, &interaction, json!({"decision": "deny", "text": v}));
            }
            PromptKind::BrowserUrl { pane } => crate::browser::navigate(self, &pane, &v),
            k @ (PromptKind::GroupNew { .. }
            | PromptKind::GroupRename { .. }
            | PromptKind::LayoutSave { .. }) => crate::parity::submit_prompt(self, k, v),
            PromptKind::ActionArg { action } => crate::navkeys::run_with_arg(self, &action, &v),
            PromptKind::Command => {
                let mut it = v.splitn(2, ' ');
                let action = it.next().unwrap_or("").replace('-', "_");
                if !action.is_empty() {
                    self.action(&action, None);
                }
            }
        }
    }

    fn popup_key(&mut self, ev: KeyEvent, p: Popup) {
        crate::popups::key(self, ev, p);
    }

    pub fn confirm(&mut self, a: Action) {
        match a {
            Action::ClosePane(p) => self.command("pane.close", json!({"pane": p}), Pending::Ignore),
            Action::CloseTab(t) => self.command("tab.close", json!({"tab": t}), Pending::Ignore),
            Action::CloseWorkspace(w) => {
                self.command("workspace.close", json!({"workspace": w}), Pending::Ignore)
            }
            Action::Detach => self.quit = Some("detached".into()),
            Action::ForgetTask { machine, task } => crate::taskbadge::forget(self, machine, &task),
        }
    }

    // ---- view hints + drawing -----------------------------------------------------------

    pub fn send_view_hints(&mut self, force: bool) {
        let rects = self.pane_rects();
        let active = self.host_focused;
        let cur = self.cur;
        for (mi, m) in self.machines.iter_mut().enumerate() {
            let hint: Vec<PaneRect> = if mi == cur {
                rects
                    .iter()
                    .map(|(p, r)| PaneRect {
                        pane: p.clone(),
                        cols: r.w,
                        rows: r.h,
                    })
                    .collect()
            } else {
                vec![]
            };
            if force || hint != m.last_hint {
                m.last_hint = hint.clone();
                m.send(ClientFrame::ViewHint {
                    panes: hint,
                    active,
                });
            }
        }
    }

    fn draw(&mut self) -> Result<()> {
        self.dirty = false;
        self.send_view_hints(false);
        crate::browser::update_views(self);
        crate::gallery::before_draw(self);
        crate::preview_ui::before_draw(self);
        crate::pane_images::before_draw(self);
        crate::nav::observe(self);
        crate::plugins::observe(self);
        crate::sidebar::fit(self);
        let (cols, rows) = self.size;
        let mut grid = Grid::new(cols, rows);
        let cursor = draw::compose(self, &mut grid);
        // Browser tile images first; their placeholder cells follow in the grid diff.
        let mut out = crate::browser::take_output(self);
        out.extend(crate::pane_images::take_output(self));
        if let Some(title) = crate::nav::title_update(self) {
            out.extend_from_slice(&title);
        }
        out.reserve(16 * 1024);
        if !self.caps.sync_update {
            out.extend_from_slice(b"\x1b[?25l");
        }
        crate::screen::diff(&self.prev, &grid, &self.caps, &mut out);
        match cursor {
            Some((x, y, shape)) => crate::screen::cursor(&mut out, x, y, true, shape),
            None => out.extend_from_slice(b"\x1b[?25l"),
        }
        self.prev = grid;
        crate::browser::after_write(self, &mut out);
        let mut stdout = std::io::stdout();
        stdout.write_all(&out)?;
        stdout.flush()?;
        Ok(())
    }
}

fn vk_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Pending operations popup: unknown outcomes are retried only on an explicit, warned action.
pub(crate) fn pending_ops_key(app: &mut App, ev: KeyEvent, sel: usize, confirm: bool) {
    let n = app.pending_ops.ops.len();
    let stay = |app: &mut App, sel: usize, confirm: bool| {
        app.mode = Mode::Popup(Popup::PendingOps { sel, confirm })
    };
    if confirm {
        match ev.key {
            Key::Char('y' | 'Y') => {
                if let Some(op) = app.pending_ops.ops.get(sel).cloned() {
                    let mi = app.machines.iter().position(|m| m.label == op.machine);
                    match mi {
                        Some(mi) if app.machines[mi].connected() => {
                            // Same key and payload: if the first attempt did land, the owner
                            // replays its receipt instead of applying it twice.
                            app.mutate(
                                mi,
                                &op.method,
                                op.params.clone(),
                                Pending::Toast(format!("{} — retried", op.describe())),
                            );
                        }
                        _ => app.toast(format!("{} is offline", op.machine)),
                    }
                }
                stay(app, sel, false);
            }
            _ => stay(app, sel, false),
        }
        return;
    }
    match ev.key {
        Key::Named(NamedKey::Escape) | Key::Char('q') => {}
        Key::Char('j') | Key::Named(NamedKey::Down) => {
            stay(app, (sel + 1).min(n.saturating_sub(1)), false)
        }
        Key::Char('k') | Key::Named(NamedKey::Up) => stay(app, sel.saturating_sub(1), false),
        Key::Char('r') if sel < n => stay(app, sel, true),
        Key::Char('d') if sel < n => {
            let key = app.pending_ops.ops[sel].key.clone();
            app.pending_ops.remove(&key);
            stay(app, sel.min(n.saturating_sub(2)), false);
        }
        _ => stay(app, sel, false),
    }
}

pub(crate) fn draw_pending_ops(app: &App, g: &mut Grid, sel: usize, confirm: bool) {
    let t = app.theme;
    let mut b = crate::popups::frame(app, g, 90, 20, "pending operations");
    if app.pending_ops.ops.is_empty() {
        b.line("No operations waiting for an outcome.", t.dim());
    }
    let now = vk_now_ms();
    for (i, op) in app.pending_ops.ops.iter().enumerate() {
        let status = match op.status {
            crate::pending::OpStatus::InFlight => "checking…",
            crate::pending::OpStatus::Unknown => "OUTCOME UNKNOWN",
        };
        let st = if i == sel { t.sel(t.fg) } else { t.text() };
        b.line(
            &format!(
                "{status:<16} {} · {} · {} ago",
                op.describe(),
                op.machine,
                crate::inbox::fmt_age(now - op.created_at_ms)
            ),
            st,
        );
        if let Some(n) = &op.note {
            b.line(&format!("                 {n}"), t.dim());
        }
    }
    for o in app.pending_ops.outcomes.iter().rev().take(4) {
        b.line(o, t.s(t.green));
    }
    if confirm {
        b.line(
            "The earlier request may have taken effect. Retry with the same key? [y] retry  [n] no",
            t.bold(t.yellow),
        );
    } else {
        b.line(
            "Never resubmitted automatically · [r] retry (warns)  [d] forget locally  [esc] close",
            t.dim(),
        );
    }
}

fn btn(b: CtButton) -> MouseButton {
    match b {
        CtButton::Left => MouseButton::Left,
        CtButton::Middle => MouseButton::Middle,
        CtButton::Right => MouseButton::Right,
    }
}

/// An `App` over fake machines whose outgoing frames land in the returned receivers.
#[cfg(test)]
pub(crate) fn test_app(n: usize) -> (App, Vec<mpsc::UnboundedReceiver<ClientFrame>>) {
    let mut machines = Vec::new();
    let mut rxs = Vec::new();
    for i in 0..n {
        let mut m = Machine::new(&format!("m{i}"), i == 0);
        let (tx, rx) = mpsc::unbounded_channel();
        m.tx = Some(tx);
        m.panes.insert("p1".into(), PaneBuf::blank());
        m.panes.insert("p2".into(), PaneBuf::blank());
        machines.push(m);
        rxs.push(rx);
    }
    let mut cfg = vk_config::Config::default();
    cfg.paste.translate = vk_config::PasteTranslate::Ask;
    let mut app = App::new(
        cfg,
        machines,
        "tui-test".into(),
        HostCaps::default(),
        false,
        false,
    );
    // Never touch the host terminal or the real clipboard from tests.
    app.clipboard_sink = Some(Vec::new());
    // Deterministic copy delivery whatever the test host's SSH/iTerm2/tmux environment.
    app.copyout = Default::default();
    // OSC 52 reads see a fixed clipboard, never the host's.
    app.osc = crate::osc::State::with_test_clipboard(b"(test clipboard)");
    app.size = (120, 40);
    (app, rxs)
}

#[cfg(test)]
pub(crate) fn test_run(id: &str, pane: &str, harness: &str) -> AgentRun {
    serde_json::from_value(json!({
        "id": id, "handle": "3", "name": null, "pane": pane, "harness": harness,
        "harness_version": null, "integration": "hooks", "harness_session_id": "sess-1",
        "transcript_path": null, "resume_argv": [], "cwd": null, "model": null, "task": null,
        "execution": {"value": "Idle", "since_ms": 0, "source": "Structured", "confidence": 1.0, "detail": null},
        "health": "Healthy", "yolo": false, "permission_mode": null, "last_message": null,
        "last_tool": null, "turns_completed": 2, "done_rev": 1, "started_at_ms": 0,
        "ended_at_ms": null, "capabilities": []
    }))
    .unwrap()
}

#[cfg(test)]
pub(crate) fn test_interaction(
    id: &str,
    pane: &str,
    title: &str,
    opened_at_ms: i64,
) -> Interaction {
    serde_json::from_value(json!({
        "id": id, "handle": "i1", "run": "r1", "pane": pane, "kind": "Approval", "status": "Open",
        "title": title, "body_md": null, "action": null, "questions": [], "plan_md": null,
        "answer_channel": "Native", "native_ref": null, "source": "Structured", "confidence": 1.0,
        "answerable": true, "gate": false, "decision_rev": 0, "delivery": "None",
        "delivery_error": null, "answer": null, "answered_by": null,
        "opened_at_ms": opened_at_ms, "answered_at_ms": null
    }))
    .unwrap()
}

#[cfg(test)]
mod clipboard_tests {
    use super::*;

    fn sink(app: &App) -> usize {
        app.clipboard_sink.as_ref().unwrap().len()
    }
    fn key(k: Key) -> KeyEvent {
        KeyEvent::new(k, Mods::empty())
    }
    fn ask_once(app: &mut App) {
        app.config.clipboard.remote_write = vk_config::RemoteWrite::AskOnce;
    }

    #[test]
    fn remote_writes_are_allowed_by_default() {
        let (mut app, _rx) = test_app(2);
        app.on_clipboard(1, "p1".into(), false, b"hi".to_vec());
        assert_eq!(sink(&app), 1);
        assert!(app.clip.pending.is_empty());
    }

    #[test]
    fn prefix_y_opens_the_review_and_y_allows_and_copies() {
        let (mut app, _rx) = test_app(2);
        ask_once(&mut app);
        assert!(!app.machines[1].local);
        app.on_clipboard(1, "p1".into(), false, b"from devbox".to_vec());
        assert_eq!(app.clip.pending.len(), 1);
        assert_eq!(sink(&app), 0);
        app.on_key(KeyEvent::new(Key::Char('b'), Mods::CTRL));
        assert!(matches!(app.mode, Mode::Prefix(_)));
        app.on_key(key(Key::Char('y')));
        assert!(
            matches!(app.mode, Mode::Popup(Popup::ClipboardAsk { .. })),
            "prefix+y opens the review"
        );
        assert!(app.clip.pending.is_empty());
        app.on_key(key(Key::Char('y')));
        assert_eq!(sink(&app), 1);
        assert_eq!(app.machines[1].clipboard_allowed, Some(true));
        assert!(matches!(app.mode, Mode::Normal));
        // Allowed for good: the next write goes straight through.
        app.on_clipboard(1, "p1".into(), false, b"again".to_vec());
        assert_eq!(sink(&app), 2);
    }

    #[test]
    fn unsolicited_write_does_not_replace_the_ui_mode() {
        let (mut app, _rx) = test_app(2);
        ask_once(&mut app);
        app.mode = Mode::Resize;
        app.on_clipboard(1, "p1".into(), false, b"hi".to_vec());
        assert!(matches!(app.mode, Mode::Resize));
        assert_eq!(app.clip.pending.len(), 1);
        assert_eq!(sink(&app), 0);
        app.mode = Mode::Popup(Popup::Help);
        app.on_clipboard(1, "p2".into(), false, b"again".to_vec());
        assert!(matches!(app.mode, Mode::Popup(Popup::Help)));
        assert_eq!(sink(&app), 0);
    }

    #[test]
    fn review_popup_needs_explicit_keys() {
        let (mut app, _rx) = test_app(2);
        ask_once(&mut app);
        app.on_clipboard(1, "p1".into(), false, b"secret".to_vec());
        app.review_clipboard();
        assert!(matches!(app.mode, Mode::Popup(Popup::ClipboardAsk { .. })));
        for c in "hell wrld\n abdefghijklmpqrstvwxz!/".chars() {
            app.on_key(key(Key::Char(c)));
            assert!(matches!(app.mode, Mode::Popup(Popup::ClipboardAsk { .. })));
        }
        assert_eq!(sink(&app), 0);
        assert_eq!(app.machines[1].clipboard_allowed, None);
        app.on_key(key(Key::Char('o')));
        assert_eq!(sink(&app), 1);
        assert_eq!(app.machines[1].clipboard_allowed, None);
    }

    #[test]
    fn size_cap_drops_large_writes() {
        let (mut app, _rx) = test_app(2);
        app.config.clipboard.remote_write = vk_config::RemoteWrite::Allow;
        app.config.clipboard.remote_write_max_bytes = vk_config::ByteSize(16);
        app.on_clipboard(1, "p1".into(), false, vec![b'x'; 17]);
        assert_eq!(sink(&app), 0);
        assert_eq!(app.clip.dropped, 1);
        app.on_clipboard(1, "p1".into(), false, vec![b'x'; 16]);
        assert_eq!(sink(&app), 1);
    }

    #[test]
    fn prompts_are_rate_limited_per_pane() {
        let (mut app, _rx) = test_app(2);
        ask_once(&mut app);
        app.on_clipboard(1, "p1".into(), false, b"1".to_vec());
        app.on_clipboard(1, "p1".into(), false, b"2".to_vec());
        app.on_clipboard(1, "p1".into(), false, b"3".to_vec());
        assert_eq!(app.clip.pending.len(), 1);
        assert_eq!(app.clip.pending[0].data, b"1");
        assert_eq!(app.clip.dropped, 2);
        // A different pane has its own budget (and the machine's latest request wins).
        app.on_clipboard(1, "p2".into(), false, b"4".to_vec());
        assert_eq!(app.clip.pending.len(), 1);
        assert_eq!(app.clip.pending[0].data, b"4");
        // Once the interval has passed the pane may prompt again.
        app.config.clipboard.remote_write_min_interval = vk_config::Dur(Duration::ZERO);
        app.on_clipboard(1, "p1".into(), false, b"5".to_vec());
        assert_eq!(app.clip.pending[0].data, b"5");
    }
}

#[cfg(test)]
mod pending_tests {
    use super::*;

    fn commands(rx: &mut mpsc::UnboundedReceiver<ClientFrame>) -> Vec<(u64, Value)> {
        let mut v = Vec::new();
        while let Ok(f) = rx.try_recv() {
            if let ClientFrame::Command { req, json } = f {
                let c: Value = serde_json::from_str(&json).unwrap();
                // The queries every connect makes (crate::plugins, crate::elevate) aren't
                // operations.
                if !matches!(
                    c["method"].as_str(),
                    Some(
                        "plugin.action.list"
                            | "plugin.link_handler.list"
                            | "compat.ui.state"
                            | "auth.list"
                            | "gateway.status"
                    )
                ) {
                    v.push((req, c));
                }
            }
        }
        v
    }

    fn reply(app: &mut App, mi: usize, req: u64, result: Value) {
        let json = json!({"jsonrpc": "2.0", "id": req, "result": result}).to_string();
        app.on_frame(mi, ServerFrame::CommandResult { req, json });
    }

    /// Crash after dispatch: a new client loads the file, asks the owner on connect and shows
    /// the outcome; an unknown outcome is never resubmitted on its own.
    #[test]
    fn restart_reconciles_and_never_auto_resubmits() {
        let dir = tempfile::tempdir().unwrap();
        let sess = dir.path().join("sess");
        {
            let (mut app, mut rxs) = test_app(1);
            app.pending_ops = crate::pending::PendingStore::open(sess.clone(), "crashed");
            assert!(app.mutate(
                0,
                "task.track",
                json!({"run": "r1", "title": "A", "idempotency_key": "k-a"}),
                Pending::Ignore
            ));
            assert!(app.mutate(
                0,
                "task.intent.update",
                json!({"task": "t1", "expected_revision": 1, "idempotency_key": "k-b"}),
                Pending::Ignore
            ));
            assert_eq!(commands(&mut rxs[0]).len(), 2);
            // The client dies here: no responses.
        }
        let (mut app, mut rxs) = test_app(1);
        app.pending_ops = crate::pending::PendingStore::open(sess.clone(), "restarted");
        app.pending_ops.adopt_settled(2);
        assert_eq!(app.pending_ops.ops.len(), 2);
        assert_eq!(app.pending_ops.adopted, 2);
        let path = sess.join("client-pending-restarted.json");
        app.on_connected(0);
        let c = commands(&mut rxs[0]);
        assert_eq!(c.len(), 2);
        assert!(c.iter().all(|(_, v)| v["method"] == "task.operation.get"));
        // Connecting again while those are outstanding doesn't ask twice.
        app.on_connected(0);
        assert!(commands(&mut rxs[0]).is_empty());
        let by_key = |k: &str| {
            c.iter()
                .find(|(_, v)| v["params"]["idempotency_key"] == k)
                .unwrap()
                .0
        };
        reply(
            &mut app,
            0,
            by_key("k-a"),
            json!({"known": true, "method": "task.track",
                   "result": {"task": {"id": "t9"}, "binding": {"run_id": "r1"}}}),
        );
        reply(
            &mut app,
            0,
            by_key("k-b"),
            json!({"known": false, "note": "no receipt: this does not mean the operation is safe to repeat"}),
        );
        assert_eq!(
            app.task_runs.get(&(0, "r1".into())).map(String::as_str),
            Some("t9")
        );
        assert_eq!(app.pending_ops.ops.len(), 1);
        assert_eq!(app.pending_ops.ops[0].key, "k-b");
        assert_eq!(
            app.pending_ops.ops[0].status,
            crate::pending::OpStatus::Unknown
        );
        // Nothing was resubmitted, not even after another reconnect.
        app.on_disconnected(0);
        app.on_connected(0);
        assert!(commands(&mut rxs[0]).is_empty());
        let again = crate::pending::read_ops(&path).unwrap();
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].status, crate::pending::OpStatus::Unknown);
        // The banner points at the review popup; retry needs an explicit, warned confirmation.
        let mut g = Grid::new(120, 40);
        draw::compose(&app, &mut g);
        let text = crate::tasks::grid_text(&g);
        assert!(text.contains("outcome(s) unknown"), "{text}");
        app.action("pending_operations", None);
        app.on_key(KeyEvent::new(Key::Char('r'), Mods::empty()));
        assert!(commands(&mut rxs[0]).is_empty());
        assert!(matches!(
            app.mode,
            Mode::Popup(Popup::PendingOps { confirm: true, .. })
        ));
        let mut g = Grid::new(120, 40);
        draw::compose(&app, &mut g);
        assert!(crate::tasks::grid_text(&g).contains("may have taken effect"));
        app.on_key(KeyEvent::new(Key::Char('y'), Mods::empty()));
        let c = commands(&mut rxs[0]);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].1["method"], "task.intent.update");
        // Same key: a landed first attempt is replayed by the owner, not applied twice.
        assert_eq!(c[0].1["params"]["idempotency_key"], "k-b");
    }

    /// Link drop mid-request: the response never comes; reconnect asks instead of resending.
    #[test]
    fn link_drop_reconciles_on_reconnect() {
        let (mut app, mut rxs) = test_app(1);
        assert!(app.mutate(
            0,
            "task.bind",
            json!({"task": "t1", "run": "r1", "idempotency_key": "k-c"}),
            Pending::Ignore
        ));
        commands(&mut rxs[0]);
        app.on_connected(0);
        // Still live on this connection: nothing to reconcile yet.
        assert!(commands(&mut rxs[0]).is_empty());
        app.on_disconnected(0);
        app.on_connected(0);
        let c = commands(&mut rxs[0]);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].1["method"], "task.operation.get");
        reply(
            &mut app,
            0,
            c[0].0,
            json!({"known": true, "method": "task.bind", "result": {}}),
        );
        assert!(app.pending_ops.ops.is_empty());
        assert!(app.pending_ops.outcomes[0].contains("Continue task"));
    }

    #[test]
    fn offline_machine_sends_nothing_and_records_nothing() {
        let (mut app, mut rxs) = test_app(1);
        app.machines[0].tx = None;
        assert!(!app.mutate(0, "task.track", json!({"run": "r1"}), Pending::Ignore));
        assert!(app.pending_ops.ops.is_empty());
        assert!(commands(&mut rxs[0]).is_empty());
    }
}

#[cfg(test)]
mod isolation_tests {
    use super::*;

    fn pane(id: &str, iso: Value) -> Pane {
        serde_json::from_value(json!({
            "id": id, "handle": "w1:p1", "tab": "t", "workspace": "w", "title": null,
            "auto_title": "zsh", "cwd": null, "cols": 80, "rows": 24, "child_pid": null,
            "fg_cmdline": [], "exited": false, "exit_code": null, "unread": false,
            "marked_unread": false, "pinned": false, "created_by": "user", "recovered": null,
            "isolation": iso
        }))
        .unwrap()
    }

    #[test]
    fn yolo_badge_and_isolation_glyph() {
        let (mut app, _rxs) = test_app(1);
        let mut r = test_run("r1", "p1", "claude");
        r.yolo = true;
        app.machines[0].model.runs.push(r);
        app.machines[0].model.panes.push(pane("p1", json!({})));
        let row = crate::draw::agent_row_text(&app, 0, "r1");
        assert!(row.contains("YOLO·HOST"), "{row}");
        app.machines[0].model.panes[0] = pane(
            "p1",
            json!({"level": "sandbox", "provider": "seatbelt", "network": "none", "yolo": true, "scope": "pane", "visible_roots": []}),
        );
        let row = crate::draw::agent_row_text(&app, 0, "r1");
        assert!(row.contains("sb·none "), "{row}");
        assert!(row.contains("YOLO ") && !row.contains("YOLO·HOST"), "{row}");
    }

    #[test]
    fn local_sandbox_paste_translates_only_outside_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("co")).unwrap();
        std::fs::create_dir_all(root.join("Desktop")).unwrap();
        std::fs::write(root.join("co/a.rs"), "x").unwrap();
        std::fs::write(root.join("Desktop/shot.png"), "x").unwrap();
        let (mut app, mut rxs) = test_app(1);
        assert!(app.machines[0].local);
        app.machines[0].model.panes.push(pane(
            "p1",
            json!({"level": "sandbox", "provider": "seatbelt", "network": "dev", "yolo": false, "scope": "pane", "visible_roots": [root.join("co")]}),
        ));
        app.machines[0].focus.pane = Some("p1".into());
        // Inside the checkout: pasted untouched.
        let inside = root.join("co/a.rs").to_string_lossy().into_owned();
        app.on_paste(inside.clone());
        let mut sent = None;
        while let Ok(f) = rxs[0].try_recv() {
            if let ClientFrame::Paste { text, .. } = f {
                sent = Some(text);
            }
        }
        assert_eq!(sent.as_deref(), Some(inside.as_str()));
        // Outside (Desktop): goes through the A11 translation path (Ask mode in tests).
        app.on_paste(root.join("Desktop/shot.png").to_string_lossy().into_owned());
        assert!(matches!(app.mode, Mode::Popup(Popup::PasteAsk { .. })));
    }

    #[test]
    fn local_host_paste_is_never_translated() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("shot.png");
        std::fs::write(&f, "x").unwrap();
        let (mut app, _rxs) = test_app(1);
        app.machines[0].model.panes.push(pane("p1", json!({})));
        app.machines[0].focus.pane = Some("p1".into());
        app.on_paste(f.to_string_lossy().into_owned());
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn remote_paste_of_escaped_screenshot_path_translates() {
        let dir = tempfile::tempdir().unwrap();
        let name = "Screenshot 2026-10-07 at 07.31.34.png";
        std::fs::write(dir.path().join(name), "x").unwrap();
        let (mut app, _rxs) = test_app(1);
        app.machines[0].local = false;
        app.machines[0].model.panes.push(pane("p1", json!({})));
        app.machines[0].focus.pane = Some("p1".into());
        let escaped = dir.path().join(name).to_string_lossy().replace(' ', "\\ ");
        app.on_paste(format!("{escaped} "));
        assert!(matches!(app.mode, Mode::Popup(Popup::PasteAsk { .. })));
    }
}
